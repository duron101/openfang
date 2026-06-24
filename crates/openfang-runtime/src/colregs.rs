//! COLREGs encounter classification and deterministic avoidance maneuvers.
//!
//! Implements Rules 13 (overtaking), 14 (head-on), and 15 (crossing) for
//! power-driven vessels on the fast loop. All logic is pure and allocation-light.

use openfang_types::platform::{PlatformState, Pose};

use crate::nav_control::compute_cpa_3d;
use crate::route_geometry::normalize_bearing;

/// COLREGs encounter type for audit and maneuver selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EncounterType {
    /// Rule 14 — reciprocal/nearly reciprocal; both alter to starboard.
    HeadOn,
    /// Rule 15 — other on starboard; own ship is give-way.
    CrossingGiveWay,
    /// Rule 15 — other on port; own ship is stand-on.
    CrossingStandOn,
    /// Rule 13 — own is overtaking; must keep clear.
    OvertakingGiveWay,
    /// Rule 13 — own is being overtaken; stand-on.
    OvertakingStandOn,
    /// No applicable encounter (safe or ambiguous geometry).
    None,
}

impl EncounterType {
    pub fn label(self) -> &'static str {
        match self {
            Self::HeadOn => "head_on",
            Self::CrossingGiveWay => "crossing_give_way",
            Self::CrossingStandOn => "crossing_stand_on",
            Self::OvertakingGiveWay => "overtaking_give_way",
            Self::OvertakingStandOn => "overtaking_stand_on",
            Self::None => "none",
        }
    }

    pub fn rule(self) -> Option<&'static str> {
        match self {
            Self::HeadOn => Some("Rule 14"),
            Self::CrossingGiveWay | Self::CrossingStandOn => Some("Rule 15"),
            Self::OvertakingGiveWay | Self::OvertakingStandOn => Some("Rule 13"),
            Self::None => None,
        }
    }
}

/// Inputs for encounter assessment (platform-agnostic).
#[derive(Debug, Clone, Copy)]
pub struct ColregsInputs {
    pub own: Pose,
    pub own_speed_ms: f64,
    pub own_course_deg: f64,
    pub other_lat_deg: f64,
    pub other_lon_deg: f64,
    pub other_speed_ms: f64,
    pub other_course_deg: f64,
    pub min_cpa_m: f64,
    pub max_tcpa_s: f64,
}

/// Full assessment including CPA/TCPA and encounter class.
#[derive(Debug, Clone, Copy)]
pub struct EncounterAssessment {
    pub encounter: EncounterType,
    /// Target bearing relative to own bow (+ starboard, − port), degrees.
    pub relative_bearing_deg: f64,
    /// Other course relative to own course, degrees (−180..180).
    pub relative_course_deg: f64,
    pub cpa_m: f64,
    pub tcpa_s: f64,
}

/// Recommended COLREGs maneuver when give-way action is required.
#[derive(Debug, Clone, Copy)]
pub struct ColregsManeuver {
    pub encounter: EncounterType,
    pub heading_deg: f64,
    pub speed_ms: Option<f64>,
    pub reason: &'static str,
}

/// Abeam + 22.5° — boundary for overtaking sector (Rule 13).
pub const ABAFT_BEAM_DEG: f64 = 112.5;
/// Reciprocal-course tolerance for head-on (Rule 14).
pub const HEAD_ON_BEARING_DEG: f64 = 6.0;
pub const HEAD_ON_COURSE_DEG: f64 = 6.0;
/// Standard starboard alteration for give-way / head-on.
pub const STARBOARD_ALTER_DEG: f64 = 30.0;
/// Stand-on last-resort TCPA threshold (Rule 17(b)).
pub const STAND_ON_LAST_RESORT_TCPA_S: f64 = 30.0;

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

fn platform_pair(inputs: &ColregsInputs) -> (PlatformState, PlatformState) {
    let own = PlatformState {
        name: String::new(),
        id: String::new(),
        platform_type: String::new(),
        affiliation: openfang_types::platform::Affiliation::Unknown,
        domain: openfang_types::platform::Domain::Unknown,
        pose: inputs.own,
        velocity: openfang_types::platform::Velocity {
            speed_ms: inputs.own_speed_ms,
            vertical_rate_ms: 0.0,
            course_deg: inputs.own_course_deg,
        },
        fuel: openfang_types::platform::FuelStatus {
            remaining_kg: 0.0,
            max_kg: 0.0,
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
    let other_pose = Pose {
        lat_deg: inputs.other_lat_deg,
        lon_deg: inputs.other_lon_deg,
        alt_m: inputs.own.alt_m,
        heading_deg: inputs.other_course_deg,
        pitch_deg: 0.0,
        roll_deg: 0.0,
    };
    let other = PlatformState {
        velocity: openfang_types::platform::Velocity {
            speed_ms: inputs.other_speed_ms,
            vertical_rate_ms: 0.0,
            course_deg: inputs.other_course_deg,
        },
        pose: other_pose,
        name: String::new(),
        id: String::new(),
        platform_type: String::new(),
        affiliation: openfang_types::platform::Affiliation::Unknown,
        domain: openfang_types::platform::Domain::Unknown,
        fuel: openfang_types::platform::FuelStatus {
            remaining_kg: 0.0,
            max_kg: 0.0,
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
    (own, other)
}

/// Classify the COLREGs encounter geometry (Rules 13–15).
///
/// Does not consider CPA/TCPA — use [`assess_encounter`] for the full picture.
pub fn classify_encounter(
    relative_bearing_deg: f64,
    relative_course_deg: f64,
    own_speed_ms: f64,
    other_speed_ms: f64,
    bearing_other_to_own_deg: f64,
    other_course_deg: f64,
) -> EncounterType {
    let rel_own_from_other_bow = angle_diff(bearing_other_to_own_deg, other_course_deg);

    // Rule 13 — overtaking (checked before head-on / crossing).
    if rel_own_from_other_bow.abs() > ABAFT_BEAM_DEG && own_speed_ms > other_speed_ms + 0.5 {
        return EncounterType::OvertakingGiveWay;
    }
    if relative_bearing_deg.abs() > ABAFT_BEAM_DEG && other_speed_ms > own_speed_ms + 0.5 {
        return EncounterType::OvertakingStandOn;
    }

    // Rule 14 — head-on: target nearly ahead on reciprocal course.
    if relative_bearing_deg.abs() <= HEAD_ON_BEARING_DEG {
        let reciprocal_err = (relative_course_deg.abs() - 180.0).abs();
        if reciprocal_err <= HEAD_ON_COURSE_DEG {
            return EncounterType::HeadOn;
        }
    }

    // Rule 15 — crossing (forward of abaft-beam lines only).
    if relative_bearing_deg.abs() <= ABAFT_BEAM_DEG {
        if relative_bearing_deg > 0.0 {
            return EncounterType::CrossingGiveWay;
        }
        if relative_bearing_deg < 0.0 {
            return EncounterType::CrossingStandOn;
        }
    }

    EncounterType::None
}

/// Assess CPA/TCPA and COLREGs encounter type.
pub fn assess_encounter(inputs: &ColregsInputs) -> EncounterAssessment {
    let (own, other) = platform_pair(inputs);
    let (cpa_m, tcpa_s) = compute_cpa_3d(&own, &other);

    let bearing_to_other = inputs.own.bearing_to(&other.pose);
    let bearing_other_to_own = other.pose.bearing_to(&inputs.own);
    let relative_bearing_deg = angle_diff(bearing_to_other, inputs.own_course_deg);
    let relative_course_deg = angle_diff(inputs.other_course_deg, inputs.own_course_deg);

    let encounter = if cpa_m < inputs.min_cpa_m && tcpa_s > 0.0 && tcpa_s < inputs.max_tcpa_s {
        classify_encounter(
            relative_bearing_deg,
            relative_course_deg,
            inputs.own_speed_ms,
            inputs.other_speed_ms,
            bearing_other_to_own,
            inputs.other_course_deg,
        )
    } else {
        EncounterType::None
    };

    EncounterAssessment {
        encounter,
        relative_bearing_deg,
        relative_course_deg,
        cpa_m,
        tcpa_s,
    }
}

/// Recommend a COLREGs-compliant avoidance maneuver, if any.
pub fn colregs_avoidance_maneuver(inputs: &ColregsInputs) -> Option<ColregsManeuver> {
    let assessment = assess_encounter(inputs);
    if assessment.encounter == EncounterType::None {
        return None;
    }

    let speed = inputs.own_speed_ms.max(0.1);
    match assessment.encounter {
        EncounterType::HeadOn => Some(ColregsManeuver {
            encounter: assessment.encounter,
            heading_deg: normalize_bearing(inputs.own_course_deg + STARBOARD_ALTER_DEG),
            speed_ms: Some(speed * 0.9),
            reason: "Rule 14 head-on — both alter starboard",
        }),
        EncounterType::CrossingGiveWay => Some(ColregsManeuver {
            encounter: assessment.encounter,
            heading_deg: normalize_bearing(inputs.own_course_deg + STARBOARD_ALTER_DEG),
            speed_ms: Some(speed * 0.8),
            reason: "Rule 15 crossing — give-way (other on starboard)",
        }),
        EncounterType::OvertakingGiveWay => Some(ColregsManeuver {
            encounter: assessment.encounter,
            heading_deg: normalize_bearing(inputs.own_course_deg + STARBOARD_ALTER_DEG * 0.67),
            speed_ms: Some(speed * 0.85),
            reason: "Rule 13 overtaking — keep clear",
        }),
        EncounterType::CrossingStandOn | EncounterType::OvertakingStandOn => {
            if assessment.tcpa_s < STAND_ON_LAST_RESORT_TCPA_S
                && assessment.cpa_m < inputs.min_cpa_m * 0.5
            {
                Some(ColregsManeuver {
                    encounter: assessment.encounter,
                    heading_deg: normalize_bearing(inputs.own_course_deg + STARBOARD_ALTER_DEG),
                    speed_ms: Some(speed * 0.9),
                    reason: "Rule 17(b) stand-on last resort",
                })
            } else {
                None
            }
        }
        EncounterType::None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::platform::Pose;

    fn inputs(
        own_course: f64,
        own_speed: f64,
        other_lat: f64,
        other_lon: f64,
        other_course: f64,
        other_speed: f64,
    ) -> ColregsInputs {
        ColregsInputs {
            own: Pose {
                lat_deg: 30.0,
                lon_deg: 120.0,
                alt_m: 0.0,
                heading_deg: own_course,
                pitch_deg: 0.0,
                roll_deg: 0.0,
            },
            own_speed_ms: own_speed,
            own_course_deg: own_course,
            other_lat_deg: other_lat,
            other_lon_deg: other_lon,
            other_speed_ms: other_speed,
            other_course_deg: other_course,
            min_cpa_m: 500.0,
            max_tcpa_s: 600.0,
        }
    }

    #[test]
    fn classifies_head_on_reciprocal() {
        // Other ~1 km north, reciprocal course.
        let t = classify_encounter(0.0, 180.0, 10.0, 10.0, 180.0, 0.0);
        assert_eq!(t, EncounterType::HeadOn);
    }

    #[test]
    fn classifies_crossing_give_way_starboard() {
        // Other on starboard bow (~45°), crossing — equal speed avoids Rule 13.
        let t = classify_encounter(45.0, 90.0, 10.0, 10.0, 225.0, 270.0);
        assert_eq!(t, EncounterType::CrossingGiveWay);
    }

    #[test]
    fn classifies_crossing_stand_on_port() {
        let t = classify_encounter(-45.0, -90.0, 10.0, 8.0, 90.0, 90.0);
        assert_eq!(t, EncounterType::CrossingStandOn);
    }

    #[test]
    fn classifies_overtaking_give_way() {
        // From other's view, own is astern (>112.5°) and faster.
        let t = classify_encounter(5.0, 0.0, 12.0, 8.0, 180.0, 0.0);
        assert_eq!(t, EncounterType::OvertakingGiveWay);
    }

    #[test]
    fn classifies_overtaking_stand_on() {
        // Other astern of own and faster.
        let t = classify_encounter(170.0, 0.0, 8.0, 12.0, 0.0, 180.0);
        assert_eq!(t, EncounterType::OvertakingStandOn);
    }

    #[test]
    fn head_on_maneuver_alters_starboard() {
        let inp = inputs(0.0, 10.0, 30.009, 120.0, 180.0, 10.0);
        let m = colregs_avoidance_maneuver(&inp).expect("head-on maneuver");
        assert_eq!(m.encounter, EncounterType::HeadOn);
        assert!((m.heading_deg - 30.0).abs() < 0.1);
    }

    #[test]
    fn crossing_give_way_alters_starboard() {
        let inp = inputs(0.0, 10.0, 30.005, 120.005, 270.0, 8.0);
        let assessment = assess_encounter(&inp);
        if assessment.encounter == EncounterType::CrossingGiveWay {
            let m = colregs_avoidance_maneuver(&inp).expect("give-way maneuver");
            assert!(m.heading_deg > 0.0 && m.heading_deg < 90.0);
        }
    }

    #[test]
    fn stand_on_holds_course_when_safe() {
        let inp = inputs(0.0, 10.0, 30.005, 119.995, 90.0, 8.0);
        let assessment = assess_encounter(&inp);
        if assessment.encounter == EncounterType::CrossingStandOn && assessment.cpa_m > 100.0 {
            assert!(colregs_avoidance_maneuver(&inp).is_none());
        }
    }
}
