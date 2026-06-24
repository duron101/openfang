//! Federation primitives (M4-U6) — deterministic, allocation-light, no-LLM.
//!
//! This module provides the **brain-side** federation engine that turns a
//! fleet snapshot + link quality + autonomy config into three load-bearing
//! decisions:
//!
//! 1. [`select_leader`] — picks the highest-priority *healthy* member as
//!    leader using a static priority order. No election, no quorum, no
//!    randomness: same inputs ⇒ same leader, byte-for-byte.
//! 2. [`resolve_active_profile`] — when the link to the leader is `Poor`
//!    or `Lost`, returns the configured `degraded_profile` instead of the
//!    operator profile. Member self-defense reflexes stay alive while
//!    auto-engagement is suppressed (it remains profile-gated downstream).
//! 3. [`filter_stale_commands`] — drops any queued dangerous command
//!    (`FireAtTarget`, `AssignMission`) older than the configured staleness
//!    window. Replaying a fire order from before a blackout is the exact
//!    failure mode the plan calls out.
//!
//! All three are *pure functions*. The kernel calls them on the slow path
//! to refresh a [`FederationStatus`] that the API and dashboard consume.

use openfang_types::config::{AutonomyConfig, AutonomyModeProfile, FederationConfig};
use openfang_types::platform::{FleetSnapshot, LinkQuality, PlatformCommand};
use openfang_types::tactical::CandidateIntent;
use serde::Serialize;

/// Inputs the federation engine evaluates each refresh. Kept small so the
/// hot path doesn't allocate.
#[derive(Debug, Clone)]
pub struct FederationInputs<'a> {
    /// This node's stable platform id (e.g. `KernelConfig::own_platform_id`).
    pub local_id: &'a str,
    /// Latest fleet picture published by the cerebellum's CMS lane.
    pub fleet: &'a FleetSnapshot,
    /// CMS-derived link quality bucket (canonical `LinkQuality`).
    pub link_quality: LinkQuality,
    /// Wall-clock seconds, used to age queued dangerous commands.
    pub now_secs: f64,
}

/// Outcome of the federation refresh. Stable, serializable; safe to expose
/// to the dashboard and audit log.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct FederationStatus {
    /// Local id that produced this status.
    pub local_id: String,
    /// Currently elected leader id. Empty when no healthy member exists.
    pub leader_id: String,
    /// `true` iff `leader_id == local_id`.
    pub is_leader: bool,
    /// Next failover designate (highest-priority healthy member that is *not*
    /// the current leader). Empty when no fallback is available.
    pub failover_designate: String,
    /// The link-quality bucket that produced this status, snake_case
    /// (`excellent` / `good` / `marginal` / `poor` / `lost`).
    pub link_quality: String,
    /// Effective autonomy profile id after link-driven degradation. Equal to
    /// the operator-configured profile under healthy links; equal to
    /// `degraded_profile` under `Poor`/`Lost`.
    pub effective_profile: String,
    /// Operator-configured profile id (pre-degradation).
    pub configured_profile: String,
    /// `true` iff degradation rewrote the active profile.
    pub degraded: bool,
    /// Reason the federation engine produced this outcome — for audit and
    /// dashboard tooltips. Always non-empty.
    pub reason: String,
}

/// Pick the highest-priority healthy member as leader.
///
/// "Healthy" = present in the fleet snapshot AND in contact (
/// [`UavState::is_in_contact`]). When the configured `priority_order` is
/// empty or no healthy member is present, returns the local id unchanged
/// (`is_leader = true`) — a stranded single member is its own leader.
pub fn select_leader(inputs: &FederationInputs<'_>, fed: &FederationConfig) -> (String, String) {
    let priority = &fed.priority_order;

    // Build the set of healthy members keyed by id. A member with no entry
    // in the fleet picture is treated as unknown / unhealthy.
    let healthy = |id: &str| -> bool {
        // The local node is healthy by construction (it's the one evaluating).
        if id == inputs.local_id {
            return true;
        }
        inputs
            .fleet
            .uavs
            .iter()
            .any(|u| u.uav_id == id && u.is_in_contact())
    };

    let leader = priority
        .iter()
        .find(|id| healthy(id))
        .cloned()
        .unwrap_or_else(|| inputs.local_id.to_string());

    let failover = priority
        .iter()
        .find(|id| id.as_str() != leader && healthy(id))
        .cloned()
        .unwrap_or_default();

    (leader, failover)
}

/// Resolve the effective autonomy profile after link-driven degradation.
///
/// Returns the operator-configured profile under healthy links; switches to
/// `degraded_profile` when CMS reports `Poor`/`Lost`. Falls back to the
/// configured profile if `degraded_profile` is unset or unknown — never
/// returns a permissive profile.
pub fn resolve_active_profile(
    config: &AutonomyConfig,
    link_quality: LinkQuality,
) -> (AutonomyModeProfile, bool, &str) {
    let configured = config.active();
    if !link_quality.should_force_defensive() {
        return (configured, false, "");
    }

    let Some(degraded_id) = config.degraded_profile.as_ref().filter(|s| !s.is_empty()) else {
        return (configured, false, "no degraded_profile configured");
    };

    match config.profile(degraded_id) {
        Some(p) => (p.clone(), true, "link degraded → degraded profile"),
        None => (
            configured,
            false,
            "degraded_profile id missing from profiles table",
        ),
    }
}

/// Issued-at age in seconds for a `CandidateIntent`. Inline-pure to keep
/// the slow path branchless.
fn age_secs(intent: &CandidateIntent, now_secs: f64) -> f64 {
    (now_secs - intent.issued_at).max(0.0)
}

/// `true` iff the command is "dangerous" in the federation sense — replaying
/// it after a blackout could cause an unsafe physical action.
fn is_dangerous(command: &PlatformCommand) -> bool {
    matches!(
        command,
        PlatformCommand::FireAtTarget { .. } | PlatformCommand::AssignMission { .. }
    )
}

/// Drop dangerous queued intents older than the configured staleness window.
/// Returns `(kept, dropped)`; the dropped list is useful for audit.
///
/// Non-dangerous classes (motion/sensor/comm/EW/survivability) are always
/// kept regardless of age — the federation engine never silently drops
/// non-weapon intents.
pub fn filter_stale_commands(
    intents: Vec<CandidateIntent>,
    fed: &FederationConfig,
    now_secs: f64,
) -> (Vec<CandidateIntent>, Vec<CandidateIntent>) {
    filter_stale_by_window(intents, fed.effective_stale_window_s(), now_secs)
}

/// Window-level variant of [`filter_stale_commands`] used on the control-loop
/// hot path, where the loop stores only the staleness window (`f64`) and must
/// not allocate a [`FederationConfig`] every tick. Same semantics: dangerous
/// classes older than `window_secs` are dropped, everything else is kept.
pub fn filter_stale_by_window(
    intents: Vec<CandidateIntent>,
    window_secs: f64,
    now_secs: f64,
) -> (Vec<CandidateIntent>, Vec<CandidateIntent>) {
    let mut kept = Vec::with_capacity(intents.len());
    let mut dropped = Vec::new();
    for intent in intents {
        if is_dangerous(&intent.command) && age_secs(&intent, now_secs) > window_secs {
            dropped.push(intent);
        } else {
            kept.push(intent);
        }
    }
    (kept, dropped)
}

/// Refresh the federation status in one shot — convenience wrapper around
/// [`select_leader`] + [`resolve_active_profile`]. The kernel publishes the
/// result for the API and audit log to read.
pub fn refresh_status(
    inputs: &FederationInputs<'_>,
    fed: &FederationConfig,
    autonomy: &AutonomyConfig,
) -> FederationStatus {
    let (leader_id, failover_designate) = select_leader(inputs, fed);
    let is_leader = leader_id == inputs.local_id;
    let (profile, degraded, reason) = resolve_active_profile(autonomy, inputs.link_quality);

    let configured_profile = autonomy.active().id;
    let reason_str = if degraded {
        reason.to_string()
    } else if is_leader {
        "leader: link healthy".to_string()
    } else if !reason.is_empty() {
        reason.to_string()
    } else {
        "member: link healthy".to_string()
    };

    FederationStatus {
        local_id: inputs.local_id.to_string(),
        leader_id,
        is_leader,
        failover_designate,
        link_quality: inputs.link_quality.as_str().to_string(),
        effective_profile: profile.id,
        configured_profile,
        degraded,
        reason: reason_str,
    }
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::config::{AutonomyConfig, AutonomyModeProfile, FederationConfig};
    use openfang_types::platform::{
        FleetSnapshot, LinkQuality, PlatformCommand, UavState, UavStatus,
    };
    use openfang_types::tactical::{CandidateIntent, CommandPriority, IntentSource};

    fn uav(id: &str, in_contact: bool) -> UavState {
        UavState {
            uav_id: id.into(),
            uav_type: "cca".into(),
            status: UavStatus::Airborne,
            fuel_pct: 0.9,
            seconds_since_contact: if in_contact { 1.0 } else { 999.0 },
            mission: None,
        }
    }

    fn fleet(members: &[(&str, bool)]) -> FleetSnapshot {
        let mut snap = FleetSnapshot::new("leader-1");
        for (id, in_contact) in members {
            snap.uavs.push(uav(id, *in_contact));
        }
        snap
    }

    fn fed(order: &[&str]) -> FederationConfig {
        FederationConfig {
            priority_order: order.iter().map(|s| s.to_string()).collect(),
            member_id: String::new(),
            stale_command_window_s: 5.0,
        }
    }

    fn autonomy_with_degraded() -> AutonomyConfig {
        AutonomyConfig {
            active_profile: "supervised_autonomy".into(),
            profiles: vec![
                AutonomyModeProfile {
                    id: "supervised_autonomy".into(),
                    ..AutonomyModeProfile::default()
                },
                AutonomyModeProfile {
                    id: "defensive_autonomy".into(),
                    ..AutonomyModeProfile::default()
                },
            ],
            degraded_profile: Some("defensive_autonomy".into()),
        }
    }

    fn make_intent(command: PlatformCommand, issued_at: f64) -> CandidateIntent {
        CandidateIntent::new(
            command,
            CommandPriority::Normal,
            IntentSource::External {
                label: "fed-test".into(),
            },
            issued_at,
            "federation staleness test",
        )
    }

    // ── select_leader ──

    #[test]
    fn select_leader_picks_highest_priority_healthy() {
        let snap = fleet(&[("alpha", true), ("bravo", true)]);
        let cfg = fed(&["alpha", "bravo"]);
        let inputs = FederationInputs {
            local_id: "alpha",
            fleet: &snap,
            link_quality: LinkQuality::Excellent,
            now_secs: 100.0,
        };
        let (leader, failover) = select_leader(&inputs, &cfg);
        assert_eq!(leader, "alpha");
        assert_eq!(failover, "bravo");
    }

    #[test]
    fn select_leader_skips_unhealthy_member() {
        let snap = fleet(&[("alpha", false), ("bravo", true)]);
        let cfg = fed(&["alpha", "bravo"]);
        let inputs = FederationInputs {
            local_id: "bravo",
            fleet: &snap,
            link_quality: LinkQuality::Good,
            now_secs: 100.0,
        };
        // alpha not in contact AND not local, so bravo (local + healthy) wins.
        let (leader, failover) = select_leader(&inputs, &cfg);
        assert_eq!(leader, "bravo");
        assert_eq!(failover, "");
    }

    #[test]
    fn select_leader_local_node_is_self_when_priority_empty() {
        let snap = fleet(&[("bravo", true)]);
        let cfg = fed(&[]);
        let inputs = FederationInputs {
            local_id: "alpha",
            fleet: &snap,
            link_quality: LinkQuality::Excellent,
            now_secs: 100.0,
        };
        let (leader, failover) = select_leader(&inputs, &cfg);
        assert_eq!(leader, "alpha");
        assert_eq!(failover, "");
    }

    #[test]
    fn select_leader_is_deterministic() {
        // Same inputs → identical outputs across many calls.
        let snap = fleet(&[("alpha", true), ("bravo", true), ("charlie", true)]);
        let cfg = fed(&["charlie", "alpha", "bravo"]);
        let inputs = FederationInputs {
            local_id: "bravo",
            fleet: &snap,
            link_quality: LinkQuality::Excellent,
            now_secs: 100.0,
        };
        let first = select_leader(&inputs, &cfg);
        for _ in 0..50 {
            assert_eq!(select_leader(&inputs, &cfg), first);
        }
        assert_eq!(first, ("charlie".into(), "alpha".into()));
    }

    // ── resolve_active_profile ──

    #[test]
    fn healthy_link_keeps_operator_profile() {
        let cfg = autonomy_with_degraded();
        let (profile, degraded, _) = resolve_active_profile(&cfg, LinkQuality::Good);
        assert_eq!(profile.id, "supervised_autonomy");
        assert!(!degraded);
    }

    #[test]
    fn poor_link_switches_to_degraded_profile() {
        let cfg = autonomy_with_degraded();
        let (profile, degraded, reason) = resolve_active_profile(&cfg, LinkQuality::Poor);
        assert_eq!(profile.id, "defensive_autonomy");
        assert!(degraded);
        assert!(reason.contains("degraded"));
    }

    #[test]
    fn lost_link_switches_to_degraded_profile() {
        let cfg = autonomy_with_degraded();
        let (profile, degraded, _) = resolve_active_profile(&cfg, LinkQuality::Lost);
        assert_eq!(profile.id, "defensive_autonomy");
        assert!(degraded);
    }

    #[test]
    fn missing_degraded_profile_stays_configured() {
        let mut cfg = autonomy_with_degraded();
        cfg.degraded_profile = None;
        let (profile, degraded, _) = resolve_active_profile(&cfg, LinkQuality::Lost);
        // Without a configured fallback we keep the operator profile rather
        // than silently relax to permissive default.
        assert_eq!(profile.id, "supervised_autonomy");
        assert!(!degraded);
    }

    #[test]
    fn unknown_degraded_profile_stays_configured() {
        let mut cfg = autonomy_with_degraded();
        cfg.degraded_profile = Some("ghost_profile".into());
        let (profile, degraded, _) = resolve_active_profile(&cfg, LinkQuality::Lost);
        assert_eq!(profile.id, "supervised_autonomy");
        assert!(!degraded);
    }

    // ── filter_stale_commands ──

    #[test]
    fn stale_fire_intent_is_dropped() {
        let cfg = fed(&[]);
        let stale = make_intent(
            PlatformCommand::FireAtTarget {
                platform_id: "p1".into(),
                weapon_id: "missile-1".into(),
                track_id: "trk-1".into(),
            },
            10.0,
        );
        let fresh = make_intent(
            PlatformCommand::FireAtTarget {
                platform_id: "p1".into(),
                weapon_id: "missile-2".into(),
                track_id: "trk-2".into(),
            },
            58.0,
        );
        let (kept, dropped) = filter_stale_commands(vec![stale, fresh], &cfg, 60.0);
        assert_eq!(kept.len(), 1);
        assert_eq!(dropped.len(), 1);
        let kept_weapon = match &kept[0].command {
            PlatformCommand::FireAtTarget { weapon_id, .. } => weapon_id.clone(),
            _ => unreachable!(),
        };
        assert_eq!(kept_weapon, "missile-2");
    }

    #[test]
    fn stale_assign_mission_is_dropped() {
        let cfg = fed(&[]);
        let stale_assign = make_intent(
            PlatformCommand::AssignMission {
                uav_id: "alpha".into(),
                mission_type: "strike".into(),
                params_json: "{}".into(),
            },
            10.0,
        );
        let (kept, dropped) = filter_stale_commands(vec![stale_assign], &cfg, 100.0);
        assert!(kept.is_empty());
        assert_eq!(dropped.len(), 1);
    }

    #[test]
    fn stale_non_dangerous_intent_is_kept() {
        let cfg = fed(&[]);
        // Old motion intent stays — only dangerous classes are filtered.
        let stale_motion = make_intent(
            PlatformCommand::SetHeading {
                platform_id: "p1".into(),
                heading_deg: 180.0,
                speed_ms: None,
                turn_direction: None,
            },
            0.0,
        );
        let (kept, dropped) = filter_stale_commands(vec![stale_motion], &cfg, 1000.0);
        assert_eq!(kept.len(), 1);
        assert!(dropped.is_empty());
    }

    #[test]
    fn fresh_fire_intent_inside_window_is_kept() {
        let cfg = fed(&[]);
        let fresh = make_intent(
            PlatformCommand::FireAtTarget {
                platform_id: "p1".into(),
                weapon_id: "missile-1".into(),
                track_id: "trk-1".into(),
            },
            58.0,
        );
        let (kept, _) = filter_stale_commands(vec![fresh], &cfg, 60.0);
        assert_eq!(kept.len(), 1);
    }

    // ── refresh_status integration ──

    #[test]
    fn refresh_status_member_under_link_loss_degrades() {
        let snap = fleet(&[("leader", false), ("self", true)]);
        let cfg_fed = fed(&["leader", "self"]);
        let cfg_aut = autonomy_with_degraded();
        let inputs = FederationInputs {
            local_id: "self",
            fleet: &snap,
            link_quality: LinkQuality::Lost,
            now_secs: 100.0,
        };
        let status = refresh_status(&inputs, &cfg_fed, &cfg_aut);
        assert!(status.degraded);
        assert_eq!(status.effective_profile, "defensive_autonomy");
        assert_eq!(status.configured_profile, "supervised_autonomy");
        // With leader offline, self becomes the local fallback leader.
        assert_eq!(status.leader_id, "self");
        assert!(status.is_leader);
    }

    #[test]
    fn refresh_status_member_healthy_link_stays_configured() {
        let snap = fleet(&[("leader", true), ("self", true)]);
        let cfg_fed = fed(&["leader", "self"]);
        let cfg_aut = autonomy_with_degraded();
        let inputs = FederationInputs {
            local_id: "self",
            fleet: &snap,
            link_quality: LinkQuality::Excellent,
            now_secs: 100.0,
        };
        let status = refresh_status(&inputs, &cfg_fed, &cfg_aut);
        assert!(!status.degraded);
        assert_eq!(status.effective_profile, "supervised_autonomy");
        assert_eq!(status.leader_id, "leader");
        assert_eq!(status.failover_designate, "self");
        assert!(!status.is_leader);
    }
}
