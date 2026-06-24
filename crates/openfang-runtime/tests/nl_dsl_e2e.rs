//! End-to-end pipeline test: natural-language intent → `MissionDsl` →
//! own-platform fast-loop `CandidateIntent`s.
//!
//! Exercises the full single-platform autonomous chain wired by the plan
//! (`IntentExtractor` → `MissionCompiler` → `FunctionExecutor`) across a range
//! of operator phrasings, asserting the right mission class, command specs,
//! phase ordering, and standoff / ROE gating — without a live daemon.

use async_trait::async_trait;
use openfang_runtime::function_executor::{ExecutionPlan, FunctionExecutor};
use openfang_runtime::intent_extractor::{
    ExtractContext, IntentExtractDriver, IntentExtractor, IntentSemanticSource, StructuredIntent,
    SymbolicTask,
};
use openfang_runtime::mission_compiler::{CompileParams, MissionCompiler};
use openfang_runtime::platform_allocator::SelfPlatformAllocator;
use openfang_runtime::play_registry::PlayRegistry;

use openfang_types::config::{ControlledSide, PlatformControlPolicy, ThreatSide};
use openfang_types::mission_dsl::{MissionDsl, MissionKind, PlatformCommandSpec};
use openfang_types::platform::{
    Affiliation, Domain, FuelStatus, PlatformCommand, PlatformState, Pose, SensorState, SensorType,
    Track, Velocity, WeaponState, WorldSnapshot,
};

fn policy() -> PlatformControlPolicy {
    PlatformControlPolicy {
        controlled_side: ControlledSide::Red,
        threat_side: ThreatSide::Opposite,
        controlled_platforms: vec!["red-uav-1".into()],
        own_platform_id: "red-uav-1".into(),
        controller_id: "operator".into(),
        ..Default::default()
    }
}

/// World with one armed+sensored own UAV holding a foe track, plus a hostile
/// command-post platform (target ~7 km away, outside the default standoff ring).
fn snapshot() -> WorldSnapshot {
    let mut own = PlatformState::minimal("red-uav-1");
    own.affiliation = Affiliation::Red;
    own.platform_type = "uav".into();
    own.domain = Domain::Air;
    own.pose = Pose {
        lat_deg: 30.0,
        lon_deg: 120.0,
        alt_m: 1000.0,
        heading_deg: 0.0,
        pitch_deg: 0.0,
        roll_deg: 0.0,
    };
    own.velocity = Velocity {
        speed_ms: 50.0,
        vertical_rate_ms: 0.0,
        course_deg: 0.0,
    };
    own.fuel = FuelStatus {
        remaining_kg: 80.0,
        max_kg: 100.0,
        consumption_rate_kg_s: 0.1,
    };
    own.onboard_weapons = vec![WeaponState {
        weapon_id: "w1".into(),
        weapon_type: "missile".into(),
        quantity_remaining: 2.0,
        max_range_m: Some(20_000.0),
        min_range_m: Some(0.0),
        guidance_type: None,
        speed_ms: None,
        is_ready: true,
        quantity_from_snapshot: true,
    }];
    own.onboard_sensors = vec![SensorState {
        sensor_id: "eo1".into(),
        sensor_type: SensorType::EOIR,
        mode: "search".into(),
        frequency_hz: None,
        bandwidth_hz: None,
        azimuth_fov_deg: None,
        elevation_fov_deg: None,
        range_max_m: Some(15_000.0),
        damage: 0.0,
        host_platform_id: "red-uav-1".into(),
    }];
    own.tracks = vec![
        Track {
            track_id: "blue_command_post:1".into(),
            target_name: "blue_command_post".into(),
            classification: "command_post".into(),
            affiliation: Affiliation::Blue,
            iff: "foe".into(),
            position_lla: Some((30.05, 120.05, 0.0)),
            heading_deg: Some(0.0),
            speed_ms: None,
            range_m: Some(7_000.0),
            bearing_deg: Some(45.0),
            elevation_deg: None,
            quality: 0.9,
            stale: false,
            last_update_s: 1.0,
            is_active: true,
        },
        Track {
            track_id: "blue_patrol_1".into(),
            target_name: "blue_patrol_1".into(),
            classification: "patrol_boat".into(),
            affiliation: Affiliation::Blue,
            iff: "foe".into(),
            position_lla: Some((30.04, 120.04, 0.0)),
            heading_deg: Some(180.0),
            speed_ms: Some(15.0),
            range_m: Some(6_000.0),
            bearing_deg: Some(45.0),
            elevation_deg: None,
            quality: 0.9,
            stale: false,
            last_update_s: 1.0,
            is_active: true,
        },
    ];
    let blue = {
        let mut b = PlatformState::minimal("blue_command_post");
        b.affiliation = Affiliation::Blue;
        b.platform_type = "command_post".into();
        b.pose = Pose {
            lat_deg: 30.05,
            lon_deg: 120.05,
            alt_m: 0.0,
            heading_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
        };
        b
    };
    let patrol = {
        let mut b = PlatformState::minimal("blue_patrol_1");
        b.affiliation = Affiliation::Blue;
        b.platform_type = "patrol_boat".into();
        b.pose = Pose {
            lat_deg: 30.04,
            lon_deg: 120.04,
            alt_m: 0.0,
            heading_deg: 180.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
        };
        b
    };
    WorldSnapshot {
        timestamp: 100.0,
        platforms: vec![own, blue, patrol],
        active_munitions: vec![],
        events: vec![],
        fleet: None,
    }
}

/// Run the full pipeline for one NL input.
fn run_pipeline(text: &str) -> (MissionDsl, ExecutionPlan) {
    let snap = snapshot();
    let policy = policy();
    let intent = IntentExtractor::new().extract(text, &snap, &policy);
    let registry = PlayRegistry::bundled();
    let compiler = MissionCompiler::new(SelfPlatformAllocator::new());
    let mission = compiler.compile(
        &intent,
        &snap,
        &registry,
        &policy,
        &CompileParams::default(),
    );

    let own = snap.find_platform("red-uav-1").cloned().unwrap();
    let plan = FunctionExecutor::new().execute(&mission, &snap, &own, snap.timestamp);
    (mission, plan)
}

async fn run_pipeline_with_driver(
    text: &str,
    driver: &dyn IntentExtractDriver,
) -> (StructuredIntent, MissionDsl, ExecutionPlan) {
    let snap = snapshot();
    let policy = policy();
    let registry = PlayRegistry::bundled();
    let params = CompileParams::default();
    let output = openfang_runtime::mission_compiler::compile_objective_with_semantics(
        text,
        &snap,
        &policy,
        &registry,
        &params,
        Some(driver),
        0.5,
    )
    .await;

    let own = snap.find_platform("red-uav-1").cloned().unwrap();
    let plan = FunctionExecutor::new().execute(&output.mission, &snap, &own, snap.timestamp);
    (output.structured_intent, output.mission, plan)
}

fn first_index<F: Fn(&PlatformCommand) -> bool>(plan: &ExecutionPlan, pred: F) -> Option<usize> {
    plan.intents.iter().position(|i| pred(&i.command))
}

#[test]
fn engage_produces_fire_candidate_outside_standoff() {
    let (mission, plan) = run_pipeline("打击蓝方指挥所");
    assert_eq!(mission.kind, MissionKind::Engage);
    assert!(mission.is_valid(), "issues: {:?}", mission.validate());
    // Target ~7 km out, default standoff 3 km → fire is allowed (emitted).
    assert!(
        plan.has_lethal_intent(),
        "expected a fire candidate, held: {:?}",
        plan.held
    );
}

#[test]
fn kill_enemy_patrol_boat_compiles_to_engage_with_fire() {
    let (mission, plan) = run_pipeline("杀伤敌方巡逻艇");
    assert_eq!(mission.kind, MissionKind::Engage);
    assert!(
        !mission.plays.is_empty(),
        "Engage should select a play, rendered:\n{}",
        mission
    );
    assert!(
        mission
            .functions
            .iter()
            .any(|f| matches!(f.command, PlatformCommandSpec::Fire { .. })),
        "Engage should bind a fire function, rendered:\n{}",
        mission
    );
    assert!(plan.has_lethal_intent());
}

#[tokio::test]
async fn llm_first_kill_enemy_patrol_boat_compiles_to_fire() {
    struct SemanticDriver;
    #[async_trait]
    impl IntentExtractDriver for SemanticDriver {
        async fn extract(&self, ctx: ExtractContext) -> Option<StructuredIntent> {
            let mut intent = StructuredIntent::unknown("杀伤敌方巡逻艇");
            intent.kind = MissionKind::Engage;
            intent.target_labels = vec!["敌方巡逻艇".into()];
            intent.confidence = 0.9;
            intent.rationale = "semantic strike intent".into();
            let target_track_id = ctx
                .candidate_track_ids
                .first()
                .cloned()
                .unwrap_or_else(|| "blue_patrol_1".into());
            intent.task_plan = vec![SymbolicTask {
                task_id: "T1".into(),
                platform: Some("red-uav-1".into()),
                action: "Fire".into(),
                target: Some(target_track_id.clone()),
                criteria: Some("target_destroyed".into()),
                preconditions: Vec::new(),
                parameters: serde_json::Map::from_iter([(
                    "target_track_id".into(),
                    serde_json::Value::String(target_track_id),
                )]),
                phase: 0,
                ordering: 0,
            }];
            Some(intent)
        }
    }

    let (intent, mission, plan) = run_pipeline_with_driver("杀伤敌方巡逻艇", &SemanticDriver).await;
    assert_eq!(intent.semantic_source, IntentSemanticSource::Llm);
    assert_eq!(intent.target_track_ids, vec!["blue_patrol_1".to_string()]);
    assert_eq!(mission.kind, MissionKind::Engage);
    assert!(mission.is_valid(), "issues: {:?}", mission.validate());
    assert!(plan.has_lethal_intent());
}

#[test]
fn track_only_with_weapons_hold_emits_no_lethal() {
    let (mission, plan) = run_pipeline("对蓝方指挥所只跟踪，武器先别动");
    assert_eq!(mission.kind, MissionKind::Track);
    assert!(!mission.has_lethal_function());
    assert!(!plan.has_lethal_intent());
}

#[test]
fn rtb_lowers_to_goto_no_weapon() {
    let (mission, plan) = run_pipeline("所有无人机返航");
    assert_eq!(mission.kind, MissionKind::Rtb);
    assert!(mission
        .functions
        .iter()
        .any(|f| matches!(f.command, PlatformCommandSpec::Goto { .. })));
    assert!(!plan.has_lethal_intent());
    // A navigation command is emitted (GotoLocation / SetHeading / FollowRoute).
    assert!(first_index(&plan, |c| matches!(
        c,
        PlatformCommand::GotoLocation { .. }
            | PlatformCommand::FollowRoute { .. }
            | PlatformCommand::SetHeading { .. }
    ))
    .is_some());
}

#[test]
fn recon_flank_strike_orders_recon_before_strike() {
    let (mission, plan) =
        run_pipeline("绕后使用侦察无人机察打一体打击蓝方指挥所，注意保持安全距离3公里");
    assert_eq!(mission.kind, MissionKind::ReconFlankStrike);
    assert_eq!(mission.standoff_m(), Some(3000.0));
    // A flank route (recon phase) is generated.
    assert!(mission
        .functions
        .iter()
        .any(|f| matches!(f.command, PlatformCommandSpec::FollowRoute { .. })));
    // Recon (route / sensor) phase precedes any lethal release in the emitted
    // candidate stream.
    let recon_idx = first_index(&plan, |c| {
        matches!(
            c,
            PlatformCommand::FollowRoute { .. } | PlatformCommand::SensorOn { .. }
        )
    });
    let strike_idx = first_index(&plan, |c| {
        matches!(
            c,
            PlatformCommand::FireAtTarget { .. }
                | PlatformCommand::CoordinatedStrike { .. }
                | PlatformCommand::FireSalvo { .. }
        )
    });
    assert!(recon_idx.is_some(), "expected a recon-phase command");
    if let (Some(r), Some(s)) = (recon_idx, strike_idx) {
        assert!(r < s, "recon (idx {r}) must precede strike (idx {s})");
    }
}

#[test]
fn turn_left_emits_set_heading_in_fast_loop() {
    let (mission, plan) = run_pipeline("self 左转，速度5米每秒");
    assert_eq!(mission.kind, MissionKind::Recon);
    assert!(first_index(&plan, |c| matches!(
        c,
        PlatformCommand::SetHeading {
            turn_direction: Some(openfang_types::platform::TurnDirection::Left),
            ..
        }
    ))
    .is_some());
    assert!(first_index(&plan, |c| {
        matches!(c, PlatformCommand::SetSpeed { speed_ms, .. } if (*speed_ms - 5.0).abs() < 0.01)
    })
    .is_some());
}

#[test]
fn coordinated_strike_has_a_real_strike_platform() {
    let (mission, _plan) = run_pipeline("协同打击蓝方指挥所");
    assert_eq!(mission.kind, MissionKind::CoordinatedStrike);
    let strike = mission
        .functions
        .iter()
        .find_map(|f| match &f.command {
            PlatformCommandSpec::CoordinatedStrike {
                strike_platform_ids,
                ..
            } => Some(strike_platform_ids),
            _ => None,
        })
        .expect("coordinated strike function should exist");

    assert!(
        !strike.is_empty(),
        "coordinated strike must name at least one strike platform"
    );
    assert_eq!(strike, &vec!["red-uav-1".to_string()]);
}

#[test]
fn unknown_intent_yields_no_actuation() {
    let (mission, plan) = run_pipeline("今天天气不错");
    assert_eq!(mission.kind, MissionKind::Unknown);
    assert!(mission.functions.is_empty());
    assert!(plan.intents.is_empty());
    assert!(!plan.has_lethal_intent());
}
