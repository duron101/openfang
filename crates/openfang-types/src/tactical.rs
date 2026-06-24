//! Tactical command-pipeline contracts — the safety boundary types.
//!
//! These types define the *one and only* path a desired action may take from a
//! decision producer (LLM Agent, Direct Command Channel, operator, workflow) to
//! a platform adapter:
//!
//! ```text
//! producer ──► CandidateIntent ──► ActionComposer ──► CommandGate ──► PlatformCommand ──► Adapter
//! ```
//!
//! Producers may ONLY emit [`CandidateIntent`]. A [`CandidateIntent`] is *not*
//! a sendable command — it must be composed (priority/conflict resolution) and
//! then cleared by the command gate (capability → approval → SPGS → audit)
//! before it becomes a dispatchable [`crate::platform::PlatformCommand`].
//!
//! This module deliberately lives in the leaf `openfang-types` crate so every
//! layer shares the exact same contract and no layer can invent a side channel.

use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::platform::PlatformCommand;

// ─────────────────────────────────────────────
// Command priority
// ─────────────────────────────────────────────

/// Priority of a tactical action. Lower discriminant means higher urgency.
///
/// `Critical` actions (e.g. evasive maneuver from the DCC) may *preempt* the
/// active plan during composition, but they can never bypass the command gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandPriority {
    /// Time-critical reflex (collision/evasion). Preempts the active plan.
    Critical = 0,
    /// Ahead of routine LLM commands, but does not preempt.
    High = 1,
    /// Standard slow-loop (LLM/workflow) plan.
    Normal = 2,
}

impl CommandPriority {
    /// Whether an action at this priority may preempt the active plan.
    pub fn preempts(&self) -> bool {
        matches!(self, Self::Critical)
    }
}

// ─────────────────────────────────────────────
// Intent source
// ─────────────────────────────────────────────

/// Where a [`CandidateIntent`] originated. Drives audit provenance and the
/// composition policy (only some sources are allowed to preempt).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail")]
pub enum IntentSource {
    /// Slow-loop LLM agent decision.
    Llm { agent_id: String },
    /// Fast-loop Direct Command Channel rule.
    Dcc { rule_name: String },
    /// Human operator (shore C2).
    Operator { operator_id: String },
    /// Deterministic workflow step.
    Workflow { workflow_id: String },
    /// Anything else (test harness, external integration).
    External { label: String },
}

impl IntentSource {
    /// Short, stable label for audit detail lines.
    pub fn label(&self) -> String {
        match self {
            Self::Llm { agent_id } => format!("llm:{agent_id}"),
            Self::Dcc { rule_name } => format!("dcc:{rule_name}"),
            Self::Operator { operator_id } => format!("operator:{operator_id}"),
            Self::Workflow { workflow_id } => format!("workflow:{workflow_id}"),
            Self::External { label } => format!("external:{label}"),
        }
    }
}

// ─────────────────────────────────────────────
// Candidate intent
// ─────────────────────────────────────────────

/// A desired action produced by a decision source.
///
/// A `CandidateIntent` is intentionally *not* directly dispatchable. It carries
/// the proposed [`PlatformCommand`] plus the metadata the composer and gate need
/// to order, deconflict, approve, and audit it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateIntent {
    /// Stable unique id (audit correlation across compose → gate → adapter).
    pub id: String,
    /// The proposed command. Not valid to send until gate-approved.
    pub command: PlatformCommand,
    /// Urgency / preemption class.
    pub priority: CommandPriority,
    /// Provenance.
    pub source: IntentSource,
    /// Producer timestamp (seconds; sim or wall, per the active TimeSource).
    pub issued_at: f64,
    /// Human-readable justification (logged + audited).
    pub reason: String,
}

impl CandidateIntent {
    /// Create a new intent with a generated id.
    pub fn new(
        command: PlatformCommand,
        priority: CommandPriority,
        source: IntentSource,
        issued_at: f64,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            command,
            priority,
            source,
            issued_at,
            reason: reason.into(),
        }
    }

    /// Target platform id of the wrapped command.
    pub fn target_platform_id(&self) -> &str {
        self.command.target_platform_id()
    }

    /// Class of the wrapped command (used for capability routing & conflict keys).
    pub fn class(&self) -> CommandClass {
        self.command.command_class()
    }

    /// Conflict-resolution key: two intents with the same key target the same
    /// effector channel on the same platform and cannot both be active.
    pub fn conflict_key(&self) -> (String, CommandClass, String) {
        (
            self.target_platform_id().to_string(),
            self.class(),
            self.command.effector_subchannel(),
        )
    }
}

// ─────────────────────────────────────────────
// Command classification
// ─────────────────────────────────────────────

/// Coarse classification of a [`PlatformCommand`] used for capability checks,
/// gate routing, and conflict resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandClass {
    Motion,
    Sensor,
    Weapon,
    ElectronicWarfare,
    Comm,
    Command,
    Uav,
    Formation,
    Aux,
}

impl CommandClass {
    /// Whether this class is weapons-release related (gated by ROE/approval).
    pub fn is_weapon(&self) -> bool {
        matches!(self, Self::Weapon)
    }

    /// Stable lowercase identifier used in config, audit logs, and policy
    /// manifests. Mirrors the `serde(rename_all = "snake_case")` form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Motion => "motion",
            Self::Sensor => "sensor",
            Self::Weapon => "weapon",
            Self::ElectronicWarfare => "electronic_warfare",
            Self::Comm => "comm",
            Self::Command => "command",
            Self::Uav => "uav",
            Self::Formation => "formation",
            Self::Aux => "aux",
        }
    }

    /// Parse a config/manifest token into a [`CommandClass`]. Accepts
    /// `electronic_warfare`/`ew`, `c2`/`command`, etc.
    pub fn from_token(token: &str) -> Option<Self> {
        match token.trim().to_ascii_lowercase().as_str() {
            "motion" | "nav" | "maneuver" => Some(Self::Motion),
            "sensor" | "isr" => Some(Self::Sensor),
            "weapon" | "fire" | "engage" => Some(Self::Weapon),
            "electronic_warfare" | "ew" | "jam" => Some(Self::ElectronicWarfare),
            "comm" | "comms" | "communications" => Some(Self::Comm),
            "command" | "c2" => Some(Self::Command),
            "uav" => Some(Self::Uav),
            "formation" => Some(Self::Formation),
            "aux" | "auxiliary" | "survivability" => Some(Self::Aux),
            _ => None,
        }
    }
}

// ─────────────────────────────────────────────
// Tactical agent policy
// ─────────────────────────────────────────────

/// Per-agent tactical envelope — declares which [`CommandClass`]es this LLM
/// persona is permitted to emit, which autonomy profiles it may operate in,
/// and whether every one of its tool-driven intents must be funneled into the
/// human-approval queue regardless of the active profile.
///
/// This is a *soft* layer enforced at the LLM↔platform boundary
/// ([`dispatch_platform_command`](crate::kernel_handle::KernelHandle::dispatch_platform_command)).
/// It exists to make agent intent explicit and to give the gate a place to
/// reject obvious cross-domain over-reach with a clear audit reason **before**
/// the intent enters the action composer. It does NOT replace the hard
/// SPGS/ROE/quorum gates downstream — those still apply for every intent that
/// passes this filter.
///
/// Empty / default policy means "no extra restriction beyond the standard
/// safety pipeline" — backwards-compatible with existing manifests.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TacticalAgentPolicy {
    /// Allow-list of [`CommandClass`] tokens this agent may emit. Empty = no
    /// extra restriction (only the platform capability + autonomy profile
    /// gates apply). Tokens follow [`CommandClass::from_token`].
    pub allowed_command_classes: Vec<String>,
    /// Allow-list of autonomy profile ids in which this agent's intents may be
    /// auto-routed to the pipeline. When the active profile is not on this
    /// list, intents are converted to advisory entries (no actuation). Empty
    /// = participates in every profile.
    pub allowed_autonomy_modes: Vec<String>,
    /// When `true`, every intent produced by this agent is queued for human
    /// approval even if the autonomy profile would normally auto-execute the
    /// class. Useful for fire-control-style personas under defensive autonomy.
    pub requires_human_approval: bool,
    /// When `true`, this agent is purely advisory: it may produce
    /// [`CommanderIntent`]-style suggestions but its [`PlatformCommand`] tool
    /// calls are dropped with an audit entry. Mirrors `observe_only` semantics
    /// at the per-agent level.
    pub advisory_only: bool,
}

impl TacticalAgentPolicy {
    /// Whether the policy permits the given [`CommandClass`]. An empty
    /// allowlist permits everything (subject to downstream gates).
    pub fn allows_class(&self, class: CommandClass) -> bool {
        if self.allowed_command_classes.is_empty() {
            return true;
        }
        self.allowed_command_classes
            .iter()
            .filter_map(|t| CommandClass::from_token(t))
            .any(|c| c == class)
    }

    /// Whether the agent may participate in the given autonomy profile id.
    /// An empty allowlist participates everywhere.
    pub fn allows_autonomy_mode(&self, mode_id: &str) -> bool {
        if self.allowed_autonomy_modes.is_empty() {
            return true;
        }
        self.allowed_autonomy_modes
            .iter()
            .any(|m| m.eq_ignore_ascii_case(mode_id))
    }
}

/// Outcome of evaluating a tool-emitted [`CandidateIntent`] against the
/// calling agent's [`TacticalAgentPolicy`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TacticalPolicyOutcome {
    /// Intent may proceed into the pipeline (standard SPGS/ROE/quorum still
    /// apply downstream).
    Allow,
    /// Intent is dropped at the LLM↔platform boundary. Caller should audit the
    /// rejection and surface the reason to the LLM so it self-corrects.
    Deny(String),
    /// Intent is dropped but counts as advisory: the producing agent only
    /// suggests, never actuates.
    Advisory(String),
    /// Intent must be funneled to the human-approval queue regardless of the
    /// active autonomy profile.
    RequireApproval(String),
}

impl TacticalAgentPolicy {
    /// Evaluate a [`CandidateIntent`] for this policy in the context of the
    /// active autonomy profile id.
    pub fn evaluate(&self, class: CommandClass, autonomy_mode_id: &str) -> TacticalPolicyOutcome {
        if self.advisory_only {
            return TacticalPolicyOutcome::Advisory(
                "agent is advisory_only — intent recorded as suggestion".into(),
            );
        }
        if !self.allows_autonomy_mode(autonomy_mode_id) {
            return TacticalPolicyOutcome::Deny(format!(
                "agent not permitted to act under autonomy profile '{autonomy_mode_id}'"
            ));
        }
        if !self.allows_class(class) {
            return TacticalPolicyOutcome::Deny(format!(
                "agent not permitted to emit {} commands",
                class.as_str()
            ));
        }
        if self.requires_human_approval {
            return TacticalPolicyOutcome::RequireApproval(
                "agent policy requires per-intent human approval".into(),
            );
        }
        TacticalPolicyOutcome::Allow
    }
}

// ─────────────────────────────────────────────
// Gate decision
// ─────────────────────────────────────────────

/// The stage of the gate pipeline at which a decision was rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateStage {
    Capability,
    Approval,
    Spgs,
    Audit,
}

/// Outcome of running a [`CandidateIntent`] through the command gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum GateDecision {
    /// Cleared — caller may dispatch the wrapped command.
    Approved,
    /// Blocked at `stage` for `reason`. Must NOT be dispatched.
    Rejected { stage: GateStage, reason: String },
    /// Held pending an asynchronous human/quorum approval.
    Pending { approval_id: String },
}

impl GateDecision {
    pub fn is_approved(&self) -> bool {
        matches!(self, Self::Approved)
    }
    pub fn rejected(stage: GateStage, reason: impl Into<String>) -> Self {
        Self::Rejected {
            stage,
            reason: reason.into(),
        }
    }
}

// ─────────────────────────────────────────────
// Time source
// ─────────────────────────────────────────────

/// Abstraction over "now". Sim backends drive time from
/// [`crate::platform::WorldSnapshot::timestamp`]; hardware uses wall clock.
///
/// All timeout / cooldown / deadline logic must take time from a `TimeSource`
/// so simulation and hardware behave identically under test.
pub trait TimeSource: Send + Sync {
    /// Current time in seconds.
    fn now_secs(&self) -> f64;
}

/// Wall-clock time source for hardware / live operation.
#[derive(Debug, Default, Clone, Copy)]
pub struct WallClock;

impl TimeSource for WallClock {
    fn now_secs(&self) -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    }
}

/// Manually-advanced time source for simulation and deterministic tests.
///
/// Either set absolute sim time (e.g. from `WorldSnapshot.timestamp`) or advance
/// it by a delta each tick.
pub struct ManualClock {
    secs: Mutex<f64>,
}

impl ManualClock {
    pub fn new(initial_secs: f64) -> Self {
        Self {
            secs: Mutex::new(initial_secs),
        }
    }
    /// Set the absolute current time (e.g. from a fresh snapshot timestamp).
    pub fn set(&self, secs: f64) {
        *self.secs.lock().unwrap_or_else(|e| e.into_inner()) = secs;
    }
    /// Advance time by `dt` seconds.
    pub fn advance(&self, dt: f64) {
        *self.secs.lock().unwrap_or_else(|e| e.into_inner()) += dt;
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new(0.0)
    }
}

impl TimeSource for ManualClock {
    fn now_secs(&self) -> f64 {
        *self.secs.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_ordering_and_preemption() {
        assert!(CommandPriority::Critical < CommandPriority::High);
        assert!(CommandPriority::High < CommandPriority::Normal);
        assert!(CommandPriority::Critical.preempts());
        assert!(!CommandPriority::Normal.preempts());
    }

    #[test]
    fn manual_clock_set_and_advance() {
        let c = ManualClock::new(10.0);
        assert_eq!(c.now_secs(), 10.0);
        c.advance(2.5);
        assert_eq!(c.now_secs(), 12.5);
        c.set(100.0);
        assert_eq!(c.now_secs(), 100.0);
    }

    #[test]
    fn intent_conflict_key_groups_same_effector() {
        let mk = |hdg: f64| {
            CandidateIntent::new(
                PlatformCommand::SetHeading {
                    platform_id: "usv-01".into(),
                    heading_deg: hdg,
                    speed_ms: None,
                    turn_direction: None,
                },
                CommandPriority::Normal,
                IntentSource::External {
                    label: "test".into(),
                },
                0.0,
                "t",
            )
        };
        let a = mk(90.0);
        let b = mk(180.0);
        assert_eq!(a.conflict_key(), b.conflict_key());
        assert_eq!(a.class(), CommandClass::Motion);
    }

    #[test]
    fn motion_axes_resolve_on_independent_lanes() {
        let source = || IntentSource::External {
            label: "test".into(),
        };
        let heading = CandidateIntent::new(
            PlatformCommand::SetHeading {
                platform_id: "usv-01".into(),
                heading_deg: 90.0,
                speed_ms: None,
                turn_direction: None,
            },
            CommandPriority::Normal,
            source(),
            0.0,
            "t",
        );
        let speed = CandidateIntent::new(
            PlatformCommand::SetSpeed {
                platform_id: "usv-01".into(),
                speed_ms: 12.0,
                acceleration_ms2: None,
            },
            CommandPriority::Normal,
            source(),
            0.0,
            "t",
        );
        let altitude = CandidateIntent::new(
            PlatformCommand::SetAltitude {
                platform_id: "usv-01".into(),
                altitude_m: 500.0,
                rate_ms: None,
            },
            CommandPriority::Normal,
            source(),
            0.0,
            "t",
        );
        let goto = CandidateIntent::new(
            PlatformCommand::GotoLocation {
                platform_id: "usv-01".into(),
                lat: 30.0,
                lon: 120.0,
                alt: None,
                speed_ms: None,
            },
            CommandPriority::Normal,
            source(),
            0.0,
            "t",
        );

        // Same Motion class, but heading / speed / altitude are distinct lanes so
        // they never evict one another during conflict resolution.
        assert_eq!(heading.class(), CommandClass::Motion);
        assert_eq!(speed.class(), CommandClass::Motion);
        assert_ne!(heading.conflict_key(), speed.conflict_key());
        assert_ne!(heading.conflict_key(), altitude.conflict_key());
        assert_ne!(speed.conflict_key(), altitude.conflict_key());
        assert_ne!(heading.conflict_key(), goto.conflict_key());
        assert_eq!(heading.conflict_key().2, "heading");
        assert_eq!(speed.conflict_key().2, "speed");
        assert_eq!(altitude.conflict_key().2, "altitude");
        assert_eq!(goto.conflict_key().2, "nav");
    }

    #[test]
    fn weapon_conflict_key_keeps_parallel_engagements_distinct() {
        let source = || IntentSource::External {
            label: "test".into(),
        };
        let fire_a = CandidateIntent::new(
            PlatformCommand::FireAtTarget {
                platform_id: "self".into(),
                weapon_id: "loiter_wave3".into(),
                track_id: "blue_patrol_1".into(),
            },
            CommandPriority::Normal,
            source(),
            1.0,
            "fire a",
        );
        let fire_b = CandidateIntent::new(
            PlatformCommand::FireAtTarget {
                platform_id: "self".into(),
                weapon_id: "loiter_wave3".into(),
                track_id: "blue_patrol_2".into(),
            },
            CommandPriority::Normal,
            source(),
            1.0,
            "fire b",
        );
        let designate = CandidateIntent::new(
            PlatformCommand::UpdateTarget {
                platform_id: "self".into(),
                track_id: "blue_patrol_1".into(),
            },
            CommandPriority::Normal,
            source(),
            1.0,
            "designate",
        );
        let safe = CandidateIntent::new(
            PlatformCommand::WeaponSafeAll {
                platform_id: "self".into(),
            },
            CommandPriority::Normal,
            source(),
            1.0,
            "safe",
        );

        assert_ne!(fire_a.conflict_key(), fire_b.conflict_key());
        assert_ne!(fire_a.conflict_key(), designate.conflict_key());
        assert_ne!(fire_a.conflict_key(), safe.conflict_key());
        assert_eq!(fire_a.conflict_key().2, "fire:loiter_wave3->blue_patrol_1");
        assert_eq!(designate.conflict_key().2, "designate:blue_patrol_1");
        assert_eq!(safe.conflict_key().2, "safe");
    }

    #[test]
    fn sensor_and_ew_conflict_keys_are_per_component() {
        let source = || IntentSource::External {
            label: "test".into(),
        };
        let sensor_a = CandidateIntent::new(
            PlatformCommand::SensorOn {
                platform_id: "self".into(),
                sensor_id: "eoir".into(),
            },
            CommandPriority::Normal,
            source(),
            1.0,
            "sensor a",
        );
        let sensor_b = CandidateIntent::new(
            PlatformCommand::SensorOff {
                platform_id: "self".into(),
                sensor_id: "surf_radar".into(),
            },
            CommandPriority::Normal,
            source(),
            1.0,
            "sensor b",
        );
        let jam_a = CandidateIntent::new(
            PlatformCommand::JamStop {
                platform_id: "self".into(),
                jammer_id: "jammer_a".into(),
            },
            CommandPriority::Normal,
            source(),
            1.0,
            "jam a",
        );
        let jam_b = CandidateIntent::new(
            PlatformCommand::JamSetMode {
                platform_id: "self".into(),
                jammer_id: "jammer_b".into(),
                frequency_hz: Some(1.0),
                bandwidth_hz: None,
            },
            CommandPriority::Normal,
            source(),
            1.0,
            "jam b",
        );

        assert_ne!(sensor_a.conflict_key(), sensor_b.conflict_key());
        assert_ne!(jam_a.conflict_key(), jam_b.conflict_key());
        assert_eq!(sensor_a.conflict_key().2, "sensor:eoir");
        assert_eq!(sensor_b.conflict_key().2, "sensor:surf_radar");
        assert_eq!(jam_a.conflict_key().2, "jam:jammer_a");
        assert_eq!(jam_b.conflict_key().2, "jam:jammer_b");
    }

    #[test]
    fn gate_decision_helpers() {
        assert!(GateDecision::Approved.is_approved());
        let r = GateDecision::rejected(GateStage::Spgs, "weapons hold");
        assert!(!r.is_approved());
    }

    #[test]
    fn command_class_tokens_roundtrip() {
        for class in [
            CommandClass::Motion,
            CommandClass::Sensor,
            CommandClass::Weapon,
            CommandClass::ElectronicWarfare,
            CommandClass::Comm,
            CommandClass::Command,
            CommandClass::Uav,
            CommandClass::Formation,
            CommandClass::Aux,
        ] {
            assert_eq!(CommandClass::from_token(class.as_str()), Some(class));
        }
        // Aliases
        assert_eq!(
            CommandClass::from_token("ew"),
            Some(CommandClass::ElectronicWarfare)
        );
        assert_eq!(CommandClass::from_token("c2"), Some(CommandClass::Command));
        assert_eq!(
            CommandClass::from_token("survivability"),
            Some(CommandClass::Aux)
        );
        assert_eq!(CommandClass::from_token("unknown"), None);
    }

    #[test]
    fn tactical_policy_empty_allows_everything() {
        let policy = TacticalAgentPolicy::default();
        assert!(policy.allows_class(CommandClass::Weapon));
        assert!(policy.allows_autonomy_mode("supervised_autonomy"));
        assert_eq!(
            policy.evaluate(CommandClass::Motion, "supervised_autonomy"),
            TacticalPolicyOutcome::Allow,
        );
    }

    #[test]
    fn tactical_policy_class_allowlist_denies_other_classes() {
        let policy = TacticalAgentPolicy {
            allowed_command_classes: vec!["motion".into(), "sensor".into()],
            ..Default::default()
        };
        assert!(policy.allows_class(CommandClass::Motion));
        assert!(policy.allows_class(CommandClass::Sensor));
        assert!(!policy.allows_class(CommandClass::Weapon));
        match policy.evaluate(CommandClass::Weapon, "supervised_autonomy") {
            TacticalPolicyOutcome::Deny(reason) => assert!(reason.contains("weapon")),
            other => panic!("expected deny, got {other:?}"),
        }
    }

    #[test]
    fn tactical_policy_autonomy_mode_gate() {
        let policy = TacticalAgentPolicy {
            allowed_autonomy_modes: vec!["defensive_autonomy".into()],
            ..Default::default()
        };
        assert!(policy.allows_autonomy_mode("defensive_autonomy"));
        assert!(!policy.allows_autonomy_mode("weapons_free_constrained"));
        match policy.evaluate(CommandClass::Motion, "weapons_free_constrained") {
            TacticalPolicyOutcome::Deny(reason) => assert!(reason.contains("autonomy profile")),
            other => panic!("expected deny, got {other:?}"),
        }
    }

    #[test]
    fn tactical_policy_advisory_only_short_circuits() {
        let policy = TacticalAgentPolicy {
            advisory_only: true,
            ..Default::default()
        };
        match policy.evaluate(CommandClass::Motion, "supervised_autonomy") {
            TacticalPolicyOutcome::Advisory(_) => {}
            other => panic!("expected advisory, got {other:?}"),
        }
    }

    #[test]
    fn tactical_policy_requires_approval_routes_to_human() {
        let policy = TacticalAgentPolicy {
            requires_human_approval: true,
            ..Default::default()
        };
        match policy.evaluate(CommandClass::Motion, "supervised_autonomy") {
            TacticalPolicyOutcome::RequireApproval(_) => {}
            other => panic!("expected RequireApproval, got {other:?}"),
        }
    }

    #[test]
    fn tactical_policy_serde_roundtrip() {
        let policy = TacticalAgentPolicy {
            allowed_command_classes: vec!["motion".into(), "weapon".into()],
            allowed_autonomy_modes: vec!["supervised_autonomy".into()],
            requires_human_approval: true,
            advisory_only: false,
        };
        let json = serde_json::to_string(&policy).unwrap();
        let back: TacticalAgentPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(policy, back);
    }
}
