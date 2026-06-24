//! Route-planning domain types (MMS cerebellum).
//!
//! These types describe deterministic path plans produced by the Maneuver
//! Management Service. They are serializable for API/dashboard reporting and
//! contain no LLM or network state.

use serde::{Deserialize, Serialize};

use crate::platform::{Pose, Waypoint};

/// Why the MMS (re)planned a route this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutePlanReason {
    #[default]
    Initial,
    KeepOutReroute,
    CpaAvoid,
    Replan,
    Degraded,
}

/// Dubins arc segment metadata attached to a [`RouteLeg`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ArcSegment {
    pub center_lat_deg: f64,
    pub center_lon_deg: f64,
    pub radius_m: f64,
    pub start_bearing_deg: f64,
    pub sweep_deg: f64,
}

/// One leg of a planned route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteLeg {
    pub from: Waypoint,
    pub to: Waypoint,
    pub heading_deg: f64,
    pub length_m: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arc: Option<ArcSegment>,
}

/// A complete planned route — the MMS output before conversion to
/// [`crate::platform::PlatformCommand::FollowRoute`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutePlan {
    pub legs: Vec<RouteLeg>,
    /// Dispatch-ready waypoint sequence (includes Dubins-smoothed corners).
    pub waypoints: Vec<Waypoint>,
    pub total_length_m: f64,
    pub generated_at: f64,
    pub reason: RoutePlanReason,
    /// `false` when no feasible path was found (degraded loiter/hold).
    pub feasible: bool,
}

impl Default for RoutePlan {
    fn default() -> Self {
        Self {
            legs: Vec::new(),
            waypoints: Vec::new(),
            total_length_m: 0.0,
            generated_at: 0.0,
            reason: RoutePlanReason::Initial,
            feasible: false,
        }
    }
}

/// High-level navigation goal passed to the planner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PlanGoal {
    Point {
        lat_deg: f64,
        lon_deg: f64,
        alt_m: f64,
    },
    ZoneCenter {
        lat_deg: f64,
        lon_deg: f64,
        alt_m: f64,
    },
    Patrol {
        zone_id: String,
    },
    Standoff {
        track_id: String,
        range_m: f64,
    },
    Loiter {
        center_lat_deg: f64,
        center_lon_deg: f64,
        radius_m: f64,
        alt_m: f64,
    },
}

/// Platform motion constraints used by the planner.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PlatformKinematics {
    pub min_turn_radius_m: f64,
    pub max_climb_rate_ms: f64,
    pub speed_ms: f64,
    pub max_speed_ms: f64,
}

impl Default for PlatformKinematics {
    fn default() -> Self {
        Self {
            min_turn_radius_m: 50.0,
            max_climb_rate_ms: 5.0,
            speed_ms: 8.0,
            max_speed_ms: 30.0,
        }
    }
}

/// A keep-out region extruded into a vertical prism.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeepOutPrism {
    pub name: String,
    pub polygon: Vec<(f64, f64)>,
    pub alt_min_m: f64,
    pub alt_max_m: f64,
}

impl KeepOutPrism {
    /// Whether this prism blocks motion at the given cruise altitude.
    pub fn blocks_altitude(&self, alt_m: f64) -> bool {
        alt_m >= self.alt_min_m && alt_m <= self.alt_max_m
    }
}

/// Dynamic obstacle from a track (CPA layer, not visibility graph).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MovingObstacle {
    pub track_id: String,
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub alt_m: f64,
    pub speed_ms: f64,
    pub course_deg: f64,
    pub vertical_rate_ms: f64,
    pub radius_m: f64,
}

/// Full planning request — pure input to [`openfang_runtime::route_planner`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanRequest {
    pub start: Pose,
    pub goal: PlanGoal,
    pub kinematics: PlatformKinematics,
    pub keepouts: Vec<KeepOutPrism>,
    pub dynamic_obstacles: Vec<MovingObstacle>,
    /// UAV cruise altitude; `None` for surface craft (alt≈start.alt).
    pub cruise_alt_m: Option<f64>,
    /// Weight for routing away from hostile tracks (0 = disabled).
    pub threat_avoid_weight: f64,
}
