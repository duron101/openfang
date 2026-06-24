//! Mission decomposition and playbook scheduling for the cognition loop.

use openfang_types::cognition::{Tactic, TacticStep, Task, TaskKind};
use openfang_types::platform::PlatformCommand;
use openfang_types::semantic_frame::Action;
use openfang_types::tactical::{CandidateIntent, CommandPriority, IntentSource};
use openfang_types::umaa::MissionConfig;

use crate::play_registry::{PlayDef, PlayRegistry};

/// Decomposes an approved mission into schedulable [`Task`]s.
///
/// Two paths:
/// - **Fire path (unchanged):** when the mission carries weapon allocations, each
///   allocation becomes a high-priority `Engage` task carrying its concrete
///   weapon/track. The selected play (style) only *enriches* these tasks with
///   `role` / `phase` metadata — the fire mapping is byte-for-byte preserved so
///   the small-loop → arksim path is never disturbed.
/// - **Role-slot path (new, S3+S4):** when there are no allocations but a
///   `play_name` was selected (the commander-level own-scope missions — picket,
///   escort, interdiction, deception, …), the play's `required_roles` are
///   expanded into one task per role, each bound to a concrete platform id
///   (S4 asset binding) and tagged with an OODA `phase` hint.
pub struct MissionDecomposer {
    play_registry: PlayRegistry,
    /// Allow-list of platform ids the slow loop may task (S4 binding pool).
    controlled_platforms: Vec<String>,
    /// `"self"` resolves to this id during role binding.
    own_platform_id: String,
}

impl MissionDecomposer {
    pub fn new() -> Self {
        Self {
            play_registry: PlayRegistry::bundled(),
            controlled_platforms: Vec::new(),
            own_platform_id: String::new(),
        }
    }

    /// Wire the S4 binding pool (controlled platforms + own platform id) from
    /// the active control policy. Without it, `"self"` roles bind to `"self"`
    /// and capability roles fall back to the role type as a symbolic id.
    pub fn with_binding(
        mut self,
        controlled_platforms: Vec<String>,
        own_platform_id: String,
    ) -> Self {
        self.controlled_platforms = controlled_platforms;
        self.own_platform_id = own_platform_id;
        self
    }

    /// Hot-swap the S4 binding pool in place (mirrors the control-policy reload
    /// path used by the cognitive pipeline).
    pub fn set_binding(&mut self, controlled_platforms: Vec<String>, own_platform_id: String) {
        self.controlled_platforms = controlled_platforms;
        self.own_platform_id = own_platform_id;
    }

    pub fn decompose(&self, mission: &MissionConfig) -> Vec<Task> {
        let play = mission
            .play_name
            .as_deref()
            .and_then(|name| self.play_registry.get(name));

        if mission.allocations.is_empty() {
            // Role-slot path: expand the selected play's required roles into
            // bound tasks. Falls back to the legacy phase default when no play
            // (or no roles) is available.
            if let Some(play) = play {
                let tasks = self.expand_role_slots(mission, play);
                if !tasks.is_empty() {
                    return tasks;
                }
            }
            return mission
                .phase
                .as_deref()
                .map(default_task_for_phase)
                .into_iter()
                .flatten()
                .collect();
        }

        // Fire path: one task per weapon allocation. Preserve the exact mapping
        // intents_for relies on; only attach play role/phase metadata.
        let fire_role = play.and_then(primary_fire_role).unwrap_or("strike");
        let phase = mission.phase.clone().unwrap_or_else(|| "engage".into());
        mission
            .allocations
            .iter()
            .map(|allocation| Task {
                id: format!(
                    "{}:{}:{}",
                    mission.mission_id, allocation.platform_id, allocation.track_id
                ),
                kind: TaskKind::Engage,
                assignee: allocation.platform_id.clone(),
                params: serde_json::json!({
                    "weapon_id": allocation.weapon_id,
                    "track_id": allocation.track_id,
                    "salvo_size": allocation.salvo_size,
                    "weapon_policy": allocation.weapon_policy,
                    "role": fire_role,
                    "play": mission.play_name,
                    "phase": phase,
                }),
                priority: CommandPriority::High,
            })
            .collect()
    }

    /// S3 + S4: expand a play's `required_roles` into one bound task per role.
    fn expand_role_slots(&self, mission: &MissionConfig, play: &PlayDef) -> Vec<Task> {
        // Deterministic order so decomposition is reproducible across runs.
        let mut roles: Vec<(&String, &String)> = play.required_roles.iter().collect();
        roles.sort_by(|a, b| a.0.cmp(b.0));

        roles
            .into_iter()
            .map(|(role, platform_type)| {
                let mut assignee = self.bind_role(platform_type);
                let kind = task_kind_for_role(role);
                let mut params = serde_json::json!({
                    "role": role,
                    "platform_type": platform_type,
                    "play": play.name,
                    "phase": role_phase_hint(role),
                });
                if let Some(track_id) = mission.target_track_id.as_deref() {
                    params["track_id"] = serde_json::json!(track_id);
                    if play.name == "ReconPatrol" && role == "recon" {
                        // The scout-UAV slot physically lives on the mothership;
                        // releasing it is a `self` Employ regardless of where the
                        // airborne UAV is later controlled. Bind the launch
                        // subject to the own platform so the Fire command targets
                        // a real platform (the role's `lsuav` token would
                        // otherwise resolve to a non-existent platform in the
                        // single-ship case and silently drop the launch).
                        assignee = self.bind_role("self");
                        params["weapon_id"] = serde_json::json!("scout_uav_slot");
                        params["sensor_mode"] = serde_json::json!("track");
                    }
                    if role_uses_autocannon(role) {
                        params["weapon_id"] = serde_json::json!("autocannon");
                    }
                }
                Task {
                    id: format!("{}:{}:{}", mission.mission_id, play.name, role),
                    kind,
                    assignee,
                    params,
                    priority: CommandPriority::Normal,
                }
            })
            .collect()
    }

    /// S4: resolve a logical role's required platform *type* to a concrete
    /// platform id from the controlled pool. `"self"`/`"usv"` map to the own
    /// platform; capability types (`lsuav`, `cca`, …) greedily match the first
    /// controlled platform whose id contains the type token; `"any"` takes the
    /// own platform (single-ship default). Falls back to the type token itself
    /// so the slot is never silently dropped.
    fn bind_role(&self, platform_type: &str) -> String {
        let t = platform_type.to_ascii_lowercase();
        let own = if self.own_platform_id.is_empty() {
            "self".to_string()
        } else {
            self.own_platform_id.clone()
        };
        match t.as_str() {
            "self" | "usv" | "mothership" => return own,
            "any" => return own,
            _ => {}
        }
        self.controlled_platforms
            .iter()
            .find(|id| id.to_ascii_lowercase().contains(&t))
            .cloned()
            .unwrap_or_else(|| platform_type.to_string())
    }
}

/// The fire-capable role of a play, if any (used to tag allocation tasks).
fn primary_fire_role(play: &PlayDef) -> Option<&'static str> {
    const FIRE_ROLES: [&str; 5] = ["strike", "shooter", "defense", "coordinator", "interdictor"];
    FIRE_ROLES
        .into_iter()
        .find(|fr| play.required_roles.keys().any(|k| k == fr))
}

/// Map a logical play role to the closest existing [`TaskKind`]. Recon/screen
/// style roles become `Track`/`Patrol`; fire roles become `Engage`.
fn task_kind_for_role(role: &str) -> TaskKind {
    match role {
        "recon" | "picket" | "escort" | "interdictor" => TaskKind::Patrol,
        "shooter" | "strike" | "defense" | "coordinator" => TaskKind::Engage,
        "decoy" | "patrol" => TaskKind::Patrol,
        _ => TaskKind::Track,
    }
}

fn role_uses_autocannon(role: &str) -> bool {
    matches!(role, "defense" | "interdictor")
}

/// OODA phase hint attached to a role task's params for downstream tracing.
fn role_phase_hint(role: &str) -> &'static str {
    match role {
        "recon" | "picket" => "observe",
        "interdictor" | "escort" => "orient",
        "shooter" | "strike" | "defense" | "coordinator" => "act",
        "decoy" => "act",
        _ => "orient",
    }
}

impl Default for MissionDecomposer {
    fn default() -> Self {
        Self::new()
    }
}

pub struct PlaybookScheduler {
    play_registry: PlayRegistry,
}

#[derive(Debug, Clone)]
pub struct ScheduledTactic {
    pub tactic: Tactic,
    pub intents: Vec<CandidateIntent>,
}

impl PlaybookScheduler {
    pub fn new() -> Self {
        Self {
            play_registry: PlayRegistry::bundled(),
        }
    }

    pub fn schedule(&self, task: Task, issued_at: f64) -> Result<ScheduledTactic, String> {
        let selected_play = optional_string(&task.params, "play").filter(|p| !p.is_empty());
        let play = selected_play
            .as_deref()
            .and_then(|name| self.play_registry.get(name));
        let playbook = play
            .map(|play| play.name.as_str())
            .unwrap_or_else(|| playbook_for(task.kind));
        let steps = play
            .filter(|play| !play.steps.is_empty())
            .map(|play| play.steps.clone())
            .unwrap_or_else(|| steps_for(task.kind));
        let tactic = Tactic {
            task_id: task.id.clone(),
            playbook: playbook.to_string(),
            steps,
        };
        let intents = if selected_play.is_some() {
            intents_for_steps(&task, &tactic.steps, issued_at)?
        } else {
            intents_for(&task, issued_at)?
        };
        Ok(ScheduledTactic { tactic, intents })
    }
}

impl Default for PlaybookScheduler {
    fn default() -> Self {
        Self::new()
    }
}

fn default_task_for_phase(phase: &str) -> Option<Task> {
    let kind = match phase {
        "patrol" => TaskKind::Patrol,
        "track" => TaskKind::Track,
        "rtb" => TaskKind::Rtb,
        _ => return None,
    };
    Some(Task {
        id: format!("{phase}:default"),
        kind,
        assignee: "self".into(),
        params: default_params_for(kind),
        priority: CommandPriority::Normal,
    })
}

fn default_params_for(kind: TaskKind) -> serde_json::Value {
    match kind {
        // Empty sensor_id = command all/default sensors (validated safe path).
        // A fabricated "primary" sensor crashes AFSIM (null platform-part event).
        TaskKind::Patrol => serde_json::json!({
            "heading_deg": 0.0,
            "speed_ms": 5.0,
            "sensor_id": ""
        }),
        TaskKind::Track => serde_json::json!({
            "sensor_id": "",
            "sensor_mode": "track"
        }),
        TaskKind::Rtb => serde_json::json!({
            "home_lat": 0.0,
            "home_lon": 0.0,
            "home_alt": 0.0,
            "speed_ms": 10.0
        }),
        _ => serde_json::Value::Object(Default::default()),
    }
}

fn playbook_for(kind: TaskKind) -> &'static str {
    match kind {
        TaskKind::Patrol => "Patrol",
        TaskKind::Track => "Track",
        TaskKind::Engage => "Engage",
        TaskKind::Strike => "CoordinatedStrike",
        TaskKind::Relay => "CommRelayHandoff",
        TaskKind::Goto => "Patrol",
        TaskKind::Rtb => "FleetRecovery",
    }
}

fn steps_for(kind: TaskKind) -> Vec<TacticStep> {
    match kind {
        TaskKind::Engage => vec![
            TacticStep {
                agent: "tca".into(),
                action: Action::Track,
                role: None,
                subject: None,
                object: Default::default(),
                guard: Default::default(),
                timeout_secs: 5,
            },
            TacticStep {
                agent: "fca".into(),
                action: Action::Noop,
                role: None,
                subject: None,
                object: Default::default(),
                guard: Default::default(),
                timeout_secs: 3,
            },
            TacticStep {
                agent: "fca".into(),
                action: Action::Employ,
                role: None,
                subject: None,
                object: Default::default(),
                guard: Default::default(),
                timeout_secs: 5,
            },
        ],
        _ => vec![TacticStep {
            agent: "tca".into(),
            action: action_for_task_kind(kind),
            role: None,
            subject: None,
            object: Default::default(),
            guard: Default::default(),
            timeout_secs: 5,
        }],
    }
}

fn action_for_task_kind(kind: TaskKind) -> Action {
    match kind {
        TaskKind::Patrol | TaskKind::Goto => Action::FollowRoute,
        TaskKind::Track => Action::SensorSetMode,
        TaskKind::Engage | TaskKind::Strike => Action::Employ,
        TaskKind::Rtb => Action::Goto,
        TaskKind::Relay => Action::Coordinate,
    }
}

fn intents_for_steps(
    task: &Task,
    steps: &[TacticStep],
    issued_at: f64,
) -> Result<Vec<CandidateIntent>, String> {
    let mut intents = Vec::new();
    for step in steps {
        if let Some(command) = command_for_step(task, step)? {
            intents.push(workflow_intent(
                task,
                issued_at,
                command,
                format!("{:?} step generated tactical intent", step.action),
            ));
        }
    }
    if intents.is_empty() {
        return intents_for(task, issued_at);
    }
    Ok(intents)
}

fn command_for_step(task: &Task, step: &TacticStep) -> Result<Option<PlatformCommand>, String> {
    match step.action {
        Action::FollowRoute | Action::SetHeading => {
            let heading_deg = optional_f64(&task.params, "heading_deg").unwrap_or(0.0);
            let speed_ms = optional_f64(&task.params, "speed_ms");
            Ok(Some(PlatformCommand::SetHeading {
                platform_id: task.assignee.clone(),
                heading_deg,
                speed_ms,
                turn_direction: None,
            }))
        }
        Action::SetSpeed => {
            Ok(
                optional_f64(&task.params, "speed_ms").map(|speed_ms| PlatformCommand::SetSpeed {
                    platform_id: task.assignee.clone(),
                    speed_ms,
                    acceleration_ms2: None,
                }),
            )
        }
        Action::SensorOn => {
            let sensor_id = optional_string(&task.params, "sensor_id").unwrap_or_default();
            Ok(Some(PlatformCommand::SensorOn {
                platform_id: task.assignee.clone(),
                sensor_id,
            }))
        }
        Action::SensorSetMode => {
            let sensor_id = optional_string(&task.params, "sensor_id").unwrap_or_default();
            let mode =
                optional_string(&task.params, "sensor_mode").unwrap_or_else(|| "track".to_string());
            Ok(Some(PlatformCommand::SensorSetMode {
                platform_id: task.assignee.clone(),
                sensor_id,
                mode,
            }))
        }
        Action::Track => Ok(optional_string(&task.params, "track_id")
            .filter(|track_id| !track_id.is_empty())
            .filter(|_| optional_string(&task.params, "weapon_id").is_none())
            .map(|track_id| PlatformCommand::UpdateTarget {
                platform_id: task.assignee.clone(),
                track_id,
            })),
        Action::Employ => {
            let Some(weapon_id) =
                optional_string(&task.params, "weapon_id").filter(|w| !w.is_empty())
            else {
                return Ok(None);
            };
            let track_id = required_param(&task.params, "track_id")?;
            let salvo_size = optional_f64(&task.params, "salvo_size")
                .map(|v| v as u32)
                .filter(|n| *n > 1);
            Ok(Some(match salvo_size {
                Some(salvo_size) => PlatformCommand::FireSalvo {
                    platform_id: task.assignee.clone(),
                    weapon_id,
                    track_id,
                    salvo_size,
                },
                None => PlatformCommand::FireAtTarget {
                    platform_id: task.assignee.clone(),
                    weapon_id,
                    track_id,
                },
            }))
        }
        Action::Goto => {
            let Some(lat) = optional_f64(&task.params, "home_lat") else {
                return Ok(None);
            };
            let lon = optional_f64(&task.params, "home_lon")
                .ok_or_else(|| "missing task param 'home_lon'".to_string())?;
            let alt = optional_f64(&task.params, "home_alt");
            let speed_ms = optional_f64(&task.params, "speed_ms");
            Ok(Some(PlatformCommand::GotoLocation {
                platform_id: task.assignee.clone(),
                lat,
                lon,
                alt,
                speed_ms,
            }))
        }
        Action::Safe => Ok(Some(PlatformCommand::WeaponSafeAll {
            platform_id: task.assignee.clone(),
        })),
        Action::SetAltitude
        | Action::SensorOff
        | Action::Coordinate
        | Action::Jam
        | Action::Noop => Ok(None),
    }
}

fn intents_for(task: &Task, issued_at: f64) -> Result<Vec<CandidateIntent>, String> {
    match task.kind {
        TaskKind::Patrol => {
            let heading_deg = optional_f64(&task.params, "heading_deg").unwrap_or(0.0);
            let speed_ms = optional_f64(&task.params, "speed_ms");
            // Empty default = command all/default sensors (validated safe path).
            // A fabricated "primary" sensor crashes AFSIM (null part event).
            let sensor_id = optional_string(&task.params, "sensor_id").unwrap_or_default();
            Ok(vec![
                workflow_intent(
                    task,
                    issued_at,
                    PlatformCommand::SetHeading {
                        platform_id: task.assignee.clone(),
                        heading_deg,
                        speed_ms,
                        turn_direction: None,
                    },
                    "patrol heading command",
                ),
                workflow_intent(
                    task,
                    issued_at,
                    PlatformCommand::SensorOn {
                        platform_id: task.assignee.clone(),
                        sensor_id,
                    },
                    "patrol sensor activation",
                ),
            ])
        }
        TaskKind::Track => {
            // Empty default = all/default sensors (validated safe path); a made-up
            // "primary" sensor crashes AFSIM (null part event).
            let sensor_id = optional_string(&task.params, "sensor_id").unwrap_or_default();
            let mode =
                optional_string(&task.params, "sensor_mode").unwrap_or_else(|| "track".to_string());
            Ok(vec![workflow_intent(
                task,
                issued_at,
                PlatformCommand::SensorSetMode {
                    platform_id: task.assignee.clone(),
                    sensor_id,
                    mode,
                },
                "track playbook set sensor mode",
            )])
        }
        TaskKind::Engage | TaskKind::Strike => {
            // Role-slot tasks (S3) carry a logical role but no weapon allocation
            // — e.g. a PointDefense `defense` slot or a TargetingHandoff
            // `shooter` slot expanded with no designated track. They must NOT
            // fire blindly. When a track is known we emit an AFSIM-supported
            // `UpdateTarget` (shared-track / targeting handoff, no kinetic
            // effect); otherwise the slot produces no actuation (fail-safe). The
            // real fire path always carries weapon_id + track_id from a
            // validated allocation and is unchanged below.
            let weapon_id = match optional_string(&task.params, "weapon_id") {
                Some(w) if !w.is_empty() => w,
                _ => {
                    if let Some(track_id) =
                        optional_string(&task.params, "track_id").filter(|t| !t.is_empty())
                    {
                        return Ok(vec![workflow_intent(
                            task,
                            issued_at,
                            PlatformCommand::UpdateTarget {
                                platform_id: task.assignee.clone(),
                                track_id,
                            },
                            "role-slot targeting handoff: shared-track update (no fire)",
                        )]);
                    }
                    return Ok(Vec::new());
                }
            };
            let track_id = required_param(&task.params, "track_id")?;
            // Weapon employment: brain-set salvo (>1) ⇒ FireSalvo, else single
            // FireAtTarget. Either way the EngagementGuard + CommandGate + ROE
            // re-validate ammo/range/authority downstream.
            let salvo_size = optional_f64(&task.params, "salvo_size")
                .map(|v| v as u32)
                .filter(|n| *n > 1);
            let command = match salvo_size {
                Some(salvo_size) => PlatformCommand::FireSalvo {
                    platform_id: task.assignee.clone(),
                    weapon_id,
                    track_id,
                    salvo_size,
                },
                None => PlatformCommand::FireAtTarget {
                    platform_id: task.assignee.clone(),
                    weapon_id,
                    track_id,
                },
            };
            Ok(vec![workflow_intent(
                task,
                issued_at,
                command,
                format!(
                    "{} playbook generated tactical fire intent",
                    playbook_for(task.kind)
                ),
            )])
        }
        TaskKind::Rtb => {
            let lat = optional_f64(&task.params, "home_lat")
                .ok_or_else(|| "missing task param 'home_lat'".to_string())?;
            let lon = optional_f64(&task.params, "home_lon")
                .ok_or_else(|| "missing task param 'home_lon'".to_string())?;
            let alt = optional_f64(&task.params, "home_alt");
            let speed_ms = optional_f64(&task.params, "speed_ms");
            Ok(vec![workflow_intent(
                task,
                issued_at,
                PlatformCommand::GotoLocation {
                    platform_id: task.assignee.clone(),
                    lat,
                    lon,
                    alt,
                    speed_ms,
                },
                "rtb playbook generated home navigation intent",
            )])
        }
        _ => Ok(Vec::new()),
    }
}

fn workflow_intent(
    task: &Task,
    issued_at: f64,
    command: PlatformCommand,
    reason: impl Into<String>,
) -> CandidateIntent {
    CandidateIntent::new(
        command,
        task.priority,
        IntentSource::Workflow {
            workflow_id: task.id.clone(),
        },
        issued_at,
        reason,
    )
}

fn optional_string(params: &serde_json::Value, key: &str) -> Option<String> {
    params.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

fn optional_f64(params: &serde_json::Value, key: &str) -> Option<f64> {
    params.get(key).and_then(|v| v.as_f64())
}

fn required_param(params: &serde_json::Value, key: &str) -> Result<String, String> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("missing task param '{key}'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::platform::PlatformCommand;

    #[test]
    fn patrol_task_generates_motion_and_sensor_intents() {
        let task = Task {
            id: "patrol:default".into(),
            kind: TaskKind::Patrol,
            assignee: "self".into(),
            params: serde_json::json!({
                "heading_deg": 45.0,
                "speed_ms": 8.0,
                "sensor_id": "radar"
            }),
            priority: CommandPriority::Normal,
        };

        let scheduled = PlaybookScheduler::new().schedule(task, 10.0).unwrap();

        assert_eq!(scheduled.intents.len(), 2);
        assert!(matches!(
            scheduled.intents[0].command,
            PlatformCommand::SetHeading { ref platform_id, heading_deg, speed_ms: Some(8.0), .. }
                if platform_id == "self" && (heading_deg - 45.0).abs() < f64::EPSILON
        ));
        assert!(matches!(
            scheduled.intents[1].command,
            PlatformCommand::SensorOn { ref platform_id, ref sensor_id }
                if platform_id == "self" && sensor_id == "radar"
        ));
    }

    #[test]
    fn track_task_generates_sensor_track_mode_intent() {
        let task = Task {
            id: "track:default".into(),
            kind: TaskKind::Track,
            assignee: "self".into(),
            params: serde_json::json!({
                "sensor_id": "radar",
                "sensor_mode": "track"
            }),
            priority: CommandPriority::Normal,
        };

        let scheduled = PlaybookScheduler::new().schedule(task, 10.0).unwrap();

        assert_eq!(scheduled.intents.len(), 1);
        assert!(matches!(
            scheduled.intents[0].command,
            PlatformCommand::SensorSetMode { ref platform_id, ref sensor_id, ref mode }
                if platform_id == "self" && sensor_id == "radar" && mode == "track"
        ));
    }

    #[test]
    fn recon_patrol_play_employs_scout_uav_slot_from_template_steps() {
        let task = Task {
            id: "mission:ReconPatrol:recon".into(),
            kind: TaskKind::Patrol,
            assignee: "red_usv_1".into(),
            params: serde_json::json!({
                "play": "ReconPatrol",
                "role": "recon",
                "weapon_id": "scout_uav_slot",
                "track_id": "blue_command_post:1",
                "sensor_id": "eoir",
                "sensor_mode": "track"
            }),
            priority: CommandPriority::Normal,
        };

        let scheduled = PlaybookScheduler::new().schedule(task, 10.0).unwrap();

        assert!(scheduled.intents.iter().any(|intent| matches!(
            intent.command,
            PlatformCommand::FireAtTarget {
                ref platform_id,
                ref weapon_id,
                ref track_id
            } if platform_id == "red_usv_1"
                && weapon_id == "scout_uav_slot"
                && track_id == "blue_command_post:1"
        )));
    }

    #[test]
    fn rtb_task_generates_goto_home_intent() {
        let task = Task {
            id: "rtb:default".into(),
            kind: TaskKind::Rtb,
            assignee: "self".into(),
            params: serde_json::json!({
                "home_lat": 30.0,
                "home_lon": 120.0,
                "home_alt": 1000.0,
                "speed_ms": 20.0
            }),
            priority: CommandPriority::High,
        };

        let scheduled = PlaybookScheduler::new().schedule(task, 10.0).unwrap();

        assert_eq!(scheduled.intents.len(), 1);
        assert!(matches!(
            scheduled.intents[0].command,
            PlatformCommand::GotoLocation { ref platform_id, lat, lon, alt: Some(1000.0), speed_ms: Some(20.0) }
                if platform_id == "self" && (lat - 30.0).abs() < f64::EPSILON && (lon - 120.0).abs() < f64::EPSILON
        ));
    }

    fn mission_with_play(play_name: &str) -> MissionConfig {
        use openfang_types::umaa::{AutonomyMode, CommPlan, PlatformLimits, RulesOfEngagement};
        MissionConfig {
            mission_id: "m-roles".into(),
            roe: RulesOfEngagement::default(),
            geofences: vec![],
            platform_limits: PlatformLimits::default(),
            comm_plan: CommPlan::default(),
            contingency_plans: vec![],
            activated_at: None,
            autonomy_mode: AutonomyMode::HumanSupervised,
            phase: Some("patrol".into()),
            objectives: vec![],
            allocations: vec![],
            target_track_id: None,
            play_name: Some(play_name.into()),
        }
    }

    #[test]
    fn role_slot_decomposition_binds_self_role() {
        // PointDefense requires `defense = "self"`; with no allocations it must
        // expand to a single bound role task on the own platform.
        let decomposer =
            MissionDecomposer::new().with_binding(vec!["uav-lsuav-1".into()], "usv-01".into());
        let mut mission = mission_with_play("PointDefense");
        mission.target_track_id = Some("trk-inbound".into());
        let tasks = decomposer.decompose(&mission);
        assert_eq!(tasks.len(), 1, "PointDefense has one role slot (defense)");
        assert_eq!(tasks[0].assignee, "usv-01", "`self` binds to own platform");
        assert_eq!(tasks[0].params["role"], "defense");
        assert_eq!(tasks[0].params["phase"], "act");
        assert_eq!(tasks[0].params["track_id"], "trk-inbound");
        assert_eq!(tasks[0].params["weapon_id"], "autocannon");
    }

    #[test]
    fn role_slot_decomposition_binds_capability_role() {
        // TargetingHandoff requires recon=lsuav + shooter=cca: two bound slots.
        let decomposer = MissionDecomposer::new().with_binding(
            vec!["uav-lsuav-1".into(), "uav-cca-2".into()],
            "usv-01".into(),
        );
        let mut mission = mission_with_play("TargetingHandoff");
        mission.target_track_id = Some("trk-9".into());
        let tasks = decomposer.decompose(&mission);
        assert_eq!(tasks.len(), 2);
        let recon = tasks.iter().find(|t| t.params["role"] == "recon").unwrap();
        let shooter = tasks
            .iter()
            .find(|t| t.params["role"] == "shooter")
            .unwrap();
        assert_eq!(
            recon.assignee, "uav-lsuav-1",
            "recon binds to lsuav platform"
        );
        assert_eq!(
            shooter.assignee, "uav-cca-2",
            "shooter binds to cca platform"
        );
        assert_eq!(shooter.params["track_id"], "trk-9");
    }

    #[test]
    fn recon_patrol_decompose_binds_uav_launch_to_mothership() {
        // Regression: the scout-UAV slot lives on the mothership. Even though
        // ReconPatrol's `recon` role nominally requires an `lsuav` platform,
        // the *launch* (Employ scout_uav_slot) must bind to the own platform,
        // not to a non-existent `lsuav` token, or AFSIM silently drops it and
        // the UAV never deploys.
        let decomposer = MissionDecomposer::new().with_binding(vec![], "usv-01".into());
        let mut mission = mission_with_play("ReconPatrol");
        mission.target_track_id = Some("blue_command_post:1".into());
        let tasks = decomposer.decompose(&mission);
        let recon = tasks.iter().find(|t| t.params["role"] == "recon").unwrap();
        assert_eq!(
            recon.assignee, "usv-01",
            "UAV launch binds to the mothership that holds the slot"
        );
        assert_eq!(recon.params["weapon_id"], "scout_uav_slot");

        let scheduled = PlaybookScheduler::new()
            .schedule(recon.clone(), 1.0)
            .unwrap();
        assert!(
            scheduled.intents.iter().any(|intent| matches!(
                intent.command,
                PlatformCommand::FireAtTarget { ref platform_id, ref weapon_id, .. }
                    if platform_id == "usv-01" && weapon_id == "scout_uav_slot"
            )),
            "decompose→schedule must emit FireAtTarget(self, scout_uav_slot)"
        );
    }

    #[test]
    fn role_slot_engage_without_weapon_does_not_fire() {
        // A bound defense role with neither weapon nor track must produce no
        // actuation (fail-safe), and the whole tactic must still schedule.
        let task = Task {
            id: "m:PointDefense:defense".into(),
            kind: TaskKind::Engage,
            assignee: "usv-01".into(),
            params: serde_json::json!({ "role": "defense", "phase": "act" }),
            priority: CommandPriority::Normal,
        };
        let scheduled = PlaybookScheduler::new().schedule(task, 1.0).unwrap();
        assert!(
            scheduled.intents.is_empty(),
            "no weapon/track → no fire intent"
        );
    }

    #[test]
    fn role_slot_engage_with_track_emits_supported_update_target() {
        // A handoff slot carrying only a track emits UpdateTarget (AFSIM-
        // supported, non-kinetic) rather than a blind fire.
        let task = Task {
            id: "m:TargetingHandoff:shooter".into(),
            kind: TaskKind::Engage,
            assignee: "uav-cca-2".into(),
            params: serde_json::json!({ "role": "shooter", "track_id": "trk-9" }),
            priority: CommandPriority::Normal,
        };
        let scheduled = PlaybookScheduler::new().schedule(task, 1.0).unwrap();
        assert_eq!(scheduled.intents.len(), 1);
        assert!(matches!(
            scheduled.intents[0].command,
            PlatformCommand::UpdateTarget { ref platform_id, ref track_id }
                if platform_id == "uav-cca-2" && track_id == "trk-9"
        ));
    }

    #[test]
    fn role_slot_defense_with_target_emits_autocannon_fire() {
        let task = Task {
            id: "m:PointDefense:defense".into(),
            kind: TaskKind::Engage,
            assignee: "usv-01".into(),
            params: serde_json::json!({
                "role": "defense",
                "track_id": "trk-inbound",
                "weapon_id": "autocannon",
                "play": "PointDefense",
            }),
            priority: CommandPriority::Normal,
        };
        let scheduled = PlaybookScheduler::new().schedule(task, 1.0).unwrap();
        assert_eq!(scheduled.tactic.playbook, "PointDefense");
        assert_eq!(scheduled.tactic.steps.len(), 5);
        assert_eq!(scheduled.intents.len(), 1);
        assert!(matches!(
            scheduled.intents[0].command,
            PlatformCommand::FireAtTarget { ref platform_id, ref weapon_id, ref track_id }
                if platform_id == "usv-01" && weapon_id == "autocannon" && track_id == "trk-inbound"
        ));
    }

    #[test]
    fn allocation_fire_path_is_preserved_with_play_metadata() {
        use openfang_types::umaa::TargetAllocation;
        let mut mission = mission_with_play("Engage");
        mission.phase = Some("engage".into());
        mission.allocations = vec![TargetAllocation {
            platform_id: "usv-01".into(),
            weapon_id: "gun".into(),
            track_id: "trk-1".into(),
            allocated_at: 1.0,
            ..Default::default()
        }];
        let tasks = MissionDecomposer::new().decompose(&mission);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].kind, TaskKind::Engage);
        assert_eq!(tasks[0].assignee, "usv-01");
        // Fire mapping fields intact …
        assert_eq!(tasks[0].params["weapon_id"], "gun");
        assert_eq!(tasks[0].params["track_id"], "trk-1");
        // … plus the new style metadata.
        assert_eq!(tasks[0].params["role"], "strike");
        assert_eq!(tasks[0].params["phase"], "engage");
    }
}
