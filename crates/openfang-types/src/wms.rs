//! Weapon Management Service (WMS) policy types.
//!
//! These shared types describe weapon classes, re-attack behavior and
//! ROE/autonomy dispositions. Runtime policy matching lives outside this crate.

use serde::{Deserialize, Serialize};

use crate::umaa::WeaponReleaseLevel;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReattackMode {
    /// Repeated release is allowed after the configured cooldown window.
    CooldownOnly,
    /// After the first release, a fresh operator re-designation is required.
    FireOnce,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WmsDisposition {
    /// Release may continue through the remaining hard gates immediately.
    Auto,
    /// Release must enter the asynchronous approval queue.
    Pending,
    /// Release is denied before it can reach platform dispatch.
    Deny,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WmsWeaponClass {
    pub id: String,
    pub match_weapon_ids: Vec<String>,
    pub reattack_mode: ReattackMode,
    pub cooldown_secs: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WmsRule {
    pub autonomy_profile: String,
    pub roe: WeaponReleaseLevel,
    pub weapon_class: String,
    pub disposition: WmsDisposition,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WmsPolicyConfig {
    pub weapon_classes: Vec<WmsWeaponClass>,
    pub rules: Vec<WmsRule>,
}
