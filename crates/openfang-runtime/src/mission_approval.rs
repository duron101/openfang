//! Mission-plan approval registry for human-confirmed plan fingerprints.
//!
//! Mirrors [`crate::target_authorization::TargetAuthorizationRegistry`] but keys
//! approvals by a **plan fingerprint** (a content hash of the mission plan)
//! rather than a target. This gives the `Confirm`/`Quorum` intervention modes a
//! durable backing store so an approved plan is released on the next slow-loop
//! cycle, while any change to the plan content yields a new fingerprint that
//! requires fresh approval.

use std::collections::HashSet;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

/// A persisted approval for a specific plan fingerprint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub fingerprint: String,
    pub signers: Vec<String>,
    pub last_at: f64,
}

#[derive(Default)]
pub struct MissionApprovalRegistry {
    records: DashMap<String, RecordInner>,
}

#[derive(Default)]
struct RecordInner {
    signers: HashSet<String>,
    last_at: f64,
}

impl MissionApprovalRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an approving signature for a plan fingerprint. Idempotent per
    /// signer (a signer approving twice still counts once).
    pub fn approve(&self, fingerprint: impl Into<String>, signer: impl Into<String>, at: f64) {
        let fingerprint = fingerprint.into();
        let mut entry = self.records.entry(fingerprint).or_default();
        entry.signers.insert(signer.into());
        entry.last_at = at;
    }

    /// Whether the fingerprint has reached the required signer quorum.
    pub fn is_approved(&self, fingerprint: &str, quorum: u32) -> bool {
        let needed = quorum.max(1) as usize;
        self.records
            .get(fingerprint)
            .map(|entry| entry.signers.len() >= needed)
            .unwrap_or(false)
    }

    pub fn revoke(&self, fingerprint: &str) -> Option<ApprovalRecord> {
        self.records
            .remove(fingerprint)
            .map(|(fingerprint, inner)| ApprovalRecord {
                fingerprint,
                signers: inner.signers.into_iter().collect(),
                last_at: inner.last_at,
            })
    }

    pub fn list(&self) -> Vec<ApprovalRecord> {
        self.records
            .iter()
            .map(|entry| ApprovalRecord {
                fingerprint: entry.key().clone(),
                signers: entry.value().signers.iter().cloned().collect(),
                last_at: entry.value().last_at,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_signer_confirm_quorum_one() {
        let reg = MissionApprovalRegistry::new();
        assert!(!reg.is_approved("fp-1", 1));
        reg.approve("fp-1", "operator", 10.0);
        assert!(reg.is_approved("fp-1", 1));
    }

    #[test]
    fn quorum_requires_multiple_distinct_signers() {
        let reg = MissionApprovalRegistry::new();
        reg.approve("fp-2", "alice", 1.0);
        assert!(!reg.is_approved("fp-2", 2), "one signer below quorum");
        // Same signer again does not advance quorum.
        reg.approve("fp-2", "alice", 2.0);
        assert!(!reg.is_approved("fp-2", 2));
        reg.approve("fp-2", "bob", 3.0);
        assert!(reg.is_approved("fp-2", 2));
    }

    #[test]
    fn revoke_clears_approval() {
        let reg = MissionApprovalRegistry::new();
        reg.approve("fp-3", "operator", 5.0);
        assert!(reg.is_approved("fp-3", 1));
        let record = reg.revoke("fp-3").expect("record exists");
        assert_eq!(record.signers, vec!["operator".to_string()]);
        assert!(!reg.is_approved("fp-3", 1));
    }

    #[test]
    fn list_reflects_all_records() {
        let reg = MissionApprovalRegistry::new();
        reg.approve("fp-a", "x", 1.0);
        reg.approve("fp-b", "y", 2.0);
        assert_eq!(reg.list().len(), 2);
    }
}
