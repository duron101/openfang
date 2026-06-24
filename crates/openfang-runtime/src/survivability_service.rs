//! Platform Survivability Service (PSS) — the long-missing cerebellum lane.
//!
//! Watches own-platform health (fuel/battery + damage) every tick and emits
//! deterministic survival reflexes:
//!
//! - **damage ≥ critical_damage**  →  Critical `SetSpeed` to slow-stop, plus
//!   Critical `ReturnToBase`. The ACS preemption policy ensures this beats any
//!   normal-priority navigation intent already on the queue.
//! - **fuel ≤ rtb_fuel_pct**       →  High `ReturnToBase` (graceful recall;
//!   ACS still deconflicts against other navigation intents but won't preempt
//!   weapons-defense reflexes).
//! - **fuel ≤ low_fuel_pct**       →  audit-only warning hint (no intent
//!   emitted yet); used to surface the dashboard "low fuel" indicator.
//!
//! Design choices, per the plan:
//!
//! 1. PSS sits between the SMS poll and the role/fleet logic in the control
//!    loop so its intents enter the cerebellum queue alongside DCC reflexes.
//! 2. It only ever emits intents — never touches the adapter or audit log
//!    directly. The cerebellum will route them through SPGS/WMS like any
//!    other producer.
//! 3. PSS is debounced per-event-kind so a sustained fault doesn't flood the
//!    queue every tick; once an intent is emitted, the same event must
//!    "recover" before it can fire again.
//! 4. PSS is allocation-light: it never grows beyond a small fixed set of
//!    `String` ids and the per-evaluation `Vec<CandidateIntent>`.

use openfang_types::platform::PlatformCommand;
use openfang_types::tactical::{CandidateIntent, CommandPriority, IntentSource};

use crate::cerebellum_services::{
    CerebellumService, CerebellumServiceId, ServiceAuditHint, ServiceContext, ServiceOutput,
};

/// Thresholds the PSS uses to fire reflexes. All thresholds are in *fractional*
/// form (0.0–1.0) so the same service applies to USVs, UAVs, and UUVs without
/// per-platform tuning.
#[derive(Debug, Clone, Copy)]
pub struct PssThresholds {
    /// Fuel/battery fraction at which `ReturnToBase` is emitted (default 25%).
    pub rtb_fuel_pct: f64,
    /// Fuel/battery fraction at which the dashboard low-fuel warning fires
    /// (default 40%). Audit-only.
    pub low_fuel_pct: f64,
    /// Damage fraction at which the Critical slow-and-RTB sequence fires
    /// (default 60%).
    pub critical_damage: f64,
    /// Damage fraction at which the audit-only "damage advisory" fires
    /// (default 30%).
    pub advisory_damage: f64,
    /// Cruise speed (m/s) used when slowing for damage recovery. Conservative
    /// default keeps the boat moving but well below max speed.
    pub recovery_speed_ms: f64,
}

impl Default for PssThresholds {
    fn default() -> Self {
        Self {
            rtb_fuel_pct: 0.25,
            low_fuel_pct: 0.40,
            critical_damage: 0.60,
            advisory_damage: 0.30,
            recovery_speed_ms: 3.0,
        }
    }
}

/// Per-event latched state so a sustained fault doesn't re-emit every tick.
#[derive(Debug, Default, Clone, Copy)]
struct PssLatch {
    /// True while we have already emitted an RTB for low fuel and the fuel
    /// hasn't recovered above `rtb_fuel_pct`.
    rtb_active: bool,
    /// True while we have already emitted the critical damage sequence and
    /// damage hasn't recovered below `critical_damage`.
    critical_active: bool,
    /// True while we have already emitted the low-fuel warning.
    low_fuel_active: bool,
    /// True while we have already emitted the damage advisory.
    damage_advisory_active: bool,
}

/// Platform Survivability Service.
pub struct SurvivabilityService {
    thresholds: PssThresholds,
    latch: PssLatch,
}

impl Default for SurvivabilityService {
    fn default() -> Self {
        Self::new(PssThresholds::default())
    }
}

impl SurvivabilityService {
    pub fn new(thresholds: PssThresholds) -> Self {
        Self {
            thresholds,
            latch: PssLatch::default(),
        }
    }

    /// Current thresholds (for dashboards / config introspection).
    pub fn thresholds(&self) -> &PssThresholds {
        &self.thresholds
    }

    /// Replace thresholds at runtime (config hot-reload).
    pub fn set_thresholds(&mut self, thresholds: PssThresholds) {
        self.thresholds = thresholds;
    }

    /// Reset all latches — used in tests and on platform reset.
    pub fn reset(&mut self) {
        self.latch = PssLatch::default();
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
                rule_name: format!("pss:{}", CerebellumServiceId::Pss.label()),
            },
            now,
            reason,
        )
    }
}

impl CerebellumService for SurvivabilityService {
    fn id(&self) -> CerebellumServiceId {
        CerebellumServiceId::Pss
    }

    fn evaluate(&mut self, ctx: &ServiceContext<'_>) -> ServiceOutput {
        let Some(state) = ctx.own_platform else {
            return ServiceOutput::empty();
        };

        let fuel_pct = state.fuel.remaining_pct();
        let damage = state.damage.clamp(0.0, 1.0);
        let id = state.id.clone();
        let now = ctx.now;
        let mut out = ServiceOutput::empty();

        // ── Critical damage: slow + RTB ────────────────────────────────
        if damage >= self.thresholds.critical_damage {
            if !self.latch.critical_active {
                self.latch.critical_active = true;
                out.intents.push(self.intent(
                    PlatformCommand::SetSpeed {
                        platform_id: id.clone(),
                        speed_ms: self.thresholds.recovery_speed_ms,
                        acceleration_ms2: None,
                    },
                    CommandPriority::Critical,
                    now,
                    format!("pss critical damage {:.2} — slow to recovery", damage),
                ));
                out.intents.push(self.intent(
                    PlatformCommand::ReturnToBase { uav_id: id.clone() },
                    CommandPriority::Critical,
                    now,
                    format!("pss critical damage {:.2} — return to base", damage),
                ));
                out.audit_hints.push(
                    ServiceAuditHint::new(CerebellumServiceId::Pss, "critical_damage")
                        .with_detail(format!("damage={damage:.2}")),
                );
            }
        } else {
            self.latch.critical_active = false;
        }

        // ── Damage advisory (audit only) ──────────────────────────────
        if damage >= self.thresholds.advisory_damage && damage < self.thresholds.critical_damage {
            if !self.latch.damage_advisory_active {
                self.latch.damage_advisory_active = true;
                out.audit_hints.push(
                    ServiceAuditHint::new(CerebellumServiceId::Pss, "damage_advisory")
                        .with_detail(format!("damage={damage:.2}")),
                );
            }
        } else if damage < self.thresholds.advisory_damage {
            self.latch.damage_advisory_active = false;
        }

        // ── Low fuel → RTB ─────────────────────────────────────────────
        // Skip if a critical-damage RTB is already active to avoid double-RTB.
        if fuel_pct <= self.thresholds.rtb_fuel_pct && !self.latch.critical_active {
            if !self.latch.rtb_active {
                self.latch.rtb_active = true;
                out.intents.push(self.intent(
                    PlatformCommand::ReturnToBase { uav_id: id.clone() },
                    CommandPriority::High,
                    now,
                    format!("pss fuel low {:.2} — return to base", fuel_pct),
                ));
                out.audit_hints.push(
                    ServiceAuditHint::new(CerebellumServiceId::Pss, "low_fuel_rtb")
                        .with_detail(format!("fuel_pct={fuel_pct:.2}")),
                );
            }
        } else if fuel_pct > self.thresholds.rtb_fuel_pct {
            self.latch.rtb_active = false;
        }

        // ── Low fuel warning (audit only) ──────────────────────────────
        if fuel_pct <= self.thresholds.low_fuel_pct && fuel_pct > self.thresholds.rtb_fuel_pct {
            if !self.latch.low_fuel_active {
                self.latch.low_fuel_active = true;
                out.audit_hints.push(
                    ServiceAuditHint::new(CerebellumServiceId::Pss, "low_fuel_warning")
                        .with_detail(format!("fuel_pct={fuel_pct:.2}")),
                );
            }
        } else if fuel_pct > self.thresholds.low_fuel_pct {
            self.latch.low_fuel_active = false;
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::platform::{
        Affiliation, CcaRole, Domain, FuelStatus, PlatformCapabilities, PlatformState, Pose,
        Velocity,
    };

    fn caps() -> PlatformCapabilities {
        PlatformCapabilities {
            supports_motion_control: true,
            supports_sensor_control: true,
            supports_weapon_control: true,
            supports_jammer_control: true,
            supports_comm_control: true,
            supports_uav_launch_recovery: false,
            supports_formation_control: true,
            supports_handoff: true,
            max_platforms: 1,
            supports_simulation: true,
            supports_hardware: false,
        }
    }

    fn state(fuel_pct: f64, damage: f64) -> PlatformState {
        let mut s = PlatformState::minimal("self");
        s.affiliation = Affiliation::Blue;
        s.domain = Domain::Surface;
        s.pose = Pose {
            lat_deg: 30.0,
            lon_deg: 120.0,
            alt_m: 0.0,
            heading_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
        };
        s.velocity = Velocity {
            speed_ms: 10.0,
            vertical_rate_ms: 0.0,
            course_deg: 0.0,
        };
        s.fuel = FuelStatus {
            remaining_kg: fuel_pct * 100.0,
            max_kg: 100.0,
            consumption_rate_kg_s: 0.1,
        };
        s.damage = damage;
        s
    }

    fn ctx<'a>(
        own: &'a PlatformState,
        caps: &'a PlatformCapabilities,
        now: f64,
    ) -> ServiceContext<'a> {
        ServiceContext {
            snapshot: None,
            own_platform: Some(own),
            fused_tracks: &[],
            autonomy: None,
            capabilities: caps,
            posture: CcaRole::Adaptive,
            now,
            own_platform_id: "self",
        }
    }

    #[test]
    fn healthy_platform_emits_nothing() {
        let mut svc = SurvivabilityService::default();
        let s = state(0.9, 0.0);
        let caps = caps();
        let out = svc.evaluate(&ctx(&s, &caps, 1.0));
        assert!(out.is_empty(), "healthy platform must not emit reflexes");
    }

    #[test]
    fn low_fuel_emits_rtb_high_priority() {
        let mut svc = SurvivabilityService::default();
        let s = state(0.10, 0.0);
        let caps = caps();
        let out = svc.evaluate(&ctx(&s, &caps, 1.0));
        assert_eq!(out.intents.len(), 1);
        assert_eq!(out.intents[0].priority, CommandPriority::High);
        assert!(matches!(
            out.intents[0].command,
            PlatformCommand::ReturnToBase { .. }
        ));
        assert!(out.audit_hints.iter().any(|h| h.event == "low_fuel_rtb"));
    }

    #[test]
    fn critical_damage_emits_slow_and_rtb_critical() {
        let mut svc = SurvivabilityService::default();
        let s = state(0.9, 0.85);
        let caps = caps();
        let out = svc.evaluate(&ctx(&s, &caps, 1.0));
        assert_eq!(out.intents.len(), 2, "expect SetSpeed + ReturnToBase");
        assert!(out
            .intents
            .iter()
            .all(|i| i.priority == CommandPriority::Critical));
        assert!(matches!(
            out.intents[0].command,
            PlatformCommand::SetSpeed { .. }
        ));
        assert!(matches!(
            out.intents[1].command,
            PlatformCommand::ReturnToBase { .. }
        ));
    }

    #[test]
    fn latches_prevent_re_emission_every_tick() {
        let mut svc = SurvivabilityService::default();
        let s = state(0.10, 0.0);
        let caps = caps();
        let first = svc.evaluate(&ctx(&s, &caps, 1.0));
        let second = svc.evaluate(&ctx(&s, &caps, 1.1));
        assert_eq!(first.intents.len(), 1);
        assert!(second.intents.is_empty(), "must not flood the queue");
    }

    #[test]
    fn recovery_resets_the_latch() {
        let mut svc = SurvivabilityService::default();
        let caps = caps();
        let low = state(0.10, 0.0);
        svc.evaluate(&ctx(&low, &caps, 1.0));
        let restored = state(0.9, 0.0);
        svc.evaluate(&ctx(&restored, &caps, 2.0));
        let drained = state(0.10, 0.0);
        let out = svc.evaluate(&ctx(&drained, &caps, 3.0));
        assert_eq!(out.intents.len(), 1, "latch must reset once fuel recovers");
    }

    #[test]
    fn critical_damage_suppresses_low_fuel_rtb_duplication() {
        let mut svc = SurvivabilityService::default();
        let s = state(0.10, 0.85);
        let caps = caps();
        let out = svc.evaluate(&ctx(&s, &caps, 1.0));
        let rtb_count = out
            .intents
            .iter()
            .filter(|i| matches!(i.command, PlatformCommand::ReturnToBase { .. }))
            .count();
        assert_eq!(
            rtb_count, 1,
            "must emit exactly one RTB even when both faults active"
        );
    }

    #[test]
    fn damage_advisory_emits_audit_only() {
        let mut svc = SurvivabilityService::default();
        let s = state(0.9, 0.40);
        let caps = caps();
        let out = svc.evaluate(&ctx(&s, &caps, 1.0));
        assert!(out.intents.is_empty(), "advisory must not produce intents");
        assert!(out.audit_hints.iter().any(|h| h.event == "damage_advisory"));
    }

    #[test]
    fn empty_snapshot_means_empty_output() {
        let mut svc = SurvivabilityService::default();
        let caps = caps();
        let ctx = ServiceContext {
            snapshot: None,
            own_platform: None,
            fused_tracks: &[],
            autonomy: None,
            capabilities: &caps,
            posture: CcaRole::Adaptive,
            now: 0.0,
            own_platform_id: "self",
        };
        let out = svc.evaluate(&ctx);
        assert!(out.is_empty());
    }

    #[test]
    fn service_id_is_pss() {
        let svc = SurvivabilityService::default();
        assert_eq!(svc.id(), CerebellumServiceId::Pss);
    }
}
