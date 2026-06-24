//! Scenario component manifest support for ArkSIM/Warlock.
//!
//! StateMessage carries dynamic tracks and, in the customized proto, weapon maps.
//! It does not reliably expose every stable platform part (sensors, comms,
//! movers). A scenario manifest fills only that stable component tree. Tracks are
//! deliberately excluded: track ids are time-varying and must be resolved from
//! the latest snapshot at send time.

use std::path::Path;

use openfang_types::platform::{SensorState, SensorType, WeaponState, WorldSnapshot};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ComponentManifest {
    pub platforms: Vec<ManifestPlatform>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ManifestPlatform {
    pub id: String,
    pub name: Option<String>,
    pub aliases: Vec<String>,
    pub mover: Option<String>,
    pub sensors: Vec<ManifestSensor>,
    pub weapons: Vec<ManifestWeapon>,
    pub comms: Vec<String>,
    pub jammers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ManifestSensor {
    pub id: String,
    pub sensor_type: SensorType,
    pub mode: String,
}

impl Default for ManifestSensor {
    fn default() -> Self {
        Self {
            id: String::new(),
            sensor_type: SensorType::Other,
            mode: "SEARCH".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ManifestWeapon {
    pub id: String,
    pub weapon_type: String,
    pub quantity_remaining: Option<f64>,
}

impl ComponentManifest {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("read component manifest {}: {e}", path.display()))?;
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if ext == "json" {
            serde_json::from_str(&text)
                .map_err(|e| format!("parse component manifest JSON {}: {e}", path.display()))
        } else {
            toml::from_str(&text)
                .map_err(|e| format!("parse component manifest TOML {}: {e}", path.display()))
        }
    }

    pub fn apply_to_snapshot(&self, snapshot: &mut WorldSnapshot) {
        for platform in &mut snapshot.platforms {
            let Some(entry) = self.find_platform(&platform.id, &platform.name) else {
                continue;
            };

            if platform.onboard_sensors.is_empty() {
                platform.onboard_sensors = entry
                    .sensors
                    .iter()
                    .filter(|s| !s.id.is_empty())
                    .map(|s| SensorState {
                        sensor_id: s.id.clone(),
                        sensor_type: s.sensor_type,
                        mode: s.mode.clone(),
                        frequency_hz: None,
                        bandwidth_hz: None,
                        azimuth_fov_deg: None,
                        elevation_fov_deg: None,
                        range_max_m: None,
                        damage: 0.0,
                        host_platform_id: platform.id.clone(),
                    })
                    .collect();
            }

            for weapon in &entry.weapons {
                if weapon.id.is_empty() {
                    continue;
                }
                if let Some(existing) = platform
                    .onboard_weapons
                    .iter_mut()
                    .find(|w| w.weapon_id == weapon.id)
                {
                    merge_weapon_from_manifest(existing, weapon);
                    continue;
                }
                let qty = weapon.quantity_remaining.unwrap_or(1.0);
                platform.onboard_weapons.push(WeaponState {
                    weapon_id: weapon.id.clone(),
                    weapon_type: if weapon.weapon_type.is_empty() {
                        weapon.id.clone()
                    } else {
                        weapon.weapon_type.clone()
                    },
                    quantity_remaining: qty,
                    max_range_m: None,
                    min_range_m: None,
                    guidance_type: None,
                    speed_ms: None,
                    is_ready: qty > 0.0,
                    quantity_from_snapshot: false,
                });
            }
        }
    }

    fn find_platform(&self, id: &str, name: &str) -> Option<&ManifestPlatform> {
        self.platforms.iter().find(|p| {
            p.id == id
                || p.id == name
                || p.name.as_deref() == Some(id)
                || p.name.as_deref() == Some(name)
                || p.aliases.iter().any(|a| a == id || a == name)
        })
    }
}

/// Seed or refresh manifest inventory when live telemetry has not yet reported
/// `quantityRemaining` for a weapon component that already exists in the snapshot.
fn merge_weapon_from_manifest(existing: &mut WeaponState, manifest: &ManifestWeapon) {
    if existing.quantity_from_snapshot {
        existing.is_ready = existing.quantity_remaining > 0.0;
        return;
    }
    let Some(manifest_qty) = manifest.quantity_remaining else {
        return;
    };
    if manifest_qty <= 0.0 {
        return;
    }
    if existing.weapon_type.is_empty() && !manifest.weapon_type.is_empty() {
        existing.weapon_type = manifest.weapon_type.clone();
    }
    existing.quantity_remaining = manifest_qty;
    existing.is_ready = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::platform::PlatformState;

    #[test]
    fn bundled_usv_loiter_strike_manifest_loads_from_repo() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tactical-assets/config/arksim.usv_loiter_strike.components.toml"
        );
        let manifest = ComponentManifest::load(path).expect("repo manifest must parse");
        assert_eq!(manifest.platforms.len(), 1);
        let platform = &manifest.platforms[0];
        assert_eq!(platform.id, "self");
        assert!(
            platform.weapons.iter().any(|w| w.id == "scout_uav_slot"),
            "scout_uav_slot must be declared"
        );
    }

    #[test]
    fn manifest_adds_stable_components_but_not_tracks() {
        let manifest = ComponentManifest {
            platforms: vec![ManifestPlatform {
                id: "self".into(),
                sensors: vec![ManifestSensor {
                    id: "surf_radar".into(),
                    sensor_type: SensorType::Radar,
                    mode: "SEARCH".into(),
                }],
                weapons: vec![ManifestWeapon {
                    id: "loiter_wave3".into(),
                    weapon_type: "RED_LOITER_MUN".into(),
                    quantity_remaining: Some(2.0),
                }],
                ..Default::default()
            }],
        };
        let mut snapshot = WorldSnapshot {
            timestamp: 1.0,
            platforms: vec![PlatformState::minimal("self")],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        };

        manifest.apply_to_snapshot(&mut snapshot);

        let platform = &snapshot.platforms[0];
        assert_eq!(platform.onboard_sensors[0].sensor_id, "surf_radar");
        assert_eq!(platform.onboard_weapons[0].weapon_id, "loiter_wave3");
        assert!(
            platform.tracks.is_empty(),
            "tracks must remain snapshot-owned"
        );
    }

    #[test]
    fn manifest_seeds_quantity_when_live_weapon_omits_quantity_remaining() {
        let manifest = ComponentManifest {
            platforms: vec![ManifestPlatform {
                id: "self".into(),
                weapons: vec![ManifestWeapon {
                    id: "scout_uav_slot".into(),
                    weapon_type: "SCOUT_UAV_SLOT".into(),
                    quantity_remaining: Some(2.0),
                }],
                ..Default::default()
            }],
        };
        let mut snapshot = WorldSnapshot {
            timestamp: 1.0,
            platforms: vec![PlatformState {
                onboard_weapons: vec![WeaponState {
                    weapon_id: "scout_uav_slot".into(),
                    weapon_type: "SCOUT_UAV_SLOT".into(),
                    quantity_remaining: 0.0,
                    max_range_m: None,
                    min_range_m: None,
                    guidance_type: None,
                    speed_ms: None,
                    is_ready: false,
                    quantity_from_snapshot: false,
                }],
                ..PlatformState::minimal("self")
            }],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        };

        manifest.apply_to_snapshot(&mut snapshot);

        let scout = &snapshot.platforms[0].onboard_weapons[0];
        assert_eq!(scout.quantity_remaining, 2.0);
        assert!(scout.is_ready);
        assert!(!scout.quantity_from_snapshot);
    }

    #[test]
    fn manifest_does_not_override_live_depleted_weapon() {
        let manifest = ComponentManifest {
            platforms: vec![ManifestPlatform {
                id: "self".into(),
                weapons: vec![ManifestWeapon {
                    id: "loiter_wave3".into(),
                    weapon_type: "RED_LOITER_MUN".into(),
                    quantity_remaining: Some(16.0),
                }],
                ..Default::default()
            }],
        };
        let mut snapshot = WorldSnapshot {
            timestamp: 1.0,
            platforms: vec![PlatformState {
                onboard_weapons: vec![WeaponState {
                    weapon_id: "loiter_wave3".into(),
                    weapon_type: "RED_LOITER_MUN".into(),
                    quantity_remaining: 0.0,
                    max_range_m: None,
                    min_range_m: None,
                    guidance_type: None,
                    speed_ms: None,
                    is_ready: false,
                    quantity_from_snapshot: true,
                }],
                ..PlatformState::minimal("self")
            }],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        };

        manifest.apply_to_snapshot(&mut snapshot);

        let loiter = &snapshot.platforms[0].onboard_weapons[0];
        assert_eq!(loiter.quantity_remaining, 0.0);
        assert!(!loiter.is_ready);
        assert!(loiter.quantity_from_snapshot);
    }
}
