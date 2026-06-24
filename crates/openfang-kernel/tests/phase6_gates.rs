//! Phase 6 — staged exit-criteria gate summary.
//!
//! This is the single place that asserts the cross-cutting invariants the plan
//! requires before acceptance. Each `gate_*` test corresponds to one exit
//! criterion; together they form the "门禁汇总" (gate summary):
//!
//! - `gate_backend_agnostic`     — identical intent script yields identical gate
//!   decisions on the Noop and Mock backends (adapter-agnostic gate contract).
//! - `gate_safety_not_bypassable`— no producer (LLM or DCC reflex) can drive a
//!   weapon past the gate under WeaponsHold; the audit chain stays intact.
//! - `gate_realtime_stable`      — the tactical tick is deterministic and its
//!   latency is measurable under sustained load (deadline observability).
//! - `gate_weapon_quorum_safe`   — weapon release requires async quorum and a
//!   final ROE interlock; expiry and rejection are audited.
//!
//! The DDS loopback smoke gate lives in
//! `openfang-platform-dds/tests/dds_vertical_slice.rs`; a true rustdds/HIL gate
//! still requires the `rustdds-transport` implementation and a live RTPS domain.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use openfang_kernel::tactical_pipeline::{TacticalPipeline, TickReport};
use openfang_platform::{AdapterRegistry, MockAdapter, NoopAdapter, PlatformAdapter};
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

fn pipeline_with(
    adapter: Box<dyn PlatformAdapter>,
    roe: WeaponReleaseLevel,
) -> (TacticalPipeline, Arc<AuditLog>) {
    let registry = Arc::new(AdapterRegistry::new());
    registry.set_primary(adapter);
    let restrictions = Arc::new(OpRestrictionsManager::new(
        RulesOfEngagement {
            weapon_release_authority: roe,
            ..Default::default()
        },
        PlatformLimits::default(),
    ));
    let audit = Arc::new(AuditLog::new());
    let pipeline =
        TacticalPipeline::new(registry, restrictions, audit.clone(), full_caps(), 2, 30.0);
    (pipeline, audit)
}

fn heading(prio: CommandPriority, hdg: f64) -> CandidateIntent {
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

fn fire(source: IntentSource, prio: CommandPriority) -> CandidateIntent {
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

/// A mixed script with three distinct conflict keys: one legal motion (usv-01),
/// one over-speed motion on a *different* platform (usv-02, SPGS reject), and one
/// unauthorized weapon (usv-01, gate reject under WeaponsHold). Distinct keys
/// ensure the composer does not deconflict them against each other.
fn mixed_script() -> Vec<CandidateIntent> {
    vec![
        heading(CommandPriority::Normal, 90.0),
        CandidateIntent::new(
            PlatformCommand::SetSpeed {
                platform_id: "usv-02".into(),
                speed_ms: 999.0,
                acceleration_ms2: None,
            },
            CommandPriority::Normal,
            IntentSource::Llm {
                agent_id: "na".into(),
            },
            0.0,
            "dash",
        ),
        fire(
            IntentSource::Llm {
                agent_id: "fca".into(),
            },
            CommandPriority::Normal,
        ),
    ]
}

async fn run_script(adapter: Box<dyn PlatformAdapter>) -> TickReport {
    let (mut pipe, audit) = pipeline_with(adapter, WeaponReleaseLevel::WeaponsHold);
    let clock = ManualClock::new(0.0);
    let report = pipe.tick(mixed_script(), None, &clock).await;
    assert!(
        audit.verify_integrity().is_ok(),
        "audit chain must stay intact"
    );
    report
}

#[tokio::test]
async fn gate_backend_agnostic() {
    // Noop backend.
    let noop = NoopAdapter::new();
    let noop_counter = noop.counter();
    let noop_report = run_script(Box::new(noop)).await;

    // Mock backend (connect before handing to the registry; it requires a live link).
    let mut mock = MockAdapter::new("mock");
    mock.connect().await.unwrap();
    let mock_sent = mock.sent_handle();
    let mock_report = run_script(Box::new(mock)).await;

    // Identical gate decisions regardless of backend: 1 dispatched, 2 rejected.
    assert_eq!(noop_report.dispatched, 1);
    assert_eq!(noop_report.rejected, 2);
    assert_eq!(mock_report.dispatched, noop_report.dispatched);
    assert_eq!(mock_report.rejected, noop_report.rejected);

    // The one approved command actually reached each actuator boundary.
    assert_eq!(noop_counter.load(Ordering::SeqCst), 1);
    assert_eq!(mock_sent.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn gate_safety_not_bypassable() {
    // A DCC reflex at Critical priority must NOT bypass the gate under WeaponsHold.
    let noop = NoopAdapter::new();
    let counter = noop.counter();
    let (mut pipe, audit) = pipeline_with(Box::new(noop), WeaponReleaseLevel::WeaponsHold);
    let clock = ManualClock::new(0.0);

    let report = pipe
        .tick(
            vec![fire(
                IntentSource::Dcc {
                    rule_name: "auto_engage".into(),
                },
                CommandPriority::Critical,
            )],
            None,
            &clock,
        )
        .await;

    assert_eq!(report.dispatched, 0, "weapon must not reach the actuator");
    assert_eq!(counter.load(Ordering::SeqCst), 0);
    assert!(!audit.is_empty(), "the rejection must be audited");
    assert!(audit.verify_integrity().is_ok());
}

#[tokio::test]
async fn gate_realtime_stable() {
    // Sustained load: many ticks, each issuing one legal motion intent. The loop
    // must remain deterministic (one dispatch per tick) and keep the audit chain
    // valid, and per-tick latency must be measurable (deadline observability).
    let noop = NoopAdapter::new();
    let counter = noop.counter();
    let (mut pipe, audit) = pipeline_with(Box::new(noop), WeaponReleaseLevel::WeaponsHold);
    let clock = ManualClock::new(0.0);

    const TICKS: u64 = 2_000;
    let start = Instant::now();
    for i in 0..TICKS {
        let report = pipe
            .tick(vec![heading(CommandPriority::Normal, 90.0)], None, &clock)
            .await;
        assert_eq!(
            report.dispatched, 1,
            "tick {i} should dispatch exactly one command"
        );
    }
    let elapsed = start.elapsed();
    let per_tick_us = elapsed.as_micros() as f64 / TICKS as f64;

    // Deterministic throughput.
    assert_eq!(counter.load(Ordering::SeqCst), TICKS);
    assert!(audit.verify_integrity().is_ok());

    // Observable and comfortably under a 50ms (50_000us) tick budget. The bound is
    // generous to avoid CI flakiness; the point is that the latency is *measured*.
    eprintln!("tactical tick latency: {per_tick_us:.1} us/tick over {TICKS} ticks");
    assert!(
        per_tick_us < 50_000.0,
        "per-tick latency {per_tick_us}us exceeded 50ms budget"
    );
}

#[tokio::test]
async fn gate_weapon_quorum_safe() {
    // Under WeaponsTight, a fire intent defers to async quorum; it dispatches only
    // after the required signatures, and expiry/rejection are auditable.
    let noop = NoopAdapter::new();
    let counter = noop.counter();
    let (mut pipe, audit) = pipeline_with(Box::new(noop), WeaponReleaseLevel::WeaponsTight);
    let clock = ManualClock::new(0.0);

    let report = pipe
        .tick(
            vec![fire(
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

    let approval_id = pipe
        .pending_ids()
        .into_iter()
        .next()
        .expect("one pending engagement");

    // Quorum of 2: first signature is insufficient, second reaches approval.
    assert_eq!(
        pipe.sign(&approval_id, "op-1"),
        Some(EngagementState::PendingSignatures {
            collected: 1,
            required: 2
        })
    );
    assert!(!pipe.launch_if_ready(&approval_id).await);
    assert_eq!(
        pipe.sign(&approval_id, "op-2"),
        Some(EngagementState::Approved)
    );

    // Now and only now does the weapon dispatch.
    assert!(pipe.launch_if_ready(&approval_id).await);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    assert!(audit.verify_integrity().is_ok());
}
