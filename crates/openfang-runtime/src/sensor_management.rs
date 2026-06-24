use std::collections::HashMap;

use openfang_types::config::{AutonomyClassDisposition, SensorDisposition, SensorPolicyConfig};
use openfang_types::platform::{
    Affiliation, LinkQuality, PlatformCommand, PlatformState, SensorState, SensorType,
};
use openfang_types::tactical::{CandidateIntent, CommandClass, CommandPriority, IntentSource};
use openfang_types::umaa::{AutonomyMode, LinkStatus, WeaponReleaseLevel};
use serde::Serialize;

use crate::cca_role::posture_for;
use crate::cerebellum_services::{
    CerebellumService, CerebellumServiceId, ServiceAuditHint, ServiceContext, ServiceOutput,
};
use crate::sensor_fusion::ThreatLevel;
use crate::sensor_policy::SensorPolicyEngine;

#[derive(Debug, Clone)]
struct OperatorOverride {
    desired_mode: String,
    expires_at: f64,
}

/// Tunable autonomy thresholds, sourced from `[platform.sensor_policy]`.
#[derive(Debug, Clone, Copy)]
struct SmsThresholds {
    override_ttl_s: f64,
    damage_force_off: f64,
    survival_threat_range_m: f64,
    esm_threat_range_m: f64,
    track_refresh_quality: f64,
}

impl Default for SmsThresholds {
    fn default() -> Self {
        let cfg = SensorPolicyConfig::default();
        Self {
            override_ttl_s: cfg.override_ttl_s,
            damage_force_off: cfg.damage_force_off,
            survival_threat_range_m: cfg.survival_threat_range_m,
            esm_threat_range_m: cfg.esm_threat_range_m,
            track_refresh_quality: cfg.track_refresh_quality,
        }
    }
}

impl SmsThresholds {
    fn from_config(cfg: &SensorPolicyConfig) -> Self {
        Self {
            override_ttl_s: cfg.override_ttl_s,
            damage_force_off: cfg.damage_force_off,
            survival_threat_range_m: cfg.survival_threat_range_m,
            esm_threat_range_m: cfg.esm_threat_range_m,
            track_refresh_quality: cfg.track_refresh_quality,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SensorManagementStatus {
    pub sensor_id: String,
    pub sensor_type: SensorType,
    pub current_mode: String,
    pub expected_mode: Option<String>,
    pub disposition: String,
    pub recent_reason: Option<String>,
    pub damage: f64,
}

#[derive(Debug, Clone)]
struct SensorDecisionStatus {
    expected_mode: Option<String>,
    disposition: String,
    recent_reason: Option<String>,
}

pub struct SensorManagementService {
    policy: SensorPolicyEngine,
    overrides: HashMap<String, OperatorOverride>,
    last_decisions: HashMap<String, SensorDecisionStatus>,
    thresholds: SmsThresholds,
    /// Live ROE level, refreshed each tick by the control loop. Drives the
    /// policy engine's release matrix alongside the EMCON posture.
    roe: WeaponReleaseLevel,
}

impl Default for SensorManagementService {
    fn default() -> Self {
        Self::new(SensorPolicyEngine::default())
    }
}

impl SensorManagementService {
    pub fn new(policy: SensorPolicyEngine) -> Self {
        Self {
            policy,
            overrides: HashMap::new(),
            last_decisions: HashMap::new(),
            thresholds: SmsThresholds::default(),
            roe: WeaponReleaseLevel::WeaponsHold,
        }
    }

    /// Build the service from the deployment `[platform.sensor_policy]` block,
    /// wiring both the policy engine and the tunable autonomy thresholds.
    pub fn from_config(cfg: &SensorPolicyConfig) -> Self {
        Self {
            policy: SensorPolicyEngine::new(cfg.clone()),
            overrides: HashMap::new(),
            last_decisions: HashMap::new(),
            thresholds: SmsThresholds::from_config(cfg),
            roe: WeaponReleaseLevel::WeaponsHold,
        }
    }

    /// Refresh the live ROE level (called by the control loop before `evaluate`).
    pub fn set_roe(&mut self, roe: WeaponReleaseLevel) {
        self.roe = roe;
    }

    pub fn note_operator_sensor_intent(
        &mut self,
        sensor_id: impl Into<String>,
        desired_mode: impl Into<String>,
        now: f64,
    ) {
        self.overrides.insert(
            sensor_id.into(),
            OperatorOverride {
                desired_mode: desired_mode.into(),
                expires_at: now + self.thresholds.override_ttl_s,
            },
        );
    }

    pub fn status_for(&self, own_platform: Option<&PlatformState>) -> Vec<SensorManagementStatus> {
        let Some(platform) = own_platform else {
            return Vec::new();
        };
        platform
            .onboard_sensors
            .iter()
            .map(|sensor| {
                let decision = self.last_decisions.get(&sensor.sensor_id);
                SensorManagementStatus {
                    sensor_id: sensor.sensor_id.clone(),
                    sensor_type: sensor.sensor_type,
                    current_mode: sensor.mode.clone(),
                    expected_mode: decision.and_then(|d| d.expected_mode.clone()),
                    disposition: decision
                        .map(|d| d.disposition.clone())
                        .unwrap_or_else(|| "unknown".into()),
                    recent_reason: decision.and_then(|d| d.recent_reason.clone()),
                    damage: sensor.damage,
                }
            })
            .collect()
    }

    fn intent(
        &self,
        command: PlatformCommand,
        priority: CommandPriority,
        now: f64,
        reason: impl Into<String>,
    ) -> CandidateIntent {
        CandidateIntent::new(
            command,
            priority,
            IntentSource::Dcc {
                rule_name: format!("sms:{}", CerebellumServiceId::Sms.label()),
            },
            now,
            reason,
        )
    }
}

impl CerebellumService for SensorManagementService {
    fn id(&self) -> CerebellumServiceId {
        CerebellumServiceId::Sms
    }

    fn evaluate(&mut self, ctx: &ServiceContext<'_>) -> ServiceOutput {
        let Some(state) = ctx.own_platform else {
            return ServiceOutput::empty();
        };
        self.overrides
            .retain(|_, operator_override| operator_override.expires_at >= ctx.now);

        let posture = posture_for(ctx.posture);
        let roe = self.roe;
        let thresholds = self.thresholds;
        let mut out = ServiceOutput::empty();

        let autonomy_mode = effective_autonomy_mode(state, ctx);

        // Fused-track回灌: the SMS no longer reasons only about raw per-sensor
        // returns — it consumes the unified fusion picture (stable threat level)
        // so a Kalman-confirmed High/Critical contact drives active-radar posture
        // even when the latest raw return for it is momentarily stale or low
        // quality. Empty `fused_tracks` (isolated tests / first tick) ⇒ no-op.
        let fused_high_threat = ctx
            .fused_tracks
            .iter()
            .any(|track| track.threat_level >= ThreatLevel::High);

        for sensor in &state.onboard_sensors {
            if sensor.damage >= thresholds.damage_force_off {
                self.last_decisions.insert(
                    sensor.sensor_id.clone(),
                    SensorDecisionStatus {
                        expected_mode: Some("off".into()),
                        disposition: "force_off".into(),
                        recent_reason: Some("sensor_health".into()),
                    },
                );
                if sensor_is_on(sensor) {
                    out.intents.push(self.intent(
                        PlatformCommand::SensorOff {
                            platform_id: state.id.clone(),
                            sensor_id: sensor.sensor_id.clone(),
                        },
                        CommandPriority::Critical,
                        ctx.now,
                        format!(
                            "sms sensor health damage {:.2} force-off {}",
                            sensor.damage, sensor.sensor_id
                        ),
                    ));
                }
                if let Some(failover) = find_failover_sensor(
                    &state.onboard_sensors,
                    &sensor.sensor_id,
                    sensor.sensor_type,
                    thresholds.damage_force_off,
                ) {
                    self.last_decisions.insert(
                        failover.sensor_id.clone(),
                        SensorDecisionStatus {
                            expected_mode: Some("on".into()),
                            disposition: "auto".into(),
                            recent_reason: Some("sensor_failover".into()),
                        },
                    );
                    if !sensor_is_on(failover) {
                        out.intents.push(self.intent(
                            PlatformCommand::SensorOn {
                                platform_id: state.id.clone(),
                                sensor_id: failover.sensor_id.clone(),
                            },
                            CommandPriority::High,
                            ctx.now,
                            format!(
                                "sms failover {} -> {} after damage {:.2}",
                                sensor.sensor_id, failover.sensor_id, sensor.damage
                            ),
                        ));
                        out.audit_hints.push(
                            ServiceAuditHint::new(CerebellumServiceId::Sms, "sensor_failover")
                                .with_detail(format!(
                                    "from={} to={} type={:?}",
                                    sensor.sensor_id, failover.sensor_id, sensor.sensor_type
                                )),
                        );
                    }
                }
                continue;
            }

            if let Some(desired_mode) = self
                .overrides
                .get(&sensor.sensor_id)
                .map(|operator_override| operator_override.desired_mode.clone())
            {
                self.last_decisions.insert(
                    sensor.sensor_id.clone(),
                    SensorDecisionStatus {
                        expected_mode: Some(desired_mode.clone()),
                        disposition: "operator_override".into(),
                        recent_reason: Some("operator_explicit".into()),
                    },
                );
                // Actively drive the sensor to the operator-requested mode. SMS
                // never reverses an explicit operator command while the override
                // is live; it converges the live state onto `desired_mode` and
                // emits nothing once they match (idempotent).
                if let Some(command) = enforce_command(&state.id, sensor, &desired_mode) {
                    out.intents.push(self.intent(
                        command,
                        CommandPriority::High,
                        ctx.now,
                        format!(
                            "sms operator override enforce {} -> {}",
                            sensor.sensor_id, desired_mode
                        ),
                    ));
                }
                out.audit_hints.push(
                    ServiceAuditHint::new(CerebellumServiceId::Sms, "sensor_operator_override")
                        .with_detail(format!(
                            "sensor_id={} desired_mode={}",
                            sensor.sensor_id, desired_mode
                        )),
                );
                continue;
            }

            if survival_radar_needed(state, sensor, thresholds.survival_threat_range_m) {
                self.last_decisions.insert(
                    sensor.sensor_id.clone(),
                    SensorDecisionStatus {
                        expected_mode: Some("on".into()),
                        disposition: "force_on".into(),
                        recent_reason: Some("lost_link_survival".into()),
                    },
                );
                if !sensor_is_on(sensor) {
                    out.intents.push(self.intent(
                        PlatformCommand::SensorOn {
                            platform_id: state.id.clone(),
                            sensor_id: sensor.sensor_id.clone(),
                        },
                        CommandPriority::Critical,
                        ctx.now,
                        format!("sms lost-link survival radar on {}", sensor.sensor_id),
                    ));
                }
                continue;
            }

            if esm_threat_needs_active_radar(
                state,
                sensor,
                thresholds.esm_threat_range_m,
                fused_high_threat,
            ) {
                let fused_driven = fused_high_threat
                    && !esm_threat_needs_active_radar(
                        state,
                        sensor,
                        thresholds.esm_threat_range_m,
                        false,
                    );
                let reason = if fused_driven {
                    "fused_high_threat_active_track"
                } else {
                    "esm_threat_active_track"
                };
                self.last_decisions.insert(
                    sensor.sensor_id.clone(),
                    SensorDecisionStatus {
                        expected_mode: Some("on".into()),
                        disposition: "auto".into(),
                        recent_reason: Some(reason.into()),
                    },
                );
                if should_block_autonomous_active_emitter(
                    autonomy_mode,
                    sensor.sensor_type,
                    SensorDisposition::Auto,
                    posture.emcon,
                    ctx,
                ) {
                    out.audit_hints.push(
                        ServiceAuditHint::new(
                            CerebellumServiceId::Sms,
                            "sensor_pending_approval_l3",
                        )
                        .with_detail(format!(
                            "sensor_id={} reason={reason} autonomy=L3",
                            sensor.sensor_id
                        )),
                    );
                    continue;
                }
                if !sensor_is_on(sensor) {
                    let detail = if autonomy_mode == AutonomyMode::HumanOnTheLoop {
                        "autonomy_l4_advisory"
                    } else {
                        reason
                    };
                    out.intents.push(self.intent(
                        PlatformCommand::SensorOn {
                            platform_id: state.id.clone(),
                            sensor_id: sensor.sensor_id.clone(),
                        },
                        CommandPriority::High,
                        ctx.now,
                        format!("sms {detail} radar on {}", sensor.sensor_id),
                    ));
                    if autonomy_mode == AutonomyMode::HumanOnTheLoop {
                        out.audit_hints.push(
                            ServiceAuditHint::new(CerebellumServiceId::Sms, "sensor_l4_advisory")
                                .with_detail(format!(
                                    "sensor_id={} reason={reason}",
                                    sensor.sensor_id
                                )),
                        );
                    }
                }
                continue;
            }

            if track_refresh_needed(state, sensor, thresholds.track_refresh_quality) {
                self.last_decisions.insert(
                    sensor.sensor_id.clone(),
                    SensorDecisionStatus {
                        expected_mode: Some("track".into()),
                        disposition: "auto".into(),
                        recent_reason: Some("track_quality_refresh".into()),
                    },
                );
                if sensor.mode != "track" {
                    out.intents.push(self.intent(
                        PlatformCommand::SensorSetMode {
                            platform_id: state.id.clone(),
                            sensor_id: sensor.sensor_id.clone(),
                            mode: "track".into(),
                        },
                        CommandPriority::High,
                        ctx.now,
                        format!("sms refresh stale track via {}", sensor.sensor_id),
                    ));
                }
                continue;
            }

            match self
                .policy
                .disposition_for(sensor.sensor_type, posture.emcon, roe)
            {
                SensorDisposition::ForceOff | SensorDisposition::Deny if sensor_is_on(sensor) => {
                    self.last_decisions.insert(
                        sensor.sensor_id.clone(),
                        SensorDecisionStatus {
                            expected_mode: Some("off".into()),
                            disposition: "force_off".into(),
                            recent_reason: Some("emcon_restricted".into()),
                        },
                    );
                    out.intents.push(self.intent(
                        PlatformCommand::SensorOff {
                            platform_id: state.id.clone(),
                            sensor_id: sensor.sensor_id.clone(),
                        },
                        CommandPriority::High,
                        ctx.now,
                        format!(
                            "sms emcon {:?} force-off {}",
                            posture.emcon, sensor.sensor_id
                        ),
                    ));
                    out.audit_hints.push(
                        ServiceAuditHint::new(CerebellumServiceId::Sms, "sensor_force_off")
                            .with_detail(format!(
                                "sensor_id={} emcon={:?}",
                                sensor.sensor_id, posture.emcon
                            )),
                    );
                }
                SensorDisposition::ForceOn if !sensor_is_on(sensor) => {
                    if should_block_autonomous_active_emitter(
                        autonomy_mode,
                        sensor.sensor_type,
                        SensorDisposition::ForceOn,
                        posture.emcon,
                        ctx,
                    ) {
                        self.last_decisions.insert(
                            sensor.sensor_id.clone(),
                            SensorDecisionStatus {
                                expected_mode: Some("on".into()),
                                disposition: "pending_approval".into(),
                                recent_reason: Some("autonomy_l3_pending".into()),
                            },
                        );
                        out.audit_hints.push(
                            ServiceAuditHint::new(
                                CerebellumServiceId::Sms,
                                "sensor_pending_approval_l3",
                            )
                            .with_detail(format!(
                                "sensor_id={} emcon={:?} autonomy=L3",
                                sensor.sensor_id, posture.emcon
                            )),
                        );
                    } else {
                        self.last_decisions.insert(
                            sensor.sensor_id.clone(),
                            SensorDecisionStatus {
                                expected_mode: Some("on".into()),
                                disposition: "force_on".into(),
                                recent_reason: Some("policy_force_on".into()),
                            },
                        );
                        out.intents.push(self.intent(
                            PlatformCommand::SensorOn {
                                platform_id: state.id.clone(),
                                sensor_id: sensor.sensor_id.clone(),
                            },
                            CommandPriority::High,
                            ctx.now,
                            format!("sms policy force-on {}", sensor.sensor_id),
                        ));
                    }
                }
                SensorDisposition::PendingApproval => {
                    // Policy demands a human in the loop before this emitter can
                    // change state autonomously. SMS records the gap and raises an
                    // approval audit, but never emits an autonomous on/off intent.
                    self.last_decisions.insert(
                        sensor.sensor_id.clone(),
                        SensorDecisionStatus {
                            expected_mode: None,
                            disposition: "pending_approval".into(),
                            recent_reason: Some("policy_pending_approval".into()),
                        },
                    );
                    out.audit_hints.push(
                        ServiceAuditHint::new(CerebellumServiceId::Sms, "sensor_pending_approval")
                            .with_detail(format!(
                                "sensor_id={} emcon={:?} roe={:?}",
                                sensor.sensor_id, posture.emcon, roe
                            )),
                    );
                }
                disposition => {
                    self.last_decisions.insert(
                        sensor.sensor_id.clone(),
                        SensorDecisionStatus {
                            expected_mode: None,
                            disposition: format!("{disposition:?}").to_lowercase(),
                            recent_reason: Some("policy_no_action".into()),
                        },
                    );
                }
            }
        }

        out
    }
}

fn sensor_is_on(sensor: &SensorState) -> bool {
    !matches!(sensor.mode.as_str(), "standby" | "off")
}

/// Translate an operator override `desired_mode` into the platform command that
/// converges the live sensor onto it. Returns `None` when already satisfied so
/// the override stays idempotent across ticks.
fn enforce_command(
    platform_id: &str,
    sensor: &SensorState,
    desired_mode: &str,
) -> Option<PlatformCommand> {
    match desired_mode {
        "off" | "standby" => sensor_is_on(sensor).then(|| PlatformCommand::SensorOff {
            platform_id: platform_id.to_string(),
            sensor_id: sensor.sensor_id.clone(),
        }),
        "on" => (!sensor_is_on(sensor)).then(|| PlatformCommand::SensorOn {
            platform_id: platform_id.to_string(),
            sensor_id: sensor.sensor_id.clone(),
        }),
        mode => (sensor.mode != mode).then(|| PlatformCommand::SensorSetMode {
            platform_id: platform_id.to_string(),
            sensor_id: sensor.sensor_id.clone(),
            mode: mode.to_string(),
        }),
    }
}

fn survival_radar_needed(state: &PlatformState, sensor: &SensorState, threat_range_m: f64) -> bool {
    matches!(sensor.sensor_type, SensorType::Radar)
        && state
            .link
            .map(|link| link.quality == LinkQuality::Lost)
            .unwrap_or(false)
        && state.tracks.iter().any(|track| {
            matches!(track.affiliation, Affiliation::Red | Affiliation::Foe)
                && !track.stale
                && track
                    .range_m
                    .map(|range| range <= threat_range_m)
                    .unwrap_or(false)
        })
}

fn track_refresh_needed(state: &PlatformState, sensor: &SensorState, refresh_quality: f64) -> bool {
    matches!(sensor.sensor_type, SensorType::EOIR)
        && state.tracks.iter().any(|track| {
            matches!(track.affiliation, Affiliation::Red | Affiliation::Foe)
                && (track.stale || track.quality < refresh_quality)
        })
}

fn find_failover_sensor<'a>(
    sensors: &'a [SensorState],
    damaged_id: &str,
    sensor_type: SensorType,
    damage_force_off: f64,
) -> Option<&'a SensorState> {
    sensors.iter().find(|sensor| {
        sensor.sensor_id != damaged_id
            && sensor.sensor_type == sensor_type
            && sensor.damage < damage_force_off
    })
}

fn effective_autonomy_mode(state: &PlatformState, ctx: &ServiceContext<'_>) -> AutonomyMode {
    if let Some(profile) = ctx.autonomy {
        if profile
            .pending_approval_classes
            .iter()
            .any(|token| token == "sensor")
        {
            return AutonomyMode::HumanSupervised;
        }
    }
    let link_status = state
        .link
        .map(|link| match link.quality {
            LinkQuality::Lost => LinkStatus::Lost,
            LinkQuality::Marginal | LinkQuality::Poor => LinkStatus::Degraded,
            _ => LinkStatus::Connected,
        })
        .unwrap_or(LinkStatus::Connected);
    AutonomyMode::from_link_status(link_status)
}

fn should_block_autonomous_active_emitter(
    autonomy: AutonomyMode,
    sensor_type: SensorType,
    disposition: SensorDisposition,
    emcon: crate::cca_role::EmconLevel,
    ctx: &ServiceContext<'_>,
) -> bool {
    if !matches!(sensor_type, SensorType::Radar | SensorType::Lidar) {
        return false;
    }
    if matches!(autonomy, AutonomyMode::FullyAutonomous) {
        return false;
    }
    if matches!(autonomy, AutonomyMode::HumanOnTheLoop) {
        return false;
    }
    if let Some(profile) = ctx.autonomy {
        if profile.disposition_for(CommandClass::Sensor)
            == AutonomyClassDisposition::PendingApproval
        {
            return true;
        }
    }
    matches!(autonomy, AutonomyMode::HumanSupervised)
        && matches!(
            emcon,
            crate::cca_role::EmconLevel::Restricted | crate::cca_role::EmconLevel::Silent
        )
        && matches!(
            disposition,
            SensorDisposition::Auto | SensorDisposition::ForceOn
        )
}

fn esm_threat_needs_active_radar(
    state: &PlatformState,
    sensor: &SensorState,
    threat_range_m: f64,
    fused_high_threat: bool,
) -> bool {
    if !matches!(sensor.sensor_type, SensorType::Radar) {
        return false;
    }
    // Fused回灌: a Kalman-confirmed High/Critical contact justifies an active
    // radar cue on its own, independent of whether ESM is currently radiating.
    if fused_high_threat {
        return true;
    }
    let esm_active = state
        .onboard_sensors
        .iter()
        .any(|s| s.sensor_type == SensorType::ESM && sensor_is_on(s));
    if !esm_active {
        return false;
    }
    state.tracks.iter().any(|track| {
        matches!(track.affiliation, Affiliation::Red | Affiliation::Foe)
            && (track.quality < 0.5 || track.stale)
            && track
                .range_m
                .map(|range| range <= threat_range_m)
                .unwrap_or(true)
    })
}
