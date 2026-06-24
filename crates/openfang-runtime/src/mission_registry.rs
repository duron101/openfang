//! Pending mission registry — the `Confirm`-mode backing store for compiled
//! [`MissionDsl`]s awaiting operator approval.
//!
//! Mirrors [`crate::planning::LabelResolutionRegistry`]: the slow loop submits a
//! compiled DSL (held, not dispatched), the API lists/renders it, and the
//! operator confirms (→ dispatch on the next cycle) or dismisses it.

use std::sync::Mutex;

use openfang_types::mission_dsl::MissionDsl;
use serde::{Deserialize, Serialize};

/// Lifecycle state of a pending DSL mission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingMissionState {
    /// Awaiting operator confirm/dismiss.
    Pending,
    /// Operator confirmed; ready to dispatch (consumed by the loop).
    Confirmed,
    /// Operator dismissed; will not dispatch.
    Dismissed,
}

/// A compiled mission held for approval, with audit/rendering metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingMission {
    pub id: String,
    /// Originating commander-intent id (when submitted from the slow loop).
    pub intent_id: Option<String>,
    pub mission: MissionDsl,
    /// Operator-facing rendered text (promt.md `MISSION{...}` style).
    pub rendered: String,
    pub created_at: f64,
    pub state: PendingMissionState,
}

/// Thread-safe store of pending DSL missions.
#[derive(Default)]
pub struct PendingMissionRegistry {
    missions: Mutex<Vec<PendingMission>>,
}

impl PendingMissionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Submit a compiled mission for approval. Idempotent on `mission.id`: an
    /// existing pending entry with the same id is returned unchanged.
    pub fn submit(
        &self,
        mission: MissionDsl,
        intent_id: Option<String>,
        created_at: f64,
    ) -> PendingMission {
        let id = mission.id.clone();
        let mut missions = self.missions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = missions
            .iter()
            .find(|m| m.id == id && m.state == PendingMissionState::Pending)
            .cloned()
        {
            return existing;
        }
        let entry = PendingMission {
            id,
            intent_id,
            rendered: mission.to_string(),
            mission,
            created_at,
            state: PendingMissionState::Pending,
        };
        missions.push(entry.clone());
        entry
    }

    /// All missions still awaiting a decision.
    pub fn list_pending(&self) -> Vec<PendingMission> {
        self.missions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|m| m.state == PendingMissionState::Pending)
            .cloned()
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<PendingMission> {
        self.missions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|m| m.id == id)
            .cloned()
    }

    pub fn has_pending_for_intent(&self, intent_id: &str) -> bool {
        self.missions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|m| {
                m.state == PendingMissionState::Pending && m.intent_id.as_deref() == Some(intent_id)
            })
    }

    /// Mark a pending mission confirmed and return it (for dispatch). Returns
    /// `None` if missing or not currently pending.
    pub fn confirm(&self, id: &str) -> Option<PendingMission> {
        self.transition(id, PendingMissionState::Confirmed)
    }

    /// Mark a pending mission dismissed.
    pub fn dismiss(&self, id: &str) -> Option<PendingMission> {
        self.transition(id, PendingMissionState::Dismissed)
    }

    fn transition(&self, id: &str, state: PendingMissionState) -> Option<PendingMission> {
        let mut missions = self.missions.lock().unwrap_or_else(|e| e.into_inner());
        let entry = missions
            .iter_mut()
            .find(|m| m.id == id && m.state == PendingMissionState::Pending)?;
        entry.state = state;
        Some(entry.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::mission_dsl::{DslObjective, MissionKind};

    fn mission(id: &str) -> MissionDsl {
        MissionDsl {
            id: id.into(),
            intent_text: "engage".into(),
            kind: MissionKind::Engage,
            time_window: None,
            objectives: vec![DslObjective {
                id: "obj".into(),
                description: "neutralize".into(),
                feedback_var: Some("track:t:engaged".into()),
                priority: 100,
            }],
            constraints: vec![],
            plays: vec![],
            functions: vec![],
            intervention_points: vec![],
            explanation_trace: "M→".into(),
            confidence: 0.8,
            provenance: "test".into(),
        }
    }

    #[test]
    fn submit_lists_and_is_idempotent() {
        let reg = PendingMissionRegistry::new();
        reg.submit(mission("m1"), Some("i1".into()), 1.0);
        reg.submit(mission("m1"), Some("i1".into()), 2.0);
        assert_eq!(reg.list_pending().len(), 1);
        assert!(reg.has_pending_for_intent("i1"));
    }

    #[test]
    fn confirm_removes_from_pending_and_returns_entry() {
        let reg = PendingMissionRegistry::new();
        reg.submit(mission("m1"), None, 1.0);
        let confirmed = reg.confirm("m1").expect("confirmed");
        assert_eq!(confirmed.state, PendingMissionState::Confirmed);
        assert!(reg.list_pending().is_empty());
        assert!(reg.confirm("m1").is_none(), "already transitioned");
    }

    #[test]
    fn dismiss_marks_dismissed() {
        let reg = PendingMissionRegistry::new();
        reg.submit(mission("m1"), None, 1.0);
        assert!(reg.dismiss("m1").is_some());
        assert!(reg.list_pending().is_empty());
        assert_eq!(reg.get("m1").unwrap().state, PendingMissionState::Dismissed);
    }
}
