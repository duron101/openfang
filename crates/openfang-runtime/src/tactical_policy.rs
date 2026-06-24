//! Per-agent tactical-policy enforcement (LLM ↔ platform boundary).
//!
//! [`TacticalAgentPolicy`] declares the envelope of a tactical-brain persona:
//! which [`CommandClass`]es it may emit, which autonomy modes it may operate
//! under, whether its tool calls are advisory-only, and whether every emitted
//! intent must be funneled to human approval regardless of the active profile.
//!
//! This module is the kernel-runtime wrapper that turns a `(policy, intent,
//! active_mode)` tuple into a [`TacticalGuardResult`] the dispatcher can act on
//! deterministically — and that writes one audit entry for every decision.
//!
//! Iron Law: this is a *soft* layer at the boundary. Everything that survives
//! it still flows through the standard pipeline
//! (`ActionComposer → CommandGate → WeaponEngagementManager`), so SPGS/ROE/
//! capability/quorum checks remain authoritative.

use std::sync::Arc;

use openfang_types::tactical::{
    CandidateIntent, CommandClass, IntentSource, TacticalAgentPolicy, TacticalPolicyOutcome,
};

use crate::audit::{AuditAction, AuditLog};

/// Default identifier for the active autonomy mode when no profile system is
/// wired (legacy callers, tests). Treated as "permissive — no mode-level
/// allowlist applies".
pub const DEFAULT_AUTONOMY_MODE: &str = "default";

/// Result of running a tool-emitted intent through the tactical-policy guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TacticalGuardResult {
    /// Intent may continue into the pipeline.
    Allow,
    /// Intent dropped at the boundary. Caller should audit the rejection and
    /// surface the reason to the LLM.
    Reject(String),
    /// Intent dropped but recorded as advisory (no actuation). Caller should
    /// audit as such; LLM should treat as "suggestion logged, no action".
    Advisory(String),
    /// Intent must be routed to the human-approval queue regardless of the
    /// active autonomy profile.
    RequireApproval(String),
}

impl TacticalGuardResult {
    /// Whether the dispatcher should continue routing the intent into the
    /// composer/gate pipeline.
    pub fn should_dispatch(&self) -> bool {
        matches!(self, Self::Allow)
    }

    /// Human-readable explanation suitable for surfacing to the LLM tool
    /// response. Empty when `Allow`.
    pub fn reason(&self) -> &str {
        match self {
            Self::Allow => "",
            Self::Reject(r) | Self::Advisory(r) | Self::RequireApproval(r) => r,
        }
    }
}

/// Map a [`TacticalPolicyOutcome`] to a [`TacticalGuardResult`]. Trivial today
/// but kept as a seam in case the runtime adds richer decisions later.
fn map_outcome(outcome: TacticalPolicyOutcome) -> TacticalGuardResult {
    match outcome {
        TacticalPolicyOutcome::Allow => TacticalGuardResult::Allow,
        TacticalPolicyOutcome::Deny(reason) => TacticalGuardResult::Reject(reason),
        TacticalPolicyOutcome::Advisory(reason) => TacticalGuardResult::Advisory(reason),
        TacticalPolicyOutcome::RequireApproval(reason) => {
            TacticalGuardResult::RequireApproval(reason)
        }
    }
}

/// Evaluate `intent`'s [`CommandClass`] against `policy` for `autonomy_mode_id`
/// and write a single audit entry describing the decision. Returns the guard
/// result the caller should act on.
pub fn enforce(
    audit: &Arc<AuditLog>,
    intent: &CandidateIntent,
    policy: &TacticalAgentPolicy,
    autonomy_mode_id: &str,
) -> TacticalGuardResult {
    let class = intent.class();
    let result = map_outcome(policy.evaluate(class, autonomy_mode_id));
    let actor = intent.source.label();
    let detail = format!(
        "{} class={} mode={} intent={}",
        actor,
        class.as_str(),
        autonomy_mode_id,
        intent.id
    );
    let outcome = match &result {
        TacticalGuardResult::Allow => "allow".to_string(),
        TacticalGuardResult::Reject(reason) => format!("reject:{reason}"),
        TacticalGuardResult::Advisory(reason) => format!("advisory:{reason}"),
        TacticalGuardResult::RequireApproval(reason) => format!("require_approval:{reason}"),
    };
    audit.record(actor, AuditAction::CapabilityCheck, detail, &outcome);
    result
}

/// Build the audit-friendly source for a tool-emitted intent. When
/// `agent_id` is empty/unknown, falls back to a stable `tool` label so audit
/// provenance is never ambiguous.
pub fn intent_source_for_agent(agent_id: Option<&str>) -> IntentSource {
    let id = agent_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("tool")
        .to_string();
    IntentSource::Llm { agent_id: id }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::platform::PlatformCommand;
    use openfang_types::tactical::CommandPriority;

    fn motion_intent() -> CandidateIntent {
        CandidateIntent::new(
            PlatformCommand::SetHeading {
                platform_id: "usv-01".into(),
                heading_deg: 45.0,
                speed_ms: None,
                turn_direction: None,
            },
            CommandPriority::Normal,
            IntentSource::Llm {
                agent_id: "na".into(),
            },
            0.0,
            "test",
        )
    }

    fn fire_intent() -> CandidateIntent {
        CandidateIntent::new(
            PlatformCommand::FireAtTarget {
                platform_id: "usv-01".into(),
                weapon_id: "w1".into(),
                track_id: "trk-1".into(),
            },
            CommandPriority::Critical,
            IntentSource::Llm {
                agent_id: "fca".into(),
            },
            0.0,
            "test",
        )
    }

    #[test]
    fn empty_policy_allows() {
        let audit = Arc::new(AuditLog::new());
        let policy = TacticalAgentPolicy::default();
        assert_eq!(
            enforce(&audit, &motion_intent(), &policy, DEFAULT_AUTONOMY_MODE),
            TacticalGuardResult::Allow,
        );
        assert_eq!(audit.len(), 1);
    }

    #[test]
    fn navigation_persona_cannot_fire_weapons() {
        let audit = Arc::new(AuditLog::new());
        let policy = TacticalAgentPolicy {
            allowed_command_classes: vec!["motion".into(), "sensor".into()],
            ..Default::default()
        };
        let result = enforce(&audit, &fire_intent(), &policy, DEFAULT_AUTONOMY_MODE);
        match result {
            TacticalGuardResult::Reject(reason) => assert!(reason.contains("weapon")),
            other => panic!("expected reject, got {other:?}"),
        }
        assert_eq!(audit.len(), 1);
    }

    #[test]
    fn advisory_only_persona_does_not_actuate() {
        let audit = Arc::new(AuditLog::new());
        let policy = TacticalAgentPolicy {
            advisory_only: true,
            ..Default::default()
        };
        let result = enforce(&audit, &motion_intent(), &policy, DEFAULT_AUTONOMY_MODE);
        assert!(matches!(result, TacticalGuardResult::Advisory(_)));
        assert!(!result.should_dispatch());
    }

    #[test]
    fn require_human_approval_short_circuits() {
        let audit = Arc::new(AuditLog::new());
        let policy = TacticalAgentPolicy {
            requires_human_approval: true,
            ..Default::default()
        };
        let result = enforce(&audit, &motion_intent(), &policy, DEFAULT_AUTONOMY_MODE);
        assert!(matches!(result, TacticalGuardResult::RequireApproval(_)));
    }

    #[test]
    fn mode_allowlist_blocks_off_profile_actuation() {
        let audit = Arc::new(AuditLog::new());
        let policy = TacticalAgentPolicy {
            allowed_autonomy_modes: vec!["defensive_autonomy".into()],
            ..Default::default()
        };
        let result = enforce(
            &audit,
            &motion_intent(),
            &policy,
            "weapons_free_constrained",
        );
        assert!(matches!(result, TacticalGuardResult::Reject(_)));
    }

    #[test]
    fn intent_source_falls_back_to_tool_when_unknown() {
        match intent_source_for_agent(None) {
            IntentSource::Llm { agent_id } => assert_eq!(agent_id, "tool"),
            other => panic!("expected Llm source, got {other:?}"),
        }
        match intent_source_for_agent(Some("  ")) {
            IntentSource::Llm { agent_id } => assert_eq!(agent_id, "tool"),
            other => panic!("expected Llm source, got {other:?}"),
        }
        match intent_source_for_agent(Some("na")) {
            IntentSource::Llm { agent_id } => assert_eq!(agent_id, "na"),
            other => panic!("expected Llm source, got {other:?}"),
        }
    }
}
