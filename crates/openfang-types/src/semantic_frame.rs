//! Semantic intent frames between natural language and executable mission DSL.
//!
//! `CommanderFrame` captures a commander's effect-oriented intent without
//! choosing a concrete platform action. `TaskFrame` is the compiled, bound step
//! form used by workflow templates and step interpreters.

use serde::{Deserialize, Serialize};

use crate::cognition::TimeWindow;
use crate::mission_dsl::{MissionKind, PlatformCommandSpec, SafetyGuard};
use crate::platform::{CcaRole, TurnDirection, Waypoint};
use crate::umaa::WeaponReleaseLevel;

/// Top-level effect requested by the commander. This is intentionally more
/// stable than natural-language verbs and maps back to existing `MissionKind`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Reconnoiter,
    Surveil,
    Track,
    Suppress,
    Destroy,
    Escort,
    Screen,
    Deceive,
    Defend,
    Evade,
    Interdict,
    ReturnToBase,
    #[default]
    Unknown,
}

impl Effect {
    pub fn mission_kind(self) -> MissionKind {
        match self {
            Self::Reconnoiter | Self::Surveil => MissionKind::Recon,
            Self::Track => MissionKind::Track,
            Self::Suppress | Self::Destroy => MissionKind::Engage,
            Self::Escort => MissionKind::Escort,
            Self::Screen => MissionKind::Picket,
            Self::Deceive => MissionKind::Deception,
            Self::Defend => MissionKind::PointDefense,
            Self::Evade => MissionKind::ReactiveDefense,
            Self::Interdict => MissionKind::MaritimeInterdiction,
            Self::ReturnToBase => MissionKind::Rtb,
            Self::Unknown => MissionKind::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    Track,
    Label,
    Area,
    Asset,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApproachSide {
    Left,
    Right,
}

impl ApproachSide {
    pub fn turn_direction(self) -> TurnDirection {
        match self {
            Self::Left => TurnDirection::Left,
            Self::Right => TurnDirection::Right,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GeoPoint {
    pub lat: f64,
    pub lon: f64,
    #[serde(default)]
    pub alt: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GeoArea {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub center: Option<GeoPoint>,
    #[serde(default)]
    pub radius_m: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ObjectRef {
    pub kind: ObjectKind,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub track_id: Option<String>,
    #[serde(default)]
    pub area: Option<GeoArea>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Environment {
    #[serde(default)]
    pub area: Option<GeoArea>,
    #[serde(default)]
    pub approach: Option<ApproachSide>,
    #[serde(default)]
    pub standoff_m: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FrameConstraints {
    #[serde(default)]
    pub roe: Option<WeaponReleaseLevel>,
    #[serde(default)]
    pub time_window: Option<TimeWindow>,
    #[serde(default)]
    pub allow_degrade: bool,
    #[serde(default)]
    pub pid_required: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SubjectHint {
    #[serde(default)]
    pub platform_id: Option<String>,
    #[serde(default)]
    pub role: Option<CcaRole>,
    #[serde(default)]
    pub all_platforms: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommanderFrame {
    pub raw_text: String,
    pub effect: Effect,
    #[serde(default)]
    pub objects: Vec<ObjectRef>,
    #[serde(default)]
    pub environment: Environment,
    #[serde(default)]
    pub constraints: FrameConstraints,
    #[serde(default)]
    pub subject_hints: Vec<SubjectHint>,
    pub confidence: f64,
    pub rationale: String,
    /// String instead of runtime enum to keep `openfang-types` dependency-free.
    #[serde(default)]
    pub semantic_source: String,
}

impl CommanderFrame {
    pub fn unknown(raw_text: impl Into<String>) -> Self {
        Self {
            raw_text: raw_text.into(),
            effect: Effect::Unknown,
            objects: Vec::new(),
            environment: Environment::default(),
            constraints: FrameConstraints::default(),
            subject_hints: Vec::new(),
            confidence: 0.0,
            rationale: "unclassified intent".into(),
            semantic_source: "deterministic".into(),
        }
    }

    pub fn mission_kind(&self) -> MissionKind {
        self.effect.mission_kind()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectRef {
    Platform { platform_id: String, role: CcaRole },
    Role { role: CcaRole },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectBinding {
    #[serde(default)]
    pub kind: ObjectKind,
    #[serde(default)]
    pub track_id: Option<String>,
    #[serde(default)]
    pub asset_id: Option<String>,
    #[serde(default)]
    pub area: Option<GeoArea>,
    #[serde(default)]
    pub waypoints: Vec<Waypoint>,
    #[serde(default)]
    pub point: Option<GeoPoint>,
    #[serde(default)]
    pub sensor_id: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub weapon_id: Option<String>,
    #[serde(default)]
    pub salvo_size: Option<u32>,
    #[serde(default)]
    pub speed_ms: Option<f64>,
    #[serde(default)]
    pub heading_deg: Option<f64>,
    #[serde(default)]
    pub turn_direction: Option<TurnDirection>,
}

impl Default for ObjectBinding {
    fn default() -> Self {
        Self {
            kind: ObjectKind::Unknown,
            track_id: None,
            asset_id: None,
            area: None,
            waypoints: Vec::new(),
            point: None,
            sensor_id: None,
            mode: None,
            weapon_id: None,
            salvo_size: None,
            speed_ms: None,
            heading_deg: None,
            turn_direction: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Goto,
    FollowRoute,
    SetHeading,
    SetSpeed,
    SetAltitude,
    SensorOn,
    SensorOff,
    SensorSetMode,
    Employ,
    Track,
    Coordinate,
    Jam,
    Safe,
    #[default]
    Noop,
}

impl Action {
    pub fn is_lethal(self) -> bool {
        matches!(self, Self::Employ)
    }

    pub fn lower(self, binding: &ObjectBinding) -> Option<PlatformCommandSpec> {
        match self {
            Self::FollowRoute => Some(PlatformCommandSpec::FollowRoute {
                waypoints: binding.waypoints.clone(),
            })
            .filter(|_| !binding.waypoints.is_empty()),
            Self::Goto => binding
                .point
                .as_ref()
                .map(|point| PlatformCommandSpec::Goto {
                    lat: point.lat,
                    lon: point.lon,
                    alt: point.alt,
                    speed_ms: binding.speed_ms,
                }),
            Self::SetHeading => {
                binding
                    .heading_deg
                    .map(|heading_deg| PlatformCommandSpec::SetHeading {
                        heading_deg,
                        speed_ms: binding.speed_ms,
                        turn_direction: binding.turn_direction,
                    })
            }
            Self::SetSpeed => binding
                .speed_ms
                .map(|speed_ms| PlatformCommandSpec::SetSpeed { speed_ms }),
            Self::SensorOn => Some(PlatformCommandSpec::SensorOn {
                sensor_id: binding.sensor_id.clone().unwrap_or_default(),
            }),
            Self::SensorSetMode => Some(PlatformCommandSpec::SensorSetMode {
                sensor_id: binding.sensor_id.clone().unwrap_or_default(),
                mode: binding.mode.clone().unwrap_or_else(|| "track".into()),
            }),
            Self::Employ => {
                let weapon_id = binding
                    .weapon_id
                    .as_ref()
                    .or(binding.asset_id.as_ref())?
                    .to_string();
                let track_id = binding.track_id.as_ref()?.to_string();
                Some(PlatformCommandSpec::Fire {
                    weapon_id,
                    track_id,
                    salvo_size: binding.salvo_size,
                })
            }
            Self::Track => {
                binding
                    .track_id
                    .as_ref()
                    .map(|track_id| PlatformCommandSpec::Designate {
                        track_id: track_id.clone(),
                    })
            }
            Self::Jam => Some(PlatformCommandSpec::Jam {
                jammer_id: binding.asset_id.clone().unwrap_or_default(),
                technique: binding.mode.clone(),
                frequency_hz: None,
                bandwidth_hz: None,
                target_track_id: binding.track_id.clone(),
            }),
            Self::Safe => Some(PlatformCommandSpec::WeaponSafe),
            // No direct AFSIM-safe PlatformCommandSpec exists yet; keep these
            // non-lowering until an adapter-supported mapping is explicitly added.
            Self::SetAltitude | Self::SensorOff | Self::Coordinate | Self::Noop => None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskFrame {
    pub subject: Option<SubjectRef>,
    pub action: Action,
    #[serde(default)]
    pub object: ObjectBinding,
    #[serde(default)]
    pub phase: u32,
    #[serde(default)]
    pub guard: SafetyGuard,
    #[serde(default)]
    pub timeout_secs: u64,
}

impl TaskFrame {
    pub fn lower(&self) -> Option<PlatformCommandSpec> {
        self.action.lower(&self.object)
    }
}
