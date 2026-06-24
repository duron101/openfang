//! Maneuver Management Service (MMS) — deterministic route planning cerebellum.
//!
//! Converts semantic [`NavIntent`] + live snapshot into `FollowRoute`
//! [`CandidateIntent`]s. No LLM on the hot path.

use openfang_types::config::ManeuverConfig;
use openfang_types::platform::{
    Domain, PlatformCommand, PlatformState, TurnDirection, WorldSnapshot,
};
use openfang_types::route::{PlanGoal, RoutePlan, RoutePlanReason};
use openfang_types::tactical::{CandidateIntent, CommandPriority, IntentSource};
use openfang_types::umaa::Geofence;

use crate::cerebellum_services::{
    CerebellumService, CerebellumServiceId, ServiceAuditHint, ServiceContext, ServiceOutput,
};
use crate::flank_geometry::{flank_route, FlankRequest};
use crate::geo_zones::{extract_nav_intent, GeoZoneRegistry, NavIntent};
use crate::intent_extractor::FlankSide;
use crate::route_planner::{cpa_avoidance_maneuver, keepouts_from_geofences, plan_route};

/// MMS state: cached route + active navigation intent.
#[derive(Debug, Default)]
pub struct ManeuverManagementService {
    config: ManeuverConfig,
    zones: GeoZoneRegistry,
    geofences: Vec<Geofence>,
    nav_intent: Option<NavIntent>,
    /// Intent the current `active_plan` was built for (detects goal changes).
    planned_intent: Option<NavIntent>,
    active_plan: Option<RoutePlan>,
    /// The exact `DirectManeuver` order already emitted. A heading/speed order is
    /// one-shot in ArkSIM, so we emit once per distinct command and then hold —
    /// without this the slow loop re-issued `SetHeading` every tick (~15k times),
    /// flooding the simulator. Cleared whenever the nav intent changes.
    direct_emitted: Option<NavIntent>,
    link_degraded: bool,
}

impl ManeuverManagementService {
    pub fn new(config: ManeuverConfig, zones: GeoZoneRegistry, geofences: Vec<Geofence>) -> Self {
        Self {
            config,
            zones,
            geofences,
            nav_intent: None,
            planned_intent: None,
            active_plan: None,
            direct_emitted: None,
            link_degraded: false,
        }
    }

    pub fn set_link_degraded(&mut self, degraded: bool) {
        self.link_degraded = degraded;
    }

    pub fn set_nav_intent(&mut self, intent: Option<NavIntent>) {
        // A changed objective must re-emit its one-shot maneuver order.
        if self.nav_intent != intent {
            self.direct_emitted = None;
        }
        self.nav_intent = intent;
    }

    pub fn set_objective_text(&mut self, text: &str) -> bool {
        self.set_nav_intent(extract_nav_intent(text, &self.zones));
        self.nav_intent.is_some()
    }

    pub fn active_plan(&self) -> Option<&RoutePlan> {
        self.active_plan.as_ref()
    }

    pub fn zones(&self) -> &GeoZoneRegistry {
        &self.zones
    }

    fn kinematics(&self, state: &PlatformState) -> openfang_types::route::PlatformKinematics {
        let speed = state.velocity.speed_ms.max(1.0);
        let turn_r = if self.config.min_turn_radius_m > 0.0 {
            self.config.min_turn_radius_m
        } else {
            (speed * speed / 9.81).max(30.0)
        };
        openfang_types::route::PlatformKinematics {
            min_turn_radius_m: turn_r,
            max_climb_rate_ms: self.config.max_climb_rate_ms,
            speed_ms: speed,
            max_speed_ms: 30.0,
        }
    }

    fn cruise_alt(&self, state: &PlatformState) -> f64 {
        if state.domain == Domain::Air || state.domain == Domain::Subsurface {
            if self.config.cruise_alt_m > 0.0 {
                self.config.cruise_alt_m
            } else {
                state.pose.alt_m
            }
        } else {
            state.pose.alt_m
        }
    }

    /// Replan only on meaningful events — not every tick. Triggers: no/invalid
    /// plan, link degraded, navigation goal changed, stale plan (safety bound),
    /// or the platform has drifted off the planned track beyond the cross-track
    /// threshold. Holds the plan once near the final waypoint.
    fn needs_replan(&self, state: &PlatformState, now: f64) -> bool {
        let Some(plan) = &self.active_plan else {
            return true;
        };
        if !plan.feasible || self.link_degraded {
            return true;
        }
        // Goal/intent changed since the plan was built.
        if self.planned_intent != self.nav_intent {
            return true;
        }
        // Stale-plan safety bound (well above tick rate, not per-tick).
        if (now - plan.generated_at) > self.config.replan_interval_s.max(60.0) {
            return true;
        }
        // Arrived near the final waypoint → hold.
        if let Some(wp) = plan.waypoints.last() {
            let target = openfang_types::platform::Pose {
                lat_deg: wp.lat,
                lon_deg: wp.lon,
                alt_m: wp.alt.unwrap_or(state.pose.alt_m),
                heading_deg: 0.0,
                pitch_deg: 0.0,
                roll_deg: 0.0,
            };
            if state.pose.distance_m(&target) < self.config.arrival_radius_m.max(25.0) {
                return false;
            }
        }
        // Off-track: cross-track deviation from the planned polyline.
        let polyline: Vec<(f64, f64)> = plan.waypoints.iter().map(|w| (w.lat, w.lon)).collect();
        let xt = crate::route_geometry::cross_track_distance_m(
            state.pose.lat_deg,
            state.pose.lon_deg,
            &polyline,
        );
        xt > self.config.replan_cross_track_m.max(20.0)
    }

    fn build_plan(
        &mut self,
        state: &PlatformState,
        snapshot: &WorldSnapshot,
        now: f64,
    ) -> RoutePlan {
        let intent = match &self.nav_intent {
            Some(i) => i.clone(),
            None => {
                return RoutePlan::default();
            }
        };

        let cruise = self.cruise_alt(state);
        let keepouts = keepouts_from_geofences(&self.geofences, cruise);
        let kinematics = self.kinematics(state);

        if self.link_degraded {
            // Real circular orbit around the current position (safe hold while
            // the C2 link is down), not a one-shot point goal.
            let wps = crate::route_planner::loiter_orbit(
                state.pose.lat_deg,
                state.pose.lon_deg,
                200.0,
                cruise,
                kinematics.speed_ms,
                8,
            );
            return RoutePlan {
                legs: vec![],
                waypoints: wps,
                total_length_m: 0.0,
                generated_at: now,
                reason: RoutePlanReason::Degraded,
                feasible: true,
            };
        }

        if let NavIntent::Patrol { zone, .. } = &intent {
            let wps = self
                .zones
                .patrol_waypoints(zone, cruise, kinematics.speed_ms);
            if !wps.is_empty() {
                return RoutePlan {
                    legs: vec![],
                    waypoints: wps,
                    total_length_m: 0.0,
                    generated_at: now,
                    reason: RoutePlanReason::Initial,
                    feasible: true,
                };
            }
        }

        if let NavIntent::FlankStandoff {
            track_label,
            range_m,
            turn_direction,
        } = &intent
        {
            if let Some(track) = find_track(snapshot, track_label) {
                if let Some((lat, lon, alt)) = track.position_lla {
                    let waypoints = flank_route(&FlankRequest {
                        own: state.pose,
                        target_lat: lat,
                        target_lon: lon,
                        target_alt_m: Some(alt),
                        target_heading_deg: track.heading_deg,
                        standoff_m: *range_m,
                        side: flank_side_from_turn(*turn_direction),
                        speed_ms: Some(kinematics.speed_ms),
                    });
                    let total_length_m = route_length_m(state, &waypoints);
                    return RoutePlan {
                        legs: vec![],
                        waypoints,
                        total_length_m,
                        generated_at: now,
                        reason: RoutePlanReason::Initial,
                        feasible: true,
                    };
                }
            }
            return RoutePlan::default();
        }

        let Some(mut req) = self.zones.compile_plan_request(
            &intent,
            state.pose,
            kinematics,
            Some(cruise),
            keepouts,
            self.config.threat_avoid_weight,
        ) else {
            return RoutePlan::default();
        };

        // Standoff: resolve track position from snapshot.
        if let PlanGoal::Standoff { track_id, range_m } = &mut req.goal {
            if let Some(track) = find_track(snapshot, track_id) {
                if let Some((lat, lon, _)) = track.position_lla {
                    let bearing = state.pose.bearing_to(&openfang_types::platform::Pose {
                        lat_deg: lat,
                        lon_deg: lon,
                        alt_m: cruise,
                        heading_deg: 0.0,
                        pitch_deg: 0.0,
                        roll_deg: 0.0,
                    });
                    let (glat, glon) = crate::route_geometry::destination(
                        lat,
                        lon,
                        (bearing + 180.0) % 360.0,
                        *range_m,
                    );
                    req.goal = PlanGoal::Point {
                        lat_deg: glat,
                        lon_deg: glon,
                        alt_m: cruise,
                    };
                }
            }
        }

        plan_route(&req, now, RoutePlanReason::Initial)
    }

    fn cpa_intents(
        &self,
        state: &PlatformState,
        snapshot: &WorldSnapshot,
        now: f64,
        platform_id: &str,
    ) -> Vec<CandidateIntent> {
        let mut out = Vec::new();
        for platform in &snapshot.platforms {
            if platform.id == platform_id {
                continue;
            }
            if !platform.affiliation.is_hostile() {
                continue;
            }
            if let Some(maneuver) = cpa_avoidance_maneuver(
                &state.pose,
                state.velocity.speed_ms,
                state.velocity.course_deg,
                platform.pose.lat_deg,
                platform.pose.lon_deg,
                platform.velocity.speed_ms,
                platform.velocity.course_deg,
                self.config.cpa_min_m,
                self.config.cpa_max_tcpa_s,
            ) {
                out.push(CandidateIntent::new(
                    PlatformCommand::SetHeading {
                        platform_id: platform_id.to_string(),
                        heading_deg: maneuver.heading_deg,
                        speed_ms: maneuver.speed_ms.or(Some(state.velocity.speed_ms * 0.8)),
                        turn_direction: None,
                    },
                    CommandPriority::Critical,
                    IntentSource::Dcc {
                        rule_name: format!("mms:cpa:{}", maneuver.encounter.label()),
                    },
                    now,
                    format!(
                        "mms COLREGs {} — {}",
                        maneuver.encounter.label(),
                        maneuver.reason
                    ),
                ));
            }
        }
        out
    }

    fn direct_maneuver_intents(
        &self,
        state: &PlatformState,
        intent: &NavIntent,
        now: f64,
        platform_id: &str,
    ) -> Vec<CandidateIntent> {
        let NavIntent::DirectManeuver {
            heading_deg,
            heading_delta_deg,
            turn_direction,
            speed_ms,
        } = intent
        else {
            return Vec::new();
        };

        let mut out = Vec::new();
        if let Some(speed) = speed_ms {
            out.push(self.intent(
                PlatformCommand::SetSpeed {
                    platform_id: platform_id.to_string(),
                    speed_ms: *speed,
                    acceleration_ms2: None,
                },
                CommandPriority::Normal,
                now,
                format!("mms direct speed {speed:.1}m/s"),
            ));
        }
        let resolved_heading = heading_deg.or_else(|| {
            heading_delta_deg.map(|delta| normalize_heading_deg(state.pose.heading_deg + delta))
        });
        if let Some(heading) = resolved_heading {
            out.push(self.intent(
                PlatformCommand::SetHeading {
                    platform_id: platform_id.to_string(),
                    heading_deg: heading,
                    speed_ms: *speed_ms,
                    turn_direction: *turn_direction,
                },
                CommandPriority::Normal,
                now,
                format!("mms direct heading {heading:.1}deg"),
            ));
        }
        out
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
                rule_name: format!("mms:{}", CerebellumServiceId::Mms.label()),
            },
            now,
            reason,
        )
    }
}

fn find_track<'a>(
    snapshot: &'a WorldSnapshot,
    track_id: &str,
) -> Option<&'a openfang_types::platform::Track> {
    for platform in &snapshot.platforms {
        for track in &platform.tracks {
            if track.track_id == track_id || track.target_name == track_id {
                return Some(track);
            }
        }
    }
    None
}

fn flank_side_from_turn(turn: Option<TurnDirection>) -> Option<FlankSide> {
    match turn {
        Some(TurnDirection::Left) => Some(FlankSide::Left),
        Some(TurnDirection::Right) => Some(FlankSide::Right),
        Some(TurnDirection::Shortest) | None => None,
    }
}

fn route_length_m(state: &PlatformState, waypoints: &[openfang_types::platform::Waypoint]) -> f64 {
    let mut cursor = state.pose;
    let mut total = 0.0;
    for wp in waypoints {
        let next = openfang_types::platform::Pose {
            lat_deg: wp.lat,
            lon_deg: wp.lon,
            alt_m: wp.alt.unwrap_or(cursor.alt_m),
            heading_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
        };
        total += cursor.distance_m(&next);
        cursor = next;
    }
    total
}

fn normalize_heading_deg(deg: f64) -> f64 {
    let mut h = deg % 360.0;
    if h < 0.0 {
        h += 360.0;
    }
    h
}

impl CerebellumService for ManeuverManagementService {
    fn id(&self) -> CerebellumServiceId {
        CerebellumServiceId::Mms
    }

    fn evaluate(&mut self, ctx: &ServiceContext<'_>) -> ServiceOutput {
        if !self.config.enabled {
            return ServiceOutput::empty();
        }
        let Some(state) = ctx.own_platform else {
            return ServiceOutput::empty();
        };
        let Some(snapshot) = ctx.snapshot else {
            return ServiceOutput::empty();
        };

        let mut out = ServiceOutput::empty();

        // CPA reflexes (Critical) — parallel to route following.
        for intent in self.cpa_intents(state, snapshot, ctx.now, ctx.own_platform_id) {
            out.intents.push(intent);
        }

        if let Some(intent) = self.nav_intent.clone() {
            if matches!(intent, NavIntent::DirectManeuver { .. }) {
                self.active_plan = None;
                self.planned_intent = Some(intent.clone());
                // One-shot: re-issue only when the order itself changed. A
                // heading/speed order persists in ArkSIM, so re-emitting every
                // tick is both pointless and a flood vector.
                if self.direct_emitted.as_ref() == Some(&intent) {
                    return out;
                }
                self.direct_emitted = Some(intent.clone());
                out.intents.extend(self.direct_maneuver_intents(
                    state,
                    &intent,
                    ctx.now,
                    ctx.own_platform_id,
                ));
                return out;
            }
        }

        if self.nav_intent.is_none() {
            return out;
        }

        if self.needs_replan(state, ctx.now) {
            let plan = self.build_plan(state, snapshot, ctx.now);
            if plan.feasible && !plan.waypoints.is_empty() {
                self.active_plan = Some(plan);
                self.planned_intent = self.nav_intent.clone();
                out.audit_hints.push(
                    ServiceAuditHint::new(CerebellumServiceId::Mms, "route_planned").with_detail(
                        format!(
                            "waypoints={}",
                            self.active_plan
                                .as_ref()
                                .map(|p| p.waypoints.len())
                                .unwrap_or(0)
                        ),
                    ),
                );
            }
        }

        let Some(plan) = &self.active_plan else {
            return out;
        };
        if plan.waypoints.is_empty() {
            return out;
        }

        out.intents.push(self.intent(
            PlatformCommand::FollowRoute {
                platform_id: ctx.own_platform_id.to_string(),
                waypoints: plan.waypoints.clone(),
            },
            CommandPriority::Normal,
            ctx.now,
            format!(
                "mms route {:?} len={:.0}m",
                plan.reason, plan.total_length_m
            ),
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::config::GeoZoneConfig;
    use openfang_types::platform::{
        Affiliation, CcaRole, FuelStatus, PlatformCapabilities, PlatformState, Pose, Velocity,
        WorldSnapshot,
    };
    use openfang_types::umaa::GeofenceType;

    fn ctx<'a>(
        snap: &'a WorldSnapshot,
        state: &'a PlatformState,
        caps: &'a PlatformCapabilities,
    ) -> ServiceContext<'a> {
        ServiceContext {
            snapshot: Some(snap),
            own_platform: Some(state),
            fused_tracks: &[],
            autonomy: None,
            capabilities: caps,
            posture: CcaRole::Adaptive,
            now: 1.0,
            own_platform_id: "usv-01",
        }
    }

    fn caps() -> PlatformCapabilities {
        PlatformCapabilities {
            supports_motion_control: true,
            ..Default::default()
        }
    }

    fn state() -> PlatformState {
        let mut s = PlatformState::minimal("usv-01");
        s.pose = Pose {
            lat_deg: 30.0,
            lon_deg: 120.0,
            alt_m: 0.0,
            heading_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
        };
        s.velocity = Velocity {
            speed_ms: 8.0,
            vertical_rate_ms: 0.0,
            course_deg: 0.0,
        };
        s
    }

    #[test]
    fn mms_emits_follow_route_for_goto_zone() {
        let zones = GeoZoneRegistry::from_config(&[GeoZoneConfig {
            id: "sector_north".into(),
            kind: "area".into(),
            aliases: vec!["north".into()],
            polygon: vec![
                (30.05, 120.05),
                (30.05, 120.06),
                (30.06, 120.06),
                (30.06, 120.05),
            ],
            point: Some((30.055, 120.055)),
            alt_band_m: [0.0, 500.0],
            patrol_pattern: None,
        }]);
        let mut mms = ManeuverManagementService::new(ManeuverConfig::default(), zones, vec![]);
        mms.set_nav_intent(Some(NavIntent::GotoZone {
            zone: "sector_north".into(),
        }));
        let snap = WorldSnapshot {
            timestamp: 1.0,
            platforms: vec![state()],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        };
        let st = state();
        let out = mms.evaluate(&ctx(&snap, &st, &caps()));
        assert!(
            out.intents
                .iter()
                .any(|i| matches!(i.command, PlatformCommand::FollowRoute { .. })),
            "expected FollowRoute intent"
        );
    }
}
