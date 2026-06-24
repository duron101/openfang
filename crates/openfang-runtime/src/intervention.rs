//! Configurable intervention rule engine shared by slow-loop checkpoints and gates.

use std::sync::Arc;

use openfang_types::config::{InterventionConfig, InterventionMode, InterventionRule};
use openfang_types::tactical::{CommandClass, IntentSource};
use openfang_types::umaa::WeaponReleaseLevel;

use crate::mission_approval::MissionApprovalRegistry;
use crate::target_authorization::TargetAuthorizationRegistry;

#[derive(Debug, Clone)]
pub struct InterventionRequest<'a> {
    pub stage: &'a str,
    pub platform_id: &'a str,
    pub command_class: Option<CommandClass>,
    pub source: Option<&'a IntentSource>,
    pub track_id: Option<&'a str>,
    pub intent_id: &'a str,
    /// Live weapon-release authority for fast-loop weapon checks. Mission-level
    /// planning checkpoints leave this unset.
    pub weapon_release_authority: Option<WeaponReleaseLevel>,
    /// Content fingerprint of the plan under evaluation. When present, the
    /// `Confirm`/`Quorum` modes check it against the persistent
    /// [`MissionApprovalRegistry`] so an approved plan is released and a changed
    /// plan requires fresh approval.
    pub plan_fingerprint: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterventionDecision {
    Pass,
    Deny(String),
    Pending { approval_id: String, quorum: u32 },
    RoeDriven,
}

pub struct InterventionGate {
    config: InterventionConfig,
    targets: Arc<TargetAuthorizationRegistry>,
    mission_approvals: Arc<MissionApprovalRegistry>,
}

impl InterventionGate {
    pub fn new(
        config: InterventionConfig,
        targets: Arc<TargetAuthorizationRegistry>,
        mission_approvals: Arc<MissionApprovalRegistry>,
    ) -> Self {
        Self {
            config,
            targets,
            mission_approvals,
        }
    }

    pub fn target_registry(&self) -> Arc<TargetAuthorizationRegistry> {
        Arc::clone(&self.targets)
    }

    pub fn mission_approvals(&self) -> Arc<MissionApprovalRegistry> {
        Arc::clone(&self.mission_approvals)
    }

    pub fn evaluate(&self, req: InterventionRequest<'_>) -> InterventionDecision {
        let mode = self
            .config
            .rules
            .iter()
            .find(|rule| matches_rule(rule, &req))
            .map(|rule| rule.mode)
            .unwrap_or(self.config.default_mode);

        match mode {
            InterventionMode::Auto => InterventionDecision::Pass,
            InterventionMode::Deny => {
                InterventionDecision::Deny(format!("intervention denied at {}", req.stage))
            }
            InterventionMode::Confirm => self.resolve_plan_approval(&req, 1),
            InterventionMode::Quorum => {
                let quorum = self
                    .config
                    .rules
                    .iter()
                    .find(|rule| matches_rule(rule, &req))
                    .map(|rule| rule.quorum.max(1))
                    .unwrap_or(1);
                self.resolve_plan_approval(&req, quorum)
            }
            InterventionMode::AuthorizedTarget => {
                if let Some(track_id) = req.track_id {
                    if self.targets.is_authorized_for_roe(
                        req.platform_id,
                        track_id,
                        req.weapon_release_authority,
                    ) {
                        return InterventionDecision::Pass;
                    }
                }
                InterventionDecision::Pending {
                    approval_id: format!("target:{}:{}", req.platform_id, req.intent_id),
                    quorum: 1,
                }
            }
            InterventionMode::RoeDriven => InterventionDecision::RoeDriven,
        }
    }

    /// Resolve a `Confirm`/`Quorum` checkpoint against the persistent approval
    /// registry. Mission plans use their content fingerprint as the approval id;
    /// non-mission stages use a stable `intervention:*` id so fast-loop
    /// Confirm/Quorum rules can also be approved through the API instead of
    /// deadlocking forever.
    fn resolve_plan_approval(
        &self,
        req: &InterventionRequest<'_>,
        quorum: u32,
    ) -> InterventionDecision {
        let approval_id = req
            .plan_fingerprint
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("intervention:{}:{}:{}", req.stage, quorum, req.intent_id));
        if self.mission_approvals.is_approved(&approval_id, quorum) {
            InterventionDecision::Pass
        } else {
            InterventionDecision::Pending {
                approval_id,
                quorum,
            }
        }
    }
}

fn matches_rule(rule: &InterventionRule, req: &InterventionRequest<'_>) -> bool {
    matches_strs(&rule.stage, req.stage)
        && matches_strs(&rule.platform_ids, req.platform_id)
        && matches_class(&rule.command_classes, req.command_class)
        && matches_source(&rule.sources, req.source)
}

fn matches_strs(values: &[String], needle: &str) -> bool {
    values.is_empty() || values.iter().any(|value| value == needle)
}

fn matches_class(values: &[String], class: Option<CommandClass>) -> bool {
    values.is_empty()
        || class
            .map(|class| {
                values
                    .iter()
                    .any(|value| value == command_class_name(class))
            })
            .unwrap_or(false)
}

fn matches_source(values: &[String], source: Option<&IntentSource>) -> bool {
    values.is_empty()
        || source
            .map(|source| values.iter().any(|value| value == source_kind(source)))
            .unwrap_or(false)
}

pub fn command_class_name(class: CommandClass) -> &'static str {
    match class {
        CommandClass::Motion => "motion",
        CommandClass::Sensor => "sensor",
        CommandClass::Weapon => "weapon",
        CommandClass::ElectronicWarfare => "electronic_warfare",
        CommandClass::Comm => "comm",
        CommandClass::Command => "command",
        CommandClass::Uav => "uav",
        CommandClass::Formation => "formation",
        CommandClass::Aux => "aux",
    }
}

pub fn source_kind(source: &IntentSource) -> &'static str {
    match source {
        IntentSource::Llm { .. } => "llm",
        IntentSource::Dcc { .. } => "dcc",
        IntentSource::Operator { .. } => "operator",
        IntentSource::Workflow { .. } => "workflow",
        IntentSource::External { .. } => "external",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::config::{InterventionConfig, InterventionMode};
    use openfang_types::umaa::WeaponReleaseLevel;

    fn authorized_target_request(roe: WeaponReleaseLevel) -> InterventionRequest<'static> {
        InterventionRequest {
            stage: "weapon_release",
            platform_id: "usv-01",
            command_class: Some(CommandClass::Weapon),
            source: None,
            track_id: Some("trk-1"),
            intent_id: "intent-1",
            weapon_release_authority: Some(roe),
            plan_fingerprint: None,
        }
    }

    #[test]
    fn llm_authorized_target_is_scoped_to_weapons_free() {
        let targets = Arc::new(TargetAuthorizationRegistry::new());
        let approvals = Arc::new(MissionApprovalRegistry::new());
        let gate = InterventionGate::new(
            InterventionConfig {
                default_mode: InterventionMode::AuthorizedTarget,
                rules: vec![],
            },
            Arc::clone(&targets),
            approvals,
        );
        targets.authorize("usv-01", "trk-1", "llm:planner", 1.0);

        assert_eq!(
            gate.evaluate(authorized_target_request(WeaponReleaseLevel::WeaponsFree)),
            InterventionDecision::Pass
        );
        assert!(matches!(
            gate.evaluate(authorized_target_request(WeaponReleaseLevel::WeaponsTight)),
            InterventionDecision::Pending { .. }
        ));
    }
}
