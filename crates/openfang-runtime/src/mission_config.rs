//! Mission Configuration + ContingencyPlan orchestrator + MissionPackage manager.
//!
//! Per PRD §12.2.5/§12.2.6: given a set of registered mission configurations and
//! contingency plans, evaluate triggers against current world state and dispatch
//! the configured actions. Per PRD §12.3 (UAV coordination) the MissionPackage
//! manager validates package-vs-platform compatibility and estimates endurance.

use openfang_types::platform::{FuelStatus, PlatformState, WorldSnapshot};
use openfang_types::umaa::*;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

// ── MissionConfig Orchestrator ──

/// Outcome of a contingency plan evaluation cycle.
#[derive(Debug, Clone, PartialEq)]
pub enum ContingencyOutcome {
    /// No trigger fired
    NoAction,
    /// One or more plans fired, here are the actions
    Fired(Vec<ContingencyAction>),
}

/// Evaluates ContingencyPlan triggers against the current world state and emits
/// ContingencyAction sequences. Drop-in orchestrator that HMA/TCA can poll.
pub struct MissionConfigOrchestrator {
    state: Arc<Mutex<OrchestratorState>>,
}

struct OrchestratorState {
    /// Active mission config
    active: Option<MissionConfig>,
    /// Last link status (drives autonomy mode)
    last_link: LinkStatus,
    /// Last known ROE level (for diffing)
    last_roe: Option<WeaponReleaseLevel>,
    /// Last known geofence violation (for diffing)
    last_violation: Option<String>,
    /// Last geofence violation already fired, to avoid repeating a persistent
    /// violation every slow-loop cycle.
    last_fired_violation: Option<String>,
    /// Last known health status (for diffing)
    last_health: Option<HealthStatus>,
    /// Last own-platform fuel percentage used for threshold-crossing triggers.
    last_fuel_pct: Option<f64>,
    /// Last platform ids seen, used to detect platform-lost edges.
    last_platform_ids: HashSet<String>,
}

impl MissionConfigOrchestrator {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(OrchestratorState {
                active: None,
                last_link: LinkStatus::Connected,
                last_roe: None,
                last_violation: None,
                last_fired_violation: None,
                last_health: None,
                last_fuel_pct: None,
                last_platform_ids: HashSet::new(),
            })),
        }
    }

    /// Activate a mission configuration (replaces any currently active one).
    pub fn activate(&self, cfg: MissionConfig) {
        let mut s = self.state.lock().unwrap();
        s.active = Some(cfg);
    }

    /// Clear the active mission configuration.
    pub fn deactivate(&self) {
        self.state.lock().unwrap().active = None;
    }

    /// Get the active mission config (read-only clone).
    pub fn active(&self) -> Option<MissionConfig> {
        self.state.lock().unwrap().active.clone()
    }

    /// Number of contingency plans in the active mission.
    pub fn plan_count(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .active
            .as_ref()
            .map(|c| c.contingency_plans.len())
            .unwrap_or(0)
    }

    /// Evaluate all contingency plans against the current world state.
    /// Returns the union of actions to take (in priority order).
    pub fn evaluate(&self, snapshot: &WorldSnapshot) -> ContingencyOutcome {
        let mut s = self.state.lock().unwrap();
        let cfg = match &s.active {
            Some(c) => c.clone(),
            None => return ContingencyOutcome::NoAction,
        };

        // Read current signal values from the world
        let current_link = derive_link_status(snapshot);
        let current_health = derive_overall_health(snapshot);
        let current_fuel_pct = snapshot
            .platforms
            .first()
            .map(|p| p.fuel.remaining_pct())
            .unwrap_or(1.0);
        let current_platform_ids: HashSet<String> =
            snapshot.platforms.iter().map(|p| p.id.clone()).collect();

        // Compute diffs vs. last evaluation
        let link_changed = current_link != s.last_link;
        let roe_changed = s
            .last_roe
            .map(|l| l != cfg.roe.weapon_release_authority)
            .unwrap_or(true);
        let health_changed = s.last_health.map(|h| h != current_health).unwrap_or(true);
        let comm_lost = current_link == LinkStatus::Lost && s.last_link != LinkStatus::Lost;
        let health_degraded = matches!(
            current_health,
            HealthStatus::Degraded | HealthStatus::Inoperable
        ) && !matches!(
            s.last_health,
            Some(HealthStatus::Degraded | HealthStatus::Inoperable)
        );
        let previous_fuel_pct = s.last_fuel_pct;
        let previous_platform_ids = s.last_platform_ids.clone();
        let last_violation = s.last_violation.clone();
        let last_fired_violation = s.last_fired_violation.clone();

        let mut fired_plans: Vec<(u32, usize, Vec<ContingencyAction>)> = Vec::new();
        let mut newly_fired_violation: Option<String> = None;
        for (index, plan) in cfg.contingency_plans.iter().enumerate() {
            let fire = match &plan.trigger {
                ContingencyTrigger::CommLost { .. } => comm_lost,
                ContingencyTrigger::LowFuel { min_pct } => {
                    current_fuel_pct < *min_pct
                        && previous_fuel_pct
                            .map(|fuel_pct| fuel_pct >= *min_pct)
                            .unwrap_or(true)
                }
                ContingencyTrigger::RoeChange { new_level } => {
                    roe_changed && &cfg.roe.weapon_release_authority == new_level
                }
                ContingencyTrigger::GeofenceViolation { fence_name } => {
                    let fired = last_violation.as_deref() == Some(fence_name.as_str())
                        && last_fired_violation.as_deref() != Some(fence_name.as_str());
                    if fired {
                        newly_fired_violation = Some(fence_name.clone());
                    }
                    fired
                }
                ContingencyTrigger::HealthDegraded { component } => {
                    health_degraded && snapshot.platforms.iter().any(|p| p.id == *component)
                }
                ContingencyTrigger::PlatformLost { platform_id } => {
                    previous_platform_ids.contains(platform_id)
                        && !current_platform_ids.contains(platform_id)
                }
                ContingencyTrigger::ThreatLevelChange { new_level: _ } => false, // not derivable from snapshot alone
            };
            if fire {
                fired_plans.push((plan.priority, index, plan.actions.clone()));
            }
        }

        // Update state after evaluating all triggers against the previous cycle.
        s.last_link = current_link;
        s.last_roe = Some(cfg.roe.weapon_release_authority);
        s.last_health = Some(current_health);
        s.last_fuel_pct = Some(current_fuel_pct);
        s.last_platform_ids = current_platform_ids;
        if newly_fired_violation.is_some() {
            s.last_fired_violation = newly_fired_violation;
        }

        fired_plans.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let actions: Vec<ContingencyAction> = fired_plans
            .into_iter()
            .flat_map(|(_, _, actions)| actions)
            .collect();
        if actions.is_empty() {
            ContingencyOutcome::NoAction
        } else {
            ContingencyOutcome::Fired(actions)
        }
    }

    /// Notify of a geofence violation (used for GeofenceViolation trigger matching).
    pub fn report_geofence_violation(&self, fence_name: &str) {
        self.state.lock().unwrap().last_violation = Some(fence_name.to_string());
    }

    /// Force the autonomy mode to a specific level.
    pub fn set_autonomy_mode(&self, mode: AutonomyMode) {
        if let Some(cfg) = self.state.lock().unwrap().active.as_mut() {
            cfg.autonomy_mode = mode;
            cfg.activated_at = Some(now_f64());
        }
    }

    /// Get the current autonomy mode (driven by last link status if no override).
    pub fn current_autonomy(&self) -> AutonomyMode {
        let s = self.state.lock().unwrap();
        s.active
            .as_ref()
            .map(|c| c.autonomy_mode)
            .unwrap_or(AutonomyMode::from_link_status(s.last_link))
    }
}

impl Default for MissionConfigOrchestrator {
    fn default() -> Self {
        Self::new()
    }
}

fn derive_link_status(snapshot: &WorldSnapshot) -> LinkStatus {
    // Heuristic: if any platform has comm-related issues or 0 platforms, treat as lost
    if snapshot.platforms.is_empty() {
        return LinkStatus::Lost;
    }
    let has_active = snapshot.platforms.iter().any(|p| p.damage < 0.9);
    if !has_active {
        return LinkStatus::Lost;
    }
    LinkStatus::Connected // simplified — production would read heartbeat freshness
}

fn derive_overall_health(snapshot: &WorldSnapshot) -> HealthStatus {
    let total = snapshot.platforms.len();
    if total == 0 {
        return HealthStatus::Unknown;
    }
    let damaged = snapshot.platforms.iter().filter(|p| p.damage > 0.3).count();
    if damaged == 0 {
        HealthStatus::Nominal
    } else if damaged * 2 < total {
        HealthStatus::Degraded
    } else {
        HealthStatus::Inoperable
    }
}

fn now_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// ── MissionPackage Manager ──

/// MissionPackage registry + activation state.
pub struct MissionPackageManager {
    packages: Arc<Mutex<HashMap<String, MissionPackage>>>,
    active: Arc<Mutex<Option<String>>>,
}

impl MissionPackageManager {
    pub fn new() -> Self {
        Self {
            packages: Arc::new(Mutex::new(HashMap::new())),
            active: Arc::new(Mutex::new(None)),
        }
    }

    /// Register a mission package.
    pub fn register(&self, pkg: MissionPackage) {
        self.packages
            .lock()
            .unwrap()
            .insert(pkg.package_id.clone(), pkg);
    }

    /// Get a registered package.
    pub fn get(&self, id: &str) -> Option<MissionPackage> {
        self.packages.lock().unwrap().get(id).cloned()
    }

    /// List all registered packages.
    pub fn list(&self) -> Vec<MissionPackage> {
        self.packages.lock().unwrap().values().cloned().collect()
    }

    /// Currently active package ID.
    pub fn active(&self) -> Option<String> {
        self.active.lock().unwrap().clone()
    }

    /// Activate a package. Returns Err if the package is incompatible with the platform.
    pub fn activate(&self, package_id: &str, platform: &PlatformState) -> Result<(), String> {
        let pkg = self
            .get(package_id)
            .ok_or_else(|| format!("package '{package_id}' not registered"))?;
        pkg.validate_compatibility(&platform.platform_type)?;
        *self.active.lock().unwrap() = Some(package_id.to_string());
        Ok(())
    }

    /// Deactivate the current package.
    pub fn deactivate(&self) {
        *self.active.lock().unwrap() = None;
    }

    /// Estimate the endurance (seconds) of the platform with this package active.
    /// Formula: base_endurance - estimated_endurance_impact_s.
    pub fn estimate_endurance(&self, package_id: &str, fuel_kg: f64) -> Option<f64> {
        let pkg = self.get(package_id)?;
        let base_endurance = if fuel_kg <= 0.0 {
            f64::INFINITY
        } else {
            // Heuristic: 1 kg → 1000 s baseline
            fuel_kg * 1000.0
        };
        Some((base_endurance - pkg.estimated_endurance_impact_s).max(0.0))
    }
}

impl Default for MissionPackageManager {
    fn default() -> Self {
        Self::new()
    }
}

// Free-function builder (since MissionPackage is in another crate, we can't
// inherent-impl it). Use `build_mission_package(...)` instead of `MissionPackage::builder(...)`.
pub fn build_mission_package(
    id: impl Into<String>,
    package_type: PackageType,
) -> MissionPackageBuilder {
    MissionPackageBuilder {
        id: id.into(),
        package_type,
        sensors: vec![],
        weapons: vec![],
        estimated_endurance_impact_s: 0.0,
        compatibility: vec![],
    }
}

pub struct MissionPackageBuilder {
    id: String,
    package_type: PackageType,
    sensors: Vec<SensorAsset>,
    weapons: Vec<WeaponAsset>,
    estimated_endurance_impact_s: f64,
    compatibility: Vec<String>,
}

impl MissionPackageBuilder {
    pub fn sensor(mut self, sensor: SensorAsset) -> Self {
        self.sensors.push(sensor);
        self
    }
    pub fn weapon(mut self, weapon: WeaponAsset) -> Self {
        self.weapons.push(weapon);
        self
    }
    pub fn endurance_impact_s(mut self, seconds: f64) -> Self {
        self.estimated_endurance_impact_s = seconds;
        self
    }
    pub fn compatible_with(mut self, platform_type: impl Into<String>) -> Self {
        self.compatibility.push(platform_type.into());
        self
    }
    pub fn build(self) -> MissionPackage {
        MissionPackage {
            package_id: self.id,
            package_type: self.package_type,
            sensors: self.sensors,
            weapons: self.weapons,
            estimated_endurance_impact_s: self.estimated_endurance_impact_s,
            compatibility: self.compatibility,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::platform::{Affiliation, Domain, Pose, Velocity};

    fn sample_snapshot(platform_count: usize, damaged: usize) -> WorldSnapshot {
        let mut platforms = Vec::new();
        for i in 0..platform_count {
            platforms.push(PlatformState {
                id: format!("p-{i}"),
                name: format!("p-{i}"),
                platform_type: "usv".into(),
                affiliation: Affiliation::Blue,
                domain: Domain::Surface,
                pose: Pose {
                    lat_deg: 30.0,
                    lon_deg: 120.0,
                    alt_m: 0.0,
                    heading_deg: 0.0,
                    pitch_deg: 0.0,
                    roll_deg: 0.0,
                },
                velocity: Velocity {
                    speed_ms: 10.0,
                    vertical_rate_ms: 0.0,
                    course_deg: 0.0,
                },
                fuel: FuelStatus {
                    remaining_kg: 100.0,
                    max_kg: 200.0,
                    consumption_rate_kg_s: 0.1,
                },
                damage: if i < damaged { 0.5 } else { 0.0 },
                tracks: vec![],
                onboard_sensors: vec![],
                onboard_weapons: vec![],
                onboard_jammers: vec![],
                current_target: None,
                commander: None,
                survivability: None,
                emcon: None,
                link: None,
            });
        }
        WorldSnapshot {
            timestamp: 0.0,
            platforms,
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        }
    }

    fn sample_mission_with_comm_lost_plan() -> MissionConfig {
        MissionConfig {
            mission_id: "test".into(),
            roe: RulesOfEngagement::default(),
            geofences: vec![],
            platform_limits: PlatformLimits::default(),
            comm_plan: CommPlan::default(),
            contingency_plans: vec![ContingencyPlan {
                name: "comm_lost_rtb".into(),
                trigger: ContingencyTrigger::CommLost { timeout_s: 30.0 },
                actions: vec![ContingencyAction::ReturnToBase {
                    urgency: "high".into(),
                }],
                priority: 100,
            }],
            activated_at: Some(0.0),
            autonomy_mode: AutonomyMode::HumanSupervised,
            phase: None,
            objectives: vec![],
            allocations: vec![],
            target_track_id: None,
            play_name: None,
        }
    }

    #[test]
    fn test_orchestrator_no_active_mission() {
        let orch = MissionConfigOrchestrator::new();
        let outcome = orch.evaluate(&sample_snapshot(1, 0));
        assert_eq!(outcome, ContingencyOutcome::NoAction);
    }

    #[test]
    fn test_orchestrator_comm_lost_fires_plan() {
        let orch = MissionConfigOrchestrator::new();
        orch.activate(sample_mission_with_comm_lost_plan());
        // First call: empty snapshot → link Lost → CommLost fires
        let empty = WorldSnapshot {
            timestamp: 0.0,
            platforms: vec![],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        };
        let outcome = orch.evaluate(&empty);
        assert!(matches!(outcome, ContingencyOutcome::Fired(_)));
    }

    #[test]
    fn test_orchestrator_comm_lost_can_emit_sensor_set_mode() {
        let orch = MissionConfigOrchestrator::new();
        let mut mission = sample_mission_with_comm_lost_plan();
        mission.contingency_plans = vec![ContingencyPlan {
            name: "comm_lost_sensor_silent".into(),
            trigger: ContingencyTrigger::CommLost { timeout_s: 30.0 },
            actions: vec![ContingencyAction::SensorSetMode {
                sensor_id: "surf_radar".into(),
                mode: "off".into(),
            }],
            priority: 100,
        }];
        orch.activate(mission);
        let empty = WorldSnapshot {
            timestamp: 1.0,
            platforms: vec![],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        };

        assert!(matches!(
            orch.evaluate(&empty),
            ContingencyOutcome::Fired(actions)
                if actions == vec![ContingencyAction::SensorSetMode {
                    sensor_id: "surf_radar".into(),
                    mode: "off".into()
                }]
        ));
    }

    #[test]
    fn test_contingency_dcc_rule_disable_toggles_real_dcc() {
        use crate::direct_channel::{DirectCommandChannel, TriggerCondition, TriggerRule};
        use openfang_types::platform::PlatformCommand;
        use openfang_types::tactical::CommandPriority;

        // A reflex rule the brain may switch off via a contingency.
        let mut dcc = DirectCommandChannel::new();
        dcc.add_rule(TriggerRule::new(
            "evade",
            TriggerCondition::Always,
            PlatformCommand::CommOn {
                platform_id: "x".into(),
            },
            CommandPriority::Critical,
        ));

        // Mission whose CommLost contingency disables the "evade" reflex.
        let mission = MissionConfig {
            mission_id: "t".into(),
            roe: RulesOfEngagement::default(),
            geofences: vec![],
            platform_limits: PlatformLimits::default(),
            comm_plan: CommPlan::default(),
            contingency_plans: vec![ContingencyPlan {
                name: "comm_lost_safe".into(),
                trigger: ContingencyTrigger::CommLost { timeout_s: 30.0 },
                actions: vec![ContingencyAction::DccRuleDisable {
                    rule_name: "evade".into(),
                }],
                priority: 100,
            }],
            activated_at: Some(0.0),
            autonomy_mode: AutonomyMode::HumanSupervised,
            phase: None,
            objectives: vec![],
            allocations: vec![],
            target_track_id: None,
            play_name: None,
        };

        let orch = MissionConfigOrchestrator::new();
        orch.activate(mission);
        // Empty snapshot → link Lost → CommLost fires.
        let empty = WorldSnapshot {
            timestamp: 0.0,
            platforms: vec![],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        };
        let ContingencyOutcome::Fired(actions) = orch.evaluate(&empty) else {
            panic!("CommLost should fire");
        };
        // Apply exactly as the kernel slow loop does.
        for action in &actions {
            if let ContingencyAction::DccRuleDisable { rule_name } = action {
                assert!(dcc.set_rule_enabled(rule_name, false));
            }
        }
        // The reflex rule is now suppressed.
        let (critical, high) = dcc.evaluate(&empty, "x");
        assert!(
            critical.is_empty() && high.is_empty(),
            "evade reflex disabled"
        );
    }

    #[test]
    fn test_orchestrator_low_fuel_fires_without_link_or_health_change() {
        let orch = MissionConfigOrchestrator::new();
        let mut mission = sample_mission_with_comm_lost_plan();
        mission.contingency_plans = vec![ContingencyPlan {
            name: "low_fuel_safe".into(),
            trigger: ContingencyTrigger::LowFuel { min_pct: 0.25 },
            actions: vec![ContingencyAction::DccRuleDisable {
                rule_name: "aggressive_intercept".into(),
            }],
            priority: 100,
        }];
        orch.activate(mission);

        let mut snap = sample_snapshot(1, 0);
        let _ = orch.evaluate(&snap);
        snap.platforms[0].fuel.remaining_kg = 10.0;

        assert!(matches!(
            orch.evaluate(&snap),
            ContingencyOutcome::Fired(actions)
                if actions == vec![ContingencyAction::DccRuleDisable {
                    rule_name: "aggressive_intercept".into()
                }]
        ));
    }

    #[test]
    fn test_orchestrator_platform_lost_fires_without_link_or_health_change() {
        let orch = MissionConfigOrchestrator::new();
        let mut mission = sample_mission_with_comm_lost_plan();
        mission.contingency_plans = vec![ContingencyPlan {
            name: "wingman_lost".into(),
            trigger: ContingencyTrigger::PlatformLost {
                platform_id: "wingman".into(),
            },
            actions: vec![ContingencyAction::DccRuleEnable {
                rule_name: "defensive_screen".into(),
            }],
            priority: 100,
        }];
        orch.activate(mission);

        let mut snap = sample_snapshot(1, 0);
        let mut wingman = snap.platforms[0].clone();
        wingman.id = "wingman".into();
        snap.platforms.push(wingman);
        let _ = orch.evaluate(&snap);
        snap.platforms.retain(|p| p.id != "wingman");

        assert!(matches!(
            orch.evaluate(&snap),
            ContingencyOutcome::Fired(actions)
                if actions == vec![ContingencyAction::DccRuleEnable {
                    rule_name: "defensive_screen".into()
                }]
        ));
    }

    #[test]
    fn test_orchestrator_orders_actions_by_plan_priority() {
        let orch = MissionConfigOrchestrator::new();
        let mut mission = sample_mission_with_comm_lost_plan();
        mission.contingency_plans = vec![
            ContingencyPlan {
                name: "lower".into(),
                trigger: ContingencyTrigger::CommLost { timeout_s: 30.0 },
                actions: vec![ContingencyAction::DccRuleDisable {
                    rule_name: "evade".into(),
                }],
                priority: 10,
            },
            ContingencyPlan {
                name: "higher".into(),
                trigger: ContingencyTrigger::CommLost { timeout_s: 30.0 },
                actions: vec![ContingencyAction::DccRuleEnable {
                    rule_name: "evade".into(),
                }],
                priority: 100,
            },
        ];
        orch.activate(mission);

        let empty = WorldSnapshot {
            timestamp: 0.0,
            platforms: vec![],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        };

        assert!(matches!(
            orch.evaluate(&empty),
            ContingencyOutcome::Fired(actions)
                if actions == vec![
                    ContingencyAction::DccRuleEnable {
                        rule_name: "evade".into()
                    },
                    ContingencyAction::DccRuleDisable {
                        rule_name: "evade".into()
                    },
                ]
        ));
    }

    #[test]
    fn test_orchestrator_idempotent() {
        let orch = MissionConfigOrchestrator::new();
        orch.activate(sample_mission_with_comm_lost_plan());
        let snap = sample_snapshot(1, 0);
        let _ = orch.evaluate(&snap);
        // Second call with same state — should be NoAction
        let outcome = orch.evaluate(&snap);
        assert_eq!(outcome, ContingencyOutcome::NoAction);
    }

    #[test]
    fn test_mission_package_compatibility_check() {
        let usv = PlatformState {
            id: "usv-01".into(),
            name: "usv-01".into(),
            platform_type: "usv".into(),
            affiliation: Affiliation::Blue,
            domain: Domain::Surface,
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
                remaining_kg: 100.0,
                max_kg: 200.0,
                consumption_rate_kg_s: 0.1,
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
        let pkg = build_mission_package("strike-01", PackageType::Strike)
            .weapon(WeaponAsset {
                weapon_id: "torpedo".into(),
                weapon_type: "torpedo".into(),
                quantity: 4,
                max_range_m: 10000.0,
            })
            .endurance_impact_s(3600.0)
            .compatible_with("usv")
            .compatible_with("destroyer")
            .build();
        assert!(pkg.validate_compatibility(&usv.platform_type).is_ok());

        let uav = PlatformState {
            platform_type: "uav".into(),
            ..usv.clone()
        };
        assert!(pkg.validate_compatibility(&uav.platform_type).is_err());
    }

    #[test]
    fn test_mission_package_activate_deactivate() {
        let mgr = MissionPackageManager::new();
        let pkg = build_mission_package("isr-01", PackageType::Isr)
            .compatible_with("usv")
            .build();
        mgr.register(pkg);
        let usv = PlatformState {
            id: "u".into(),
            name: "u".into(),
            platform_type: "usv".into(),
            affiliation: Affiliation::Blue,
            domain: Domain::Surface,
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
                remaining_kg: 100.0,
                max_kg: 200.0,
                consumption_rate_kg_s: 0.1,
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
        assert!(mgr.activate("isr-01", &usv).is_ok());
        assert_eq!(mgr.active(), Some("isr-01".to_string()));
        mgr.deactivate();
        assert_eq!(mgr.active(), None);
    }

    #[test]
    fn test_mission_package_endurance_estimate() {
        let mgr = MissionPackageManager::new();
        let pkg = build_mission_package("strike-01", PackageType::Strike)
            .endurance_impact_s(60_000.0) // 16.7 hours impact
            .build();
        mgr.register(pkg);
        // 100 kg fuel → 100_000 s baseline, minus 60_000 = 40_000 s
        let est = mgr.estimate_endurance("strike-01", 100.0);
        assert!(est.is_some());
        assert!((est.unwrap() - 40_000.0).abs() < 0.1);
    }

    #[test]
    fn test_autonomy_mode_default() {
        let orch = MissionConfigOrchestrator::new();
        assert_eq!(orch.current_autonomy(), AutonomyMode::HumanSupervised);
    }
}
