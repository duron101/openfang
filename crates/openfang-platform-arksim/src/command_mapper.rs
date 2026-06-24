//! PlatformCommand → protobuf bytes mapping.
//! Uses hand-coded protobuf encoder (proto_manual) for ArkSIM 4.1 wire format.

use crate::proto_manual::{self, ProtoWriter};
use crate::track_id::normalize_track_id;
use openfang_types::platform::PlatformCommand;

// ArkSim E_Actions enum values (see arksimActions.proto).
const E_SET_OUTSIDE_CONTROL: i32 = 0;
const E_RELEASE_OUTSIDE_CONTROL: i32 = 1;
const E_TURN_ON_SENSOR: i32 = 8;
const E_TURN_OFF_SENSOR: i32 = 9;
const E_FIRE_AT_TARGET: i32 = 12;
const E_UPDATE_TARGET: i32 = 14;
const E_STOP_JAMMING: i32 = 16;
// E_TurnOnComm (18) / E_TurnOffComm (19) deliberately unused: CommOn/CommOff are
// not forwarded to ArkSIM (empty comm component crashes AFSIM); see to_proto_bytes.

// ActionsFromOutside field numbers.
const F_AGENT_CONTRL: u32 = 1;
const F_DESIRED_HEADING: u32 = 2;
const F_DESIRED_ALTITUDE: u32 = 3;
const F_DESIRED_VELOCITY: u32 = 4;
const F_GOTO_LOCATION: u32 = 5;
const F_FOLLOW_ROUTE: u32 = 6;
const F_SENSOR_ACTION: u32 = 7;
const F_CHANGE_SENSOR_MODE: u32 = 8;
const F_FIRE_AT_TARGET: u32 = 9;
const F_FIRE_SALVO: u32 = 10;
const F_CHANGE_JAMMING_MODE: u32 = 11;
const F_SEND_MSG_TO_PLATFORM: u32 = 12;
// ActionsFromOutside field for AfsimAuxCommand. Aux is not forwarded to AFSIM
// (see is_supported), but the wire field number is kept for reference.
#[allow(dead_code)]
const F_AUX_COMMAND: u32 = 14;
const F_CHANGE_COMMANDER: u32 = 15;

/// Whether a [`PlatformCommand`] has an ArkSim protobuf mapping.
pub fn is_supported(cmd: &PlatformCommand) -> bool {
    use PlatformCommand::*;
    matches!(
        cmd,
        SetOutsideControl { .. }
            | ReleaseOutsideControl { .. }
            | SetHeading { .. }
            | SetAltitude { .. }
            | SetSpeed { .. }
            | GotoLocation { .. }
            | FollowRoute { .. }
            | SensorOn { .. }
            | SensorOff { .. }
            | SensorSetMode { .. }
            | FireAtTarget { .. }
            | FireSalvo { .. }
            | UpdateTarget { .. }
            | JamStart { .. }
            | JamStop { .. }
            | JamSetMode { .. }
            | SendMessage { .. }
            | ChangeCommander { .. } // AuxCommand intentionally NOT supported: OpenFang emits aux commands as
                                     // internal coordination signals (e.g. {platform:"fma", key:"uav_lost"} or
                                     // the "self" alias) whose names are NOT real AFSIM platforms. Forwarding
                                     // them makes the WSF_ZMQ_PROCESSOR fail platform lookup ("Attempted to set
                                     // aux data but platform was null. name=self") and can crash Warlock. Aux is
                                     // handled internally by OpenFang, never pushed to the simulator.
    )
}

/// Convert PlatformCommands to raw protobuf bytes (ActionsFromOutside).
///
/// Commands without an ArkSim equivalent (UAV launch/recovery, formation,
/// coordinated strike, deck ops, …) are skipped here — they are rejected
/// upstream by the capability gate, so they never reach a real actuator.
pub fn to_proto_bytes(commands: &[PlatformCommand]) -> Vec<u8> {
    use PlatformCommand::*;
    let mut w = ProtoWriter::new();

    for cmd in commands {
        match cmd {
            SetOutsideControl { platform_id } => {
                w.field_message(
                    F_AGENT_CONTRL,
                    &proto_manual::encode_agent_contrl(E_SET_OUTSIDE_CONTROL, platform_id),
                );
            }
            ReleaseOutsideControl { platform_id } => {
                w.field_message(
                    F_AGENT_CONTRL,
                    &proto_manual::encode_agent_contrl(E_RELEASE_OUTSIDE_CONTROL, platform_id),
                );
            }
            SetHeading {
                platform_id,
                heading_deg,
                speed_ms,
                turn_direction,
            } => {
                let turn = turn_direction.map(|t| match t {
                    openfang_types::platform::TurnDirection::Left => 0u32,
                    openfang_types::platform::TurnDirection::Right => 1u32,
                    openfang_types::platform::TurnDirection::Shortest => 2u32,
                });
                let msg = proto_manual::encode_desired_heading_full(
                    platform_id,
                    heading_deg.to_radians(),
                    *speed_ms,
                    turn,
                );
                w.field_message(F_DESIRED_HEADING, &msg);
            }
            SetAltitude {
                platform_id,
                altitude_m,
                rate_ms,
            } => {
                w.field_message(
                    F_DESIRED_ALTITUDE,
                    &proto_manual::encode_desired_altitude(platform_id, *altitude_m, *rate_ms),
                );
            }
            SetSpeed {
                platform_id,
                speed_ms,
                acceleration_ms2,
            } => {
                w.field_message(
                    F_DESIRED_VELOCITY,
                    &proto_manual::encode_desired_velocity(
                        platform_id,
                        *speed_ms,
                        *acceleration_ms2,
                    ),
                );
            }
            GotoLocation {
                platform_id,
                lat,
                lon,
                alt,
                ..
            } => {
                w.field_message(
                    F_GOTO_LOCATION,
                    &proto_manual::encode_goto_location(
                        platform_id,
                        *lat,
                        *lon,
                        alt.unwrap_or(0.0),
                    ),
                );
            }
            FollowRoute {
                platform_id,
                waypoints,
            } => {
                let wps: Vec<(String, f64, f64, f64, f64)> = waypoints
                    .iter()
                    .map(|wp| {
                        (
                            String::new(),
                            wp.speed_ms.unwrap_or(0.0),
                            wp.lat,
                            wp.lon,
                            wp.alt.unwrap_or(0.0),
                        )
                    })
                    .collect();
                w.field_message(
                    F_FOLLOW_ROUTE,
                    &proto_manual::encode_follow_route(platform_id, "route", &wps),
                );
            }
            SensorOn {
                platform_id,
                sensor_id,
            } => {
                w.field_message(
                    F_SENSOR_ACTION,
                    &proto_manual::encode_sensor_action(E_TURN_ON_SENSOR, platform_id, sensor_id),
                );
            }
            SensorOff {
                platform_id,
                sensor_id,
            } => {
                w.field_message(
                    F_SENSOR_ACTION,
                    &proto_manual::encode_sensor_action(E_TURN_OFF_SENSOR, platform_id, sensor_id),
                );
            }
            SensorSetMode {
                platform_id,
                sensor_id,
                mode,
            } => {
                w.field_message(
                    F_CHANGE_SENSOR_MODE,
                    &proto_manual::encode_change_sensor_mode(platform_id, sensor_id, mode),
                );
            }
            UpdateTarget {
                platform_id,
                track_id,
            } => {
                let track_id = normalize_track_id(track_id);
                w.field_message(
                    F_SENSOR_ACTION,
                    &proto_manual::encode_fire_at_target(
                        E_UPDATE_TARGET,
                        platform_id,
                        "",
                        &track_id,
                    ),
                );
            }
            FireAtTarget {
                platform_id,
                weapon_id,
                track_id,
            } => {
                let track_id = normalize_track_id(track_id);
                w.field_message(
                    F_FIRE_AT_TARGET,
                    &proto_manual::encode_fire_at_target(
                        E_FIRE_AT_TARGET,
                        platform_id,
                        weapon_id,
                        &track_id,
                    ),
                );
            }
            FireSalvo {
                platform_id,
                weapon_id,
                track_id,
                salvo_size,
            } => {
                let track_id = normalize_track_id(track_id);
                w.field_message(
                    F_FIRE_SALVO,
                    &proto_manual::encode_fire_salvo(
                        platform_id,
                        weapon_id,
                        &track_id,
                        *salvo_size,
                    ),
                );
            }
            JamStart {
                platform_id,
                jammer_id,
                frequency_hz,
                bandwidth_hz,
                ..
            } => {
                w.field_message(
                    F_CHANGE_JAMMING_MODE,
                    &proto_manual::encode_change_jamming_mode(
                        platform_id,
                        jammer_id,
                        *frequency_hz,
                        *bandwidth_hz,
                        0,
                    ),
                );
            }
            JamSetMode {
                platform_id,
                jammer_id,
                frequency_hz,
                bandwidth_hz,
            } => {
                w.field_message(
                    F_CHANGE_JAMMING_MODE,
                    &proto_manual::encode_change_jamming_mode(
                        platform_id,
                        jammer_id,
                        frequency_hz.unwrap_or(0.0),
                        bandwidth_hz.unwrap_or(0.0),
                        0,
                    ),
                );
            }
            JamStop {
                platform_id,
                jammer_id,
            } => {
                w.field_message(
                    F_SENSOR_ACTION,
                    &proto_manual::encode_sensor_action(E_STOP_JAMMING, platform_id, jammer_id),
                );
            }
            // CommOn/CommOff intentionally NOT mapped: the variants carry no comm
            // component id, and AFSIM's WSF_ZMQ_PROCESSOR null-derefs
            // (WsfPlatformPartEvent::Execute) on E_TurnOnComm/E_TurnOffComm with an
            // empty Component_id. Comms are managed by the scenario (radios start
            // `on`), and we have no comm telemetry to name one — so skip them
            // rather than crash Warlock. (See is_supported: also excluded there.)
            SendMessage {
                from_platform_id,
                to_platform_id,
                message,
            } => {
                w.field_message(
                    F_SEND_MSG_TO_PLATFORM,
                    &proto_manual::encode_send_msg_to_platform(
                        from_platform_id,
                        to_platform_id,
                        message,
                    ),
                );
            }
            ChangeCommander {
                platform_id,
                new_commander_id,
            } => {
                w.field_message(
                    F_CHANGE_COMMANDER,
                    &proto_manual::encode_change_commander(platform_id, new_commander_id),
                );
            }
            // AuxCommand intentionally NOT mapped — see is_supported(): aux
            // carries OpenFang-internal coordination (fma/uav_lost, "self"
            // alias) that AFSIM cannot resolve and crashes Warlock on.
            other => {
                tracing::debug!(
                    "ArkSIM adapter: command {:?} has no ArkSim mapping; skipped",
                    other.command_class()
                );
            }
        }
    }

    w.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::platform::Waypoint;

    #[test]
    fn test_set_outside_control() {
        let cmds = vec![PlatformCommand::SetOutsideControl {
            platform_id: "Flight_01".into(),
        }];
        let bytes = to_proto_bytes(&cmds);
        // Should be ActionsFromOutside with field 1 (a_agentcontrl)
        assert!(bytes.len() > 5);
        assert_eq!(bytes[0], 0x0a); // field 1 LEN
    }

    #[test]
    fn set_outside_control_self_matches_python_fire_probe() {
        // protobuf/test_warlock_tcp_fire.py logs this as 8 bytes:
        // ProtoStringBuilder().set_agent_outside_control("self")
        let cmds = vec![PlatformCommand::SetOutsideControl {
            platform_id: "self".into(),
        }];
        let bytes = to_proto_bytes(&cmds);
        assert_eq!(bytes, [0x0a, 0x06, 0x12, 0x04, 0x73, 0x65, 0x6c, 0x66]);
    }

    #[test]
    fn test_set_heading() {
        let cmds = vec![PlatformCommand::SetHeading {
            platform_id: "Flight_01".into(),
            heading_deg: 90.0,
            speed_ms: None,
            turn_direction: None,
        }];
        let bytes = to_proto_bytes(&cmds);
        // Should be ActionsFromOutside with field 2 (a_desiredheading)
        assert!(bytes.len() > 10);
        assert_eq!(bytes[0], 0x12); // field 2 LEN
    }

    #[test]
    fn test_fire_at_target_maps_to_field_9() {
        let cmds = vec![PlatformCommand::FireAtTarget {
            platform_id: "usv-01".into(),
            weapon_id: "cannon".into(),
            track_id: "trk-1".into(),
        }];
        let bytes = to_proto_bytes(&cmds);
        // field 9 LEN tag = (9<<3)|2 = 74 = 0x4a
        assert_eq!(bytes[0], 0x4a);
    }

    #[test]
    fn fire_at_target_matches_arkcmd_reference_bytes() {
        // Generated by protobuf/arkcmd ProtoStringBuilder:
        // fire_at_target("self", "gun_30mm", "xq58a_b1:1").SerializeToString()
        let cmds = vec![PlatformCommand::FireAtTarget {
            platform_id: "self".into(),
            weapon_id: "gun_30mm".into(),
            track_id: "xq58a_b1:1".into(),
        }];
        let bytes = to_proto_bytes(&cmds);
        let expected = [
            0x4a, 0x20, 0x08, 0x0c, 0x12, 0x10, 0x0a, 0x04, 0x73, 0x65, 0x6c, 0x66, 0x12, 0x08,
            0x67, 0x75, 0x6e, 0x5f, 0x33, 0x30, 0x6d, 0x6d, 0x1a, 0x0a, 0x78, 0x71, 0x35, 0x38,
            0x61, 0x5f, 0x62, 0x31, 0x3a, 0x31,
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn fire_at_target_self_loiter_wave2_matches_python_fire_probe() {
        // protobuf/test_warlock_tcp_fire.py logs this as 34 bytes:
        // fire_at_target("self", "loiter_wave2", "self:1")
        let cmds = vec![PlatformCommand::FireAtTarget {
            platform_id: "self".into(),
            weapon_id: "loiter_wave2".into(),
            track_id: "self:1".into(),
        }];
        let bytes = to_proto_bytes(&cmds);
        let expected = [
            0x4a, 0x20, 0x08, 0x0c, 0x12, 0x14, 0x0a, 0x04, 0x73, 0x65, 0x6c, 0x66, 0x12, 0x0c,
            0x6c, 0x6f, 0x69, 0x74, 0x65, 0x72, 0x5f, 0x77, 0x61, 0x76, 0x65, 0x32, 0x1a, 0x06,
            0x73, 0x65, 0x6c, 0x66, 0x3a, 0x31,
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn fire_at_target_normalizes_evt_style_track_id_at_final_mapping() {
        let cmds = vec![PlatformCommand::FireAtTarget {
            platform_id: "self".into(),
            weapon_id: "loiter_wave2".into(),
            track_id: "self.1".into(),
        }];
        let bytes = to_proto_bytes(&cmds);
        assert!(
            bytes
                .windows(b"self:1".len())
                .any(|window| window == b"self:1"),
            "final protobuf mapping must normalize self.1 to self:1"
        );
    }

    #[test]
    fn test_salvo_maps_to_field_10() {
        let cmds = vec![PlatformCommand::FireSalvo {
            platform_id: "usv-01".into(),
            weapon_id: "vls".into(),
            track_id: "trk-2".into(),
            salvo_size: 4,
        }];
        let bytes = to_proto_bytes(&cmds);
        // field 10 LEN tag = (10<<3)|2 = 82 = 0x52
        assert_eq!(bytes[0], 0x52);
    }

    #[test]
    fn fire_salvo_self_loiter_wave3_matches_python_salvo_probe() {
        // Generated by protobuf/test_warlock_tcp_fire.py:
        // fire_salvo_at_target("self", "loiter_wave3", "self:10", 2)
        let cmds = vec![PlatformCommand::FireSalvo {
            platform_id: "self".into(),
            weapon_id: "loiter_wave3".into(),
            track_id: "self:10".into(),
            salvo_size: 2,
        }];
        let bytes = to_proto_bytes(&cmds);
        let expected = [
            0x52, 0x21, 0x0a, 0x14, 0x0a, 0x04, 0x73, 0x65, 0x6c, 0x66, 0x12, 0x0c, 0x6c, 0x6f,
            0x69, 0x74, 0x65, 0x72, 0x5f, 0x77, 0x61, 0x76, 0x65, 0x33, 0x12, 0x07, 0x73, 0x65,
            0x6c, 0x66, 0x3a, 0x31, 0x30, 0x18, 0x02,
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn test_unsupported_command_skipped() {
        let cmds = vec![PlatformCommand::LaunchUav {
            uav_id: "uav-1".into(),
        }];
        let bytes = to_proto_bytes(&cmds);
        assert!(bytes.is_empty(), "UAV launch has no ArkSim mapping");
        assert!(!is_supported(&cmds[0]));
        assert!(is_supported(&PlatformCommand::FireAtTarget {
            platform_id: "a".into(),
            weapon_id: "b".into(),
            track_id: "c".into(),
        }));
    }

    #[test]
    fn supported_command_protocol_matrix_has_expected_top_level_fields() {
        let cases: Vec<(PlatformCommand, u8)> = vec![
            (
                PlatformCommand::SetOutsideControl {
                    platform_id: "p1".into(),
                },
                0x0a,
            ),
            (
                PlatformCommand::ReleaseOutsideControl {
                    platform_id: "p1".into(),
                },
                0x0a,
            ),
            (
                PlatformCommand::SetHeading {
                    platform_id: "p1".into(),
                    heading_deg: 90.0,
                    speed_ms: Some(50.0),
                    turn_direction: None,
                },
                0x12,
            ),
            (
                PlatformCommand::SetAltitude {
                    platform_id: "p1".into(),
                    altitude_m: 1000.0,
                    rate_ms: Some(5.0),
                },
                0x1a,
            ),
            (
                PlatformCommand::SetSpeed {
                    platform_id: "p1".into(),
                    speed_ms: 80.0,
                    acceleration_ms2: Some(2.0),
                },
                0x22,
            ),
            (
                PlatformCommand::GotoLocation {
                    platform_id: "p1".into(),
                    lat: 30.0,
                    lon: 120.0,
                    alt: Some(1000.0),
                    speed_ms: None,
                },
                0x2a,
            ),
            (
                PlatformCommand::FollowRoute {
                    platform_id: "p1".into(),
                    waypoints: vec![Waypoint {
                        lat: 30.0,
                        lon: 120.0,
                        alt: Some(1000.0),
                        speed_ms: Some(50.0),
                    }],
                },
                0x32,
            ),
            (
                PlatformCommand::SensorOn {
                    platform_id: "p1".into(),
                    sensor_id: "s1".into(),
                },
                0x3a,
            ),
            (
                PlatformCommand::SensorOff {
                    platform_id: "p1".into(),
                    sensor_id: "s1".into(),
                },
                0x3a,
            ),
            (
                PlatformCommand::SensorSetMode {
                    platform_id: "p1".into(),
                    sensor_id: "s1".into(),
                    mode: "track".into(),
                },
                0x42,
            ),
            (
                PlatformCommand::UpdateTarget {
                    platform_id: "p1".into(),
                    track_id: "trk-1".into(),
                },
                0x3a,
            ),
            (
                PlatformCommand::FireAtTarget {
                    platform_id: "p1".into(),
                    weapon_id: "w1".into(),
                    track_id: "trk-1".into(),
                },
                0x4a,
            ),
            (
                PlatformCommand::FireSalvo {
                    platform_id: "p1".into(),
                    weapon_id: "w1".into(),
                    track_id: "trk-1".into(),
                    salvo_size: 2,
                },
                0x52,
            ),
            (
                PlatformCommand::JamStart {
                    platform_id: "p1".into(),
                    jammer_id: "j1".into(),
                    frequency_hz: 1.0,
                    bandwidth_hz: 2.0,
                    target_track_id: "trk-1".into(),
                },
                0x5a,
            ),
            (
                PlatformCommand::JamStop {
                    platform_id: "p1".into(),
                    jammer_id: "j1".into(),
                },
                0x3a,
            ),
            (
                PlatformCommand::JamSetMode {
                    platform_id: "p1".into(),
                    jammer_id: "j1".into(),
                    frequency_hz: Some(1.0),
                    bandwidth_hz: Some(2.0),
                },
                0x5a,
            ),
            // CommOn/CommOff are intentionally NOT supported by the ArkSIM adapter
            // (empty comm component crashes AFSIM); covered by
            // comm_commands_are_not_forwarded_to_arksim below.
            (
                PlatformCommand::SendMessage {
                    from_platform_id: "p1".into(),
                    to_platform_id: "p2".into(),
                    message: "hello".into(),
                },
                0x62,
            ),
            (
                PlatformCommand::ChangeCommander {
                    platform_id: "p1".into(),
                    new_commander_id: "cmdr".into(),
                },
                0x7a,
            ),
            // AuxCommand is intentionally NOT supported by the ArkSIM adapter
            // (OpenFang-internal coordination names crash AFSIM); covered by
            // aux_commands_are_not_forwarded_to_arksim below.
        ];

        for (cmd, expected_tag) in cases {
            assert!(is_supported(&cmd), "case should be supported: {cmd:?}");
            let bytes = to_proto_bytes(std::slice::from_ref(&cmd));
            assert!(
                !bytes.is_empty(),
                "supported command yielded empty payload: {cmd:?}"
            );
            assert_eq!(
                bytes[0], expected_tag,
                "unexpected ActionsFromOutside top-level field for {cmd:?}"
            );
        }
    }

    #[test]
    fn comm_commands_are_not_forwarded_to_arksim() {
        // E_TurnOnComm/E_TurnOffComm with an empty comm component crash AFSIM
        // (null deref in WsfPlatformPartEvent::Execute). CommOn/CommOff carry no
        // comm id, so the adapter must drop them entirely.
        for cmd in [
            PlatformCommand::CommOn {
                platform_id: "self".into(),
            },
            PlatformCommand::CommOff {
                platform_id: "self".into(),
            },
        ] {
            assert!(
                !is_supported(&cmd),
                "comm command must be unsupported: {cmd:?}"
            );
            assert!(
                to_proto_bytes(std::slice::from_ref(&cmd)).is_empty(),
                "comm command must produce no wire bytes: {cmd:?}"
            );
        }
    }

    #[test]
    fn aux_commands_are_not_forwarded_to_arksim() {
        // Aux carries OpenFang-internal coordination (e.g. {platform:"fma",
        // key:"uav_lost"} or the "self" alias) whose names are NOT real AFSIM
        // platforms. Forwarding them makes the WSF_ZMQ_PROCESSOR null-deref
        // ("Attempted to set aux data but platform was null. name=self").
        for cmd in [
            PlatformCommand::AuxCommand {
                platform_id: "self".into(),
                key: "uav_lost".into(),
                value_json: "{}".into(),
            },
            PlatformCommand::AuxCommand {
                platform_id: "fma".into(),
                key: "k".into(),
                value_json: "{}".into(),
            },
        ] {
            assert!(
                !is_supported(&cmd),
                "aux command must be unsupported: {cmd:?}"
            );
            assert!(
                to_proto_bytes(std::slice::from_ref(&cmd)).is_empty(),
                "aux command must produce no wire bytes: {cmd:?}"
            );
        }
    }

    #[test]
    fn test_control_and_heading() {
        let cmds = vec![
            PlatformCommand::SetOutsideControl {
                platform_id: "Flight_01".into(),
            },
            PlatformCommand::SetHeading {
                platform_id: "Flight_01".into(),
                heading_deg: 90.0,
                speed_ms: Some(50.0),
                turn_direction: None,
            },
        ];
        let bytes = to_proto_bytes(&cmds);
        assert!(bytes.len() > 20);
    }
}
