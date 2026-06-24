//! Standoff flank-approach route generation.
//!
//! Produces a short waypoint route that swings out to one side and approaches a
//! target from its rear while keeping the final leg at a configured standoff
//! distance. Used by the `recon_flank_route` function in the Mission compiler.
//!
//! All geometry is great-circle (spherical Earth); precision is ample for the
//! tactical ranges involved (single-digit km).

use openfang_types::platform::{Pose, Waypoint};

use crate::intent_extractor::FlankSide;

const EARTH_R_M: f64 = 6_371_000.0;

/// Great-circle destination point: start `(lat, lon)`, travel `dist_m` along
/// initial `bearing_deg` (true, clockwise from north). Returns `(lat, lon)` deg.
pub fn destination(lat_deg: f64, lon_deg: f64, bearing_deg: f64, dist_m: f64) -> (f64, f64) {
    let ang = dist_m / EARTH_R_M;
    let br = bearing_deg.to_radians();
    let lat1 = lat_deg.to_radians();
    let lon1 = lon_deg.to_radians();
    let lat2 = (lat1.sin() * ang.cos() + lat1.cos() * ang.sin() * br.cos()).asin();
    let lon2 =
        lon1 + (br.sin() * ang.sin() * lat1.cos()).atan2(ang.cos() - lat1.sin() * lat2.sin());
    (lat2.to_degrees(), normalize_lon(lon2.to_degrees()))
}

fn normalize_lon(lon: f64) -> f64 {
    let mut l = lon;
    while l > 180.0 {
        l -= 360.0;
    }
    while l < -180.0 {
        l += 360.0;
    }
    l
}

fn normalize_bearing(deg: f64) -> f64 {
    let mut b = deg % 360.0;
    if b < 0.0 {
        b += 360.0;
    }
    b
}

/// Inputs for a flank-approach route.
#[derive(Debug, Clone)]
pub struct FlankRequest {
    /// Current own pose.
    pub own: Pose,
    pub target_lat: f64,
    pub target_lon: f64,
    /// Target altitude; falls back to own altitude when `None`.
    pub target_alt_m: Option<f64>,
    /// Target heading (deg true). When known, "rear" = target's six o'clock;
    /// otherwise the route approaches from the far side of the own→target line.
    pub target_heading_deg: Option<f64>,
    /// Final-leg standoff distance to the target, meters.
    pub standoff_m: f64,
    /// Which side to swing out to. `None` defaults to the right.
    pub side: Option<FlankSide>,
    /// Cruise speed for the route legs.
    pub speed_ms: Option<f64>,
}

/// Generate a flank-approach route: a lateral swing waypoint followed by a
/// rear standoff waypoint. The final waypoint sits `standoff_m` from the target.
pub fn flank_route(req: &FlankRequest) -> Vec<Waypoint> {
    let target = Pose {
        lat_deg: req.target_lat,
        lon_deg: req.target_lon,
        alt_m: req.target_alt_m.unwrap_or(req.own.alt_m),
        heading_deg: 0.0,
        pitch_deg: 0.0,
        roll_deg: 0.0,
    };
    let alt = Some(req.own.alt_m);
    let bearing_to_target = req.own.bearing_to(&target);
    let range = req.own.distance_m(&target);
    let standoff = req.standoff_m.max(1.0);

    let side_sign = match req.side {
        Some(FlankSide::Left) => -1.0,
        Some(FlankSide::Right) | None => 1.0,
    };

    // Swing waypoint: offset laterally from the midpoint of the own→target leg.
    let (mid_lat, mid_lon) = destination(
        req.own.lat_deg,
        req.own.lon_deg,
        bearing_to_target,
        (range * 0.5).max(standoff),
    );
    let lateral = (standoff * 1.5).max(range * 0.4);
    let (swing_lat, swing_lon) = destination(
        mid_lat,
        mid_lon,
        normalize_bearing(bearing_to_target + 90.0 * side_sign),
        lateral,
    );

    // Rear standoff waypoint: target's six o'clock when heading is known, else
    // the far side of the own→target line (i.e. "around the back").
    let rear_bearing_from_target = match req.target_heading_deg {
        Some(heading) => normalize_bearing(heading + 180.0),
        None => normalize_bearing(bearing_to_target),
    };
    let (rear_lat, rear_lon) = destination(
        target.lat_deg,
        target.lon_deg,
        rear_bearing_from_target,
        standoff,
    );

    vec![
        Waypoint {
            lat: swing_lat,
            lon: swing_lon,
            alt,
            speed_ms: req.speed_ms,
        },
        Waypoint {
            lat: rear_lat,
            lon: rear_lon,
            alt,
            speed_ms: req.speed_ms,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own() -> Pose {
        Pose {
            lat_deg: 30.0,
            lon_deg: 120.0,
            alt_m: 1000.0,
            heading_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
        }
    }

    fn target_pose(lat: f64, lon: f64) -> Pose {
        Pose {
            lat_deg: lat,
            lon_deg: lon,
            alt_m: 1000.0,
            heading_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
        }
    }

    #[test]
    fn destination_moves_north_by_distance() {
        let (lat, lon) = destination(30.0, 120.0, 0.0, 1000.0);
        assert!(lat > 30.0, "north bearing should increase latitude");
        assert!((lon - 120.0).abs() < 1e-6, "due north keeps longitude");
    }

    #[test]
    fn final_waypoint_is_at_standoff_distance() {
        let req = FlankRequest {
            own: own(),
            target_lat: 30.1,
            target_lon: 120.1,
            target_alt_m: None,
            target_heading_deg: None,
            standoff_m: 3000.0,
            side: Some(FlankSide::Right),
            speed_ms: Some(50.0),
        };
        let route = flank_route(&req);
        assert_eq!(route.len(), 2);
        let last = route.last().unwrap();
        let last_pose = target_pose(last.lat, last.lon);
        let target = target_pose(req.target_lat, req.target_lon);
        let dist = target.distance_m(&last_pose);
        assert!(
            (dist - 3000.0).abs() < 50.0,
            "final waypoint should be ~standoff from target, got {dist:.0}m"
        );
    }

    #[test]
    fn left_and_right_swing_to_opposite_sides() {
        let base = FlankRequest {
            own: own(),
            target_lat: 30.2,
            target_lon: 120.0, // due north of own
            target_alt_m: None,
            target_heading_deg: None,
            standoff_m: 2000.0,
            side: Some(FlankSide::Right),
            speed_ms: None,
        };
        let right = flank_route(&base);
        let left = flank_route(&FlankRequest {
            side: Some(FlankSide::Left),
            ..base.clone()
        });
        // Target is due north → right swing goes east (lon up), left goes west.
        assert!(right[0].lon > 120.0, "right flank should swing east");
        assert!(left[0].lon < 120.0, "left flank should swing west");
    }

    #[test]
    fn rear_waypoint_uses_target_heading_when_known() {
        // Target heading north (0°); its six o'clock is due south.
        let req = FlankRequest {
            own: own(),
            target_lat: 30.1,
            target_lon: 120.0,
            target_alt_m: None,
            target_heading_deg: Some(0.0),
            standoff_m: 2000.0,
            side: None,
            speed_ms: None,
        };
        let route = flank_route(&req);
        let rear = route.last().unwrap();
        // Six o'clock of a north-bound target is south of it → lower latitude.
        assert!(rear.lat < 30.1, "rear waypoint should be south of target");
    }
}
