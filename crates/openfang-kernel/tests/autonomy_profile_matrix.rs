//! M3-U5 — single-vessel autonomy profile matrix.
//!
//! Verifies that the *gate-side* half of the dual-landing actually changes
//! dispatch behaviour as the active [`AutonomyModeProfile`] is hot-swapped:
//!
//! * `observe_only` — every executable class is advisory; nothing reaches
//!   the adapter, even though intents are submitted normally.
//! * `supervised_autonomy` — motion auto-dispatches; weapons are deferred
//!   to the pending-approval queue (combined with the existing WMS quorum
//!   gate, which independently turns weapons into pending anyway).
//! * `defensive_autonomy` — motion auto-dispatches; defensive reflex
//!   priority is honoured (the profile sets `allow_defensive_reflex=true`).
//!
//! The test also exercises the kernel-level
//! [`PlatformControlLoop::set_autonomy_profile`] hot-swap path used by
//! `PUT /api/autonomy/profile` so the runtime override and the gate
//! profile remain consistent.

use std::sync::Arc;

use openfang_kernel::platform_control::PlatformControlLoop;
use openfang_platform::{AdapterRegistry, MockAdapter};
use openfang_runtime::audit::AuditLog;
use openfang_runtime::mission_approval::MissionApprovalRegistry;
use openfang_runtime::op_restrictions::OpRestrictionsManager;
use openfang_runtime::target_authorization::TargetAuthorizationRegistry;
use openfang_types::config::{
    AutonomyConfig, AutonomyModeProfile, PlatformConfig, PlatformMode, WeaponDisposition,
};
use openfang_types::platform::{PlatformCapabilities, PlatformCommand};
use openfang_types::tactical::{CandidateIntent, CommandPriority, IntentSource};
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

fn restrictions_weapons_tight() -> Arc<OpRestrictionsManager> {
    Arc::new(OpRestrictionsManager::new(
        RulesOfEngagement {
            weapon_release_authority: WeaponReleaseLevel::WeaponsTight,
            ..Default::default()
        },
        PlatformLimits::default(),
    ))
}

fn three_profile_config(active: &str) -> PlatformConfig {
    PlatformConfig {
        mode: PlatformMode::Simulation,
        own_platform_id: "usv-01".into(),
        autonomy: AutonomyConfig {
            active_profile: active.into(),
            profiles: vec![
                AutonomyModeProfile {
                    id: "observe_only".into(),
                    description: "advisory only — no actuation".into(),
                    auto_classes: vec![],
                    pending_approval_classes: vec![],
                    advisory_classes: vec![
                        "motion".into(),
                        "sensor".into(),
                        "comm".into(),
                        "ew".into(),
                        "aux".into(),
                    ],
                    weapon_disposition: WeaponDisposition::SuggestOnly,
                    max_weapon_release: WeaponReleaseLevel::WeaponsHold,
                    allow_defensive_reflex: false,
                    prompt_template: None,
                },
                AutonomyModeProfile {
                    id: "supervised_autonomy".into(),
                    description: "motion/sensor/comm auto; weapons pending".into(),
                    auto_classes: vec!["motion".into(), "sensor".into(), "comm".into()],
                    pending_approval_classes: vec![],
                    advisory_classes: vec![],
                    weapon_disposition: WeaponDisposition::PendingApproval,
                    max_weapon_release: WeaponReleaseLevel::WeaponsTight,
                    allow_defensive_reflex: true,
                    prompt_template: None,
                },
                AutonomyModeProfile {
                    id: "defensive_autonomy".into(),
                    description: "defensive reflexes auto; weapons constrained".into(),
                    auto_classes: vec![
                        "motion".into(),
                        "sensor".into(),
                        "ew".into(),
                        "comm".into(),
                    ],
                    pending_approval_classes: vec![],
                    advisory_classes: vec![],
                    weapon_disposition: WeaponDisposition::AutoConstrained,
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
    active_profile: &str,
) -> (
    PlatformControlLoop,
    Arc<std::sync::Mutex<Vec<PlatformCommand>>>,
) {
    let registry = Arc::new(AdapterRegistry::new());
    let mock = MockAdapter::new("primary");
    let log = mock.sent_handle();
    registry.set_primary(Box::new(mock));
    registry.connect_all().await.unwrap();

    let target_registry = Arc::new(TargetAuthorizationRegistry::new());
    let mission_approvals = Arc::new(MissionApprovalRegistry::new());
    let cfg = three_profile_config(active_profile);
    let lp = PlatformControlLoop::from_config(
        registry,
        &cfg,
        restrictions_weapons_tight(),
        Arc::new(AuditLog::new()),
        full_caps(),
        target_registry,
        mission_approvals,
    );
    (lp, log)
}

fn motion_intent() -> CandidateIntent {
    CandidateIntent::new(
        PlatformCommand::SetHeading {
            platform_id: "usv-01".into(),
            heading_deg: 45.0,
            speed_ms: Some(8.0),
            turn_direction: None,
        },
        CommandPriority::Normal,
        IntentSource::Llm {
            agent_id: "nav".into(),
        },
        0.0,
        "matrix:motion",
    )
}

fn weapon_intent() -> CandidateIntent {
    CandidateIntent::new(
        PlatformCommand::FireAtTarget {
            platform_id: "usv-01".into(),
            weapon_id: "w1".into(),
            track_id: "t1".into(),
        },
        CommandPriority::Normal,
        IntentSource::Llm {
            agent_id: "fca".into(),
        },
        0.0,
        "matrix:weapon",
    )
}

#[tokio::test]
async fn observe_only_advises_and_dispatches_nothing() {
    let (mut lp, log) = loop_with("observe_only").await;
    lp.submit_intent(motion_intent());
    lp.submit_intent(weapon_intent());

    let report = lp.step().await;

    assert_eq!(
        report.pipeline.dispatched, 0,
        "observe_only must not dispatch anything to the adapter"
    );
    assert!(
        report.pipeline.pending == 0,
        "observe_only must not park weapon intents in the pending queue \
         (they should be rejected as advisory)"
    );
    assert!(
        report.pipeline.rejected >= 2,
        "observe_only must reject all executable classes as advisory; got {:?}",
        report.pipeline,
    );
    assert!(
        log.lock().unwrap().is_empty(),
        "observe_only must not actuate the adapter"
    );
}

#[tokio::test]
async fn supervised_autonomy_dispatches_motion_holds_weapon() {
    let (mut lp, log) = loop_with("supervised_autonomy").await;
    lp.submit_intent(motion_intent());
    lp.submit_intent(weapon_intent());

    let report = lp.step().await;

    assert_eq!(
        report.pipeline.dispatched, 1,
        "supervised_autonomy must dispatch motion intents"
    );
    assert!(
        report.pipeline.pending + report.pipeline.rejected >= 1,
        "supervised_autonomy must hold or reject weapon intents (never auto-dispatch)"
    );
    assert_eq!(
        log.lock().unwrap().len(),
        1,
        "only the motion command should reach the adapter; got {:?}",
        log.lock().unwrap()
    );
}

#[tokio::test]
async fn defensive_autonomy_dispatches_motion_and_constrains_weapons() {
    let (mut lp, log) = loop_with("defensive_autonomy").await;
    lp.submit_intent(motion_intent());
    lp.submit_intent(weapon_intent());

    let report = lp.step().await;

    assert_eq!(
        report.pipeline.dispatched, 1,
        "defensive_autonomy auto-classes (incl. motion) must dispatch"
    );
    // Weapons under defensive_autonomy are AutoConstrained — the profile gate
    // passes them through, but the deterministic WMS quorum still defers the
    // engagement, so the adapter never receives the raw FireAtTarget.
    assert!(
        report.pipeline.pending + report.pipeline.rejected >= 1,
        "weapons must remain under WMS quorum even when the profile allows auto-constrained"
    );
    let sent = log.lock().unwrap();
    assert_eq!(sent.len(), 1, "only motion reaches adapter; got {sent:?}");
    assert!(matches!(sent[0], PlatformCommand::SetHeading { .. }));
}

#[tokio::test]
async fn hot_swap_profile_changes_next_tick_dispatch_outcome() {
    let (mut lp, log) = loop_with("observe_only").await;

    // Tick 1 under observe_only — should reject motion as advisory.
    lp.submit_intent(motion_intent());
    let r1 = lp.step().await;
    assert_eq!(r1.pipeline.dispatched, 0, "observe_only rejects motion");
    assert_eq!(log.lock().unwrap().len(), 0);

    // Hot-swap → supervised_autonomy.
    let supervised = three_profile_config("supervised_autonomy")
        .autonomy
        .profile("supervised_autonomy")
        .cloned()
        .expect("supervised profile present");
    let prev = lp.set_autonomy_profile(supervised);
    assert_eq!(
        prev.as_deref(),
        Some("observe_only"),
        "set_autonomy_profile must return the previous id"
    );

    // Tick 2 under supervised_autonomy — same motion intent now flows.
    lp.submit_intent(motion_intent());
    let r2 = lp.step().await;
    assert_eq!(
        r2.pipeline.dispatched, 1,
        "after hot-swap motion must reach the adapter"
    );
    assert_eq!(log.lock().unwrap().len(), 1);
}
