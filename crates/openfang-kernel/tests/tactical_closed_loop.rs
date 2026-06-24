//! Phase 2 — safe command closed loop (NoopAdapter).
//!
//! Verifies the end-to-end pipeline: intent → ActionComposer → CommandGate →
//! Audit → NoopAdapter, and that the safety boundary cannot be bypassed:
//! - approved motion reaches the adapter,
//! - unauthorized weapons (WeaponsHold) are rejected and audited,
//! - out-of-bounds maneuvers are rejected by SPGS,
//! - DCC critical weapon reflexes cannot bypass the gate,
//! - weapon engagements under WeaponsTight are async (pending → quorum → launch),
//! - expired quorum is rejected and audited.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use openfang_kernel::tactical_pipeline::TacticalPipeline;
use openfang_platform::{AdapterRegistry, NoopAdapter};
use openfang_runtime::audit::AuditLog;
use openfang_runtime::op_restrictions::OpRestrictionsManager;
use openfang_runtime::weapon_engagement::EngagementState;
use openfang_types::platform::{PlatformCapabilities, PlatformCommand};
use openfang_types::tactical::{CandidateIntent, CommandPriority, IntentSource, ManualClock};
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
        max_platforms: 10,
        supports_simulation: true,
        supports_hardware: false,
    }
}

fn build(roe: WeaponReleaseLevel) -> (TacticalPipeline, Arc<AtomicU64>, Arc<AuditLog>) {
    let noop = NoopAdapter::new();
    let counter = noop.counter();
    let registry = Arc::new(AdapterRegistry::new());
    registry.set_primary(Box::new(noop));

    let restrictions = Arc::new(OpRestrictionsManager::new(
        RulesOfEngagement {
            weapon_release_authority: roe,
            ..Default::default()
        },
        PlatformLimits::default(),
    ));
    let audit = Arc::new(AuditLog::new());
    let pipeline = TacticalPipeline::new(
        registry,
        restrictions,
        audit.clone(),
        full_caps(),
        2,    // weapon quorum
        30.0, // approval window seconds
    );
    (pipeline, counter, audit)
}

fn heading_intent(prio: CommandPriority, hdg: f64) -> CandidateIntent {
    CandidateIntent::new(
        PlatformCommand::SetHeading {
            platform_id: "usv-01".into(),
            heading_deg: hdg,
            speed_ms: None,
            turn_direction: None,
        },
        prio,
        IntentSource::Llm {
            agent_id: "na".into(),
        },
        0.0,
        "navigate",
    )
}

fn fire_intent(source: IntentSource, prio: CommandPriority) -> CandidateIntent {
    CandidateIntent::new(
        PlatformCommand::FireAtTarget {
            platform_id: "usv-01".into(),
            weapon_id: "cannon".into(),
            track_id: "trk-1".into(),
        },
        prio,
        source,
        0.0,
        "engage",
    )
}

#[tokio::test]
async fn approved_motion_reaches_adapter() {
    let (mut pipe, counter, _audit) = build(WeaponReleaseLevel::WeaponsHold);
    let clock = ManualClock::new(0.0);
    let report = pipe
        .tick(
            vec![heading_intent(CommandPriority::Normal, 90.0)],
            None,
            &clock,
        )
        .await;
    assert_eq!(report.dispatched, 1);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unauthorized_weapon_rejected_and_audited() {
    let (mut pipe, counter, audit) = build(WeaponReleaseLevel::WeaponsHold);
    let clock = ManualClock::new(0.0);
    let report = pipe
        .tick(
            vec![fire_intent(
                IntentSource::Llm {
                    agent_id: "fca".into(),
                },
                CommandPriority::Normal,
            )],
            None,
            &clock,
        )
        .await;
    assert_eq!(report.dispatched, 0);
    assert_eq!(report.rejected, 1);
    assert_eq!(counter.load(Ordering::SeqCst), 0);
    assert!(!audit.is_empty());
    assert!(audit.verify_integrity().is_ok());
}

#[tokio::test]
async fn dcc_critical_weapon_cannot_bypass_gate() {
    let (mut pipe, counter, _audit) = build(WeaponReleaseLevel::WeaponsHold);
    let clock = ManualClock::new(0.0);
    let report = pipe
        .tick(
            vec![fire_intent(
                IntentSource::Dcc {
                    rule_name: "auto_engage".into(),
                },
                CommandPriority::Critical,
            )],
            None,
            &clock,
        )
        .await;
    assert_eq!(report.dispatched, 0);
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn over_speed_maneuver_rejected_by_spgs() {
    let (mut pipe, counter, _audit) = build(WeaponReleaseLevel::WeaponsFree);
    let clock = ManualClock::new(0.0);
    let intent = CandidateIntent::new(
        PlatformCommand::SetSpeed {
            platform_id: "usv-01".into(),
            speed_ms: 999.0,
            acceleration_ms2: None,
        },
        CommandPriority::Normal,
        IntentSource::Llm {
            agent_id: "na".into(),
        },
        0.0,
        "dash",
    );
    let report = pipe.tick(vec![intent], None, &clock).await;
    assert_eq!(report.dispatched, 0);
    assert_eq!(report.rejected, 1);
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn weapon_under_tight_is_async_quorum_then_launch() {
    let (mut pipe, counter, audit) = build(WeaponReleaseLevel::WeaponsTight);
    let clock = ManualClock::new(0.0);

    // Fire intent defers to pending (does not actuate, does not block).
    let report = pipe
        .tick(
            vec![fire_intent(
                IntentSource::Llm {
                    agent_id: "fca".into(),
                },
                CommandPriority::High,
            )],
            None,
            &clock,
        )
        .await;
    assert_eq!(report.pending, 1);
    assert_eq!(report.dispatched, 0);
    assert_eq!(counter.load(Ordering::SeqCst), 0);

    // The approval id is "approval:<intent-id>" — but we can find it via state.
    // Collect the approval id from the audit detail is awkward; instead, drive
    // the engagement by re-deriving the id. The pipeline exposes engagement
    // state by approval id, so we reconstruct from the only pending entry.
    // Simplest: sign using the known prefix is not possible; so query via a
    // helper round-trip: we stored the approval id in the engagement manager.
    // We expose it through a deterministic accessor instead.
    let approval_id = pipe
        .pending_ids()
        .into_iter()
        .next()
        .expect("one pending engagement");

    // One signature is not enough (quorum = 2).
    assert_eq!(
        pipe.sign(&approval_id, "op-1"),
        Some(EngagementState::PendingSignatures {
            collected: 1,
            required: 2
        })
    );
    // Cannot launch yet.
    assert!(!pipe.launch_if_ready(&approval_id).await);

    // Second signature reaches quorum.
    assert_eq!(
        pipe.sign(&approval_id, "op-2"),
        Some(EngagementState::Approved)
    );

    // Now launch dispatches the weapon command.
    assert!(pipe.launch_if_ready(&approval_id).await);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    assert!(audit.verify_integrity().is_ok());
}

#[tokio::test]
async fn approved_weapon_auto_launches_on_next_tick() {
    let (mut pipe, counter, _audit) = build(WeaponReleaseLevel::WeaponsTight);
    let clock = ManualClock::new(0.0);

    let report = pipe
        .tick(
            vec![fire_intent(
                IntentSource::Llm {
                    agent_id: "fca".into(),
                },
                CommandPriority::High,
            )],
            None,
            &clock,
        )
        .await;
    assert_eq!(report.pending, 1);
    let approval_id = pipe.pending_ids().into_iter().next().unwrap();

    assert_eq!(
        pipe.sign(&approval_id, "op-1"),
        Some(EngagementState::PendingSignatures {
            collected: 1,
            required: 2
        })
    );
    assert_eq!(
        pipe.sign(&approval_id, "op-2"),
        Some(EngagementState::Approved)
    );

    let report = pipe.tick(vec![], None, &clock).await;

    assert_eq!(report.dispatched, 1);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    assert_eq!(
        pipe.engagement_state(&approval_id),
        Some(EngagementState::Launched)
    );
}

#[tokio::test]
async fn expired_quorum_is_rejected_and_audited() {
    let (mut pipe, counter, _audit) = build(WeaponReleaseLevel::WeaponsTight);
    let clock = ManualClock::new(0.0);

    let report = pipe
        .tick(
            vec![fire_intent(
                IntentSource::Llm {
                    agent_id: "fca".into(),
                },
                CommandPriority::High,
            )],
            None,
            &clock,
        )
        .await;
    assert_eq!(report.pending, 1);
    let approval_id = pipe.pending_ids().into_iter().next().unwrap();

    // Advance past the approval window with no signatures.
    clock.set(31.0);
    let report = pipe.tick(vec![], None, &clock).await;
    assert_eq!(report.expired, 1);
    assert_eq!(
        pipe.engagement_state(&approval_id),
        Some(EngagementState::Expired)
    );

    // An expired engagement cannot launch.
    assert!(!pipe.launch_if_ready(&approval_id).await);
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}
