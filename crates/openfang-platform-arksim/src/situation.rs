//! ArkService 定制态势（`rate = 0`）解析与默认配置。
//!
//! ArkSIM 有两种态势输出（见 `interface_new.json`）：
//! - **定制态势** (`changesituation.rate = 0`) → `arksimproto.proto` / JSON `customizedsituation`
//! - **实时态势** (`rate = 1`) → `zmq_observer_pb3.proto`
//!
//! OpenFang **默认只处理定制态势**；仿真控制、实体控制和态势观测均走
//! ArkService ZMQ ROUTER/DEALER `60004` 端口。

use openfang_types::platform::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 态势输出类型（ArkService `changesituation.rate`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum SituationKind {
    /// 定制态势 — `arksimproto.proto` / JSON `customizedsituation`（**默认**）
    #[default]
    Customized = 0,
    /// 实时战场态势 — `zmq_observer_pb3.proto`（OpenFang 不解析）
    Realtime = 1,
}

impl SituationKind {
    pub fn rate(self) -> u8 {
        self as u8
    }
}

/// 连接 ArkService 后应发送的默认定制态势配置（`rate = 0`）。
pub fn default_situation_commands(uuid: &str, interval_secs: f64) -> Vec<Value> {
    vec![
        serde_json::json!({
            "fn": "changesituation",
            "rate": SituationKind::Customized.rate(),
            "uuid": uuid,
        }),
        serde_json::json!({
            "fn": "customizedsituation",
            "time": interval_secs,
            "uuid": uuid,
        }),
    ]
}

/// 从 ArkService 任意回包中提取定制态势并转为 [`WorldSnapshot`]。
///
/// 兼容键名：`customizedsituation` / `situation` / `state`，以及嵌套在 `data` 内的情况
/// （与 `protobuf/arkcomm/response_handler.py` 一致）。
pub fn snapshot_from_arkservice_message(msg: &Value) -> Option<WorldSnapshot> {
    extract_customized_body(msg).map(from_customized_json)
}

/// Inbound ArkService message category — mirrors Python `MESSAGE_KEYS` routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArkMessageKind {
    Command,
    CustomizedSituation,
    Progress,
    Scenarios,
}

/// Classify and normalize an inbound frame (mutates `msg` like Python
/// `_determine_message_type` hoisting nested keys).
pub fn classify_message(msg: &mut Value) -> ArkMessageKind {
    hoist_nested_situation_fields(msg);

    if msg.get("customizedsituation").is_some()
        || msg.get("situation").is_some()
        || msg.get("state").is_some()
    {
        return ArkMessageKind::CustomizedSituation;
    }
    if msg.get("progressValue").is_some() {
        return ArkMessageKind::Progress;
    }
    if msg.get("scenarios").is_some() {
        return ArkMessageKind::Scenarios;
    }
    if msg.get("platforms").is_some() || msg.get("Weapons").is_some() {
        return ArkMessageKind::CustomizedSituation;
    }
    for key in ["cmd", "code", "fn"] {
        if msg.get(key).is_some() {
            return ArkMessageKind::Command;
        }
    }
    ArkMessageKind::Command
}

fn hoist_nested_situation_fields(msg: &mut Value) {
    let Some(obj) = msg.as_object_mut() else {
        return;
    };
    for (_key, value) in obj.clone().iter() {
        if let Some(nested) = value.as_object() {
            for sit_key in ["customizedsituation", "situation", "state"] {
                if nested.contains_key(sit_key) {
                    if let Some(v) = nested.get(sit_key) {
                        obj.insert(sit_key.to_string(), v.clone());
                    }
                }
            }
            if nested.contains_key("progressValue") {
                if let Some(v) = nested.get("progressValue") {
                    obj.insert("progressValue".to_string(), v.clone());
                }
            }
        }
    }
    if let Some(data) = obj.get("data").cloned() {
        if let Some(data_obj) = data.as_object() {
            for sit_key in ["customizedsituation", "state"] {
                if let Some(v) = data_obj.get(sit_key) {
                    obj.insert(sit_key.to_string(), v.clone());
                }
            }
            for cmd_key in ["code", "cmd"] {
                if let Some(v) = data_obj.get(cmd_key) {
                    obj.insert(cmd_key.to_string(), v.clone());
                }
            }
        }
    }
}

fn extract_customized_body(msg: &Value) -> Option<Value> {
    for key in ["customizedsituation", "situation", "state"] {
        if let Some(body) = msg.get(key) {
            return Some(body.clone());
        }
    }
    if let Some(data) = msg.get("data") {
        for key in ["customizedsituation", "situation", "state"] {
            if let Some(body) = data.get(key) {
                return Some(body.clone());
            }
        }
    }
    // Some ArkService builds push bare StateMessage JSON at the top level.
    if msg.get("platforms").and_then(|v| v.as_array()).is_some() {
        return Some(msg.clone());
    }
    None
}

/// 将定制态势 JSON 体（`StateMessage` 的 JSON 投影）映射为 [`WorldSnapshot`]。
pub fn from_customized_json(body: Value) -> WorldSnapshot {
    let time = body.get("time").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let platforms = body
        .get("platforms")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(map_platform_json).collect())
        .unwrap_or_default();

    WorldSnapshot {
        timestamp: time,
        platforms,
        active_munitions: vec![],
        events: vec![],
        fleet: None,
    }
}

fn map_platform_json(v: &Value) -> Option<PlatformState> {
    let name = v.get("name")?.as_str()?.to_string();
    let side = v.get("side").and_then(|s| s.as_str()).unwrap_or("");
    let domain_str = v
        .get("spatialDomain")
        .and_then(|s| s.as_str())
        .unwrap_or("unknown");
    let lla = v.get("locationLLA").and_then(as_f64_triple);
    let vel = v.get("velocityNED").and_then(as_f64_triple);
    let orient = v.get("orientationNED").and_then(as_f64_triple);

    let (lat, lon, alt) = lla.unwrap_or((0.0, 0.0, 0.0));
    let (vn, ve, vd) = vel.unwrap_or((0.0, 0.0, 0.0));
    let (heading_rad, pitch_rad, roll_rad) = orient.unwrap_or((0.0, 0.0, 0.0));
    let speed_ms = (vn * vn + ve * ve).sqrt();
    let mut course_deg = ve.atan2(vn).to_degrees();
    if course_deg < 0.0 {
        course_deg += 360.0;
    }

    let platform_type = v
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("unknown")
        .to_string();

    let tracks = v
        .get("tracks")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(map_track_json)
                .collect::<Vec<Track>>()
        })
        .unwrap_or_default();

    let onboard_weapons = map_weapons_json(v.get("weapons"));

    Some(PlatformState {
        id: name.clone(),
        name,
        platform_type,
        affiliation: map_affiliation(side),
        domain: map_domain(domain_str),
        pose: Pose {
            lat_deg: lat,
            lon_deg: lon,
            alt_m: alt,
            heading_deg: heading_rad.to_degrees(),
            pitch_deg: pitch_rad.to_degrees(),
            roll_deg: roll_rad.to_degrees(),
        },
        velocity: Velocity {
            speed_ms,
            vertical_rate_ms: vd,
            course_deg,
        },
        fuel: FuelStatus {
            remaining_kg: v.get("fuel").and_then(|f| f.as_f64()).unwrap_or(0.0),
            max_kg: v.get("maxFuel").and_then(|f| f.as_f64()).unwrap_or(0.0),
            consumption_rate_kg_s: v
                .get("fuelConsumptionRate")
                .and_then(|f| f.as_f64())
                .unwrap_or(0.0),
        },
        damage: v
            .get("damageFactor")
            .and_then(|f| f.as_f64())
            .unwrap_or(0.0),
        tracks,
        onboard_sensors: vec![],
        onboard_weapons,
        onboard_jammers: vec![],
        current_target: v
            .get("currentTarget")
            .and_then(|t| t.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from),
        commander: v
            .get("commander")
            .and_then(|c| c.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from),
        survivability: None,
        emcon: None,
        link: None,
    })
}

fn map_track_json(v: &Value) -> Option<Track> {
    let track_id = v
        .get("trackId")
        .or_else(|| v.get("track_id"))
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    if track_id.is_empty() {
        return None;
    }

    let side = v.get("side").and_then(|s| s.as_str()).unwrap_or("");
    let target_name = v
        .get("targetName")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    let classification = v
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("unknown")
        .to_string();
    let iff = v
        .get("iff")
        .and_then(|i| i.as_str())
        .unwrap_or("unknown")
        .to_string();

    let position_lla = v
        .get("reportedLocationLLA")
        .or_else(|| v.get("currentLocationLLA"))
        .and_then(as_f64_triple);

    let heading_deg = v
        .get("heading")
        .and_then(|h| h.as_f64())
        .map(|r| r.to_degrees());

    let speed_ms = v
        .get("velocityNED")
        .and_then(as_f64_triple)
        .map(|(vn, ve, _)| (vn * vn + ve * ve).sqrt());

    Some(Track {
        track_id,
        target_name,
        classification,
        affiliation: map_affiliation(side),
        iff,
        position_lla,
        heading_deg,
        speed_ms,
        range_m: v.get("range").and_then(|r| r.as_f64()),
        bearing_deg: v
            .get("bearing")
            .and_then(|b| b.as_f64())
            .map(|r| r.to_degrees()),
        elevation_deg: v
            .get("elevation")
            .and_then(|e| e.as_f64())
            .map(|r| r.to_degrees()),
        quality: v
            .get("trackQuality")
            .and_then(|q| q.as_f64())
            .unwrap_or(0.0),
        stale: v.get("stale").and_then(|s| s.as_bool()).unwrap_or(false),
        last_update_s: v.get("updateTime").and_then(|t| t.as_f64()).unwrap_or(0.0),
        is_active: !v.get("stale").and_then(|s| s.as_bool()).unwrap_or(false),
    })
}

fn map_weapons_json(v: Option<&Value>) -> Vec<WeaponState> {
    let Some(v) = v else {
        return vec![];
    };

    match v {
        Value::Object(map) => map
            .iter()
            .filter_map(|(weapon_id, entry)| map_weapon_entry(weapon_id, entry))
            .collect(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|entry| {
                let id = entry.get("name").and_then(|n| n.as_str()).unwrap_or("");
                map_weapon_entry(id, entry)
            })
            .collect(),
        _ => vec![],
    }
}

fn map_weapon_entry(weapon_id: &str, entry: &Value) -> Option<WeaponState> {
    if weapon_id.is_empty() {
        return None;
    }
    let weapon_type = entry
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("unknown")
        .to_string();
    let quantity_from_snapshot = entry
        .get("quantityRemaining")
        .and_then(|q| q.as_f64())
        .is_some();
    let quantity_remaining = entry
        .get("quantityRemaining")
        .and_then(|q| q.as_f64())
        .unwrap_or(0.0);
    Some(WeaponState {
        weapon_id: weapon_id.to_string(),
        weapon_type,
        quantity_remaining,
        max_range_m: None,
        min_range_m: None,
        guidance_type: None,
        speed_ms: None,
        is_ready: quantity_from_snapshot && quantity_remaining > 0.0,
        quantity_from_snapshot,
    })
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

fn map_domain(domain: &str) -> Domain {
    match domain {
        "surface" => Domain::Surface,
        "air" => Domain::Air,
        "subsurface" => Domain::Subsurface,
        "land" => Domain::Land,
        "space" => Domain::Space,
        _ => Domain::Unknown,
    }
}

fn as_f64_triple(v: &Value) -> Option<(f64, f64, f64)> {
    let arr = v.as_array()?;
    if arr.len() < 3 {
        return None;
    }
    Some((arr[0].as_f64()?, arr[1].as_f64()?, arr[2].as_f64()?))
}

/// 若消息为实时态势（`rate = 1` 或未识别），返回 `true` — 调用方应忽略。
pub fn is_realtime_situation_message(msg: &Value) -> bool {
    msg.get("rate")
        .and_then(|r| r.as_u64())
        .is_some_and(|r| r == SituationKind::Realtime as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_kind_is_customized() {
        assert_eq!(SituationKind::default(), SituationKind::Customized);
        assert_eq!(SituationKind::Customized.rate(), 0);
    }

    #[test]
    fn default_commands_use_rate_zero() {
        let cmds = default_situation_commands("abc", 3.0);
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0]["fn"], "changesituation");
        assert_eq!(cmds[0]["rate"], 0);
        assert_eq!(cmds[1]["fn"], "customizedsituation");
        assert_eq!(cmds[1]["time"], 3.0);
    }

    #[test]
    fn parses_usv_platform_with_tracks() {
        let body = serde_json::json!({
            "time": 100.0,
            "endTime": 18000.0,
            "platforms": [{
                "name": "usv_mothership_1",
                "type": "JARI_USV_MOTHERSHIP",
                "side": "red",
                "spatialDomain": "surface",
                "locationLLA": [20.5, 122.5, 0.0],
                "velocityNED": [10.0, 5.0, 0.0],
                "orientationNED": [std::f64::consts::FRAC_PI_2, 0.0, 0.0],
                "fuel": 8000.0,
                "maxFuel": 10000.0,
                "damageFactor": 0.0,
                "tracks": [{
                    "trackId": "xq58a_b1:1",
                    "type": "BLUE_PATROL_BOAT",
                    "side": "blue",
                    "iff": "foe",
                    "reportedLocationLLA": [20.4, 122.3, 0.0],
                    "trackQuality": 0.85,
                    "stale": false,
                    "updateTime": 99.0
                }],
                "weapons": {
                    "gun_30mm": { "type": "30MM_BULLET", "quantityRemaining": 1500.0 }
                }
            }]
        });

        let snap = from_customized_json(body);
        assert_eq!(snap.timestamp, 100.0);
        assert_eq!(snap.platforms.len(), 1);
        let usv = &snap.platforms[0];
        assert_eq!(usv.name, "usv_mothership_1");
        assert_eq!(usv.affiliation, Affiliation::Red);
        assert_eq!(usv.domain, Domain::Surface);
        assert_eq!(usv.tracks.len(), 1);
        assert_eq!(usv.tracks[0].track_id, "xq58a_b1:1");
        assert_eq!(usv.onboard_weapons.len(), 1);
        assert_eq!(usv.onboard_weapons[0].weapon_id, "gun_30mm");
    }

    #[test]
    fn json_side_mapping_accepts_common_arksim_variants() {
        let body = serde_json::json!({
            "time": 1.0,
            "platforms": [
                { "name": "blue-upper", "side": "BLUE", "spatialDomain": "surface" },
                { "name": "blue-force", "side": "blue force", "spatialDomain": "surface" },
                { "name": "friend", "side": "Friend", "spatialDomain": "surface" },
                { "name": "red-upper", "side": "RED", "spatialDomain": "surface" },
                { "name": "red-force", "side": "red-force", "spatialDomain": "surface" },
                { "name": "foe", "side": "Foe", "spatialDomain": "surface" }
            ]
        });
        let snap = from_customized_json(body);
        let sides: Vec<_> = snap.platforms.iter().map(|p| p.affiliation).collect();
        assert_eq!(
            sides,
            vec![
                Affiliation::Blue,
                Affiliation::Blue,
                Affiliation::Friend,
                Affiliation::Red,
                Affiliation::Red,
                Affiliation::Foe,
            ]
        );
    }

    #[test]
    fn arkservice_envelope_aliases() {
        let msg = serde_json::json!({
            "customizedsituation": {
                "time": 1.0,
                "platforms": [{ "name": "p1", "side": "red", "spatialDomain": "surface" }]
            }
        });
        let snap = snapshot_from_arkservice_message(&msg).unwrap();
        assert_eq!(snap.platforms[0].name, "p1");

        let wrapped = serde_json::json!({
            "data": { "state": { "time": 2.0, "platforms": [{ "name": "p2", "side": "blue", "spatialDomain": "air" }] } }
        });
        let snap2 = snapshot_from_arkservice_message(&wrapped).unwrap();
        assert_eq!(snap2.platforms[0].name, "p2");
    }

    #[test]
    fn bare_state_message_at_top_level() {
        let msg = serde_json::json!({
            "time": 3.0,
            "platforms": [{ "name": "p3", "side": "red", "spatialDomain": "surface" }]
        });
        let snap = snapshot_from_arkservice_message(&msg).unwrap();
        assert_eq!(snap.platforms[0].name, "p3");
    }

    #[test]
    fn classify_message_detects_nested_and_bare_situation() {
        let mut nested = serde_json::json!({
            "data": { "customizedsituation": { "time": 1.0, "platforms": [] } }
        });
        assert_eq!(
            classify_message(&mut nested),
            ArkMessageKind::CustomizedSituation
        );

        let mut bare = serde_json::json!({
            "time": 2.0,
            "platforms": [{ "name": "x" }]
        });
        assert_eq!(
            classify_message(&mut bare),
            ArkMessageKind::CustomizedSituation
        );

        let mut cmd = serde_json::json!({ "fn": "pause", "uuid": "abc" });
        assert_eq!(classify_message(&mut cmd), ArkMessageKind::Command);
    }
}
