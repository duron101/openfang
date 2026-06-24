//! Deterministic geospatial helpers for MMS route planning.
//!
//! All functions are pure and allocation-light where possible. Coordinate
//! quantization ensures repeatable graph node ordering.

use openfang_types::platform::Pose;

pub const EARTH_R_M: f64 = 6_371_000.0;
/// Quantize lat/lon to ~1 cm precision at equator (deterministic graph keys).
pub const QUANT_DEG: f64 = 1e-7;

/// Quantize a degree value for stable ordering / hashing.
pub fn quantize_deg(v: f64) -> f64 {
    (v / QUANT_DEG).round() * QUANT_DEG
}

pub fn quantize_point(lat_deg: f64, lon_deg: f64) -> (f64, f64) {
    (quantize_deg(lat_deg), quantize_deg(lon_deg))
}

/// Great-circle destination (true bearing, meters).
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

pub fn normalize_lon(lon: f64) -> f64 {
    let mut l = lon;
    while l > 180.0 {
        l -= 360.0;
    }
    while l < -180.0 {
        l += 360.0;
    }
    l
}

pub fn normalize_bearing(deg: f64) -> f64 {
    let mut b = deg % 360.0;
    if b < 0.0 {
        b += 360.0;
    }
    b
}

pub fn distance_m(a: &Pose, b: &Pose) -> f64 {
    a.distance_m(b)
}

pub fn bearing_deg(from: &Pose, to_lat: f64, to_lon: f64) -> f64 {
    let to = Pose {
        lat_deg: to_lat,
        lon_deg: to_lon,
        alt_m: from.alt_m,
        heading_deg: 0.0,
        pitch_deg: 0.0,
        roll_deg: 0.0,
    };
    from.bearing_to(&to)
}

/// Ray-casting point-in-polygon (lat, lon).
pub fn point_in_polygon(point: (f64, f64), polygon: &[(f64, f64)]) -> bool {
    if polygon.len() < 3 {
        return false;
    }
    let (x, y) = point;
    let mut inside = false;
    let mut j = polygon.len() - 1;
    for i in 0..polygon.len() {
        let (xi, yi) = polygon[i];
        let (xj, yj) = polygon[j];
        if ((yi > y) != (yj > y)) && (x < (xj - xi) * (y - yi) / ((yj - yi).abs().max(1e-9)) + xi) {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Orientation test: cross product sign for segment (a,b) vs point c.
fn orient(a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> f64 {
    (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
}

fn on_segment(a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> bool {
    c.0 <= a.0.max(b.0) + 1e-12
        && c.0 + 1e-12 >= a.0.min(b.0)
        && c.1 <= a.1.max(b.1) + 1e-12
        && c.1 + 1e-12 >= a.1.min(b.1)
}

/// True if segment ab intersects segment cd (inclusive of touching).
pub fn segments_intersect(a: (f64, f64), b: (f64, f64), c: (f64, f64), d: (f64, f64)) -> bool {
    let o1 = orient(a, b, c);
    let o2 = orient(a, b, d);
    let o3 = orient(c, d, a);
    let o4 = orient(c, d, b);

    if o1 * o2 < 0.0 && o3 * o4 < 0.0 {
        return true;
    }
    if o1.abs() < 1e-12 && on_segment(a, b, c) {
        return true;
    }
    if o2.abs() < 1e-12 && on_segment(a, b, d) {
        return true;
    }
    if o3.abs() < 1e-12 && on_segment(c, d, a) {
        return true;
    }
    if o4.abs() < 1e-12 && on_segment(c, d, b) {
        return true;
    }
    false
}

/// True if the open segment crosses into or through the polygon interior.
pub fn segment_intersects_polygon(a: (f64, f64), b: (f64, f64), polygon: &[(f64, f64)]) -> bool {
    if polygon.len() < 3 {
        return false;
    }
    if point_in_polygon(a, polygon) || point_in_polygon(b, polygon) {
        return true;
    }
    let n = polygon.len();
    for i in 0..n {
        let j = (i + 1) % n;
        if segments_intersect(a, b, polygon[i], polygon[j]) {
            return true;
        }
    }
    false
}

/// Strict proper crossing of segments ab and cd — excludes shared-endpoint /
/// collinear touching (used by visibility-graph edges that legitimately touch
/// polygon vertices).
pub fn segments_properly_cross(a: (f64, f64), b: (f64, f64), c: (f64, f64), d: (f64, f64)) -> bool {
    let o1 = orient(a, b, c);
    let o2 = orient(a, b, d);
    let o3 = orient(c, d, a);
    let o4 = orient(c, d, b);
    o1 * o2 < 0.0 && o3 * o4 < 0.0
}

/// True if `p` lies strictly inside the polygon (vertices/edges count as
/// outside). Robust for visibility-graph nodes that sit on keep-out vertices.
pub fn point_strictly_inside(p: (f64, f64), polygon: &[(f64, f64)]) -> bool {
    if polygon
        .iter()
        .any(|v| (v.0 - p.0).abs() < 1e-12 && (v.1 - p.1).abs() < 1e-12)
    {
        return false;
    }
    point_in_polygon(p, polygon)
}

/// True if segment ab passes through the polygon **interior**, treating a
/// shared vertex / boundary graze as non-blocking. This is the correct
/// visibility-graph edge test: a sampling-point heuristic (endpoints+midpoint)
/// can miss a segment whose intersection interval is offset toward one end.
pub fn segment_enters_polygon(a: (f64, f64), b: (f64, f64), polygon: &[(f64, f64)]) -> bool {
    if polygon.len() < 3 {
        return false;
    }
    if point_strictly_inside(a, polygon) || point_strictly_inside(b, polygon) {
        return true;
    }
    let n = polygon.len();
    for i in 0..n {
        let j = (i + 1) % n;
        if segments_properly_cross(a, b, polygon[i], polygon[j]) {
            return true;
        }
    }
    // A segment can enter through a vertex without a proper edge crossing
    // (e.g. via a reflex/duplicate vertex); the midpoint guard catches that.
    let mid = ((a.0 + b.0) * 0.5, (a.1 + b.1) * 0.5);
    point_strictly_inside(mid, polygon)
}

/// Local equirectangular projection of `p` relative to `origin`, in meters.
fn to_local_m(origin: (f64, f64), p: (f64, f64)) -> (f64, f64) {
    let lat0 = origin.0.to_radians();
    let x = (p.1 - origin.1).to_radians() * lat0.cos() * EARTH_R_M;
    let y = (p.0 - origin.0).to_radians() * EARTH_R_M;
    (x, y)
}

/// Minimum cross-track distance (meters) from point `(p_lat,p_lon)` to a
/// polyline of `(lat, lon)` vertices. Returns 0 for an empty polyline.
pub fn cross_track_distance_m(p_lat: f64, p_lon: f64, polyline: &[(f64, f64)]) -> f64 {
    match polyline.len() {
        0 => 0.0,
        1 => {
            let (x, y) = to_local_m((p_lat, p_lon), polyline[0]);
            (x * x + y * y).sqrt()
        }
        _ => {
            let origin = (p_lat, p_lon);
            let mut min = f64::INFINITY;
            for seg in polyline.windows(2) {
                let a = to_local_m(origin, seg[0]);
                let b = to_local_m(origin, seg[1]);
                let abx = b.0 - a.0;
                let aby = b.1 - a.1;
                let len2 = abx * abx + aby * aby;
                let d = if len2 < 1e-9 {
                    (a.0 * a.0 + a.1 * a.1).sqrt()
                } else {
                    // Point is the origin (0,0); project onto segment.
                    let t = -(a.0 * abx + a.1 * aby) / len2;
                    let t = t.clamp(0.0, 1.0);
                    let px = a.0 + t * abx;
                    let py = a.1 + t * aby;
                    (px * px + py * py).sqrt()
                };
                min = min.min(d);
            }
            min
        }
    }
}

/// Centroid of a polygon (for zone goals).
pub fn polygon_centroid(polygon: &[(f64, f64)]) -> Option<(f64, f64)> {
    if polygon.is_empty() {
        return None;
    }
    let (mut lat, mut lon) = (0.0, 0.0);
    for (la, lo) in polygon {
        lat += la;
        lon += lo;
    }
    let n = polygon.len() as f64;
    Some((lat / n, lon / n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_round_trip_bearing() {
        let (lat2, lon2) = destination(30.0, 120.0, 90.0, 1000.0);
        let back = destination(lat2, lon2, 270.0, 1000.0);
        assert!((back.0 - 30.0).abs() < 0.01);
        assert!((normalize_lon(back.1 - 120.0)).abs() < 0.01);
    }

    #[test]
    fn segment_blocked_by_square_keepout() {
        let square = vec![(0.0, 0.0), (0.0, 1.0), (1.0, 1.0), (1.0, 0.0)];
        assert!(segment_intersects_polygon((0.5, -0.5), (0.5, 1.5), &square));
        assert!(!segment_intersects_polygon((2.0, 0.5), (2.0, 0.6), &square));
    }

    #[test]
    fn quantize_is_stable() {
        assert_eq!(quantize_deg(1.0), quantize_deg(1.00000004));
    }
}
