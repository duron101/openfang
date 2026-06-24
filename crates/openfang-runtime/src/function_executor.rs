//! Function executor — lower a compiled [`MissionDsl`] into ordered fast-loop
//! [`CandidateIntent`]s for the own platform.
//!
//! Single-platform phase: every [`FunctionCall`] is already bound to the own
//! platform id by the compiler. The executor:
//!
//! - walks functions in declared (execution) order,
//! - drives the own platform's [`CcaRole`] posture (EMCON / sensors / weapon
//!   safing) whenever the active role changes between functions
//!   (recon → designator → striker), via [`crate::cca_role::posture_commands`],
//! - lowers each function spec to a concrete [`PlatformCommand`] and wraps it in
//!   a [`CandidateIntent`] (the only path into the composer/gate),
//! - applies the **standoff gate** and **ROE gate**: lethal functions are *held*
//!   (not emitted) when the own platform is inside the standoff radius or when
//!   ROE is weapons-hold. The CommandGate remains the authoritative interlock;
//!   this is defense-in-depth at the planning boundary.
//!
//! Lethal *release* is never produced here unconditionally — it is emitted as a
//! candidate that still must clear capability → approval → SPGS downstream.

use openfang_types::mission_dsl::{MissionDsl, PlatformCommandSpec};
use openfang_types::platform::{CcaRole, PlatformCommand, PlatformState, Pose, WorldSnapshot};
use openfang_types::tactical::{CandidateIntent, CommandPriority, IntentSource};
use openfang_types::umaa::WeaponReleaseLevel;

use crate::cca_role::{posture_commands, posture_for};
use crate::nav_control::NavController;

/// A function the executor refused to emit at the planning boundary, with why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldFunction {
    pub function_id: String,
    pub reason: String,
}

/// Result of executing (lowering) a mission for the own platform.
#[derive(Debug, Clone, Default)]
pub struct ExecutionPlan {
    pub mission_id: String,
    /// Ordered, gate-bound candidate intents ready for the composer.
    pub intents: Vec<CandidateIntent>,
    /// Functions held back at the planning boundary (standoff / ROE).
    pub held: Vec<HeldFunction>,
}

impl ExecutionPlan {
    /// Whether any weapon-*release* candidate survived to be emitted. Note this
    /// is narrower than [`CommandClass::is_weapon`], which also covers targeting
    /// and weapon-safing; only an actual release counts as lethal here.
    pub fn has_lethal_intent(&self) -> bool {
        self.intents.iter().any(|i| {
            matches!(
                i.command,
                PlatformCommand::FireAtTarget { .. }
                    | PlatformCommand::FireSalvo { .. }
                    | PlatformCommand::CoordinatedStrike { .. }
            )
        })
    }
}

/// Lowers compiled missions into fast-loop candidate intents for the own platform.
#[derive(Debug, Default, Clone)]
pub struct FunctionExecutor;

impl FunctionExecutor {
    pub fn new() -> Self {
        Self
    }

    /// Execute a mission against the current world: produce ordered candidate
    /// intents driving role posture and the mission's functions.
    ///
    /// `own_state` is the own platform's current state (drives posture diffs);
    /// `now_secs` is the producer timestamp (sim or wall).
    pub fn execute(
        &self,
        mission: &MissionDsl,
        snapshot: &WorldSnapshot,
        own_state: &PlatformState,
        now_secs: f64,
    ) -> ExecutionPlan {
        let mut plan = ExecutionPlan {
            mission_id: mission.id.clone(),
            ..Default::default()
        };

        let standoff_m = mission.standoff_m();
        let weapons_hold = matches!(mission.roe(), Some(WeaponReleaseLevel::WeaponsHold));
        let source = IntentSource::Workflow {
            workflow_id: mission.id.clone(),
        };

        if let (Some(standoff), Some(target_id)) = (standoff_m, mission_target_id(mission)) {
            if let Some(target) = resolve_target_pose(snapshot, target_id) {
                let nav = NavController::new(own_state.id.clone());
                let (commands, breached) = nav.standoff_correction(
                    target.lat_deg,
                    target.lon_deg,
                    Some(target.alt_m),
                    standoff,
                    snapshot,
                );
                if breached {
                    for cmd in commands {
                        plan.intents.push(CandidateIntent::new(
                            cmd,
                            CommandPriority::High,
                            source.clone(),
                            now_secs,
                            format!("mission {} standoff backoff from {}", mission.id, target_id),
                        ));
                    }
                }
            }
        }

        let mut current_role: Option<CcaRole> = None;
        let mut entered_engagement = false;
        let mut seq: u64 = 0;
        // Coalesce idempotent posture commands across role changes: the platform
        // need not be told CommOn / SensorOn again if it was already driven into
        // that state, which is what produced the repeated `CommOn`/`SensorOff`
        // noise in the trace. Once a mission enters the engagement phase, later
        // posture changes must not inject platform-level WeaponSafeAll that would
        // contradict the active fire sequence.
        let mut posture_state: std::collections::HashMap<String, &'static str> =
            std::collections::HashMap::new();

        for function in &mission.functions {
            // Drive posture whenever the active tactical role changes.
            let role = role_for_command(&function.command);
            if role == CcaRole::Striker || function.is_lethal() {
                entered_engagement = true;
            }
            if current_role != Some(role) {
                current_role = Some(role);
                for cmd in posture_commands(own_state, &posture_for(role)) {
                    if entered_engagement && matches!(cmd, PlatformCommand::WeaponSafeAll { .. }) {
                        continue;
                    }
                    if let Some((lane, value)) = posture_lane(&cmd) {
                        if posture_state.get(&lane) == Some(&value) {
                            continue; // already in this posture on this lane
                        }
                        posture_state.insert(lane, value);
                    }
                    plan.intents.push(CandidateIntent::new(
                        cmd,
                        CommandPriority::Normal,
                        source.clone(),
                        now_secs,
                        format!("mission {} posture {:?}", mission.id, role),
                    ));
                    seq += 1;
                }
            }

            // Standoff / ROE gate for lethal functions.
            if function.is_lethal() {
                if weapons_hold {
                    plan.held.push(HeldFunction {
                        function_id: function.id.clone(),
                        reason: "ROE weapons-hold: lethal function suppressed".into(),
                    });
                    continue;
                }
                if let Some(reason) = target_pose_unavailable(&function.command, snapshot) {
                    plan.held.push(HeldFunction {
                        function_id: function.id.clone(),
                        reason,
                    });
                    continue;
                }
                if let Some(standoff) = standoff_m {
                    if let Some(reason) =
                        standoff_violation(&function.command, snapshot, own_state, standoff)
                    {
                        plan.held.push(HeldFunction {
                            function_id: function.id.clone(),
                            reason,
                        });
                        continue;
                    }
                }
            }

            let command = function.command.to_platform_command(&function.platform_id);
            plan.intents.push(CandidateIntent::new(
                command,
                CommandPriority::Normal,
                source.clone(),
                now_secs,
                format!("mission {} fn {}", mission.id, function.id),
            ));
            seq += 1;
        }

        let _ = seq;
        plan
    }
}

/// Map a function's command to the tactical role whose posture should be active
/// while it runs. Drives the EMCON/sensor/weapon-safe posture for the phase.
fn role_for_command(spec: &PlatformCommandSpec) -> CcaRole {
    match spec {
        // A recon-UAV slot release is an ISR deploy, not a strike: keep the Recon
        // posture (do not flip the platform into Striker / weapons-hot for it).
        PlatformCommandSpec::Fire { .. } if spec.is_isr_release() => CcaRole::Recon,
        PlatformCommandSpec::Fire { .. } | PlatformCommandSpec::CoordinatedStrike { .. } => {
            CcaRole::Striker
        }
        PlatformCommandSpec::Designate { .. } => CcaRole::Designator,
        PlatformCommandSpec::SetHeading { .. }
        | PlatformCommandSpec::SetSpeed { .. }
        | PlatformCommandSpec::FollowRoute { .. }
        | PlatformCommandSpec::Goto { .. } => CcaRole::Recon,
        _ => CcaRole::Recon,
    }
}

/// Coalescing lane + desired state for an idempotent posture command. Returns
/// `(lane_key, state)`; a command whose lane is already in `state` is redundant
/// and can be suppressed. `None` ⇒ never coalesce (e.g. weapon-safing, which is
/// a safety action that must always be re-issued after a lethal phase).
fn posture_lane(cmd: &PlatformCommand) -> Option<(String, &'static str)> {
    match cmd {
        PlatformCommand::CommOn { .. } => Some(("comm".into(), "on")),
        PlatformCommand::CommOff { .. } => Some(("comm".into(), "off")),
        PlatformCommand::SensorOn { sensor_id, .. } => Some((format!("sensor:{sensor_id}"), "on")),
        PlatformCommand::SensorOff { sensor_id, .. } => {
            Some((format!("sensor:{sensor_id}"), "off"))
        }
        PlatformCommand::JamStop { jammer_id, .. } => Some((format!("jam:{jammer_id}"), "stop")),
        _ => None,
    }
}

fn mission_target_id(mission: &MissionDsl) -> Option<&str> {
    mission
        .functions
        .iter()
        .find_map(|function| match &function.command {
            PlatformCommandSpec::Fire { track_id, .. } => Some(track_id.as_str()),
            PlatformCommandSpec::CoordinatedStrike { target_id, .. } => Some(target_id.as_str()),
            PlatformCommandSpec::Designate { track_id } => Some(track_id.as_str()),
            _ => None,
        })
}

fn target_id_for_lethal(spec: &PlatformCommandSpec) -> Option<&str> {
    match spec {
        PlatformCommandSpec::Fire { track_id, .. } => Some(track_id.as_str()),
        PlatformCommandSpec::CoordinatedStrike { target_id, .. } => Some(target_id.as_str()),
        _ => None,
    }
}

fn target_pose_unavailable(spec: &PlatformCommandSpec, snapshot: &WorldSnapshot) -> Option<String> {
    let target_id = target_id_for_lethal(spec)?;
    if resolve_target_pose(snapshot, target_id).is_some() {
        return None;
    }
    Some(format!(
        "target pose unavailable: cannot verify standoff or release geometry for '{target_id}'"
    ))
}

/// Returns a reason string when the own platform is inside the standoff radius of
/// the lethal command's target (i.e. too close to release safely).
fn standoff_violation(
    spec: &PlatformCommandSpec,
    snapshot: &WorldSnapshot,
    own_state: &PlatformState,
    standoff_m: f64,
) -> Option<String> {
    let target_id = target_id_for_lethal(spec)?;
    let target = resolve_target_pose(snapshot, target_id)?;
    let range = own_state.pose.distance_m(&target);
    if range < standoff_m {
        Some(format!(
            "standoff violation: range {range:.0}m < standoff {standoff_m:.0}m to '{target_id}'"
        ))
    } else {
        None
    }
}

/// Resolve a target id to a pose: a matching track first, then a platform.
fn resolve_target_pose(snapshot: &WorldSnapshot, target_id: &str) -> Option<Pose> {
    for platform in &snapshot.platforms {
        for track in &platform.tracks {
            if track.track_id == target_id {
                if let Some((lat, lon, alt)) = track.position_lla {
                    return Some(Pose {
                        lat_deg: lat,
                        lon_deg: lon,
                        alt_m: alt,
                        heading_deg: 0.0,
                        pitch_deg: 0.0,
                        roll_deg: 0.0,
                    });
                }
            }
        }
    }
    snapshot.find_platform(target_id).map(|p| p.pose)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent_extractor::IntentExtractor;
    use crate::mission_compiler::{CompileParams, MissionCompiler};
    use crate::platform_allocator::SelfPlatformAllocator;
    use crate::play_registry::PlayRegistry;
    use openfang_types::config::{ControlledSide, PlatformControlPolicy, ThreatSide};
    use openfang_types::mission_dsl::{
        DslObjective, FunctionCall, MissionKind, PlatformCommandSpec, SafetyGuard,
    };
    use openfang_types::platform::{
        Affiliation, Domain, FuelStatus, PlatformCommand, PlatformState, Pose, SensorState,
        SensorType, Track, Velocity, WeaponState,
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

    fn own_state() -> PlatformState {
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
        own
    }

    fn snapshot_with_target(target_lat: f64, target_lon: f64) -> WorldSnapshot {
        let mut own = own_state();
        own.tracks = vec![Track {
            track_id: "blue_command_post:1".into(),
            target_name: "blue_command_post".into(),
            classification: "command_post".into(),
            affiliation: Affiliation::Blue,
            iff: "foe".into(),
            position_lla: Some((target_lat, target_lon, 0.0)),
            heading_deg: Some(0.0),
            speed_ms: None,
            range_m: Some(5_000.0),
            bearing_deg: Some(45.0),
            elevation_deg: None,
            quality: 0.9,
            stale: false,
            last_update_s: 1.0,
            is_active: true,
        }];
        let blue = {
            let mut b = PlatformState::minimal("blue_command_post");
            b.affiliation = Affiliation::Blue;
            b.platform_type = "command_post".into();
            b
        };
        WorldSnapshot {
            timestamp: 100.0,
            platforms: vec![own, blue],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        }
    }

    fn compile(text: &str, snap: &WorldSnapshot) -> MissionDsl {
        let policy = policy();
        let intent = IntentExtractor::new().extract(text, snap, &policy);
        let registry = PlayRegistry::bundled();
        let compiler = MissionCompiler::new(SelfPlatformAllocator::new());
        compiler.compile(&intent, snap, &registry, &policy, &CompileParams::default())
    }

    #[test]
    fn recon_uav_deploy_not_held_when_target_pose_unresolved() {
        // Mirrors the live console case: a scout-UAV deploy whose target id could
        // not be grounded to a real track (placeholder id). A *kinetic* fire would
        // be held by the lethal target-pose gate, but an ISR release must still
        // launch — you deploy a scout precisely to go find the target.
        let snap = snapshot_with_target(30.05, 120.05);
        let mission = MissionDsl {
            id: "mission:recon".into(),
            intent_text: "发射侦察无人机".into(),
            kind: MissionKind::Recon,
            time_window: None,
            objectives: vec![],
            constraints: vec![],
            plays: vec![],
            functions: vec![FunctionCall {
                id: "ReconPatrol:self:0:0:deploy_recon_uav:employ".into(),
                task_id: "ReconPatrol:self:0:0:deploy_recon_uav:employ".into(),
                parent_play: "ReconPatrol".into(),
                platform_id: "red-uav-1".into(),
                command: PlatformCommandSpec::Fire {
                    weapon_id: "scout_uav_slot".into(),
                    track_id: "self:9".into(), // intentionally unresolvable
                    salvo_size: None,
                },
                preconditions: Vec::new(),
                criteria: None,
                phase: 0,
                ordering: 0,
                service: None,
                safety_guard: SafetyGuard::default(),
            }],
            intervention_points: vec![],
            explanation_trace: String::new(),
            confidence: 0.75,
            provenance: "test".into(),
        };

        let plan = FunctionExecutor::new().execute(&mission, &snap, &own_state(), 100.0);

        assert!(
            !plan
                .held
                .iter()
                .any(|h| h.reason.contains("target pose unavailable")),
            "recon UAV deploy must not be held by the lethal target-pose gate, held={:?}",
            plan.held
        );
        assert!(
            plan.intents.iter().any(|i| matches!(
                &i.command,
                PlatformCommand::FireAtTarget { weapon_id, .. } if weapon_id == "scout_uav_slot"
            )),
            "scout UAV release should be emitted as a FireAtTarget candidate, intents={:?}",
            plan.intents.iter().map(|i| &i.command).collect::<Vec<_>>()
        );
    }

    #[test]
    fn engage_emits_posture_then_fire_when_clear_of_standoff() {
        // Target far away (≈6.9 km) — beyond the 3 km default standoff.
        let snap = snapshot_with_target(30.05, 120.05);
        let mission = compile("打击蓝方指挥所", &snap);
        let plan = FunctionExecutor::new().execute(&mission, &snap, &own_state(), 100.0);

        assert!(plan.held.is_empty(), "should not hold: {:?}", plan.held);
        assert!(plan.has_lethal_intent(), "fire should be emitted");
        // A weapon command exists in the stream.
        assert!(plan
            .intents
            .iter()
            .any(|i| matches!(i.command, PlatformCommand::FireAtTarget { .. })));
    }

    #[test]
    fn fire_held_when_inside_standoff_radius() {
        // Target essentially co-located with own → range ≈ 0 < standoff.
        let snap = snapshot_with_target(30.0001, 120.0001);
        let mission = compile("打击蓝方指挥所", &snap);
        let plan = FunctionExecutor::new().execute(&mission, &snap, &own_state(), 100.0);

        assert!(
            plan.held.iter().any(|h| h.reason.contains("standoff")),
            "fire should be held by standoff, held={:?}",
            plan.held
        );
        assert!(!plan.has_lethal_intent());
    }

    #[test]
    fn lethal_held_when_target_pose_cannot_be_verified() {
        let compile_snap = snapshot_with_target(30.05, 120.05);
        let mission = compile("打击蓝方指挥所", &compile_snap);
        let mut confirm_snap = compile_snap.clone();
        confirm_snap.platforms[0].tracks.clear();
        confirm_snap
            .platforms
            .retain(|p| p.id != "blue_command_post");

        let plan = FunctionExecutor::new().execute(&mission, &confirm_snap, &own_state(), 100.0);

        assert!(
            plan.held
                .iter()
                .any(|h| h.reason.contains("target pose unavailable")),
            "lethal command must fail closed when target pose disappears, held={:?}",
            plan.held
        );
        assert!(!plan.has_lethal_intent());
    }

    #[test]
    fn standoff_breach_emits_backoff_navigation() {
        // The compiled strike target is inside the 3km ring. The executor should
        // emit a navigation correction before holding the lethal release.
        let snap = snapshot_with_target(30.0001, 120.0001);
        let mission = compile("绕后侦察打击蓝方指挥所，保持安全距离3公里", &snap);
        let plan = FunctionExecutor::new().execute(&mission, &snap, &own_state(), 100.0);

        assert!(
            plan.intents
                .iter()
                .any(|i| matches!(i.command, PlatformCommand::SetHeading { .. })),
            "standoff breach should produce a back-off heading command"
        );
        assert!(!plan.has_lethal_intent());
    }

    #[test]
    fn weapons_hold_roe_suppresses_lethal() {
        let snap = snapshot_with_target(30.05, 120.05);
        // Engage class but with explicit weapons-hold ROE.
        let mut mission = compile("打击蓝方指挥所", &snap);
        mission
            .constraints
            .push(openfang_types::mission_dsl::Constraint::roe(
                WeaponReleaseLevel::WeaponsHold,
            ));
        let plan = FunctionExecutor::new().execute(&mission, &snap, &own_state(), 100.0);
        assert!(!plan.has_lethal_intent());
        assert!(plan.held.iter().any(|h| h.reason.contains("weapons-hold")));
    }

    #[test]
    fn recon_phase_safes_weapons_via_posture() {
        let snap = snapshot_with_target(30.05, 120.05);
        let mission = compile("绕后侦察打击蓝方指挥所，保持安全距离3公里", &snap);
        let plan = FunctionExecutor::new().execute(&mission, &snap, &own_state(), 100.0);
        // Recon posture safes weapons before the strike phase.
        assert!(plan
            .intents
            .iter()
            .any(|i| matches!(i.command, PlatformCommand::WeaponSafeAll { .. })));
    }

    #[test]
    fn post_engagement_recon_posture_does_not_resafe_weapons() {
        let snap = snapshot_with_target(30.05, 120.05);
        let mission = MissionDsl {
            id: "mission:post-engagement-recon".into(),
            intent_text: "strike then monitor".into(),
            kind: MissionKind::Engage,
            time_window: None,
            objectives: vec![DslObjective {
                id: "obj".into(),
                description: "engage and observe".into(),
                feedback_var: None,
                priority: 1,
            }],
            constraints: vec![],
            plays: vec![],
            functions: vec![
                FunctionCall {
                    id: "sensor-before".into(),
                    task_id: "sensor-before".into(),
                    parent_play: "test".into(),
                    platform_id: "red-uav-1".into(),
                    command: PlatformCommandSpec::SensorOn {
                        sensor_id: "eo1".into(),
                    },
                    preconditions: Vec::new(),
                    criteria: None,
                    phase: 0,
                    ordering: 0,
                    service: None,
                    safety_guard: SafetyGuard::default(),
                },
                FunctionCall {
                    id: "fire".into(),
                    task_id: "fire".into(),
                    parent_play: "test".into(),
                    platform_id: "red-uav-1".into(),
                    command: PlatformCommandSpec::Fire {
                        weapon_id: "w1".into(),
                        track_id: "blue_command_post:1".into(),
                        salvo_size: None,
                    },
                    preconditions: Vec::new(),
                    criteria: None,
                    phase: 1,
                    ordering: 0,
                    service: None,
                    safety_guard: SafetyGuard::default(),
                },
                FunctionCall {
                    id: "sensor-after".into(),
                    task_id: "sensor-after".into(),
                    parent_play: "test".into(),
                    platform_id: "red-uav-1".into(),
                    command: PlatformCommandSpec::SensorOn {
                        sensor_id: "eo1".into(),
                    },
                    preconditions: Vec::new(),
                    criteria: None,
                    phase: 2,
                    ordering: 0,
                    service: None,
                    safety_guard: SafetyGuard::default(),
                },
            ],
            intervention_points: vec![],
            explanation_trace: String::new(),
            confidence: 1.0,
            provenance: "test".into(),
        };

        let plan = FunctionExecutor::new().execute(&mission, &snap, &own_state(), 100.0);

        assert!(plan.has_lethal_intent(), "fire should be emitted");
        assert_eq!(
            plan.intents
                .iter()
                .filter(|intent| matches!(intent.command, PlatformCommand::WeaponSafeAll { .. }))
                .count(),
            1,
            "only the pre-engagement recon posture should safe weapons"
        );
    }

    #[test]
    fn rtb_lowers_to_goto_intent() {
        let snap = snapshot_with_target(30.05, 120.05);
        let mission = compile("所有无人机返航", &snap);
        let plan = FunctionExecutor::new().execute(&mission, &snap, &own_state(), 100.0);
        assert!(plan
            .intents
            .iter()
            .any(|i| matches!(i.command, PlatformCommand::GotoLocation { .. })));
    }
}
