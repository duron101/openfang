//! Platform abstraction types — protocol-agnostic domain model.
//!
//! These types form the common language between the Agent decision layer
//! and all platform adapters (ArkSIM, DDS, CAN, etc.). Every adapter
//! translates its native protocol to/from these types.

use serde::{Deserialize, Serialize};

// ─────────────────────────────────────────────
// World State (inbound: adapter → agent)
// ─────────────────────────────────────────────

/// A snapshot of the world at a point in time.
/// Sent by the platform adapter after each `poll_state()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorldSnapshot {
    /// Simulation time or system time (seconds since epoch)
    pub timestamp: f64,
    /// All platforms in the world (own + allied + neutral + hostile)
    pub platforms: Vec<PlatformState>,
    /// In-flight munitions (missiles, torpedoes, shells)
    pub active_munitions: Vec<ActiveMunition>,
    /// Discrete events since last snapshot
    pub events: Vec<WorldEvent>,
    /// Mothership fleet picture (Track 2). `None` for single-platform adapters.
    #[serde(default)]
    pub fleet: Option<FleetSnapshot>,
}

impl WorldSnapshot {
    /// Find a platform by its ID
    pub fn find_platform(&self, id: &str) -> Option<&PlatformState> {
        self.platforms.iter().find(|p| p.id == id)
    }

    /// All platforms of a given affiliation
    pub fn platforms_by_affiliation(&self, aff: Affiliation) -> Vec<&PlatformState> {
        self.platforms
            .iter()
            .filter(|p| p.affiliation == aff)
            .collect()
    }
}

/// State of a single platform (USV, UAV, UUV, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformState {
    pub id: String,
    pub name: String,
    pub platform_type: String, // "usv", "uav", "uuv", "destroyer", ...
    pub affiliation: Affiliation,
    pub domain: Domain,
    pub pose: Pose,
    pub velocity: Velocity,
    pub fuel: FuelStatus,
    /// Damage factor: 0.0 (pristine) to 1.0 (destroyed)
    pub damage: f64,
    pub tracks: Vec<Track>,
    pub onboard_sensors: Vec<SensorState>,
    pub onboard_weapons: Vec<WeaponState>,
    pub onboard_jammers: Vec<JammerState>,
    pub current_target: Option<String>,
    pub commander: Option<String>,
    /// Platform Survivability Service (PSS) live view — battery, water
    /// ingress, propulsion health, structural integrity. `None` until the
    /// adapter publishes one (legacy mocks/tests stay backward-compatible).
    #[serde(default)]
    pub survivability: Option<SurvivabilityStatus>,
    /// EMCON (emission control) posture and per-emitter silencing flags.
    /// Populated by the SMS lane's EMCON state machine. `None` = no EMCON
    /// envelope configured (operate as-is).
    #[serde(default)]
    pub emcon: Option<EmconStatus>,
    /// CMS-side link quality bucket plus last-heartbeat age. Populated by
    /// the [`crate::tactical::CommandClass::Comm`] lane / CommunicationMonitor.
    #[serde(default)]
    pub link: Option<LinkStatusReport>,
}

// ─────────────────────────────────────────────
// PSS / EMCON / CMS domain types
// ─────────────────────────────────────────────

/// Live survivability view of the own platform, published every tick by
/// adapters that can observe battery/structural health.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SurvivabilityStatus {
    /// Battery/secondary-power fraction (0.0..1.0). `None` when the platform
    /// has no electrical store or the value is not reported.
    pub battery_pct: Option<f64>,
    /// True when bilge/water-ingress sensors trip.
    pub water_ingress: bool,
    /// True when propulsion (engine, motor) is operating within spec.
    pub propulsion_healthy: bool,
    /// Structural integrity fraction (0.0..1.0); complements `PlatformState::damage`.
    /// 1.0 = pristine hull, 0.0 = catastrophic structural loss.
    pub structural_integrity_pct: f64,
}

impl Default for SurvivabilityStatus {
    fn default() -> Self {
        Self {
            battery_pct: None,
            water_ingress: false,
            propulsion_healthy: true,
            structural_integrity_pct: 1.0,
        }
    }
}

/// EMCON posture — how much the platform is allowed to emit on the RF
/// spectrum (radar, comms, datalink).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EmconPosture {
    /// All emitters allowed.
    #[default]
    Full,
    /// Receive-only with brief, mission-critical transmissions allowed.
    Limited,
    /// Receive-only; no transmissions allowed (full silence).
    Silent,
}

impl EmconPosture {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Limited => "limited",
            Self::Silent => "silent",
        }
    }
}

/// Live EMCON state — the posture plus per-emitter overrides for cases where
/// a specific subsystem must be silenced without changing the global posture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmconStatus {
    pub posture: EmconPosture,
    pub radio_silent: bool,
    pub radar_silent: bool,
}

impl Default for EmconStatus {
    fn default() -> Self {
        Self {
            posture: EmconPosture::Full,
            radio_silent: false,
            radar_silent: false,
        }
    }
}

/// Coarse link-quality bucket consumed by CMS (Communications Management
/// Service), the dashboard, and the autonomy-profile downgrade triggers.
///
/// Variants are ordered from best to worst so consumers can compare directly
/// with `bucket >= LinkQuality::Poor` semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LinkQuality {
    #[default]
    Excellent,
    Good,
    Marginal,
    Poor,
    Lost,
}

impl LinkQuality {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Excellent => "excellent",
            Self::Good => "good",
            Self::Marginal => "marginal",
            Self::Poor => "poor",
            Self::Lost => "lost",
        }
    }

    /// Whether this bucket should trigger the defensive-autonomy downgrade
    /// (members drop to local autonomy when this returns true).
    pub fn should_force_defensive(&self) -> bool {
        matches!(self, Self::Poor | Self::Lost)
    }
}

/// CMS-side link strategy — how the platform should manage its uplinks.
/// Used as the parameter for [`PlatformCommand::SetLinkStrategy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LinkStrategy {
    /// Normal operation — full bandwidth, periodic heartbeats.
    #[default]
    Default,
    /// Reduce telemetry rate to preserve link headroom.
    LowBandwidth,
    /// Send only mission-critical bursts; otherwise idle.
    BurstOnly,
    /// Receive-only; do not transmit. Pairs with `EmconPosture::Silent`.
    Silent,
}

impl LinkStrategy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::LowBandwidth => "low_bandwidth",
            Self::BurstOnly => "burst_only",
            Self::Silent => "silent",
        }
    }
}

/// Snapshot of the current CMS-observed link state — quality bucket plus
/// per-link heartbeat age. Used by the autonomy downgrade trigger and the
/// dashboard "link" panel.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LinkStatusReport {
    pub quality: LinkQuality,
    /// Age (s) of the most recent heartbeat from shore command or leader.
    /// Negative or zero ⇒ unknown / never received.
    pub last_heartbeat_age_s: f64,
    /// Currently active link strategy (commanded by CMS or operator).
    #[serde(default)]
    pub strategy: LinkStrategy,
}

impl Default for LinkStatusReport {
    fn default() -> Self {
        Self {
            quality: LinkQuality::Excellent,
            last_heartbeat_age_s: 0.0,
            strategy: LinkStrategy::Default,
        }
    }
}

impl PlatformState {
    /// A minimal, zeroed platform state with the given id — convenient for
    /// tests, mock snapshots, and adapters that only know a platform's id.
    pub fn minimal(id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            name: id.clone(),
            id,
            platform_type: "unknown".to_string(),
            affiliation: Affiliation::Unknown,
            domain: Domain::Unknown,
            pose: Pose {
                lat_deg: 0.0,
                lon_deg: 0.0,
                alt_m: 0.0,
                heading_deg: 0.0,
                pitch_deg: 0.0,
                roll_deg: 0.0,
            },
            velocity: Velocity {
                speed_ms: 0.0,
                vertical_rate_ms: 0.0,
                course_deg: 0.0,
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
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Affiliation {
    Blue,
    Red,
    Neutral,
    Unknown,
    // UMAA-compatible aliases
    Friend,
    Foe,
}

impl Affiliation {
    pub fn is_hostile(&self) -> bool {
        matches!(self, Self::Red | Self::Foe)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Domain {
    Surface,
    Air,
    Subsurface,
    Land,
    Space,
    Unknown,
}

/// Air-domain envelope constraints for a UAV platform. Used by nav/op layers
/// to keep flight commands inside a safe, achievable flight envelope.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AirDomainConstraints {
    /// Minimum safe altitude (m, MSL). Floor for terrain / MSA avoidance.
    pub min_alt_m: f64,
    /// Service ceiling (m, MSL).
    pub max_alt_m: f64,
    /// Stall / minimum airspeed (m/s).
    pub min_speed_ms: f64,
    /// Never-exceed speed (m/s).
    pub max_speed_ms: f64,
    /// Maximum sustained climb rate (m/s).
    pub max_climb_rate_ms: f64,
    /// Maximum sustained descent rate (m/s, positive magnitude).
    pub max_descent_rate_ms: f64,
    /// Maximum bank-limited turn rate (deg/s).
    pub max_turn_rate_dps: f64,
}

impl Default for AirDomainConstraints {
    fn default() -> Self {
        // Conservative defaults sized for a generic CCA-class jet UAV.
        Self {
            min_alt_m: 150.0,
            max_alt_m: 15_000.0,
            min_speed_ms: 50.0,
            max_speed_ms: 340.0,
            max_climb_rate_ms: 150.0,
            max_descent_rate_ms: 150.0,
            max_turn_rate_dps: 20.0,
        }
    }
}

impl AirDomainConstraints {
    /// Clamp an altitude into the safe envelope.
    pub fn clamp_alt(&self, alt_m: f64) -> f64 {
        alt_m.clamp(self.min_alt_m, self.max_alt_m)
    }

    /// Clamp an airspeed into the safe envelope.
    pub fn clamp_speed(&self, speed_ms: f64) -> f64 {
        speed_ms.clamp(self.min_speed_ms, self.max_speed_ms)
    }

    /// True if the given altitude/speed pair is within the envelope.
    pub fn is_within(&self, alt_m: f64, speed_ms: f64) -> bool {
        alt_m >= self.min_alt_m
            && alt_m <= self.max_alt_m
            && speed_ms >= self.min_speed_ms
            && speed_ms <= self.max_speed_ms
    }
}

/// ABMS-derived tactical role assigned to a Collaborative Combat Aircraft (CCA)
/// by its commander. The role drives autonomous behavior (sensing posture,
/// EMCON, weapon safing, formation intent) without per-tick human tasking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CcaRole {
    /// Forward reconnaissance / ISR collection.
    Recon,
    /// Target designation / laser or track illumination for shooters.
    Designator,
    /// Communications / data relay node.
    Relay,
    /// Offensive strike.
    Striker,
    /// Decoy — present a deceptive signature to draw fire / attention.
    Decoy,
    /// Air intercept of a threat track.
    Intercept,
    /// Routine area patrol.
    Patrol,
    /// Escort a protected (often manned) asset.
    Escort,
    /// Persistent surveillance of a fixed area / point.
    Surveil,
    /// Formation leader / mission commander node.
    Leader,
    /// Adaptive — switch roles dynamically based on the tactical picture.
    #[default]
    Adaptive,
    /// Electronic protection (defensive EW for the package).
    EwProtection,
    /// Electronic attack / jamming.
    EwJamming,
}

/// Lifecycle status of a child UAV as tracked by its mothership (Track 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UavStatus {
    /// Stowed on deck / in the hangar, not yet launched.
    #[default]
    Stowed,
    /// In the launch sequence.
    Launching,
    /// Airborne and available for tasking.
    Airborne,
    /// Executing an assigned mission.
    OnMission,
    /// Returning to the mothership.
    Returning,
    /// In the recovery sequence.
    Recovering,
    /// Lost (destroyed / unrecoverable comms).
    Lost,
}

/// A mission assigned to a child UAV by the mothership.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UavMission {
    pub mission_id: String,
    /// "area_search", "track_target", "strike", "bda", "comm_relay", "patrol".
    pub mission_type: String,
    /// Optional CCA tactical role driving the child's behavior.
    #[serde(default)]
    pub role: Option<CcaRole>,
    /// Free-form mission parameters as JSON.
    #[serde(default)]
    pub params_json: String,
    /// Assigned target track, if any.
    #[serde(default)]
    pub target_track_id: Option<String>,
}

/// Mothership-side view of a single child UAV.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UavState {
    pub uav_id: String,
    /// "cca", "lsuav", …
    pub uav_type: String,
    pub status: UavStatus,
    /// Fuel/energy remaining fraction (0.0..1.0).
    pub fuel_pct: f64,
    /// Seconds since last contact with this child (link health).
    pub seconds_since_contact: f64,
    /// Currently assigned mission, if any.
    #[serde(default)]
    pub mission: Option<UavMission>,
}

impl UavState {
    /// A child is considered out of contact past this threshold.
    pub const COMM_LOSS_S: f64 = 30.0;

    pub fn is_in_contact(&self) -> bool {
        self.seconds_since_contact < Self::COMM_LOSS_S
    }

    pub fn is_available(&self) -> bool {
        matches!(self.status, UavStatus::Airborne) && self.is_in_contact()
    }
}

/// Aggregate fleet picture maintained by a mothership.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FleetSnapshot {
    /// Mothership platform id owning this fleet.
    pub mothership_id: String,
    /// All child UAVs known to the mothership.
    pub uavs: Vec<UavState>,
}

impl FleetSnapshot {
    pub fn new(mothership_id: impl Into<String>) -> Self {
        Self {
            mothership_id: mothership_id.into(),
            uavs: Vec::new(),
        }
    }

    pub fn get(&self, uav_id: &str) -> Option<&UavState> {
        self.uavs.iter().find(|u| u.uav_id == uav_id)
    }

    /// Airborne, in-contact children available for (re)tasking.
    pub fn available(&self) -> impl Iterator<Item = &UavState> {
        self.uavs.iter().filter(|u| u.is_available())
    }

    /// Children that need attention: lost link or critically low fuel.
    pub fn needs_attention(&self, min_fuel_pct: f64) -> Vec<&UavState> {
        self.uavs
            .iter()
            .filter(|u| {
                u.status != UavStatus::Stowed
                    && u.status != UavStatus::Lost
                    && (!u.is_in_contact() || u.fuel_pct < min_fuel_pct)
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Pose {
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub alt_m: f64,       // altitude (UAV) or depth negative (subsurface)
    pub heading_deg: f64, // true north clockwise, 0-360
    pub pitch_deg: f64,
    pub roll_deg: f64,
}

impl Pose {
    /// Distance in meters to another pose (Haversine, horizontal only)
    pub fn distance_m(&self, other: &Pose) -> f64 {
        let r = 6_371_000.0; // Earth radius
        let dlat = (other.lat_deg - self.lat_deg).to_radians();
        let dlon = (other.lon_deg - self.lon_deg).to_radians();
        let a = (dlat / 2.0).sin().powi(2)
            + self.lat_deg.to_radians().cos()
                * other.lat_deg.to_radians().cos()
                * (dlon / 2.0).sin().powi(2);
        r * 2.0 * a.sqrt().asin()
    }

    /// Bearing from this pose to another (degrees, 0=north, clockwise)
    pub fn bearing_to(&self, other: &Pose) -> f64 {
        let lat1 = self.lat_deg.to_radians();
        let lat2 = other.lat_deg.to_radians();
        let dlon = (other.lon_deg - self.lon_deg).to_radians();
        let y = dlon.sin() * lat2.cos();
        let x = lat1.cos() * lat2.sin() - lat1.sin() * lat2.cos() * dlon.cos();
        let bearing = y.atan2(x).to_degrees();
        if bearing < 0.0 {
            bearing + 360.0
        } else {
            bearing
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Velocity {
    pub speed_ms: f64,         // ground speed
    pub vertical_rate_ms: f64, // positive = ascending
    pub course_deg: f64,       // track angle (may differ from heading due to drift)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct FuelStatus {
    pub remaining_kg: f64,
    pub max_kg: f64,
    pub consumption_rate_kg_s: f64,
}

impl FuelStatus {
    pub fn remaining_pct(&self) -> f64 {
        if self.max_kg <= 0.0 {
            0.0
        } else {
            self.remaining_kg / self.max_kg
        }
    }

    pub fn endurance_s(&self) -> f64 {
        if self.consumption_rate_kg_s <= 0.0 {
            f64::INFINITY
        } else {
            self.remaining_kg / self.consumption_rate_kg_s
        }
    }
}

// ── Track ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    pub track_id: String,
    /// Truth name of the tracked platform (ArkSIM `TrackState.targetName`).
    /// Lets the planner map a human-readable enemy name (e.g. "blue_patrol_3")
    /// back to the real firing track id (e.g. "self:3"). Empty when unknown.
    #[serde(default)]
    pub target_name: String,
    pub classification: String, // "destroyer", "submarine", "aircraft", "missile", ...
    pub affiliation: Affiliation,
    pub iff: String, // "friend", "foe", "neutral", "unknown", "ambiguous"
    pub position_lla: Option<(f64, f64, f64)>,
    pub heading_deg: Option<f64>,
    pub speed_ms: Option<f64>,
    pub range_m: Option<f64>,
    pub bearing_deg: Option<f64>,
    pub elevation_deg: Option<f64>,
    /// Track quality: 0.0 (noise) to 1.0 (confirmed)
    pub quality: f64,
    pub stale: bool,
    /// Last update time (simulation seconds)
    pub last_update_s: f64,
    /// Is the track currently being tracked by ownship sensors
    pub is_active: bool,
}

// ── Sensors ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorState {
    pub sensor_id: String,
    pub sensor_type: SensorType,
    pub mode: String, // "active", "passive", "search", "track", "standby"
    pub frequency_hz: Option<f64>,
    pub bandwidth_hz: Option<f64>,
    pub azimuth_fov_deg: Option<(f64, f64)>,
    pub elevation_fov_deg: Option<(f64, f64)>,
    pub range_max_m: Option<f64>,
    pub damage: f64, // 0.0 to 1.0
    pub host_platform_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensorType {
    Radar,
    ESM,  // Electronic Support Measures
    EOIR, // Electro-Optical / Infrared
    Sonar,
    Lidar,
    AIS, // Automatic Identification System
    Other,
}

// ── Weapons ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeaponState {
    pub weapon_id: String,
    pub weapon_type: String,
    pub quantity_remaining: f64,
    pub max_range_m: Option<f64>,
    pub min_range_m: Option<f64>,
    pub guidance_type: Option<String>, // "radar", "iir", "laser", "gps_ins", "acoustic"
    pub speed_ms: Option<f64>,
    pub is_ready: bool,
    /// When true, `quantity_remaining` came from live situation telemetry
    /// (`quantityRemaining` on the wire). When false, the value may have been
    /// seeded from the scenario component manifest until the next live update.
    #[serde(default)]
    pub quantity_from_snapshot: bool,
}

// ── Jammers ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JammerState {
    pub jammer_id: String,
    pub host_id: String,
    pub is_active: bool,
    pub beams: Vec<JammerBeam>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JammerBeam {
    pub beam_index: u32,
    pub frequency_hz: f64,
    pub bandwidth_hz: f64,
    pub azimuth_min_deg: f64,
    pub azimuth_max_deg: f64,
    pub elevation_min_deg: f64,
    pub elevation_max_deg: f64,
    pub is_active: bool,
}

// ── Active Munitions ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveMunition {
    pub munition_id: String,
    pub munition_type: String,
    pub affiliation: Affiliation,
    pub position_lla: Option<(f64, f64, f64)>,
    pub heading_deg: Option<f64>,
    pub speed_ms: Option<f64>,
    pub target_id: Option<String>,
    pub time_to_impact_s: Option<f64>,
    pub host_platform_id: Option<String>,
}

// ── World Events ──

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WorldEvent {
    PlatformDestroyed {
        platform_id: String,
        destroyed_by: Option<String>,
    },
    WeaponLaunched {
        launch_platform_id: String,
        weapon_id: String,
        target_id: Option<String>,
    },
    TrackLost {
        track_id: String,
        platform_id: String,
    },
    NewContact {
        track_id: String,
        detecting_platform_id: String,
    },
    SensorDamaged {
        platform_id: String,
        sensor_id: String,
        damage: f64,
    },
    PlatformHealth {
        platform_id: String,
        uptime_s: u64,
        cpu_pct: f64,
        mem_mb: f64,
        disk_mb: f64,
        link_quality: f64,
        autonomy_mode: String,
    },
    MessageReceived {
        from_platform_id: String,
        to_platform_id: String,
        message: String,
    },
}

// ─────────────────────────────────────────────
// Platform Commands (outbound: agent → adapter)
// ─────────────────────────────────────────────

/// A control command sent from the Agent to a platform via the adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", content = "params")]
pub enum PlatformCommand {
    // ── Motion ──
    SetHeading {
        platform_id: String,
        heading_deg: f64,
        speed_ms: Option<f64>,
        turn_direction: Option<TurnDirection>,
    },
    SetSpeed {
        platform_id: String,
        speed_ms: f64,
        acceleration_ms2: Option<f64>,
    },
    SetAltitude {
        platform_id: String,
        altitude_m: f64,
        rate_ms: Option<f64>,
    },
    GotoLocation {
        platform_id: String,
        lat: f64,
        lon: f64,
        alt: Option<f64>,
        speed_ms: Option<f64>,
    },
    FollowRoute {
        platform_id: String,
        waypoints: Vec<Waypoint>,
    },

    // ── Sensors ──
    SensorOn {
        platform_id: String,
        sensor_id: String,
    },
    SensorOff {
        platform_id: String,
        sensor_id: String,
    },
    SensorSetMode {
        platform_id: String,
        sensor_id: String,
        mode: String,
    },

    // ── Weapons ──
    FireAtTarget {
        platform_id: String,
        weapon_id: String,
        track_id: String,
    },
    FireSalvo {
        platform_id: String,
        weapon_id: String,
        track_id: String,
        salvo_size: u32,
    },
    FireChaff {
        platform_id: String,
        weapon_id: String,
        count: u32,
        interval_s: f64,
    },
    UpdateTarget {
        platform_id: String,
        track_id: String,
    },
    WeaponSafeAll {
        platform_id: String,
    },

    // ── Electronic Warfare ──
    JamStart {
        platform_id: String,
        jammer_id: String,
        frequency_hz: f64,
        bandwidth_hz: f64,
        target_track_id: String,
    },
    JamStop {
        platform_id: String,
        jammer_id: String,
    },
    JamSetMode {
        platform_id: String,
        jammer_id: String,
        frequency_hz: Option<f64>,
        bandwidth_hz: Option<f64>,
    },

    // ── Communications ──
    SendMessage {
        from_platform_id: String,
        to_platform_id: String,
        message: String,
    },
    CommOn {
        platform_id: String,
    },
    CommOff {
        platform_id: String,
    },

    // ── Command & Control ──
    ChangeCommander {
        platform_id: String,
        new_commander_id: String,
    },
    SetOutsideControl {
        platform_id: String,
    },
    ReleaseOutsideControl {
        platform_id: String,
    },

    // ── UAV Operations (heterogeneous fleet) ──
    LaunchUav {
        uav_id: String,
    },
    RecoverUav {
        uav_id: String,
    },
    ReturnToBase {
        uav_id: String,
    },
    AssignMission {
        uav_id: String,
        mission_type: String, // "area_search", "track_target", "strike", "bda", "comm_relay"
        params_json: String,  // mission-specific parameters as JSON
    },
    AbortMission {
        uav_id: String,
    },

    // ── Formation ──
    FormUp {
        formation_type: String, // "line_abreast", "echelon_left", "vee", "diamond", "column"
        reference_platform_id: String,
        spacing_m: f64,
    },
    BreakFormation,
    FormationManeuver {
        reference_platform_id: String,
        delta_heading_deg: f64,
        delta_speed_ms: f64,
    },

    // ── Target Handoff ──
    HandoffTarget {
        from_platform_id: String,
        to_platform_id: String,
        track_id: String,
    },

    // ── Coordinated Strike (heterogeneous USV + UAV) ──
    /// Synchronize time-on-target across multiple weapon systems
    CoordinatedStrike {
        coordinator_platform_id: String,  // USV (suppresses air defense)
        strike_platform_ids: Vec<String>, // UAVs delivering ordnance
        target_id: String,
        time_on_target_us: u64,
    },
    /// Hand off in-flight weapon guidance to a different platform (e.g. UAV mid-course to USV terminal)
    WeaponGuidanceHandoff {
        from_platform_id: String,
        to_platform_id: String,
        munition_id: String,
    },

    // ── Deck / Launch Recovery ──
    /// Reconfigure deck resources (reloading, refueling, maintenance bay assignment)
    DeckReconfigure {
        deck_id: String,
        action: String, // "reload_weapon", "refuel_uav", "swap_payload", "maintenance"
        target_id: String, // weapon_id or uav_id
    },

    // ── Comm Relay ──
    RelayEnable {
        uav_id: String,
        bandwidth_hz: f64,
    },
    RelayDisable {
        uav_id: String,
    },

    // ── Aux / Passthrough ──
    AuxCommand {
        platform_id: String,
        key: String,
        value_json: String,
    },

    // ── EMCON / Survivability / Link strategy (new in U4) ──
    /// SMS / operator command: switch the platform's emission posture.
    /// Adapters that don't model EMCON must surface an explicit "unsupported"
    /// audit rejection (no silent dropping).
    SetEmcon {
        platform_id: String,
        posture: EmconPosture,
        radio_silent: bool,
        radar_silent: bool,
    },
    /// PSS / operator command: isolate a damaged subsystem (propulsion bus,
    /// power rail, weapon mount, …) until inspected. `reason` carries the
    /// audit explanation.
    IsolateDamage {
        platform_id: String,
        subsystem: String,
        reason: String,
    },
    /// CMS / operator command: set the active link strategy. Pairs with
    /// EMCON posture but represents a softer policy knob (telemetry rate,
    /// burst-only, etc).
    SetLinkStrategy {
        platform_id: String,
        strategy: LinkStrategy,
    },
}

impl PlatformCommand {
    /// Get the target platform ID for this command
    pub fn target_platform_id(&self) -> &str {
        match self {
            Self::SetHeading { platform_id, .. }
            | Self::SetSpeed { platform_id, .. }
            | Self::SetAltitude { platform_id, .. }
            | Self::GotoLocation { platform_id, .. }
            | Self::FollowRoute { platform_id, .. }
            | Self::SensorOn { platform_id, .. }
            | Self::SensorOff { platform_id, .. }
            | Self::SensorSetMode { platform_id, .. }
            | Self::FireAtTarget { platform_id, .. }
            | Self::FireSalvo { platform_id, .. }
            | Self::FireChaff { platform_id, .. }
            | Self::UpdateTarget { platform_id, .. }
            | Self::WeaponSafeAll { platform_id }
            | Self::JamStart { platform_id, .. }
            | Self::JamStop { platform_id, .. }
            | Self::JamSetMode { platform_id, .. }
            | Self::CommOn { platform_id }
            | Self::CommOff { platform_id }
            | Self::ChangeCommander { platform_id, .. }
            | Self::SetOutsideControl { platform_id }
            | Self::ReleaseOutsideControl { platform_id }
            | Self::AuxCommand { platform_id, .. }
            | Self::SetEmcon { platform_id, .. }
            | Self::IsolateDamage { platform_id, .. }
            | Self::SetLinkStrategy { platform_id, .. } => platform_id,
            Self::LaunchUav { uav_id }
            | Self::RecoverUav { uav_id }
            | Self::ReturnToBase { uav_id }
            | Self::AssignMission { uav_id, .. }
            | Self::AbortMission { uav_id } => uav_id,
            Self::SendMessage {
                from_platform_id, ..
            } => from_platform_id,
            Self::FormUp {
                reference_platform_id,
                ..
            }
            | Self::FormationManeuver {
                reference_platform_id,
                ..
            } => reference_platform_id,
            Self::BreakFormation => "",
            Self::HandoffTarget {
                from_platform_id, ..
            } => from_platform_id,
            Self::CoordinatedStrike {
                coordinator_platform_id,
                ..
            } => coordinator_platform_id,
            Self::WeaponGuidanceHandoff {
                from_platform_id, ..
            } => from_platform_id,
            Self::DeckReconfigure { deck_id, .. } => deck_id,
            Self::RelayEnable { uav_id, .. } | Self::RelayDisable { uav_id } => uav_id,
        }
    }

    /// True when this command deploys a reconnaissance UAV via the weapon-slot
    /// wire (`FireAtTarget`/`FireSalvo` with a scout/recon slot id). Such
    /// releases are ISR asset employment, not kinetic strikes — they must bypass
    /// kinetic engagement locks (fire-once, in-flight munition deconfliction)
    /// and weapon-release approval gates while still honouring ammo/range checks.
    pub fn is_isr_uav_release(&self) -> bool {
        match self {
            Self::FireAtTarget { weapon_id, .. } | Self::FireSalvo { weapon_id, .. } => {
                crate::mission_dsl::is_recon_uav_weapon_id(weapon_id)
            }
            _ => false,
        }
    }

    /// Fine-grained effector channel used by tactical conflict resolution.
    ///
    /// [`command_class`](Self::command_class) stays intentionally coarse for
    /// capability and approval routing, but the composer needs a narrower lane
    /// for commands that can coexist inside one class. For example, firing the
    /// same weapon at three different tracks is three engagements, while setting
    /// two headings for the same platform is still one motion lane.
    pub fn effector_subchannel(&self) -> String {
        match self {
            // Motion is multi-axis: heading, speed and altitude are independent
            // control lanes and must coexist (setting a heading must not evict a
            // speed order). Goto / route are composite path directives that share
            // one navigation lane (the latest path directive wins).
            Self::SetHeading { .. } => "heading".into(),
            Self::SetSpeed { .. } => "speed".into(),
            Self::SetAltitude { .. } => "altitude".into(),
            Self::GotoLocation { .. } | Self::FollowRoute { .. } => "nav".into(),
            Self::FireAtTarget {
                weapon_id,
                track_id,
                ..
            }
            | Self::FireSalvo {
                weapon_id,
                track_id,
                ..
            } => format!("fire:{weapon_id}->{track_id}"),
            Self::FireChaff { weapon_id, .. } => format!("chaff:{weapon_id}"),
            Self::UpdateTarget { track_id, .. } => format!("designate:{track_id}"),
            Self::WeaponSafeAll { .. } => "safe".into(),
            Self::CoordinatedStrike { target_id, .. } => {
                format!("fire:coord->{target_id}")
            }
            Self::WeaponGuidanceHandoff { munition_id, .. } => {
                format!("handoff:{munition_id}")
            }
            Self::SensorOn { sensor_id, .. }
            | Self::SensorOff { sensor_id, .. }
            | Self::SensorSetMode { sensor_id, .. } => format!("sensor:{sensor_id}"),
            Self::JamStart { jammer_id, .. }
            | Self::JamStop { jammer_id, .. }
            | Self::JamSetMode { jammer_id, .. } => format!("jam:{jammer_id}"),
            _ => String::new(),
        }
    }

    /// Coarse classification used by capability checks, gate routing, and
    /// conflict resolution. See [`crate::tactical::CommandClass`].
    pub fn command_class(&self) -> crate::tactical::CommandClass {
        use crate::tactical::CommandClass as C;
        match self {
            Self::SetHeading { .. }
            | Self::SetSpeed { .. }
            | Self::SetAltitude { .. }
            | Self::GotoLocation { .. }
            | Self::FollowRoute { .. } => C::Motion,

            Self::SensorOn { .. } | Self::SensorOff { .. } | Self::SensorSetMode { .. } => {
                C::Sensor
            }

            Self::FireAtTarget { .. }
            | Self::FireSalvo { .. }
            | Self::UpdateTarget { .. }
            | Self::WeaponSafeAll { .. }
            | Self::CoordinatedStrike { .. }
            | Self::WeaponGuidanceHandoff { .. } => C::Weapon,

            Self::FireChaff { .. }
            | Self::JamStart { .. }
            | Self::JamStop { .. }
            | Self::JamSetMode { .. } => C::ElectronicWarfare,

            Self::SendMessage { .. }
            | Self::CommOn { .. }
            | Self::CommOff { .. }
            | Self::RelayEnable { .. }
            | Self::RelayDisable { .. }
            | Self::SetEmcon { .. }
            | Self::SetLinkStrategy { .. } => C::Comm,

            Self::ChangeCommander { .. }
            | Self::SetOutsideControl { .. }
            | Self::ReleaseOutsideControl { .. }
            | Self::HandoffTarget { .. } => C::Command,

            Self::LaunchUav { .. }
            | Self::RecoverUav { .. }
            | Self::ReturnToBase { .. }
            | Self::AssignMission { .. }
            | Self::AbortMission { .. }
            | Self::DeckReconfigure { .. } => C::Uav,

            Self::FormUp { .. } | Self::BreakFormation | Self::FormationManeuver { .. } => {
                C::Formation
            }

            Self::AuxCommand { .. } | Self::IsolateDamage { .. } => C::Aux,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnDirection {
    Left,
    Right,
    Shortest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Waypoint {
    pub lat: f64,
    pub lon: f64,
    pub alt: Option<f64>,
    pub speed_ms: Option<f64>,
}

// ─────────────────────────────────────────────
// Command Result
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResult {
    /// Number of commands accepted
    pub accepted: u32,
    /// Number of commands rejected
    pub rejected: u32,
    /// Per-command errors (optional detail)
    pub errors: Vec<CommandError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandError {
    pub command_index: usize,
    pub platform_id: String,
    pub error: String,
}

impl CommandResult {
    pub fn all_accepted(count: u32) -> Self {
        Self {
            accepted: count,
            rejected: 0,
            errors: vec![],
        }
    }

    pub fn all_rejected(count: u32, error: String) -> Self {
        Self {
            accepted: 0,
            rejected: count,
            errors: (0..count as usize)
                .map(|i| CommandError {
                    command_index: i,
                    platform_id: String::new(),
                    error: error.clone(),
                })
                .collect(),
        }
    }
}

// ─────────────────────────────────────────────
// Platform Capabilities
// ─────────────────────────────────────────────

/// Bitmap of capabilities supported by a platform adapter.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlatformCapabilities {
    pub supports_motion_control: bool,
    pub supports_sensor_control: bool,
    pub supports_weapon_control: bool,
    pub supports_jammer_control: bool,
    pub supports_comm_control: bool,
    pub supports_uav_launch_recovery: bool,
    pub supports_formation_control: bool,
    pub supports_handoff: bool,
    pub max_platforms: u32,
    pub supports_simulation: bool,
    pub supports_hardware: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pose_distance() {
        let a = Pose {
            lat_deg: 30.0,
            lon_deg: 120.0,
            alt_m: 0.0,
            heading_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
        };
        let b = Pose {
            lat_deg: 30.001,
            lon_deg: 120.0,
            alt_m: 0.0,
            heading_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
        };
        let d = a.distance_m(&b);
        // 0.001 deg ≈ 111m
        assert!(d > 100.0 && d < 120.0, "distance = {d}");
    }

    #[test]
    fn test_fuel_pct() {
        let f = FuelStatus {
            remaining_kg: 500.0,
            max_kg: 1000.0,
            consumption_rate_kg_s: 0.1,
        };
        assert!((f.remaining_pct() - 0.5).abs() < 0.001);
        assert!((f.endurance_s() - 5000.0).abs() < 1.0);
    }

    #[test]
    fn test_command_target_platform() {
        let cmd = PlatformCommand::SetHeading {
            platform_id: "usv-01".into(),
            heading_deg: 90.0,
            speed_ms: None,
            turn_direction: None,
        };
        assert_eq!(cmd.target_platform_id(), "usv-01");
    }

    #[test]
    fn test_platform_command_serde() {
        let cmd = PlatformCommand::FireAtTarget {
            platform_id: "usv-01".into(),
            weapon_id: "cannon".into(),
            track_id: "trk-42".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: PlatformCommand = serde_json::from_str(&json).unwrap();
        match back {
            PlatformCommand::FireAtTarget {
                platform_id,
                weapon_id,
                track_id,
            } => {
                assert_eq!(platform_id, "usv-01");
                assert_eq!(weapon_id, "cannon");
                assert_eq!(track_id, "trk-42");
            }
            _ => panic!("wrong variant"),
        }
    }

    // ── PSS / EMCON / CMS domain-type tests (U4) ──────────────────────

    #[test]
    fn set_emcon_targets_platform_and_is_comm_class() {
        let cmd = PlatformCommand::SetEmcon {
            platform_id: "usv-01".into(),
            posture: EmconPosture::Silent,
            radio_silent: true,
            radar_silent: true,
        };
        assert_eq!(cmd.target_platform_id(), "usv-01");
        assert_eq!(cmd.command_class(), crate::tactical::CommandClass::Comm);
    }

    #[test]
    fn isolate_damage_targets_platform_and_is_aux_class() {
        let cmd = PlatformCommand::IsolateDamage {
            platform_id: "usv-01".into(),
            subsystem: "propulsion".into(),
            reason: "overcurrent".into(),
        };
        assert_eq!(cmd.target_platform_id(), "usv-01");
        assert_eq!(cmd.command_class(), crate::tactical::CommandClass::Aux);
    }

    #[test]
    fn set_link_strategy_targets_platform_and_is_comm_class() {
        let cmd = PlatformCommand::SetLinkStrategy {
            platform_id: "usv-01".into(),
            strategy: LinkStrategy::BurstOnly,
        };
        assert_eq!(cmd.target_platform_id(), "usv-01");
        assert_eq!(cmd.command_class(), crate::tactical::CommandClass::Comm);
    }

    #[test]
    fn new_commands_round_trip_through_serde() {
        for cmd in [
            PlatformCommand::SetEmcon {
                platform_id: "p".into(),
                posture: EmconPosture::Limited,
                radio_silent: false,
                radar_silent: true,
            },
            PlatformCommand::IsolateDamage {
                platform_id: "p".into(),
                subsystem: "battery_bus_a".into(),
                reason: "thermal_runaway".into(),
            },
            PlatformCommand::SetLinkStrategy {
                platform_id: "p".into(),
                strategy: LinkStrategy::LowBandwidth,
            },
        ] {
            let s = serde_json::to_string(&cmd).unwrap();
            let back: PlatformCommand = serde_json::from_str(&s).unwrap();
            assert_eq!(back.target_platform_id(), "p");
        }
    }

    #[test]
    fn emcon_link_strategy_string_labels() {
        assert_eq!(EmconPosture::Full.as_str(), "full");
        assert_eq!(EmconPosture::Limited.as_str(), "limited");
        assert_eq!(EmconPosture::Silent.as_str(), "silent");
        assert_eq!(LinkStrategy::Default.as_str(), "default");
        assert_eq!(LinkStrategy::Silent.as_str(), "silent");
        assert_eq!(LinkQuality::Excellent.as_str(), "excellent");
        assert_eq!(LinkQuality::Lost.as_str(), "lost");
    }

    #[test]
    fn link_quality_should_force_defensive_only_for_poor_and_lost() {
        assert!(!LinkQuality::Excellent.should_force_defensive());
        assert!(!LinkQuality::Good.should_force_defensive());
        assert!(!LinkQuality::Marginal.should_force_defensive());
        assert!(LinkQuality::Poor.should_force_defensive());
        assert!(LinkQuality::Lost.should_force_defensive());
    }

    #[test]
    fn platform_state_new_fields_are_optional_via_serde_default() {
        // Older JSON (without `survivability`/`emcon`/`link`) must still
        // deserialize so legacy adapters / mocks don't break.
        let s = serde_json::json!({
            "id": "p",
            "name": "p",
            "platform_type": "usv",
            "affiliation": "blue",
            "domain": "surface",
            "pose": {"lat_deg":0.0,"lon_deg":0.0,"alt_m":0.0,"heading_deg":0.0,"pitch_deg":0.0,"roll_deg":0.0},
            "velocity": {"speed_ms":0.0,"vertical_rate_ms":0.0,"course_deg":0.0},
            "fuel": {"remaining_kg":0.0,"max_kg":0.0,"consumption_rate_kg_s":0.0},
            "damage": 0.0,
            "tracks": [],
            "onboard_sensors": [],
            "onboard_weapons": [],
            "onboard_jammers": [],
            "current_target": null,
            "commander": null,
        });
        let back: PlatformState = serde_json::from_value(s).unwrap();
        assert!(back.survivability.is_none());
        assert!(back.emcon.is_none());
        assert!(back.link.is_none());
    }

    #[test]
    fn survivability_emcon_link_default_values_are_sane() {
        let s = SurvivabilityStatus::default();
        assert!(s.battery_pct.is_none());
        assert!(!s.water_ingress);
        assert!(s.propulsion_healthy);
        assert!((s.structural_integrity_pct - 1.0).abs() < 1e-9);

        let e = EmconStatus::default();
        assert_eq!(e.posture, EmconPosture::Full);
        assert!(!e.radio_silent);
        assert!(!e.radar_silent);

        let l = LinkStatusReport::default();
        assert_eq!(l.quality, LinkQuality::Excellent);
        assert!((l.last_heartbeat_age_s - 0.0).abs() < 1e-9);
        assert_eq!(l.strategy, LinkStrategy::Default);
    }
}
