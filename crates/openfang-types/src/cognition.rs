//! Cognition-loop types shared by slow planning and fast control injection.

use serde::{Deserialize, Serialize};

use crate::mission_dsl::SafetyGuard;
use crate::semantic_frame::{Action, ObjectBinding, SubjectRef};
use crate::tactical::CommandPriority;
use crate::umaa::WeaponReleaseLevel;

/// Situation understanding distilled from a [`crate::platform::WorldSnapshot`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SituationAssessment {
    pub timestamp: f64,
    pub threats: Vec<ThreatTrack>,
    pub opportunities: Vec<EngageOpportunity>,
    pub own_force: OwnForceStatus,
    pub summary: String,
}

/// A threat track enriched with tactical geometry and scoring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreatTrack {
    pub track_id: String,
    pub platform_type: String,
    pub distance_m: f64,
    pub closing_rate_ms: f64,
    pub threat_score: f64,
}

/// A potential weapon-to-track engagement option.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngageOpportunity {
    pub platform_id: String,
    pub weapon_id: String,
    pub track_id: String,
    pub estimated_p_hit: f64,
}

/// Own-force aggregate status for planning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnForceStatus {
    pub total_platforms: usize,
    pub average_damage: f64,
    pub average_fuel_pct: f64,
    pub link_status: String,
}

/// A permissible execution window for the mission, in absolute sim seconds.
/// Both bounds are optional: `start=None` ⇒ "now", `end=None` ⇒ "open-ended".
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TimeWindow {
    #[serde(default)]
    pub start_s: Option<f64>,
    #[serde(default)]
    pub end_s: Option<f64>,
}

impl TimeWindow {
    /// Whether `t` (sim seconds) falls inside this window (inclusive bounds).
    pub fn contains(&self, t: f64) -> bool {
        self.start_s.map(|s| t >= s).unwrap_or(true) && self.end_s.map(|e| t <= e).unwrap_or(true)
    }
}

/// Human/operator intent injected into the slow planning loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommanderIntent {
    pub id: String,
    pub issued_at: f64,
    pub issued_by: String,
    pub objective: String,
    pub priority_tracks: Vec<String>,
    #[serde(default)]
    pub priority_labels: Vec<String>,
    pub constraints: Vec<String>,
    pub roe_pref: Option<WeaponReleaseLevel>,
    /// Commander cost-policy weights (promt.md §S1) used by the LLM mission-plan
    /// refiner to trade off effect vs. time vs. survivability when re-ranking
    /// engagement opportunities. Conventional keys: `w_effect`, `w_time`,
    /// `w_survive`, `w_cost`. Empty ⇒ refiner uses its built-in defaults.
    #[serde(default)]
    pub cost_policy: std::collections::HashMap<String, f64>,
    /// Permissible execution windows. Empty ⇒ no timing restriction.
    #[serde(default)]
    pub time_windows: Vec<TimeWindow>,
    /// Whether the planner may accept a degraded (partial-effect / reduced-asset)
    /// solution when the full plan is infeasible. Defaults to `false`
    /// (fail-closed: do not silently degrade).
    #[serde(default)]
    pub allow_degrade: bool,
}

/// A schedulable unit derived from an active mission.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub kind: TaskKind,
    pub assignee: String,
    pub params: serde_json::Value,
    pub priority: CommandPriority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Patrol,
    Track,
    Engage,
    Strike,
    Relay,
    Goto,
    Rtb,
}

/// Playbook-expanded tactic ready to be converted into candidate intents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tactic {
    pub task_id: String,
    pub playbook: String,
    pub steps: Vec<TacticStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TacticStep {
    pub agent: String,
    pub action: Action,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub subject: Option<SubjectRef>,
    #[serde(default)]
    pub object: ObjectBinding,
    #[serde(default)]
    pub guard: SafetyGuard,
    #[serde(default)]
    pub timeout_secs: u64,
}
