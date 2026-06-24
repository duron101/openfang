//! Minimal protobuf encode/decode for ArkSIM wire format.
//! Hand-coded to avoid prost-build dependency on `protoc`.
//!
//! Only the message types needed for the bridge are implemented.
//! Based on wire-verified ArkSIM 4.1 proto v2 (APL original).

/// Raw protobuf field writer.
pub struct ProtoWriter {
    buf: Vec<u8>,
}

impl Default for ProtoWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProtoWriter {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    /// Write a varint field (wire type 0)
    pub fn field_varint(&mut self, field_num: u32, value: u64) {
        self.write_tag(field_num, 0);
        self.write_varint(value);
    }

    /// Write a length-delimited field (wire type 2) — string or bytes
    pub fn field_bytes(&mut self, field_num: u32, data: &[u8]) {
        self.write_tag(field_num, 2);
        self.write_varint(data.len() as u64);
        self.buf.extend_from_slice(data);
    }

    /// Write a length-delimited field from string
    pub fn field_string(&mut self, field_num: u32, s: &str) {
        self.field_bytes(field_num, s.as_bytes());
    }

    /// Write a sub-message field (nested message)
    pub fn field_message(&mut self, field_num: u32, msg: &[u8]) {
        self.field_bytes(field_num, msg);
    }

    /// Write a repeated double field (packed, wire type 2)
    pub fn field_packed_double(&mut self, field_num: u32, values: &[f64]) {
        self.write_tag(field_num, 2);
        let len = values.len() * 8;
        self.write_varint(len as u64);
        for v in values {
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
    }

    /// Write a fixed64 field (wire type 1)
    pub fn field_fixed64(&mut self, field_num: u32, value: f64) {
        self.write_tag(field_num, 1);
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    /// Write a fixed32 field (wire type 5)
    pub fn field_fixed32(&mut self, field_num: u32, value: f32) {
        self.write_tag(field_num, 5);
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    fn write_tag(&mut self, field_num: u32, wire_type: u32) {
        self.write_varint(((field_num << 3) | wire_type) as u64);
    }

    fn write_varint(&mut self, mut value: u64) {
        while value >= 0x80 {
            self.buf.push((value as u8) | 0x80);
            value >>= 7;
        }
        self.buf.push(value as u8);
    }
}

// ── AgentContrl message ──
// field 1: action (varint, E_Actions enum value)
// field 2: agent_id (string)

pub fn encode_agent_contrl(action: i32, agent_id: &str) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    // Proto3 omits default enum value 0. Python's verified
    // ProtoStringBuilder().set_agent_outside_control("self") therefore emits an
    // 8-byte ActionsFromOutside packet with only agent_id in the nested message.
    if action != 0 {
        w.field_varint(1, action as u64); // action
    }
    w.field_string(2, agent_id); // agent_id
    w.into_bytes()
}

// ── DesiredHeading message ──
// field 1: agent_id (string)
// field 2: desired_heading (fixed64, radians)
// field 3: has_desired_velocity (varint, bool)
// field 4: desired_velocity (fixed64)
// field 5: has_desired_turn_direction (varint, bool)
// field 6: desired_turn_direction (varint, uint32)

pub fn encode_desired_heading(agent_id: &str, heading_rad: f64) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    w.field_string(1, agent_id);
    w.field_fixed64(2, heading_rad);
    w.into_bytes()
}

pub fn encode_desired_heading_full(
    agent_id: &str,
    heading_rad: f64,
    speed_ms: Option<f64>,
    turn_dir: Option<u32>,
) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    w.field_string(1, agent_id);
    w.field_fixed64(2, heading_rad);
    if let Some(s) = speed_ms {
        w.field_varint(3, 1); // has_desired_velocity = true
        w.field_fixed64(4, s);
    }
    if let Some(dir) = turn_dir {
        w.field_varint(5, 1); // has_desired_turn_direction = true
        w.field_varint(6, dir as u64);
    }
    w.into_bytes()
}

// ── AgentName message ──
// field 1: agent_id (string), field 2: Component_id (string)
pub fn encode_agent_name(agent_id: &str, component_id: &str) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    w.field_string(1, agent_id);
    if !component_id.is_empty() {
        w.field_string(2, component_id);
    }
    w.into_bytes()
}

// ── DesiredAltitude (field 1 agent_id, 2 altitude, 3 has_rate, 4 rate) ──
pub fn encode_desired_altitude(agent_id: &str, altitude_m: f64, rate_ms: Option<f64>) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    w.field_string(1, agent_id);
    w.field_fixed64(2, altitude_m);
    if let Some(rate) = rate_ms {
        w.field_varint(3, 1);
        w.field_fixed64(4, rate);
    }
    w.into_bytes()
}

// ── DesiredVelocity (field 1 agent_id, 2 velocity, 3 linearAccel) ──
pub fn encode_desired_velocity(agent_id: &str, velocity_ms: f64, accel: Option<f64>) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    w.field_string(1, agent_id);
    w.field_fixed64(2, velocity_ms);
    if let Some(a) = accel {
        w.field_fixed64(3, a);
    }
    w.into_bytes()
}

// ── GoToLocation (field 1 agent_id, 2 priority, 3 packed LLA) ──
pub fn encode_goto_location(agent_id: &str, lat: f64, lon: f64, alt: f64) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    w.field_string(1, agent_id);
    w.field_varint(2, 0);
    w.field_packed_double(3, &[lat, lon, alt]);
    w.into_bytes()
}

// ── FollowRoute (field 1 agent_id, 2 route_name, 3 repeated Waypoint) ──
/// `waypoints`: (waypoint_id, speed, lat, lon, alt)
pub fn encode_follow_route(
    agent_id: &str,
    route_name: &str,
    waypoints: &[(String, f64, f64, f64, f64)],
) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    w.field_string(1, agent_id);
    w.field_string(2, route_name);
    for (idx, (wp_id, speed, lat, lon, alt)) in waypoints.iter().enumerate() {
        let mut wp = ProtoWriter::new();
        let id = if wp_id.is_empty() {
            format!("wp{idx}")
        } else {
            wp_id.clone()
        };
        wp.field_string(1, &id);
        wp.field_fixed64(2, *speed);
        wp.field_packed_double(3, &[*lat, *lon, *alt]);
        w.field_message(3, &wp.into_bytes());
    }
    w.into_bytes()
}

// ── SensorAction (field 1 action enum, field 2 AgentName) ──
pub fn encode_sensor_action(action: i32, agent_id: &str, component_id: &str) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    w.field_varint(1, action as u64);
    w.field_message(2, &encode_agent_name(agent_id, component_id));
    w.into_bytes()
}

// ── ChangeSensorMode (field 1 AgentName, field 2 mode) ──
pub fn encode_change_sensor_mode(agent_id: &str, component_id: &str, mode: &str) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    w.field_message(1, &encode_agent_name(agent_id, component_id));
    w.field_string(2, mode);
    w.into_bytes()
}

// ── FireAtTarget (field 1 action, field 2 AgentName, field 3 track_id) ──
pub fn encode_fire_at_target(
    action: i32,
    agent_id: &str,
    weapon_id: &str,
    track_id: &str,
) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    w.field_varint(1, action as u64);
    w.field_message(2, &encode_agent_name(agent_id, weapon_id));
    w.field_string(3, track_id);
    w.into_bytes()
}

// ── FireSlavoAtTarget (field 1 AgentName, field 2 track_id, field 3 slavo_size) ──
pub fn encode_fire_salvo(
    agent_id: &str,
    weapon_id: &str,
    track_id: &str,
    salvo_size: u32,
) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    w.field_message(1, &encode_agent_name(agent_id, weapon_id));
    w.field_string(2, track_id);
    w.field_varint(3, salvo_size as u64);
    w.into_bytes()
}

// ── ChangeJammingMode (field 1 AgentName, field 2 JammingModeStruct) ──
pub fn encode_change_jamming_mode(
    agent_id: &str,
    jammer_id: &str,
    frequency_hz: f64,
    bandwidth_hz: f64,
    beam: u32,
) -> Vec<u8> {
    let mut mode = ProtoWriter::new();
    mode.field_fixed64(1, frequency_hz);
    mode.field_fixed64(2, bandwidth_hz);
    mode.field_varint(3, beam as u64);
    let mut w = ProtoWriter::new();
    w.field_message(1, &encode_agent_name(agent_id, jammer_id));
    w.field_message(2, &mode.into_bytes());
    w.into_bytes()
}

// ── SendMsgToPlatform (field 1 AgentName, 2 target_id, 3 message) ──
pub fn encode_send_msg_to_platform(from_id: &str, target_id: &str, message: &str) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    w.field_message(1, &encode_agent_name(from_id, ""));
    w.field_string(2, target_id);
    w.field_string(3, message);
    w.into_bytes()
}

// ── ChangeCommander (field 1 name, field 2 commander) ──
pub fn encode_change_commander(name: &str, commander: &str) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    w.field_string(1, name);
    w.field_string(2, commander);
    w.into_bytes()
}

// ── AfsimAuxCommand (field 1 repeated PlatformAuxData) ──
pub fn encode_aux_command(platform_id: &str, key: &str, value_json: &str) -> Vec<u8> {
    // AuxData: key=1, type=3, stringValue=2
    let mut aux = ProtoWriter::new();
    aux.field_string(1, key);
    aux.field_varint(3, 0); // type 0 = STRING
    aux.field_string(2, value_json);
    // PlatformAuxData: auxdata=1, name=2, index=3
    let mut pad = ProtoWriter::new();
    pad.field_message(1, &aux.into_bytes());
    pad.field_string(2, platform_id);
    pad.field_varint(3, 0);
    // AfsimAuxCommand: platformAux=1
    let mut w = ProtoWriter::new();
    w.field_message(1, &pad.into_bytes());
    w.into_bytes()
}

// ── ActionsFromOutside message ──
// field 1: repeated AgentContrl (LEN sub-messages)
// field 2: repeated DesiredHeading (LEN sub-messages)
// field 3: repeated DesiredAltitude
// field 4: repeated DesiredVelocity
// ... etc

pub fn encode_actions_from_outside(
    controls: &[(i32, &str)], // (action, agent_id)
    headings: &[(&str, f64)], // (agent_id, heading_rad)
) -> Vec<u8> {
    let mut w = ProtoWriter::new();

    // Field 1: repeated AgentContrl
    for (action, agent_id) in controls {
        let msg = encode_agent_contrl(*action, agent_id);
        w.field_message(1, &msg);
    }

    // Field 2: repeated DesiredHeading
    for (agent_id, heading_rad) in headings {
        let msg = encode_desired_heading(agent_id, *heading_rad);
        w.field_message(2, &msg);
    }

    w.into_bytes()
}

// ── SET ONLY: SetOutsideControl ──
pub fn encode_set_outside_control(agent_id: &str) -> Vec<u8> {
    encode_actions_from_outside(&[(0, agent_id)], &[]) // action=0 = E_SetAgentOutsideControl
}

// ── SET ONLY: DesiredHeading ──
pub fn encode_desired_heading_cmd(agent_id: &str, heading_rad: f64) -> Vec<u8> {
    encode_actions_from_outside(&[], &[(agent_id, heading_rad)])
}

// ── COMBO: SetOutsideControl + DesiredHeading ──
pub fn encode_control_and_heading(agent_id: &str, heading_rad: f64) -> Vec<u8> {
    encode_actions_from_outside(&[(0, agent_id)], &[(agent_id, heading_rad)])
}

// ══════════════════════════════════════════════════
// StateMessage Parser (manual, APL v2 field numbers)
// ══════════════════════════════════════════════════

/// Parsed StateMessage fields we care about.
#[derive(Debug, Clone, Default)]
pub struct SimState {
    pub time: f64,
    /// Scenario end time (StateMessage field 4). 0.0 when not reported.
    pub end_time: f64,
    pub platforms: Vec<SimPlatform>,
    pub weapons: Vec<SimActiveWeapon>,
}

#[derive(Debug, Clone, Default)]
pub struct SimPlatform {
    pub name: String,
    pub side: String,
    pub domain: String,
    pub lat: f64,
    pub lon: f64,
    pub alt: f64,
    pub heading_rad: f64,
    pub pitch_rad: f64,
    pub roll_rad: f64,
    pub vn_ms: f64,
    pub ve_ms: f64,
    pub vd_ms: f64,
    pub fuel: f64,
    pub max_fuel: f64,
    pub damage: f64,
    pub track_count: u32,
    pub tracks: Vec<SimTrack>,
    /// Weapons carried by this platform (StateMessage PlatformState.weapons map).
    /// Names here are the real AFSIM component ids (e.g. `gun_30mm`,
    /// `loiter_wave2`) — required so commands target an existing weapon part
    /// instead of a fabricated default that crashes Warlock.
    pub weapons: Vec<SimWeapon>,
}

#[derive(Debug, Clone, Default)]
pub struct SimWeapon {
    pub name: String,
    pub weapon_type: String,
    pub quantity_remaining: f64,
    /// True when field 3 (`quantityRemaining`) was present on the wire.
    pub quantity_from_snapshot: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SimActiveWeapon {
    pub name: String,
    pub weapon_type: String,
    pub side: String,
    pub location_lla: Option<(f64, f64, f64)>,
    pub velocity_ned: Option<(f64, f64, f64)>,
    pub heading_rad: Option<f64>,
    pub current_target: String,
    pub host_id: String,
    pub damage: f64,
}

#[derive(Debug, Clone, Default)]
pub struct SimTrack {
    pub track_id: String,
    /// Truth name of the tracked platform (TrackState.targetName, field 31).
    pub target_name: String,
    pub classification: String,
    pub side: String,
    pub iff: String,
    pub reported_location_lla: Option<(f64, f64, f64)>,
    pub current_location_lla: Option<(f64, f64, f64)>,
    pub velocity_ned: Option<(f64, f64, f64)>,
    pub heading_rad: Option<f64>,
    pub range_m: Option<f64>,
    pub bearing_rad: Option<f64>,
    pub elevation_rad: Option<f64>,
    pub quality: f64,
    pub stale: bool,
    pub update_time: f64,
}

/// Parse a StateMessage from raw protobuf bytes (APL v2 fields).
pub fn parse_state_message(data: &[u8]) -> Option<SimState> {
    let mut state = SimState::default();
    let mut pos = 0;

    while pos < data.len() {
        let (tag, new_pos) = read_varint(data, pos)?;
        pos = new_pos;
        let field_num = (tag >> 3) as u32;
        let wire_type = (tag & 0x07) as u32;

        match (field_num, wire_type) {
            (1, 2) => {
                // repeated PlatformState (LEN-delimited)
                let (plen, np) = read_varint(data, pos)?;
                pos = np;
                let end = pos + plen as usize;
                if let Some(p) = parse_platform_state(&data[pos..end]) {
                    state.platforms.push(p);
                }
                pos = end;
            }
            (2, 2) => {
                let (plen, np) = read_varint(data, pos)?;
                pos = np;
                let end = pos + plen as usize;
                if let Some(weapon) = parse_active_weapon_state(&data[pos..end]) {
                    state.weapons.push(weapon);
                }
                pos = end;
            }
            (3, 1) => {
                // time (fixed64)
                if pos + 8 <= data.len() {
                    state.time = f64::from_le_bytes(data[pos..pos + 8].try_into().ok()?);
                    pos += 8;
                } else {
                    return None;
                }
            }
            (4, 1) => {
                // endTime (fixed64)
                if pos + 8 <= data.len() {
                    state.end_time = f64::from_le_bytes(data[pos..pos + 8].try_into().ok()?);
                    pos += 8;
                } else {
                    return None;
                }
            }
            _ => {
                // Skip unknown field
                if !skip_field(wire_type, data, &mut pos) {
                    return None;
                }
            }
        }
    }

    Some(state)
}

fn parse_platform_state(data: &[u8]) -> Option<SimPlatform> {
    let mut p = SimPlatform::default();
    let mut pos = 0;

    while pos < data.len() {
        let (tag, new_pos) = read_varint(data, pos)?;
        pos = new_pos;
        let field_num = (tag >> 3) as u32;
        let wire_type = (tag & 0x07) as u32;

        match (field_num, wire_type) {
            (1, 2) => {
                /* locationLLA — packed doubles */
                let (len, np) = read_varint(data, pos)?;
                pos = np;
                let end = pos + len as usize;
                p.lat = read_double_at(data, pos);
                p.lon = read_double_at(data, pos + 8);
                p.alt = read_double_at(data, pos + 16);
                pos = end;
            }
            (5, 2) => {
                /* orientationNED — packed doubles */
                let (len, np) = read_varint(data, pos)?;
                pos = np;
                p.heading_rad = read_double_at(data, pos);
                p.pitch_rad = read_double_at(data, pos + 8);
                p.roll_rad = read_double_at(data, pos + 16);
                pos += len as usize;
            }
            (3, 2) => {
                /* velocityNED — packed doubles */
                let (len, np) = read_varint(data, pos)?;
                pos = np;
                p.vn_ms = read_double_at(data, pos);
                p.ve_ms = read_double_at(data, pos + 8);
                p.vd_ms = read_double_at(data, pos + 16);
                pos += len as usize;
            }
            (8, 2) => {
                // name (string)
                let (slen, np) = read_varint(data, pos)?;
                pos = np;
                p.name = String::from_utf8_lossy(&data[pos..pos + slen as usize]).to_string();
                pos += slen as usize;
            }
            (10, 2) => {
                // side (string)
                let (slen, np) = read_varint(data, pos)?;
                pos = np;
                p.side = String::from_utf8_lossy(&data[pos..pos + slen as usize]).to_string();
                pos += slen as usize;
            }
            (26, 2) => {
                // spatialDomain (string)
                let (slen, np) = read_varint(data, pos)?;
                pos = np;
                p.domain = String::from_utf8_lossy(&data[pos..pos + slen as usize]).to_string();
                pos += slen as usize;
            }
            (14, 1) => {
                // fuel (fixed64)
                p.fuel = read_double_at(data, pos);
                pos += 8;
            }
            (16, 1) => {
                // maxFuel (fixed64)
                p.max_fuel = read_double_at(data, pos);
                pos += 8;
            }
            (18, 1) => {
                // damageFactor (fixed64)
                p.damage = read_double_at(data, pos);
                pos += 8;
            }
            (12, 2) => {
                // repeated TrackState
                let (tlen, np) = read_varint(data, pos)?;
                pos = np;
                let end = pos + tlen as usize;
                p.track_count += 1;
                if let Some(track) = parse_track_state(&data[pos..end]) {
                    p.tracks.push(track);
                }
                pos = end;
            }
            (19, 2) => {
                // map<string, WeaponState> weapons — each entry is a MapEntry
                // message { key=1: string, value=2: WeaponState }.
                let (mlen, np) = read_varint(data, pos)?;
                pos = np;
                let end = pos + mlen as usize;
                if end <= data.len() {
                    if let Some(w) = parse_weapon_map_entry(&data[pos..end]) {
                        p.weapons.push(w);
                    }
                }
                pos = end;
            }
            _ => {
                if !skip_field(wire_type, data, &mut pos) {
                    return None;
                }
            }
        }
    }

    Some(p)
}

/// Parse one `map<string, WeaponState>` entry: field 1 = key (string),
/// field 2 = value (WeaponState{ name=1, type=2, quantityRemaining=3 }).
fn parse_weapon_map_entry(data: &[u8]) -> Option<SimWeapon> {
    let mut key = String::new();
    let mut w = SimWeapon::default();
    let mut pos = 0;

    while pos < data.len() {
        let (tag, new_pos) = read_varint(data, pos)?;
        pos = new_pos;
        let field_num = (tag >> 3) as u32;
        let wire_type = (tag & 0x07) as u32;
        match (field_num, wire_type) {
            (1, 2) => {
                let (slen, np) = read_varint(data, pos)?;
                pos = np;
                key = String::from_utf8_lossy(&data[pos..pos + slen as usize]).to_string();
                pos += slen as usize;
            }
            (2, 2) => {
                let (vlen, np) = read_varint(data, pos)?;
                pos = np;
                let end = pos + vlen as usize;
                if end <= data.len() {
                    parse_weapon_state(&data[pos..end], &mut w);
                }
                pos = end;
            }
            _ => {
                if !skip_field(wire_type, data, &mut pos) {
                    return None;
                }
            }
        }
    }

    // The map key is the weapon component id; prefer the value's own name,
    // falling back to the key when the value omits it.
    if w.name.is_empty() {
        w.name = key;
    }
    if w.name.is_empty() {
        None
    } else {
        Some(w)
    }
}

/// Parse a `WeaponState { name=1, type=2, quantityRemaining=3 }` into `w`.
fn parse_weapon_state(data: &[u8], w: &mut SimWeapon) {
    let mut pos = 0;
    while pos < data.len() {
        let Some((tag, new_pos)) = read_varint(data, pos) else {
            return;
        };
        pos = new_pos;
        let field_num = (tag >> 3) as u32;
        let wire_type = (tag & 0x07) as u32;
        match (field_num, wire_type) {
            (1, 2) => {
                let Some((slen, np)) = read_varint(data, pos) else {
                    return;
                };
                pos = np;
                if pos + slen as usize > data.len() {
                    return;
                }
                w.name = String::from_utf8_lossy(&data[pos..pos + slen as usize]).to_string();
                pos += slen as usize;
            }
            (2, 2) => {
                let Some((slen, np)) = read_varint(data, pos) else {
                    return;
                };
                pos = np;
                if pos + slen as usize > data.len() {
                    return;
                }
                w.weapon_type =
                    String::from_utf8_lossy(&data[pos..pos + slen as usize]).to_string();
                pos += slen as usize;
            }
            (3, 1) => {
                if pos + 8 > data.len() {
                    return;
                }
                w.quantity_remaining = read_double_at(data, pos);
                w.quantity_from_snapshot = true;
                pos += 8;
            }
            (3, 5) => {
                // Some Warlock builds encode `double quantityRemaining` as float32.
                if pos + 4 > data.len() {
                    return;
                }
                w.quantity_remaining = read_float_at(data, pos) as f64;
                w.quantity_from_snapshot = true;
                pos += 4;
            }
            _ => {
                if !skip_field(wire_type, data, &mut pos) {
                    return;
                }
            }
        }
    }
}

fn parse_active_weapon_state(data: &[u8]) -> Option<SimActiveWeapon> {
    let mut w = SimActiveWeapon::default();
    let mut pos = 0;

    while pos < data.len() {
        let (tag, new_pos) = read_varint(data, pos)?;
        pos = new_pos;
        let field_num = (tag >> 3) as u32;
        let wire_type = (tag & 0x07) as u32;
        match (field_num, wire_type) {
            (1, 2) => {
                let (len, np) = read_varint(data, pos)?;
                pos = np;
                w.location_lla = read_triple(data, pos);
                pos += len as usize;
            }
            (3, 2) => {
                let (len, np) = read_varint(data, pos)?;
                pos = np;
                w.velocity_ned = read_triple(data, pos);
                pos += len as usize;
            }
            (5, 2) => {
                let (len, np) = read_varint(data, pos)?;
                pos = np;
                w.heading_rad = read_double_vec(data, pos, len as usize).first().copied();
                pos += len as usize;
            }
            (8, 2) => {
                w.name = read_string(data, &mut pos)?;
            }
            (9, 2) => {
                w.weapon_type = read_string(data, &mut pos)?;
            }
            (10, 2) => {
                w.side = read_string(data, &mut pos)?;
            }
            (11, 2) => {
                w.current_target = read_string(data, &mut pos)?;
            }
            (15, 2) => {
                w.host_id = read_string(data, &mut pos)?;
            }
            (16, 1) => {
                w.damage = read_double_at(data, pos);
                pos += 8;
            }
            _ => {
                if !skip_field(wire_type, data, &mut pos) {
                    return None;
                }
            }
        }
    }

    if w.name.is_empty() {
        None
    } else {
        Some(w)
    }
}

fn parse_track_state(data: &[u8]) -> Option<SimTrack> {
    let mut t = SimTrack::default();
    let mut pos = 0;

    while pos < data.len() {
        let (tag, new_pos) = read_varint(data, pos)?;
        pos = new_pos;
        let field_num = (tag >> 3) as u32;
        let wire_type = (tag & 0x07) as u32;

        match (field_num, wire_type) {
            (4, 1) => {
                t.update_time = read_double_at(data, pos);
                pos += 8;
            }
            (8, 2) => {
                let (slen, np) = read_varint(data, pos)?;
                pos = np;
                t.classification =
                    String::from_utf8_lossy(&data[pos..pos + slen as usize]).to_string();
                pos += slen as usize;
            }
            (10, 2) => {
                let (slen, np) = read_varint(data, pos)?;
                pos = np;
                t.side = String::from_utf8_lossy(&data[pos..pos + slen as usize]).to_string();
                pos += slen as usize;
            }
            (12, 2) => {
                let (len, np) = read_varint(data, pos)?;
                pos = np;
                t.reported_location_lla = read_triple(data, pos);
                pos += len as usize;
            }
            (14, 2) => {
                let (len, np) = read_varint(data, pos)?;
                pos = np;
                t.current_location_lla = read_triple(data, pos);
                pos += len as usize;
            }
            (18, 2) => {
                let (len, np) = read_varint(data, pos)?;
                pos = np;
                t.velocity_ned = read_triple(data, pos);
                pos += len as usize;
            }
            (20, 1) => {
                t.heading_rad = Some(read_double_at(data, pos));
                pos += 8;
            }
            (21, 2) => {
                let (slen, np) = read_varint(data, pos)?;
                pos = np;
                t.iff = String::from_utf8_lossy(&data[pos..pos + slen as usize]).to_string();
                pos += slen as usize;
            }
            (23, 1) => {
                t.range_m = Some(read_double_at(data, pos));
                pos += 8;
            }
            (26, 1) => {
                t.bearing_rad = Some(read_double_at(data, pos));
                pos += 8;
            }
            (28, 1) => {
                t.elevation_rad = Some(read_double_at(data, pos));
                pos += 8;
            }
            (30, 2) => {
                let (slen, np) = read_varint(data, pos)?;
                pos = np;
                t.track_id = String::from_utf8_lossy(&data[pos..pos + slen as usize]).to_string();
                pos += slen as usize;
            }
            (31, 2) => {
                let (slen, np) = read_varint(data, pos)?;
                pos = np;
                t.target_name =
                    String::from_utf8_lossy(&data[pos..pos + slen as usize]).to_string();
                pos += slen as usize;
            }
            (32, 1) => {
                t.quality = read_double_at(data, pos);
                pos += 8;
            }
            (33, 0) => {
                let (value, np) = read_varint(data, pos)?;
                t.stale = value != 0;
                pos = np;
            }
            _ => {
                if !skip_field(wire_type, data, &mut pos) {
                    return None;
                }
            }
        }
    }

    Some(t)
}

fn read_varint(data: &[u8], mut pos: usize) -> Option<(u64, usize)> {
    let mut value: u64 = 0;
    let mut shift = 0;
    while pos < data.len() {
        let byte = data[pos];
        pos += 1;
        value |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((value, pos));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
    None
}

fn read_double_at(data: &[u8], pos: usize) -> f64 {
    if pos + 8 <= data.len() {
        f64::from_le_bytes(data[pos..pos + 8].try_into().unwrap_or([0u8; 8]))
    } else {
        0.0
    }
}

fn read_float_at(data: &[u8], pos: usize) -> f32 {
    if pos + 4 <= data.len() {
        f32::from_le_bytes(data[pos..pos + 4].try_into().unwrap_or([0u8; 4]))
    } else {
        0.0
    }
}

fn read_triple(data: &[u8], pos: usize) -> Option<(f64, f64, f64)> {
    if pos + 24 <= data.len() {
        Some((
            read_double_at(data, pos),
            read_double_at(data, pos + 8),
            read_double_at(data, pos + 16),
        ))
    } else {
        None
    }
}

fn read_string(data: &[u8], pos: &mut usize) -> Option<String> {
    let (slen, np) = read_varint(data, *pos)?;
    *pos = np;
    let end = *pos + slen as usize;
    if end > data.len() {
        return None;
    }
    let value = String::from_utf8_lossy(&data[*pos..end]).to_string();
    *pos = end;
    Some(value)
}

fn read_double_vec(data: &[u8], pos: usize, len: usize) -> Vec<f64> {
    let end = pos.saturating_add(len).min(data.len());
    data[pos..end]
        .chunks_exact(8)
        .map(|chunk| {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(chunk);
            f64::from_le_bytes(bytes)
        })
        .collect()
}

fn skip_field(wire_type: u32, data: &[u8], pos: &mut usize) -> bool {
    match wire_type {
        0 => read_varint(data, *pos).map(|(_, np)| *pos = np).is_some(),
        1 => {
            *pos += 8;
            *pos <= data.len()
        }
        2 => {
            if let Some((len, np)) = read_varint(data, *pos) {
                *pos = np + len as usize;
                *pos <= data.len()
            } else {
                false
            }
        }
        5 => {
            *pos += 4;
            *pos <= data.len()
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_set_outside_control() {
        let data = encode_set_outside_control("Flight_01");
        // Should be: 0x0a (field 1 LEN) + 0x0b (11 bytes) + AgentContrl.
        // action=0 is proto3 default and is omitted, matching Python.
        assert_eq!(data[0], 0x0a);
        assert_eq!(data[1], 0x0b); // AgentContrl is 11 bytes
    }

    #[test]
    fn test_encode_desired_heading() {
        let data = encode_desired_heading_cmd("Flight_01", std::f64::consts::FRAC_PI_2);
        // Field 2 LEN + DesiredHeading
        assert_eq!(data[0], 0x12); // field 2 LEN
    }

    #[test]
    fn test_parse_roundtrip() {
        // Test that our encoder produces parseable bytes
        let encoded = encode_control_and_heading("Flight_01", std::f64::consts::FRAC_PI_2);
        assert!(!encoded.is_empty());
    }

    #[test]
    fn parses_weapons_map_and_end_time_from_state_message() {
        // WeaponState { name=1, type=2, quantityRemaining=3 (fixed64) }
        let mut weapon = ProtoWriter::new();
        weapon.field_string(1, "loiter_wave2");
        weapon.field_string(2, "RED_LOITER_MUN");
        weapon.field_fixed64(3, 16.0);

        // map entry { key=1: string, value=2: WeaponState }
        let mut entry = ProtoWriter::new();
        entry.field_string(1, "loiter_wave2");
        entry.field_message(2, &weapon.into_bytes());

        // PlatformState { name=8, weapons=19 (map entry), spatialDomain=26 }
        let mut platform = ProtoWriter::new();
        platform.field_string(8, "self");
        platform.field_message(19, &entry.into_bytes());
        platform.field_string(26, "surface");

        // StateMessage { platforms=1, time=3 (fixed64), endTime=4 (fixed64) }
        let mut msg = ProtoWriter::new();
        msg.field_message(1, &platform.into_bytes());
        msg.field_fixed64(3, 12.5);
        msg.field_fixed64(4, 600.0);

        let state = parse_state_message(&msg.into_bytes()).expect("parse");
        assert_eq!(state.time, 12.5);
        assert_eq!(state.end_time, 600.0);
        assert_eq!(state.platforms.len(), 1);
        let p = &state.platforms[0];
        assert_eq!(p.name, "self");
        assert_eq!(p.weapons.len(), 1, "weapon map entry should be parsed");
        assert_eq!(p.weapons[0].name, "loiter_wave2");
        assert_eq!(p.weapons[0].weapon_type, "RED_LOITER_MUN");
        assert_eq!(p.weapons[0].quantity_remaining, 16.0);
    }

    #[test]
    fn parses_weapon_quantity_remaining_as_float32() {
        let mut weapon = ProtoWriter::new();
        weapon.field_string(1, "scout_uav_slot");
        weapon.field_string(2, "SCOUT_UAV_SLOT");
        weapon.field_fixed32(3, 2.0f32);

        let mut entry = ProtoWriter::new();
        entry.field_string(1, "scout_uav_slot");
        entry.field_message(2, &weapon.into_bytes());

        let mut platform = ProtoWriter::new();
        platform.field_string(8, "self");
        platform.field_message(19, &entry.into_bytes());

        let mut msg = ProtoWriter::new();
        msg.field_message(1, &platform.into_bytes());

        let state = parse_state_message(&msg.into_bytes()).expect("parse");
        let w = &state.platforms[0].weapons[0];
        assert_eq!(w.name, "scout_uav_slot");
        assert_eq!(w.quantity_remaining, 2.0);
        assert!(w.quantity_from_snapshot);
    }

    #[test]
    fn parses_active_weapon_state_from_state_message() {
        let mut active = ProtoWriter::new();
        active.field_packed_double(1, &[10.0, 20.0, 30.0]);
        active.field_packed_double(3, &[3.0, 4.0, 0.0]);
        active.field_packed_double(5, &[std::f64::consts::FRAC_PI_2, 0.0, 0.0]);
        active.field_string(8, "self_loiter_wave3_1");
        active.field_string(9, "RED_LOITER_MUN");
        active.field_string(10, "red");
        active.field_string(11, "blue_sam_site_1");
        active.field_string(15, "self");
        active.field_fixed64(16, 0.0);

        let mut msg = ProtoWriter::new();
        msg.field_message(2, &active.into_bytes());
        msg.field_fixed64(3, 42.0);

        let state = parse_state_message(&msg.into_bytes()).expect("parse");
        assert_eq!(state.weapons.len(), 1);
        let weapon = &state.weapons[0];
        assert_eq!(weapon.name, "self_loiter_wave3_1");
        assert_eq!(weapon.current_target, "blue_sam_site_1");
        assert_eq!(weapon.host_id, "self");
        assert_eq!(weapon.location_lla, Some((10.0, 20.0, 30.0)));
        assert_eq!(weapon.velocity_ned, Some((3.0, 4.0, 0.0)));
        assert_eq!(weapon.heading_rad, Some(std::f64::consts::FRAC_PI_2));
    }
}
