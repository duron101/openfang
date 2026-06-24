//! SimState → WorldSnapshot mapping.
//! Uses hand-parsed proto_manual::SimState from ArkSIM 4.1 wire format.
//!
//! `StateMessage` 与 **定制态势** protobuf（`arksimproto.proto`）同构。
//! ArkService ZMQ/JSON 定制态势见 [`crate::situation`]。

use crate::proto_manual::SimState;
use openfang_types::platform::*;

/// Convert a hand-parsed SimState to an OpenFang WorldSnapshot.
pub fn from_sim_state(state: &SimState) -> WorldSnapshot {
    let platforms: Vec<PlatformState> = state.platforms.iter().map(map_platform).collect();
    let active_munitions = state.weapons.iter().filter_map(map_active_weapon).collect();

    WorldSnapshot {
        timestamp: state.time,
        platforms,
        active_munitions,
        events: vec![],
        fleet: None,
    }
}

fn map_platform(p: &crate::proto_manual::SimPlatform) -> PlatformState {
    let speed_ms = (p.vn_ms * p.vn_ms + p.ve_ms * p.ve_ms).sqrt();
    let course_deg = p.ve_ms.atan2(p.vn_ms).to_degrees();

    let affiliation = map_affiliation(&p.side);

    let domain = map_domain(&p.domain);

    // Derive platform_type from domain instead of hard-coding "aircraft": a
    // hard-coded aircraft type makes every entity (incl. surface USVs) look like
    // a UAV to the DCC, which then misfires air-domain rules such as
    // auto_rtb_on_low_fuel on a boat.
    let platform_type = match domain {
        Domain::Air => "aircraft",
        Domain::Surface => "usv",
        Domain::Subsurface => "uuv",
        Domain::Land => "ugv",
        Domain::Space => "spacecraft",
        Domain::Unknown => "unknown",
    };

    PlatformState {
        id: p.name.clone(),
        name: p.name.clone(),
        platform_type: platform_type.into(),
        affiliation,
        domain,
        pose: Pose {
            lat_deg: p.lat,
            lon_deg: p.lon,
            alt_m: p.alt,
            heading_deg: p.heading_rad.to_degrees(),
            pitch_deg: p.pitch_rad.to_degrees(),
            roll_deg: p.roll_rad.to_degrees(),
        },
        velocity: Velocity {
            speed_ms,
            vertical_rate_ms: p.vd_ms,
            course_deg: if course_deg < 0.0 {
                course_deg + 360.0
            } else {
                course_deg
            },
        },
        fuel: FuelStatus {
            remaining_kg: p.fuel,
            max_kg: p.max_fuel,
            consumption_rate_kg_s: 0.0,
        },
        damage: p.damage,
        tracks: p.tracks.iter().filter_map(map_track).collect(),
        // Sensors are NOT carried in the StateMessage — leave empty rather than
        // fabricate a name. The mission compiler then commands sensors with an
        // empty component id (the validated "all/default" path) instead of a
        // made-up "primary" that doesn't exist and crashes Warlock.
        onboard_sensors: vec![],
        // Weapons ARE carried (PlatformState.weapons map): map them so commands
        // target a real weapon part (e.g. `loiter_wave2`, `gun_30mm`).
        onboard_weapons: p.weapons.iter().map(map_weapon).collect(),
        onboard_jammers: vec![],
        current_target: None,
        commander: None,
        survivability: None,
        emcon: None,
        link: None,
    }
}

fn map_weapon(w: &crate::proto_manual::SimWeapon) -> WeaponState {
    let is_ready = w.quantity_from_snapshot && w.quantity_remaining > 0.0;
    WeaponState {
        weapon_id: w.name.clone(),
        weapon_type: w.weapon_type.clone(),
        quantity_remaining: w.quantity_remaining,
        max_range_m: None,
        min_range_m: None,
        guidance_type: None,
        speed_ms: None,
        is_ready,
        quantity_from_snapshot: w.quantity_from_snapshot,
    }
}

fn map_track(t: &crate::proto_manual::SimTrack) -> Option<Track> {
    if t.track_id.is_empty() {
        return None;
    }
    let speed_ms = t.velocity_ned.map(|(vn, ve, _)| (vn * vn + ve * ve).sqrt());
    Some(Track {
        track_id: t.track_id.clone(),
        target_name: t.target_name.clone(),
        classification: if t.classification.is_empty() {
            "unknown".into()
        } else {
            t.classification.clone()
        },
        affiliation: map_affiliation(&t.side),
        iff: if t.iff.is_empty() {
            "unknown".into()
        } else {
            t.iff.clone()
        },
        position_lla: t.reported_location_lla.or(t.current_location_lla),
        heading_deg: t.heading_rad.map(f64::to_degrees),
        speed_ms,
        range_m: t.range_m,
        bearing_deg: t.bearing_rad.map(f64::to_degrees),
        elevation_deg: t.elevation_rad.map(f64::to_degrees),
        quality: t.quality,
        stale: t.stale,
        last_update_s: t.update_time,
        is_active: !t.stale,
    })
}

fn map_active_weapon(w: &crate::proto_manual::SimActiveWeapon) -> Option<ActiveMunition> {
    if w.name.is_empty() || w.damage >= 1.0 {
        return None;
    }
    let speed_ms = w
        .velocity_ned
        .map(|(vn, ve, vd)| (vn * vn + ve * ve + vd * vd).sqrt());
    Some(ActiveMunition {
        munition_id: w.name.clone(),
        munition_type: w.weapon_type.clone(),
        affiliation: map_affiliation(&w.side),
        position_lla: w.location_lla,
        heading_deg: w.heading_rad.map(f64::to_degrees),
        speed_ms,
        target_id: if w.current_target.is_empty() {
            None
        } else {
            Some(w.current_target.clone())
        },
        time_to_impact_s: None,
        host_platform_id: if w.host_id.is_empty() {
            None
        } else {
            Some(w.host_id.clone())
        },
    })
}

fn map_domain(domain: &str) -> Domain {
    let d = domain.trim().to_ascii_lowercase();
    match d.as_str() {
        "surface" | "sea" | "ship" | "usv" | "naval" => Domain::Surface,
        "air" | "aircraft" | "uav" | "aerial" => Domain::Air,
        "subsurface" | "submarine" | "uuv" | "underwater" => Domain::Subsurface,
        "land" | "ground" | "ugv" => Domain::Land,
        "space" | "spacecraft" | "orbital" => Domain::Space,
        // ArkSIM scenarios may report Chinese domain labels.
        _ if domain.contains("水面") || domain.contains("水上") => Domain::Surface,
        _ if domain.contains("空") || domain.contains("航空") => Domain::Air,
        _ if domain.contains("水下") || domain.contains("潜") => Domain::Subsurface,
        _ if domain.contains("陆") || domain.contains("地面") => Domain::Land,
        _ if domain.contains("天") || domain.contains("轨道") => Domain::Space,
        _ => Domain::Unknown,
    }
}

fn map_affiliation(side: &str) -> Affiliation {
    let normalized = side.trim().to_ascii_lowercase().replace([' ', '-'], "_");
    match normalized.as_str() {
        "blue" | "blue1" | "blue_force" | "blue_team" => Affiliation::Blue,
        "friend" | "friendly" => Affiliation::Friend,
        "red" | "red1" | "red_force" | "red_team" => Affiliation::Red,
        "foe" | "enemy" | "hostile" => Affiliation::Foe,
        "neutral" => Affiliation::Neutral,
        // Chinese side labels seen in ArkSIM scenarios (e.g. "蓝方"/"红方").
        _ if side.contains('蓝') => Affiliation::Blue,
        _ if side.contains('红') => Affiliation::Red,
        _ if side.contains('友') => Affiliation::Friend,
        _ if side.contains('敌') => Affiliation::Foe,
        _ if side.contains("中立") => Affiliation::Neutral,
        _ => Affiliation::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_state() {
        let state = SimState::default();
        let snapshot = from_sim_state(&state);
        assert_eq!(snapshot.timestamp, 0.0);
        assert!(snapshot.platforms.is_empty());
    }

    #[test]
    fn test_platform_mapping() {
        let p = crate::proto_manual::SimPlatform {
            name: "TestFlight".into(),
            side: "Blue1".into(),
            domain: "air".into(),
            lat: 30.0,
            lon: 120.0,
            alt: 5000.0,
            heading_rad: std::f64::consts::FRAC_PI_2,
            vn_ms: 100.0,
            ..Default::default()
        };
        let state = SimState {
            time: 42.0,
            end_time: 0.0,
            platforms: vec![p],
            ..Default::default()
        };
        let snapshot = from_sim_state(&state);
        assert_eq!(snapshot.platforms.len(), 1);
        let plat = &snapshot.platforms[0];
        assert_eq!(plat.name, "TestFlight");
        assert_eq!(plat.affiliation, Affiliation::Blue);
        assert!((plat.pose.heading_deg - 90.0).abs() < 1.0);
    }

    #[test]
    fn platform_side_mapping_accepts_common_arksim_variants() {
        let cases = [
            ("BLUE", Affiliation::Blue),
            ("blue_force", Affiliation::Blue),
            ("Friend", Affiliation::Friend),
            ("RED", Affiliation::Red),
            ("red_force", Affiliation::Red),
            ("Foe", Affiliation::Foe),
        ];

        for (side, expected) in cases {
            let p = crate::proto_manual::SimPlatform {
                name: format!("{side}-entity"),
                side: side.into(),
                ..Default::default()
            };
            let snapshot = from_sim_state(&SimState {
                time: 0.0,
                end_time: 0.0,
                platforms: vec![p],
                ..Default::default()
            });
            assert_eq!(snapshot.platforms[0].affiliation, expected, "side={side}");
        }
    }

    #[test]
    fn surface_platform_is_not_typed_as_aircraft() {
        // A USV must not be classified as an aircraft/UAV, or DCC air rules
        // (e.g. auto_rtb_on_low_fuel) misfire on it.
        let p = crate::proto_manual::SimPlatform {
            name: "self".into(),
            side: "Red".into(),
            domain: "surface".into(),
            ..Default::default()
        };
        let snapshot = from_sim_state(&SimState {
            time: 0.0,
            end_time: 0.0,
            platforms: vec![p],
            ..Default::default()
        });
        let plat = &snapshot.platforms[0];
        assert_eq!(plat.domain, Domain::Surface);
        assert_eq!(plat.platform_type, "usv");
    }

    #[test]
    fn protobuf_tracks_are_mapped_into_world_snapshot() {
        let p = crate::proto_manual::SimPlatform {
            name: "blue-scout".into(),
            side: "Blue".into(),
            tracks: vec![crate::proto_manual::SimTrack {
                track_id: "red-bandit:1".into(),
                classification: "uav".into(),
                side: "Red".into(),
                iff: "foe".into(),
                reported_location_lla: Some((30.1, 120.2, 1000.0)),
                heading_rad: Some(std::f64::consts::FRAC_PI_2),
                velocity_ned: Some((20.0, 0.0, 0.0)),
                range_m: Some(9_000.0),
                bearing_rad: Some(0.5),
                elevation_rad: Some(0.1),
                quality: 0.82,
                stale: false,
                update_time: 7.5,
                ..Default::default()
            }],
            ..Default::default()
        };
        let snapshot = from_sim_state(&SimState {
            time: 8.0,
            end_time: 0.0,
            platforms: vec![p],
            ..Default::default()
        });

        let track = &snapshot.platforms[0].tracks[0];
        assert_eq!(track.track_id, "red-bandit:1");
        assert_eq!(track.affiliation, Affiliation::Red);
        assert_eq!(track.iff, "foe");
        assert_eq!(track.position_lla, Some((30.1, 120.2, 1000.0)));
        assert_eq!(track.range_m, Some(9_000.0));
        assert!(!track.stale);
        assert!(track.is_active);
    }

    #[test]
    fn uav_launcher_slot_maps_quantity_from_live_snapshot() {
        let p = crate::proto_manual::SimPlatform {
            name: "self".into(),
            side: "Red".into(),
            domain: "surface".into(),
            weapons: vec![crate::proto_manual::SimWeapon {
                name: "scout_uav_slot".into(),
                weapon_type: "SCOUT_UAV_SLOT".into(),
                quantity_remaining: 2.0,
                quantity_from_snapshot: true,
            }],
            ..Default::default()
        };
        let snapshot = from_sim_state(&SimState {
            time: 0.0,
            end_time: 0.0,
            platforms: vec![p],
            ..Default::default()
        });
        let scout = &snapshot.platforms[0].onboard_weapons[0];
        assert_eq!(scout.quantity_remaining, 2.0);
        assert!(scout.is_ready);
        assert!(scout.quantity_from_snapshot);
    }

    #[test]
    fn weapon_without_quantity_field_maps_as_not_ready_until_manifest_seed() {
        let p = crate::proto_manual::SimPlatform {
            name: "self".into(),
            side: "Red".into(),
            domain: "surface".into(),
            weapons: vec![crate::proto_manual::SimWeapon {
                name: "scout_uav_slot".into(),
                weapon_type: "SCOUT_UAV_SLOT".into(),
                quantity_remaining: 0.0,
                quantity_from_snapshot: false,
            }],
            ..Default::default()
        };
        let snapshot = from_sim_state(&SimState {
            time: 0.0,
            end_time: 0.0,
            platforms: vec![p],
            ..Default::default()
        });
        let scout = &snapshot.platforms[0].onboard_weapons[0];
        assert!(!scout.is_ready);
        assert!(!scout.quantity_from_snapshot);
    }

    #[test]
    fn active_weapons_are_mapped_to_active_munitions() {
        let snapshot = from_sim_state(&SimState {
            time: 30.0,
            weapons: vec![crate::proto_manual::SimActiveWeapon {
                name: "self_loiter_wave3_1".into(),
                weapon_type: "RED_LOITER_MUN".into(),
                side: "Red".into(),
                location_lla: Some((10.0, 20.0, 30.0)),
                velocity_ned: Some((3.0, 4.0, 0.0)),
                heading_rad: Some(std::f64::consts::FRAC_PI_2),
                current_target: "blue_sam_site_1".into(),
                host_id: "self".into(),
                damage: 0.0,
            }],
            ..Default::default()
        });

        assert_eq!(snapshot.active_munitions.len(), 1);
        let munition = &snapshot.active_munitions[0];
        assert_eq!(munition.munition_id, "self_loiter_wave3_1");
        assert_eq!(munition.target_id.as_deref(), Some("blue_sam_site_1"));
        assert_eq!(munition.host_platform_id.as_deref(), Some("self"));
        assert_eq!(munition.speed_ms, Some(5.0));
        assert_eq!(munition.heading_deg, Some(90.0));
    }
}
