//! ActionComposer (ACS) — merges slow-loop and fast-loop intents into a single,
//! deconflicted, priority-ordered action set.
//!
//! The composer is the *only* place where the active plan (routine LLM/workflow
//! commands) and reflexive DCC commands are reconciled. Rules:
//!
//! - Inputs are [`CandidateIntent`]s only — never raw commands.
//! - Two intents that share a [`CandidateIntent::conflict_key`] (same platform +
//!   effector class) conflict; only one survives.
//! - Higher priority wins (`Critical` > `High` > `Normal`). On a tie, the more
//!   recently issued intent wins (last writer for that effector).
//! - A `Critical` intent *preempts*: it removes lower-priority intents on the
//!   same effector from the active plan.
//! - The composer does NOT perform safety checks — every surviving intent must
//!   still pass the [`crate::command_gate::CommandGate`]. The composer cannot
//!   emit a [`PlatformCommand`]; it only orders intents.

use std::collections::{HashMap, HashSet};

use openfang_types::platform::PlatformCommand;
use openfang_types::tactical::{CandidateIntent, CommandClass, CommandPriority};

type ConflictKey = (String, CommandClass, String);

/// Stateless-by-default action composer.
///
/// An optional *active plan* may be retained between ticks so that newly
/// arriving reflexive (DCC) intents can preempt the standing plan.
#[derive(Default)]
pub struct ActionComposer {
    /// The standing plan from the slow loop, keyed by effector.
    active_plan: HashMap<ConflictKey, CandidateIntent>,
}

impl ActionComposer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of standing intents currently in the active plan.
    pub fn active_len(&self) -> usize {
        self.active_plan.len()
    }

    /// Replace the active plan with a fresh set of slow-loop intents.
    ///
    /// Conflicts inside `intents` are resolved with [`resolve`].
    pub fn set_active_plan(&mut self, intents: Vec<CandidateIntent>) {
        self.active_plan.clear();
        for intent in resolve(intents) {
            self.active_plan.insert(intent.conflict_key(), intent);
        }
    }

    /// Compose the current active plan with a batch of new intents (typically
    /// fast-loop DCC reflexes) and return the deconflicted, priority-ordered
    /// result. The active plan is updated in place: preempting intents replace
    /// the standing intent for their effector.
    pub fn compose(&mut self, new_intents: Vec<CandidateIntent>) -> Vec<CandidateIntent> {
        // Start from the standing plan.
        let mut merged: HashMap<ConflictKey, CandidateIntent> = self.active_plan.clone();

        for intent in new_intents {
            let key = intent.conflict_key();
            match merged.get(&key) {
                Some(existing) if !wins(&intent, existing) => {
                    // Existing intent stays.
                }
                _ => {
                    // New intent wins this effector.
                    if intent.priority.preempts() {
                        // Critical reflex preempts the standing plan too.
                        self.active_plan.insert(key.clone(), intent.clone());
                    }
                    merged.insert(key, intent);
                }
            }
        }

        let mut out: Vec<CandidateIntent> = merged.into_values().collect();
        drop_safe_when_firing(&mut out);
        sort_by_priority(&mut out);
        out
    }

    /// Clear the active plan (e.g. on mission abort / safe-all).
    pub fn clear(&mut self) {
        self.active_plan.clear();
    }
}

/// Resolve conflicts within a single batch of intents (no standing plan).
/// Returns priority-ordered survivors, one per effector.
pub fn resolve(intents: Vec<CandidateIntent>) -> Vec<CandidateIntent> {
    let mut best: HashMap<ConflictKey, CandidateIntent> = HashMap::new();
    for intent in intents {
        let key = intent.conflict_key();
        match best.get(&key) {
            Some(existing) if !wins(&intent, existing) => {}
            _ => {
                best.insert(key, intent);
            }
        }
    }
    let mut out: Vec<CandidateIntent> = best.into_values().collect();
    drop_safe_when_firing(&mut out);
    sort_by_priority(&mut out);
    out
}

/// Convenience: extract the ordered commands from a list of intents.
///
/// NOTE: this does NOT make the commands safe to dispatch — they must still pass
/// the command gate. It exists for telemetry/inspection only.
pub fn intents_to_commands(intents: &[CandidateIntent]) -> Vec<PlatformCommand> {
    intents.iter().map(|i| i.command.clone()).collect()
}

/// Does `challenger` beat `incumbent` for the same effector?
/// Higher priority wins; on a tie the later `issued_at` wins.
fn wins(challenger: &CandidateIntent, incumbent: &CandidateIntent) -> bool {
    match challenger.priority.cmp(&incumbent.priority) {
        std::cmp::Ordering::Less => true, // lower discriminant = higher urgency
        std::cmp::Ordering::Greater => false,
        std::cmp::Ordering::Equal => challenger.issued_at >= incumbent.issued_at,
    }
}

fn drop_safe_when_firing(intents: &mut Vec<CandidateIntent>) {
    use PlatformCommand::*;

    let firing_platforms: HashSet<String> = intents
        .iter()
        .filter(|intent| {
            matches!(
                intent.command,
                FireAtTarget { .. } | FireSalvo { .. } | CoordinatedStrike { .. }
            )
        })
        .map(|intent| intent.target_platform_id().to_string())
        .collect();

    if firing_platforms.is_empty() {
        return;
    }

    intents.retain(|intent| {
        !(matches!(intent.command, WeaponSafeAll { .. })
            && firing_platforms.contains(intent.target_platform_id()))
    });
}

/// Order intents Critical-first, then by issue time (oldest first within a tier).
fn sort_by_priority(intents: &mut [CandidateIntent]) {
    intents.sort_by(|a, b| {
        a.priority.cmp(&b.priority).then(
            a.issued_at
                .partial_cmp(&b.issued_at)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::tactical::IntentSource;

    fn heading(platform: &str, hdg: f64, prio: CommandPriority, t: f64) -> CandidateIntent {
        CandidateIntent::new(
            PlatformCommand::SetHeading {
                platform_id: platform.into(),
                heading_deg: hdg,
                speed_ms: None,
                turn_direction: None,
            },
            prio,
            IntentSource::External {
                label: "test".into(),
            },
            t,
            "test",
        )
    }

    fn fire(track: &str) -> CandidateIntent {
        CandidateIntent::new(
            PlatformCommand::FireAtTarget {
                platform_id: "self".into(),
                weapon_id: "loiter_wave3".into(),
                track_id: track.into(),
            },
            CommandPriority::Normal,
            IntentSource::External {
                label: "test".into(),
            },
            1.0,
            "fire",
        )
    }

    fn designate(track: &str) -> CandidateIntent {
        CandidateIntent::new(
            PlatformCommand::UpdateTarget {
                platform_id: "self".into(),
                track_id: track.into(),
            },
            CommandPriority::Normal,
            IntentSource::External {
                label: "test".into(),
            },
            1.0,
            "designate",
        )
    }

    fn weapon_safe() -> CandidateIntent {
        CandidateIntent::new(
            PlatformCommand::WeaponSafeAll {
                platform_id: "self".into(),
            },
            CommandPriority::Normal,
            IntentSource::External {
                label: "test".into(),
            },
            1.0,
            "safe",
        )
    }

    #[test]
    fn higher_priority_wins_same_effector() {
        let out = resolve(vec![
            heading("usv-01", 90.0, CommandPriority::Normal, 1.0),
            heading("usv-01", 270.0, CommandPriority::Critical, 0.5),
        ]);
        assert_eq!(out.len(), 1);
        match &out[0].command {
            PlatformCommand::SetHeading { heading_deg, .. } => assert_eq!(*heading_deg, 270.0),
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn tie_breaks_on_latest() {
        let out = resolve(vec![
            heading("usv-01", 90.0, CommandPriority::Normal, 1.0),
            heading("usv-01", 180.0, CommandPriority::Normal, 2.0),
        ]);
        assert_eq!(out.len(), 1);
        match &out[0].command {
            PlatformCommand::SetHeading { heading_deg, .. } => assert_eq!(*heading_deg, 180.0),
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn distinct_effectors_both_survive() {
        let out = resolve(vec![
            heading("usv-01", 90.0, CommandPriority::Normal, 1.0),
            heading("usv-02", 180.0, CommandPriority::Normal, 1.0),
        ]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn multitarget_weapon_plan_survives_conflict_resolution() {
        let out = resolve(vec![
            weapon_safe(),
            designate("blue_patrol_1"),
            designate("blue_patrol_2"),
            designate("blue_patrol_3"),
            fire("blue_patrol_1"),
            fire("blue_patrol_2"),
            fire("blue_patrol_3"),
            weapon_safe(),
        ]);

        assert_eq!(
            out.iter()
                .filter(|intent| matches!(intent.command, PlatformCommand::UpdateTarget { .. }))
                .count(),
            3
        );
        assert_eq!(
            out.iter()
                .filter(|intent| matches!(intent.command, PlatformCommand::FireAtTarget { .. }))
                .count(),
            3
        );
        assert!(
            !out.iter()
                .any(|intent| matches!(intent.command, PlatformCommand::WeaponSafeAll { .. })),
            "platform-level safe must not suppress or coexist with active fires"
        );
    }

    #[test]
    fn component_specific_sensor_commands_survive() {
        let out = resolve(vec![
            CandidateIntent::new(
                PlatformCommand::SensorOff {
                    platform_id: "self".into(),
                    sensor_id: "surf_radar".into(),
                },
                CommandPriority::Normal,
                IntentSource::External {
                    label: "test".into(),
                },
                1.0,
                "radar off",
            ),
            CandidateIntent::new(
                PlatformCommand::SensorOn {
                    platform_id: "self".into(),
                    sensor_id: "eoir".into(),
                },
                CommandPriority::Normal,
                IntentSource::External {
                    label: "test".into(),
                },
                1.0,
                "eoir on",
            ),
        ]);

        assert_eq!(out.len(), 2);
        assert!(out
            .iter()
            .any(|intent| matches!(intent.command, PlatformCommand::SensorOff { .. })));
        assert!(out
            .iter()
            .any(|intent| matches!(intent.command, PlatformCommand::SensorOn { .. })));
    }

    #[test]
    fn critical_dcc_preempts_active_plan() {
        let mut acs = ActionComposer::new();
        acs.set_active_plan(vec![heading("usv-01", 90.0, CommandPriority::Normal, 1.0)]);
        assert_eq!(acs.active_len(), 1);

        // A critical evasive turn arrives from the DCC.
        let out = acs.compose(vec![heading(
            "usv-01",
            270.0,
            CommandPriority::Critical,
            2.0,
        )]);
        assert_eq!(out.len(), 1);
        match &out[0].command {
            PlatformCommand::SetHeading { heading_deg, .. } => assert_eq!(*heading_deg, 270.0),
            _ => panic!("wrong command"),
        }
        // The active plan was preempted: the standing heading is now the critical one.
        let standing = acs.compose(vec![]);
        match &standing[0].command {
            PlatformCommand::SetHeading { heading_deg, .. } => assert_eq!(*heading_deg, 270.0),
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn critical_first_ordering() {
        let out = resolve(vec![
            heading("a", 1.0, CommandPriority::Normal, 1.0),
            heading("b", 2.0, CommandPriority::Critical, 1.0),
            heading("c", 3.0, CommandPriority::High, 1.0),
        ]);
        assert_eq!(out[0].priority, CommandPriority::Critical);
        assert_eq!(out[1].priority, CommandPriority::High);
        assert_eq!(out[2].priority, CommandPriority::Normal);
    }
}
