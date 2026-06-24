//! MMS integration — deterministic route planning wired through the control loop.

use std::sync::Arc;

use openfang_kernel::platform_control::PlatformControlLoop;
use openfang_platform::{AdapterRegistry, MockAdapter};
use openfang_runtime::audit::AuditLog;
use openfang_runtime::geo_zones::NavIntent;
use openfang_runtime::intent_extractor::{FlankSide, ManeuverIntent, StructuredIntent};
use openfang_runtime::mission_approval::MissionApprovalRegistry;
use openfang_runtime::op_restrictions::OpRestrictionsManager;
use openfang_runtime::target_authorization::TargetAuthorizationRegistry;
use openfang_types::config::{
    AutonomyConfig, AutonomyModeProfile, GeoZoneConfig, PlatformConfig, PlatformMode,
    WeaponDisposition,
};
use openfang_types::platform::{
    Affiliation, Domain, FuelStatus, PlatformCapabilities, PlatformCommand, PlatformState, Pose,
    Track, Velocity, WorldSnapshot,
};
use openfang_types::umaa::{PlatformLimits, RulesOfEngagement, WeaponReleaseLevel};

fn full_caps() -> PlatformCapabilities {
    PlatformCapabilities {
        supports_motion_control: true,
        supports_sensor_control: true,
        supports_weapon_control: true,
        supports_jammer_control: true,
        supports_comm_control: true,
        supports_uav_launch_recovery: true,
        supports_formation_control: true,
        supports_handoff: true,
        max_platforms: 4,
        supports_simulation: true,
        supports_hardware: false,
    }
}

fn restrictions() -> Arc<OpRestrictionsManager> {
    Arc::new(OpRestrictionsManager::new(
        RulesOfEngagement {
            weapon_release_authority: WeaponReleaseLevel::WeaponsTight,
            ..Default::default()
        },
        PlatformLimits::default(),
    ))
}

fn own_state() -> PlatformState {
    let mut s = PlatformState::minimal("usv-01");
    s.affiliation = Affiliation::Blue;
    s.domain = Domain::Surface;
    s.pose = Pose {
        lat_deg: 30.0,
        lon_deg: 120.0,
        alt_m: 0.0,
        heading_deg: 0.0,
        pitch_deg: 0.0,
        roll_deg: 0.0,
    };
    s.velocity = Velocity {
        speed_ms: 8.0,
        vertical_rate_ms: 0.0,
        course_deg: 45.0,
    };
    s.fuel = FuelStatus {
        remaining_kg: 80.0,
        max_kg: 100.0,
        consumption_rate_kg_s: 0.1,
    };
    s
}

fn snapshot() -> WorldSnapshot {
    WorldSnapshot {
        timestamp: 1.0,
        platforms: vec![own_state()],
        active_munitions: vec![],
        events: vec![],
        fleet: None,
    }
}

fn snapshot_with_track() -> WorldSnapshot {
    let mut own = own_state();
    own.tracks = vec![Track {
        track_id: "self:3".into(),
        target_name: "blue_patrol_3".into(),
        classification: "surface".into(),
        affiliation: Affiliation::Red,
        iff: "foe".into(),
        position_lla: Some((30.05, 120.05, 0.0)),
        heading_deg: Some(0.0),
        speed_ms: Some(6.0),
        range_m: Some(7000.0),
        bearing_deg: Some(45.0),
        elevation_deg: None,
        quality: 0.9,
        stale: false,
        last_update_s: 1.0,
        is_active: true,
    }];
    WorldSnapshot {
        timestamp: 1.0,
        platforms: vec![own],
        active_munitions: vec![],
        events: vec![],
        fleet: None,
    }
}

fn config_with_profiles(active: &str) -> PlatformConfig {
    PlatformConfig {
        mode: PlatformMode::Simulation,
        own_platform_id: "usv-01".into(),
        geo_zones: vec![GeoZoneConfig {
            id: "sector_north".into(),
            kind: "area".into(),
            aliases: vec!["north sector".into()],
            polygon: vec![
                (30.05, 120.05),
                (30.05, 120.06),
                (30.06, 120.06),
                (30.06, 120.05),
            ],
            point: Some((30.055, 120.055)),
            alt_band_m: [0.0, 500.0],
            patrol_pattern: None,
        }],
        autonomy: AutonomyConfig {
            active_profile: active.into(),
            profiles: vec![
                AutonomyModeProfile {
                    id: "observe_only".into(),
                    description: "advisory".into(),
                    auto_classes: vec![],
                    pending_approval_classes: vec![],
                    advisory_classes: vec!["motion".into()],
                    weapon_disposition: WeaponDisposition::SuggestOnly,
                    max_weapon_release: WeaponReleaseLevel::WeaponsHold,
                    allow_defensive_reflex: false,
                    prompt_template: None,
                },
                AutonomyModeProfile {
                    id: "supervised_autonomy".into(),
                    description: "motion auto".into(),
                    auto_classes: vec!["motion".into()],
                    pending_approval_classes: vec![],
                    advisory_classes: vec![],
                    weapon_disposition: WeaponDisposition::PendingApproval,
                    max_weapon_release: WeaponReleaseLevel::WeaponsTight,
                    allow_defensive_reflex: true,
                    prompt_template: None,
                },
            ],
            degraded_profile: None,
        },
        ..Default::default()
    }
}

async fn loop_with(
    active: &str,
) -> (
    PlatformControlLoop,
    Arc<std::sync::Mutex<Vec<PlatformCommand>>>,
) {
    loop_with_snapshot(active, snapshot()).await
}

async fn loop_with_snapshot(
    active: &str,
    snap: WorldSnapshot,
) -> (
    PlatformControlLoop,
    Arc<std::sync::Mutex<Vec<PlatformCommand>>>,
) {
    let registry = Arc::new(AdapterRegistry::new());
    let mock = MockAdapter::new("primary").with_snapshot(snap);
    let log = mock.sent_handle();
    registry.set_primary(Box::new(mock));
    registry.connect_all().await.unwrap();

    let lp = PlatformControlLoop::from_config(
        registry,
        &config_with_profiles(active),
        restrictions(),
        Arc::new(AuditLog::new()),
        full_caps(),
        Arc::new(TargetAuthorizationRegistry::new()),
        Arc::new(MissionApprovalRegistry::new()),
    );
    (lp, log)
}

#[tokio::test]
async fn mms_goto_zone_dispatches_follow_route() {
    let (mut lp, log) = loop_with("supervised_autonomy").await;
    lp.set_mms_nav_intent(Some(NavIntent::GotoZone {
        zone: "sector_north".into(),
    }));

    let report = lp.step().await;

    assert!(report.mms_intents >= 1, "MMS should emit route intent");
    assert!(
        report.pipeline.dispatched >= 1,
        "FollowRoute should reach adapter under supervised_autonomy"
    );
    let sent = log.lock().unwrap();
    assert!(
        sent.iter()
            .any(|c| matches!(c, PlatformCommand::FollowRoute { .. })),
        "adapter should receive FollowRoute; got {sent:?}"
    );
    assert!(
        lp.latest_route_plan()
            .map(|p| p.feasible && !p.waypoints.is_empty())
            .unwrap_or(false),
        "route plan should be feasible with waypoints"
    );
}

#[tokio::test]
async fn mms_observe_only_advises_route_without_dispatch() {
    let (mut lp, log) = loop_with("observe_only").await;
    lp.set_mms_nav_intent(Some(NavIntent::GotoZone {
        zone: "sector_north".into(),
    }));

    let report = lp.step().await;

    assert!(report.mms_intents >= 1);
    assert_eq!(
        report.pipeline.dispatched, 0,
        "observe_only must not dispatch MMS FollowRoute"
    );
    assert!(log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn mms_objective_input_dispatches_direct_maneuver() {
    let (mut lp, log) = loop_with("supervised_autonomy").await;
    assert!(lp.set_mms_objective("self 左转，速度5米每秒"));

    let report = lp.step().await;

    assert!(
        report.mms_intents >= 1,
        "MMS should emit direct maneuver intents"
    );
    assert!(report.pipeline.dispatched >= 1);
    let sent = log.lock().unwrap();
    assert!(
        sent.iter().any(|c| matches!(
            c,
            PlatformCommand::SetHeading {
                heading_deg,
                speed_ms: Some(speed),
                ..
            } if (*heading_deg - 270.0).abs() < 0.01 && (*speed - 5.0).abs() < 0.01
        )),
        "adapter should receive SetHeading carrying speed; got {sent:?}"
    );
}

#[tokio::test]
async fn mms_objective_input_dispatches_speed_only() {
    let (mut lp, log) = loop_with("supervised_autonomy").await;
    assert!(lp.set_mms_objective("速度5米每秒"));

    let report = lp.step().await;

    assert!(report.mms_intents >= 1, "MMS should emit speed intent");
    assert!(report.pipeline.dispatched >= 1);
    let sent = log.lock().unwrap();
    assert!(
        sent.iter().any(
            |c| matches!(c, PlatformCommand::SetSpeed { speed_ms, .. } if (*speed_ms - 5.0).abs() < 0.01)
        ),
        "adapter should receive SetSpeed; got {sent:?}"
    );
}

#[tokio::test]
async fn brain_structured_intent_syncs_flank_route_to_mms() {
    let (mut lp, log) = loop_with_snapshot("supervised_autonomy", snapshot_with_track()).await;
    let mut intent = StructuredIntent::unknown("绕后接近 blue_patrol_3 保持安全距离3公里");
    intent.target_track_ids = vec!["self:3".into()];
    intent.standoff_m = Some(3000.0);
    intent.flank_side = Some(FlankSide::Left);
    intent.maneuver = ManeuverIntent {
        flank_approach: true,
        ..Default::default()
    };

    assert!(lp.sync_mms_from_structured_intent(&intent));
    let report = lp.step().await;

    assert!(
        report.mms_intents >= 1,
        "MMS should emit flank route intent"
    );
    assert!(report.pipeline.dispatched >= 1);
    let sent = log.lock().unwrap();
    assert!(
        sent.iter()
            .any(|c| matches!(c, PlatformCommand::FollowRoute { .. })),
        "adapter should receive FollowRoute from MMS; got {sent:?}"
    );
    assert!(
        lp.latest_route_plan()
            .map(|p| p.feasible && p.waypoints.len() >= 2)
            .unwrap_or(false),
        "MMS flank route should be feasible with waypoints"
    );
}
