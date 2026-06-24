//! WMS policy matching and default release matrix.

use std::fs;
use std::path::Path;

use openfang_types::umaa::WeaponReleaseLevel;
use openfang_types::wms::{ReattackMode, WmsDisposition, WmsPolicyConfig, WmsRule, WmsWeaponClass};

#[derive(Debug, Clone)]
pub struct WmsPolicyEngine {
    config: WmsPolicyConfig,
}

impl WmsPolicyEngine {
    pub fn new(config: WmsPolicyConfig) -> Self {
        Self { config }
    }

    pub fn from_toml_str(raw: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(raw).map(Self::new)
    }

    pub fn load_from_home(home_dir: &Path) -> Result<Self, String> {
        let path = home_dir.join("wms_policy.toml");
        let raw = fs::read_to_string(&path)
            .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        Self::from_toml_str(&raw)
            .map_err(|err| format!("failed to parse {}: {err}", path.display()))
    }

    pub fn class_for_weapon(&self, weapon_id: &str) -> Option<&WmsWeaponClass> {
        self.config
            .weapon_classes
            .iter()
            .find(|class| class_matches_weapon(class, weapon_id))
    }

    pub fn disposition_for(
        &self,
        autonomy_profile: &str,
        roe: WeaponReleaseLevel,
        weapon_id: &str,
    ) -> WmsDisposition {
        let Some(class) = self.class_for_weapon(weapon_id) else {
            return default_disposition_for_unknown(roe);
        };
        self.config
            .rules
            .iter()
            .find(|rule| {
                rule.autonomy_profile == autonomy_profile
                    && rule.roe == roe
                    && rule.weapon_class == class.id
            })
            .map(|rule| rule.disposition)
            .unwrap_or_else(|| default_disposition_for_class(roe, class))
    }

    pub fn reattack_mode_for(&self, weapon_id: &str) -> ReattackMode {
        self.class_for_weapon(weapon_id)
            .map(|class| class.reattack_mode)
            .unwrap_or(ReattackMode::FireOnce)
    }

    pub fn cooldown_secs_for(&self, weapon_id: &str) -> Option<f64> {
        self.class_for_weapon(weapon_id)
            .map(|class| class.cooldown_secs.max(0.0))
    }
}

impl Default for WmsPolicyEngine {
    fn default() -> Self {
        Self::new(WmsPolicyConfig {
            weapon_classes: vec![
                WmsWeaponClass {
                    id: "gun_fire".into(),
                    match_weapon_ids: vec!["gun*".into(), "*cannon*".into()],
                    reattack_mode: ReattackMode::FireOnce,
                    cooldown_secs: 2.0,
                },
                WmsWeaponClass {
                    id: "kinetic_strike".into(),
                    match_weapon_ids: vec![
                        "loiter*".into(),
                        "*missile*".into(),
                        "*rocket*".into(),
                        "*torpedo*".into(),
                    ],
                    reattack_mode: ReattackMode::FireOnce,
                    cooldown_secs: 60.0,
                },
                WmsWeaponClass {
                    id: "self_defense_countermeasure".into(),
                    match_weapon_ids: vec![
                        "*chaff*".into(),
                        "*flare*".into(),
                        "*decoy*".into(),
                        "*jammer*".into(),
                    ],
                    reattack_mode: ReattackMode::CooldownOnly,
                    cooldown_secs: 1.0,
                },
                WmsWeaponClass {
                    id: "isr_deploy".into(),
                    match_weapon_ids: vec![
                        "scout_uav*".into(),
                        "*scout_uav*".into(),
                        "recon_uav*".into(),
                        "*recon_uav*".into(),
                        "j7_uav*".into(),
                        "*j7_uav*".into(),
                        "*uav_weapon*".into(),
                    ],
                    reattack_mode: ReattackMode::CooldownOnly,
                    cooldown_secs: 2.0,
                },
            ],
            rules: default_rules(),
        })
    }
}

fn class_matches_weapon(class: &WmsWeaponClass, weapon_id: &str) -> bool {
    let value = weapon_id.to_ascii_lowercase();
    class
        .match_weapon_ids
        .iter()
        .any(|pattern| wildcard_match(&pattern.to_ascii_lowercase(), &value))
}

fn default_rules() -> Vec<WmsRule> {
    use WeaponReleaseLevel::{WeaponsFree, WeaponsHold, WeaponsTight};
    use WmsDisposition::{Auto, Deny, Pending};

    let kinetic_classes = ["gun_fire", "kinetic_strike"];
    let profiles = ["default", "defensive_autonomy", "weapons_free_constrained"];
    let mut rules = Vec::new();
    for profile in profiles {
        for weapon_class in kinetic_classes {
            rules.extend([
                WmsRule {
                    autonomy_profile: profile.into(),
                    roe: WeaponsHold,
                    weapon_class: weapon_class.into(),
                    disposition: Deny,
                },
                WmsRule {
                    autonomy_profile: profile.into(),
                    roe: WeaponsTight,
                    weapon_class: weapon_class.into(),
                    disposition: Pending,
                },
                WmsRule {
                    autonomy_profile: profile.into(),
                    roe: WeaponsFree,
                    weapon_class: weapon_class.into(),
                    disposition: Auto,
                },
            ]);
        }
        rules.extend([
            WmsRule {
                autonomy_profile: profile.into(),
                roe: WeaponsHold,
                weapon_class: "isr_deploy".into(),
                disposition: Auto,
            },
            WmsRule {
                autonomy_profile: profile.into(),
                roe: WeaponsTight,
                weapon_class: "isr_deploy".into(),
                disposition: Auto,
            },
            WmsRule {
                autonomy_profile: profile.into(),
                roe: WeaponsFree,
                weapon_class: "isr_deploy".into(),
                disposition: Auto,
            },
            WmsRule {
                autonomy_profile: profile.into(),
                roe: WeaponsHold,
                weapon_class: "self_defense_countermeasure".into(),
                disposition: Auto,
            },
            WmsRule {
                autonomy_profile: profile.into(),
                roe: WeaponsTight,
                weapon_class: "self_defense_countermeasure".into(),
                disposition: Auto,
            },
            WmsRule {
                autonomy_profile: profile.into(),
                roe: WeaponsFree,
                weapon_class: "self_defense_countermeasure".into(),
                disposition: Auto,
            },
        ]);
    }
    for weapon_class in kinetic_classes {
        rules.extend([
            WmsRule {
                autonomy_profile: "supervised_autonomy".into(),
                roe: WeaponsHold,
                weapon_class: weapon_class.into(),
                disposition: Deny,
            },
            WmsRule {
                autonomy_profile: "supervised_autonomy".into(),
                roe: WeaponsTight,
                weapon_class: weapon_class.into(),
                disposition: Pending,
            },
            WmsRule {
                autonomy_profile: "supervised_autonomy".into(),
                roe: WeaponsFree,
                weapon_class: weapon_class.into(),
                disposition: Pending,
            },
        ]);
    }
    for weapon_class in ["isr_deploy", "self_defense_countermeasure"] {
        for roe in [WeaponsHold, WeaponsTight, WeaponsFree] {
            rules.push(WmsRule {
                autonomy_profile: "supervised_autonomy".into(),
                roe,
                weapon_class: weapon_class.into(),
                disposition: Auto,
            });
        }
    }
    rules
}

fn default_disposition_for_unknown(roe: WeaponReleaseLevel) -> WmsDisposition {
    match roe {
        WeaponReleaseLevel::WeaponsHold => WmsDisposition::Deny,
        WeaponReleaseLevel::WeaponsTight => WmsDisposition::Pending,
        WeaponReleaseLevel::WeaponsFree => WmsDisposition::Auto,
    }
}

fn default_disposition_for_class(
    roe: WeaponReleaseLevel,
    class: &WmsWeaponClass,
) -> WmsDisposition {
    match class.reattack_mode {
        ReattackMode::CooldownOnly => WmsDisposition::Auto,
        ReattackMode::FireOnce => default_disposition_for_unknown(roe),
    }
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" || pattern == value {
        return true;
    }

    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    if !first.is_empty() && !value.starts_with(first) {
        return false;
    }

    let mut cursor = first.len();
    let mut last_part = first;
    for part in parts {
        last_part = part;
        if part.is_empty() {
            continue;
        }
        let Some(found) = value[cursor..].find(part) else {
            return false;
        };
        cursor += found + part.len();
    }

    pattern.ends_with('*') || last_part.is_empty() || value.ends_with(last_part)
}
