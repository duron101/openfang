//! Workflow trigger manager — the brain's decision layer.
//!
//! The slow loop (brain) does not blindly run one pipeline. Instead, each cycle
//! it asks the [`WorkflowTriggerManager`] which tactical workflows should fire,
//! based on:
//! - **periodic** cadence (e.g. `Patrol` every N seconds),
//! - **events** derived from the [`SituationAssessment`] (e.g. `NewContact`,
//!   `ThreatEmitter`, `IncomingMunition`, `LinkLost`),
//! - **operator commands** (e.g. `electronic_attack`, `sead`).
//!
//! Each trigger declares a [`WorkflowScope`]: `own` workflows run on this
//! instance's own platform (single-instance default); `formation` workflows only
//! fire when this node is the formation [`FleetRole::Lead`].
//!
//! The manager is deterministic and side-effect free (it only *decides*); the
//! caller maps a fired own-scope workflow to a [`CcaRole`] via
//! [`workflow_to_role`] and fans the resulting posture out to the cerebellum
//! lanes.

use openfang_types::cognition::SituationAssessment;
use openfang_types::config::{
    FleetRole, WorkflowConfig, WorkflowScope, WorkflowTriggerConfig, WorkflowTriggerKind,
};
use openfang_types::platform::CcaRole;
use serde::Serialize;

/// Threshold above which a threat's score is treated as an active-emitter threat
/// (fires `ThreatEmitter` → electronic-attack workflows).
const THREAT_EMITTER_SCORE: f64 = 0.7;
/// Distance (m) under which an approaching threat is treated as an incoming
/// munition / imminent threat (fires `IncomingMunition` → EW protection).
const INCOMING_RANGE_M: f64 = 6000.0;
/// Estimated hit probability above which an engage opportunity is treated as a
/// high-value contact (fires `HighValueContact` → drives the designate phase of
/// recon-to-strike plays).
const HIGH_VALUE_PHIT: f64 = 0.6;

/// A workflow the manager decided to fire this cycle.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FiredWorkflow {
    pub workflow: String,
    pub scope: WorkflowScope,
    pub trigger: WorkflowTriggerKind,
    /// Human-readable reason (event name, "periodic", or command).
    pub reason: String,
}

/// Derive the set of situation event names raised by an assessment. These are
/// matched against [`WorkflowTriggerKind::Event`] triggers.
pub fn derive_events(assessment: &SituationAssessment) -> Vec<String> {
    let mut events = Vec::new();
    if !assessment.threats.is_empty() {
        events.push("NewContact".to_string());
    }
    let max_score = assessment
        .threats
        .iter()
        .map(|t| t.threat_score)
        .fold(0.0_f64, f64::max);
    if max_score >= THREAT_EMITTER_SCORE {
        events.push("ThreatEmitter".to_string());
    }
    let incoming = assessment
        .threats
        .iter()
        .any(|t| t.distance_m <= INCOMING_RANGE_M && t.closing_rate_ms < 0.0);
    if incoming {
        events.push("IncomingMunition".to_string());
    }
    if link_lost(&assessment.own_force.link_status) {
        events.push("LinkLost".to_string());
    }
    // A high-confidence engage opportunity is a high-value contact worth
    // designating — this drives the designate → strike phase of recon-to-strike
    // plays without implying an immediate weapon release.
    let high_value = assessment
        .opportunities
        .iter()
        .any(|o| o.estimated_p_hit >= HIGH_VALUE_PHIT);
    if high_value {
        events.push("HighValueContact".to_string());
    }
    events
}

/// Whether a link-status string indicates loss / degradation.
fn link_lost(status: &str) -> bool {
    let s = status.trim().to_ascii_lowercase();
    matches!(s.as_str(), "none" | "no_link" | "no link" | "offline")
        || s.contains("lost")
        || s.contains("down")
        || s.contains("degraded")
        || s.contains("loss")
}

/// Map a fired tactical workflow to the own-platform [`CcaRole`] the brain should
/// adopt. Returns `None` for workflows that do not imply an own-platform posture
/// (e.g. pure fleet-coordination workflows on a lead).
pub fn workflow_to_role(workflow: &str) -> Option<CcaRole> {
    match workflow {
        "Patrol" => Some(CcaRole::Patrol),
        "Track" => Some(CcaRole::Surveil),
        "Engage" => Some(CcaRole::Striker),
        "Survive" => Some(CcaRole::Recon),
        "ElectronicAttack" | "SEAD" => Some(CcaRole::EwJamming),
        "EwProtection" => Some(CcaRole::EwProtection),
        "Decoy" => Some(CcaRole::Decoy),
        _ => None,
    }
}

/// Stateful trigger manager owned by the slow-loop task.
pub struct WorkflowTriggerManager {
    triggers: Vec<WorkflowTriggerConfig>,
    fleet_role: FleetRole,
    /// Last fire time (seconds) per periodic trigger, indexed parallel to
    /// `triggers`. `None` = never fired.
    last_fired: Vec<Option<f64>>,
}

impl WorkflowTriggerManager {
    /// Build from a [`WorkflowConfig`] and this node's [`FleetRole`]. A disabled
    /// config yields a manager that never fires anything.
    pub fn new(config: &WorkflowConfig, fleet_role: FleetRole) -> Self {
        let triggers = if config.enabled {
            config
                .triggers
                .iter()
                .filter(|t| t.enabled && !t.workflow.is_empty())
                .cloned()
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let last_fired = vec![None; triggers.len()];
        Self {
            triggers,
            fleet_role,
            last_fired,
        }
    }

    /// Whether any triggers are configured (i.e. workflow orchestration is live).
    pub fn is_active(&self) -> bool {
        !self.triggers.is_empty()
    }

    /// Evaluate all triggers for this cycle. `now` is the snapshot timestamp in
    /// seconds; `command` is an optional pending operator command name.
    pub fn evaluate(
        &mut self,
        now: f64,
        assessment: &SituationAssessment,
        command: Option<&str>,
    ) -> Vec<FiredWorkflow> {
        let events = derive_events(assessment);
        let mut fired = Vec::new();
        for idx in 0..self.triggers.len() {
            let trig = &self.triggers[idx];
            // Scope gate: formation workflows require this node to be the lead.
            if !trig.scope.runnable_by(self.fleet_role) {
                continue;
            }
            let hit = match trig.trigger {
                WorkflowTriggerKind::Periodic => {
                    let due = match self.last_fired[idx] {
                        None => true,
                        Some(last) => now - last >= trig.interval_secs.max(0.0),
                    };
                    if due {
                        self.last_fired[idx] = Some(now);
                    }
                    due.then(|| "periodic".to_string())
                }
                WorkflowTriggerKind::Event => trig
                    .event
                    .as_deref()
                    .filter(|want| events.iter().any(|e| e.eq_ignore_ascii_case(want)))
                    .map(|e| format!("event:{e}")),
                WorkflowTriggerKind::Command => match (command, trig.command.as_deref()) {
                    (Some(cmd), Some(want)) if cmd.eq_ignore_ascii_case(want) => {
                        Some(format!("command:{cmd}"))
                    }
                    _ => None,
                },
            };
            if let Some(reason) = hit {
                fired.push(FiredWorkflow {
                    workflow: trig.workflow.clone(),
                    scope: trig.scope,
                    trigger: trig.trigger,
                    reason,
                });
            }
        }
        fired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::cognition::{OwnForceStatus, ThreatTrack};

    fn assessment(threats: Vec<ThreatTrack>, link: &str) -> SituationAssessment {
        SituationAssessment {
            timestamp: 0.0,
            threats,
            opportunities: Vec::new(),
            own_force: OwnForceStatus {
                total_platforms: 1,
                average_damage: 0.0,
                average_fuel_pct: 1.0,
                link_status: link.to_string(),
            },
            summary: String::new(),
        }
    }

    fn threat(score: f64, distance_m: f64, closing: f64) -> ThreatTrack {
        ThreatTrack {
            track_id: "trk-1".into(),
            platform_type: "aircraft".into(),
            distance_m,
            closing_rate_ms: closing,
            threat_score: score,
        }
    }

    fn cfg(triggers: Vec<WorkflowTriggerConfig>) -> WorkflowConfig {
        WorkflowConfig {
            enabled: true,
            definitions_path: None,
            triggers,
        }
    }

    fn trig(
        workflow: &str,
        scope: WorkflowScope,
        kind: WorkflowTriggerKind,
        event: Option<&str>,
        command: Option<&str>,
        interval: f64,
    ) -> WorkflowTriggerConfig {
        WorkflowTriggerConfig {
            workflow: workflow.into(),
            scope,
            trigger: kind,
            interval_secs: interval,
            event: event.map(|s| s.to_string()),
            command: command.map(|s| s.to_string()),
            enabled: true,
        }
    }

    #[test]
    fn derive_events_maps_threats_to_event_names() {
        let a = assessment(vec![threat(0.9, 4000.0, -100.0)], "nominal");
        let ev = derive_events(&a);
        assert!(ev.contains(&"NewContact".to_string()));
        assert!(ev.contains(&"ThreatEmitter".to_string()));
        assert!(ev.contains(&"IncomingMunition".to_string()));
        assert!(!ev.contains(&"LinkLost".to_string()));
    }

    #[test]
    fn link_loss_event_detected() {
        let a = assessment(vec![], "link lost");
        assert!(derive_events(&a).contains(&"LinkLost".to_string()));
    }

    #[test]
    fn periodic_trigger_respects_interval() {
        let mut mgr = WorkflowTriggerManager::new(
            &cfg(vec![trig(
                "Patrol",
                WorkflowScope::Own,
                WorkflowTriggerKind::Periodic,
                None,
                None,
                30.0,
            )]),
            FleetRole::Standalone,
        );
        let a = assessment(vec![], "nominal");
        // First eval fires immediately.
        assert_eq!(mgr.evaluate(0.0, &a, None).len(), 1);
        // Too soon — no fire.
        assert_eq!(mgr.evaluate(10.0, &a, None).len(), 0);
        // After the interval — fires again.
        assert_eq!(mgr.evaluate(31.0, &a, None).len(), 1);
    }

    #[test]
    fn event_trigger_fires_on_matching_event() {
        let mut mgr = WorkflowTriggerManager::new(
            &cfg(vec![trig(
                "ElectronicAttack",
                WorkflowScope::Own,
                WorkflowTriggerKind::Event,
                Some("ThreatEmitter"),
                None,
                0.0,
            )]),
            FleetRole::Standalone,
        );
        let calm = assessment(vec![threat(0.3, 20000.0, 50.0)], "nominal");
        assert_eq!(mgr.evaluate(0.0, &calm, None).len(), 0);
        let hot = assessment(vec![threat(0.95, 9000.0, -10.0)], "nominal");
        let fired = mgr.evaluate(1.0, &hot, None);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].workflow, "ElectronicAttack");
    }

    #[test]
    fn formation_scope_inert_for_non_lead() {
        let triggers = vec![trig(
            "SEAD",
            WorkflowScope::Formation,
            WorkflowTriggerKind::Command,
            None,
            Some("sead"),
            0.0,
        )];
        let a = assessment(vec![], "nominal");
        // Standalone: formation workflow never fires.
        let mut standalone =
            WorkflowTriggerManager::new(&cfg(triggers.clone()), FleetRole::Standalone);
        assert_eq!(standalone.evaluate(0.0, &a, Some("sead")).len(), 0);
        // Lead: it does.
        let mut lead = WorkflowTriggerManager::new(&cfg(triggers), FleetRole::Lead);
        assert_eq!(lead.evaluate(0.0, &a, Some("sead")).len(), 1);
    }

    #[test]
    fn disabled_config_never_fires() {
        let mut mgr = WorkflowTriggerManager::new(
            &WorkflowConfig {
                enabled: false,
                definitions_path: None,
                triggers: vec![trig(
                    "Patrol",
                    WorkflowScope::Own,
                    WorkflowTriggerKind::Periodic,
                    None,
                    None,
                    0.0,
                )],
            },
            FleetRole::Standalone,
        );
        assert!(!mgr.is_active());
        assert_eq!(
            mgr.evaluate(0.0, &assessment(vec![], "nominal"), None)
                .len(),
            0
        );
    }

    #[test]
    fn workflow_role_mapping() {
        assert_eq!(
            workflow_to_role("ElectronicAttack"),
            Some(CcaRole::EwJamming)
        );
        assert_eq!(
            workflow_to_role("EwProtection"),
            Some(CcaRole::EwProtection)
        );
        assert_eq!(workflow_to_role("Engage"), Some(CcaRole::Striker));
        assert_eq!(workflow_to_role("Patrol"), Some(CcaRole::Patrol));
        assert_eq!(workflow_to_role("Unknown"), None);
    }
}
