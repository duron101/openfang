//! Config-driven named geographic zones for MMS semantic routing.

use std::collections::HashMap;

use openfang_types::config::GeoZoneConfig;
use openfang_types::platform::{Pose, TurnDirection};
use openfang_types::route::{PlanGoal, PlanRequest, PlatformKinematics};
use serde::{Deserialize, Serialize};

use crate::route_geometry::polygon_centroid;
use crate::route_planner::patrol_racetrack;

/// Semantic navigation intent extracted from commander text + zone registry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NavIntent {
    GotoZone {
        zone: String,
    },
    Patrol {
        zone: String,
        pattern: Option<String>,
    },
    Transit {
        lane: String,
    },
    Standoff {
        track_label: String,
        range_m: f64,
    },
    FlankStandoff {
        track_label: String,
        range_m: f64,
        turn_direction: Option<TurnDirection>,
    },
    DirectManeuver {
        heading_deg: Option<f64>,
        heading_delta_deg: Option<f64>,
        turn_direction: Option<TurnDirection>,
        speed_ms: Option<f64>,
    },
    Avoid {
        zone: String,
    },
}

/// Resolved geographic zone.
#[derive(Debug, Clone)]
pub struct GeoZone {
    pub id: String,
    pub kind: GeoZoneKind,
    pub polygon: Vec<(f64, f64)>,
    pub point: Option<(f64, f64)>,
    pub alt_min_m: f64,
    pub alt_max_m: f64,
    pub patrol_pattern: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeoZoneKind {
    Patrol,
    Area,
    Waypoint,
    Lane,
    Keepout,
}

impl GeoZoneKind {
    fn from_config(s: &str) -> Self {
        match s {
            "patrol" => Self::Patrol,
            "area" => Self::Area,
            "waypoint" => Self::Waypoint,
            "lane" => Self::Lane,
            "keepout" => Self::Keepout,
            _ => Self::Area,
        }
    }
}

/// Registry of named zones loaded from `[platform.geo_zones]`.
#[derive(Debug, Default, Clone)]
pub struct GeoZoneRegistry {
    by_id: HashMap<String, GeoZone>,
    alias_map: HashMap<String, String>,
}

impl GeoZoneRegistry {
    pub fn from_config(zones: &[GeoZoneConfig]) -> Self {
        let mut reg = Self::default();
        for z in zones {
            let id = z.id.trim().to_string();
            if id.is_empty() {
                continue;
            }
            let alt_band = z.effective_alt_band();
            let zone = GeoZone {
                id: id.clone(),
                kind: GeoZoneKind::from_config(&z.kind),
                polygon: z.polygon.clone(),
                point: z.point,
                alt_min_m: alt_band.0,
                alt_max_m: alt_band.1,
                patrol_pattern: z.patrol_pattern.clone(),
            };
            reg.by_id.insert(id.clone(), zone);
            for alias in &z.aliases {
                let a = alias.trim().to_lowercase();
                if !a.is_empty() {
                    reg.alias_map.insert(a, id.clone());
                }
            }
            reg.alias_map.insert(id.to_lowercase(), id);
        }
        reg
    }

    pub fn resolve(&self, name_or_alias: &str) -> Option<&GeoZone> {
        let key = name_or_alias.trim().to_lowercase();
        self.alias_map
            .get(&key)
            .and_then(|id| self.by_id.get(id))
            .or_else(|| self.by_id.get(name_or_alias.trim()))
    }

    pub fn zones(&self) -> impl Iterator<Item = &GeoZone> {
        self.by_id.values()
    }

    /// Aliases ordered deterministically: longest (most specific) first, then
    /// lexicographic. Avoids HashMap-iteration nondeterminism when several
    /// aliases match the same objective text.
    pub fn sorted_aliases(&self) -> Vec<(&str, &str)> {
        let mut out: Vec<(&str, &str)> = self
            .alias_map
            .iter()
            .map(|(a, id)| (a.as_str(), id.as_str()))
            .collect();
        out.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(b.0)));
        out
    }

    /// Compile a [`NavIntent`] + own pose into a [`PlanRequest`].
    pub fn compile_plan_request(
        &self,
        intent: &NavIntent,
        start: Pose,
        kinematics: PlatformKinematics,
        cruise_alt_m: Option<f64>,
        keepouts: Vec<openfang_types::route::KeepOutPrism>,
        threat_weight: f64,
    ) -> Option<PlanRequest> {
        let cruise = cruise_alt_m.unwrap_or(start.alt_m);
        let goal = match intent {
            NavIntent::GotoZone { zone } => {
                let z = self.resolve(zone)?;
                let (lat, lon) = z.point.or_else(|| polygon_centroid(&z.polygon))?;
                PlanGoal::Point {
                    lat_deg: lat,
                    lon_deg: lon,
                    alt_m: cruise,
                }
            }
            NavIntent::Patrol { zone, .. } => PlanGoal::Patrol {
                zone_id: zone.clone(),
            },
            NavIntent::Transit { lane } => {
                let z = self.resolve(lane)?;
                let (lat, lon) = z.point.or_else(|| polygon_centroid(&z.polygon))?;
                PlanGoal::ZoneCenter {
                    lat_deg: lat,
                    lon_deg: lon,
                    alt_m: cruise,
                }
            }
            NavIntent::Standoff {
                track_label,
                range_m,
            }
            | NavIntent::FlankStandoff {
                track_label,
                range_m,
                ..
            } => PlanGoal::Standoff {
                track_id: track_label.clone(),
                range_m: *range_m,
            },
            NavIntent::DirectManeuver { .. } => return None,
            NavIntent::Avoid { .. } => return None,
        };
        Some(PlanRequest {
            start,
            goal,
            kinematics,
            keepouts,
            dynamic_obstacles: vec![],
            cruise_alt_m,
            threat_avoid_weight: threat_weight,
        })
    }

    pub fn patrol_waypoints(
        &self,
        zone_id: &str,
        cruise_alt_m: f64,
        speed_ms: f64,
    ) -> Vec<openfang_types::platform::Waypoint> {
        let Some(z) = self.resolve(zone_id) else {
            return Vec::new();
        };
        patrol_racetrack(&z.polygon, cruise_alt_m, speed_ms, 1)
    }
}

/// Deterministic NavIntent extraction from commander objective text.
pub fn extract_nav_intent(text: &str, registry: &GeoZoneRegistry) -> Option<NavIntent> {
    let lower = text.to_lowercase();
    let is_patrol = lower.contains("patrol")
        || lower.contains("巡逻")
        || lower.contains("巡航")
        || lower.contains("cap");

    let aliases = registry.sorted_aliases();

    if let Some(maneuver) = parse_direct_maneuver(&lower) {
        return Some(maneuver);
    }

    if lower.contains("standoff") || lower.contains("安全距离") || lower.contains("保持距离")
    {
        if let Some(range_m) = parse_standoff_m(&lower) {
            for (alias, id) in &aliases {
                if lower.contains(alias) {
                    return Some(NavIntent::Standoff {
                        track_label: (*id).to_string(),
                        range_m,
                    });
                }
            }
            return Some(NavIntent::Standoff {
                track_label: "primary".into(),
                range_m,
            });
        }
    }

    for (alias, id) in &aliases {
        if lower.contains(alias) {
            if is_patrol {
                return Some(NavIntent::Patrol {
                    zone: (*id).to_string(),
                    pattern: Some("racetrack".into()),
                });
            }
            return Some(NavIntent::GotoZone {
                zone: (*id).to_string(),
            });
        }
    }
    None
}

fn parse_direct_maneuver(lower: &str) -> Option<NavIntent> {
    let turn_direction = parse_turn_direction(lower);
    let heading_deg = parse_absolute_heading_deg(lower);
    let mut heading_delta_deg = parse_heading_delta_deg(lower);
    let speed_ms = parse_speed_ms(lower);

    if turn_direction.is_some() && heading_deg.is_none() && heading_delta_deg.is_none() {
        heading_delta_deg = Some(match turn_direction {
            Some(TurnDirection::Left) => -90.0,
            Some(TurnDirection::Right) => 90.0,
            _ => 90.0,
        });
    }

    if heading_deg.is_none()
        && heading_delta_deg.is_none()
        && turn_direction.is_none()
        && speed_ms.is_none()
    {
        return None;
    }

    Some(NavIntent::DirectManeuver {
        heading_deg,
        heading_delta_deg,
        turn_direction,
        speed_ms,
    })
}

fn parse_turn_direction(lower: &str) -> Option<TurnDirection> {
    if lower.contains("左转")
        || lower.contains("向左")
        || lower.contains("turn left")
        || lower.contains("left turn")
        || lower.contains("port turn")
    {
        Some(TurnDirection::Left)
    } else if lower.contains("右转")
        || lower.contains("向右")
        || lower.contains("turn right")
        || lower.contains("right turn")
        || lower.contains("starboard turn")
    {
        Some(TurnDirection::Right)
    } else {
        None
    }
}

fn parse_absolute_heading_deg(lower: &str) -> Option<f64> {
    let re = regex_lite::Regex::new(
        r"(?:航向|heading|course|转向)\s*(\d+(?:\.\d+)?)\s*(?:度|°|deg|degrees)?",
    )
    .ok()?;
    let caps = re.captures(lower)?;
    let deg: f64 = caps.get(1)?.as_str().parse().ok()?;
    Some(normalize_heading_deg(deg))
}

fn parse_heading_delta_deg(lower: &str) -> Option<f64> {
    let turn = parse_turn_direction(lower)?;
    let re = regex_lite::Regex::new(
        r"(?:左转|右转|left turn|right turn|turn left|turn right|转)\s*(\d+(?:\.\d+)?)\s*(?:度|°|deg|degrees)?",
    )
    .ok()?;
    let caps = re.captures(lower)?;
    let deg: f64 = caps.get(1)?.as_str().parse().ok()?;
    Some(match turn {
        TurnDirection::Left => -deg.abs(),
        TurnDirection::Right => deg.abs(),
        TurnDirection::Shortest => deg,
    })
}

fn parse_speed_ms(lower: &str) -> Option<f64> {
    if lower.contains("停止")
        || lower.contains("停船")
        || lower.contains("全停")
        || lower.contains("stop")
        || lower.contains("halt")
    {
        return Some(0.0);
    }
    if lower.contains("全速")
        || lower.contains("最快")
        || lower.contains("最大速度")
        || lower.contains("full speed")
        || lower.contains("max speed")
        || lower.contains("flank speed")
    {
        return Some(15.0);
    }
    if let Ok(re) = regex_lite::Regex::new(r"(\d+(?:\.\d+)?)\s*(?:节|knots?|kn)\b") {
        if let Some(caps) = re.captures(lower) {
            if let Ok(knots) = caps.get(1)?.as_str().parse::<f64>() {
                return Some(knots * 0.514444);
            }
        }
    }
    if let Ok(re) = regex_lite::Regex::new(
        r"(?:速度|speed|航速|速率)\s*(\d+(?:\.\d+)?)\s*(米每秒|米/?秒|m/?s|节|knots?|kn)?",
    ) {
        if let Some(caps) = re.captures(lower) {
            if let Ok(v) = caps.get(1)?.as_str().parse::<f64>() {
                let unit = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                return Some(
                    if unit.contains('节') || unit.starts_with("knot") || unit == "kn" {
                        v * 0.514444
                    } else {
                        v
                    },
                );
            }
        }
    }
    if lower.contains("加速") || lower.contains("提速") || lower.contains("speed up") {
        return Some(10.0);
    }
    if lower.contains("减速") || lower.contains("慢下来") || lower.contains("slow down") {
        return Some(3.0);
    }
    None
}

fn normalize_heading_deg(deg: f64) -> f64 {
    let mut h = deg % 360.0;
    if h < 0.0 {
        h += 360.0;
    }
    h
}

fn parse_standoff_m(lower: &str) -> Option<f64> {
    let re = regex_lite::Regex::new(
        r"(\d+(?:\.\d+)?)\s*(公里|千米|km|kilometers?|米|metres?|meters?|m)\b",
    )
    .ok()?;
    let caps = re.captures(lower)?;
    let value: f64 = caps.get(1)?.as_str().parse().ok()?;
    let unit = caps.get(2)?.as_str();
    Some(
        if matches!(unit, "公里" | "千米" | "km") || unit.starts_with("kilom") {
            value * 1000.0
        } else {
            value
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::config::GeoZoneConfig;

    fn registry() -> GeoZoneRegistry {
        GeoZoneRegistry::from_config(&[GeoZoneConfig {
            id: "sector_north".into(),
            kind: "patrol".into(),
            aliases: vec!["北部扇区".into(), "north sector".into()],
            polygon: vec![(30.0, 120.0), (30.0, 120.1), (30.1, 120.1), (30.1, 120.0)],
            point: None,
            alt_band_m: [0.0, 500.0],
            patrol_pattern: Some("racetrack".into()),
        }])
    }

    #[test]
    fn resolves_alias_to_zone() {
        let reg = registry();
        assert!(reg.resolve("北部扇区").is_some());
        assert_eq!(reg.resolve("north sector").unwrap().id, "sector_north");
    }

    #[test]
    fn extracts_patrol_intent() {
        let reg = registry();
        let intent = extract_nav_intent("巡逻北部扇区", &reg).unwrap();
        assert!(matches!(intent, NavIntent::Patrol { .. }));
    }
}
