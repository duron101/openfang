//! Deterministic route planner — visibility graph + A* + Dubins smoothing.
//!
//! Produces [`RoutePlan`] values for the MMS cerebellum service. Same inputs
//! always yield the same plan (quantized coordinates, fixed tie-break).

use openfang_types::platform::{Pose, Waypoint};
use openfang_types::route::{
    KeepOutPrism, MovingObstacle, PlanGoal, PlanRequest, PlatformKinematics, RouteLeg, RoutePlan,
    RoutePlanReason,
};

use openfang_types::route::ArcSegment;

use crate::route_geometry::{
    bearing_deg, destination, distance_m, normalize_bearing, point_in_polygon, polygon_centroid,
    quantize_deg, quantize_point, segment_enters_polygon,
};

/// Plan a route from a [`PlanRequest`]. Never panics; returns `feasible=false`
/// on failure with a hold/loiter fallback when possible.
pub fn plan_route(req: &PlanRequest, now: f64, reason: RoutePlanReason) -> RoutePlan {
    let cruise_alt = req.cruise_alt_m.unwrap_or(req.start.alt_m);
    let (goal_lat, goal_lon, goal_alt) = resolve_goal(req);

    let start_pose = req.start;
    let goal_pose = Pose {
        lat_deg: goal_lat,
        lon_deg: goal_lon,
        alt_m: goal_alt,
        heading_deg: start_pose.heading_deg,
        pitch_deg: 0.0,
        roll_deg: 0.0,
    };

    // Active keep-outs at cruise altitude (horizontal projection).
    let active_keepouts: Vec<&KeepOutPrism> = req
        .keepouts
        .iter()
        .filter(|k| k.blocks_altitude(cruise_alt))
        .collect();

    let polyline = if active_keepouts.is_empty() {
        vec![
            quantize_point(start_pose.lat_deg, start_pose.lon_deg),
            quantize_point(goal_lat, goal_lon),
        ]
    } else {
        match astar_visibility(
            &start_pose,
            &goal_pose,
            &active_keepouts,
            cruise_alt,
            &req.kinematics,
            &req.dynamic_obstacles,
            req.threat_avoid_weight,
        ) {
            Some(path) => path,
            None => {
                return degraded_plan(req, now, reason);
            }
        }
    };

    let mut smoothed = dubins_smooth_polyline(&polyline, &req.kinematics);

    // Post-smoothing safety: arc fillets can bulge into a keep-out the raw
    // polyline cleared. If any smoothed segment enters an active prism, fall
    // back to the unsmoothed (verified-clear) polyline.
    if !smoothed_clear(&smoothed, &active_keepouts) {
        smoothed = polyline
            .iter()
            .map(|&(lat, lon)| SmoothPt {
                lat,
                lon,
                arc: None,
            })
            .collect();
    }

    let (with_alt, climb_ok) =
        assign_vertical_profile(&smoothed, start_pose.alt_m, goal_alt, &req.kinematics);

    let mut legs = Vec::new();
    let mut total = 0.0;
    for i in 0..with_alt.len().saturating_sub(1) {
        let from = with_alt[i].clone();
        let to = with_alt[i + 1].clone();
        let from_pose = waypoint_to_pose(&from);
        let to_pose = waypoint_to_pose(&to);
        let len = distance_m(&from_pose, &to_pose);
        total += len;
        legs.push(RouteLeg {
            heading_deg: bearing_deg(&from_pose, to.lat, to.lon),
            length_m: len,
            from,
            to,
            arc: smoothed.get(i).and_then(|p| p.arc),
        });
    }

    RoutePlan {
        legs,
        waypoints: with_alt,
        total_length_m: total,
        generated_at: now,
        reason,
        feasible: climb_ok,
    }
}

/// A smoothed planar point plus the arc (if any) of the leg leaving it.
#[derive(Clone)]
struct SmoothPt {
    lat: f64,
    lon: f64,
    arc: Option<ArcSegment>,
}

fn smoothed_clear(pts: &[SmoothPt], keepouts: &[&KeepOutPrism]) -> bool {
    for w in pts.windows(2) {
        for ko in keepouts {
            if segment_enters_polygon((w[0].lat, w[0].lon), (w[1].lat, w[1].lon), &ko.polygon) {
                return false;
            }
        }
    }
    true
}

fn resolve_goal(req: &PlanRequest) -> (f64, f64, f64) {
    let cruise = req.cruise_alt_m.unwrap_or(req.start.alt_m);
    match &req.goal {
        PlanGoal::Point {
            lat_deg,
            lon_deg,
            alt_m,
        } => (*lat_deg, *lon_deg, *alt_m),
        PlanGoal::ZoneCenter {
            lat_deg,
            lon_deg,
            alt_m,
        } => (*lat_deg, *lon_deg, *alt_m),
        PlanGoal::Patrol { zone_id: _ } => (req.start.lat_deg, req.start.lon_deg, cruise),
        PlanGoal::Standoff {
            track_id: _,
            range_m,
        } => {
            // Standoff along current heading when track geometry unavailable here.
            let (lat, lon) = destination(
                req.start.lat_deg,
                req.start.lon_deg,
                req.start.heading_deg,
                *range_m,
            );
            (lat, lon, cruise)
        }
        PlanGoal::Loiter {
            center_lat_deg,
            center_lon_deg,
            radius_m,
            alt_m,
        } => {
            let (lat, lon) = destination(*center_lat_deg, *center_lon_deg, 0.0, *radius_m);
            (lat, lon, *alt_m)
        }
    }
}

fn degraded_plan(req: &PlanRequest, now: f64, reason: RoutePlanReason) -> RoutePlan {
    let hold = Waypoint {
        lat: req.start.lat_deg,
        lon: req.start.lon_deg,
        alt: Some(req.start.alt_m),
        speed_ms: Some(0.0),
    };
    RoutePlan {
        legs: vec![],
        waypoints: vec![hold],
        total_length_m: 0.0,
        generated_at: now,
        reason,
        feasible: false,
    }
}

fn waypoint_to_pose(w: &Waypoint) -> Pose {
    Pose {
        lat_deg: w.lat,
        lon_deg: w.lon,
        alt_m: w.alt.unwrap_or(0.0),
        heading_deg: 0.0,
        pitch_deg: 0.0,
        roll_deg: 0.0,
    }
}

// ── Visibility graph + A* ────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct Node {
    lat: f64,
    lon: f64,
}

impl Node {
    fn pose_at_alt(&self, alt_m: f64) -> Pose {
        Pose {
            lat_deg: self.lat,
            lon_deg: self.lon,
            alt_m,
            heading_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
        }
    }
}

fn build_nodes(start: &Pose, goal: &Pose, keepouts: &[&KeepOutPrism]) -> Vec<Node> {
    let mut nodes = vec![
        Node {
            lat: quantize_deg(start.lat_deg),
            lon: quantize_deg(start.lon_deg),
        },
        Node {
            lat: quantize_deg(goal.lat_deg),
            lon: quantize_deg(goal.lon_deg),
        },
    ];
    for ko in keepouts {
        for &(lat, lon) in &ko.polygon {
            let q = quantize_point(lat, lon);
            if !nodes.iter().any(|n| n.lat == q.0 && n.lon == q.1) {
                nodes.push(Node { lat: q.0, lon: q.1 });
            }
        }
    }
    // Deterministic order: start stays 0, goal stays 1, rest sorted.
    let start_n = nodes[0];
    let goal_n = nodes[1];
    let mut rest: Vec<Node> = nodes.into_iter().skip(2).collect();
    rest.sort_by(|a, b| {
        a.lat
            .partial_cmp(&b.lat)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                a.lon
                    .partial_cmp(&b.lon)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });
    let mut out = vec![start_n, goal_n];
    out.extend(rest);
    out
}

fn edge_clear(a: &Node, b: &Node, keepouts: &[&KeepOutPrism]) -> bool {
    for ko in keepouts {
        // Exact interior test that tolerates visibility edges touching polygon
        // vertices (shared-endpoint grazing is not a crossing) but rejects any
        // segment that actually passes through the keep-out interior.
        if segment_enters_polygon((a.lat, a.lon), (b.lat, b.lon), &ko.polygon) {
            return false;
        }
    }
    true
}

fn edge_cost(
    a: &Node,
    b: &Node,
    alt_m: f64,
    threats: &[MovingObstacle],
    threat_weight: f64,
) -> f64 {
    let pa = a.pose_at_alt(alt_m);
    let pb = b.pose_at_alt(alt_m);
    let mut cost = distance_m(&pa, &pb);
    if threat_weight > 0.0 {
        for t in threats {
            let tp = Pose {
                lat_deg: t.lat_deg,
                lon_deg: t.lon_deg,
                alt_m: t.alt_m,
                heading_deg: 0.0,
                pitch_deg: 0.0,
                roll_deg: 0.0,
            };
            let d = distance_m(&pa, &tp).max(1.0);
            cost += threat_weight * 1000.0 / d;
        }
    }
    cost
}

fn heuristic(a: &Node, goal: &Node, alt_m: f64) -> f64 {
    distance_m(&a.pose_at_alt(alt_m), &goal.pose_at_alt(alt_m))
}

fn astar_visibility(
    start: &Pose,
    goal: &Pose,
    keepouts: &[&KeepOutPrism],
    alt_m: f64,
    _kinematics: &PlatformKinematics,
    threats: &[MovingObstacle],
    threat_weight: f64,
) -> Option<Vec<(f64, f64)>> {
    let nodes = build_nodes(start, goal, keepouts);
    let n = nodes.len();
    if n < 2 {
        return None;
    }
    let goal_idx = 1usize;

    // Adjacency: for each pair, if edge clear, store cost.
    let mut adj: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    for i in 0..n {
        for j in (i + 1)..n {
            if edge_clear(&nodes[i], &nodes[j], keepouts) {
                let c = edge_cost(&nodes[i], &nodes[j], alt_m, threats, threat_weight);
                adj[i].push((j, c));
                adj[j].push((i, c));
            }
        }
    }

    // A* with deterministic tie-break (f, g, node_id).
    let mut g_score = vec![f64::INFINITY; n];
    let mut came_from: Vec<Option<usize>> = vec![None; n];
    g_score[0] = 0.0;

    let mut open: Vec<(usize, f64, f64)> =
        vec![(0, heuristic(&nodes[0], &nodes[goal_idx], alt_m), 0.0)];
    let mut closed = vec![false; n];

    while !open.is_empty() {
        open.sort_by(|a, b| {
            let fa = a.1 + a.2;
            let fb = b.1 + b.2;
            fa.partial_cmp(&fb)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
                .then_with(|| a.0.cmp(&b.0))
        });
        let (current, _, g_cur) = open.remove(0);
        if closed[current] {
            continue;
        }
        if current == goal_idx {
            // Reconstruct path.
            let mut path = vec![(nodes[current].lat, nodes[current].lon)];
            let mut c = current;
            while let Some(prev) = came_from[c] {
                path.push((nodes[prev].lat, nodes[prev].lon));
                c = prev;
            }
            path.reverse();
            return Some(path);
        }
        closed[current] = true;
        for &(next, cost) in &adj[current] {
            if closed[next] {
                continue;
            }
            let tentative = g_cur + cost;
            if tentative < g_score[next] {
                g_score[next] = tentative;
                came_from[next] = Some(current);
                let h = heuristic(&nodes[next], &nodes[goal_idx], alt_m);
                // Remove stale entries lazily via closed set.
                open.push((next, h, tentative));
            }
        }
    }
    None
}

// ── Dubins smoothing (tangent circular-arc fillets) ──────────────────────

/// Signed deflection from `b_in` to `b_out`, in (-180, 180]. Positive = turn
/// to starboard (clockwise compass), negative = port.
fn signed_delta(b_in: f64, b_out: f64) -> f64 {
    let d = normalize_bearing(b_out - b_in);
    if d > 180.0 {
        d - 360.0
    } else {
        d
    }
}

/// Replace each interior vertex with a circular arc tangent to both edges,
/// honoring `min_turn_radius_m` (reduced only when adjacent edges are too short
/// for the full radius). Fills [`ArcSegment`] metadata on the leg entering the
/// arc. Closed-form and deterministic.
fn dubins_smooth_polyline(
    polyline: &[(f64, f64)],
    kinematics: &PlatformKinematics,
) -> Vec<SmoothPt> {
    if polyline.len() <= 2 {
        return polyline
            .iter()
            .map(|&(lat, lon)| SmoothPt {
                lat,
                lon,
                arc: None,
            })
            .collect();
    }

    let r = kinematics.min_turn_radius_m.max(5.0);
    let mut out = vec![SmoothPt {
        lat: polyline[0].0,
        lon: polyline[0].1,
        arc: None,
    }];

    for i in 1..polyline.len() - 1 {
        let prev = polyline[i - 1];
        let cur = polyline[i];
        let next = polyline[i + 1];

        let p_pose = pose_at(prev.0, prev.1);
        let c_pose = pose_at(cur.0, cur.1);
        let n_pose = pose_at(next.0, next.1);

        let b_in = bearing_deg(&p_pose, cur.0, cur.1);
        let b_out = bearing_deg(&c_pose, next.0, next.1);
        let delta = signed_delta(b_in, b_out);
        let theta = delta.abs();

        // Near-straight or near-reversal: keep the vertex as-is.
        if !(5.0..=175.0).contains(&theta) {
            out.push(SmoothPt {
                lat: cur.0,
                lon: cur.1,
                arc: None,
            });
            continue;
        }

        let in_len = distance_m(&p_pose, &c_pose);
        let out_len = distance_m(&c_pose, &n_pose);
        let half = (theta * 0.5).to_radians();
        let tan_half = half.tan().max(1e-6);

        // Tangent distance d = r·tan(θ/2); shrink r if edges are too short.
        let mut d = r * tan_half;
        let cap = 0.45 * in_len.min(out_len);
        let r_eff = if cap > 0.0 && d > cap {
            d = cap;
            cap / tan_half
        } else {
            r
        };

        let sign = if delta > 0.0 { 1.0 } else { -1.0 };
        let t1 = destination(cur.0, cur.1, normalize_bearing(b_in + 180.0), d);
        let t2 = destination(cur.0, cur.1, b_out, d);
        let center = destination(t1.0, t1.1, normalize_bearing(b_in + 90.0 * sign), r_eff);
        let center_pose = pose_at(center.0, center.1);
        let start_bearing = normalize_bearing(bearing_deg(&center_pose, t1.0, t1.1));

        out.push(SmoothPt {
            lat: t1.0,
            lon: t1.1,
            arc: Some(ArcSegment {
                center_lat_deg: center.0,
                center_lon_deg: center.1,
                radius_m: r_eff,
                start_bearing_deg: start_bearing,
                sweep_deg: sign * theta,
            }),
        });

        let arc_len = r_eff * theta.to_radians();
        let steps = ((arc_len / 40.0).ceil() as usize).clamp(2, 16);
        for s in 1..steps {
            let f = s as f64 / steps as f64;
            let br = normalize_bearing(start_bearing + sign * theta * f);
            let (lat, lon) = destination(center.0, center.1, br, r_eff);
            out.push(SmoothPt {
                lat,
                lon,
                arc: None,
            });
        }
        out.push(SmoothPt {
            lat: t2.0,
            lon: t2.1,
            arc: None,
        });
    }

    let last = polyline[polyline.len() - 1];
    out.push(SmoothPt {
        lat: last.0,
        lon: last.1,
        arc: None,
    });
    out
}

fn pose_at(lat: f64, lon: f64) -> Pose {
    Pose {
        lat_deg: lat,
        lon_deg: lon,
        alt_m: 0.0,
        heading_deg: 0.0,
        pitch_deg: 0.0,
        roll_deg: 0.0,
    }
}

// ── Vertical profile (UAV 3D) ────────────────────────────────────────────

/// Assign a climb-rate-limited vertical profile. Returns the altitude-tagged
/// waypoints plus whether the goal altitude was actually reached (climb-rate
/// feasibility — drives [`RoutePlan::feasible`]).
fn assign_vertical_profile(
    pts: &[SmoothPt],
    start_alt: f64,
    goal_alt: f64,
    kinematics: &PlatformKinematics,
) -> (Vec<Waypoint>, bool) {
    if pts.is_empty() {
        return (Vec::new(), true);
    }
    let mut out = Vec::with_capacity(pts.len());
    let mut current_alt = start_alt;
    let max_climb = kinematics.max_climb_rate_ms.max(0.1);
    let denom = (pts.len().saturating_sub(1).max(1)) as f64;

    for (i, p) in pts.iter().enumerate() {
        let frac = i as f64 / denom;
        let target_alt = start_alt + (goal_alt - start_alt) * frac;
        let seg_len = if i == 0 {
            0.0
        } else {
            distance_m(
                &pose_at(pts[i - 1].lat, pts[i - 1].lon),
                &pose_at(p.lat, p.lon),
            )
        };
        let seg_time = if kinematics.speed_ms > 0.1 {
            seg_len / kinematics.speed_ms
        } else {
            0.0
        };
        let max_delta = max_climb * seg_time;
        let delta = (target_alt - current_alt).clamp(-max_delta, max_delta);
        current_alt += delta;
        out.push(Waypoint {
            lat: p.lat,
            lon: p.lon,
            alt: Some(current_alt),
            speed_ms: Some(kinematics.speed_ms),
        });
    }

    let tol = 2.0_f64.max((goal_alt - start_alt).abs() * 0.02);
    let reached = (current_alt - goal_alt).abs() <= tol;
    (out, reached)
}

/// Build keep-out prisms from geofence polygons at a given cruise altitude.
pub fn keepouts_from_geofences(
    geofences: &[openfang_types::umaa::Geofence],
    cruise_alt_m: f64,
) -> Vec<KeepOutPrism> {
    use openfang_types::umaa::GeofenceType;
    geofences
        .iter()
        .filter_map(|g| {
            // Altitude bands only become horizontal obstacles when the cruise
            // altitude violates them; a UAV above an AltitudeCeiling region (or
            // below an AltitudeFloor) can overfly/underfly without blocking.
            let (alt_min, alt_max) = match &g.restriction {
                GeofenceType::KeepOut => (f64::NEG_INFINITY, f64::INFINITY),
                GeofenceType::AltitudeCeiling { max_alt_m } => {
                    if cruise_alt_m <= *max_alt_m {
                        return None; // within ceiling → free to transit
                    }
                    (*max_alt_m, f64::INFINITY)
                }
                GeofenceType::AltitudeFloor { min_alt_m } => {
                    if cruise_alt_m >= *min_alt_m {
                        return None; // above floor → free to transit
                    }
                    (f64::NEG_INFINITY, *min_alt_m)
                }
                _ => return None,
            };
            Some(KeepOutPrism {
                name: g.name.clone(),
                polygon: g.boundary.clone(),
                alt_min_m: alt_min,
                alt_max_m: alt_max,
            })
        })
        .collect()
}

/// Offset a point by `along_m` on bearing `along_b` then `perp_m` on bearing
/// `perp_b` (negative distances flip the bearing).
fn offset_point(
    lat: f64,
    lon: f64,
    along_b: f64,
    along_m: f64,
    perp_b: f64,
    perp_m: f64,
) -> (f64, f64) {
    let (ab, ad) = if along_m >= 0.0 {
        (along_b, along_m)
    } else {
        (normalize_bearing(along_b + 180.0), -along_m)
    };
    let (p1, p2) = destination(lat, lon, ab, ad);
    let (pb, pd) = if perp_m >= 0.0 {
        (perp_b, perp_m)
    } else {
        (normalize_bearing(perp_b + 180.0), -perp_m)
    };
    destination(p1, p2, pb, pd)
}

/// Patrol racetrack (stadium) inside a zone polygon: two parallel straights
/// joined by semicircular ends, oriented along the zone's longer axis.
pub fn patrol_racetrack(
    polygon: &[(f64, f64)],
    cruise_alt_m: f64,
    speed_ms: f64,
    laps: usize,
) -> Vec<Waypoint> {
    let Some((clat, clon)) = polygon_centroid(polygon) else {
        return Vec::new();
    };
    let center_wp = || Waypoint {
        lat: clat,
        lon: clon,
        alt: Some(cruise_alt_m),
        speed_ms: Some(speed_ms),
    };

    let (mut min_lat, mut max_lat) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut min_lon, mut max_lon) = (f64::INFINITY, f64::NEG_INFINITY);
    for &(la, lo) in polygon {
        min_lat = min_lat.min(la);
        max_lat = max_lat.max(la);
        min_lon = min_lon.min(lo);
        max_lon = max_lon.max(lo);
    }
    if !min_lat.is_finite() {
        return vec![center_wp()];
    }

    let half_ns = distance_m(&pose_at(min_lat, clon), &pose_at(max_lat, clon)) * 0.5;
    let half_ew = distance_m(&pose_at(clat, min_lon), &pose_at(clat, max_lon)) * 0.5;
    let margin = 0.75;
    // Long axis bearing; perpendicular is short axis.
    let (long_b, long_half, short_half) = if half_ew >= half_ns {
        (90.0, half_ew * margin, half_ns * margin)
    } else {
        (0.0, half_ns * margin, half_ew * margin)
    };
    let short_b = normalize_bearing(long_b - 90.0);
    let r = short_half.max(20.0);
    let l_half = (long_half - r).max(0.0);

    let mk = |lat: f64, lon: f64| Waypoint {
        lat,
        lon,
        alt: Some(cruise_alt_m),
        speed_ms: Some(speed_ms),
    };
    let arc_steps = 6usize;
    let mut wps = Vec::new();

    for _ in 0..laps.max(1) {
        // Straight 1 (+perp side): A(+long) → B(-long).
        let a = offset_point(clat, clon, long_b, l_half, short_b, r);
        let b = offset_point(clat, clon, long_b, -l_half, short_b, r);
        wps.push(mk(a.0, a.1));
        wps.push(mk(b.0, b.1));
        // Semicircle at -long end (center on axis), B → C bulging outward.
        let c_neg = offset_point(clat, clon, long_b, -l_half, short_b, 0.0);
        for s in 1..arc_steps {
            let f = s as f64 / arc_steps as f64;
            let br = normalize_bearing(short_b - 180.0 * f);
            let (la, lo) = destination(c_neg.0, c_neg.1, br, r);
            wps.push(mk(la, lo));
        }
        // Straight 2 (-perp side): C(-long) → D(+long).
        let c = offset_point(clat, clon, long_b, -l_half, short_b, -r);
        let d = offset_point(clat, clon, long_b, l_half, short_b, -r);
        wps.push(mk(c.0, c.1));
        wps.push(mk(d.0, d.1));
        // Semicircle at +long end, D → A bulging outward.
        let c_pos = offset_point(clat, clon, long_b, l_half, short_b, 0.0);
        for s in 1..arc_steps {
            let f = s as f64 / arc_steps as f64;
            let br = normalize_bearing((short_b + 180.0) - 180.0 * f);
            let (la, lo) = destination(c_pos.0, c_pos.1, br, r);
            wps.push(mk(la, lo));
        }
    }

    // Drop any points escaping a concave zone; keep racetrack for convex zones.
    wps.retain(|w| point_in_polygon((w.lat, w.lon), polygon));
    if wps.len() < 4 {
        return vec![center_wp()];
    }
    wps
}

/// Circular orbit waypoints around a center — degraded hold / safe loiter
/// (closes the loop back to the first point).
pub fn loiter_orbit(
    center_lat: f64,
    center_lon: f64,
    radius_m: f64,
    alt_m: f64,
    speed_ms: f64,
    points: usize,
) -> Vec<Waypoint> {
    let n = points.max(4);
    let r = radius_m.max(20.0);
    let mut wps: Vec<Waypoint> = (0..n)
        .map(|i| {
            let br = 360.0 * i as f64 / n as f64;
            let (lat, lon) = destination(center_lat, center_lon, br, r);
            Waypoint {
                lat,
                lon,
                alt: Some(alt_m),
                speed_ms: Some(speed_ms),
            }
        })
        .collect();
    if let Some(first) = wps.first().cloned() {
        wps.push(first);
    }
    wps
}

/// Local CPA avoidance heading using full COLREGs encounter classification.
#[allow(clippy::too_many_arguments)]
pub fn cpa_avoidance_heading(
    own: &Pose,
    own_speed_ms: f64,
    own_course_deg: f64,
    obstacle_lat: f64,
    obstacle_lon: f64,
    obstacle_speed_ms: f64,
    obstacle_course_deg: f64,
    min_cpa_m: f64,
    max_tcpa_s: f64,
) -> Option<f64> {
    cpa_avoidance_maneuver(
        own,
        own_speed_ms,
        own_course_deg,
        obstacle_lat,
        obstacle_lon,
        obstacle_speed_ms,
        obstacle_course_deg,
        min_cpa_m,
        max_tcpa_s,
    )
    .map(|m| m.heading_deg)
}

/// COLREGs-classified avoidance maneuver (heading + optional speed).
#[allow(clippy::too_many_arguments)]
pub fn cpa_avoidance_maneuver(
    own: &Pose,
    own_speed_ms: f64,
    own_course_deg: f64,
    obstacle_lat: f64,
    obstacle_lon: f64,
    obstacle_speed_ms: f64,
    obstacle_course_deg: f64,
    min_cpa_m: f64,
    max_tcpa_s: f64,
) -> Option<crate::colregs::ColregsManeuver> {
    crate::colregs::colregs_avoidance_maneuver(&crate::colregs::ColregsInputs {
        own: *own,
        own_speed_ms,
        own_course_deg,
        other_lat_deg: obstacle_lat,
        other_lon_deg: obstacle_lon,
        other_speed_ms: obstacle_speed_ms,
        other_course_deg: obstacle_course_deg,
        min_cpa_m,
        max_tcpa_s,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::route::PlanGoal;

    fn square_keepout() -> KeepOutPrism {
        KeepOutPrism {
            name: "box".into(),
            polygon: vec![
                (30.01, 120.01),
                (30.01, 120.02),
                (30.02, 120.02),
                (30.02, 120.01),
            ],
            alt_min_m: 0.0,
            alt_max_m: 10_000.0,
        }
    }

    #[test]
    fn astar_routes_around_keepout() {
        let start = Pose {
            lat_deg: 30.0,
            lon_deg: 120.0,
            alt_m: 100.0,
            heading_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
        };
        let req = PlanRequest {
            start,
            goal: PlanGoal::Point {
                lat_deg: 30.03,
                lon_deg: 120.03,
                alt_m: 100.0,
            },
            kinematics: PlatformKinematics::default(),
            keepouts: vec![square_keepout()],
            dynamic_obstacles: vec![],
            cruise_alt_m: Some(100.0),
            threat_avoid_weight: 0.0,
        };
        let plan = plan_route(&req, 1.0, RoutePlanReason::Initial);
        assert!(plan.feasible);
        assert!(plan.waypoints.len() >= 2);
    }

    #[test]
    fn route_never_crosses_keepout() {
        use crate::route_geometry::segment_enters_polygon;
        let start = Pose {
            lat_deg: 30.0,
            lon_deg: 120.0,
            alt_m: 100.0,
            heading_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
        };
        let ko = square_keepout();
        let req = PlanRequest {
            start,
            goal: PlanGoal::Point {
                lat_deg: 30.03,
                lon_deg: 120.03,
                alt_m: 100.0,
            },
            kinematics: PlatformKinematics::default(),
            keepouts: vec![ko.clone()],
            dynamic_obstacles: vec![],
            cruise_alt_m: Some(100.0),
            threat_avoid_weight: 0.0,
        };
        let plan = plan_route(&req, 1.0, RoutePlanReason::Initial);
        assert!(plan.feasible);
        for w in plan.waypoints.windows(2) {
            assert!(
                !segment_enters_polygon((w[0].lat, w[0].lon), (w[1].lat, w[1].lon), &ko.polygon),
                "leg {:?}->{:?} crosses keep-out",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn steep_climb_marks_infeasible() {
        let start = Pose {
            lat_deg: 30.0,
            lon_deg: 120.0,
            alt_m: 0.0,
            heading_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
        };
        let req = PlanRequest {
            start,
            goal: PlanGoal::Point {
                lat_deg: 30.001,
                lon_deg: 120.0,
                alt_m: 1000.0,
            },
            kinematics: PlatformKinematics {
                min_turn_radius_m: 50.0,
                max_climb_rate_ms: 1.0,
                speed_ms: 20.0,
                max_speed_ms: 30.0,
            },
            keepouts: vec![],
            dynamic_obstacles: vec![],
            cruise_alt_m: Some(1000.0),
            threat_avoid_weight: 0.0,
        };
        let plan = plan_route(&req, 0.0, RoutePlanReason::Initial);
        assert!(!plan.feasible, "unreachable climb must be infeasible");
    }

    #[test]
    fn plan_route_is_deterministic() {
        let start = Pose {
            lat_deg: 30.0,
            lon_deg: 120.0,
            alt_m: 50.0,
            heading_deg: 45.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
        };
        let req = PlanRequest {
            start,
            goal: PlanGoal::Point {
                lat_deg: 30.05,
                lon_deg: 120.05,
                alt_m: 120.0,
            },
            kinematics: PlatformKinematics {
                min_turn_radius_m: 80.0,
                max_climb_rate_ms: 4.0,
                speed_ms: 10.0,
                max_speed_ms: 25.0,
            },
            keepouts: vec![],
            dynamic_obstacles: vec![],
            cruise_alt_m: Some(120.0),
            threat_avoid_weight: 0.0,
        };
        let a = plan_route(&req, 0.0, RoutePlanReason::Initial);
        let b = plan_route(&req, 0.0, RoutePlanReason::Initial);
        assert_eq!(a.waypoints.len(), b.waypoints.len());
        for (wa, wb) in a.waypoints.iter().zip(b.waypoints.iter()) {
            assert!((wa.lat - wb.lat).abs() < 1e-9);
            assert!((wa.lon - wb.lon).abs() < 1e-9);
        }
    }
}
