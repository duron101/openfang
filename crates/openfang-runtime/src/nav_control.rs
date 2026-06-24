//! Navigation Control — real-time path planning, collision avoidance, and
//! PlatformCommand generation. Pure Rust, microsecond-level response.
//!
//! # Architecture
//! - `NavController` takes a `WorldSnapshot` + navigation goal
//! - Computes heading/speed corrections
//! - Runs CPA collision avoidance against all tracks
//! - Outputs `Vec<PlatformCommand>` for the platform adapter

use openfang_types::platform::*;

/// Navigation goal — what the platform should navigate toward.
#[derive(Debug, Clone)]
pub enum NavGoal {
    /// Steer to a specific heading
    Heading {
        heading_deg: f64,
        speed_ms: Option<f64>,
    },
    /// Navigate to a specific LLA location
    Waypoint {
        lat: f64,
        lon: f64,
        alt_m: Option<f64>,
        speed_ms: Option<f64>,
    },
    /// Follow a sequence of waypoints
    Route { waypoints: Vec<Waypoint> },
    /// Hold position (loiter at current location)
    Loiter { radius_m: f64 },
}

/// Navigation controller — stateless computation engine.
pub struct NavController {
    /// Safety radius for collision avoidance (meters)
    safety_radius_m: f64,
    /// Minimum CPA distance before evasive action (meters)
    cpa_warning_m: f64,
    /// Maximum heading change per control cycle (degrees)
    max_heading_change_deg: f64,
    /// Maximum speed change per control cycle (m/s)
    max_speed_change_ms: f64,
    /// Own platform ID (skip self-tracks)
    own_platform_id: String,
}

/// Computed navigation output
#[derive(Debug, Clone)]
pub struct NavOutput {
    pub commands: Vec<PlatformCommand>,
    pub warnings: Vec<NavWarning>,
}

#[derive(Debug, Clone)]
pub enum NavWarning {
    CollisionRisk {
        track_id: String,
        cpa_m: f64,
        tcpa_s: f64,
    },
    GroundProximity {
        alt_m: f64,
        min_safe_alt_m: f64,
    },
    WaypointReached {
        waypoint_index: usize,
    },
    FuelLow {
        remaining_pct: f64,
    },
}

impl NavController {
    pub fn new(own_platform_id: String) -> Self {
        Self {
            safety_radius_m: 500.0,
            cpa_warning_m: 1000.0,
            max_heading_change_deg: 30.0,
            max_speed_change_ms: 10.0,
            own_platform_id,
        }
    }

    /// Compute navigation commands for a given goal and world state.
    pub fn compute(&self, goal: &NavGoal, snapshot: &WorldSnapshot) -> NavOutput {
        let ownship = match snapshot.find_platform(&self.own_platform_id) {
            Some(p) => p,
            None => {
                return NavOutput {
                    commands: vec![],
                    warnings: vec![],
                }
            }
        };

        match goal {
            NavGoal::Heading {
                heading_deg,
                speed_ms,
            } => self.compute_heading(ownship, *heading_deg, *speed_ms, snapshot),
            NavGoal::Waypoint {
                lat,
                lon,
                alt_m,
                speed_ms,
            } => self.compute_waypoint(ownship, *lat, *lon, *alt_m, *speed_ms, snapshot),
            NavGoal::Route { waypoints } => self.compute_route(ownship, waypoints, snapshot),
            NavGoal::Loiter { radius_m } => self.compute_loiter(ownship, *radius_m, snapshot),
        }
    }

    // ── Heading mode ──

    fn compute_heading(
        &self,
        ownship: &PlatformState,
        target_heading: f64,
        speed_ms: Option<f64>,
        snapshot: &WorldSnapshot,
    ) -> NavOutput {
        let mut commands = Vec::new();
        let mut warnings = Vec::new();

        // Clamp heading change
        let current_heading = ownship.pose.heading_deg;
        let heading_delta = angle_diff(target_heading, current_heading);
        let clamped_heading = if heading_delta.abs() > self.max_heading_change_deg {
            current_heading + heading_delta.signum() * self.max_heading_change_deg
        } else {
            target_heading
        };

        commands.push(PlatformCommand::SetHeading {
            platform_id: self.own_platform_id.clone(),
            heading_deg: normalize_angle(clamped_heading),
            speed_ms,
            turn_direction: None,
        });

        if let Some(spd) = speed_ms {
            let spd_delta = spd - ownship.velocity.speed_ms;
            if spd_delta.abs() > self.max_speed_change_ms {
                commands.push(PlatformCommand::SetSpeed {
                    platform_id: self.own_platform_id.clone(),
                    speed_ms: ownship.velocity.speed_ms
                        + spd_delta.signum() * self.max_speed_change_ms,
                    acceleration_ms2: None,
                });
            }
        }

        // Collision avoidance
        warnings.extend(self.check_collisions(ownship, snapshot));

        NavOutput { commands, warnings }
    }

    // ── Waypoint mode ──

    fn compute_waypoint(
        &self,
        ownship: &PlatformState,
        lat: f64,
        lon: f64,
        alt_m: Option<f64>,
        speed_ms: Option<f64>,
        snapshot: &WorldSnapshot,
    ) -> NavOutput {
        let wp = Pose {
            lat_deg: lat,
            lon_deg: lon,
            alt_m: alt_m.unwrap_or(ownship.pose.alt_m),
            heading_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
        };
        let dist = ownship.pose.distance_m(&wp);

        // Check if waypoint reached
        if dist < 100.0 {
            return NavOutput {
                commands: vec![],
                warnings: vec![NavWarning::WaypointReached { waypoint_index: 0 }],
            };
        }

        // Compute heading to waypoint
        let bearing = ownship.pose.bearing_to(&wp);

        // Use heading mode internally
        self.compute_heading(ownship, bearing, speed_ms, snapshot)
    }

    // ── Route mode ──

    fn compute_route(
        &self,
        ownship: &PlatformState,
        waypoints: &[Waypoint],
        snapshot: &WorldSnapshot,
    ) -> NavOutput {
        if waypoints.is_empty() {
            return NavOutput {
                commands: vec![],
                warnings: vec![],
            };
        }

        // Navigate toward first waypoint
        let wp = &waypoints[0];
        self.compute_waypoint(ownship, wp.lat, wp.lon, wp.alt, wp.speed_ms, snapshot)
    }

    // ── Loiter mode ──

    fn compute_loiter(
        &self,
        ownship: &PlatformState,
        radius_m: f64,
        snapshot: &WorldSnapshot,
    ) -> NavOutput {
        // Simple loiter: orbit current position with slow speed
        let mut commands = Vec::new();
        let mut warnings = Vec::new();

        // Slow down to loiter speed (~30% cruise)
        let loiter_speed = (ownship.velocity.speed_ms * 0.3).max(5.0);
        commands.push(PlatformCommand::SetSpeed {
            platform_id: self.own_platform_id.clone(),
            speed_ms: loiter_speed,
            acceleration_ms2: None,
        });

        // Rotate slowly (increment heading by 5 deg each cycle)
        let new_heading = normalize_angle(ownship.pose.heading_deg + 5.0);
        commands.push(PlatformCommand::SetHeading {
            platform_id: self.own_platform_id.clone(),
            heading_deg: new_heading,
            speed_ms: Some(loiter_speed),
            turn_direction: Some(TurnDirection::Left),
        });

        warnings.extend(self.check_collisions(ownship, snapshot));

        NavOutput { commands, warnings }
    }

    // ── Standoff Keeping ──

    /// Enforce a minimum standoff distance from a target pose. When the ownship
    /// is within `standoff_m` of the target, emit a back-off command (turn to the
    /// reciprocal of the bearing-to-target and reduce speed) so the platform
    /// opens range; otherwise return no correction. This is the navigation-level
    /// counterpart to the fire-gate standoff check — the fast loop calls it to
    /// hold a recon/observe orbit outside weapon-engagement range.
    ///
    /// Returns `(commands, breached)` where `breached` is true if the ownship is
    /// currently inside the standoff ring.
    pub fn standoff_correction(
        &self,
        target_lat: f64,
        target_lon: f64,
        target_alt_m: Option<f64>,
        standoff_m: f64,
        snapshot: &WorldSnapshot,
    ) -> (Vec<PlatformCommand>, bool) {
        let ownship = match snapshot.find_platform(&self.own_platform_id) {
            Some(p) => p,
            None => return (vec![], false),
        };
        let target = Pose {
            lat_deg: target_lat,
            lon_deg: target_lon,
            alt_m: target_alt_m.unwrap_or(ownship.pose.alt_m),
            heading_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
        };
        let dist = ownship.pose.distance_m(&target);
        if dist >= standoff_m {
            return (vec![], false);
        }

        // Inside the ring — open range: steer to the reciprocal of bearing-to-target.
        let bearing_to = ownship.pose.bearing_to(&target);
        let away = normalize_angle(bearing_to + 180.0);
        let current = ownship.pose.heading_deg;
        let delta = angle_diff(away, current);
        let clamped = if delta.abs() > self.max_heading_change_deg {
            normalize_angle(current + delta.signum() * self.max_heading_change_deg)
        } else {
            away
        };
        let loiter_speed = (ownship.velocity.speed_ms * 0.5).max(5.0);
        (
            vec![PlatformCommand::SetHeading {
                platform_id: self.own_platform_id.clone(),
                heading_deg: clamped,
                speed_ms: Some(loiter_speed),
                turn_direction: None,
            }],
            true,
        )
    }

    // ── Collision Avoidance ──

    fn check_collisions(
        &self,
        ownship: &PlatformState,
        snapshot: &WorldSnapshot,
    ) -> Vec<NavWarning> {
        let mut warnings = Vec::new();

        for platform in &snapshot.platforms {
            if platform.id == self.own_platform_id {
                continue;
            }

            let (cpa_m, tcpa_s) = compute_cpa_3d(ownship, platform);

            if cpa_m < self.cpa_warning_m && tcpa_s > 0.0 && tcpa_s < 300.0 {
                warnings.push(NavWarning::CollisionRisk {
                    track_id: platform.id.clone(),
                    cpa_m,
                    tcpa_s,
                });
            }
        }

        for track in &ownship.tracks {
            if track.speed_ms.unwrap_or(0.0) < 1.0 {
                continue;
            }

            let (cpa_m, tcpa_s) = compute_cpa_track(ownship, track);

            if cpa_m < self.cpa_warning_m && tcpa_s > 0.0 && tcpa_s < 120.0 {
                warnings.push(NavWarning::CollisionRisk {
                    track_id: track.track_id.clone(),
                    cpa_m,
                    tcpa_s,
                });
            }
        }

        warnings
    }
}

// ── Helper Functions ──

/// Compute Closest Point of Approach between two platforms.
/// Returns (CPA distance in meters, time to CPA in seconds).
fn compute_cpa(a: &PlatformState, b: &PlatformState) -> (f64, f64) {
    let dist = a.pose.distance_m(&b.pose);
    let bearing_a = a.pose.bearing_to(&b.pose);
    let bearing_b = b.pose.bearing_to(&a.pose);

    let va = a.velocity.speed_ms;
    let vb = b.velocity.speed_ms;
    let course_a = a.velocity.course_deg.to_radians();
    let course_b = b.velocity.course_deg.to_radians();

    let vrx = va * course_a.cos() - vb * course_b.cos();
    let vry = va * course_a.sin() - vb * course_b.sin();
    let vr = (vrx * vrx + vry * vry).sqrt();

    if vr < 0.1 {
        // Parallel paths — use current distance
        return (dist, 0.0);
    }

    // Project distance onto relative velocity
    let bearing_rad = bearing_a.to_radians();
    let dot = dist * (bearing_rad.cos() * vrx + bearing_rad.sin() * vry) / vr;
    let tcpa = -dot / vr;

    if tcpa < 0.0 {
        // Already past CPA
        return (dist, tcpa);
    }

    // CPA distance
    let cpa_x = -tcpa * vrx;
    let cpa_y = -tcpa * vry;
    let cpa = (cpa_x * cpa_x + cpa_y * cpa_y).sqrt();

    (cpa, tcpa)
}

/// Compute CPA between ownship and a track
fn compute_cpa_track(ownship: &PlatformState, track: &Track) -> (f64, f64) {
    let vo = ownship.velocity.speed_ms;
    let co = ownship.velocity.course_deg.to_radians();

    let vt = track.speed_ms.unwrap_or(0.0);
    let ct = track.heading_deg.unwrap_or(0.0).to_radians();

    let vrx = vo * co.cos() - vt * ct.cos();
    let vry = vo * co.sin() - vt * ct.sin();
    let vr = (vrx * vrx + vry * vry).sqrt();

    if vr < 0.1 {
        return (track.range_m.unwrap_or(99999.0), 0.0);
    }

    let bearing = track.bearing_deg.unwrap_or(0.0).to_radians();
    let dist = track.range_m.unwrap_or(99999.0);

    let dot = dist * (bearing.cos() * vrx + bearing.sin() * vry) / vr;
    let tcpa = -dot / vr;

    if tcpa < 0.0 {
        return (dist, tcpa);
    }

    let cpa_x = -tcpa * vrx;
    let cpa_y = -tcpa * vry;
    let cpa = (cpa_x * cpa_x + cpa_y * cpa_y).sqrt();

    (cpa, tcpa)
}

/// Compute the 3D Closest Point of Approach between two platforms, accounting
/// for altitude separation and vertical rate — required for the air domain
/// where two platforms can be horizontally converging yet safely stacked in
/// altitude (or vice versa).
///
/// Returns `(cpa_distance_m, tcpa_s)`. A negative `tcpa` means CPA already passed.
pub fn compute_cpa_3d(a: &PlatformState, b: &PlatformState) -> (f64, f64) {
    const R: f64 = 6_371_000.0;
    let mean_lat = a.pose.lat_deg.to_radians();

    // Relative position of b w.r.t a in local ENU (meters).
    let r_e = (b.pose.lon_deg - a.pose.lon_deg).to_radians() * mean_lat.cos() * R;
    let r_n = (b.pose.lat_deg - a.pose.lat_deg).to_radians() * R;
    let r_u = b.pose.alt_m - a.pose.alt_m;

    // Velocities in ENU (course: 0=north, clockwise).
    let (va, ca) = (a.velocity.speed_ms, a.velocity.course_deg.to_radians());
    let (vb, cb) = (b.velocity.speed_ms, b.velocity.course_deg.to_radians());
    let va_e = va * ca.sin();
    let va_n = va * ca.cos();
    let va_u = a.velocity.vertical_rate_ms;
    let vb_e = vb * cb.sin();
    let vb_n = vb * cb.cos();
    let vb_u = b.velocity.vertical_rate_ms;

    // Relative velocity of b w.r.t a.
    let rv_e = vb_e - va_e;
    let rv_n = vb_n - va_n;
    let rv_u = vb_u - va_u;

    let rv2 = rv_e * rv_e + rv_n * rv_n + rv_u * rv_u;
    let cur_dist = (r_e * r_e + r_n * r_n + r_u * r_u).sqrt();

    if rv2 < 1e-6 {
        // No relative motion — current separation is the CPA.
        return (cur_dist, 0.0);
    }

    let tcpa = -(r_e * rv_e + r_n * rv_n + r_u * rv_u) / rv2;
    if tcpa < 0.0 {
        return (cur_dist, tcpa);
    }

    let cx = r_e + rv_e * tcpa;
    let cy = r_n + rv_n * tcpa;
    let cz = r_u + rv_u * tcpa;
    let cpa = (cx * cx + cy * cy + cz * cz).sqrt();
    (cpa, tcpa)
}

/// Compute the smallest signed angle difference (-180..180).
fn angle_diff(target: f64, current: f64) -> f64 {
    let mut d = target - current;
    while d > 180.0 {
        d -= 360.0;
    }
    while d <= -180.0 {
        d += 360.0;
    }
    d
}

/// Normalize angle to [0, 360).
fn normalize_angle(a: f64) -> f64 {
    let mut r = a % 360.0;
    if r < 0.0 {
        r += 360.0;
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_snapshot() -> WorldSnapshot {
        WorldSnapshot {
            timestamp: 0.0,
            platforms: vec![PlatformState {
                id: "usv-01".into(),
                name: "TestUSV".into(),
                platform_type: "usv".into(),
                affiliation: Affiliation::Blue,
                domain: Domain::Surface,
                pose: Pose {
                    lat_deg: 30.0,
                    lon_deg: 120.0,
                    alt_m: 0.0,
                    heading_deg: 0.0,
                    pitch_deg: 0.0,
                    roll_deg: 0.0,
                },
                velocity: Velocity {
                    speed_ms: 10.0,
                    vertical_rate_ms: 0.0,
                    course_deg: 0.0,
                },
                fuel: FuelStatus {
                    remaining_kg: 500.0,
                    max_kg: 1000.0,
                    consumption_rate_kg_s: 0.01,
                },
                damage: 0.0,
                tracks: vec![],
                onboard_sensors: vec![],
                onboard_weapons: vec![],
                onboard_jammers: vec![],
                current_target: None,
                commander: None,
                survivability: None,
                emcon: None,
                link: None,
            }],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        }
    }

    fn air_platform(
        id: &str,
        lat: f64,
        lon: f64,
        alt: f64,
        course: f64,
        spd: f64,
        vrate: f64,
    ) -> PlatformState {
        PlatformState {
            id: id.into(),
            name: id.into(),
            platform_type: "cca".into(),
            affiliation: Affiliation::Blue,
            domain: Domain::Air,
            pose: Pose {
                lat_deg: lat,
                lon_deg: lon,
                alt_m: alt,
                heading_deg: course,
                pitch_deg: 0.0,
                roll_deg: 0.0,
            },
            velocity: Velocity {
                speed_ms: spd,
                vertical_rate_ms: vrate,
                course_deg: course,
            },
            fuel: FuelStatus {
                remaining_kg: 500.0,
                max_kg: 1000.0,
                consumption_rate_kg_s: 0.01,
            },
            damage: 0.0,
            tracks: vec![],
            onboard_sensors: vec![],
            onboard_weapons: vec![],
            onboard_jammers: vec![],
            current_target: None,
            commander: None,
            survivability: None,
            emcon: None,
            link: None,
        }
    }

    #[test]
    fn test_cpa_3d_altitude_separation_is_safe() {
        // Two platforms head-on horizontally but stacked 1000m apart in altitude.
        let a = air_platform("cca-1", 30.0, 120.0, 3000.0, 90.0, 100.0, 0.0);
        let b = air_platform("cca-2", 30.0, 120.05, 4000.0, 270.0, 100.0, 0.0);
        let (cpa, _tcpa) = compute_cpa_3d(&a, &b);
        // Even at horizontal merge, vertical 1000m keeps them clear.
        assert!(
            cpa >= 999.0,
            "expected >=1000m vertical clearance, got {cpa}"
        );
    }

    #[test]
    fn test_cpa_3d_converging_is_unsafe() {
        // Co-altitude, converging head-on → CPA near zero.
        let a = air_platform("cca-1", 30.0, 120.0, 3000.0, 90.0, 100.0, 0.0);
        let b = air_platform("cca-2", 30.0, 120.05, 3000.0, 270.0, 100.0, 0.0);
        let (cpa, tcpa) = compute_cpa_3d(&a, &b);
        assert!(cpa < 100.0, "expected near-zero CPA, got {cpa}");
        assert!(tcpa > 0.0, "expected positive tcpa, got {tcpa}");
    }

    #[test]
    fn test_heading_north() {
        let ctrl = NavController::new("usv-01".into());
        let snapshot = make_snapshot();
        let output = ctrl.compute(
            &NavGoal::Heading {
                heading_deg: 0.0,
                speed_ms: None,
            },
            &snapshot,
        );
        assert_eq!(output.commands.len(), 1);
        match &output.commands[0] {
            PlatformCommand::SetHeading { heading_deg, .. } => {
                assert!((*heading_deg - 0.0).abs() < 0.1);
            }
            _ => panic!("Expected SetHeading"),
        }
    }

    #[test]
    fn test_heading_change_clamped() {
        let ctrl = NavController::new("usv-01".into());
        let snapshot = make_snapshot();
        // Heading=0, target=180 — should clamp to 30
        let output = ctrl.compute(
            &NavGoal::Heading {
                heading_deg: 180.0,
                speed_ms: None,
            },
            &snapshot,
        );
        match &output.commands[0] {
            PlatformCommand::SetHeading { heading_deg, .. } => {
                assert!(
                    (*heading_deg - 30.0).abs() < 0.1,
                    "Expected 30, got {}",
                    heading_deg
                );
            }
            _ => panic!("Expected SetHeading"),
        }
    }

    #[test]
    fn test_waypoint_bearing() {
        let ctrl = NavController::new("usv-01".into());
        let snapshot = make_snapshot();
        // USV at (30,120), waypoint at (30.001,120) — bearing north
        let output = ctrl.compute(
            &NavGoal::Waypoint {
                lat: 30.001,
                lon: 120.0,
                alt_m: None,
                speed_ms: None,
            },
            &snapshot,
        );
        assert!(!output.commands.is_empty());
    }

    #[test]
    fn test_standoff_correction_inside_ring_backs_off() {
        // USV at (30,120). Target ~96m north (0.001 deg lat ≈ 111m). standoff 3000m.
        let ctrl = NavController::new("usv-01".into());
        let snapshot = make_snapshot();
        let (cmds, breached) = ctrl.standoff_correction(30.001, 120.0, None, 3000.0, &snapshot);
        assert!(breached, "should be inside standoff ring");
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            PlatformCommand::SetHeading { heading_deg, .. } => {
                // Bearing to target is ~0 (north); back-off heading should turn
                // toward south (~180), clamped by max heading change from 0 → 30.
                assert!(
                    (*heading_deg - 30.0).abs() < 0.5,
                    "expected clamped turn toward reciprocal, got {heading_deg}"
                );
            }
            other => panic!("expected SetHeading, got {other:?}"),
        }
    }

    #[test]
    fn test_standoff_correction_outside_ring_no_op() {
        let ctrl = NavController::new("usv-01".into());
        let snapshot = make_snapshot();
        // Target far away (~11km north) with a small standoff → no correction.
        let (cmds, breached) = ctrl.standoff_correction(30.1, 120.0, None, 3000.0, &snapshot);
        assert!(!breached, "should be outside standoff ring");
        assert!(cmds.is_empty());
    }

    #[test]
    fn test_cpa_calculation() {
        let a = PlatformState {
            id: "a".into(),
            name: "A".into(),
            platform_type: "ship".into(),
            affiliation: Affiliation::Blue,
            domain: Domain::Surface,
            pose: Pose {
                lat_deg: 30.0,
                lon_deg: 120.0,
                alt_m: 0.0,
                heading_deg: 0.0,
                pitch_deg: 0.0,
                roll_deg: 0.0,
            },
            velocity: Velocity {
                speed_ms: 10.0,
                vertical_rate_ms: 0.0,
                course_deg: 90.0,
            },
            fuel: FuelStatus {
                remaining_kg: 100.0,
                max_kg: 100.0,
                consumption_rate_kg_s: 0.0,
            },
            damage: 0.0,
            tracks: vec![],
            onboard_sensors: vec![],
            onboard_weapons: vec![],
            onboard_jammers: vec![],
            current_target: None,
            commander: None,
            survivability: None,
            emcon: None,
            link: None,
        };
        let b = PlatformState {
            id: "b".into(),
            name: "B".into(),
            platform_type: "ship".into(),
            affiliation: Affiliation::Red,
            domain: Domain::Surface,
            pose: Pose {
                lat_deg: 30.0,
                lon_deg: 120.001,
                alt_m: 0.0,
                heading_deg: 0.0,
                pitch_deg: 0.0,
                roll_deg: 0.0,
            },
            velocity: Velocity {
                speed_ms: 10.0,
                vertical_rate_ms: 0.0,
                course_deg: 270.0,
            },
            fuel: FuelStatus {
                remaining_kg: 100.0,
                max_kg: 100.0,
                consumption_rate_kg_s: 0.0,
            },
            damage: 0.0,
            tracks: vec![],
            onboard_sensors: vec![],
            onboard_weapons: vec![],
            onboard_jammers: vec![],
            current_target: None,
            commander: None,
            survivability: None,
            emcon: None,
            link: None,
        };

        let (cpa, _tcpa) = compute_cpa(&a, &b);
        // Two ships heading toward each other on same latitude → CPA should be small
        assert!(cpa < 100.0, "CPA should be small for head-on, got {cpa}");
    }
}
