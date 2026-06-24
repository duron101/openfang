//! UMAA-aligned (Unmanned Maritime Autonomy Architecture) domain types.
//!
//! Per PRD §12 / Plan §12. These types model the UMAA service view
//! (Health Monitoring, Operational Restrictions, Mission Configuration,
//! Mission Package, Autonomy Levels) on top of the existing platform model.

use serde::{Deserialize, Serialize};

// ── Health Monitoring (UMAA §3) ──

/// Overall health of a component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    /// Component operating within all parameters
    Nominal,
    /// Component operating with reduced capability
    Degraded,
    /// Component is not available
    Inoperable,
    /// Component is undergoing maintenance
    Maintenance,
    /// Status not yet determined
    Unknown,
}

impl HealthStatus {
    pub fn is_operational(&self) -> bool {
        matches!(self, Self::Nominal | Self::Degraded)
    }
    pub fn is_critical(&self) -> bool {
        matches!(self, Self::Inoperable)
    }
}

/// Built-In Test result for a single component test.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BitResult {
    pub component: String,
    pub test_name: String,
    pub passed: bool,
    pub fault_code: Option<String>,
    pub timestamp: f64,
    pub recommended_action: Option<String>, // "Restart", "SwitchToBackup", "AbortMission"
}

/// Resource usage snapshot for a component.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceUsage {
    pub cpu_pct: f32,
    pub mem_mb: f64,
    pub disk_mb: f64,
    pub gpu_pct: Option<f32>,
    pub temperature_c: Option<f32>,
}

/// Per-component health tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentHealth {
    pub component: String,
    pub status: HealthStatus,
    pub last_bit_result: Option<BitResult>,
    pub error_count_since_boot: u32,
    pub uptime_s: f64,
    pub resource_usage: ResourceUsage,
}

/// System-level health report (UMAA HealthReport).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthReport {
    pub platform_id: String,
    pub overall_status: HealthStatus,
    pub components: Vec<ComponentHealth>,
    pub active_alerts: Vec<UmaaAlert>,
    pub generated_at: f64,
}

/// Discrete alert surfaced in the health report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UmaaAlert {
    pub severity: AlertSeverity,
    pub component: String,
    pub message: String,
    pub timestamp: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertSeverity {
    Info,
    Warning,
    Error,
    Critical,
}

// ── Operational Restrictions (UMAA §6) ──

/// Rules of engagement — governs which targets may be engaged and how.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesOfEngagement {
    pub weapon_release_authority: WeaponReleaseLevel,
    pub engagement_zones: Vec<EngagementZone>,
    pub restricted_targets: Vec<String>,
    pub warning_before_engage: bool,
    pub self_defense_threshold: ThreatLevel,
}

impl Default for RulesOfEngagement {
    fn default() -> Self {
        Self {
            weapon_release_authority: WeaponReleaseLevel::WeaponsHold,
            engagement_zones: vec![],
            restricted_targets: vec![],
            warning_before_engage: true,
            self_defense_threshold: ThreatLevel::High,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WeaponReleaseLevel {
    /// No weapon use permitted
    WeaponsHold,
    /// Self-defense use only, requires human confirmation
    WeaponsTight,
    /// Commander has authorized free engagement
    WeaponsFree,
}

/// Threat-level used by ROE and DCC triggers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreatLevel {
    Low,
    Medium,
    High,
    Critical,
}

/// Geographic area where engagement is allowed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngagementZone {
    pub name: String,
    pub boundary: Vec<(f64, f64)>, // LLA polygon
    pub threat_filter: Option<ThreatLevel>,
}

/// Geofence — geographic boundary that constrains platform movement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Geofence {
    pub name: String,
    pub boundary: Vec<(f64, f64)>, // LLA polygon
    pub restriction: GeofenceType,
    pub violation_action: ViolationAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeofenceType {
    KeepIn,
    KeepOut,
    AltitudeCeiling {
        max_alt_m: f64,
    },
    /// Minimum altitude floor (air domain — terrain / MSA avoidance).
    AltitudeFloor {
        min_alt_m: f64,
    },
    SpeedLimit {
        max_speed_ms: f64,
    },
    DepthLimit {
        max_depth_m: f64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViolationAction {
    /// Log and report
    Warn,
    /// Log, report, and auto-correct (e.g. turn away)
    AutoCorrect,
    /// Abort current mission
    AbortMission,
}

/// Hard platform limits (max speed, max depth, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformLimits {
    pub max_speed_ms: f64,
    pub max_depth_m: f64,
    pub min_altitude_m: f64,
    pub max_acceleration_ms2: f64,
    pub endurance_limit_s: f64,
}

impl Default for PlatformLimits {
    fn default() -> Self {
        Self {
            max_speed_ms: 30.0,
            max_depth_m: 300.0,
            min_altitude_m: 0.0,
            max_acceleration_ms2: 5.0,
            endurance_limit_s: 86_400.0, // 24h
        }
    }
}

// ── Mission Configuration (UMAA §10) ──

/// A mission configuration — a complete operational snapshot that can be activated/deactivated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionConfig {
    pub mission_id: String,
    pub roe: RulesOfEngagement,
    pub geofences: Vec<Geofence>,
    pub platform_limits: PlatformLimits,
    pub comm_plan: CommPlan,
    pub contingency_plans: Vec<ContingencyPlan>,
    pub activated_at: Option<f64>,
    pub autonomy_mode: AutonomyMode,
    #[serde(default)]
    pub phase: Option<String>,
    #[serde(default)]
    pub objectives: Vec<Objective>,
    #[serde(default)]
    pub allocations: Vec<TargetAllocation>,
    /// Best-known target track selected by the slow loop for non-allocation
    /// role-slot plays (e.g. targeting handoff, point defense). Allocations
    /// remain authoritative for fire; this field only preserves target context
    /// when a play decomposes into role slots before a concrete weapon allocation
    /// exists.
    #[serde(default)]
    pub target_track_id: Option<String>,
    /// Tactical play (style) selected by the slow loop for this mission, taken
    /// from the `[workflow.play]` library. `None` falls back to the legacy
    /// hardcoded `playbook_for(TaskKind)` mapping (fail-safe). Set by
    /// `Planner::baseline` via `PlayRegistry::select`.
    #[serde(default)]
    pub play_name: Option<String>,
}

/// Dynamic objective attached to a mission plan.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Objective {
    pub id: String,
    pub description: String,
    pub priority: u32,
    pub status: String,
}

/// Weapon-to-track allocation produced by the planning loop.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TargetAllocation {
    pub platform_id: String,
    pub weapon_id: String,
    pub track_id: String,
    pub allocated_at: f64,
    /// Weapon-employment: rounds to fire in a salvo. `None`/`Some(1)` ⇒ a single
    /// `FireAtTarget`; `Some(n>1)` ⇒ a `FireSalvo`. Set by the slow loop (brain
    /// policy), clamped to a safe bound. The EngagementGuard + CommandGate still
    /// validate ammo/range/ROE downstream — this only shapes the proposal.
    #[serde(default)]
    pub salvo_size: Option<u32>,
    /// Free-form weapon-employment tag for audit/traceability (e.g. "single",
    /// "salvo", "conserve"). Advisory; does not bypass any gate.
    #[serde(default)]
    pub weapon_policy: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommPlan {
    pub primary_channel_hz: Option<f64>,
    pub backup_channel_hz: Option<f64>,
    pub heartbeat_interval_s: u32,
    pub silence_window_s: Option<u32>,
}

impl Default for CommPlan {
    fn default() -> Self {
        Self {
            primary_channel_hz: None,
            backup_channel_hz: None,
            heartbeat_interval_s: 30,
            silence_window_s: None,
        }
    }
}

/// Pre-planned response to a contingency event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContingencyPlan {
    pub name: String,
    pub trigger: ContingencyTrigger,
    pub actions: Vec<ContingencyAction>,
    pub priority: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "params")]
pub enum ContingencyTrigger {
    CommLost { timeout_s: f64 },
    LowFuel { min_pct: f64 },
    ThreatLevelChange { new_level: ThreatLevel },
    GeofenceViolation { fence_name: String },
    RoeChange { new_level: WeaponReleaseLevel },
    HealthDegraded { component: String },
    PlatformLost { platform_id: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "params")]
pub enum ContingencyAction {
    SwitchToBackupComm,
    ReturnToBase { urgency: String },
    SetSpeed { speed_ms: f64 },
    SetHeading { heading_deg: f64 },
    DccRuleEnable { rule_name: String },
    DccRuleDisable { rule_name: String },
    SensorSetMode { sensor_id: String, mode: String },
    SensorOffAll,
    NotifyAgent { target_agent: String },
    WeaponSafeAll,
}

// ── Mission Package (UMAA §11) ──

/// A mission package — a swappable bundle of sensors + weapons.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionPackage {
    pub package_id: String,
    pub package_type: PackageType,
    pub sensors: Vec<SensorAsset>,
    pub weapons: Vec<WeaponAsset>,
    pub estimated_endurance_impact_s: f64,
    pub compatibility: Vec<String>, // list of platform types
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageType {
    Isr,    // Intelligence, Surveillance, Reconnaissance
    Strike, // Anti-surface / land attack
    Mcm,    // Mine Counter-Measures
    Asw,    // Anti-Submarine Warfare
    Suw,    // Surface Warfare
    MultiMission,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorAsset {
    pub sensor_id: String,
    pub sensor_type: String,
    pub max_range_m: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeaponAsset {
    pub weapon_id: String,
    pub weapon_type: String,
    pub quantity: u32,
    pub max_range_m: f64,
}

impl MissionPackage {
    /// Validate that the package is compatible with the given platform type.
    /// Returns Ok(()) if the package has no compatibility constraints, or
    /// if the platform_type appears in the compatibility list.
    pub fn validate_compatibility(&self, platform_type: &str) -> Result<(), String> {
        if self.compatibility.is_empty() {
            return Ok(());
        }
        if self.compatibility.iter().any(|p| p == platform_type) {
            Ok(())
        } else {
            Err(format!(
                "package {} ({:?}) not compatible with platform_type={}",
                self.package_id, self.package_type, platform_type
            ))
        }
    }
}

// ── Autonomy Mode (UMAA §3.3) ──

/// Tactical autonomy level — drives which DCC rules are active and
/// which approval gates are required.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutonomyMode {
    /// Human-in-the-loop: human reviews every non-trivial decision
    #[default]
    HumanSupervised, // L3
    /// Human-on-the-loop: agent acts autonomously, human can intervene
    HumanOnTheLoop, // L4
    /// Fully autonomous: no human contact, all gates on local policies
    FullyAutonomous, // L5
}

/// Comms link state — drives autonomy mode transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkStatus {
    Connected,
    Degraded,
    Lost,
}

impl AutonomyMode {
    /// Pick the autonomy level appropriate for the current link status.
    pub fn from_link_status(status: LinkStatus) -> Self {
        match status {
            LinkStatus::Connected => Self::HumanSupervised,
            LinkStatus::Degraded => Self::HumanOnTheLoop,
            LinkStatus::Lost => Self::FullyAutonomous,
        }
    }
}

// ── Track Management (UMAA §5) ──

/// Correlation between a sensor contact and an existing track.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrelationResult {
    pub track_id: String,
    pub contact_id: String,
    pub correlation_score: f64, // 0.0 - 1.0
    pub is_new_track: bool,
}

/// Identification result (target classification).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentificationResult {
    pub track_id: String,
    pub classification: String,
    pub confidence: f64,               // 0.0 - 1.0
    pub classification_source: String, // "rule", "ml", "shore_confirmation"
}

/// Fusion result from a remote track (e.g. another USV).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FusionResult {
    pub track_id: String,
    pub accepted: bool,
    pub reason: Option<String>,
}

/// Track quality metrics (UMAA TrackQuality).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackQuality {
    pub existence_prob: f64,
    pub identification_confidence: f64,
    pub position_accuracy_cep_m: f64,
    pub age_s: f64,
    pub update_rate_hz: f64,
    pub staleness: TrackStaleness,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackStaleness {
    Fresh,
    Aging,
    Stale,
    Lost,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_autonomy_mode_transitions() {
        assert_eq!(
            AutonomyMode::from_link_status(LinkStatus::Connected),
            AutonomyMode::HumanSupervised
        );
        assert_eq!(
            AutonomyMode::from_link_status(LinkStatus::Degraded),
            AutonomyMode::HumanOnTheLoop
        );
        assert_eq!(
            AutonomyMode::from_link_status(LinkStatus::Lost),
            AutonomyMode::FullyAutonomous
        );
    }

    #[test]
    fn test_health_status_predicates() {
        assert!(HealthStatus::Nominal.is_operational());
        assert!(HealthStatus::Degraded.is_operational());
        assert!(!HealthStatus::Inoperable.is_operational());
        assert!(HealthStatus::Inoperable.is_critical());
    }

    #[test]
    fn test_threat_level_ordering() {
        assert!(ThreatLevel::Critical > ThreatLevel::High);
        assert!(ThreatLevel::High > ThreatLevel::Medium);
        assert!(ThreatLevel::Medium > ThreatLevel::Low);
    }

    #[test]
    fn test_default_roe() {
        let roe = RulesOfEngagement::default();
        assert_eq!(
            roe.weapon_release_authority,
            WeaponReleaseLevel::WeaponsHold
        );
        assert!(roe.warning_before_engage);
    }

    #[test]
    fn test_mission_config_dynamic_fields_default_when_missing() {
        let json = serde_json::json!({
            "mission_id": "m1",
            "roe": RulesOfEngagement::default(),
            "geofences": [],
            "platform_limits": PlatformLimits::default(),
            "comm_plan": CommPlan::default(),
            "contingency_plans": [],
            "activated_at": null,
            "autonomy_mode": AutonomyMode::HumanSupervised
        });

        let cfg: MissionConfig = serde_json::from_value(json).unwrap();

        assert!(cfg.phase.is_none());
        assert!(cfg.objectives.is_empty());
        assert!(cfg.allocations.is_empty());
    }
}
