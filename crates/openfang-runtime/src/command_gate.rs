//! CommandGate — the single mandatory checkpoint every intent must clear before
//! it can become a dispatchable [`PlatformCommand`].
//!
//! Pipeline order (Iron Law): `Capability → Approval → SPGS → Audit`.
//!
//! - Producers (LLM, DCC) emit [`CandidateIntent`]s; the [`crate::action_composer`]
//!   deconflicts them; the gate is the *only* component that yields commands the
//!   adapter layer may send.
//! - Every decision (approved / rejected / pending) is written to the tamper-
//!   evident [`AuditLog`].
//! - The gate is composed of ordered [`GateLayer`]s. The concrete approval layer
//!   is injected (e.g. the kernel's quorum/ApprovalManager); a permissive default
//!   is provided for read-only and simulation builds.
//!
//! The gate performs no I/O on the hot path other than the synchronous audit
//! append, so it is safe to call from the real-time tick.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use openfang_types::config::{AutonomyClassDisposition, AutonomyModeProfile};
use openfang_types::platform::{PlatformCapabilities, PlatformCommand, WorldSnapshot};
use openfang_types::tactical::{CandidateIntent, CommandClass, GateDecision, GateStage};
use openfang_types::umaa::WeaponReleaseLevel;
use openfang_types::wms::WmsDisposition;

use crate::audit::{AuditAction, AuditLog};
use crate::intervention::{InterventionDecision, InterventionGate, InterventionRequest};
use crate::op_restrictions::OpRestrictionsManager;
use crate::wms_policy::WmsPolicyEngine;

// ─────────────────────────────────────────────
// Gate layer abstraction
// ─────────────────────────────────────────────

/// Context available to every gate layer for a single intent.
pub struct GateContext<'a> {
    /// Capabilities of the backend that would execute the command.
    pub capabilities: &'a PlatformCapabilities,
    /// Latest world snapshot, if available (for pose/geofence-aware checks).
    pub snapshot: Option<&'a WorldSnapshot>,
    /// Effective autonomy profile id for policy-table lookups.
    pub autonomy_profile: Option<&'a str>,
}

/// Result of a single layer's evaluation.
#[derive(Debug, Clone)]
pub enum GateOutcome {
    /// Continue to the next layer.
    Pass,
    /// Block the command at this stage.
    Reject(String),
    /// Defer pending asynchronous approval (carries the approval request id).
    Pending(String),
}

/// One stage of the gate pipeline.
pub trait GateLayer: Send + Sync {
    fn stage(&self) -> GateStage;
    fn check(&self, intent: &CandidateIntent, ctx: &GateContext<'_>) -> GateOutcome;
}

// ─────────────────────────────────────────────
// Capability layer
// ─────────────────────────────────────────────

/// Rejects commands the backend has not declared support for.
pub struct CapabilityGate;

impl GateLayer for CapabilityGate {
    fn stage(&self) -> GateStage {
        GateStage::Capability
    }

    fn check(&self, intent: &CandidateIntent, ctx: &GateContext<'_>) -> GateOutcome {
        let caps = ctx.capabilities;
        let supported = match intent.class() {
            CommandClass::Motion => caps.supports_motion_control,
            CommandClass::Sensor => caps.supports_sensor_control,
            CommandClass::Weapon => caps.supports_weapon_control,
            CommandClass::ElectronicWarfare => caps.supports_jammer_control,
            CommandClass::Comm => caps.supports_comm_control,
            CommandClass::Uav => caps.supports_uav_launch_recovery,
            CommandClass::Formation => caps.supports_formation_control,
            CommandClass::Command => true, // C2 routing is always permitted
            CommandClass::Aux => true,
        };
        if supported {
            GateOutcome::Pass
        } else {
            GateOutcome::Reject(format!(
                "backend does not support {:?} commands",
                intent.class()
            ))
        }
    }
}

// ─────────────────────────────────────────────
// Autonomy-mode profile cap (hard envelope)
// ─────────────────────────────────────────────

/// Hard envelope for the active [`AutonomyModeProfile`]. Sits between the
/// capability layer and the approval/SPGS layers, so:
///
/// - `auto_classes`        → `Pass` (continue into Approval → SPGS).
/// - `pending_approval`    → `Pending` (deferred to the human queue).
/// - `advisory_classes`    → `Reject` with audit reason (no actuation).
///
/// The profile is held behind an [`Arc<RwLock<_>>`] so it can be hot-swapped
/// by the config-reload subsystem without locking the tick.
pub struct AutonomyProfileGate {
    profile: Arc<RwLock<AutonomyModeProfile>>,
}

impl AutonomyProfileGate {
    pub fn new(profile: AutonomyModeProfile) -> Self {
        Self {
            profile: Arc::new(RwLock::new(profile)),
        }
    }

    pub fn with_shared(profile: Arc<RwLock<AutonomyModeProfile>>) -> Self {
        Self { profile }
    }

    /// Hot-swap the active profile (config reload).
    pub fn set_profile(&self, profile: AutonomyModeProfile) {
        *self.profile.write().unwrap_or_else(|e| e.into_inner()) = profile;
    }

    pub fn shared(&self) -> Arc<RwLock<AutonomyModeProfile>> {
        Arc::clone(&self.profile)
    }
}

impl GateLayer for AutonomyProfileGate {
    fn stage(&self) -> GateStage {
        GateStage::Capability
    }

    fn check(&self, intent: &CandidateIntent, _ctx: &GateContext<'_>) -> GateOutcome {
        let profile = self.profile.read().unwrap_or_else(|e| e.into_inner());
        // Weapon-class intents are refined by WMS/Approval because disposition
        // depends on weapon id and ROE, not just the coarse command class.
        if intent.class().is_weapon() {
            if matches!(
                profile.disposition_for(intent.class()),
                AutonomyClassDisposition::Advisory
            ) {
                return GateOutcome::Reject(format!(
                    "profile '{}' marks {} as advisory_only",
                    profile.id,
                    intent.class().as_str()
                ));
            }
            return GateOutcome::Pass;
        }
        match profile.disposition_for(intent.class()) {
            AutonomyClassDisposition::Auto => GateOutcome::Pass,
            AutonomyClassDisposition::PendingApproval => {
                GateOutcome::Pending(format!("autonomy:{}:{}", profile.id, intent.id))
            }
            AutonomyClassDisposition::Advisory => GateOutcome::Reject(format!(
                "profile '{}' marks {} as advisory_only",
                profile.id,
                intent.class().as_str()
            )),
        }
    }
}

// ─────────────────────────────────────────────
// Approval layer
// ─────────────────────────────────────────────

/// Decides whether an intent may proceed, must wait for human/quorum approval,
/// or must be denied. The real implementation (multi-party quorum) is injected
/// by the kernel; runtime/sim builds use [`PermissiveApproval`].
pub trait ApprovalPolicy: Send + Sync {
    /// Returns the approval outcome for the given intent under the current ROE.
    fn evaluate(
        &self,
        intent: &CandidateIntent,
        roe: WeaponReleaseLevel,
        ctx: &GateContext<'_>,
    ) -> GateOutcome;
}

/// Approves everything (read-only / simulation default). Weapons are still gated
/// by SPGS (ROE), so this is safe for non-live builds only.
pub struct PermissiveApproval;

impl ApprovalPolicy for PermissiveApproval {
    fn evaluate(
        &self,
        _intent: &CandidateIntent,
        _roe: WeaponReleaseLevel,
        _ctx: &GateContext<'_>,
    ) -> GateOutcome {
        GateOutcome::Pass
    }
}

/// Requires human approval (Pending) for weapon-class commands when ROE is
/// `WeaponsTight`, denies them outright under `WeaponsHold`, and lets
/// non-weapon commands pass. `WeaponsFree` lets weapons pass without per-shot
/// approval. This is the default *safe* policy.
pub struct WeaponApproval {
    /// Generates an approval request id for deferred decisions.
    id_prefix: String,
    wms_policy: Arc<WmsPolicyEngine>,
}

impl WeaponApproval {
    pub fn new(id_prefix: impl Into<String>) -> Self {
        Self {
            id_prefix: id_prefix.into(),
            wms_policy: Arc::new(WmsPolicyEngine::default()),
        }
    }

    pub fn with_wms_policy(id_prefix: impl Into<String>, wms_policy: Arc<WmsPolicyEngine>) -> Self {
        Self {
            id_prefix: id_prefix.into(),
            wms_policy,
        }
    }
}

impl Default for WeaponApproval {
    fn default() -> Self {
        Self::new("approval")
    }
}

impl ApprovalPolicy for WeaponApproval {
    fn evaluate(
        &self,
        intent: &CandidateIntent,
        roe: WeaponReleaseLevel,
        ctx: &GateContext<'_>,
    ) -> GateOutcome {
        if !intent.class().is_weapon() {
            return GateOutcome::Pass;
        }
        let Some(weapon_id) = command_weapon_id(&intent.command) else {
            return fallback_weapon_approval(&self.id_prefix, intent, roe);
        };
        match self.wms_policy.disposition_for(
            ctx.autonomy_profile.unwrap_or("default"),
            roe,
            weapon_id,
        ) {
            WmsDisposition::Auto => GateOutcome::Pass,
            WmsDisposition::Pending => {
                GateOutcome::Pending(format!("{}:{}", self.id_prefix, intent.id))
            }
            WmsDisposition::Deny => GateOutcome::Reject(format!(
                "WMS policy denies weapon release for {weapon_id} under {:?}",
                roe
            )),
        }
    }
}

/// Optional approval policy for sensor-control commands. The main deployment
/// path can still use configurable intervention rules; this policy gives SMS or
/// tests a small static interface for sensor-specific pending/deny decisions.
pub struct SensorApproval {
    id_prefix: String,
    pending_sensor_ids: HashSet<String>,
    denied_sensor_ids: HashSet<String>,
}

impl SensorApproval {
    pub fn new(id_prefix: impl Into<String>) -> Self {
        Self {
            id_prefix: id_prefix.into(),
            pending_sensor_ids: HashSet::new(),
            denied_sensor_ids: HashSet::new(),
        }
    }

    pub fn with_pending_sensor(mut self, sensor_id: impl Into<String>) -> Self {
        self.pending_sensor_ids.insert(sensor_id.into());
        self
    }

    pub fn with_denied_sensor(mut self, sensor_id: impl Into<String>) -> Self {
        self.denied_sensor_ids.insert(sensor_id.into());
        self
    }
}

impl ApprovalPolicy for SensorApproval {
    fn evaluate(
        &self,
        intent: &CandidateIntent,
        _roe: WeaponReleaseLevel,
        _ctx: &GateContext<'_>,
    ) -> GateOutcome {
        let Some(sensor_id) = command_sensor_id(&intent.command) else {
            return GateOutcome::Pass;
        };
        if self.denied_sensor_ids.contains(sensor_id) {
            return GateOutcome::Reject(format!("sensor approval denies {sensor_id}"));
        }
        if self.pending_sensor_ids.contains(sensor_id) {
            return GateOutcome::Pending(format!("{}:{}", self.id_prefix, intent.id));
        }
        GateOutcome::Pass
    }
}

/// Approval policy backed by the configurable stage/entity intervention rules.
pub struct ConfigurableApproval {
    gate: Arc<RwLock<InterventionGate>>,
    fallback: WeaponApproval,
    wms_policy: Arc<WmsPolicyEngine>,
}

impl ConfigurableApproval {
    pub fn new(gate: InterventionGate) -> Self {
        Self::with_shared_gate(Arc::new(RwLock::new(gate)))
    }

    pub fn with_shared_gate(gate: Arc<RwLock<InterventionGate>>) -> Self {
        let wms_policy = Arc::new(WmsPolicyEngine::default());
        Self {
            gate,
            fallback: WeaponApproval::with_wms_policy("approval", Arc::clone(&wms_policy)),
            wms_policy,
        }
    }

    pub fn with_shared_gate_and_wms_policy(
        gate: Arc<RwLock<InterventionGate>>,
        wms_policy: Arc<WmsPolicyEngine>,
    ) -> Self {
        Self {
            gate,
            fallback: WeaponApproval::with_wms_policy("approval", Arc::clone(&wms_policy)),
            wms_policy,
        }
    }

    pub fn set_gate(&self, gate: InterventionGate) {
        *self.gate.write().unwrap_or_else(|e| e.into_inner()) = gate;
    }

    pub fn shared_gate(&self) -> Arc<RwLock<InterventionGate>> {
        Arc::clone(&self.gate)
    }
}

impl ApprovalPolicy for ConfigurableApproval {
    fn evaluate(
        &self,
        intent: &CandidateIntent,
        roe: WeaponReleaseLevel,
        ctx: &GateContext<'_>,
    ) -> GateOutcome {
        if let Some(weapon_id) = command_weapon_id(&intent.command) {
            match self.wms_policy.disposition_for(
                ctx.autonomy_profile.unwrap_or("default"),
                roe,
                weapon_id,
            ) {
                WmsDisposition::Auto => return GateOutcome::Pass,
                WmsDisposition::Deny => {
                    return GateOutcome::Reject(format!(
                        "WMS policy denies weapon release for {weapon_id} under {:?}",
                        roe
                    ))
                }
                WmsDisposition::Pending => {}
            }
        }
        let stage = approval_stage(intent.class());
        let (platform_id, track_id) = command_target_and_track(&intent.command);
        let gate = self.gate.read().unwrap_or_else(|e| e.into_inner());
        match gate.evaluate(InterventionRequest {
            stage,
            platform_id,
            command_class: Some(intent.class()),
            source: Some(&intent.source),
            track_id,
            intent_id: &intent.id,
            weapon_release_authority: Some(roe),
            plan_fingerprint: None,
        }) {
            InterventionDecision::Pass => GateOutcome::Pass,
            InterventionDecision::Deny(reason) => GateOutcome::Reject(reason),
            InterventionDecision::Pending { approval_id, .. } => GateOutcome::Pending(approval_id),
            InterventionDecision::RoeDriven => self.fallback.evaluate(intent, roe, ctx),
        }
    }
}

fn command_weapon_id(cmd: &PlatformCommand) -> Option<&str> {
    match cmd {
        PlatformCommand::FireAtTarget { weapon_id, .. }
        | PlatformCommand::FireSalvo { weapon_id, .. } => Some(weapon_id),
        _ => None,
    }
}

fn command_sensor_id(cmd: &PlatformCommand) -> Option<&str> {
    match cmd {
        PlatformCommand::SensorOn { sensor_id, .. }
        | PlatformCommand::SensorOff { sensor_id, .. }
        | PlatformCommand::SensorSetMode { sensor_id, .. } => Some(sensor_id),
        _ => None,
    }
}

fn fallback_weapon_approval(
    id_prefix: &str,
    intent: &CandidateIntent,
    roe: WeaponReleaseLevel,
) -> GateOutcome {
    match roe {
        WeaponReleaseLevel::WeaponsHold => {
            GateOutcome::Reject("ROE WeaponsHold: weapon release prohibited".into())
        }
        WeaponReleaseLevel::WeaponsTight => {
            GateOutcome::Pending(format!("{id_prefix}:{}", intent.id))
        }
        WeaponReleaseLevel::WeaponsFree => GateOutcome::Pass,
    }
}

fn approval_stage(class: CommandClass) -> &'static str {
    match class {
        CommandClass::Weapon => "weapon_release",
        CommandClass::Motion => "motion",
        CommandClass::Sensor => "sensor",
        CommandClass::ElectronicWarfare => "electronic_warfare",
        CommandClass::Comm => "comm",
        CommandClass::Command => "command",
        CommandClass::Uav => "uav",
        CommandClass::Formation => "formation",
        CommandClass::Aux => "aux",
    }
}

fn command_target_and_track(cmd: &PlatformCommand) -> (&str, Option<&str>) {
    match cmd {
        PlatformCommand::FireAtTarget {
            platform_id,
            track_id,
            ..
        }
        | PlatformCommand::FireSalvo {
            platform_id,
            track_id,
            ..
        } => (platform_id, Some(track_id)),
        _ => (cmd.target_platform_id(), None),
    }
}

/// Adapter that turns any [`ApprovalPolicy`] into a [`GateLayer`].
pub struct ApprovalGate {
    policy: Arc<dyn ApprovalPolicy>,
    restrictions: Arc<OpRestrictionsManager>,
}

impl ApprovalGate {
    pub fn new(policy: Arc<dyn ApprovalPolicy>, restrictions: Arc<OpRestrictionsManager>) -> Self {
        Self {
            policy,
            restrictions,
        }
    }
}

impl GateLayer for ApprovalGate {
    fn stage(&self) -> GateStage {
        GateStage::Approval
    }

    fn check(&self, intent: &CandidateIntent, ctx: &GateContext<'_>) -> GateOutcome {
        let roe = self.restrictions.get_roe().weapon_release_authority;
        self.policy.evaluate(intent, roe, ctx)
    }
}

// ─────────────────────────────────────────────
// SPGS layer (final safety policy gate)
// ─────────────────────────────────────────────

/// Final safety policy gate: ROE (weapons), geofence, and platform limits.
pub struct SpgsGate {
    restrictions: Arc<OpRestrictionsManager>,
    wms_policy: Arc<WmsPolicyEngine>,
}

impl SpgsGate {
    pub fn new(restrictions: Arc<OpRestrictionsManager>) -> Self {
        Self {
            restrictions,
            wms_policy: Arc::new(WmsPolicyEngine::default()),
        }
    }

    pub fn with_wms_policy(
        restrictions: Arc<OpRestrictionsManager>,
        wms_policy: Arc<WmsPolicyEngine>,
    ) -> Self {
        Self {
            restrictions,
            wms_policy,
        }
    }
}

impl GateLayer for SpgsGate {
    fn stage(&self) -> GateStage {
        GateStage::Spgs
    }

    fn check(&self, intent: &CandidateIntent, ctx: &GateContext<'_>) -> GateOutcome {
        // Weapons: even after approval, never release under WeaponsHold.
        if intent.class().is_weapon() {
            let roe = self.restrictions.get_roe().weapon_release_authority;
            if roe == WeaponReleaseLevel::WeaponsHold {
                if let Some(weapon_id) = command_weapon_id(&intent.command) {
                    if self.wms_policy.disposition_for(
                        ctx.autonomy_profile.unwrap_or("default"),
                        roe,
                        weapon_id,
                    ) == WmsDisposition::Auto
                    {
                        return GateOutcome::Pass;
                    }
                }
                return GateOutcome::Reject("SPGS: ROE WeaponsHold blocks weapon release".into());
            }
        }

        // Motion: enforce hard platform limits where a speed is requested.
        if let Some(speed) = requested_speed(&intent.command) {
            if let Err(v) = self.restrictions.check_limits(speed, 0.0) {
                return GateOutcome::Reject(format!(
                    "SPGS: {} limit exceeded ({} > {})",
                    v.kind, v.requested, v.limit
                ));
            }
        }

        // Geofence: block motion that would (or currently does) violate a fence.
        if matches!(
            intent.class(),
            CommandClass::Motion | CommandClass::Uav | CommandClass::Formation
        ) {
            if let Some(v) = self.restrictions.check_geofence_violation() {
                return GateOutcome::Reject(format!(
                    "SPGS: geofence '{}' violated ({})",
                    v.fence_name, v.kind
                ));
            }
        }

        GateOutcome::Pass
    }
}

fn requested_speed(cmd: &PlatformCommand) -> Option<f64> {
    match cmd {
        PlatformCommand::SetSpeed { speed_ms, .. } => Some(*speed_ms),
        PlatformCommand::SetHeading { speed_ms, .. } => *speed_ms,
        PlatformCommand::GotoLocation { speed_ms, .. } => *speed_ms,
        _ => None,
    }
}

// ─────────────────────────────────────────────
// CommandGate
// ─────────────────────────────────────────────

/// The composed gate. Run intents through it to obtain a [`GateResult`].
pub struct CommandGate {
    layers: Vec<Box<dyn GateLayer>>,
    audit: Arc<AuditLog>,
}

/// Aggregate outcome of evaluating a batch of intents.
#[derive(Debug, Default)]
pub struct GateResult {
    /// Intents cleared for dispatch, in input order.
    pub approved: Vec<CandidateIntent>,
    /// Intents rejected, with the rendered decision.
    pub rejected: Vec<(CandidateIntent, GateDecision)>,
    /// Intents awaiting asynchronous approval.
    pub pending: Vec<(CandidateIntent, String)>,
}

impl CommandGate {
    /// Build an empty gate. Add layers in pipeline order.
    pub fn new(audit: Arc<AuditLog>) -> Self {
        Self {
            layers: Vec::new(),
            audit,
        }
    }

    /// Append a layer (call in pipeline order: capability → approval → SPGS).
    pub fn with_layer(mut self, layer: Box<dyn GateLayer>) -> Self {
        self.layers.push(layer);
        self
    }

    /// Construct the standard safe gate: Capability → Approval → SPGS → Audit.
    pub fn standard(
        audit: Arc<AuditLog>,
        approval: Arc<dyn ApprovalPolicy>,
        restrictions: Arc<OpRestrictionsManager>,
    ) -> Self {
        Self::new(audit)
            .with_layer(Box::new(CapabilityGate))
            .with_layer(Box::new(ApprovalGate::new(approval, restrictions.clone())))
            .with_layer(Box::new(SpgsGate::new(restrictions)))
    }

    /// Construct the standard safe gate with the autonomy-mode profile cap
    /// inserted immediately after the capability check: `Capability →
    /// AutonomyProfile → Approval → SPGS`. The profile is shared so callers
    /// can hot-swap it without rebuilding the gate.
    pub fn standard_with_autonomy(
        audit: Arc<AuditLog>,
        approval: Arc<dyn ApprovalPolicy>,
        restrictions: Arc<OpRestrictionsManager>,
        profile: Arc<RwLock<AutonomyModeProfile>>,
    ) -> Self {
        Self::new(audit)
            .with_layer(Box::new(CapabilityGate))
            .with_layer(Box::new(AutonomyProfileGate::with_shared(profile)))
            .with_layer(Box::new(ApprovalGate::new(approval, restrictions.clone())))
            .with_layer(Box::new(SpgsGate::new(restrictions)))
    }

    /// Evaluate one intent through the full pipeline, recording an audit entry.
    pub fn evaluate(&self, intent: &CandidateIntent, ctx: &GateContext<'_>) -> GateDecision {
        for layer in &self.layers {
            match layer.check(intent, ctx) {
                GateOutcome::Pass => continue,
                GateOutcome::Reject(reason) => {
                    let decision = GateDecision::rejected(layer.stage(), reason.clone());
                    self.record(intent, &format!("rejected:{:?}:{}", layer.stage(), reason));
                    return decision;
                }
                GateOutcome::Pending(approval_id) => {
                    self.record(intent, &format!("pending:{approval_id}"));
                    return GateDecision::Pending { approval_id };
                }
            }
        }
        self.record(intent, "approved");
        GateDecision::Approved
    }

    /// Evaluate a deconflicted batch of intents. Only approved intents yield
    /// dispatchable commands.
    pub fn evaluate_batch(
        &self,
        intents: Vec<CandidateIntent>,
        ctx: &GateContext<'_>,
    ) -> GateResult {
        let mut result = GateResult::default();
        for intent in intents {
            match self.evaluate(&intent, ctx) {
                GateDecision::Approved => result.approved.push(intent),
                GateDecision::Pending { approval_id } => {
                    result.pending.push((intent, approval_id));
                }
                decision @ GateDecision::Rejected { .. } => {
                    result.rejected.push((intent, decision));
                }
            }
        }
        result
    }

    fn record(&self, intent: &CandidateIntent, outcome: &str) {
        let detail = format!(
            "{} {} [{}]",
            intent.source.label(),
            command_summary(&intent.command),
            intent.id
        );
        self.audit.record(
            intent.source.label(),
            AuditAction::CapabilityCheck,
            detail,
            outcome,
        );
    }
}

/// Short one-line summary of a command for audit/logging.
pub fn command_summary(cmd: &PlatformCommand) -> String {
    match cmd {
        PlatformCommand::FireAtTarget {
            platform_id,
            weapon_id,
            track_id,
        } => {
            format!("FireAtTarget {platform_id}/{weapon_id}->{track_id}")
        }
        PlatformCommand::FireSalvo {
            platform_id,
            weapon_id,
            track_id,
            salvo_size,
        } => {
            format!("FireSalvo {platform_id}/{weapon_id}->{track_id} x{salvo_size}")
        }
        PlatformCommand::SetHeading {
            platform_id,
            heading_deg,
            ..
        } => {
            format!("SetHeading {platform_id} {heading_deg}deg")
        }
        PlatformCommand::SetSpeed {
            platform_id,
            speed_ms,
            ..
        } => {
            format!("SetSpeed {platform_id} {speed_ms}m/s")
        }
        other => format!("{:?}", other.command_class()) + " " + other.target_platform_id(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::tactical::{CommandPriority, IntentSource};
    use openfang_types::umaa::{PlatformLimits, RulesOfEngagement};

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

    fn fire_intent(src: IntentSource) -> CandidateIntent {
        CandidateIntent::new(
            PlatformCommand::FireAtTarget {
                platform_id: "usv-01".into(),
                weapon_id: "cannon".into(),
                track_id: "trk-1".into(),
            },
            CommandPriority::Critical,
            src,
            0.0,
            "engage",
        )
    }

    fn standard_gate(roe: WeaponReleaseLevel) -> (CommandGate, Arc<AuditLog>) {
        let audit = Arc::new(AuditLog::new());
        let restrictions = Arc::new(OpRestrictionsManager::new(
            RulesOfEngagement {
                weapon_release_authority: roe,
                ..Default::default()
            },
            PlatformLimits::default(),
        ));
        let gate = CommandGate::standard(
            audit.clone(),
            Arc::new(WeaponApproval::default()),
            restrictions,
        );
        (gate, audit)
    }

    #[test]
    fn weapon_blocked_under_weapons_hold() {
        let (gate, audit) = standard_gate(WeaponReleaseLevel::WeaponsHold);
        let caps = full_caps();
        let ctx = GateContext {
            capabilities: &caps,
            snapshot: None,
            autonomy_profile: None,
        };
        let d = gate.evaluate(
            &fire_intent(IntentSource::Llm {
                agent_id: "tca".into(),
            }),
            &ctx,
        );
        match d {
            GateDecision::Rejected { stage, .. } => assert_eq!(stage, GateStage::Approval),
            other => panic!("expected reject, got {other:?}"),
        }
        // Decision was audited.
        assert_eq!(audit.len(), 1);
        assert!(audit.verify_integrity().is_ok());
    }

    #[test]
    fn weapon_pending_under_weapons_tight() {
        let (gate, _) = standard_gate(WeaponReleaseLevel::WeaponsTight);
        let caps = full_caps();
        let ctx = GateContext {
            capabilities: &caps,
            snapshot: None,
            autonomy_profile: None,
        };
        let d = gate.evaluate(
            &fire_intent(IntentSource::Llm {
                agent_id: "tca".into(),
            }),
            &ctx,
        );
        assert!(matches!(d, GateDecision::Pending { .. }));
    }

    #[test]
    fn sensor_approval_can_defer_specific_sensor_command() {
        let policy = SensorApproval::new("sensor_approval").with_pending_sensor("surf_radar");
        let caps = full_caps();
        let ctx = GateContext {
            capabilities: &caps,
            snapshot: None,
            autonomy_profile: Some("default"),
        };
        let intent = CandidateIntent::new(
            PlatformCommand::SensorOn {
                platform_id: "self".into(),
                sensor_id: "surf_radar".into(),
            },
            CommandPriority::Normal,
            IntentSource::Llm {
                agent_id: "sms-test".into(),
            },
            0.0,
            "radar on",
        );

        assert!(matches!(
            policy.evaluate(&intent, WeaponReleaseLevel::WeaponsHold, &ctx),
            GateOutcome::Pending(_)
        ));
    }

    #[test]
    fn weapon_approved_under_weapons_free() {
        let (gate, _) = standard_gate(WeaponReleaseLevel::WeaponsFree);
        let caps = full_caps();
        let ctx = GateContext {
            capabilities: &caps,
            snapshot: None,
            autonomy_profile: None,
        };
        let d = gate.evaluate(
            &fire_intent(IntentSource::Llm {
                agent_id: "tca".into(),
            }),
            &ctx,
        );
        assert!(d.is_approved());
    }

    #[test]
    fn dcc_critical_weapon_cannot_bypass_gate() {
        // A DCC reflex emitting a weapon intent is still blocked under WeaponsHold.
        let (gate, audit) = standard_gate(WeaponReleaseLevel::WeaponsHold);
        let caps = full_caps();
        let ctx = GateContext {
            capabilities: &caps,
            snapshot: None,
            autonomy_profile: None,
        };
        let intent = fire_intent(IntentSource::Dcc {
            rule_name: "auto_engage".into(),
        });
        let d = gate.evaluate(&intent, &ctx);
        assert!(!d.is_approved(), "DCC must not bypass SPGS/approval");
        assert_eq!(audit.len(), 1);
    }

    #[test]
    fn configurable_approval_authorized_target_passes_known_track() {
        let registry = Arc::new(crate::target_authorization::TargetAuthorizationRegistry::new());
        registry.authorize("usv-01", "trk-1", "operator-1", 0.0);
        let gate = crate::intervention::InterventionGate::new(
            openfang_types::config::InterventionConfig {
                rules: vec![openfang_types::config::InterventionRule {
                    stage: vec!["weapon_release".into()],
                    platform_ids: vec!["usv-01".into()],
                    command_classes: vec!["weapon".into()],
                    sources: vec!["llm".into()],
                    mode: openfang_types::config::InterventionMode::AuthorizedTarget,
                    quorum: 1,
                    window_s: 30.0,
                }],
                ..Default::default()
            },
            registry,
            Arc::new(crate::mission_approval::MissionApprovalRegistry::new()),
        );
        let policy = ConfigurableApproval::new(gate);
        let caps = full_caps();
        let ctx = GateContext {
            capabilities: &caps,
            snapshot: None,
            autonomy_profile: Some("supervised_autonomy"),
        };
        let outcome = policy.evaluate(
            &fire_intent(IntentSource::Llm {
                agent_id: "fca".into(),
            }),
            WeaponReleaseLevel::WeaponsTight,
            &ctx,
        );
        assert!(matches!(outcome, GateOutcome::Pass));
    }

    #[test]
    fn configurable_approval_authorized_target_pends_unknown_track() {
        let gate = crate::intervention::InterventionGate::new(
            openfang_types::config::InterventionConfig {
                rules: vec![openfang_types::config::InterventionRule {
                    stage: vec!["weapon_release".into()],
                    platform_ids: vec!["usv-01".into()],
                    command_classes: vec!["weapon".into()],
                    sources: vec!["llm".into()],
                    mode: openfang_types::config::InterventionMode::AuthorizedTarget,
                    quorum: 1,
                    window_s: 30.0,
                }],
                ..Default::default()
            },
            Arc::new(crate::target_authorization::TargetAuthorizationRegistry::new()),
            Arc::new(crate::mission_approval::MissionApprovalRegistry::new()),
        );
        let policy = ConfigurableApproval::new(gate);
        let caps = full_caps();
        let ctx = GateContext {
            capabilities: &caps,
            snapshot: None,
            autonomy_profile: Some("supervised_autonomy"),
        };
        let outcome = policy.evaluate(
            &fire_intent(IntentSource::Llm {
                agent_id: "fca".into(),
            }),
            WeaponReleaseLevel::WeaponsTight,
            &ctx,
        );
        assert!(matches!(outcome, GateOutcome::Pending(_)));
    }

    #[test]
    fn configurable_confirm_can_release_fast_loop_intervention_after_approval() {
        let approvals = Arc::new(crate::mission_approval::MissionApprovalRegistry::new());
        let gate = crate::intervention::InterventionGate::new(
            openfang_types::config::InterventionConfig {
                rules: vec![openfang_types::config::InterventionRule {
                    stage: vec!["weapon_release".into()],
                    platform_ids: vec!["usv-01".into()],
                    command_classes: vec!["weapon".into()],
                    sources: vec!["llm".into()],
                    mode: openfang_types::config::InterventionMode::Confirm,
                    quorum: 1,
                    window_s: 30.0,
                }],
                ..Default::default()
            },
            Arc::new(crate::target_authorization::TargetAuthorizationRegistry::new()),
            Arc::clone(&approvals),
        );
        let policy = ConfigurableApproval::new(gate);
        let intent = fire_intent(IntentSource::Llm {
            agent_id: "fca".into(),
        });
        let caps = full_caps();
        let ctx = GateContext {
            capabilities: &caps,
            snapshot: None,
            autonomy_profile: Some("supervised_autonomy"),
        };

        let outcome = policy.evaluate(&intent, WeaponReleaseLevel::WeaponsTight, &ctx);
        let GateOutcome::Pending(approval_id) = outcome else {
            panic!("expected pending intervention approval");
        };
        assert!(approval_id.starts_with("intervention:weapon_release:1:"));

        approvals.approve(&approval_id, "operator", 1.0);
        let outcome = policy.evaluate(&intent, WeaponReleaseLevel::WeaponsTight, &ctx);
        assert!(matches!(outcome, GateOutcome::Pass));
    }

    #[test]
    fn capability_missing_rejects() {
        let audit = Arc::new(AuditLog::new());
        let restrictions = Arc::new(OpRestrictionsManager::default_restrictions("usv-01"));
        let gate = CommandGate::standard(audit, Arc::new(WeaponApproval::default()), restrictions);
        let mut caps = full_caps();
        caps.supports_weapon_control = false;
        let ctx = GateContext {
            capabilities: &caps,
            snapshot: None,
            autonomy_profile: None,
        };
        let d = gate.evaluate(
            &fire_intent(IntentSource::Llm {
                agent_id: "tca".into(),
            }),
            &ctx,
        );
        match d {
            GateDecision::Rejected { stage, .. } => assert_eq!(stage, GateStage::Capability),
            other => panic!("expected capability reject, got {other:?}"),
        }
    }

    #[test]
    fn over_speed_motion_rejected_by_spgs() {
        let (gate, _) = standard_gate(WeaponReleaseLevel::WeaponsFree);
        let caps = full_caps();
        let ctx = GateContext {
            capabilities: &caps,
            snapshot: None,
            autonomy_profile: Some("supervised_autonomy"),
        };
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
        let d = gate.evaluate(&intent, &ctx);
        match d {
            GateDecision::Rejected { stage, .. } => assert_eq!(stage, GateStage::Spgs),
            other => panic!("expected SPGS reject, got {other:?}"),
        }
    }

    fn motion_intent_for_speed(speed: f64) -> CandidateIntent {
        CandidateIntent::new(
            PlatformCommand::SetSpeed {
                platform_id: "usv-01".into(),
                speed_ms: speed,
                acceleration_ms2: None,
            },
            CommandPriority::Normal,
            IntentSource::Llm {
                agent_id: "na".into(),
            },
            0.0,
            "test",
        )
    }

    #[test]
    fn autonomy_observe_only_rejects_motion() {
        let audit = Arc::new(AuditLog::new());
        let restrictions = Arc::new(OpRestrictionsManager::new(
            RulesOfEngagement::default(),
            PlatformLimits::default(),
        ));
        let profile = openfang_types::config::AutonomyModeProfile {
            id: "observe_only".into(),
            advisory_classes: vec!["motion".into(), "sensor".into(), "weapon".into()],
            weapon_disposition: openfang_types::config::WeaponDisposition::SuggestOnly,
            ..Default::default()
        };
        let shared = Arc::new(RwLock::new(profile));
        let gate = CommandGate::standard_with_autonomy(
            audit.clone(),
            Arc::new(WeaponApproval::default()),
            restrictions,
            shared,
        );
        let caps = full_caps();
        let ctx = GateContext {
            capabilities: &caps,
            snapshot: None,
            autonomy_profile: Some("supervised_autonomy"),
        };
        let d = gate.evaluate(&motion_intent_for_speed(5.0), &ctx);
        match d {
            GateDecision::Rejected { reason, .. } => assert!(reason.contains("observe_only")),
            other => panic!("expected reject, got {other:?}"),
        }
        assert_eq!(audit.len(), 1);
    }

    #[test]
    fn autonomy_supervised_pends_weapons() {
        let audit = Arc::new(AuditLog::new());
        let restrictions = Arc::new(OpRestrictionsManager::new(
            RulesOfEngagement {
                weapon_release_authority: WeaponReleaseLevel::WeaponsFree,
                ..Default::default()
            },
            PlatformLimits::default(),
        ));
        let profile = openfang_types::config::AutonomyModeProfile {
            id: "supervised_autonomy".into(),
            auto_classes: vec!["motion".into(), "sensor".into(), "comm".into()],
            weapon_disposition: openfang_types::config::WeaponDisposition::PendingApproval,
            ..Default::default()
        };
        let shared = Arc::new(RwLock::new(profile));
        let gate = CommandGate::standard_with_autonomy(
            audit,
            Arc::new(WeaponApproval::default()),
            restrictions,
            shared,
        );
        let caps = full_caps();
        let ctx = GateContext {
            capabilities: &caps,
            snapshot: None,
            autonomy_profile: Some("supervised_autonomy"),
        };
        let d = gate.evaluate(
            &fire_intent(IntentSource::Llm {
                agent_id: "fca".into(),
            }),
            &ctx,
        );
        assert!(matches!(d, GateDecision::Pending { .. }));
    }

    #[test]
    fn autonomy_hot_swap_updates_envelope() {
        let audit = Arc::new(AuditLog::new());
        let restrictions = Arc::new(OpRestrictionsManager::new(
            RulesOfEngagement {
                weapon_release_authority: WeaponReleaseLevel::WeaponsFree,
                ..Default::default()
            },
            PlatformLimits::default(),
        ));
        let observe = openfang_types::config::AutonomyModeProfile {
            id: "observe_only".into(),
            advisory_classes: vec!["motion".into()],
            ..Default::default()
        };
        let supervised = openfang_types::config::AutonomyModeProfile {
            id: "supervised_autonomy".into(),
            auto_classes: vec!["motion".into()],
            ..Default::default()
        };
        let shared = Arc::new(RwLock::new(observe));
        let gate = CommandGate::standard_with_autonomy(
            audit,
            Arc::new(WeaponApproval::default()),
            restrictions,
            Arc::clone(&shared),
        );
        let caps = full_caps();
        let ctx = GateContext {
            capabilities: &caps,
            snapshot: None,
            autonomy_profile: None,
        };
        // observe_only → motion rejected
        let d = gate.evaluate(&motion_intent_for_speed(5.0), &ctx);
        assert!(!d.is_approved(), "expected reject under observe_only");
        // Hot-swap to supervised_autonomy
        *shared.write().unwrap() = supervised;
        let d = gate.evaluate(&motion_intent_for_speed(5.0), &ctx);
        assert!(d.is_approved(), "expected approved under supervised");
    }

    #[test]
    fn batch_splits_approved_and_rejected() {
        let (gate, _) = standard_gate(WeaponReleaseLevel::WeaponsHold);
        let caps = full_caps();
        let ctx = GateContext {
            capabilities: &caps,
            snapshot: None,
            autonomy_profile: None,
        };
        let intents = vec![
            CandidateIntent::new(
                PlatformCommand::SetHeading {
                    platform_id: "usv-01".into(),
                    heading_deg: 90.0,
                    speed_ms: None,
                    turn_direction: None,
                },
                CommandPriority::Normal,
                IntentSource::Llm {
                    agent_id: "na".into(),
                },
                0.0,
                "turn",
            ),
            fire_intent(IntentSource::Llm {
                agent_id: "fca".into(),
            }),
        ];
        let res = gate.evaluate_batch(intents, &ctx);
        assert_eq!(res.approved.len(), 1);
        assert_eq!(res.rejected.len(), 1);
    }

    #[test]
    fn supervised_autonomy_auto_routes_isr_deploy_while_kinetic_weapon_pends() {
        use openfang_types::config::{AutonomyModeProfile, WeaponDisposition};
        use std::sync::{Arc, RwLock};

        let profile = Arc::new(RwLock::new(AutonomyModeProfile {
            id: "supervised_autonomy".into(),
            auto_classes: vec!["motion".into(), "sensor".into(), "comm".into()],
            weapon_disposition: WeaponDisposition::PendingApproval,
            ..Default::default()
        }));
        let layer = AutonomyProfileGate::with_shared(profile);
        let ctx = GateContext {
            capabilities: &full_caps(),
            snapshot: None,
            autonomy_profile: Some("supervised_autonomy"),
        };
        let scout = CandidateIntent::new(
            PlatformCommand::FireAtTarget {
                platform_id: "self".into(),
                weapon_id: "scout_uav_slot".into(),
                track_id: "self:1".into(),
            },
            CommandPriority::Normal,
            IntentSource::Workflow {
                workflow_id: "recon".into(),
            },
            0.0,
            "deploy scout",
        );
        assert!(
            matches!(layer.check(&scout, &ctx), GateOutcome::Pass),
            "ISR deploy must auto-route under supervised_autonomy"
        );
        assert!(
            matches!(
                layer.check(
                    &fire_intent(IntentSource::Workflow {
                        workflow_id: "strike".into()
                    }),
                    &ctx
                ),
                GateOutcome::Pass
            ),
            "kinetic weapon must reach WMS approval instead of stopping at autonomy layer"
        );

        let policy = WeaponApproval::default();
        let caps = full_caps();
        let policy_ctx = GateContext {
            capabilities: &caps,
            snapshot: None,
            autonomy_profile: Some("supervised_autonomy"),
        };
        assert!(matches!(
            policy.evaluate(&scout, WeaponReleaseLevel::WeaponsTight, &policy_ctx),
            GateOutcome::Pass
        ));
        assert!(matches!(
            policy.evaluate(
                &fire_intent(IntentSource::Workflow {
                    workflow_id: "strike".into()
                }),
                WeaponReleaseLevel::WeaponsTight,
                &policy_ctx
            ),
            GateOutcome::Pending(_)
        ));
    }

    #[test]
    fn j7_uav_weapon_auto_routes_as_isr_even_under_weapons_hold() {
        let (gate, _) = standard_gate(WeaponReleaseLevel::WeaponsHold);
        let caps = full_caps();
        let ctx = GateContext {
            capabilities: &caps,
            snapshot: None,
            autonomy_profile: Some("supervised_autonomy"),
        };
        let intent = CandidateIntent::new(
            PlatformCommand::FireAtTarget {
                platform_id: "self".into(),
                weapon_id: "J7_UAV_WEAPON".into(),
                track_id: "self:1".into(),
            },
            CommandPriority::Normal,
            IntentSource::Workflow {
                workflow_id: "recon".into(),
            },
            0.0,
            "deploy J7 scout",
        );

        assert!(
            gate.evaluate(&intent, &ctx).is_approved(),
            "J7 UAV deploy is ISR asset employment, not kinetic weapon release"
        );
    }
}
