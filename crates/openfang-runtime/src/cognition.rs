//! Rule-based situation cognition for the slow planning loop.

use std::collections::{HashMap, HashSet};

use openfang_types::cognition::{
    EngageOpportunity, OwnForceStatus, SituationAssessment, ThreatTrack,
};
use openfang_types::config::PlatformControlPolicy;
use openfang_types::platform::{PlatformState, Track, WeaponState, WorldSnapshot};

use crate::sensor_fusion::{FusedTrack, ThreatLevel};

#[derive(Debug, Clone)]
pub struct CognitionEngine {
    policy: PlatformControlPolicy,
}

impl CognitionEngine {
    pub fn new(policy: PlatformControlPolicy) -> Self {
        Self { policy }
    }

    pub fn policy(&self) -> &PlatformControlPolicy {
        &self.policy
    }

    pub fn assess(&self, snapshot: &WorldSnapshot) -> SituationAssessment {
        self.assess_with_fused(snapshot, &[])
    }

    /// Threat/opportunity assessment that回灌s the unified SMS fusion picture
    /// into WMS target allocation. When a raw threat track correlates (by
    /// `track_id`) with a fused track, the Kalman-confirmed `threat_level` and
    /// fused quality raise its `threat_score` floor, so weapon allocation
    /// reasons about the *same* stable danger the cerebellum services see
    /// rather than a single noisy per-sensor return. Passing `&[]` reproduces
    /// the legacy raw-only behaviour exactly.
    pub fn assess_with_fused(
        &self,
        snapshot: &WorldSnapshot,
        fused_tracks: &[FusedTrack],
    ) -> SituationAssessment {
        let fused_by_id: HashMap<&str, &FusedTrack> = fused_tracks
            .iter()
            .map(|track| (track.track_id.as_str(), track))
            .collect();
        let threats = extract_threats(snapshot, &self.policy, &fused_by_id);
        let opportunities = extract_opportunities(snapshot, &threats, &self.policy);
        let own_force = own_force_status(snapshot, &self.policy);
        let summary = format!(
            "side={:?} platforms={} threats={} opportunities={} fused={}",
            self.policy.controlled_side,
            own_force.total_platforms,
            threats.len(),
            opportunities.len(),
            fused_tracks.len()
        );
        SituationAssessment {
            timestamp: snapshot.timestamp,
            threats,
            opportunities,
            own_force,
            summary,
        }
    }
}

/// Lower bound a fused [`ThreatLevel`] places on a correlated track's
/// `threat_score`. A Kalman-confirmed hostile never scores below this even if
/// its latest raw return is momentarily noisy.
fn fused_threat_floor(level: ThreatLevel) -> f64 {
    match level {
        ThreatLevel::None => 0.0,
        ThreatLevel::Low => 0.3,
        ThreatLevel::Medium => 0.5,
        ThreatLevel::High => 0.75,
        ThreatLevel::Critical => 0.9,
    }
}

impl Default for CognitionEngine {
    fn default() -> Self {
        Self::new(PlatformControlPolicy::default())
    }
}

fn extract_threats(
    snapshot: &WorldSnapshot,
    policy: &PlatformControlPolicy,
    fused_by_id: &HashMap<&str, &FusedTrack>,
) -> Vec<ThreatTrack> {
    let mut threats: Vec<ThreatTrack> = snapshot
        .platforms
        .iter()
        .flat_map(|platform| platform.tracks.iter().map(move |track| (platform, track)))
        .filter(|(_, track)| is_threat(track, policy))
        .map(|(platform, track)| {
            // 回灌: lift the raw score to the fused threat floor when the SMS
            // fusion engine has a correlated, Kalman-confirmed track for this id.
            let mut score = threat_score(track);
            if let Some(fused) = fused_by_id.get(track.track_id.as_str()) {
                score = score.max(fused_threat_floor(fused.threat_level));
            }
            ThreatTrack {
                track_id: track.track_id.clone(),
                platform_type: track.classification.clone(),
                distance_m: track
                    .range_m
                    .unwrap_or_else(|| distance_to_track(platform, track)),
                closing_rate_ms: closing_rate_ms(platform, track),
                threat_score: score,
            }
        })
        .collect();

    let mut seen: HashSet<String> = threats
        .iter()
        .map(|threat| threat.track_id.clone())
        .collect();
    for platform in &snapshot.platforms {
        if is_controllable_platform(platform, policy)
            || !policy.track_is_threat(platform.affiliation, "")
            || !seen.insert(platform.id.clone())
        {
            continue;
        }
        let track_id = nearest_controlled_track_id_for_platform(snapshot, platform, policy)
            .unwrap_or_else(|| platform.id.clone());
        threats.push(ThreatTrack {
            track_id,
            platform_type: platform.platform_type.clone(),
            distance_m: nearest_controlled_platform_distance(snapshot, platform, policy),
            closing_rate_ms: 0.0,
            threat_score: platform_threat_score(platform),
        });
    }
    threats
}

fn is_threat(track: &Track, policy: &PlatformControlPolicy) -> bool {
    !track.stale && track.is_active && policy.track_is_threat(track.affiliation, &track.iff)
}

/// Threat metric in `0..1` from a track's quality, range and speed. Shared by
/// the cognition engine (slow loop) and the DCC `HighThreat`/`IncomingMunition`
/// reflex conditions (fast loop) so both reason about the *same* assessed
/// danger rather than two divergent formulas.
pub fn threat_score(track: &Track) -> f64 {
    let quality = track.quality.clamp(0.0, 1.0);
    let range_factor = track
        .range_m
        .map(|range| (1.0 - (range / 20_000.0)).clamp(0.0, 1.0))
        .unwrap_or(0.25);
    let speed_factor = track
        .speed_ms
        .map(|speed| (speed / 300.0).clamp(0.0, 1.0))
        .unwrap_or(0.0);
    (0.65 * quality + 0.25 * range_factor + 0.10 * speed_factor).clamp(0.0, 1.0)
}

fn distance_to_track(platform: &PlatformState, track: &Track) -> f64 {
    track
        .position_lla
        .map(|(lat, lon, alt)| {
            let (north_m, east_m, up_m) = relative_position_m(platform, lat, lon, alt);
            (north_m * north_m + east_m * east_m + up_m * up_m).sqrt()
        })
        .unwrap_or(f64::INFINITY)
}

fn nearest_controlled_platform_distance(
    snapshot: &WorldSnapshot,
    target: &PlatformState,
    policy: &PlatformControlPolicy,
) -> f64 {
    snapshot
        .platforms
        .iter()
        .filter(|platform| is_controllable_platform(platform, policy))
        .map(|platform| distance_between_platforms(platform, target))
        .fold(f64::INFINITY, f64::min)
}

fn nearest_controlled_track_id_for_platform(
    snapshot: &WorldSnapshot,
    target: &PlatformState,
    policy: &PlatformControlPolicy,
) -> Option<String> {
    let nearest_position_track = snapshot
        .platforms
        .iter()
        .filter(|platform| is_controllable_platform(platform, policy))
        .flat_map(|platform| platform.tracks.iter())
        .filter(|track| !track.stale && track.is_active)
        .filter_map(|track| {
            let (lat, lon, alt) = track.position_lla?;
            let distance_m = distance_lla_to_platform_m(lat, lon, alt, target);
            Some((distance_m, track.track_id.clone()))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, track_id)| track_id);

    nearest_position_track
        .or_else(|| ordered_controlled_track_id_for_platform(snapshot, target, policy))
}

fn ordered_controlled_track_id_for_platform(
    snapshot: &WorldSnapshot,
    target: &PlatformState,
    policy: &PlatformControlPolicy,
) -> Option<String> {
    let target_index = snapshot
        .platforms
        .iter()
        .filter(|platform| {
            !is_controllable_platform(platform, policy)
                && policy.track_is_threat(platform.affiliation, "")
        })
        .position(|platform| platform.id == target.id)?;

    snapshot
        .platforms
        .iter()
        .filter(|platform| is_controllable_platform(platform, policy))
        .flat_map(|platform| platform.tracks.iter())
        .filter(|track| !track.stale && track.is_active)
        .nth(target_index)
        .map(|track| track.track_id.clone())
}

fn distance_between_platforms(a: &PlatformState, b: &PlatformState) -> f64 {
    let (north_m, east_m, up_m) =
        relative_position_m(a, b.pose.lat_deg, b.pose.lon_deg, b.pose.alt_m);
    (north_m * north_m + east_m * east_m + up_m * up_m).sqrt()
}

fn distance_lla_to_platform_m(lat: f64, lon: f64, alt: f64, platform: &PlatformState) -> f64 {
    let dlat = (lat - platform.pose.lat_deg).to_radians();
    let dlon = (lon - platform.pose.lon_deg).to_radians();
    let mean_lat = ((lat + platform.pose.lat_deg) * 0.5).to_radians();
    let north_m = dlat * 6_371_000.0;
    let east_m = dlon * 6_371_000.0 * mean_lat.cos();
    let up_m = alt - platform.pose.alt_m;
    (north_m * north_m + east_m * east_m + up_m * up_m).sqrt()
}

fn platform_threat_score(platform: &PlatformState) -> f64 {
    let type_bonus = if platform
        .platform_type
        .to_ascii_lowercase()
        .contains("command")
    {
        0.75
    } else if platform.platform_type.to_ascii_lowercase().contains("sam") {
        0.7
    } else {
        0.55
    };
    (type_bonus * (1.0 - platform.damage).clamp(0.0, 1.0)).clamp(0.0, 1.0)
}

fn closing_rate_ms(platform: &PlatformState, track: &Track) -> f64 {
    let Some((lat, lon, alt)) = track.position_lla else {
        return track.speed_ms.unwrap_or(0.0);
    };
    let Some(track_speed) = track.speed_ms else {
        return 0.0;
    };

    let (north_m, east_m, up_m) = relative_position_m(platform, lat, lon, alt);
    let range_m = (north_m * north_m + east_m * east_m + up_m * up_m).sqrt();
    if range_m <= f64::EPSILON {
        return 0.0;
    }

    let track_heading = track.heading_deg.unwrap_or(0.0).to_radians();
    let track_north_ms = track_speed * track_heading.cos();
    let track_east_ms = track_speed * track_heading.sin();
    let own_heading = platform.velocity.course_deg.to_radians();
    let own_north_ms = platform.velocity.speed_ms * own_heading.cos();
    let own_east_ms = platform.velocity.speed_ms * own_heading.sin();
    let relative_north_ms = track_north_ms - own_north_ms;
    let relative_east_ms = track_east_ms - own_east_ms;
    let relative_up_ms = 0.0 - platform.velocity.vertical_rate_ms;

    (north_m * relative_north_ms + east_m * relative_east_ms + up_m * relative_up_ms) / range_m
}

fn relative_position_m(
    platform: &PlatformState,
    lat_deg: f64,
    lon_deg: f64,
    alt_m: f64,
) -> (f64, f64, f64) {
    let dlat = (lat_deg - platform.pose.lat_deg).to_radians();
    let dlon = (lon_deg - platform.pose.lon_deg).to_radians();
    let mean_lat = ((lat_deg + platform.pose.lat_deg) * 0.5).to_radians();
    let north_m = dlat * 6_371_000.0;
    let east_m = dlon * 6_371_000.0 * mean_lat.cos();
    let up_m = alt_m - platform.pose.alt_m;
    (north_m, east_m, up_m)
}

fn is_controllable_platform(
    platform: &openfang_types::platform::PlatformState,
    policy: &PlatformControlPolicy,
) -> bool {
    if !policy.controlled_side.matches(platform.affiliation) {
        return false;
    }
    if policy.controlled_platforms.is_empty() {
        return true;
    }
    policy
        .controlled_platforms
        .iter()
        .any(|id| id == &platform.id)
}

fn extract_opportunities(
    snapshot: &WorldSnapshot,
    threats: &[ThreatTrack],
    policy: &PlatformControlPolicy,
) -> Vec<EngageOpportunity> {
    let mut opportunities = Vec::new();
    for platform in &snapshot.platforms {
        if !is_controllable_platform(platform, policy) {
            continue;
        }
        let mut weapons: Vec<&WeaponState> = platform
            .onboard_weapons
            .iter()
            .filter(|weapon| weapon_available(weapon))
            .collect();
        weapons.sort_by_key(|weapon| std::cmp::Reverse(recommended_weapon_rank(weapon)));
        for weapon in weapons {
            if !weapon_available(weapon) {
                continue;
            }
            for threat in threats {
                if weapon_can_reach(weapon, threat.distance_m) {
                    opportunities.push(EngageOpportunity {
                        platform_id: platform.id.clone(),
                        weapon_id: weapon.weapon_id.clone(),
                        track_id: threat.track_id.clone(),
                        estimated_p_hit: estimated_p_hit(weapon, threat),
                    });
                }
            }
        }
    }
    opportunities
}

fn weapon_available(weapon: &WeaponState) -> bool {
    weapon.is_ready && weapon.quantity_remaining > 0.0
}

fn recommended_weapon_rank(weapon: &WeaponState) -> u16 {
    let text = format!("{} {}", weapon.weapon_id, weapon.weapon_type).to_ascii_lowercase();
    let base = if text.contains("loiter") || text.contains("munition") || text.contains("mun") {
        500
    } else if text.contains("uav") {
        400
    } else if text.contains("missile") || text.contains("rocket") || text.contains("torpedo") {
        300
    } else if text.contains("gun") || text.contains("bullet") || text.contains("cannon") {
        200
    } else {
        0
    };
    base + trailing_number(&weapon.weapon_id).unwrap_or(0).min(99)
}

fn trailing_number(text: &str) -> Option<u16> {
    let digits: String = text
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    digits.parse().ok()
}

fn weapon_can_reach(weapon: &WeaponState, distance_m: f64) -> bool {
    if !distance_m.is_finite() {
        return false;
    }
    let min = weapon.min_range_m.unwrap_or(0.0);
    let max = weapon.max_range_m.unwrap_or(f64::INFINITY);
    distance_m >= min && distance_m <= max
}

fn estimated_p_hit(weapon: &WeaponState, threat: &ThreatTrack) -> f64 {
    let readiness = if weapon.is_ready { 0.2 } else { 0.0 };
    let quality = threat.threat_score * 0.5;
    let range = weapon
        .max_range_m
        .map(|max| (1.0 - (threat.distance_m / max)).clamp(0.0, 1.0) * 0.3)
        .unwrap_or(0.1);
    (readiness + quality + range).clamp(0.0, 1.0)
}

fn own_force_status(snapshot: &WorldSnapshot, policy: &PlatformControlPolicy) -> OwnForceStatus {
    let own: Vec<_> = snapshot
        .platforms
        .iter()
        .filter(|platform| is_controllable_platform(platform, policy))
        .collect();
    let total = own.len();
    let average_damage = average(total, own.iter().map(|platform| platform.damage));
    let average_fuel_pct = average(
        total,
        own.iter().map(|platform| platform.fuel.remaining_pct()),
    );
    OwnForceStatus {
        total_platforms: total,
        average_damage,
        average_fuel_pct,
        link_status: if total == 0 {
            "lost".into()
        } else {
            "connected".into()
        },
    }
}

fn average(total: usize, values: impl Iterator<Item = f64>) -> f64 {
    if total == 0 {
        return 0.0;
    }
    values.sum::<f64>() / total as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::config::{ControlledSide, PlatformControlPolicy, ThreatSide};
    use openfang_types::platform::{Affiliation, PlatformState, Track, WeaponState, WorldSnapshot};

    fn platform(id: &str, side: Affiliation, with_tracks: bool) -> PlatformState {
        let mut p = PlatformState::minimal(id);
        p.affiliation = side;
        p.onboard_weapons = vec![WeaponState {
            weapon_id: "missile".into(),
            weapon_type: "asm".into(),
            is_ready: true,
            quantity_remaining: 4.0,
            max_range_m: Some(50_000.0),
            min_range_m: None,
            guidance_type: None,
            speed_ms: None,
            quantity_from_snapshot: true,
        }];
        if with_tracks {
            p.tracks = vec![Track {
                track_id: "tgt-1".into(),
                target_name: String::new(),
                classification: "ship".into(),
                affiliation: Affiliation::Red,
                iff: "foe".into(),
                position_lla: None,
                heading_deg: None,
                speed_ms: None,
                range_m: Some(10_000.0),
                bearing_deg: None,
                elevation_deg: None,
                quality: 0.9,
                stale: false,
                last_update_s: 0.0,
                is_active: true,
            }];
        }
        p
    }

    #[test]
    fn default_policy_controls_blue_only() {
        let snapshot = WorldSnapshot {
            timestamp: 0.0,
            platforms: vec![
                platform("blue-1", Affiliation::Blue, true),
                platform("friend-1", Affiliation::Friend, false),
            ],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        };
        let assessment = CognitionEngine::default().assess(&snapshot);
        assert_eq!(assessment.own_force.total_platforms, 1);
        assert_eq!(assessment.opportunities.len(), 1);
        assert_eq!(assessment.opportunities[0].platform_id, "blue-1");
    }

    #[test]
    fn controlled_platforms_allow_list_narrows_tasking() {
        let policy = PlatformControlPolicy {
            controlled_side: ControlledSide::BlueAndFriend,
            controlled_platforms: vec!["friend-1".into()],
            ..Default::default()
        };
        let snapshot = WorldSnapshot {
            timestamp: 0.0,
            platforms: vec![
                platform("blue-1", Affiliation::Blue, true),
                platform("friend-1", Affiliation::Friend, false),
            ],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        };
        let assessment = CognitionEngine::new(policy).assess(&snapshot);
        assert_eq!(assessment.own_force.total_platforms, 1);
        assert_eq!(assessment.opportunities[0].platform_id, "friend-1");
    }

    #[test]
    fn engage_opportunities_keep_all_fireable_weapons_but_rank_recommended_names() {
        let policy = PlatformControlPolicy {
            controlled_side: ControlledSide::Red,
            threat_side: ThreatSide::Opposite,
            ..Default::default()
        };
        let mut own = platform("self", Affiliation::Red, false);
        own.onboard_weapons = vec![
            WeaponState {
                weapon_id: "scout_uav_slot".into(),
                weapon_type: "J7_UAV_WEAPON".into(),
                is_ready: true,
                quantity_remaining: 2.0,
                max_range_m: None,
                min_range_m: None,
                guidance_type: None,
                speed_ms: None,
                quantity_from_snapshot: true,
            },
            WeaponState {
                weapon_id: "gun_30mm".into(),
                weapon_type: "30MM_BULLET".into(),
                is_ready: true,
                quantity_remaining: 1500.0,
                max_range_m: None,
                min_range_m: None,
                guidance_type: None,
                speed_ms: None,
                quantity_from_snapshot: true,
            },
        ];
        own.tracks = vec![Track {
            track_id: "blue_patrol_1".into(),
            target_name: String::new(),
            classification: "patrol_boat".into(),
            affiliation: Affiliation::Blue,
            iff: "unknown".into(),
            position_lla: None,
            heading_deg: None,
            speed_ms: None,
            range_m: Some(5_000.0),
            bearing_deg: None,
            elevation_deg: None,
            quality: 0.9,
            stale: false,
            last_update_s: 0.0,
            is_active: true,
        }];

        let assessment = CognitionEngine::new(policy).assess(&WorldSnapshot {
            timestamp: 0.0,
            platforms: vec![own],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        });

        assert!(assessment
            .opportunities
            .iter()
            .any(|op| op.weapon_id == "scout_uav_slot"));
        assert_eq!(
            assessment
                .opportunities
                .first()
                .map(|op| op.weapon_id.as_str()),
            Some("scout_uav_slot")
        );
        assert!(assessment
            .opportunities
            .iter()
            .any(|op| op.weapon_id == "gun_30mm"));
    }

    #[test]
    fn hostile_iff_track_without_side_is_a_threat() {
        // ArkSIM sensor tracks frequently report an IFF verdict (`foe`) with no
        // ground-truth side, so affiliation maps to Unknown. The threat picture
        // must still flag it — this is the live "威胁=0" regression.
        let policy = PlatformControlPolicy {
            controlled_side: ControlledSide::Red,
            threat_side: ThreatSide::Opposite,
            ..Default::default()
        };
        let mut snap = platform("red-1", Affiliation::Red, false);
        snap.tracks = vec![Track {
            track_id: "iff-only".into(),
            target_name: String::new(),
            classification: "unknown".into(),
            affiliation: Affiliation::Unknown,
            iff: "foe".into(),
            position_lla: None,
            heading_deg: None,
            speed_ms: None,
            range_m: Some(5_000.0),
            bearing_deg: None,
            elevation_deg: None,
            quality: 0.8,
            stale: false,
            last_update_s: 0.0,
            is_active: true,
        }];
        let assessment = CognitionEngine::new(policy).assess(&WorldSnapshot {
            timestamp: 0.0,
            platforms: vec![snap],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        });
        assert_eq!(assessment.threats.len(), 1);
        assert_eq!(assessment.threats[0].track_id, "iff-only");
    }

    #[test]
    fn red_controlled_node_treats_blue_side_tracks_as_threats() {
        // 自身红方 → 蓝方是威胁（与 controlled_side 相反）。
        let policy = PlatformControlPolicy {
            controlled_side: ControlledSide::Red,
            threat_side: ThreatSide::Opposite,
            ..Default::default()
        };
        let mut own = platform("red-1", Affiliation::Red, false);
        own.tracks = vec![Track {
            track_id: "blue-contact".into(),
            target_name: String::new(),
            classification: "patrol_boat".into(),
            affiliation: Affiliation::Blue,
            iff: "unknown".into(),
            position_lla: None,
            heading_deg: None,
            speed_ms: None,
            range_m: Some(9_000.0),
            bearing_deg: None,
            elevation_deg: None,
            quality: 0.9,
            stale: false,
            last_update_s: 0.0,
            is_active: true,
        }];
        let assessment = CognitionEngine::new(policy).assess(&WorldSnapshot {
            timestamp: 0.0,
            platforms: vec![own],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        });
        assert_eq!(assessment.threats.len(), 1);
        assert_eq!(assessment.threats[0].track_id, "blue-contact");
    }

    #[test]
    fn hostile_platform_fallback_uses_nearest_controlled_track_id() {
        let policy = PlatformControlPolicy {
            controlled_side: ControlledSide::Red,
            threat_side: ThreatSide::Opposite,
            ..Default::default()
        };
        let mut own = platform("self", Affiliation::Red, false);
        own.tracks = vec![Track {
            track_id: "xq58a_b1:1".into(),
            target_name: String::new(),
            classification: "patrol_boat".into(),
            affiliation: Affiliation::Unknown,
            iff: "unknown".into(),
            position_lla: Some((20.6, 122.6, 0.0)),
            heading_deg: None,
            speed_ms: None,
            range_m: Some(5_000.0),
            bearing_deg: None,
            elevation_deg: None,
            quality: 0.9,
            stale: false,
            last_update_s: 0.0,
            is_active: true,
        }];
        let mut hostile = platform("blue_patrol_3", Affiliation::Blue, false);
        hostile.pose.lat_deg = 20.60001;
        hostile.pose.lon_deg = 122.60001;
        hostile.platform_type = "BLUE_PATROL_BOAT".into();

        let assessment = CognitionEngine::new(policy).assess(&WorldSnapshot {
            timestamp: 0.0,
            platforms: vec![own, hostile],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        });

        assert_eq!(assessment.threats.len(), 1);
        assert_eq!(assessment.threats[0].track_id, "xq58a_b1:1");
    }

    #[test]
    fn hostile_platform_fallback_uses_ordered_controlled_track_id_without_positions() {
        let policy = PlatformControlPolicy {
            controlled_side: ControlledSide::Red,
            threat_side: ThreatSide::Opposite,
            ..Default::default()
        };
        let mut own = platform("self", Affiliation::Red, false);
        own.tracks = (1..=5)
            .map(|n| Track {
                track_id: format!("self:{n}"),
                target_name: String::new(),
                classification: "unknown".into(),
                affiliation: Affiliation::Unknown,
                iff: "unknown".into(),
                position_lla: None,
                heading_deg: None,
                speed_ms: None,
                range_m: None,
                bearing_deg: None,
                elevation_deg: None,
                quality: 0.5,
                stale: false,
                last_update_s: 0.0,
                is_active: true,
            })
            .collect();

        let mut blue_sam = platform("blue_sam_site_1", Affiliation::Blue, false);
        blue_sam.platform_type = "BLUE_SAM_SITE".into();
        let mut blue_command = platform("blue_command_post_1", Affiliation::Blue, false);
        blue_command.platform_type = "BLUE_COMMAND_POST".into();
        let mut blue_patrol_1 = platform("blue_patrol_1", Affiliation::Blue, false);
        blue_patrol_1.platform_type = "BLUE_PATROL_BOAT".into();
        let mut blue_patrol_2 = platform("blue_patrol_2", Affiliation::Blue, false);
        blue_patrol_2.platform_type = "BLUE_PATROL_BOAT".into();
        let mut blue_patrol_3 = platform("blue_patrol_3", Affiliation::Blue, false);
        blue_patrol_3.platform_type = "BLUE_PATROL_BOAT".into();

        let assessment = CognitionEngine::new(policy).assess(&WorldSnapshot {
            timestamp: 0.0,
            platforms: vec![
                blue_sam,
                blue_command,
                blue_patrol_1,
                blue_patrol_2,
                blue_patrol_3,
                own,
            ],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        });

        let patrol_3 = assessment
            .threats
            .iter()
            .find(|threat| {
                threat.platform_type == "BLUE_PATROL_BOAT" && threat.track_id == "self:5"
            })
            .expect("blue_patrol_3 should map to the fifth ownship track");
        assert_eq!(patrol_3.track_id, "self:5");
    }

    #[test]
    fn closing_rate_is_negative_for_target_moving_toward_ownship() {
        let mut own = platform("blue-1", Affiliation::Blue, false);
        own.pose.lat_deg = 30.0;
        own.pose.lon_deg = 120.0;
        own.velocity.speed_ms = 0.0;
        own.tracks = vec![Track {
            track_id: "incoming".into(),
            target_name: String::new(),
            classification: "missile".into(),
            affiliation: Affiliation::Red,
            iff: "foe".into(),
            position_lla: Some((30.0, 120.01, 0.0)),
            heading_deg: Some(270.0),
            speed_ms: Some(100.0),
            range_m: Some(1_000.0),
            bearing_deg: Some(90.0),
            elevation_deg: None,
            quality: 0.9,
            stale: false,
            last_update_s: 0.0,
            is_active: true,
        }];

        let assessment = CognitionEngine::default().assess(&WorldSnapshot {
            timestamp: 0.0,
            platforms: vec![own],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        });

        assert_eq!(assessment.threats.len(), 1);
        assert!(
            assessment.threats[0].closing_rate_ms < -50.0,
            "closing_rate={}",
            assessment.threats[0].closing_rate_ms
        );
    }
}
