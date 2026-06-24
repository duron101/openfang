//! MAVLink codec — bidirectional mapping between OpenFang's protocol-agnostic
//! types and a (simplified) MAVLink message model.
//!
//! To keep the crate dependency-light and self-testable, MAVLink messages are
//! represented as plain serializable structs rather than pulling in the full
//! `mavlink` crate / dialect generation. The shapes mirror the real MAVLink
//! commands (`SET_POSITION_TARGET_GLOBAL_INT`, `MAV_CMD_DO_CHANGE_SPEED`,
//! `MAV_CMD_NAV_RETURN_TO_LAUNCH`, `GLOBAL_POSITION_INT`) closely enough that a
//! real transport can be slotted in behind [`MavlinkTransport`] later.

use openfang_types::platform::{
    Affiliation, Domain, FuelStatus, PlatformCommand, PlatformState, Pose, Velocity, WorldSnapshot,
};
use serde::{Deserialize, Serialize};

/// A simplified MAVLink uplink frame (autopilot command).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "msg", content = "data")]
pub enum MavFrame {
    /// Reposition / set target heading + speed.
    SetHeading {
        heading_deg: f32,
        speed_ms: Option<f32>,
    },
    /// MAV_CMD_DO_CHANGE_SPEED.
    ChangeSpeed { speed_ms: f32 },
    /// MAV_CMD_DO_CHANGE_ALTITUDE (target altitude in meters).
    ChangeAltitude { altitude_m: f32 },
    /// SET_POSITION_TARGET_GLOBAL_INT (goto).
    Goto {
        lat: f64,
        lon: f64,
        alt_m: Option<f32>,
        speed_ms: Option<f32>,
    },
    /// MAV_CMD_NAV_RETURN_TO_LAUNCH.
    ReturnToLaunch,
    /// MAV_CMD_MISSION_START (a queued waypoint mission).
    MissionStart { waypoint_count: u32 },
}

/// Map a protocol-agnostic command to a MAVLink uplink frame.
///
/// Returns `None` for commands a bare autopilot link cannot honor (weapons,
/// jammers, formation, fleet ops). The adapter reports those as *rejected*
/// rather than silently dropping them — keeping capability reporting honest.
pub fn command_to_mavlink(cmd: &PlatformCommand) -> Option<MavFrame> {
    match cmd {
        PlatformCommand::SetHeading {
            heading_deg,
            speed_ms,
            ..
        } => Some(MavFrame::SetHeading {
            heading_deg: *heading_deg as f32,
            speed_ms: speed_ms.map(|s| s as f32),
        }),
        PlatformCommand::SetSpeed { speed_ms, .. } => Some(MavFrame::ChangeSpeed {
            speed_ms: *speed_ms as f32,
        }),
        PlatformCommand::SetAltitude { altitude_m, .. } => Some(MavFrame::ChangeAltitude {
            altitude_m: *altitude_m as f32,
        }),
        PlatformCommand::GotoLocation {
            lat,
            lon,
            alt,
            speed_ms,
            ..
        } => Some(MavFrame::Goto {
            lat: *lat,
            lon: *lon,
            alt_m: alt.map(|a| a as f32),
            speed_ms: speed_ms.map(|s| s as f32),
        }),
        PlatformCommand::FollowRoute { waypoints, .. } => Some(MavFrame::MissionStart {
            waypoint_count: waypoints.len() as u32,
        }),
        PlatformCommand::ReturnToBase { .. } => Some(MavFrame::ReturnToLaunch),
        // Everything else (weapons, EW, comm, formation, fleet) is not part of
        // a bare MAVLink autopilot contract.
        _ => None,
    }
}

/// Simplified MAVLink downlink telemetry (GLOBAL_POSITION_INT + SYS_STATUS).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MavTelemetry {
    pub system_id: u8,
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub alt_m: f64,
    pub heading_deg: f64,
    pub groundspeed_ms: f64,
    pub climb_ms: f64,
    /// Battery / fuel remaining (0.0..1.0).
    pub energy_remaining_pct: f64,
    pub timestamp_s: f64,
}

impl Default for MavTelemetry {
    fn default() -> Self {
        Self {
            system_id: 1,
            lat_deg: 0.0,
            lon_deg: 0.0,
            alt_m: 0.0,
            heading_deg: 0.0,
            groundspeed_ms: 0.0,
            climb_ms: 0.0,
            energy_remaining_pct: 1.0,
            timestamp_s: 0.0,
        }
    }
}

/// Build a single-platform `WorldSnapshot` from MAVLink telemetry.
pub fn telemetry_to_snapshot(t: &MavTelemetry, platform_id: &str) -> WorldSnapshot {
    let state = PlatformState {
        id: platform_id.to_string(),
        name: format!("mav-{}", t.system_id),
        platform_type: "uav".into(),
        affiliation: Affiliation::Blue,
        domain: Domain::Air,
        pose: Pose {
            lat_deg: t.lat_deg,
            lon_deg: t.lon_deg,
            alt_m: t.alt_m,
            heading_deg: t.heading_deg,
            pitch_deg: 0.0,
            roll_deg: 0.0,
        },
        velocity: Velocity {
            speed_ms: t.groundspeed_ms,
            vertical_rate_ms: t.climb_ms,
            course_deg: t.heading_deg,
        },
        fuel: FuelStatus {
            remaining_kg: t.energy_remaining_pct * 100.0,
            max_kg: 100.0,
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
    WorldSnapshot {
        timestamp: t.timestamp_s,
        platforms: vec![state],
        active_munitions: vec![],
        events: vec![],
        fleet: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn motion_commands_map_to_frames() {
        let c = PlatformCommand::SetHeading {
            platform_id: "uav-1".into(),
            heading_deg: 90.0,
            speed_ms: Some(120.0),
            turn_direction: None,
        };
        assert_eq!(
            command_to_mavlink(&c),
            Some(MavFrame::SetHeading {
                heading_deg: 90.0,
                speed_ms: Some(120.0)
            })
        );

        let rtb = PlatformCommand::ReturnToBase {
            uav_id: "uav-1".into(),
        };
        assert_eq!(command_to_mavlink(&rtb), Some(MavFrame::ReturnToLaunch));
    }

    #[test]
    fn weapon_and_ew_commands_are_unsupported() {
        let fire = PlatformCommand::FireAtTarget {
            platform_id: "uav-1".into(),
            weapon_id: "aam".into(),
            track_id: "trk".into(),
        };
        assert_eq!(command_to_mavlink(&fire), None);

        let jam = PlatformCommand::JamStart {
            platform_id: "uav-1".into(),
            jammer_id: "j1".into(),
            frequency_hz: 1.0,
            bandwidth_hz: 1.0,
            target_track_id: "t".into(),
        };
        assert_eq!(command_to_mavlink(&jam), None);
    }

    #[test]
    fn telemetry_round_trips_into_snapshot() {
        let t = MavTelemetry {
            lat_deg: 30.0,
            lon_deg: 120.0,
            alt_m: 2500.0,
            heading_deg: 45.0,
            groundspeed_ms: 150.0,
            climb_ms: 5.0,
            energy_remaining_pct: 0.8,
            ..Default::default()
        };
        let snap = telemetry_to_snapshot(&t, "uav-1");
        assert_eq!(snap.platforms.len(), 1);
        let p = &snap.platforms[0];
        assert_eq!(p.id, "uav-1");
        assert_eq!(p.domain, Domain::Air);
        assert!((p.pose.alt_m - 2500.0).abs() < 1e-6);
        assert!((p.velocity.vertical_rate_ms - 5.0).abs() < 1e-6);
    }

    #[test]
    fn mavframe_json_round_trip() {
        let f = MavFrame::Goto {
            lat: 30.0,
            lon: 120.0,
            alt_m: Some(1000.0),
            speed_ms: None,
        };
        let j = serde_json::to_string(&f).unwrap();
        let back: MavFrame = serde_json::from_str(&j).unwrap();
        assert_eq!(f, back);
    }
}
