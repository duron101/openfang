//! EWMS — Electronic-Warfare Management Service.
//!
//! A first-class [`CerebellumService`] (previously the EW lane lived only as
//! [`crate::direct_channel`] trigger rules). It reasons over the **unified
//! fusion picture** (`ServiceContext::fused_tracks`) rather than raw per-sensor
//! returns: a Kalman-confirmed High/Critical contact drives a defensive jam
//! cue against the platform's jammers.
//!
//! Like every cerebellum producer it only ever emits [`CandidateIntent`]s and
//! audit hints — the ACS composer and SPGS gate downstream remain the sole
//! arbiters of what actually reaches an adapter, so this lane can run in
//! parallel with the legacy DCC reflex rules without bypassing safety.
//!
//! Invariants:
//! - **Idempotent**: a `JamStart` is emitted only while *no* jammer is already
//!   radiating, so the reflex never re-fires tick after tick.
//! - **Capability-aware**: a platform with no jammers produces nothing.
//! - **Fusion-gated**: an empty `fused_tracks` slice (isolated test / first
//!   tick) yields an empty output.

use openfang_types::platform::{JammerState, PlatformCommand, PlatformState};
use openfang_types::tactical::{CandidateIntent, CommandPriority, IntentSource};

use crate::cerebellum_services::{
    CerebellumService, CerebellumServiceId, ServiceAuditHint, ServiceContext, ServiceOutput,
};
use crate::sensor_fusion::{FusedTrack, ThreatLevel};

/// Minimum fused threat level that justifies a defensive jam cue.
const JAM_THREAT_FLOOR: ThreatLevel = ThreatLevel::High;
/// Default jam emission parameters (a real deployment overrides per-jammer).
const DEFAULT_JAM_FREQUENCY_HZ: f64 = 9_400_000_000.0;
const DEFAULT_JAM_BANDWIDTH_HZ: f64 = 50_000_000.0;

#[derive(Debug, Default)]
pub struct ElectronicWarfareManagementService;

impl ElectronicWarfareManagementService {
    pub fn new() -> Self {
        Self
    }
}

impl CerebellumService for ElectronicWarfareManagementService {
    fn id(&self) -> CerebellumServiceId {
        CerebellumServiceId::Ewms
    }

    fn evaluate(&mut self, ctx: &ServiceContext<'_>) -> ServiceOutput {
        let Some(state) = ctx.own_platform else {
            return ServiceOutput::empty();
        };
        if state.onboard_jammers.is_empty() {
            return ServiceOutput::empty();
        }

        // Highest-threat fused contact at/above the jam floor drives the cue.
        let Some(target) = highest_threat(ctx.fused_tracks) else {
            return ServiceOutput::empty();
        };

        let mut out = ServiceOutput::empty();

        // Idempotent: if any jammer is already radiating, the cue is satisfied —
        // emit nothing further and just leave an audit breadcrumb.
        if state.onboard_jammers.iter().any(|j| j.is_active) {
            out.audit_hints.push(
                ServiceAuditHint::new(CerebellumServiceId::Ewms, "ewms_jam_active")
                    .with_detail(format!(
                        "target={} threat={:?} jammers_active",
                        target.track_id, target.threat_level
                    )),
            );
            return out;
        }

        let Some(jammer) = idle_jammer(&state.onboard_jammers) else {
            return out;
        };

        out.intents.push(CandidateIntent::new(
            PlatformCommand::JamStart {
                platform_id: state.id.clone(),
                jammer_id: jammer.jammer_id.clone(),
                frequency_hz: DEFAULT_JAM_FREQUENCY_HZ,
                bandwidth_hz: DEFAULT_JAM_BANDWIDTH_HZ,
                target_track_id: target.track_id.clone(),
            },
            CommandPriority::High,
            IntentSource::Dcc {
                rule_name: format!("ewms:{}", CerebellumServiceId::Ewms.label()),
            },
            ctx.now,
            format!(
                "ewms fused {:?} threat → jam {} at {}",
                target.threat_level, jammer.jammer_id, target.track_id
            ),
        ));
        out.audit_hints.push(
            ServiceAuditHint::new(CerebellumServiceId::Ewms, "ewms_jam_start").with_detail(
                format!(
                    "jammer={} target={} threat={:?}",
                    jammer.jammer_id, target.track_id, target.threat_level
                ),
            ),
        );
        out
    }
}

fn highest_threat(tracks: &[FusedTrack]) -> Option<&FusedTrack> {
    tracks
        .iter()
        .filter(|t| t.threat_level >= JAM_THREAT_FLOOR)
        .max_by_key(|t| t.threat_level)
}

fn idle_jammer(jammers: &[JammerState]) -> Option<&JammerState> {
    jammers.iter().find(|j| !j.is_active)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::config::AutonomyConfig;
    use openfang_types::platform::{Affiliation, CcaRole, PlatformCapabilities};

    fn caps() -> PlatformCapabilities {
        PlatformCapabilities {
            supports_jammer_control: true,
            ..Default::default()
        }
    }

    fn fused(track_id: &str, level: ThreatLevel) -> FusedTrack {
        FusedTrack {
            track_id: track_id.into(),
            classification: "fighter".into(),
            affiliation: Affiliation::Red,
            position: (30.0, 120.0, 0.0),
            velocity: (0.0, 0.0, 0.0),
            heading_deg: 0.0,
            speed_ms: 0.0,
            quality: 0.9,
            threat_level: level,
            last_update_s: 1.0,
            age_s: 0.0,
            update_count: 3,
            position_variance: 5.0,
        }
    }

    fn jammer(id: &str, active: bool) -> JammerState {
        JammerState {
            jammer_id: id.into(),
            host_id: "self".into(),
            is_active: active,
            beams: vec![],
        }
    }

    fn run(state: &PlatformState, fused_tracks: &[FusedTrack]) -> ServiceOutput {
        let caps = caps();
        let cfg = AutonomyConfig::default();
        let active = cfg.active();
        let ctx = ServiceContext {
            snapshot: None,
            own_platform: Some(state),
            fused_tracks,
            autonomy: Some(&active),
            capabilities: &caps,
            posture: CcaRole::Adaptive,
            now: 1.0,
            own_platform_id: "self",
        };
        ElectronicWarfareManagementService::new().evaluate(&ctx)
    }

    #[test]
    fn no_jammer_means_no_output() {
        let state = PlatformState::minimal("self");
        let out = run(&state, &[fused("trk-1", ThreatLevel::Critical)]);
        assert!(out.is_empty());
    }

    #[test]
    fn high_fused_threat_jams_idle_jammer() {
        let mut state = PlatformState::minimal("self");
        state.onboard_jammers = vec![jammer("ecm-1", false)];
        let out = run(&state, &[fused("trk-1", ThreatLevel::High)]);
        assert_eq!(out.intents.len(), 1);
        assert!(matches!(
            out.intents[0].command,
            PlatformCommand::JamStart { ref target_track_id, .. } if target_track_id == "trk-1"
        ));
    }

    #[test]
    fn low_fused_threat_does_not_jam() {
        let mut state = PlatformState::minimal("self");
        state.onboard_jammers = vec![jammer("ecm-1", false)];
        let out = run(&state, &[fused("trk-1", ThreatLevel::Low)]);
        assert!(out.intents.is_empty());
    }

    #[test]
    fn active_jammer_is_idempotent_no_new_intent() {
        let mut state = PlatformState::minimal("self");
        state.onboard_jammers = vec![jammer("ecm-1", true)];
        let out = run(&state, &[fused("trk-1", ThreatLevel::Critical)]);
        assert!(out.intents.is_empty());
        assert!(!out.audit_hints.is_empty());
    }
}
