//! Cerebellum service layer — explicit "8 services" contract over the
//! existing real-time runtime.
//!
//! This module does **not** rewrite the proven fast-loop machinery
//! ([`crate::cerebellum`], [`crate::action_composer`], [`crate::command_gate`],
//! [`crate::weapon_engagement`]). Instead it declares the canonical
//! service-shaped surface that the [PlatformControlLoop] composes:
//!
//! ```text
//! SMS  — sensor management & fusion        (snapshot ingestion + EMCON state)
//! MMS  — maneuver management               (collision/geofence/formation reflexes)
//! WMS  — weapon management                 (engagement, ROE, BDA, salvo)
//! SPGS — safety policy gate                (CommandGate)
//! ACS  — action composer                   (deconflict, priority preempt)
//! EWMS — electronic-warfare management     (chaff/jam reflexes + budget)
//! CMS  — communications management         (link quality, EMCON, A2A)
//! PSS  — platform survivability service    (battery/damage/RTB reflexes)
//! ```
//!
//! Every cerebellum service shares the same RORO contract:
//! - input: [`ServiceContext`] — read-only snapshot of the live world plus
//!   capability / posture / autonomy hints,
//! - output: [`ServiceOutput`] — a vector of [`CandidateIntent`]s plus audit
//!   hints; services *never* produce dispatchable commands directly.
//!
//! The contract makes 4 invariants explicit, matching the architecture plan:
//!
//! 1. Services are pure producers of intent — they may inspect snapshot but
//!    must not mutate it or call the adapter directly.
//! 2. No service performs LLM, network, or blocking-approval work on the hot
//!    path (LLM lives only in the slow loop).
//! 3. Services can be tested in isolation by handing them a `ServiceContext`
//!    built from a mock snapshot.
//! 4. The composer (ACS) and gate (SPGS) remain the *only* place safety,
//!    deconfliction, and weapon arbitration happens.

use openfang_types::config::AutonomyModeProfile;
use openfang_types::platform::{CcaRole, PlatformCapabilities, PlatformState, WorldSnapshot};
use openfang_types::tactical::CandidateIntent;

use crate::sensor_fusion::FusedTrack;

/// Service identity — one variant per service in the 8-service contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CerebellumServiceId {
    Sms,
    Mms,
    Wms,
    Spgs,
    Acs,
    Ewms,
    Cms,
    Pss,
}

impl CerebellumServiceId {
    /// Stable lowercase label for audit and metrics.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Sms => "sms",
            Self::Mms => "mms",
            Self::Wms => "wms",
            Self::Spgs => "spgs",
            Self::Acs => "acs",
            Self::Ewms => "ewms",
            Self::Cms => "cms",
            Self::Pss => "pss",
        }
    }
}

/// Hint surfaced by a service for the audit log without forcing it to take a
/// direct dependency on the `AuditLog` type.
#[derive(Debug, Clone)]
pub struct ServiceAuditHint {
    pub service: CerebellumServiceId,
    pub event: String,
    pub detail: Option<String>,
}

impl ServiceAuditHint {
    pub fn new(service: CerebellumServiceId, event: impl Into<String>) -> Self {
        Self {
            service,
            event: event.into(),
            detail: None,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

/// Read-only context passed to a cerebellum service on every tick.
///
/// Built once per [`PlatformControlLoop::step`] and reused across all service
/// calls so they observe a coherent world view. Lifetime-borrowed to keep the
/// hot path allocation-free.
#[derive(Debug, Clone, Copy)]
pub struct ServiceContext<'a> {
    /// The freshest world snapshot, if any (None on first-tick before any poll).
    pub snapshot: Option<&'a WorldSnapshot>,
    /// Own platform state extracted from the snapshot (None until id-matched).
    pub own_platform: Option<&'a PlatformState>,
    /// Unified fused-track picture for this tick (SMS sensor-fusion output).
    ///
    /// This is the single authoritative track source — Kalman-smoothed
    /// position, stable `track_id`, fused quality and `threat_level` — that
    /// services consume instead of re-deriving danger from raw per-sensor
    /// returns. Empty on the first tick before any fusion has run, or when the
    /// service is exercised in isolation; consumers must tolerate `&[]`.
    pub fused_tracks: &'a [FusedTrack],
    /// Active autonomy-mode profile (gate-side hard envelope).
    pub autonomy: Option<&'a AutonomyModeProfile>,
    /// Live capability mask (what the adapter actually supports).
    pub capabilities: &'a PlatformCapabilities,
    /// Effective tactical role this tick (after fleet/role assignment).
    pub posture: CcaRole,
    /// Monotonic time in seconds (sim or wall, per the active `TimeSource`).
    pub now: f64,
    /// Own platform id (used for `command_target` matching and audit).
    pub own_platform_id: &'a str,
}

/// What a service emits for the cerebellum to consume.
#[derive(Debug, Default, Clone)]
pub struct ServiceOutput {
    /// Newly produced intents; will be routed via the cerebellum queue and
    /// composer like any other producer.
    pub intents: Vec<CandidateIntent>,
    /// Out-of-band events the service wants reflected in the audit log.
    pub audit_hints: Vec<ServiceAuditHint>,
}

impl ServiceOutput {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn with_intent(mut self, intent: CandidateIntent) -> Self {
        self.intents.push(intent);
        self
    }

    pub fn with_audit(mut self, hint: ServiceAuditHint) -> Self {
        self.audit_hints.push(hint);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.intents.is_empty() && self.audit_hints.is_empty()
    }
}

/// Canonical contract every cerebellum producer service implements.
///
/// `Pss`, `Ewms`, `Cms`, and (later) `Sms` newcomers implement this trait
/// directly. Existing complex services (`Acs`, `Spgs`, `Wms`, `Mms`) keep
/// their richer call shapes — this trait simply declares the canonical
/// signature so that future producers slot in without ad-hoc plumbing.
pub trait CerebellumService: Send + Sync {
    /// Service identity (for audit + metrics + DI).
    fn id(&self) -> CerebellumServiceId;

    /// Single-tick evaluation. Implementations must be:
    /// - allocation-light (the hot path runs at ≥20Hz),
    /// - side-effect free except for the returned `ServiceOutput`,
    /// - tolerant of `snapshot.is_none()` (the very first tick before any
    ///   adapter poll has succeeded).
    fn evaluate(&mut self, ctx: &ServiceContext<'_>) -> ServiceOutput;
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::config::AutonomyConfig;
    use openfang_types::platform::PlatformCapabilities;

    fn caps() -> PlatformCapabilities {
        PlatformCapabilities {
            supports_motion_control: true,
            supports_sensor_control: true,
            supports_weapon_control: true,
            supports_jammer_control: true,
            supports_comm_control: true,
            supports_uav_launch_recovery: false,
            supports_formation_control: true,
            supports_handoff: true,
            max_platforms: 1,
            supports_simulation: true,
            supports_hardware: false,
        }
    }

    #[test]
    fn service_id_labels_are_stable() {
        assert_eq!(CerebellumServiceId::Sms.label(), "sms");
        assert_eq!(CerebellumServiceId::Pss.label(), "pss");
        assert_eq!(CerebellumServiceId::Spgs.label(), "spgs");
    }

    #[test]
    fn audit_hint_with_detail() {
        let h = ServiceAuditHint::new(CerebellumServiceId::Pss, "low_battery")
            .with_detail("battery=0.12");
        assert_eq!(h.service, CerebellumServiceId::Pss);
        assert_eq!(h.event, "low_battery");
        assert_eq!(h.detail.as_deref(), Some("battery=0.12"));
    }

    #[test]
    fn service_output_builders_compose() {
        let out = ServiceOutput::empty().with_audit(ServiceAuditHint::new(
            CerebellumServiceId::Cms,
            "link_degraded",
        ));
        assert!(out.intents.is_empty());
        assert_eq!(out.audit_hints.len(), 1);
        assert!(!out.is_empty());
    }

    #[test]
    fn service_context_constructs_with_minimal_inputs() {
        let caps = caps();
        let cfg = AutonomyConfig::default();
        let active = cfg.active();
        let ctx = ServiceContext {
            snapshot: None,
            own_platform: None,
            fused_tracks: &[],
            autonomy: Some(&active),
            capabilities: &caps,
            posture: CcaRole::Adaptive,
            now: 0.0,
            own_platform_id: "self",
        };
        assert!(ctx.snapshot.is_none());
        assert_eq!(ctx.own_platform_id, "self");
        assert_eq!(ctx.posture, CcaRole::Adaptive);
        assert!(ctx.autonomy.is_some());
    }
}
