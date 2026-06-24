//! Play library — tactical templates loaded from the Play metadata embedded in
//! `tactical-assets/workflows/tactical_workflows.toml`.
//!
//! Each workflow may carry a `[workflow.play]` table with the metadata the
//! Mission compiler needs (promt.md §5): preconditions, effect/risk models,
//! expected ROI, the deterministic function list, and required roles. The
//! existing workflow loader ignores this table; only the [`PlayRegistry`] reads
//! it. Play selection is deterministic — it never asks an LLM to "run" steps.

use std::collections::HashMap;

use openfang_types::cognition::TacticStep;
use openfang_types::mission_dsl::MissionKind;
use openfang_types::semantic_frame::Action;
use serde::Deserialize;

/// Default ExpectedROI floor (ρ) and Risk ceiling (r) for play selection.
pub const DEFAULT_MIN_ROI: f64 = 0.4;
pub const DEFAULT_MAX_RISK: f64 = 0.6;

/// A tactical template with its selection metadata.
#[derive(Debug, Clone)]
pub struct PlayDef {
    pub name: String,
    pub preconditions: Vec<String>,
    pub effect_model: HashMap<String, f64>,
    pub risk_model: HashMap<String, f64>,
    pub expected_roi: f64,
    /// Ordered deterministic function names, mapped to commands by the executor.
    pub functions: Vec<String>,
    /// Logical role → required platform type (`"any"` = unconstrained).
    pub required_roles: HashMap<String, String>,
    /// Ordered OODA/task steps from the workflow template.
    pub steps: Vec<TacticStep>,
}

impl PlayDef {
    /// Aggregate risk = worst (max) component of the risk model. `0` if unknown.
    pub fn risk(&self) -> f64 {
        self.risk_model.values().copied().fold(0.0_f64, f64::max)
    }
}

/// Facts about the current situation used to evaluate play preconditions.
#[derive(Debug, Clone, Default)]
pub struct PlaySelectionContext {
    pub has_weapon: bool,
    pub has_sensor: bool,
    pub has_target: bool,
    pub pid_or_designated: bool,
    pub controlled_platform_count: usize,
    /// ExpectedROI floor (ρ). Falls back to [`DEFAULT_MIN_ROI`].
    pub min_roi: f64,
    /// Risk ceiling (r). Falls back to [`DEFAULT_MAX_RISK`].
    pub max_risk: f64,
}

impl PlaySelectionContext {
    pub fn new() -> Self {
        Self {
            min_roi: DEFAULT_MIN_ROI,
            max_risk: DEFAULT_MAX_RISK,
            ..Default::default()
        }
    }

    fn precondition_satisfied(&self, precondition: &str) -> bool {
        match precondition.trim() {
            "" => true,
            "has_weapon" | "has_strike" | "has_strike_uav" => self.has_weapon,
            "has_sensor" | "has_recon" | "has_recon_uav" => self.has_sensor,
            "has_target" | "threat_present" => self.has_target,
            "target_designated_or_labeled" => self.has_target,
            "pid_or_designated" => self.pid_or_designated || self.has_target,
            // Unknown preconditions do not block selection (fail-open for
            // metadata we don't model yet); the compiler/gates remain the
            // authoritative safety boundary.
            _ => true,
        }
    }

    /// Whether all of a play's preconditions hold.
    pub fn satisfies(&self, play: &PlayDef) -> bool {
        play.preconditions
            .iter()
            .all(|p| self.precondition_satisfied(p))
    }
}

/// Loaded play library.
#[derive(Debug, Clone, Default)]
pub struct PlayRegistry {
    plays: Vec<PlayDef>,
}

impl PlayRegistry {
    /// Parse the play metadata out of a tactical-workflows TOML document.
    /// Workflows without a `[workflow.play]` table are skipped.
    pub fn from_toml(text: &str) -> Result<Self, String> {
        let file: PlayFile = toml::from_str(text).map_err(|e| format!("parse play toml: {e}"))?;
        let plays = file
            .workflow
            .into_iter()
            .filter_map(|wf| {
                let play = wf.play?;
                Some(PlayDef {
                    name: wf.name,
                    preconditions: play.preconditions,
                    effect_model: play.effect_model,
                    risk_model: play.risk_model,
                    expected_roi: play.expected_roi,
                    functions: play.functions,
                    required_roles: play.required_roles,
                    steps: wf.step.into_iter().map(Into::into).collect(),
                })
            })
            .collect();
        Ok(Self { plays })
    }

    /// Load the play library bundled with the repository (compile-time embed).
    pub fn bundled() -> Self {
        const BUNDLED: &str =
            include_str!("../../../tactical-assets/workflows/tactical_workflows.toml");
        Self::from_toml(BUNDLED).unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.plays.len()
    }

    pub fn is_empty(&self) -> bool {
        self.plays.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&PlayDef> {
        self.plays.iter().find(|p| p.name == name)
    }

    /// Candidate play names for a mission kind, in preference order.
    pub fn candidates_for(kind: MissionKind) -> &'static [&'static str] {
        match kind {
            MissionKind::Engage => &["Engage"],
            MissionKind::ReconFlankStrike => &["ReconToStrike"],
            MissionKind::CoordinatedStrike => &["CoordinatedStrike", "ReconToStrike"],
            MissionKind::Recon => &["ReconPatrol", "Track"],
            MissionKind::Patrol => &["Patrol"],
            MissionKind::Rtb => &["FleetRecovery"],
            MissionKind::Track => &["Track"],
            MissionKind::PointDefense => &["PointDefense"],
            MissionKind::TargetingHandoff => &["TargetingHandoff"],
            MissionKind::Picket => &["Picket"],
            MissionKind::Escort => &["Escort"],
            MissionKind::MaritimeInterdiction => &["MaritimeInterdiction"],
            MissionKind::Deception => &["Deception"],
            MissionKind::ReactiveDefense => &["ReactiveDefense"],
            MissionKind::SensorControl => &[],
            MissionKind::Unknown => &[],
        }
    }

    /// Select plays for a mission kind: candidate plays whose preconditions hold
    /// and that pass `ExpectedROI ≥ ρ ∧ Risk ≤ r` (promt.md §5). Preserves the
    /// candidate preference order.
    pub fn select(&self, kind: MissionKind, ctx: &PlaySelectionContext) -> Vec<&PlayDef> {
        Self::candidates_for(kind)
            .iter()
            .filter_map(|name| self.get(name))
            .filter(|play| {
                ctx.satisfies(play)
                    && play.expected_roi >= ctx.min_roi
                    && play.risk() <= ctx.max_risk
            })
            .collect()
    }
}

// ── TOML projection (reads only the `play` sub-table) ──

#[derive(Debug, Deserialize)]
struct PlayFile {
    #[serde(default)]
    workflow: Vec<PlayWorkflow>,
}

#[derive(Debug, Deserialize)]
struct PlayWorkflow {
    name: String,
    #[serde(default)]
    step: Vec<PlayStep>,
    #[serde(default)]
    play: Option<PlayMeta>,
}

#[derive(Debug, Deserialize)]
struct PlayStep {
    agent: String,
    action: String,
    #[serde(default)]
    timeout_secs: u64,
}

impl From<PlayStep> for TacticStep {
    fn from(step: PlayStep) -> Self {
        Self {
            agent: step.agent,
            action: action_for_template(&step.action),
            role: None,
            subject: None,
            object: Default::default(),
            guard: Default::default(),
            timeout_secs: step.timeout_secs,
        }
    }
}

fn action_for_template(action: &str) -> Action {
    match action.trim().to_ascii_lowercase().as_str() {
        "navigate"
        | "navigate_to_next_waypoint"
        | "move"
        | "goto"
        | "return"
        | "rtb"
        | "broadcast_rtb" => Action::Goto,
        "patrol" | "patrol_leg" | "route" | "recon_flank_route" => Action::FollowRoute,
        "set_heading" | "heading" => Action::SetHeading,
        "set_speed" | "speed" => Action::SetSpeed,
        "observe"
        | "search"
        | "sensor_on"
        | "sensor_sweep"
        | "isr_collect"
        | "persistent_passive_surveillance"
        | "activate_sensor" => Action::SensorOn,
        "track" | "track_sensor" | "surveil" | "surveil_target_area" | "recon_uav_track_refine" => {
            Action::SensorSetMode
        }
        "designate"
        | "designate_target"
        | "designate_high_value"
        | "targeting_handoff"
        | "midcourse_guidance_update"
        | "update_target" => Action::Track,
        "fire"
        | "strike"
        | "execute_strike"
        | "engage_gun_ciws"
        | "suppress_air_defense"
        | "employ"
        | "deploy_recon_uav"
        | "release_recon_slot" => Action::Employ,
        "evasive_maneuver" => Action::SetHeading,
        "release_decoy" => Action::Employ,
        "start_jam" => Action::Jam,
        "safe" | "weapon_safe" => Action::Safe,
        "jam" | "jamming" => Action::Jam,
        "coordinate" | "handoff" | "relay" | "send_message" => Action::Coordinate,
        _ => Action::Noop,
    }
}

#[derive(Debug, Deserialize)]
struct PlayMeta {
    #[serde(default)]
    preconditions: Vec<String>,
    #[serde(default)]
    effect_model: HashMap<String, f64>,
    #[serde(default)]
    risk_model: HashMap<String, f64>,
    #[serde(default)]
    expected_roi: f64,
    #[serde(default)]
    functions: Vec<String>,
    #[serde(default)]
    required_roles: HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_registry_loads_expected_plays() {
        let registry = PlayRegistry::bundled();
        assert!(registry.get("Engage").is_some());
        assert!(registry.get("ReconToStrike").is_some());
        assert!(registry.get("ReconPatrol").is_some());
        assert!(registry.get("FleetRecovery").is_some());
    }

    #[test]
    fn engage_play_has_fire_function_and_roles() {
        let registry = PlayRegistry::bundled();
        let engage = registry.get("Engage").unwrap();
        assert!(engage.functions.iter().any(|f| f == "fire"));
        assert!(engage.expected_roi > 0.0);
    }

    #[test]
    fn select_engage_requires_weapon_and_target() {
        let registry = PlayRegistry::bundled();
        let mut ctx = PlaySelectionContext::new();
        ctx.has_weapon = false;
        ctx.has_target = true;
        ctx.pid_or_designated = true;
        assert!(
            registry.select(MissionKind::Engage, &ctx).is_empty(),
            "no weapon → Engage unavailable"
        );

        ctx.has_weapon = true;
        let selected = registry.select(MissionKind::Engage, &ctx);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "Engage");
    }

    #[test]
    fn select_recon_flank_strike_uses_recon_to_strike() {
        let registry = PlayRegistry::bundled();
        let mut ctx = PlaySelectionContext::new();
        ctx.has_weapon = true;
        ctx.has_sensor = true;
        ctx.has_target = true;
        ctx.pid_or_designated = true;
        let selected = registry.select(MissionKind::ReconFlankStrike, &ctx);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "ReconToStrike");
        assert!(selected[0]
            .functions
            .iter()
            .any(|f| f == "recon_flank_route"));
    }

    #[test]
    fn high_risk_threshold_filters_play() {
        let registry = PlayRegistry::bundled();
        let mut ctx = PlaySelectionContext::new();
        ctx.has_weapon = true;
        ctx.has_target = true;
        ctx.pid_or_designated = true;
        ctx.max_risk = 0.01; // below Engage's exposure risk
        assert!(registry.select(MissionKind::Engage, &ctx).is_empty());
    }

    #[test]
    fn rtb_play_selectable_without_preconditions() {
        let registry = PlayRegistry::bundled();
        let ctx = PlaySelectionContext::new();
        let selected = registry.select(MissionKind::Rtb, &ctx);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "FleetRecovery");
    }

    #[test]
    fn commander_level_plays_are_registered_and_selectable() {
        let registry = PlayRegistry::bundled();
        // A fully-capable, target-present context so weapon/sensor/target-gated
        // commander-level plays all pass their preconditions.
        let mut ctx = PlaySelectionContext::new();
        ctx.has_weapon = true;
        ctx.has_sensor = true;
        ctx.has_target = true;
        ctx.pid_or_designated = true;

        for (kind, expected) in [
            (MissionKind::PointDefense, "PointDefense"),
            (MissionKind::TargetingHandoff, "TargetingHandoff"),
            (MissionKind::Picket, "Picket"),
            (MissionKind::Escort, "Escort"),
            (MissionKind::MaritimeInterdiction, "MaritimeInterdiction"),
            (MissionKind::Deception, "Deception"),
        ] {
            assert!(
                registry.get(expected).is_some(),
                "play `{expected}` missing from bundled registry"
            );
            let selected = registry.select(kind, &ctx);
            assert_eq!(
                selected.first().map(|p| p.name.as_str()),
                Some(expected),
                "`{expected}` not selected for {kind:?}"
            );
        }
    }
}
