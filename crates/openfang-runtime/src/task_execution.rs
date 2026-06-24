//! Lightweight task execution status for slow-loop workflow tasks.

use std::collections::BTreeMap;

use serde::Serialize;

use openfang_types::tactical::{CandidateIntent, IntentSource};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskExecutionStatus {
    Proposed,
    Pending,
    Dispatched,
    Rejected,
    Expired,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskExecutionRecord {
    pub task_id: String,
    pub status: TaskExecutionStatus,
    pub intent_id: String,
    pub updated_at: f64,
    pub detail: String,
}

#[derive(Debug, Default, Clone)]
pub struct TaskExecutionRegistry {
    records: BTreeMap<String, TaskExecutionRecord>,
}

impl TaskExecutionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_intent(
        &mut self,
        intent: &CandidateIntent,
        status: TaskExecutionStatus,
        updated_at: f64,
        detail: impl Into<String>,
    ) {
        let Some(task_id) = task_id_for_intent(intent) else {
            return;
        };
        self.records.insert(
            task_id.clone(),
            TaskExecutionRecord {
                task_id,
                status,
                intent_id: intent.id.clone(),
                updated_at,
                detail: detail.into(),
            },
        );
    }

    pub fn list(&self) -> Vec<TaskExecutionRecord> {
        self.records.values().cloned().collect()
    }
}

fn task_id_for_intent(intent: &CandidateIntent) -> Option<String> {
    match &intent.source {
        IntentSource::Workflow { workflow_id } => Some(workflow_id.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::platform::PlatformCommand;
    use openfang_types::tactical::CommandPriority;

    #[test]
    fn records_only_workflow_intents_by_task_id() {
        let mut registry = TaskExecutionRegistry::new();
        let intent = CandidateIntent::new(
            PlatformCommand::SensorOn {
                platform_id: "self".into(),
                sensor_id: "radar".into(),
            },
            CommandPriority::Normal,
            IntentSource::Workflow {
                workflow_id: "patrol:default".into(),
            },
            1.0,
            "patrol",
        );

        registry.record_intent(&intent, TaskExecutionStatus::Dispatched, 2.0, "accepted");

        let records = registry.list();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].task_id, "patrol:default");
        assert_eq!(records[0].status, TaskExecutionStatus::Dispatched);
    }

    #[test]
    fn ignores_non_workflow_intents() {
        let mut registry = TaskExecutionRegistry::new();
        let intent = CandidateIntent::new(
            PlatformCommand::CommOn {
                platform_id: "self".into(),
            },
            CommandPriority::Normal,
            IntentSource::Dcc {
                rule_name: "comm".into(),
            },
            1.0,
            "dcc",
        );

        registry.record_intent(&intent, TaskExecutionStatus::Dispatched, 2.0, "accepted");

        assert!(registry.list().is_empty());
    }
}
