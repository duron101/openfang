//! Direct Command Channel — time-critical rule-driven fast response path.
//!
//! DCC bypasses the LLM Agent loop for pre-authorized, latency-sensitive actions.
//! Rules evaluate `WorldSnapshot` conditions and propose actions.
//!
//! Safety contract (Iron Law): the DCC NEVER writes to an adapter or registry
//! directly. It only emits [`CandidateIntent`]s, which are deconflicted by the
//! ActionComposer and must clear the CommandGate before dispatch. `Critical`
//! intents may *preempt* the active plan, but they can never bypass SPGS/audit.
//!
//! Architecture: DCC runs between `poll_state()` and Agent tick:
//!   1. Evaluate all TriggerRules against current WorldSnapshot
//!   2. Emit CandidateIntents (Critical may preempt; High queues ahead of LLM)
//!   3. ActionComposer + CommandGate decide what actually reaches the adapter

use crate::cognition::threat_score;
use dashmap::DashMap;
use openfang_types::config::EvasionParams;
use openfang_types::platform::{Affiliation, PlatformCommand, WorldSnapshot};
use openfang_types::tactical::{CandidateIntent, IntentSource};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tracing::{info, warn};

// ── TriggerRule ──

/// A reflex rule: a [`TriggerCondition`] paired with the [`PlatformCommand`] it
/// proposes. Serde-serializable so rules can be authored as data — by config in
/// Phase 1 and by the slow LLM loop in later phases — instead of being baked in
/// code. The action is still only a *proposal*: it flows through the same
/// ActionComposer + CommandGate as everything else (Iron Law).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerRule {
    pub name: String,
    pub condition: TriggerCondition,
    pub action: PlatformCommand,
    pub priority: CommandPriority,
    /// Cooldown between repeated triggers (ms)
    pub cooldown_ms: u64,
    /// Maximum fires per minute
    pub max_fires_per_minute: u32,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct FireTracker {
    last_fire: Instant,
    fires_this_minute: u32,
    minute_start: Instant,
}

impl TriggerRule {
    pub fn new(
        name: impl Into<String>,
        condition: TriggerCondition,
        action: PlatformCommand,
        priority: CommandPriority,
    ) -> Self {
        Self {
            name: name.into(),
            condition,
            action,
            priority,
            cooldown_ms: 5000,
            max_fires_per_minute: 10,
            enabled: true,
        }
    }

    pub fn with_cooldown(mut self, ms: u64) -> Self {
        self.cooldown_ms = ms;
        self
    }

    pub fn with_rate_limit(mut self, max_per_min: u32) -> Self {
        self.max_fires_per_minute = max_per_min;
        self
    }
}

// ── TriggerCondition ──

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerCondition {
    /// Radar lock: track quality > threshold AND range < max_range
    RadarLock {
        min_track_quality: f64,
        max_range_m: f64,
        track_affiliation: Option<Affiliation>,
    },
    /// Threat-assessment driven: any hostile track whose computed
    /// `threat_score` (quality/range/speed, same metric the cognition engine
    /// uses) meets `min_threat_score` within `max_range_m`. This is the small
    /// brain reading the *threat* rather than a raw radar return, so reflexes
    /// can be conditioned on assessed danger instead of any contact.
    HighThreat {
        min_threat_score: f64,
        max_range_m: f64,
        track_affiliation: Option<Affiliation>,
    },
    /// A fast-closing weapon-like contact (missile/torpedo/munition class, or a
    /// very high-threat fast track) inside `max_range_m` — the trigger for an
    /// evasive/defeat reflex. Evaluated from the track classification + threat
    /// score so it works off assessed danger, not a bespoke sensor message.
    IncomingMunition {
        min_threat_score: f64,
        max_range_m: f64,
    },
    /// Collision risk: CPA < min_cpa AND TCPA < max_tcpa
    CollisionRisk { min_cpa_m: f64, max_tcpa_s: f64 },
    /// External command via OFP/A2A
    ExternalCommand {
        command_type: String,
        requires_hmac: bool,
    },
    /// System state transition
    StateTransition {
        from_state: String,
        to_state: String,
    },
    /// UAV fuel below threshold
    UavFuelCritical { min_reserve_pct: f64 },
    /// UAV communication lost
    UavCommLost { timeout_s: f64 },
    /// UAV destroyed/lost
    UavLost,
    /// New contact detected
    ContactDetected { confidence: f64, range_m: f64 },
    /// Always true (for testing)
    Always,
}

impl TriggerCondition {
    /// Evaluate this condition against the current world state.
    pub fn evaluate(&self, snapshot: &WorldSnapshot, own_platform_id: &str) -> bool {
        match self {
            Self::Always => true,
            Self::RadarLock {
                min_track_quality,
                max_range_m,
                track_affiliation,
            } => {
                for platform in &snapshot.platforms {
                    for track in &platform.tracks {
                        if track.quality < *min_track_quality {
                            continue;
                        }
                        if let Some(range) = track.range_m {
                            if range > *max_range_m {
                                continue;
                            }
                        }
                        if let Some(aff) = track_affiliation {
                            if track.affiliation != *aff {
                                continue;
                            }
                        }
                        return true;
                    }
                }
                false
            }
            Self::HighThreat {
                min_threat_score,
                max_range_m,
                track_affiliation,
            } => snapshot.platforms.iter().any(|platform| {
                platform.tracks.iter().any(|track| {
                    if track.stale || !track.is_active {
                        return false;
                    }
                    if let Some(aff) = track_affiliation {
                        if track.affiliation != *aff {
                            return false;
                        }
                    }
                    if track.range_m.map_or(false, |r| r > *max_range_m) {
                        return false;
                    }
                    threat_score(track) >= *min_threat_score
                })
            }),
            Self::IncomingMunition {
                min_threat_score,
                max_range_m,
            } => snapshot.platforms.iter().any(|platform| {
                platform.tracks.iter().any(|track| {
                    if track.stale || !track.is_active {
                        return false;
                    }
                    if track.range_m.map_or(false, |r| r > *max_range_m) {
                        return false;
                    }
                    let cls = track.classification.to_ascii_lowercase();
                    let weapon_like = cls.contains("missile")
                        || cls.contains("munition")
                        || cls.contains("torpedo")
                        || cls.contains("rocket");
                    weapon_like || threat_score(track) >= *min_threat_score
                })
            }),
            Self::CollisionRisk {
                min_cpa_m,
                max_tcpa_s,
            } => {
                use crate::nav_control::compute_cpa_3d;
                let Some(own) = snapshot.platforms.iter().find(|p| p.id == own_platform_id) else {
                    return false;
                };
                for other in &snapshot.platforms {
                    if other.id == own_platform_id {
                        continue;
                    }
                    if !other.affiliation.is_hostile() {
                        continue;
                    }
                    let (cpa, tcpa) = compute_cpa_3d(own, other);
                    if cpa < *min_cpa_m && tcpa > 0.0 && tcpa < *max_tcpa_s {
                        return true;
                    }
                }
                false
            }
            Self::ExternalCommand { .. } => {
                // Checked by external message handler, not snapshot evaluation
                false
            }
            Self::StateTransition { .. } => {
                // Checked by state machine, not snapshot evaluation
                false
            }
            Self::UavFuelCritical { min_reserve_pct } => {
                for platform in &snapshot.platforms {
                    if !own_platform_id.is_empty() && platform.id != own_platform_id {
                        continue;
                    }
                    // Require valid fuel telemetry: a missing/zero max (e.g. a
                    // surface vessel or a sim that doesn't report fuel) must NOT
                    // be read as an empty tank — `remaining_pct()` returns 0.0 in
                    // that case, which would falsely trip emergency RTB every tick.
                    if platform.fuel.max_kg <= 0.0 {
                        continue;
                    }
                    let is_uav = platform_is_uav(platform);
                    if is_uav && platform.fuel.remaining_pct() < *min_reserve_pct {
                        return true;
                    }
                }
                false
            }
            Self::UavCommLost { timeout_s } => snapshot
                .fleet
                .as_ref()
                .and_then(|fleet| fleet.get(own_platform_id))
                .map(|uav| uav.seconds_since_contact >= *timeout_s)
                .unwrap_or(false),
            Self::UavLost => {
                // Requires dead reckoning + timeout tracking
                false
            }
            Self::ContactDetected {
                confidence,
                range_m,
            } => {
                for platform in &snapshot.platforms {
                    for track in &platform.tracks {
                        if track.quality >= *confidence
                            && track.range_m.map_or(false, |r| r <= *range_m)
                        {
                            return true;
                        }
                    }
                }
                false
            }
        }
    }
}

fn platform_is_uav(platform: &openfang_types::platform::PlatformState) -> bool {
    platform.domain == openfang_types::platform::Domain::Air
        || matches!(
            platform.platform_type.as_str(),
            "uav" | "cca" | "lsuav" | "aircraft"
        )
}

fn bind_action_target(cmd: &mut PlatformCommand, own_platform_id: &str) {
    match cmd {
        PlatformCommand::SetHeading { platform_id, .. }
        | PlatformCommand::SetSpeed { platform_id, .. }
        | PlatformCommand::SetAltitude { platform_id, .. }
        | PlatformCommand::GotoLocation { platform_id, .. }
        | PlatformCommand::SensorOn { platform_id, .. }
        | PlatformCommand::SensorOff { platform_id, .. }
        | PlatformCommand::SensorSetMode { platform_id, .. }
        | PlatformCommand::FireChaff { platform_id, .. }
        | PlatformCommand::JamStart { platform_id, .. }
        | PlatformCommand::JamStop { platform_id, .. }
        | PlatformCommand::JamSetMode { platform_id, .. }
        | PlatformCommand::CommOn { platform_id }
        | PlatformCommand::CommOff { platform_id }
        | PlatformCommand::WeaponSafeAll { platform_id }
        | PlatformCommand::AuxCommand { platform_id, .. } => {
            if platform_id.is_empty() || platform_id == "self" {
                *platform_id = own_platform_id.to_string();
            }
        }
        PlatformCommand::ReturnToBase { uav_id }
        | PlatformCommand::LaunchUav { uav_id }
        | PlatformCommand::RecoverUav { uav_id }
        | PlatformCommand::AssignMission { uav_id, .. }
        | PlatformCommand::RelayEnable { uav_id, .. }
        | PlatformCommand::RelayDisable { uav_id } => {
            if uav_id.is_empty() || uav_id == "self" {
                *uav_id = own_platform_id.to_string();
            }
        }
        _ => {}
    }
}

// ── CommandPriority ──

// Unified with the tactical command-pipeline contract. The DCC produces intents
// at these priorities; `Critical` may preempt the active plan in the ActionComposer
// but can never bypass the CommandGate.
pub use openfang_types::tactical::CommandPriority;

// ── DirectCommandChannel ──

/// The Direct Command Channel — rule engine + fast dispatch.
pub struct DirectCommandChannel {
    rules: Vec<TriggerRule>,
    fire_trackers: DashMap<String, FireTracker>,
    enabled: AtomicBool,
}

impl DirectCommandChannel {
    pub fn new() -> Self {
        Self {
            rules: Vec::new(),
            fire_trackers: DashMap::new(),
            enabled: AtomicBool::new(true),
        }
    }

    /// Register a trigger rule.
    pub fn add_rule(&mut self, rule: TriggerRule) {
        let now = Instant::now();
        // Avoid `Instant - Duration` overflow when system uptime < lookback (Windows panics).
        let lookback = std::time::Duration::from_secs(3600);
        let last_fire = now
            .checked_sub(lookback)
            .unwrap_or_else(|| now.checked_sub(now.elapsed()).unwrap_or(now));
        self.fire_trackers.insert(
            rule.name.clone(),
            FireTracker {
                last_fire,
                fires_this_minute: 0,
                minute_start: now,
            },
        );
        self.rules.push(rule);
    }

    /// Replace the entire ruleset (and reset fire trackers). The data-driven
    /// entry point used by config at boot and, in later phases, by the slow LLM
    /// loop to push a brain-authored ruleset. Safety is unchanged: rules only
    /// ever emit [`CandidateIntent`]s through the gate.
    pub fn load_rules(&mut self, rules: Vec<TriggerRule>) {
        self.rules.clear();
        self.fire_trackers.clear();
        for rule in rules {
            self.add_rule(rule);
        }
    }

    /// Remove a rule by name. Returns `true` if a rule was removed.
    pub fn remove_rule(&mut self, rule_name: &str) -> bool {
        let before = self.rules.len();
        self.rules.retain(|r| r.name != rule_name);
        let removed = self.rules.len() != before;
        if removed {
            self.fire_trackers.remove(rule_name);
            info!(rule = rule_name, "DCC rule removed");
        }
        removed
    }

    /// Enable or disable the entire DCC system.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
        info!(enabled, "DCC enabled status changed");
    }

    /// Enable or disable a specific rule by name. Returns `true` if a rule with
    /// that name existed (and was toggled), `false` otherwise.
    pub fn set_rule_enabled(&mut self, rule_name: &str, enabled: bool) -> bool {
        if let Some(rule) = self.rules.iter_mut().find(|r| r.name == rule_name) {
            rule.enabled = enabled;
            info!(rule = rule_name, enabled, "DCC rule toggled");
            true
        } else {
            false
        }
    }

    /// Evaluate all rules against the current snapshot.
    /// Returns (Critical commands, High-priority commands).
    pub fn evaluate(
        &self,
        snapshot: &WorldSnapshot,
        own_platform_id: &str,
    ) -> (Vec<PlatformCommand>, Vec<PlatformCommand>) {
        if !self.enabled.load(Ordering::SeqCst) {
            return (vec![], vec![]);
        }

        let mut critical = Vec::new();
        let mut high = Vec::new();
        let now = Instant::now();

        for rule in &self.rules {
            if !rule.enabled {
                continue;
            }

            // Check rate limits (scope the RefMut guard so it's dropped before later get_mut calls)
            {
                let mut tracker = self.fire_trackers.get_mut(&rule.name);
                if let Some(ref mut t) = tracker {
                    // A rule that has never fired is never in cooldown.
                    let never_fired = t.fires_this_minute == 0;
                    // Reset minute counter
                    if now.duration_since(t.minute_start).as_secs() >= 60 {
                        drop(tracker);
                        self.fire_trackers.insert(
                            rule.name.clone(),
                            FireTracker {
                                last_fire: now,
                                fires_this_minute: 0,
                                minute_start: now,
                            },
                        );
                    } else if t.fires_this_minute >= rule.max_fires_per_minute {
                        continue; // Rate limited
                    } else if !never_fired
                        && now.duration_since(t.last_fire).as_millis() < rule.cooldown_ms as u128
                    {
                        continue; // Cooldown
                    }
                }
            }

            // Evaluate condition
            if !rule.condition.evaluate(snapshot, own_platform_id) {
                continue;
            }

            // Update fire tracker (fresh RefMut; previous guard is already dropped)
            if let Some(mut t) = self.fire_trackers.get_mut(&rule.name) {
                t.last_fire = now;
                t.fires_this_minute += 1;
            }

            let mut cmd = rule.action.clone();
            bind_action_target(&mut cmd, own_platform_id);

            match rule.priority {
                CommandPriority::Critical => {
                    info!(rule = %rule.name, "DCC Critical: firing immediately");
                    critical.push(cmd);
                }
                CommandPriority::High => {
                    info!(rule = %rule.name, "DCC High: queued");
                    high.push(cmd);
                }
                CommandPriority::Normal => {
                    // Normal priority goes through standard LLM path
                }
            }
        }

        (critical, high)
    }

    /// Evaluate rules and emit [`CandidateIntent`]s — the ONLY legal DCC output.
    ///
    /// The DCC must never push commands to an adapter itself; these intents are
    /// handed to the ActionComposer and then the CommandGate. `now_secs` should
    /// come from the active `TimeSource` (sim time or wall clock).
    ///
    /// NOTE: full per-rule provenance is wired in Phase 2; for now Critical and
    /// High tiers are tagged with stable synthetic rule labels.
    pub fn evaluate_intents(
        &self,
        snapshot: &WorldSnapshot,
        own_platform_id: &str,
        now_secs: f64,
    ) -> Vec<CandidateIntent> {
        let (critical, high) = self.evaluate(snapshot, own_platform_id);
        let mut intents = Vec::with_capacity(critical.len() + high.len());
        for cmd in critical {
            intents.push(CandidateIntent::new(
                cmd,
                CommandPriority::Critical,
                IntentSource::Dcc {
                    rule_name: "dcc_critical".into(),
                },
                now_secs,
                "DCC critical reflex",
            ));
        }
        for cmd in high {
            intents.push(CandidateIntent::new(
                cmd,
                CommandPriority::High,
                IntentSource::Dcc {
                    rule_name: "dcc_high".into(),
                },
                now_secs,
                "DCC high-priority reflex",
            ));
        }
        intents
    }

    /// List all registered rules with their status.
    pub fn list_rules(&self) -> Vec<RuleStatus> {
        self.rules
            .iter()
            .map(|r| {
                let tracker = self.fire_trackers.get(&r.name);
                RuleStatus {
                    name: r.name.clone(),
                    enabled: r.enabled,
                    priority: format!("{:?}", r.priority),
                    fires_this_minute: tracker.as_ref().map(|t| t.fires_this_minute).unwrap_or(0),
                    cooldown_remaining_ms: tracker
                        .as_ref()
                        .map(|t| {
                            let elapsed =
                                Instant::now().duration_since(t.last_fire).as_millis() as u64;
                            r.cooldown_ms.saturating_sub(elapsed)
                        })
                        .unwrap_or(0),
                }
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct RuleStatus {
    pub name: String,
    pub enabled: bool,
    pub priority: String,
    pub fires_this_minute: u32,
    pub cooldown_remaining_ms: u64,
}

impl Default for DirectCommandChannel {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the default set of DCC rules with default [`EvasionParams`].
///
/// Thin wrapper over [`tactical_rules`] so existing callers/tests keep working
/// while the actual constants are now data-driven and operator/brain-tunable.
pub fn default_tactical_rules() -> Vec<TriggerRule> {
    tactical_rules(&EvasionParams::default())
}

/// Build the tactical DCC ruleset from tunable [`EvasionParams`]. The reflex
/// *mechanism* lives here; the *parameters* (chaff salvo, evasion heading,
/// radar-lock envelope, fuel reserve, threat threshold) come from config so the
/// policy is no longer hard-coded.
pub fn tactical_rules(params: &EvasionParams) -> Vec<TriggerRule> {
    vec![
        TriggerRule::new(
            "auto_chaff_on_radar_lock",
            TriggerCondition::RadarLock {
                min_track_quality: params.radar_lock_quality,
                max_range_m: params.radar_lock_range_m,
                track_affiliation: Some(Affiliation::Red),
            },
            PlatformCommand::FireChaff {
                platform_id: "self".into(),
                weapon_id: "chaff".into(),
                count: params.chaff_count,
                interval_s: params.chaff_interval_s,
            },
            CommandPriority::Critical,
        )
        .with_cooldown(params.chaff_cooldown_ms)
        .with_rate_limit(6),
        TriggerRule::new(
            "collision_avoidance",
            TriggerCondition::CollisionRisk {
                min_cpa_m: 200.0,
                max_tcpa_s: 60.0,
            },
            PlatformCommand::SetHeading {
                platform_id: "self".into(),
                heading_deg: params.collision_heading_deg,
                speed_ms: None,
                turn_direction: None,
            },
            CommandPriority::Critical,
        )
        .with_cooldown(params.collision_cooldown_ms)
        .with_rate_limit(30),
        TriggerRule::new(
            "auto_jam_on_threat_radar",
            TriggerCondition::RadarLock {
                min_track_quality: 0.5,
                max_range_m: 20000.0,
                track_affiliation: Some(Affiliation::Red),
            },
            PlatformCommand::JamStart {
                platform_id: "self".into(),
                jammer_id: "jammer-01".into(),
                frequency_hz: 10000.0,
                bandwidth_hz: 2000.0,
                target_track_id: String::new(),
            },
            CommandPriority::High,
        )
        .with_cooldown(10000),
        TriggerRule::new(
            "auto_rtb_on_low_fuel",
            TriggerCondition::UavFuelCritical {
                min_reserve_pct: params.low_fuel_reserve_pct,
            },
            PlatformCommand::ReturnToBase {
                uav_id: String::new(),
            },
            CommandPriority::Critical,
        )
        // Latch RTB: once commanded, don't re-issue every tick while fuel stays
        // low. The platform is already returning; spamming the order floods the
        // sim/actuator.
        .with_cooldown(30_000),
        // UAV comm loss → emergency RTB
        TriggerRule::new(
            "auto_abort_on_comm_loss",
            TriggerCondition::UavCommLost { timeout_s: 30.0 },
            PlatformCommand::ReturnToBase {
                uav_id: String::new(),
            },
            CommandPriority::Critical,
        )
        .with_cooldown(0),
        // UAV lost in action → notify FMA (High — let TCA re-task in next tick)
        TriggerRule::new(
            "auto_retask_on_uav_loss",
            TriggerCondition::UavLost,
            PlatformCommand::AuxCommand {
                platform_id: "fma".into(),
                key: "uav_lost".into(),
                value_json: "{}".into(),
            },
            CommandPriority::High,
        )
        .with_cooldown(5000),
        // New contact detected at long range → launch recon UAV
        TriggerRule::new(
            "auto_launch_recon_on_contact",
            TriggerCondition::ContactDetected {
                confidence: 0.6,
                range_m: 30000.0,
            },
            PlatformCommand::LaunchUav {
                uav_id: "recon-uav-01".into(),
            },
            CommandPriority::High,
        )
        .with_cooldown(60_000),
        // UMAA: ROE level changed to WeaponsHold → safe all weapons
        TriggerRule::new(
            "auto_safe_on_roe_hold",
            TriggerCondition::StateTransition {
                from_state: "*".into(),
                to_state: "roe_hold".into(),
            },
            PlatformCommand::WeaponSafeAll {
                platform_id: "self".into(),
            },
            CommandPriority::Critical,
        )
        .with_cooldown(0),
    ]
}

/// Build the DCC ruleset for a **single** autonomous UAV (CCA / LSUAV).
///
/// This is the fleet-free subset of [`default_tactical_rules`]: only reflexes a
/// lone airframe can execute on its own — chaff on radar lock, collision
/// avoidance, EW jam on threat radar, RTB on low fuel, RTB on comm loss, and
/// weapon-safe on ROE hold. It deliberately excludes fleet behaviors
/// (launch/recover/retask) which belong to the mothership (Track 2).
pub fn uav_single_rules() -> Vec<TriggerRule> {
    let keep = [
        "auto_chaff_on_radar_lock",
        "collision_avoidance",
        "auto_jam_on_threat_radar",
        "auto_rtb_on_low_fuel",
        "auto_abort_on_comm_loss",
        "auto_safe_on_roe_hold",
    ];
    default_tactical_rules()
        .into_iter()
        .filter(|r| keep.contains(&r.name.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_snapshot() -> WorldSnapshot {
        WorldSnapshot {
            timestamp: 0.0,
            platforms: vec![],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        }
    }

    fn low_fuel_uav(id: &str) -> WorldSnapshot {
        let mut platform = openfang_types::platform::PlatformState::minimal(id);
        platform.domain = openfang_types::platform::Domain::Air;
        platform.platform_type = "cca".into();
        platform.fuel = openfang_types::platform::FuelStatus {
            remaining_kg: 5.0,
            max_kg: 100.0,
            consumption_rate_kg_s: 0.1,
        };
        WorldSnapshot {
            timestamp: 1.0,
            platforms: vec![platform],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        }
    }

    #[test]
    fn test_dcc_default_creation() {
        let dcc = DirectCommandChannel::new();
        assert!(dcc.list_rules().is_empty());
        assert!(dcc.enabled.load(Ordering::SeqCst));
    }

    #[test]
    fn test_add_and_evaluate_always_rule() {
        let mut dcc = DirectCommandChannel::new();
        dcc.add_rule(TriggerRule::new(
            "test_always",
            TriggerCondition::Always,
            PlatformCommand::SetSpeed {
                platform_id: "test".into(),
                speed_ms: 10.0,
                acceleration_ms2: None,
            },
            CommandPriority::Critical,
        ));

        let (critical, high) = dcc.evaluate(&empty_snapshot(), "test");
        assert_eq!(critical.len(), 1);
        assert!(high.is_empty());
    }

    #[test]
    fn test_disable_rule() {
        let mut dcc = DirectCommandChannel::new();
        dcc.add_rule(TriggerRule::new(
            "test",
            TriggerCondition::Always,
            PlatformCommand::CommOn {
                platform_id: "x".into(),
            },
            CommandPriority::High,
        ));

        assert!(dcc.set_rule_enabled("test", false), "existing rule toggles");
        assert!(
            !dcc.set_rule_enabled("missing", false),
            "unknown rule reports no match"
        );
        let (critical, high) = dcc.evaluate(&empty_snapshot(), "test");
        assert!(critical.is_empty());
        assert!(high.is_empty());
    }

    #[test]
    fn test_dcc_disable_all() {
        let mut dcc = DirectCommandChannel::new();
        dcc.add_rule(TriggerRule::new(
            "test",
            TriggerCondition::Always,
            PlatformCommand::CommOn {
                platform_id: "x".into(),
            },
            CommandPriority::Critical,
        ));

        dcc.set_enabled(false);
        let (critical, high) = dcc.evaluate(&empty_snapshot(), "test");
        assert!(critical.is_empty());
    }

    #[test]
    fn test_default_tactical_rules() {
        let rules = default_tactical_rules();
        // Tactical ruleset covers: chaff/collision/jam + UAV RTB/comm-loss/retask/launch-recon + ROE-hold safe.
        // Assert a reasonable lower bound so the suite catches accidental rule removal.
        assert!(
            rules.len() >= 7,
            "expected ≥7 default rules, got {}",
            rules.len()
        );
        assert!(rules.iter().any(|r| r.name == "auto_chaff_on_radar_lock"));
        // single-UAV subset excludes fleet rules
        let single = uav_single_rules();
        assert!(single.iter().any(|r| r.name == "auto_rtb_on_low_fuel"));
        assert!(single.iter().any(|r| r.name == "auto_abort_on_comm_loss"));
        assert!(!single
            .iter()
            .any(|r| r.name == "auto_launch_recon_on_contact"));
        assert!(!single.iter().any(|r| r.name == "auto_retask_on_uav_loss"));
        assert!(rules.iter().any(|r| r.name == "collision_avoidance"));
        assert!(rules.iter().any(|r| r.name == "auto_rtb_on_low_fuel"));
        assert!(rules.iter().any(|r| r.name == "auto_abort_on_comm_loss"));
        assert!(rules.iter().any(|r| r.name == "auto_safe_on_roe_hold"));
    }

    #[test]
    fn uav_low_fuel_rtb_binds_own_platform_id() {
        let mut dcc = DirectCommandChannel::new();
        for rule in uav_single_rules() {
            if rule.name == "auto_rtb_on_low_fuel" {
                dcc.add_rule(rule);
            }
        }

        let (critical, high) = dcc.evaluate(&low_fuel_uav("cca-7"), "cca-7");
        assert!(high.is_empty());
        assert_eq!(critical.len(), 1);
        assert!(matches!(
            &critical[0],
            PlatformCommand::ReturnToBase { uav_id } if uav_id == "cca-7"
        ));
    }

    #[test]
    fn fuel_critical_ignores_platforms_without_fuel_telemetry() {
        // A surface vessel / sim that reports no fuel (max_kg == 0) must not be
        // read as an empty tank and trip emergency RTB.
        let mut dcc = DirectCommandChannel::new();
        dcc.add_rule(TriggerRule::new(
            "rtb_low_fuel",
            TriggerCondition::UavFuelCritical {
                min_reserve_pct: 0.12,
            },
            PlatformCommand::ReturnToBase {
                uav_id: String::new(),
            },
            CommandPriority::Critical,
        ));

        let mut platform = openfang_types::platform::PlatformState::minimal("self");
        platform.domain = openfang_types::platform::Domain::Air;
        platform.platform_type = "cca".into();
        platform.fuel = openfang_types::platform::FuelStatus {
            remaining_kg: 0.0,
            max_kg: 0.0,
            consumption_rate_kg_s: 0.0,
        };
        let snapshot = WorldSnapshot {
            timestamp: 1.0,
            platforms: vec![platform],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        };

        let (critical, _) = dcc.evaluate(&snapshot, "self");
        assert!(
            critical.is_empty(),
            "missing fuel telemetry must not trigger RTB"
        );
    }

    #[test]
    fn uav_comm_loss_evaluates_from_fleet_snapshot() {
        let mut dcc = DirectCommandChannel::new();
        dcc.add_rule(TriggerRule::new(
            "comm_loss",
            TriggerCondition::UavCommLost { timeout_s: 30.0 },
            PlatformCommand::ReturnToBase {
                uav_id: String::new(),
            },
            CommandPriority::Critical,
        ));

        let mut snapshot = empty_snapshot();
        snapshot.fleet = Some(openfang_types::platform::FleetSnapshot {
            mothership_id: "ms-1".into(),
            uavs: vec![openfang_types::platform::UavState {
                uav_id: "cca-7".into(),
                uav_type: "cca".into(),
                status: openfang_types::platform::UavStatus::OnMission,
                fuel_pct: 0.8,
                seconds_since_contact: 45.0,
                mission: None,
            }],
        });

        let (critical, _) = dcc.evaluate(&snapshot, "cca-7");
        assert_eq!(critical.len(), 1);
        assert!(matches!(
            &critical[0],
            PlatformCommand::ReturnToBase { uav_id } if uav_id == "cca-7"
        ));
    }

    fn hostile_track(
        track_id: &str,
        classification: &str,
        affiliation: Affiliation,
        quality: f64,
        range_m: f64,
        speed_ms: f64,
    ) -> openfang_types::platform::Track {
        openfang_types::platform::Track {
            track_id: track_id.into(),
            target_name: String::new(),
            classification: classification.into(),
            affiliation,
            iff: "foe".into(),
            position_lla: None,
            heading_deg: None,
            speed_ms: Some(speed_ms),
            range_m: Some(range_m),
            bearing_deg: None,
            elevation_deg: None,
            quality,
            stale: false,
            last_update_s: 0.0,
            is_active: true,
        }
    }

    fn snapshot_with_track(track: openfang_types::platform::Track) -> WorldSnapshot {
        let mut platform = openfang_types::platform::PlatformState::minimal("self");
        platform.tracks = vec![track];
        WorldSnapshot {
            timestamp: 1.0,
            platforms: vec![platform],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        }
    }

    #[test]
    fn high_threat_fires_on_assessed_danger() {
        let mut dcc = DirectCommandChannel::new();
        dcc.add_rule(TriggerRule::new(
            "evade_high_threat",
            TriggerCondition::HighThreat {
                min_threat_score: 0.7,
                max_range_m: 8000.0,
                track_affiliation: Some(Affiliation::Red),
            },
            PlatformCommand::SetHeading {
                platform_id: "self".into(),
                heading_deg: 120.0,
                speed_ms: None,
                turn_direction: None,
            },
            CommandPriority::Critical,
        ));
        let snap = snapshot_with_track(hostile_track(
            "self:3",
            "destroyer",
            Affiliation::Red,
            0.9,
            5000.0,
            50.0,
        ));
        let (critical, _) = dcc.evaluate(&snap, "self");
        assert_eq!(
            critical.len(),
            1,
            "high assessed threat must trip the reflex"
        );
    }

    #[test]
    fn high_threat_ignores_low_score_and_wrong_affiliation() {
        let mut dcc = DirectCommandChannel::new();
        dcc.add_rule(TriggerRule::new(
            "evade_high_threat",
            TriggerCondition::HighThreat {
                min_threat_score: 0.7,
                max_range_m: 8000.0,
                track_affiliation: Some(Affiliation::Red),
            },
            PlatformCommand::WeaponSafeAll {
                platform_id: "self".into(),
            },
            CommandPriority::Critical,
        ));
        // Friendly high-quality contact: filtered out by affiliation.
        let friendly = snapshot_with_track(hostile_track(
            "self:1",
            "destroyer",
            Affiliation::Blue,
            0.95,
            3000.0,
            40.0,
        ));
        assert!(dcc.evaluate(&friendly, "self").0.is_empty());
        // Distant, low-quality hostile: below threat threshold.
        let faint = snapshot_with_track(hostile_track(
            "self:2",
            "fishing_boat",
            Affiliation::Red,
            0.2,
            7000.0,
            2.0,
        ));
        assert!(dcc.evaluate(&faint, "self").0.is_empty());
    }

    #[test]
    fn incoming_munition_fires_on_weapon_class() {
        let mut dcc = DirectCommandChannel::new();
        dcc.add_rule(TriggerRule::new(
            "defeat_inbound",
            TriggerCondition::IncomingMunition {
                min_threat_score: 0.9,
                max_range_m: 12000.0,
            },
            PlatformCommand::FireChaff {
                platform_id: "self".into(),
                weapon_id: "chaff".into(),
                count: 4,
                interval_s: 0.4,
            },
            CommandPriority::Critical,
        ));
        // Even a low-quality contact fires if it's weapon-class.
        let inbound = snapshot_with_track(hostile_track(
            "self:9",
            "anti_ship_missile",
            Affiliation::Red,
            0.3,
            6000.0,
            280.0,
        ));
        assert_eq!(dcc.evaluate(&inbound, "self").0.len(), 1);
    }

    #[test]
    fn load_rules_replaces_and_remove_rule_drops() {
        let mut dcc = DirectCommandChannel::new();
        dcc.load_rules(default_tactical_rules());
        let installed = dcc.list_rules().len();
        assert!(installed >= 7);
        // load_rules replaces, not appends.
        dcc.load_rules(vec![TriggerRule::new(
            "solo",
            TriggerCondition::Always,
            PlatformCommand::WeaponSafeAll {
                platform_id: "self".into(),
            },
            CommandPriority::High,
        )]);
        assert_eq!(dcc.list_rules().len(), 1);
        assert!(dcc.remove_rule("solo"));
        assert!(!dcc.remove_rule("solo"));
        assert!(dcc.list_rules().is_empty());
    }

    #[test]
    fn trigger_rule_roundtrips_through_serde() {
        // The data-driven path: a rule must survive JSON so the brain/config can
        // author it. Tests both a tagged condition and the action payload.
        let rule = TriggerRule::new(
            "evade_high_threat",
            TriggerCondition::HighThreat {
                min_threat_score: 0.65,
                max_range_m: 9000.0,
                track_affiliation: Some(Affiliation::Red),
            },
            PlatformCommand::SetHeading {
                platform_id: "self".into(),
                heading_deg: 135.0,
                speed_ms: None,
                turn_direction: None,
            },
            CommandPriority::Critical,
        )
        .with_cooldown(3000);
        let json = serde_json::to_string(&rule).expect("serialize rule");
        let back: TriggerRule = serde_json::from_str(&json).expect("deserialize rule");
        assert_eq!(back.name, "evade_high_threat");
        assert_eq!(back.cooldown_ms, 3000);
        assert!(matches!(
            back.condition,
            TriggerCondition::HighThreat {
                min_threat_score,
                ..
            } if (min_threat_score - 0.65).abs() < 1e-9
        ));
    }
}
