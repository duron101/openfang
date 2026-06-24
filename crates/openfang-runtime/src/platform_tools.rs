//! Platform tools — protocol-agnostic control tools for simulation and hardware platforms.
//!
//! These tools allow an Agent (LLM) to interact with the external world
//! through the PlatformAdapter trait, without knowing which backend is active
//! (ArkSIM protobuf, DDS UDP, CAN bus, etc.).

use openfang_types::platform::{PlatformCommand, TurnDirection, Waypoint};
use openfang_types::tactical::{CandidateIntent, CommandPriority, IntentSource};
use openfang_types::tool::ToolDefinition;

/// Whether a tool name belongs to the platform control surface.
pub fn is_platform_tool(name: &str) -> bool {
    name.starts_with("platform_")
}

// ── JSON argument extractors (RORO-style, early-return on error) ──

fn req_str(args: &serde_json::Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing required string param '{key}'"))
}

fn opt_str(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn req_f64(args: &serde_json::Value, key: &str) -> Result<f64, String> {
    args.get(key)
        .and_then(|v| v.as_f64())
        .ok_or_else(|| format!("missing required number param '{key}'"))
}

fn opt_f64(args: &serde_json::Value, key: &str) -> Option<f64> {
    args.get(key).and_then(|v| v.as_f64())
}

fn req_u32(args: &serde_json::Value, key: &str) -> Result<u32, String> {
    args.get(key)
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
        .ok_or_else(|| format!("missing required integer param '{key}'"))
}

fn req_u64(args: &serde_json::Value, key: &str) -> Result<u64, String> {
    args.get(key)
        .and_then(|v| v.as_u64())
        .ok_or_else(|| format!("missing required integer param '{key}'"))
}

fn parse_turn(args: &serde_json::Value) -> Option<TurnDirection> {
    match args.get("turn_direction").and_then(|v| v.as_str()) {
        Some("left") => Some(TurnDirection::Left),
        Some("right") => Some(TurnDirection::Right),
        Some("shortest") => Some(TurnDirection::Shortest),
        _ => None,
    }
}

fn parse_waypoints(args: &serde_json::Value) -> Result<Vec<Waypoint>, String> {
    let arr = args
        .get("waypoints")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "missing required array param 'waypoints'".to_string())?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, wp) in arr.iter().enumerate() {
        let lat = wp
            .get("lat")
            .and_then(|v| v.as_f64())
            .ok_or_else(|| format!("waypoint[{i}] missing 'lat'"))?;
        let lon = wp
            .get("lon")
            .and_then(|v| v.as_f64())
            .ok_or_else(|| format!("waypoint[{i}] missing 'lon'"))?;
        out.push(Waypoint {
            lat,
            lon,
            alt: wp.get("alt").and_then(|v| v.as_f64()),
            speed_ms: wp.get("speed_ms").and_then(|v| v.as_f64()),
        });
    }
    Ok(out)
}

/// Map a `platform_*` tool call to a dispatchable [`PlatformCommand`].
///
/// - `Ok(Some(cmd))` — a control command to route through the gate/adapter.
/// - `Ok(None)`      — a recognized but non-command (query/management) tool;
///   the caller services it against kernel state, not the adapter.
/// - `Err(msg)`      — unknown tool or invalid arguments.
pub fn map_tool_to_command(
    name: &str,
    args: &serde_json::Value,
) -> Result<Option<PlatformCommand>, String> {
    let cmd = match name {
        // ── Motion ──
        "platform_set_heading" => PlatformCommand::SetHeading {
            platform_id: req_str(args, "platform_id")?,
            heading_deg: req_f64(args, "heading_deg")?,
            speed_ms: opt_f64(args, "speed_ms"),
            turn_direction: parse_turn(args),
        },
        "platform_set_speed" => PlatformCommand::SetSpeed {
            platform_id: req_str(args, "platform_id")?,
            speed_ms: req_f64(args, "speed_ms")?,
            acceleration_ms2: opt_f64(args, "acceleration_ms2"),
        },
        "platform_set_altitude" => PlatformCommand::SetAltitude {
            platform_id: req_str(args, "platform_id")?,
            altitude_m: req_f64(args, "altitude_m")?,
            rate_ms: opt_f64(args, "rate_ms"),
        },
        "platform_goto_location" => PlatformCommand::GotoLocation {
            platform_id: req_str(args, "platform_id")?,
            lat: req_f64(args, "lat")?,
            lon: req_f64(args, "lon")?,
            alt: opt_f64(args, "alt"),
            speed_ms: opt_f64(args, "speed_ms"),
        },
        "platform_follow_route" => PlatformCommand::FollowRoute {
            platform_id: req_str(args, "platform_id")?,
            waypoints: parse_waypoints(args)?,
        },
        // ── Sensors ──
        "platform_sensor_on" => PlatformCommand::SensorOn {
            platform_id: req_str(args, "platform_id")?,
            sensor_id: req_str(args, "sensor_id")?,
        },
        "platform_sensor_off" => PlatformCommand::SensorOff {
            platform_id: req_str(args, "platform_id")?,
            sensor_id: req_str(args, "sensor_id")?,
        },
        "platform_sensor_mode" => PlatformCommand::SensorSetMode {
            platform_id: req_str(args, "platform_id")?,
            sensor_id: req_str(args, "sensor_id")?,
            mode: req_str(args, "mode")?,
        },
        // SMA specialization: cue passive ESM to geolocate a threat emitter
        // (drives the ElectronicAttack / SEAD workflows). Composes SensorSetMode.
        "platform_emitter_geolocate" => PlatformCommand::SensorSetMode {
            platform_id: req_str(args, "platform_id")?,
            sensor_id: opt_str(args, "sensor_id").unwrap_or_else(|| "esm".to_string()),
            mode: "esm_geolocate".to_string(),
        },
        // ── Weapons ──
        "platform_fire_at_target" => PlatformCommand::FireAtTarget {
            platform_id: req_str(args, "platform_id")?,
            weapon_id: req_str(args, "weapon_id")?,
            track_id: req_str(args, "track_id")?,
        },
        "platform_fire_salvo" => PlatformCommand::FireSalvo {
            platform_id: req_str(args, "platform_id")?,
            weapon_id: req_str(args, "weapon_id")?,
            track_id: req_str(args, "track_id")?,
            salvo_size: req_u32(args, "salvo_size")?,
        },
        "platform_fire_chaff" => PlatformCommand::FireChaff {
            platform_id: req_str(args, "platform_id")?,
            weapon_id: req_str(args, "weapon_id")?,
            count: req_u32(args, "count")?,
            interval_s: opt_f64(args, "interval_s").unwrap_or(0.5),
        },
        "platform_update_target" => PlatformCommand::UpdateTarget {
            platform_id: req_str(args, "platform_id")?,
            track_id: req_str(args, "track_id")?,
        },
        // ── Electronic Warfare ──
        "platform_jam_start" => PlatformCommand::JamStart {
            platform_id: req_str(args, "platform_id")?,
            jammer_id: req_str(args, "jammer_id")?,
            frequency_hz: req_f64(args, "frequency_hz")?,
            bandwidth_hz: req_f64(args, "bandwidth_hz")?,
            target_track_id: req_str(args, "target_track_id")?,
        },
        "platform_jam_stop" => PlatformCommand::JamStop {
            platform_id: req_str(args, "platform_id")?,
            jammer_id: req_str(args, "jammer_id")?,
        },
        "platform_jam_mode" => PlatformCommand::JamSetMode {
            platform_id: req_str(args, "platform_id")?,
            jammer_id: req_str(args, "jammer_id")?,
            frequency_hz: opt_f64(args, "frequency_hz"),
            bandwidth_hz: opt_f64(args, "bandwidth_hz"),
        },
        // ── Communications ──
        "platform_send_message" => PlatformCommand::SendMessage {
            from_platform_id: req_str(args, "from_platform_id")?,
            to_platform_id: req_str(args, "to_platform_id")?,
            message: req_str(args, "message")?,
        },
        "platform_comm_on" => PlatformCommand::CommOn {
            platform_id: req_str(args, "platform_id")?,
        },
        "platform_comm_off" => PlatformCommand::CommOff {
            platform_id: req_str(args, "platform_id")?,
        },
        // ── Command & Control ──
        "platform_change_commander" => PlatformCommand::ChangeCommander {
            platform_id: req_str(args, "platform_id")?,
            new_commander_id: req_str(args, "new_commander_id")?,
        },
        // ── UAV ──
        "platform_launch_uav" => PlatformCommand::LaunchUav {
            uav_id: req_str(args, "uav_id")?,
        },
        "platform_recover_uav" => PlatformCommand::RecoverUav {
            uav_id: req_str(args, "uav_id")?,
        },
        "platform_rtb_uav" => PlatformCommand::ReturnToBase {
            uav_id: req_str(args, "uav_id")?,
        },
        "platform_assign_mission" => PlatformCommand::AssignMission {
            uav_id: req_str(args, "uav_id")?,
            mission_type: req_str(args, "mission_type")?,
            params_json: opt_str(args, "params_json").unwrap_or_else(|| "{}".to_string()),
        },
        // TCA/FMA specialization: assign a tactical role (CcaRole) to a platform
        // or formation member. The role is the brain↔cerebellum / lead↔member
        // contract; it rides on the mission and the member adopts it via its own
        // brain. Composes AssignMission with the role in params_json.
        "platform_assign_role" => PlatformCommand::AssignMission {
            uav_id: req_str(args, "uav_id")?,
            mission_type: "role_assignment".to_string(),
            params_json: serde_json::json!({ "role": req_str(args, "role")? }).to_string(),
        },
        // ── Formation ──
        "platform_form_up" => PlatformCommand::FormUp {
            formation_type: req_str(args, "formation_type")?,
            reference_platform_id: req_str(args, "reference_platform_id")?,
            spacing_m: req_f64(args, "spacing_m")?,
        },
        "platform_break_formation" => PlatformCommand::BreakFormation,
        "platform_formation_maneuver" => PlatformCommand::FormationManeuver {
            reference_platform_id: req_str(args, "reference_platform_id")?,
            delta_heading_deg: req_f64(args, "delta_heading_deg")?,
            delta_speed_ms: req_f64(args, "delta_speed_ms")?,
        },
        // ── Target handoff ──
        "platform_handoff_target" => PlatformCommand::HandoffTarget {
            from_platform_id: req_str(args, "from_platform_id")?,
            to_platform_id: req_str(args, "to_platform_id")?,
            track_id: req_str(args, "track_id")?,
        },
        // ── Coordinated strike ──
        "platform_coordinated_strike" => PlatformCommand::CoordinatedStrike {
            coordinator_platform_id: req_str(args, "coordinator_platform_id")?,
            strike_platform_ids: args
                .get("strike_platform_ids")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            target_id: req_str(args, "target_id")?,
            time_on_target_us: req_u64(args, "time_on_target_us")?,
        },
        "platform_weapon_guidance_handoff" => PlatformCommand::WeaponGuidanceHandoff {
            from_platform_id: req_str(args, "from_platform_id")?,
            to_platform_id: req_str(args, "to_platform_id")?,
            munition_id: req_str(args, "munition_id")?,
        },
        // ── Deck / relay ──
        "platform_deck_reconfigure" => PlatformCommand::DeckReconfigure {
            deck_id: req_str(args, "deck_id")?,
            action: req_str(args, "action")?,
            target_id: req_str(args, "target_id")?,
        },
        "platform_relay_enable" => PlatformCommand::RelayEnable {
            uav_id: req_str(args, "uav_id")?,
            bandwidth_hz: req_f64(args, "bandwidth_hz")?,
        },
        "platform_relay_disable" => PlatformCommand::RelayDisable {
            uav_id: req_str(args, "uav_id")?,
        },
        // ── Aux passthrough ──
        "platform_aux_command" => PlatformCommand::AuxCommand {
            platform_id: req_str(args, "platform_id")?,
            key: req_str(args, "key")?,
            value_json: opt_str(args, "value_json").unwrap_or_else(|| "{}".to_string()),
        },

        // Recognized but non-command (query / management) tools — serviced
        // against kernel state, not routed to an adapter.
        "platform_get_state"
        | "platform_get_fleet_status"
        | "platform_get_health_report"
        | "platform_run_bit"
        | "platform_get_roe"
        | "platform_set_roe_level"
        | "platform_get_geofence_status"
        | "platform_check_geofence_violation"
        | "platform_get_nav_status"
        | "platform_get_track"
        | "platform_mark_track_identification"
        | "platform_activate_mission_config" => return Ok(None),

        other => return Err(format!("unknown platform tool '{other}'")),
    };
    Ok(Some(cmd))
}

/// Map a `platform_*` tool call to a [`CandidateIntent`] for the gate pipeline.
///
/// Returns `Ok(None)` for non-command (query/management) tools.
pub fn map_tool_to_intent(
    name: &str,
    args: &serde_json::Value,
    source: IntentSource,
    priority: CommandPriority,
    now_secs: f64,
) -> Result<Option<CandidateIntent>, String> {
    match map_tool_to_command(name, args)? {
        Some(cmd) => Ok(Some(CandidateIntent::new(
            cmd,
            priority,
            source,
            now_secs,
            format!("tool:{name}"),
        ))),
        None => Ok(None),
    }
}

/// All platform tool definitions for registration with the tool runner.
pub fn platform_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        // ── State ──
        ToolDefinition {
            name: "platform_get_state".into(),
            description: "Get the current world state snapshot: all platforms, their positions, tracks, sensors, weapons, and active munitions. Returns a text summary suitable for tactical analysis.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "platform_id": { "type": "string", "description": "Optional: filter to a specific platform" }
                }
            }),
        },
        // ── Motion ──
        ToolDefinition {
            name: "platform_set_heading".into(),
            description: "Set a platform's desired heading (degrees, 0=north, clockwise). Optionally set speed and turn direction.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "platform_id": { "type": "string", "description": "Target platform ID" },
                    "heading_deg": { "type": "number", "description": "Desired heading in degrees (0-360)" },
                    "speed_ms": { "type": "number", "description": "Optional: desired speed in m/s" },
                    "turn_direction": { "type": "string", "enum": ["left", "right", "shortest"] }
                },
                "required": ["platform_id", "heading_deg"]
            }),
        },
        ToolDefinition {
            name: "platform_set_speed".into(),
            description: "Set a platform's desired speed in meters per second.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "platform_id": { "type": "string" },
                    "speed_ms": { "type": "number" },
                    "acceleration_ms2": { "type": "number", "description": "Optional acceleration" }
                },
                "required": ["platform_id", "speed_ms"]
            }),
        },
        ToolDefinition {
            name: "platform_set_altitude".into(),
            description: "Set a platform's desired altitude in meters (UAV only).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "platform_id": { "type": "string" },
                    "altitude_m": { "type": "number" },
                    "rate_ms": { "type": "number", "description": "Optional climb/descent rate" }
                },
                "required": ["platform_id", "altitude_m"]
            }),
        },
        ToolDefinition {
            name: "platform_goto_location".into(),
            description: "Command a platform to navigate to a specific lat/lon location.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "platform_id": { "type": "string" },
                    "lat": { "type": "number" },
                    "lon": { "type": "number" },
                    "alt": { "type": "number", "description": "Optional altitude" },
                    "speed_ms": { "type": "number" }
                },
                "required": ["platform_id", "lat", "lon"]
            }),
        },
        ToolDefinition {
            name: "platform_follow_route".into(),
            description: "Command a platform to follow a sequence of waypoints. Each waypoint: {lat, lon, alt?, speed_ms?}".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "platform_id": { "type": "string" },
                    "waypoints": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "lat": { "type": "number" },
                                "lon": { "type": "number" },
                                "alt": { "type": "number" },
                                "speed_ms": { "type": "number" }
                            },
                            "required": ["lat", "lon"]
                        }
                    }
                },
                "required": ["platform_id", "waypoints"]
            }),
        },
        // ── Sensors ──
        ToolDefinition {
            name: "platform_sensor_on".into(),
            description: "Turn on a sensor on a platform.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "platform_id": { "type": "string" },
                    "sensor_id": { "type": "string" }
                },
                "required": ["platform_id", "sensor_id"]
            }),
        },
        ToolDefinition {
            name: "platform_sensor_off".into(),
            description: "Turn off a sensor on a platform.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "platform_id": { "type": "string" },
                    "sensor_id": { "type": "string" }
                },
                "required": ["platform_id", "sensor_id"]
            }),
        },
        ToolDefinition {
            name: "platform_sensor_mode".into(),
            description: "Change a sensor's operating mode (e.g. \"search\", \"track\", \"passive\").".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "platform_id": { "type": "string" },
                    "sensor_id": { "type": "string" },
                    "mode": { "type": "string" }
                },
                "required": ["platform_id", "sensor_id", "mode"]
            }),
        },
        // ── Weapons ──
        ToolDefinition {
            name: "platform_fire_at_target".into(),
            description: "Fire a weapon at a target track. WARNING: requires weapon approval gate.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "platform_id": { "type": "string" },
                    "weapon_id": { "type": "string" },
                    "track_id": { "type": "string" }
                },
                "required": ["platform_id", "weapon_id", "track_id"]
            }),
        },
        ToolDefinition {
            name: "platform_fire_salvo".into(),
            description: "Fire multiple weapons (salvo) at a target track.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "platform_id": { "type": "string" },
                    "weapon_id": { "type": "string" },
                    "track_id": { "type": "string" },
                    "salvo_size": { "type": "integer" }
                },
                "required": ["platform_id", "weapon_id", "track_id", "salvo_size"]
            }),
        },
        ToolDefinition {
            name: "platform_fire_chaff".into(),
            description: "Deploy chaff countermeasures from a platform.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "platform_id": { "type": "string" },
                    "weapon_id": { "type": "string" },
                    "count": { "type": "integer" },
                    "interval_s": { "type": "number" }
                },
                "required": ["platform_id", "weapon_id", "count"]
            }),
        },
        ToolDefinition {
            name: "platform_update_target".into(),
            description: "Update a platform's current target track.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "platform_id": { "type": "string" },
                    "track_id": { "type": "string" }
                },
                "required": ["platform_id", "track_id"]
            }),
        },
        // ── Electronic Warfare ──
        ToolDefinition {
            name: "platform_jam_start".into(),
            description: "Start jamming on a target track with specified frequency and bandwidth.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "platform_id": { "type": "string" },
                    "jammer_id": { "type": "string" },
                    "frequency_hz": { "type": "number" },
                    "bandwidth_hz": { "type": "number" },
                    "target_track_id": { "type": "string" }
                },
                "required": ["platform_id", "jammer_id", "frequency_hz", "bandwidth_hz", "target_track_id"]
            }),
        },
        ToolDefinition {
            name: "platform_jam_stop".into(),
            description: "Stop jamming on a platform.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "platform_id": { "type": "string" },
                    "jammer_id": { "type": "string" }
                },
                "required": ["platform_id", "jammer_id"]
            }),
        },
        ToolDefinition {
            name: "platform_jam_mode".into(),
            description: "Change jammer mode (frequency/bandwidth).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "platform_id": {"type": "string"},
                    "jammer_id": {"type": "string"},
                    "frequency_hz": {"type": "number"},
                    "bandwidth_hz": {"type": "number"}
                },
                "required": ["platform_id", "jammer_id"]
            }),
        },
        // ── Communications ──
        ToolDefinition {
            name: "platform_send_message".into(),
            description: "Send a text message from one platform to another (inter-platform comms).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "from_platform_id": { "type": "string" },
                    "to_platform_id": { "type": "string" },
                    "message": { "type": "string" }
                },
                "required": ["from_platform_id", "to_platform_id", "message"]
            }),
        },
        ToolDefinition {
            name: "platform_comm_on".into(),
            description: "Enable communications on a platform.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "platform_id": { "type": "string" }
                },
                "required": ["platform_id"]
            }),
        },
        ToolDefinition {
            name: "platform_comm_off".into(),
            description: "Disable communications on a platform (EMCON).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "platform_id": { "type": "string" }
                },
                "required": ["platform_id"]
            }),
        },
        // ── UAV ──
        ToolDefinition {
            name: "platform_launch_uav".into(),
            description: "Launch a UAV from the mothership.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "uav_id": { "type": "string" }
                },
                "required": ["uav_id"]
            }),
        },
        ToolDefinition {
            name: "platform_recover_uav".into(),
            description: "Recover a UAV back to the mothership.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "uav_id": { "type": "string" }
                },
                "required": ["uav_id"]
            }),
        },
        ToolDefinition {
            name: "platform_rtb_uav".into(),
            description: "Command a UAV to return to base.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "uav_id": { "type": "string" }
                },
                "required": ["uav_id"]
            }),
        },
        // ── Aux ──
        ToolDefinition {
            name: "platform_aux_command".into(),
            description: "Send a custom key-value auxiliary command to a platform (passthrough).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "platform_id": { "type": "string" },
                    "key": { "type": "string" },
                    "value_json": { "type": "string" }
                },
                "required": ["platform_id", "key"]
            }),
        },
        // ── Commander ──
        ToolDefinition {
            name: "platform_change_commander".into(),
            description: "Reassign a platform to a new commander (chain of command change).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "platform_id": { "type": "string" },
                    "new_commander_id": { "type": "string" }
                },
                "required": ["platform_id", "new_commander_id"]
            }),
        },

        // ── Heterogeneous Fleet (FMA) ──
        ToolDefinition {
            name: "platform_assign_mission".into(),
            description: "Assign a mission to a UAV. mission_type: area_search|track_target|strike|bda|comm_relay.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "uav_id": { "type": "string" },
                    "mission_type": { "type": "string" },
                    "params_json": { "type": "string" }
                },
                "required": ["uav_id", "mission_type"]
            }),
        },
        ToolDefinition {
            name: "platform_handoff_target".into(),
            description: "Hand off a target track from one platform to another (cross-platform cueing).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "from_platform_id": { "type": "string" },
                    "to_platform_id": { "type": "string" },
                    "track_id": { "type": "string" }
                },
                "required": ["from_platform_id", "to_platform_id", "track_id"]
            }),
        },
        ToolDefinition {
            name: "platform_form_up".into(),
            description: "Form a flight/sailing formation around a reference platform.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "formation_type": { "type": "string" },
                    "reference_platform_id": { "type": "string" },
                    "spacing_m": { "type": "number" }
                },
                "required": ["formation_type", "reference_platform_id", "spacing_m"]
            }),
        },
        ToolDefinition {
            name: "platform_break_formation".into(),
            description: "Disband the current formation; each platform resumes autonomous operation.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
            }),
        },
        ToolDefinition {
            name: "platform_formation_maneuver".into(),
            description: "Apply a delta heading/speed adjustment relative to a reference platform while in formation.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "reference_platform_id": { "type": "string" },
                    "delta_heading_deg": { "type": "number" },
                    "delta_speed_ms": { "type": "number" }
                },
                "required": ["reference_platform_id", "delta_heading_deg", "delta_speed_ms"]
            }),
        },
        ToolDefinition {
            name: "platform_coordinated_strike".into(),
            description: "Coordinate a time-on-target strike across USV suppression and UAV delivery.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "coordinator_platform_id": { "type": "string" },
                    "strike_platform_ids": {
                        "type": "array",
                        "items": { "type": "string" }
                    },
                    "target_id": { "type": "string" },
                    "time_on_target_us": { "type": "integer" }
                },
                "required": ["coordinator_platform_id", "strike_platform_ids", "target_id", "time_on_target_us"]
            }),
        },
        ToolDefinition {
            name: "platform_weapon_guidance_handoff".into(),
            description: "Hand off in-flight weapon guidance to another platform (mid-course to terminal).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "from_platform_id": { "type": "string" },
                    "to_platform_id": { "type": "string" },
                    "munition_id": { "type": "string" }
                },
                "required": ["from_platform_id", "to_platform_id", "munition_id"]
            }),
        },
        ToolDefinition {
            name: "platform_deck_reconfigure".into(),
            description: "Reconfigure a deck resource: reload_weapon, refuel_uav, swap_payload, maintenance.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "deck_id": { "type": "string" },
                    "action": { "type": "string" },
                    "target_id": { "type": "string" }
                },
                "required": ["deck_id", "action", "target_id"]
            }),
        },
        ToolDefinition {
            name: "platform_relay_enable".into(),
            description: "Enable a UAV as a communications relay with given bandwidth.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "uav_id": { "type": "string" },
                    "bandwidth_hz": { "type": "number" }
                },
                "required": ["uav_id", "bandwidth_hz"]
            }),
        },
        ToolDefinition {
            name: "platform_relay_disable".into(),
            description: "Disable the communications relay function on a UAV.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "uav_id": { "type": "string" }
                },
                "required": ["uav_id"]
            }),
        },
        ToolDefinition {
            name: "platform_get_fleet_status".into(),
            description: "Get the current status of the heterogeneous fleet: UAV launch readiness, fuel, comm link quality.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },

        // ── UMAA: Health Monitoring (HMA) ──
        ToolDefinition {
            name: "platform_get_health_report".into(),
            description: "Get the UMAA HealthReport: overall platform health + per-component status + active alerts.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "platform_run_bit".into(),
            description: "Trigger Built-In Test on a specific component.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "component": { "type": "string" }
                },
                "required": ["component"]
            }),
        },

        // ── UMAA: Operational Restrictions (ORA) ──
        ToolDefinition {
            name: "platform_get_roe".into(),
            description: "Get current Rules of Engagement (weapon release level, restricted targets, engagement zones).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "platform_set_roe_level".into(),
            description: "Set the weapon release level: weapons_hold | weapons_tight | weapons_free.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "level": { "type": "string", "enum": ["weapons_hold", "weapons_tight", "weapons_free"] }
                },
                "required": ["level"]
            }),
        },
        ToolDefinition {
            name: "platform_get_geofence_status".into(),
            description: "Query the current geofence status (registered fences + any active violations).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "platform_check_geofence_violation".into(),
            description: "Check the current pose against all registered geofences; returns the first violation if any.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },

        // ── UMAA: Navigation (position estimation) ──
        ToolDefinition {
            name: "platform_get_nav_status".into(),
            description: "Get the current fused navigation status: position, accuracy (CEP), active source (GPS/INS/DR).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },

        // ── UMAA: Track Management ──
        ToolDefinition {
            name: "platform_get_track".into(),
            description: "Get detailed info for a specific track by ID.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "track_id": { "type": "string" }
                },
                "required": ["track_id"]
            }),
        },
        ToolDefinition {
            name: "platform_mark_track_identification".into(),
            description: "Manually mark a track's identification (e.g. after shore confirmation).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "track_id": { "type": "string" },
                    "classification": { "type": "string" },
                    "confidence": { "type": "number" }
                },
                "required": ["track_id", "classification", "confidence"]
            }),
        },

        // ── UMAA: Mission Configuration ──
        ToolDefinition {
            name: "platform_activate_mission_config".into(),
            description: "Activate a named mission configuration (ROE + geofences + limits + comm plan).".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "mission_id": { "type": "string" }
                },
                "required": ["mission_id"]
            }),
        },

        // ── Sub-agent specializations (role/posture contract + EW) ──
        ToolDefinition {
            name: "platform_assign_role".into(),
            description: "Assign a tactical role (CcaRole) to a platform or formation member. The role is the brain↔cerebellum / lead↔member contract and drives the member's posture lanes. role ∈ {recon, relay, striker, decoy, intercept, patrol, escort, surveil, adaptive, ew_protection, ew_jamming}.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "uav_id": { "type": "string", "description": "Target platform / member id" },
                    "role": { "type": "string", "description": "CcaRole to assign (snake_case)" }
                },
                "required": ["uav_id", "role"]
            }),
        },
        ToolDefinition {
            name: "platform_emitter_geolocate".into(),
            description: "Cue a passive ESM sensor to geolocate a threat emitter (SMA). Drives the ElectronicAttack / SEAD workflows without active radiation.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "platform_id": { "type": "string", "description": "Sensing platform id" },
                    "sensor_id": { "type": "string", "description": "ESM sensor id (default 'esm')" }
                },
                "required": ["platform_id"]
            }),
        },
    ]
}

/// Extract and count platform tools for testing
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_platform_tool() {
        assert!(is_platform_tool("platform_set_heading"));
        assert!(!is_platform_tool("web_search"));
    }

    #[test]
    fn test_map_set_heading() {
        let args = serde_json::json!({"platform_id": "usv-01", "heading_deg": 90.0, "speed_ms": 12.0, "turn_direction": "left"});
        let cmd = map_tool_to_command("platform_set_heading", &args)
            .unwrap()
            .unwrap();
        match cmd {
            PlatformCommand::SetHeading {
                platform_id,
                heading_deg,
                speed_ms,
                turn_direction,
            } => {
                assert_eq!(platform_id, "usv-01");
                assert_eq!(heading_deg, 90.0);
                assert_eq!(speed_ms, Some(12.0));
                assert!(matches!(turn_direction, Some(TurnDirection::Left)));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_map_follow_route() {
        let args = serde_json::json!({
            "platform_id": "uav-1",
            "waypoints": [{"lat": 30.0, "lon": 120.0}, {"lat": 30.1, "lon": 120.1, "alt": 500.0}]
        });
        let cmd = map_tool_to_command("platform_follow_route", &args)
            .unwrap()
            .unwrap();
        match cmd {
            PlatformCommand::FollowRoute { waypoints, .. } => assert_eq!(waypoints.len(), 2),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_map_missing_param_errors() {
        let args = serde_json::json!({"platform_id": "x"});
        assert!(map_tool_to_command("platform_set_heading", &args).is_err());
    }

    #[test]
    fn test_assign_role_encodes_role_in_mission() {
        let args = serde_json::json!({"uav_id": "cca-1", "role": "ew_jamming"});
        let cmd = map_tool_to_command("platform_assign_role", &args)
            .unwrap()
            .unwrap();
        match cmd {
            PlatformCommand::AssignMission {
                uav_id,
                mission_type,
                params_json,
            } => {
                assert_eq!(uav_id, "cca-1");
                assert_eq!(mission_type, "role_assignment");
                assert!(params_json.contains("ew_jamming"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_emitter_geolocate_defaults_esm_sensor() {
        let cmd = map_tool_to_command(
            "platform_emitter_geolocate",
            &serde_json::json!({"platform_id": "p"}),
        )
        .unwrap()
        .unwrap();
        match cmd {
            PlatformCommand::SensorSetMode {
                sensor_id, mode, ..
            } => {
                assert_eq!(sensor_id, "esm");
                assert_eq!(mode, "esm_geolocate");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_query_tools_map_to_none() {
        for t in [
            "platform_get_state",
            "platform_get_roe",
            "platform_get_health_report",
        ] {
            assert!(map_tool_to_command(t, &serde_json::json!({}))
                .unwrap()
                .is_none());
        }
    }

    #[test]
    fn test_unknown_tool_errors() {
        assert!(map_tool_to_command("platform_warp_drive", &serde_json::json!({})).is_err());
    }

    #[test]
    fn test_every_command_tool_maps() {
        // Every tool whose schema implies a command must map to Some(cmd) when
        // given its required params. Guards against schema/mapper drift.
        let cases: &[(&str, serde_json::Value)] = &[
            (
                "platform_set_speed",
                serde_json::json!({"platform_id":"p","speed_ms":5.0}),
            ),
            (
                "platform_set_altitude",
                serde_json::json!({"platform_id":"p","altitude_m":100.0}),
            ),
            (
                "platform_goto_location",
                serde_json::json!({"platform_id":"p","lat":1.0,"lon":2.0}),
            ),
            (
                "platform_sensor_on",
                serde_json::json!({"platform_id":"p","sensor_id":"s"}),
            ),
            (
                "platform_sensor_off",
                serde_json::json!({"platform_id":"p","sensor_id":"s"}),
            ),
            (
                "platform_sensor_mode",
                serde_json::json!({"platform_id":"p","sensor_id":"s","mode":"track"}),
            ),
            (
                "platform_fire_at_target",
                serde_json::json!({"platform_id":"p","weapon_id":"w","track_id":"t"}),
            ),
            (
                "platform_fire_salvo",
                serde_json::json!({"platform_id":"p","weapon_id":"w","track_id":"t","salvo_size":2}),
            ),
            (
                "platform_fire_chaff",
                serde_json::json!({"platform_id":"p","weapon_id":"w","count":3}),
            ),
            (
                "platform_update_target",
                serde_json::json!({"platform_id":"p","track_id":"t"}),
            ),
            (
                "platform_jam_start",
                serde_json::json!({"platform_id":"p","jammer_id":"j","frequency_hz":1.0,"bandwidth_hz":1.0,"target_track_id":"t"}),
            ),
            (
                "platform_jam_stop",
                serde_json::json!({"platform_id":"p","jammer_id":"j"}),
            ),
            (
                "platform_jam_mode",
                serde_json::json!({"platform_id":"p","jammer_id":"j"}),
            ),
            (
                "platform_send_message",
                serde_json::json!({"from_platform_id":"a","to_platform_id":"b","message":"hi"}),
            ),
            ("platform_comm_on", serde_json::json!({"platform_id":"p"})),
            ("platform_comm_off", serde_json::json!({"platform_id":"p"})),
            (
                "platform_change_commander",
                serde_json::json!({"platform_id":"p","new_commander_id":"c"}),
            ),
            ("platform_launch_uav", serde_json::json!({"uav_id":"u"})),
            ("platform_recover_uav", serde_json::json!({"uav_id":"u"})),
            ("platform_rtb_uav", serde_json::json!({"uav_id":"u"})),
            (
                "platform_assign_mission",
                serde_json::json!({"uav_id":"u","mission_type":"area_search"}),
            ),
            (
                "platform_form_up",
                serde_json::json!({"formation_type":"vee","reference_platform_id":"r","spacing_m":100.0}),
            ),
            ("platform_break_formation", serde_json::json!({})),
            (
                "platform_formation_maneuver",
                serde_json::json!({"reference_platform_id":"r","delta_heading_deg":10.0,"delta_speed_ms":1.0}),
            ),
            (
                "platform_handoff_target",
                serde_json::json!({"from_platform_id":"a","to_platform_id":"b","track_id":"t"}),
            ),
            (
                "platform_coordinated_strike",
                serde_json::json!({"coordinator_platform_id":"c","strike_platform_ids":["s1"],"target_id":"t","time_on_target_us":1000}),
            ),
            (
                "platform_weapon_guidance_handoff",
                serde_json::json!({"from_platform_id":"a","to_platform_id":"b","munition_id":"m"}),
            ),
            (
                "platform_deck_reconfigure",
                serde_json::json!({"deck_id":"d","action":"refuel_uav","target_id":"u"}),
            ),
            (
                "platform_relay_enable",
                serde_json::json!({"uav_id":"u","bandwidth_hz":1000.0}),
            ),
            ("platform_relay_disable", serde_json::json!({"uav_id":"u"})),
            (
                "platform_aux_command",
                serde_json::json!({"platform_id":"p","key":"k"}),
            ),
            (
                "platform_assign_role",
                serde_json::json!({"uav_id":"u","role":"ew_jamming"}),
            ),
            (
                "platform_emitter_geolocate",
                serde_json::json!({"platform_id":"p"}),
            ),
        ];
        for (name, args) in cases {
            let res = map_tool_to_command(name, args);
            assert!(res.is_ok(), "{name} errored: {res:?}");
            assert!(res.unwrap().is_some(), "{name} mapped to None");
        }
    }

    #[test]
    fn test_platform_tool_count() {
        let tools = platform_tool_definitions();
        assert!(
            tools.len() >= 45,
            "expected at least 45 platform tools, got {}",
            tools.len()
        );

        // Verify key tools exist
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"platform_get_state"));
        assert!(names.contains(&"platform_set_heading"));
        assert!(names.contains(&"platform_fire_at_target"));
        assert!(names.contains(&"platform_jam_start"));
        assert!(names.contains(&"platform_launch_uav"));
        assert!(names.contains(&"platform_assign_mission"));
        assert!(names.contains(&"platform_assign_role"));
        assert!(names.contains(&"platform_emitter_geolocate"));
        assert!(names.contains(&"platform_coordinated_strike"));
        assert!(names.contains(&"platform_get_fleet_status"));
        assert!(names.contains(&"platform_get_health_report"));
        assert!(names.contains(&"platform_get_roe"));
        assert!(names.contains(&"platform_activate_mission_config"));
    }
}
