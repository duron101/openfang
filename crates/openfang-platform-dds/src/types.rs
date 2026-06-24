//! DDS topic type definitions (IDL equivalents) and QoS profiles.
//!
//! These types match the DDS IDL defined in the plan:
//! - nav/NavPosition: platform pose + velocity
//! - nav/NavCommand: desired heading/speed/altitude
//! - sensor/RadarTrack: detected track data
//! - sensor/SensorCommand: sensor mode control
//! - weapon/WeaponStatus: weapon state + BIT
//! - weapon/WeaponCommand: fire/chaff/jam commands
//! - platform/Heartbeat: liveness + resource usage
//! - platform/Alert: severity-based alerts

use serde::{Deserialize, Serialize};

// ── DDS QoS Profiles ──

/// DDS Quality of Service profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DdsQosProfile {
    pub reliability: ReliabilityKind,
    pub durability: DurabilityKind,
    pub history_depth: u32,
    pub deadline_ms: Option<u64>,
    pub liveliness: LivelinessKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReliabilityKind {
    Reliable,
    BestEffort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DurabilityKind {
    Volatile,
    TransientLocal,
    Transient,
    Persistent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LivelinessKind {
    Automatic,
    ManualByParticipant,
    ManualByTopic,
}

impl DdsQosProfile {
    pub fn reliable_keep_last(depth: u32) -> Self {
        Self {
            reliability: ReliabilityKind::Reliable,
            durability: DurabilityKind::Volatile,
            history_depth: depth,
            deadline_ms: None,
            liveliness: LivelinessKind::Automatic,
        }
    }

    pub fn best_effort_keep_last(depth: u32) -> Self {
        Self {
            reliability: ReliabilityKind::BestEffort,
            durability: DurabilityKind::Volatile,
            history_depth: depth,
            deadline_ms: None,
            liveliness: LivelinessKind::Automatic,
        }
    }

    pub fn reliable_transient_local() -> Self {
        Self {
            reliability: ReliabilityKind::Reliable,
            durability: DurabilityKind::TransientLocal,
            history_depth: 1,
            deadline_ms: None,
            liveliness: LivelinessKind::Automatic,
        }
    }
}

impl Default for DdsQosProfile {
    fn default() -> Self {
        Self::reliable_keep_last(10)
    }
}

// ── DDS Topic Structs (IDL equivalents) ──

/// nav/NavPosition topic — published by INS/GPS at 10-50 Hz
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NavPosition {
    pub platform_id: String,
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub alt_m: f64,
    pub heading_deg: f64,
    pub pitch_deg: f64,
    pub roll_deg: f64,
    pub speed_ms: f64,
    pub vertical_rate_ms: f64,
    pub course_deg: f64,
    pub nav_source: String, // "gps", "ins", "dead_reckoning"
    pub accuracy_cep_m: f64,
    pub timestamp_us: u64,
}

/// nav/NavCommand topic — published by Agent at decision rate
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NavCommand {
    pub platform_id: String,
    pub command_type: NavCommandType,
    pub target_heading_deg: Option<f64>,
    pub target_speed_ms: Option<f64>,
    pub target_altitude_m: Option<f64>,
    pub waypoints: Vec<DdsWaypoint>,
    pub sequence_id: u64,
    pub timestamp_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NavCommandType {
    SetHeading,
    SetSpeed,
    SetAltitude,
    GotoLocation,
    FollowRoute,
    Loiter,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DdsWaypoint {
    pub lat: f64,
    pub lon: f64,
    pub alt: Option<f64>,
    pub speed_ms: Option<f64>,
}

/// sensor/RadarTrack topic — published by sensor processor at 1-10 Hz
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RadarTrack {
    pub track_id: String,
    pub classification: String,
    pub affiliation: String, // "friend", "foe", "neutral", "unknown"
    pub lat_deg: Option<f64>,
    pub lon_deg: Option<f64>,
    pub alt_m: Option<f64>,
    pub heading_deg: Option<f64>,
    pub speed_ms: Option<f64>,
    pub range_m: Option<f64>,
    pub bearing_deg: Option<f64>,
    pub quality: f64,
    pub stale: bool,
    pub detecting_platform_id: String,
    pub timestamp_us: u64,
}

/// sensor/SensorCommand topic — published by Agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorCommand {
    pub platform_id: String,
    pub sensor_id: String,
    pub command: SensorCmdType,
    pub mode: Option<String>,
    pub timestamp_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SensorCmdType {
    TurnOn,
    TurnOff,
    SetMode,
    GetMode,
}

/// weapon/WeaponCommand topic — published by Agent (must go through ApprovalManager)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeaponCommand {
    pub platform_id: String,
    pub weapon_id: String,
    pub command: WeaponCmdType,
    pub track_id: Option<String>,
    pub salvo_size: Option<u32>,
    pub params: Vec<f64>, // extra parameters (chaff count, interval, etc.)
    pub authorization_token: String, // HMAC from ApprovalManager quorum
    pub timestamp_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WeaponCmdType {
    FireAtTarget,
    FireSalvo,
    FireChaff,
    UpdateTarget,
    Arm,
    Disarm,
    BIT,
}

/// fleet/FleetCommand topic — published by a mothership to task child UAVs
/// (launch, recover, assign mission, target handoff). Track 2 §2B.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetCommand {
    pub command: FleetCmdType,
    /// Subject UAV (for launch/recover/assign) or handoff destination.
    pub uav_id: String,
    /// Source platform (for target handoff).
    pub from_platform_id: Option<String>,
    /// Mission type for AssignMission ("area_search", "strike", "bda", …).
    pub mission_type: Option<String>,
    /// Mission parameters / track id payload as JSON.
    pub params_json: Option<String>,
    /// Track id for target handoff.
    pub track_id: Option<String>,
    pub timestamp_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FleetCmdType {
    LaunchUav,
    RecoverUav,
    ReturnToBase,
    AssignMission,
    AbortMission,
    HandoffTarget,
}

/// weapon/WeaponStatus topic — published by weapon controller at 1 Hz
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeaponStatus {
    pub platform_id: String,
    pub weapon_id: String,
    pub weapon_type: String,
    pub quantity_remaining: f64,
    pub is_ready: bool,
    pub is_armed: bool,
    pub bit_passed: bool,
    pub fault_code: Option<String>,
    pub timestamp_us: u64,
}

/// platform/Heartbeat topic — published by each platform at 1 Hz
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heartbeat {
    pub platform_id: String,
    pub uptime_s: u64,
    pub cpu_pct: f32,
    pub mem_mb: f64,
    pub disk_mb: f64,
    pub link_quality: f64,     // 0.0-1.0
    pub autonomy_mode: String, // "L3", "L4", "L5"
    pub timestamp_us: u64,
}

/// platform/Alert topic — published on critical events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    pub severity: AlertSeverity,
    pub source_platform_id: String,
    pub component: String,
    pub message: String,
    pub timestamp_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AlertSeverity {
    Info,
    Warning,
    Error,
    Critical,
}
