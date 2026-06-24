#![allow(dead_code)]
//! DDS topic reads → WorldSnapshot builder.
//! Constructs a WorldSnapshot from the latest DDS topic samples.

use crate::types::*;
use openfang_platform::PlatformError;
use openfang_types::platform::*;

/// Build a WorldSnapshot from DDS topic data.
pub fn build_snapshot_from_dds(
    nav_data: Option<Vec<u8>>,
    track_data: Option<Vec<u8>>,
    heartbeat_data: Option<Vec<u8>>,
) -> Result<WorldSnapshot, PlatformError> {
    let mut platforms = Vec::new();
    let mut tracks = Vec::new();
    let mut events = Vec::new();

    // Parse nav position
    if let Some(ref data) = nav_data {
        if let Ok(nav) = serde_json::from_slice::<NavPosition>(data) {
            let platform = PlatformState {
                id: nav.platform_id.clone(),
                name: nav.platform_id,
                platform_type: "usv".into(),
                affiliation: Affiliation::Blue,
                domain: Domain::Surface,
                pose: Pose {
                    lat_deg: nav.lat_deg,
                    lon_deg: nav.lon_deg,
                    alt_m: nav.alt_m,
                    heading_deg: nav.heading_deg,
                    pitch_deg: nav.pitch_deg,
                    roll_deg: nav.roll_deg,
                },
                velocity: Velocity {
                    speed_ms: nav.speed_ms,
                    vertical_rate_ms: nav.vertical_rate_ms,
                    course_deg: nav.course_deg,
                },
                fuel: FuelStatus {
                    remaining_kg: 0.0,
                    max_kg: 0.0,
                    consumption_rate_kg_s: 0.0,
                },
                damage: 0.0,
                tracks: vec![],
                onboard_sensors: vec![],
                onboard_weapons: vec![],
                onboard_jammers: vec![],
                current_target: None,
                commander: None,
                survivability: None,
                emcon: None,
                link: None,
            };
            platforms.push(platform);
        }
    }

    // Parse radar tracks
    if let Some(ref data) = track_data {
        if let Ok(radar_track) = serde_json::from_slice::<RadarTrack>(data) {
            let track = Track {
                track_id: radar_track.track_id,
                target_name: String::new(),
                classification: radar_track.classification,
                affiliation: match radar_track.affiliation.as_str() {
                    "friend" => Affiliation::Friend,
                    "foe" => Affiliation::Foe,
                    "neutral" => Affiliation::Neutral,
                    _ => Affiliation::Unknown,
                },
                iff: radar_track.affiliation,
                position_lla: if let (Some(lat), Some(lon), Some(alt)) =
                    (radar_track.lat_deg, radar_track.lon_deg, radar_track.alt_m)
                {
                    Some((lat, lon, alt))
                } else {
                    None
                },
                heading_deg: radar_track.heading_deg,
                speed_ms: radar_track.speed_ms,
                range_m: radar_track.range_m,
                bearing_deg: radar_track.bearing_deg,
                elevation_deg: None,
                quality: radar_track.quality,
                stale: radar_track.stale,
                last_update_s: 0.0,
                is_active: !radar_track.stale,
            };
            tracks.push(track);
        }
    }

    // Attach tracks to first platform
    if let Some(plat) = platforms.first_mut() {
        plat.tracks = tracks;
    }

    // Parse heartbeat into the platform-agnostic health event contract. WorldSnapshot
    // does not yet have a dedicated health field, so DDS liveness/resource data is
    // carried as a discrete event for read-only tools and gate checks.
    if let Some(ref data) = heartbeat_data {
        if let Ok(hb) = serde_json::from_slice::<Heartbeat>(data) {
            events.push(WorldEvent::PlatformHealth {
                platform_id: hb.platform_id,
                uptime_s: hb.uptime_s,
                cpu_pct: hb.cpu_pct as f64,
                mem_mb: hb.mem_mb,
                disk_mb: hb.disk_mb,
                link_quality: hb.link_quality,
                autonomy_mode: hb.autonomy_mode,
            });
        }
    }

    Ok(WorldSnapshot {
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0),
        platforms,
        active_munitions: vec![],
        events,
        fleet: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_dds_data() {
        let snapshot = build_snapshot_from_dds(None, None, None).unwrap();
        assert!(snapshot.platforms.is_empty());
    }

    #[test]
    fn test_nav_position_parsing() {
        let nav = NavPosition {
            platform_id: "usv-01".into(),
            lat_deg: 30.0,
            lon_deg: 120.0,
            alt_m: 0.0,
            heading_deg: 90.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
            speed_ms: 15.0,
            vertical_rate_ms: 0.0,
            course_deg: 90.0,
            nav_source: "gps".into(),
            accuracy_cep_m: 5.0,
            timestamp_us: 0,
        };
        let data = serde_json::to_vec(&nav).unwrap();
        let snapshot = build_snapshot_from_dds(Some(data), None, None).unwrap();
        assert_eq!(snapshot.platforms.len(), 1);
        assert_eq!(snapshot.platforms[0].pose.heading_deg, 90.0);
    }
}
