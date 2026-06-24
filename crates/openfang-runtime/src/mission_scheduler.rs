//! MissionScheduler — dependency-graph execution manager for compiled DSL.
//!
//! The scheduler is the "communication manager" layer for NLP-generated
//! symbolic plans: it releases only the current dependency frontier, then lets
//! the existing [`FunctionExecutor`] lower those functions into gate-bound
//! intents. It is deliberately deterministic and snapshot-driven.

use std::collections::{HashMap, HashSet};

use openfang_types::mission_dsl::{FunctionCall, MissionDsl, PlatformCommandSpec};
use openfang_types::platform::{PlatformState, WorldEvent, WorldSnapshot};

use crate::function_executor::{ExecutionPlan, FunctionExecutor};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    Active,
    Done,
    Failed,
}

#[derive(Debug, Clone)]
pub struct MissionScheduler {
    mission: MissionDsl,
    status: HashMap<String, TaskStatus>,
    active_since: HashMap<String, f64>,
}

impl MissionScheduler {
    pub fn new(mission: MissionDsl) -> Self {
        let status = mission
            .functions
            .iter()
            .map(|function| (function.task_ref().to_string(), TaskStatus::Pending))
            .collect();
        Self {
            mission,
            status,
            active_since: HashMap::new(),
        }
    }

    pub fn mission(&self) -> &MissionDsl {
        &self.mission
    }

    pub fn status(&self, task_id: &str) -> Option<TaskStatus> {
        self.status.get(task_id).copied()
    }

    pub fn is_done(&self) -> bool {
        self.status
            .values()
            .all(|status| *status == TaskStatus::Done)
    }

    /// Advance the DAG once and lower only the active frontier into intents.
    pub fn tick(
        &mut self,
        snapshot: &WorldSnapshot,
        own_state: &PlatformState,
        now_secs: f64,
    ) -> ExecutionPlan {
        self.complete_active_tasks(snapshot, own_state);
        self.release_ready_tasks(snapshot, now_secs);

        let active_functions: Vec<FunctionCall> = self
            .mission
            .functions
            .iter()
            .filter(|function| self.status(function.task_ref()) == Some(TaskStatus::Active))
            .cloned()
            .collect();

        if active_functions.is_empty() {
            return ExecutionPlan {
                mission_id: self.mission.id.clone(),
                ..Default::default()
            };
        }

        let active_mission = MissionDsl {
            functions: active_functions,
            ..self.mission.clone()
        };
        FunctionExecutor::new().execute(&active_mission, snapshot, own_state, now_secs)
    }

    fn complete_active_tasks(&mut self, snapshot: &WorldSnapshot, own_state: &PlatformState) {
        let active: Vec<String> = self
            .status
            .iter()
            .filter_map(|(task, status)| {
                if *status == TaskStatus::Active {
                    Some(task.clone())
                } else {
                    None
                }
            })
            .collect();

        for task_id in active {
            let Some(function) = self
                .mission
                .functions
                .iter()
                .find(|function| function.task_ref() == task_id)
            else {
                continue;
            };
            if criteria_met(function, snapshot, own_state) {
                self.status.insert(task_id.clone(), TaskStatus::Done);
                self.active_since.remove(&task_id);
            }
        }
    }

    fn release_ready_tasks(&mut self, snapshot: &WorldSnapshot, now_secs: f64) {
        let pending: Vec<String> = self
            .status
            .iter()
            .filter_map(|(task, status)| {
                if *status == TaskStatus::Pending {
                    Some(task.clone())
                } else {
                    None
                }
            })
            .collect();

        for task_id in pending {
            let Some(function) = self
                .mission
                .functions
                .iter()
                .find(|function| function.task_ref() == task_id)
            else {
                continue;
            };
            if function
                .preconditions
                .iter()
                .all(|precondition| self.precondition_met(precondition, snapshot))
            {
                self.status.insert(task_id.clone(), TaskStatus::Active);
                self.active_since.insert(task_id, now_secs);
            }
        }
    }

    fn precondition_met(&self, precondition: &str, snapshot: &WorldSnapshot) -> bool {
        if let Some(task_id) = precondition.strip_suffix("_complete") {
            return self.status(task_id) == Some(TaskStatus::Done);
        }
        if let Some(event) = precondition.strip_prefix("event:") {
            return event_met(event, snapshot);
        }
        if precondition.starts_with("feedback:") {
            return false;
        }
        true
    }

    pub fn validate_graph(mission: &MissionDsl) -> Result<(), String> {
        let task_ids: HashSet<String> = mission
            .functions
            .iter()
            .map(|function| function.task_ref().to_string())
            .collect();
        for function in &mission.functions {
            for precondition in &function.preconditions {
                if let Some(task_id) = precondition.strip_suffix("_complete") {
                    if !task_ids.contains(task_id) {
                        return Err(format!(
                            "task '{}' references unknown precondition '{}'",
                            function.task_ref(),
                            precondition
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

fn event_met(event: &str, snapshot: &WorldSnapshot) -> bool {
    let normalized = event.trim().to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "missile_inbound" | "incomingmunition" | "incoming_munition"
    ) {
        return !snapshot.active_munitions.is_empty();
    }
    snapshot.events.iter().any(|world_event| match world_event {
        WorldEvent::WeaponLaunched { .. } => normalized == "weapon_launched",
        WorldEvent::NewContact { .. } => normalized == "new_contact",
        WorldEvent::TrackLost { .. } => normalized == "track_lost",
        WorldEvent::PlatformDestroyed { .. } => normalized == "platform_destroyed",
        WorldEvent::SensorDamaged { .. } => normalized == "sensor_damaged",
        WorldEvent::PlatformHealth { .. } => normalized == "platform_health",
        WorldEvent::MessageReceived { .. } => normalized == "message_received",
    })
}

fn criteria_met(
    function: &FunctionCall,
    snapshot: &WorldSnapshot,
    own_state: &PlatformState,
) -> bool {
    match function.criteria.as_deref().unwrap_or("").trim() {
        "" | "decoy_released" | "jammer_active" | "recon_uav_launched" => true,
        "position_reached" => position_reached(function, own_state),
        "target_destroyed" => target_destroyed(function, snapshot),
        "human/vehicle near crane" => no_human_or_vehicle_near_crane(snapshot),
        _ => true,
    }
}

fn position_reached(function: &FunctionCall, own_state: &PlatformState) -> bool {
    match &function.command {
        PlatformCommandSpec::Goto { lat, lon, alt, .. } => {
            let target = openfang_types::platform::Pose {
                lat_deg: *lat,
                lon_deg: *lon,
                alt_m: alt.unwrap_or(own_state.pose.alt_m),
                heading_deg: 0.0,
                pitch_deg: 0.0,
                roll_deg: 0.0,
            };
            own_state.pose.distance_m(&target) < 50.0
        }
        _ => true,
    }
}

fn target_destroyed(function: &FunctionCall, snapshot: &WorldSnapshot) -> bool {
    let target_id = match &function.command {
        PlatformCommandSpec::Fire { track_id, .. } => track_id,
        PlatformCommandSpec::CoordinatedStrike { target_id, .. } => target_id,
        PlatformCommandSpec::Designate { track_id } => track_id,
        _ => return true,
    };
    snapshot
        .platforms
        .iter()
        .find(|platform| platform.id == *target_id || platform.name == *target_id)
        .map(|platform| platform.damage >= 1.0)
        .unwrap_or(true)
}

fn no_human_or_vehicle_near_crane(snapshot: &WorldSnapshot) -> bool {
    !snapshot
        .platforms
        .iter()
        .flat_map(|platform| &platform.tracks)
        .any(|track| {
            let text =
                format!("{} {}", track.target_name, track.classification).to_ascii_lowercase();
            (text.contains("human") || text.contains("person") || text.contains("vehicle"))
                && text.contains("crane")
                && track.is_active
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::mission_dsl::{
        Constraint, DslObjective, FunctionCall, InterventionPoint, MissionKind,
        PlatformCommandSpec, PlayInstance, SafetyGuard,
    };
    use openfang_types::platform::{CcaRole, PlatformCommand, PlatformState, WorldSnapshot};

    fn own_state() -> PlatformState {
        PlatformState::minimal("self")
    }

    fn snapshot() -> WorldSnapshot {
        WorldSnapshot {
            timestamp: 1.0,
            platforms: vec![own_state()],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        }
    }

    fn function(
        task_id: &str,
        command: PlatformCommandSpec,
        preconditions: Vec<String>,
    ) -> FunctionCall {
        FunctionCall {
            id: format!("fn:{task_id}"),
            task_id: task_id.into(),
            parent_play: "SymbolicPlan".into(),
            platform_id: "self".into(),
            command,
            preconditions,
            criteria: None,
            phase: 0,
            ordering: 0,
            service: None,
            safety_guard: SafetyGuard::default(),
        }
    }

    fn mission() -> MissionDsl {
        MissionDsl {
            id: "mission:test".into(),
            intent_text: "test dependency graph".into(),
            kind: MissionKind::Recon,
            time_window: None,
            objectives: vec![DslObjective {
                id: "obj".into(),
                description: "test".into(),
                feedback_var: Some("test_done".into()),
                priority: 1,
            }],
            constraints: vec![Constraint::standoff(100.0, false)],
            plays: vec![PlayInstance {
                play_id: "SymbolicPlan".into(),
                assigned_platforms: vec!["self".into()],
                role: CcaRole::Adaptive,
                phase: 0,
            }],
            functions: vec![
                function(
                    "T1",
                    PlatformCommandSpec::SetSpeed { speed_ms: 10.0 },
                    vec![],
                ),
                function(
                    "T2",
                    PlatformCommandSpec::SensorOn {
                        sensor_id: "eo".into(),
                    },
                    vec!["T1_complete".into()],
                ),
            ],
            intervention_points: vec![InterventionPoint::require_approval_before_fire()],
            explanation_trace: String::new(),
            confidence: 1.0,
            provenance: "test".into(),
        }
    }

    #[test]
    fn scheduler_releases_only_ready_frontier() {
        let snap = snapshot();
        let own = own_state();
        let mut scheduler = MissionScheduler::new(mission());

        let first = scheduler.tick(&snap, &own, 1.0);
        assert_eq!(scheduler.status("T1"), Some(TaskStatus::Active));
        assert_eq!(scheduler.status("T2"), Some(TaskStatus::Pending));
        assert!(first
            .intents
            .iter()
            .any(|intent| matches!(intent.command, PlatformCommand::SetSpeed { .. })));
        assert!(!first
            .intents
            .iter()
            .any(|intent| matches!(intent.command, PlatformCommand::SensorOn { .. })));

        let second = scheduler.tick(&snap, &own, 2.0);
        assert_eq!(scheduler.status("T1"), Some(TaskStatus::Done));
        assert_eq!(scheduler.status("T2"), Some(TaskStatus::Active));
        assert!(second
            .intents
            .iter()
            .any(|intent| matches!(intent.command, PlatformCommand::SensorOn { .. })));
    }
}
