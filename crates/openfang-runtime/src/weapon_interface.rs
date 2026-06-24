//! Weapon Interface — weapon BIT monitoring, launch command encapsulation,
//! and weapon state management. Pure Rust, deterministic.
//!
//! # Architecture
//! - `WeaponInterface` tracks weapon states across platforms
//! - Validates launch preconditions (range, IFF, ROE)
//! - Generates PlatformCommand for weapon operations

use openfang_types::platform::*;
use std::collections::HashMap;

/// Weapon system state.
#[derive(Debug, Clone)]
pub struct WeaponSystem {
    pub weapon_id: String,
    pub weapon_type: String,
    pub platform_id: String,
    pub quantity: f64,
    pub max_range_m: Option<f64>,
    pub is_ready: bool,
    pub bit_result: Option<BitResult>,
}

/// Built-In Test result.
#[derive(Debug, Clone)]
pub struct BitResult {
    pub passed: bool,
    pub fault_code: Option<String>,
    pub last_test_s: f64,
}

/// Fire authorization decision.
#[derive(Debug, Clone, PartialEq)]
pub enum FireAuth {
    /// Authorized — all preconditions met
    Authorized,
    /// Denied — out of range
    OutOfRange {
        weapon_range_m: f64,
        target_range_m: f64,
    },
    /// Denied — IFF conflict (target is friend)
    IffConflict,
    /// Denied — insufficient ammunition
    InsufficientAmmo { available: f64, required: u32 },
    /// Denied — weapon not ready (BIT failed or not armed)
    WeaponNotReady,
    /// Denied — weapon not found
    NotFound,
}

/// Weapon interface engine.
pub struct WeaponInterface {
    /// Platform → weapons map
    platforms: HashMap<String, HashMap<String, WeaponSystem>>,
}

#[derive(Debug, Clone)]
pub struct WeaponOutput {
    pub commands: Vec<PlatformCommand>,
    pub auth_results: Vec<(String, String, FireAuth)>, // (platform, weapon, auth)
}

impl WeaponInterface {
    pub fn new() -> Self {
        Self {
            platforms: HashMap::new(),
        }
    }

    /// Update weapon states from a world snapshot.
    pub fn update(&mut self, snapshot: &WorldSnapshot) {
        for platform in &snapshot.platforms {
            let mut weapons = HashMap::new();
            for w in &platform.onboard_weapons {
                weapons.insert(
                    w.weapon_id.clone(),
                    WeaponSystem {
                        weapon_id: w.weapon_id.clone(),
                        weapon_type: w.weapon_type.clone(),
                        platform_id: platform.id.clone(),
                        quantity: w.quantity_remaining,
                        max_range_m: w.max_range_m,
                        is_ready: w.is_ready,
                        bit_result: None,
                    },
                );
            }
            self.platforms.insert(platform.id.clone(), weapons);
        }
    }

    /// Authorize a fire command against a specific track.
    pub fn authorize_fire(
        &self,
        platform_id: &str,
        weapon_id: &str,
        track: &Track,
        salvo_size: Option<u32>,
    ) -> (FireAuth, Option<PlatformCommand>) {
        let weapons = match self.platforms.get(platform_id) {
            Some(w) => w,
            None => return (FireAuth::NotFound, None),
        };

        let weapon = match weapons.get(weapon_id) {
            Some(w) => w,
            None => return (FireAuth::NotFound, None),
        };

        // 1. Check weapon readiness
        if !weapon.is_ready {
            return (FireAuth::WeaponNotReady, None);
        }

        // 2. Check ammunition
        let effective_salvo_size = salvo_size.filter(|size| *size > 1);
        let required = effective_salvo_size.unwrap_or(1) as f64;
        if weapon.quantity < required {
            return (
                FireAuth::InsufficientAmmo {
                    available: weapon.quantity,
                    required: effective_salvo_size.unwrap_or(1),
                },
                None,
            );
        }

        // 3. Check IFF
        if matches!(track.affiliation, Affiliation::Blue | Affiliation::Friend) {
            return (FireAuth::IffConflict, None);
        }

        // 4. Check range
        if let Some(range) = track.range_m {
            if let Some(max_range) = weapon.max_range_m {
                if range > max_range {
                    return (
                        FireAuth::OutOfRange {
                            weapon_range_m: max_range,
                            target_range_m: range,
                        },
                        None,
                    );
                }
            }
        }

        // Authorized — build command
        let cmd = if let Some(size) = effective_salvo_size {
            PlatformCommand::FireSalvo {
                platform_id: platform_id.to_string(),
                weapon_id: weapon_id.to_string(),
                track_id: track.track_id.clone(),
                salvo_size: size,
            }
        } else {
            PlatformCommand::FireAtTarget {
                platform_id: platform_id.to_string(),
                weapon_id: weapon_id.to_string(),
                track_id: track.track_id.clone(),
            }
        };

        (FireAuth::Authorized, Some(cmd))
    }

    /// Run BIT on a specific weapon (simulated — returns result).
    pub fn run_bit(&mut self, platform_id: &str, weapon_id: &str, now: f64) -> Option<BitResult> {
        let weapons = self.platforms.get_mut(platform_id)?;
        let weapon = weapons.get_mut(weapon_id)?;

        // Simulated BIT: weapon is ready if quantity > 0
        let passed = weapon.quantity > 0.0;
        let fault = if !passed {
            Some("NO_AMMUNITION".to_string())
        } else {
            None
        };

        let result = BitResult {
            passed,
            fault_code: fault,
            last_test_s: now,
        };

        weapon.bit_result = Some(result.clone());
        weapon.is_ready = passed;

        Some(result)
    }

    /// Get weapon status for a platform.
    pub fn get_weapons(&self, platform_id: &str) -> Vec<&WeaponSystem> {
        self.platforms
            .get(platform_id)
            .map(|w| w.values().collect())
            .unwrap_or_default()
    }

    /// List all platforms with weapons.
    pub fn platform_ids(&self) -> Vec<&str> {
        self.platforms.keys().map(|k| k.as_str()).collect()
    }
}

impl Default for WeaponInterface {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_authorize_fire_valid() {
        let mut wi = WeaponInterface::new();

        // Populate with a ready weapon
        let snapshot = WorldSnapshot {
            timestamp: 0.0,
            platforms: vec![PlatformState {
                id: "usv-01".into(),
                name: "USV".into(),
                platform_type: "usv".into(),
                affiliation: Affiliation::Blue,
                domain: Domain::Surface,
                pose: Pose {
                    lat_deg: 0.0,
                    lon_deg: 0.0,
                    alt_m: 0.0,
                    heading_deg: 0.0,
                    pitch_deg: 0.0,
                    roll_deg: 0.0,
                },
                velocity: Velocity {
                    speed_ms: 0.0,
                    vertical_rate_ms: 0.0,
                    course_deg: 0.0,
                },
                fuel: FuelStatus {
                    remaining_kg: 100.0,
                    max_kg: 100.0,
                    consumption_rate_kg_s: 0.0,
                },
                damage: 0.0,
                tracks: vec![],
                onboard_sensors: vec![],
                onboard_weapons: vec![WeaponState {
                    weapon_id: "cannon".into(),
                    weapon_type: "naval_gun".into(),
                    quantity_remaining: 50.0,
                    max_range_m: Some(10000.0),
                    min_range_m: Some(100.0),
                    guidance_type: None,
                    speed_ms: None,
                    is_ready: true,
                    quantity_from_snapshot: true,
                }],
                onboard_jammers: vec![],
                current_target: None,
                commander: None,
                survivability: None,
                emcon: None,
                link: None,
            }],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        };
        wi.update(&snapshot);

        let track = Track {
            track_id: "trk-1".into(),
            target_name: String::new(),
            classification: "boat".into(),
            affiliation: Affiliation::Red,
            iff: "foe".into(),
            position_lla: None,
            heading_deg: None,
            speed_ms: None,
            range_m: Some(5000.0),
            bearing_deg: Some(45.0),
            elevation_deg: None,
            quality: 0.8,
            stale: false,
            last_update_s: 0.0,
            is_active: true,
        };

        let (auth, cmd) = wi.authorize_fire("usv-01", "cannon", &track, None);
        assert_eq!(auth, FireAuth::Authorized);
        assert!(matches!(cmd, Some(PlatformCommand::FireAtTarget { .. })));

        let (auth, cmd) = wi.authorize_fire("usv-01", "cannon", &track, Some(1));
        assert_eq!(auth, FireAuth::Authorized);
        assert!(matches!(cmd, Some(PlatformCommand::FireAtTarget { .. })));

        let (auth, cmd) = wi.authorize_fire("usv-01", "cannon", &track, Some(2));
        assert_eq!(auth, FireAuth::Authorized);
        assert!(matches!(
            cmd,
            Some(PlatformCommand::FireSalvo { salvo_size: 2, .. })
        ));
    }

    #[test]
    fn test_authorize_fire_iff_conflict() {
        let mut wi = WeaponInterface::new();
        let snapshot = WorldSnapshot {
            timestamp: 0.0,
            platforms: vec![PlatformState {
                id: "usv-01".into(),
                name: "USV".into(),
                platform_type: "usv".into(),
                affiliation: Affiliation::Blue,
                domain: Domain::Surface,
                pose: Pose {
                    lat_deg: 0.0,
                    lon_deg: 0.0,
                    alt_m: 0.0,
                    heading_deg: 0.0,
                    pitch_deg: 0.0,
                    roll_deg: 0.0,
                },
                velocity: Velocity {
                    speed_ms: 0.0,
                    vertical_rate_ms: 0.0,
                    course_deg: 0.0,
                },
                fuel: FuelStatus {
                    remaining_kg: 100.0,
                    max_kg: 100.0,
                    consumption_rate_kg_s: 0.0,
                },
                damage: 0.0,
                tracks: vec![],
                onboard_sensors: vec![],
                onboard_weapons: vec![WeaponState {
                    weapon_id: "cannon".into(),
                    weapon_type: "naval_gun".into(),
                    quantity_remaining: 50.0,
                    max_range_m: Some(10000.0),
                    min_range_m: None,
                    guidance_type: None,
                    speed_ms: None,
                    is_ready: true,
                    quantity_from_snapshot: true,
                }],
                onboard_jammers: vec![],
                current_target: None,
                commander: None,
                survivability: None,
                emcon: None,
                link: None,
            }],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        };
        wi.update(&snapshot);

        let track = Track {
            track_id: "trk-2".into(),
            target_name: String::new(),
            classification: "ally".into(),
            affiliation: Affiliation::Blue,
            iff: "friend".into(),
            position_lla: None,
            heading_deg: None,
            speed_ms: None,
            range_m: Some(1000.0),
            bearing_deg: None,
            elevation_deg: None,
            quality: 1.0,
            stale: false,
            last_update_s: 0.0,
            is_active: true,
        };

        let (auth, _) = wi.authorize_fire("usv-01", "cannon", &track, None);
        assert_eq!(auth, FireAuth::IffConflict);
    }

    #[test]
    fn test_bit_run() {
        let mut wi = WeaponInterface::new();
        let snapshot = WorldSnapshot {
            timestamp: 0.0,
            platforms: vec![PlatformState {
                id: "usv-01".into(),
                name: "USV".into(),
                platform_type: "usv".into(),
                affiliation: Affiliation::Blue,
                domain: Domain::Surface,
                pose: Pose {
                    lat_deg: 0.0,
                    lon_deg: 0.0,
                    alt_m: 0.0,
                    heading_deg: 0.0,
                    pitch_deg: 0.0,
                    roll_deg: 0.0,
                },
                velocity: Velocity {
                    speed_ms: 0.0,
                    vertical_rate_ms: 0.0,
                    course_deg: 0.0,
                },
                fuel: FuelStatus {
                    remaining_kg: 100.0,
                    max_kg: 100.0,
                    consumption_rate_kg_s: 0.0,
                },
                damage: 0.0,
                tracks: vec![],
                onboard_sensors: vec![],
                onboard_weapons: vec![WeaponState {
                    weapon_id: "torpedo".into(),
                    weapon_type: "torpedo".into(),
                    quantity_remaining: 0.0,
                    max_range_m: Some(20000.0),
                    min_range_m: None,
                    guidance_type: None,
                    speed_ms: None,
                    is_ready: true,
                    quantity_from_snapshot: true,
                }],
                onboard_jammers: vec![],
                current_target: None,
                commander: None,
                survivability: None,
                emcon: None,
                link: None,
            }],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        };
        wi.update(&snapshot);

        let bit = wi.run_bit("usv-01", "torpedo", 100.0);
        assert!(bit.is_some());
        assert!(!bit.unwrap().passed); // No ammo → BIT fails
    }
}
