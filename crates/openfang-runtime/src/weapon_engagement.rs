//! Weapon engagement state machine — asynchronous, non-blocking approval flow.
//!
//! When the CommandGate defers a weapon-class intent (ROE `WeaponsTight`), it
//! returns `Pending { approval_id }`. The actual multi-party authorization then
//! proceeds as an explicit state machine that is advanced by `tick(now)` and by
//! incoming signatures — it NEVER blocks the control tick waiting for a human.
//!
//! ```text
//! Requested ─► PendingSignatures ─► Approved ─► Armed ─► Launched
//!                   │                  │           │        │
//!                   ├─► Rejected       └──────────┴────────┴─► Aborted
//!                   └─► Expired (deadline passed without quorum)
//! ```
//!
//! Time is taken from a [`TimeSource`] so simulation and hardware behave
//! identically.

use std::collections::HashMap;

use openfang_types::platform::PlatformCommand;
use openfang_types::tactical::TimeSource;
use serde::Serialize;

/// Lifecycle state of a single weapon engagement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EngagementState {
    /// Created, not yet submitted for signatures.
    Requested,
    /// Awaiting `required` signatures before `deadline_s`.
    PendingSignatures { collected: u32, required: u32 },
    /// Quorum reached; cleared for arming.
    Approved,
    /// Explicitly denied by an authority.
    Rejected,
    /// Deadline passed without reaching quorum.
    Expired,
    /// Weapon armed (post-approval).
    Armed,
    /// Weapon launched.
    Launched,
    /// Engagement aborted (any non-terminal state).
    Aborted,
}

impl EngagementState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Rejected | Self::Expired | Self::Launched | Self::Aborted
        )
    }
}

/// A single weapon engagement undergoing authorization.
#[derive(Debug, Clone)]
pub struct WeaponEngagement {
    pub approval_id: String,
    pub command: PlatformCommand,
    pub state: EngagementState,
    pub required_signatures: u32,
    pub signers: Vec<String>,
    pub created_s: f64,
    pub deadline_s: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct EngagementSnapshot {
    pub approval_id: String,
    pub state: EngagementState,
    pub required_signatures: u32,
    pub collected_signatures: u32,
    pub signers: Vec<String>,
    pub created_s: f64,
    pub deadline_s: f64,
    pub command_summary: String,
}

impl WeaponEngagement {
    fn collected(&self) -> u32 {
        self.signers.len() as u32
    }

    fn snapshot(&self) -> EngagementSnapshot {
        EngagementSnapshot {
            approval_id: self.approval_id.clone(),
            state: self.state.clone(),
            required_signatures: self.required_signatures,
            collected_signatures: self.collected(),
            signers: self.signers.clone(),
            created_s: self.created_s,
            deadline_s: self.deadline_s,
            command_summary: format!("{:?}", self.command.command_class()),
        }
    }
}

/// Outcome of advancing the manager by one tick.
#[derive(Debug, Default, Clone)]
pub struct EngagementTick {
    /// Engagements that just expired this tick.
    pub expired: Vec<String>,
}

/// Manages all in-flight weapon engagements. Single-threaded ownership; advanced
/// by the control loop. No internal locking, no blocking, no I/O.
pub struct WeaponEngagementManager {
    engagements: HashMap<String, WeaponEngagement>,
    default_required: u32,
    default_window_s: f64,
}

impl WeaponEngagementManager {
    pub fn new(default_required: u32, default_window_s: f64) -> Self {
        Self {
            engagements: HashMap::new(),
            default_required: default_required.max(1),
            default_window_s,
        }
    }

    /// Register a deferred weapon command and move it to PendingSignatures.
    /// `now` comes from the active [`TimeSource`].
    pub fn open(
        &mut self,
        approval_id: impl Into<String>,
        command: PlatformCommand,
        now: f64,
    ) -> &WeaponEngagement {
        let approval_id = approval_id.into();
        let eng = WeaponEngagement {
            approval_id: approval_id.clone(),
            command,
            state: EngagementState::PendingSignatures {
                collected: 0,
                required: self.default_required,
            },
            required_signatures: self.default_required,
            signers: Vec::new(),
            created_s: now,
            deadline_s: now + self.default_window_s,
        };
        self.engagements.insert(approval_id.clone(), eng);
        self.engagements.get(&approval_id).unwrap()
    }

    /// Add a signature from a distinct signer. Reaching quorum → Approved.
    /// Returns the new state, or None if the engagement is unknown/terminal.
    pub fn add_signature(
        &mut self,
        approval_id: &str,
        signer: impl Into<String>,
    ) -> Option<EngagementState> {
        let eng = self.engagements.get_mut(approval_id)?;
        if !matches!(eng.state, EngagementState::PendingSignatures { .. }) {
            return Some(eng.state.clone());
        }
        let signer = signer.into();
        if !eng.signers.contains(&signer) {
            eng.signers.push(signer);
        }
        let collected = eng.collected();
        eng.state = if collected >= eng.required_signatures {
            EngagementState::Approved
        } else {
            EngagementState::PendingSignatures {
                collected,
                required: eng.required_signatures,
            }
        };
        Some(eng.state.clone())
    }

    /// Reject an engagement outright.
    pub fn reject(&mut self, approval_id: &str) -> Option<EngagementState> {
        let eng = self.engagements.get_mut(approval_id)?;
        if !eng.state.is_terminal() {
            eng.state = EngagementState::Rejected;
        }
        Some(eng.state.clone())
    }

    /// Arm an approved engagement.
    pub fn arm(&mut self, approval_id: &str) -> Option<EngagementState> {
        let eng = self.engagements.get_mut(approval_id)?;
        if eng.state == EngagementState::Approved {
            eng.state = EngagementState::Armed;
        }
        Some(eng.state.clone())
    }

    /// Command to dispatch for an armed engagement. Does not mark launch.
    pub fn launch_command(&self, approval_id: &str) -> Option<PlatformCommand> {
        let eng = self.engagements.get(approval_id)?;
        (eng.state == EngagementState::Armed).then(|| eng.command.clone())
    }

    /// Mark an armed engagement as launched after adapter dispatch succeeds.
    pub fn mark_launched(&mut self, approval_id: &str) -> Option<EngagementState> {
        let eng = self.engagements.get_mut(approval_id)?;
        if eng.state == EngagementState::Armed {
            eng.state = EngagementState::Launched;
        }
        Some(eng.state.clone())
    }

    /// Abort a non-terminal engagement.
    pub fn abort(&mut self, approval_id: &str) -> Option<EngagementState> {
        let eng = self.engagements.get_mut(approval_id)?;
        if !eng.state.is_terminal() {
            eng.state = EngagementState::Aborted;
        }
        Some(eng.state.clone())
    }

    /// Non-blocking advance: expire any pending engagement past its deadline.
    pub fn tick(&mut self, time: &dyn TimeSource) -> EngagementTick {
        let now = time.now_secs();
        let mut out = EngagementTick::default();
        for eng in self.engagements.values_mut() {
            if matches!(eng.state, EngagementState::PendingSignatures { .. })
                && now >= eng.deadline_s
            {
                eng.state = EngagementState::Expired;
                out.expired.push(eng.approval_id.clone());
            }
        }
        out
    }

    pub fn state(&self, approval_id: &str) -> Option<EngagementState> {
        self.engagements.get(approval_id).map(|e| e.state.clone())
    }

    /// Approval ids of engagements still awaiting signatures.
    pub fn pending_ids(&self) -> Vec<String> {
        self.engagements
            .iter()
            .filter(|(_, e)| matches!(e.state, EngagementState::PendingSignatures { .. }))
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Approval ids that reached quorum and are ready to arm/launch.
    pub fn approved_ids(&self) -> Vec<String> {
        self.engagements
            .iter()
            .filter(|(_, e)| e.state == EngagementState::Approved)
            .map(|(k, _)| k.clone())
            .collect()
    }

    pub fn snapshots(&self) -> Vec<EngagementSnapshot> {
        let mut snapshots: Vec<_> = self
            .engagements
            .values()
            .map(WeaponEngagement::snapshot)
            .collect();
        snapshots.sort_by(|a, b| a.approval_id.cmp(&b.approval_id));
        snapshots
    }

    pub fn len(&self) -> usize {
        self.engagements.len()
    }

    pub fn is_empty(&self) -> bool {
        self.engagements.is_empty()
    }

    /// Remove terminal engagements; returns how many were pruned.
    pub fn prune_terminal(&mut self) -> usize {
        let before = self.engagements.len();
        self.engagements.retain(|_, e| !e.state.is_terminal());
        before - self.engagements.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::tactical::ManualClock;

    fn fire() -> PlatformCommand {
        PlatformCommand::FireAtTarget {
            platform_id: "usv-01".into(),
            weapon_id: "cannon".into(),
            track_id: "trk-1".into(),
        }
    }

    #[test]
    fn full_happy_path() {
        let mut mgr = WeaponEngagementManager::new(2, 30.0);
        mgr.open("a1", fire(), 0.0);
        assert!(matches!(
            mgr.state("a1"),
            Some(EngagementState::PendingSignatures { .. })
        ));
        mgr.add_signature("a1", "operator-1");
        assert!(matches!(
            mgr.state("a1"),
            Some(EngagementState::PendingSignatures { collected: 1, .. })
        ));
        let st = mgr.add_signature("a1", "operator-2").unwrap();
        assert_eq!(st, EngagementState::Approved);
        assert_eq!(mgr.arm("a1"), Some(EngagementState::Armed));
        let cmd = mgr.launch_command("a1");
        assert!(cmd.is_some());
        assert_eq!(mgr.state("a1"), Some(EngagementState::Armed));
        assert_eq!(mgr.mark_launched("a1"), Some(EngagementState::Launched));
        assert_eq!(mgr.state("a1"), Some(EngagementState::Launched));
    }

    #[test]
    fn duplicate_signer_does_not_count_twice() {
        let mut mgr = WeaponEngagementManager::new(2, 30.0);
        mgr.open("a1", fire(), 0.0);
        mgr.add_signature("a1", "operator-1");
        let st = mgr.add_signature("a1", "operator-1").unwrap();
        assert_eq!(
            st,
            EngagementState::PendingSignatures {
                collected: 1,
                required: 2
            }
        );
    }

    #[test]
    fn expires_after_deadline() {
        let mut mgr = WeaponEngagementManager::new(2, 30.0);
        let clock = ManualClock::new(0.0);
        mgr.open("a1", fire(), clock.now_secs());
        clock.set(31.0);
        let tick = mgr.tick(&clock);
        assert_eq!(tick.expired, vec!["a1".to_string()]);
        assert_eq!(mgr.state("a1"), Some(EngagementState::Expired));
        // Cannot arm an expired engagement.
        assert_eq!(mgr.arm("a1"), Some(EngagementState::Expired));
        assert!(mgr.launch_command("a1").is_none());
    }

    #[test]
    fn cannot_launch_without_arming() {
        let mut mgr = WeaponEngagementManager::new(1, 30.0);
        mgr.open("a1", fire(), 0.0);
        mgr.add_signature("a1", "op");
        assert_eq!(mgr.state("a1"), Some(EngagementState::Approved));
        assert!(mgr.launch_command("a1").is_none()); // must arm first
    }

    #[test]
    fn abort_and_prune() {
        let mut mgr = WeaponEngagementManager::new(2, 30.0);
        mgr.open("a1", fire(), 0.0);
        mgr.abort("a1");
        assert_eq!(mgr.state("a1"), Some(EngagementState::Aborted));
        assert_eq!(mgr.prune_terminal(), 1);
        assert!(mgr.is_empty());
    }
}
