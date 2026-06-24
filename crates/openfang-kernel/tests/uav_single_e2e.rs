//! Track 1 (single-UAV) end-to-end closed-loop tests.
//!
//! Verifies the air-domain reflex path through the live control loop:
//! `poll_all` (low-fuel air platform) → DCC `auto_rtb_on_low_fuel` →
//! cerebellum → TacticalPipeline → adapter. This is the "门禁 DCC RTB" gate
//! from the plan's 1G, and it exercises the air-domain broadening of
//! `UavFuelCritical` (CCA `platform_type` is not literally `"uav"`).

use std::sync::Arc;

use openfang_kernel::platform_control::PlatformControlLoop;
use openfang_platform::{AdapterRegistry, MockAdapter};
use openfang_runtime::audit::AuditLog;
use openfang_runtime::op_restrictions::OpRestrictionsManager;
use openfang_runtime::playbook_scheduler::{MissionDecomposer, PlaybookScheduler};
use openfang_types::platform::{
    Affiliation, Domain, FuelStatus, PlatformCapabilities, PlatformCommand, PlatformState, Pose,
    Velocity, WorldSnapshot,
};
use openfang_types::umaa::{AutonomyMode, CommPlan, MissionConfig};
use openfang_types::umaa::{PlatformLimits, RulesOfEngagement};

fn caps() -> PlatformCapabilities {
    PlatformCapabilities {
        supports_motion_control: true,
        supports_sensor_control: true,
        supports_weapon_control: true,
        supports_jammer_control: true,
        supports_comm_control: true,
        supports_uav_launch_recovery: true,
        supports_formation_control: true,
        supports_handoff: true,
        max_platforms: 8,
        supports_simulation: true,
        supports_hardware: false,
    }
}

fn low_fuel_cca(fuel_pct: f64) -> WorldSnapshot {
    WorldSnapshot {
        timestamp: 1.0,
        platforms: vec![PlatformState {
            id: "cca-1".into(),
            name: "CCA-1".into(),
            platform_type: "cca".into(), // deliberately NOT "uav"
            affiliation: Affiliation::Blue,
            domain: Domain::Air,
            pose: Pose {
                lat_deg: 30.0,
                lon_deg: 120.0,
                alt_m: 4000.0,
                heading_deg: 0.0,
                pitch_deg: 0.0,
                roll_deg: 0.0,
            },
            velocity: Velocity {
                speed_ms: 200.0,
                vertical_rate_ms: 0.0,
                course_deg: 0.0,
            },
            fuel: FuelStatus {
                remaining_kg: fuel_pct * 1000.0,
                max_kg: 1000.0,
                consumption_rate_kg_s: 0.1,
            },
            damage: 0.0,
            tracks: vec![],
            onboard_sensors: vec![],
            onboard_weapons: vec![],
            onboard_jammers: vec![],
            current_target: None,
            commander: None,
            survivability: None,
            emcon: None,
            link: None,
        }],
        active_munitions: vec![],
        events: vec![],
        fleet: None,
    }
}

async fn loop_with_snapshot(
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

    let restrictions = Arc::new(OpRestrictionsManager::new(
        RulesOfEngagement::default(),
        PlatformLimits::default(),
    ));
    let mut lp = PlatformControlLoop::new(
        registry,
        restrictions,
        Arc::new(AuditLog::new()),
        caps(),
        "cca-1",
        20.0,
        256,
        2,
        30.0,
    );
    lp.install_default_dcc_rules();
    (lp, log)
}

#[tokio::test]
async fn low_fuel_cca_triggers_rtb_through_dcc() {
    // 8% fuel < the 12% reserve threshold.
    let (mut lp, log) = loop_with_snapshot(low_fuel_cca(0.08)).await;
    let report = lp.step().await;

    assert!(report.polled, "control loop should poll the snapshot");
    assert!(
        report.dcc_intents >= 1,
        "low-fuel reflex should fire for an air platform"
    );
    assert!(
        report.pipeline.dispatched >= 1,
        "RTB should pass the gate and dispatch"
    );

    let sent = log.lock().unwrap();
    assert!(
        sent.iter()
            .any(|c| matches!(c, PlatformCommand::ReturnToBase { .. })),
        "an RTB command must reach the adapter, got {sent:?}"
    );
}

#[tokio::test]
async fn healthy_fuel_cca_does_not_rtb() {
    // 90% fuel — no DCC low-fuel reflex should fire. The live loop may still
    // emit non-emergency role-posture commands on the first tick.
    let (mut lp, log) = loop_with_snapshot(low_fuel_cca(0.90)).await;
    let report = lp.step().await;

    assert!(report.polled);
    assert_eq!(
        report.dcc_intents, 0,
        "healthy fuel must not trigger DCC reflexes"
    );
    let sent = log.lock().unwrap();
    assert!(
        sent.iter()
            .all(|c| !matches!(c, PlatformCommand::ReturnToBase { .. })),
        "healthy fuel must not dispatch RTB, got {sent:?}"
    );
}

#[tokio::test]
async fn patrol_mission_dispatches_motion_and_sensor_commands() {
    let (mut lp, log) = loop_with_snapshot(low_fuel_cca(0.90)).await;
    let mission = MissionConfig {
        mission_id: "patrol-e2e".into(),
        roe: RulesOfEngagement::default(),
        geofences: vec![],
        platform_limits: PlatformLimits::default(),
        comm_plan: CommPlan::default(),
        contingency_plans: vec![],
        activated_at: None,
        autonomy_mode: AutonomyMode::HumanSupervised,
        phase: Some("patrol".into()),
        objectives: vec![],
        allocations: vec![],
        target_track_id: None,
        play_name: None,
    };
    let mut tasks = MissionDecomposer::new().decompose(&mission);
    let mut task = tasks
        .pop()
        .expect("patrol phase should decompose to a task");
    task.assignee = "cca-1".into();
    task.params["heading_deg"] = serde_json::json!(45.0);
    task.params["speed_ms"] = serde_json::json!(25.0);
    task.params["sensor_id"] = serde_json::json!("radar");

    let scheduled = PlaybookScheduler::new().schedule(task, 1.0).unwrap();
    lp.set_active_plan(scheduled.intents);
    let report = lp.step().await;

    assert!(report.polled);
    assert!(
        report.pipeline.dispatched >= 2,
        "patrol should dispatch motion + sensor commands"
    );
    let sent = log.lock().unwrap();
    assert!(sent.iter().any(|c| matches!(
        c,
        PlatformCommand::SetHeading {
            platform_id,
            heading_deg,
            speed_ms: Some(25.0),
            ..
        } if platform_id == "cca-1" && (*heading_deg - 45.0).abs() < f64::EPSILON
    )));
    assert!(sent.iter().any(|c| matches!(
        c,
        PlatformCommand::SensorOn {
            platform_id,
            sensor_id
        } if platform_id == "cca-1" && sensor_id == "radar"
    )));
}
