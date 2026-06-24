//! Target authorization registry for human-confirmed tracks.

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use openfang_types::umaa::WeaponReleaseLevel;

const LLM_AUTH_PREFIX: &str = "llm:";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthRecord {
    pub platform_id: String,
    pub track_id: String,
    pub authorized_by: String,
    pub authorized_at: f64,
}

#[derive(Default)]
pub struct TargetAuthorizationRegistry {
    records: DashMap<String, AuthRecord>,
}

impl TargetAuthorizationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn authorize(
        &self,
        platform_id: impl Into<String>,
        track_id: impl Into<String>,
        authorized_by: impl Into<String>,
        authorized_at: f64,
    ) {
        let platform_id = platform_id.into();
        let track_id = track_id.into();
        let record = AuthRecord {
            platform_id: platform_id.clone(),
            track_id: track_id.clone(),
            authorized_by: authorized_by.into(),
            authorized_at,
        };
        self.records.insert(key(&platform_id, &track_id), record);
    }

    pub fn revoke(&self, platform_id: &str, track_id: &str) -> Option<AuthRecord> {
        self.records
            .remove(&key(platform_id, track_id))
            .map(|(_, record)| record)
    }

    pub fn is_authorized(&self, platform_id: &str, track_id: &str) -> bool {
        self.records.contains_key(&key(platform_id, track_id))
    }

    /// Check whether an authorization is valid for the current weapon-release
    /// authority. LLM-issued authorizations are scoped to `WeaponsFree` only:
    /// once ROE tightens, a human/operator authorization is required again.
    pub fn is_authorized_for_roe(
        &self,
        platform_id: &str,
        track_id: &str,
        roe: Option<WeaponReleaseLevel>,
    ) -> bool {
        self.records
            .get(&key(platform_id, track_id))
            .map(|record| {
                !record.authorized_by.starts_with(LLM_AUTH_PREFIX)
                    || roe == Some(WeaponReleaseLevel::WeaponsFree)
            })
            .unwrap_or(false)
    }

    pub fn list(&self) -> Vec<AuthRecord> {
        self.records
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }
}

fn key(platform_id: &str, track_id: &str) -> String {
    format!("{platform_id}\u{1f}{track_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::umaa::WeaponReleaseLevel;

    #[test]
    fn authorize_and_revoke_track() {
        let registry = TargetAuthorizationRegistry::new();

        assert!(!registry.is_authorized("usv-01", "trk-1"));
        registry.authorize("usv-01", "trk-1", "operator", 10.0);
        assert!(registry.is_authorized("usv-01", "trk-1"));
        assert_eq!(registry.list().len(), 1);

        registry.revoke("usv-01", "trk-1");
        assert!(!registry.is_authorized("usv-01", "trk-1"));
    }

    #[test]
    fn llm_authorization_is_only_valid_under_weapons_free() {
        let registry = TargetAuthorizationRegistry::new();

        registry.authorize("usv-01", "trk-1", "llm:planner", 10.0);

        assert!(registry.is_authorized_for_roe(
            "usv-01",
            "trk-1",
            Some(WeaponReleaseLevel::WeaponsFree)
        ));
        assert!(!registry.is_authorized_for_roe(
            "usv-01",
            "trk-1",
            Some(WeaponReleaseLevel::WeaponsTight)
        ));
    }

    #[test]
    fn operator_authorization_remains_valid_under_weapons_tight() {
        let registry = TargetAuthorizationRegistry::new();

        registry.authorize("usv-01", "trk-1", "operator", 10.0);

        assert!(registry.is_authorized_for_roe(
            "usv-01",
            "trk-1",
            Some(WeaponReleaseLevel::WeaponsTight)
        ));
    }
}
