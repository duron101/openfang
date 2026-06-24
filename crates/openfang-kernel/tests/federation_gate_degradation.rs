//! M4-U6 (review fix) — federation degradation must change *gate dispatch*,
//! not just the reported status string.
//!
//! The earlier M4-U6 landing computed a `FederationStatus` with
//! `effective_profile = degraded_profile` under a `Poor`/`Lost` link, but the
//! gate kept enforcing the operator profile — the degradation was cosmetic.
//! These tests drive the *real* `PlatformControlLoop` and assert the adapter
//! dispatch outcome flips when the link degrades, and that a stale dangerous
//! intent (a fire order from before a blackout) is dropped before the pipeline.

use std::sync::Arc;

use openfang_kernel::platform_control::PlatformControlLoop;
use openfang_platform::{AdapterRegistry, MockAdapter};
use openfang_runtime::audit::AuditLog;
use openfang_runtime::mission_approval::MissionApprovalRegistry;
use openfang_runtime::op_restrictions::OpRestrictionsManager;
use openfang_runtime::target_authorization::TargetAuthorizationRegistry;
use openfang_types::config::{
    AutonomyConfig, AutonomyModeProfile, FederationConfig, PlatformConfig, PlatformMode,
    WeaponDisposition,
};
use openfang_types::platform::{LinkQuality, PlatformCapabilities, PlatformCommand, WorldSnapshot};
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

fn restrictions() -> Arc<OpRestrictionsManager> {
    Arc::new(OpRestrictionsManager::new(
        RulesOfEngagement {
            weapon_release_authority: WeaponReleaseLevel::WeaponsTight,
            ..Default::default()
        },
        PlatformLimits::default(),
    ))
}

/// Operator profile = `supervised_autonomy` (motion auto-dispatches).
/// Degraded profile = `observe_only` (motion is advisory → nothing actuates).
/// The crisp, gate-observable contrast lets the test prove the *gate* honours
/// the degraded profile, not just the status report.
fn degradable_config() -> PlatformConfig {
    PlatformConfig {
        mode: PlatformMode::Simulation,
        own_platform_id: "usv-01".into(),
        autonomy: AutonomyConfig {
            active_profile: "supervised_autonomy".into(),
            degraded_profile: Some("observe_only".into()),
            profiles: vec![
                AutonomyModeProfile {
                    id: "supervised_autonomy".into(),
                    description: "motion auto; weapons pending".into(),
                    auto_classes: vec!["motion".into(), "sensor".into(), "comm".into()],
                    weapon_disposition: WeaponDisposition::PendingApproval,
                    max_weapon_release: WeaponReleaseLevel::WeaponsTight,
                    allow_defensive_reflex: true,
                    ..AutonomyModeProfile::default()
                },
                AutonomyModeProfile {
                    id: "observe_only".into(),
                    description: "advisory only — no actuation".into(),
                    auto_classes: vec![],
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
                    ..AutonomyModeProfile::default()
                },
            ],
        },
        federation: FederationConfig {
            priority_order: vec!["usv-01".into()],
            member_id: String::new(),
            stale_command_window_s: 10.0,
        },
        ..Default::default()
    }
}

fn snapshot_at(timestamp: f64) -> WorldSnapshot {
    WorldSnapshot {
        timestamp,
        platforms: vec![],
        active_munitions: vec![],
        events: vec![],
        fleet: None,
    }
}

async fn build_loop_at(
    cfg: &PlatformConfig,
    clock_time: f64,
) -> (
    PlatformControlLoop,
    Arc<std::sync::Mutex<Vec<PlatformCommand>>>,
) {
    let registry = Arc::new(AdapterRegistry::new());
    // Permanent fallback snapshot fixes the loop clock at `clock_time` on every
    // poll, so intent age (now - issued_at) is deterministic across steps.
    let mock = MockAdapter::new("primary").with_snapshot(snapshot_at(clock_time));
    let log = mock.sent_handle();
    registry.set_primary(Box::new(mock));
    registry.connect_all().await.unwrap();

    let lp = PlatformControlLoop::from_config(
        registry,
        cfg,
        restrictions(),
        Arc::new(AuditLog::new()),
        full_caps(),
        Arc::new(TargetAuthorizationRegistry::new()),
        Arc::new(MissionApprovalRegistry::new()),
    );
    (lp, log)
}

async fn build_loop(
    cfg: &PlatformConfig,
) -> (
    PlatformControlLoop,
    Arc<std::sync::Mutex<Vec<PlatformCommand>>>,
) {
    build_loop_at(cfg, 0.0).await
}

fn motion_intent(issued_at: f64) -> CandidateIntent {
    CandidateIntent::new(
        PlatformCommand::SetHeading {
            platform_id: "usv-01".into(),
            heading_deg: 30.0,
            speed_ms: Some(6.0),
            turn_direction: None,
        },
        CommandPriority::Normal,
        IntentSource::Llm {
            agent_id: "nav".into(),
        },
        issued_at,
        "degradation:motion",
    )
}

fn weapon_intent(issued_at: f64) -> CandidateIntent {
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
        issued_at,
        "degradation:weapon",
    )
}

#[tokio::test]
async fn link_loss_actually_swaps_the_gate_not_just_the_report() {
    let cfg = degradable_config();
    let (mut lp, log) = build_loop(&cfg).await;

    // ── Healthy link: operator (supervised_autonomy) → motion dispatches.
    lp.submit_intent(motion_intent(0.0));
    let healthy = lp.step().await;
    assert!(!healthy.link_degraded, "healthy link must not degrade");
    assert_eq!(
        healthy.pipeline.dispatched, 1,
        "supervised_autonomy must dispatch motion under a healthy link"
    );
    assert_eq!(log.lock().unwrap().len(), 1);

    // ── Link Lost: degraded profile (observe_only) must take the GATE, so the
    // exact same motion intent now actuates NOTHING. This is the core fix:
    // before it, the gate kept dispatching under supervised_autonomy.
    *lp.link_quality_override_handle().write().unwrap() = Some(LinkQuality::Lost);
    lp.submit_intent(motion_intent(0.0));
    let degraded = lp.step().await;
    assert!(degraded.link_degraded, "Lost link must force degradation");
    assert_eq!(
        degraded.pipeline.dispatched, 0,
        "under degradation the gate must enforce observe_only (no actuation)"
    );
    assert_eq!(
        log.lock().unwrap().len(),
        1,
        "no new command should reach the adapter while degraded"
    );

    // ── Link restored: operator profile returns losslessly, motion flows.
    *lp.link_quality_override_handle().write().unwrap() = Some(LinkQuality::Excellent);
    lp.submit_intent(motion_intent(0.0));
    let recovered = lp.step().await;
    assert!(!recovered.link_degraded, "Excellent link must recover");
    assert_eq!(
        recovered.pipeline.dispatched, 1,
        "after recovery the operator profile must dispatch motion again"
    );
    assert_eq!(log.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn stale_fire_order_is_dropped_before_the_pipeline() {
    let cfg = degradable_config();
    // Fix the loop clock at t=1000 so a FireAtTarget issued at t=0 is 1000s old
    // — far past the 10s staleness window.
    let (mut lp, log) = build_loop_at(&cfg, 1000.0).await;

    // Stale fire order (issued long before the blackout) must be dropped and
    // never parked in the pending queue nor dispatched.
    lp.submit_intent(weapon_intent(0.0));
    let stale = lp.step().await;
    assert_eq!(
        stale.stale_dropped, 1,
        "a fire order older than the staleness window must be dropped"
    );
    assert_eq!(
        stale.pipeline.pending, 0,
        "a stale fire order must not be parked for approval (replay vector)"
    );
    assert_eq!(stale.pipeline.dispatched, 0);
    assert!(log.lock().unwrap().is_empty());

    // A FRESH fire order at the current time survives the staleness filter and
    // proceeds to the WMS quorum (pending/rejected) — proving the filter is
    // age-scoped, not a blanket weapon block.
    lp.submit_intent(weapon_intent(1000.0));
    let fresh = lp.step().await;
    assert_eq!(
        fresh.stale_dropped, 0,
        "a fresh fire order must not be dropped"
    );
    assert!(
        fresh.pipeline.pending + fresh.pipeline.rejected >= 1,
        "a fresh fire order must reach the WMS quorum (pending/rejected), not vanish"
    );
}
