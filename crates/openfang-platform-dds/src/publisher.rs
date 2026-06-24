//! PlatformCommand → DDS topic publisher.
//! Maps domain commands to DDS topic writes via the DdsTransport trait.

use crate::types::*;
use crate::DdsTransport;
use openfang_types::platform::PlatformCommand;
use tracing::debug;

/// Publish a single PlatformCommand to the appropriate DDS topic.
pub async fn publish_command(
    transport: &dyn DdsTransport,
    cmd: &PlatformCommand,
) -> Result<(), String> {
    match cmd {
        // ── Navigation ──
        PlatformCommand::SetHeading {
            platform_id,
            heading_deg,
            speed_ms,
            ..
        } => {
            let nav_cmd = NavCommand {
                platform_id: platform_id.clone(),
                command_type: NavCommandType::SetHeading,
                target_heading_deg: Some(*heading_deg),
                target_speed_ms: *speed_ms,
                target_altitude_m: None,
                waypoints: vec![],
                sequence_id: 0,
                timestamp_us: now_us(),
            };
            let payload = serde_json::to_vec(&nav_cmd).map_err(|e| e.to_string())?;
            transport
                .publish(
                    "nav/NavCommand",
                    &DdsQosProfile::reliable_keep_last(10),
                    &payload,
                )
                .await
        }

        PlatformCommand::SetSpeed {
            platform_id,
            speed_ms,
            acceleration_ms2: _,
        } => {
            let nav_cmd = NavCommand {
                platform_id: platform_id.clone(),
                command_type: NavCommandType::SetSpeed,
                target_heading_deg: None,
                target_speed_ms: Some(*speed_ms),
                target_altitude_m: None,
                waypoints: vec![],
                sequence_id: 0,
                timestamp_us: now_us(),
            };
            let payload = serde_json::to_vec(&nav_cmd).map_err(|e| e.to_string())?;
            transport
                .publish(
                    "nav/NavCommand",
                    &DdsQosProfile::reliable_keep_last(10),
                    &payload,
                )
                .await
        }

        PlatformCommand::GotoLocation {
            platform_id,
            lat,
            lon,
            alt,
            speed_ms,
        } => {
            let nav_cmd = NavCommand {
                platform_id: platform_id.clone(),
                command_type: NavCommandType::GotoLocation,
                target_heading_deg: None,
                target_speed_ms: *speed_ms,
                target_altitude_m: *alt,
                waypoints: vec![DdsWaypoint {
                    lat: *lat,
                    lon: *lon,
                    alt: *alt,
                    speed_ms: *speed_ms,
                }],
                sequence_id: 0,
                timestamp_us: now_us(),
            };
            let payload = serde_json::to_vec(&nav_cmd).map_err(|e| e.to_string())?;
            transport
                .publish(
                    "nav/NavCommand",
                    &DdsQosProfile::reliable_keep_last(10),
                    &payload,
                )
                .await
        }

        // ── Sensors ──
        PlatformCommand::SensorOn {
            platform_id,
            sensor_id,
        } => {
            let cmd = SensorCommand {
                platform_id: platform_id.clone(),
                sensor_id: sensor_id.clone(),
                command: SensorCmdType::TurnOn,
                mode: None,
                timestamp_us: now_us(),
            };
            let payload = serde_json::to_vec(&cmd).map_err(|e| e.to_string())?;
            transport
                .publish(
                    "sensor/SensorCommand",
                    &DdsQosProfile::best_effort_keep_last(5),
                    &payload,
                )
                .await
        }
        PlatformCommand::SensorOff {
            platform_id,
            sensor_id,
        } => {
            let cmd = SensorCommand {
                platform_id: platform_id.clone(),
                sensor_id: sensor_id.clone(),
                command: SensorCmdType::TurnOff,
                mode: None,
                timestamp_us: now_us(),
            };
            let payload = serde_json::to_vec(&cmd).map_err(|e| e.to_string())?;
            transport
                .publish(
                    "sensor/SensorCommand",
                    &DdsQosProfile::best_effort_keep_last(5),
                    &payload,
                )
                .await
        }
        PlatformCommand::SensorSetMode {
            platform_id,
            sensor_id,
            mode,
        } => {
            let cmd = SensorCommand {
                platform_id: platform_id.clone(),
                sensor_id: sensor_id.clone(),
                command: SensorCmdType::SetMode,
                mode: Some(mode.clone()),
                timestamp_us: now_us(),
            };
            let payload = serde_json::to_vec(&cmd).map_err(|e| e.to_string())?;
            transport
                .publish(
                    "sensor/SensorCommand",
                    &DdsQosProfile::best_effort_keep_last(5),
                    &payload,
                )
                .await
        }

        // ── Weapons ──
        PlatformCommand::FireAtTarget {
            platform_id,
            weapon_id,
            track_id,
        } => {
            let cmd = WeaponCommand {
                platform_id: platform_id.clone(),
                weapon_id: weapon_id.clone(),
                command: WeaponCmdType::FireAtTarget,
                track_id: Some(track_id.clone()),
                salvo_size: None,
                params: vec![],
                authorization_token: String::new(), // filled by ApprovalManager
                timestamp_us: now_us(),
            };
            let payload = serde_json::to_vec(&cmd).map_err(|e| e.to_string())?;
            transport
                .publish(
                    "weapon/WeaponCommand",
                    &DdsQosProfile::reliable_transient_local(),
                    &payload,
                )
                .await
        }

        PlatformCommand::FireSalvo {
            platform_id,
            weapon_id,
            track_id,
            salvo_size,
        } => {
            let cmd = WeaponCommand {
                platform_id: platform_id.clone(),
                weapon_id: weapon_id.clone(),
                command: WeaponCmdType::FireSalvo,
                track_id: Some(track_id.clone()),
                salvo_size: Some(*salvo_size),
                params: vec![],
                authorization_token: String::new(),
                timestamp_us: now_us(),
            };
            let payload = serde_json::to_vec(&cmd).map_err(|e| e.to_string())?;
            transport
                .publish(
                    "weapon/WeaponCommand",
                    &DdsQosProfile::reliable_transient_local(),
                    &payload,
                )
                .await
        }

        PlatformCommand::FireChaff {
            platform_id,
            weapon_id,
            count,
            interval_s,
        } => {
            let cmd = WeaponCommand {
                platform_id: platform_id.clone(),
                weapon_id: weapon_id.clone(),
                command: WeaponCmdType::FireChaff,
                track_id: None,
                salvo_size: None,
                params: vec![*count as f64, *interval_s],
                authorization_token: String::new(),
                timestamp_us: now_us(),
            };
            let payload = serde_json::to_vec(&cmd).map_err(|e| e.to_string())?;
            transport
                .publish(
                    "weapon/WeaponCommand",
                    &DdsQosProfile::reliable_transient_local(),
                    &payload,
                )
                .await
        }

        // ── Fleet / UAV operations (Track 2 §2B) ──
        PlatformCommand::LaunchUav { uav_id } => {
            publish_fleet(
                transport,
                FleetCommand {
                    command: FleetCmdType::LaunchUav,
                    uav_id: uav_id.clone(),
                    from_platform_id: None,
                    mission_type: None,
                    params_json: None,
                    track_id: None,
                    timestamp_us: now_us(),
                },
            )
            .await
        }
        PlatformCommand::RecoverUav { uav_id } => {
            publish_fleet(
                transport,
                FleetCommand {
                    command: FleetCmdType::RecoverUav,
                    uav_id: uav_id.clone(),
                    from_platform_id: None,
                    mission_type: None,
                    params_json: None,
                    track_id: None,
                    timestamp_us: now_us(),
                },
            )
            .await
        }
        PlatformCommand::ReturnToBase { uav_id } => {
            publish_fleet(
                transport,
                FleetCommand {
                    command: FleetCmdType::ReturnToBase,
                    uav_id: uav_id.clone(),
                    from_platform_id: None,
                    mission_type: None,
                    params_json: None,
                    track_id: None,
                    timestamp_us: now_us(),
                },
            )
            .await
        }
        PlatformCommand::AssignMission {
            uav_id,
            mission_type,
            params_json,
        } => {
            publish_fleet(
                transport,
                FleetCommand {
                    command: FleetCmdType::AssignMission,
                    uav_id: uav_id.clone(),
                    from_platform_id: None,
                    mission_type: Some(mission_type.clone()),
                    params_json: Some(params_json.clone()),
                    track_id: None,
                    timestamp_us: now_us(),
                },
            )
            .await
        }
        PlatformCommand::AbortMission { uav_id } => {
            publish_fleet(
                transport,
                FleetCommand {
                    command: FleetCmdType::AbortMission,
                    uav_id: uav_id.clone(),
                    from_platform_id: None,
                    mission_type: None,
                    params_json: None,
                    track_id: None,
                    timestamp_us: now_us(),
                },
            )
            .await
        }
        PlatformCommand::HandoffTarget {
            from_platform_id,
            to_platform_id,
            track_id,
        } => {
            publish_fleet(
                transport,
                FleetCommand {
                    command: FleetCmdType::HandoffTarget,
                    uav_id: to_platform_id.clone(),
                    from_platform_id: Some(from_platform_id.clone()),
                    mission_type: None,
                    params_json: None,
                    track_id: Some(track_id.clone()),
                    timestamp_us: now_us(),
                },
            )
            .await
        }

        // ── Other commands → passed through as aux topics ──
        other => {
            debug!(
                "DDS: unsupported command type {:?}, publishing as aux",
                other
            );
            let payload = serde_json::to_vec(other).map_err(|e| e.to_string())?;
            transport
                .publish(
                    "platform/AuxCommand",
                    &DdsQosProfile::best_effort_keep_last(1),
                    &payload,
                )
                .await
        }
    }
}

/// Publish a fleet command to the `fleet/FleetCommand` topic (reliable +
/// transient-local so a late-joining child still receives its standing tasking).
async fn publish_fleet(transport: &dyn DdsTransport, cmd: FleetCommand) -> Result<(), String> {
    let payload = serde_json::to_vec(&cmd).map_err(|e| e.to_string())?;
    transport
        .publish(
            "fleet/FleetCommand",
            &DdsQosProfile::reliable_transient_local(),
            &payload,
        )
        .await
}

/// Get microseconds since epoch (monotonic for topic timestamps).
fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}
