//! Phase 1 contract-equivalence test.
//!
//! Proves that the *same* golden world, produced by the ArkSim backend
//! (SimState → WorldSnapshot mapping) and by the protocol-agnostic Mock
//! backend, normalizes to a semantically equivalent [`WorldSnapshot`]. This is
//! the guarantee the upper layers rely on: simulation and hardware backends are
//! contract-equivalent, not byte-identical.

use openfang_platform::{snapshots_equivalent, EquivalenceTolerance};
use openfang_platform_arksim::proto_manual::{SimPlatform, SimState};
use openfang_platform_arksim::state_mapper::from_sim_state;
use openfang_types::platform::*;

fn golden_mock_snapshot() -> WorldSnapshot {
    WorldSnapshot {
        timestamp: 42.0,
        platforms: vec![PlatformState {
            id: "Blue-Lead".into(),
            name: "Blue-Lead".into(),
            platform_type: "aircraft".into(),
            affiliation: Affiliation::Blue,
            domain: Domain::Air,
            pose: Pose {
                lat_deg: 30.0,
                lon_deg: 120.0,
                alt_m: 5000.0,
                heading_deg: 90.0,
                pitch_deg: 0.0,
                roll_deg: 0.0,
            },
            velocity: Velocity {
                speed_ms: 100.0,
                vertical_rate_ms: 0.0,
                course_deg: 0.0,
            },
            fuel: FuelStatus {
                remaining_kg: 800.0,
                max_kg: 1000.0,
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
        }],
        active_munitions: vec![],
        events: vec![],
        fleet: None,
    }
}

fn golden_arksim_snapshot() -> WorldSnapshot {
    // Same world, expressed in ArkSim's native SimState representation.
    let p = SimPlatform {
        name: "Blue-Lead".into(),
        side: "Blue1".into(),
        domain: "air".into(),
        lat: 30.0,
        lon: 120.0,
        alt: 5000.0,
        heading_rad: std::f64::consts::FRAC_PI_2, // 90°
        vn_ms: 100.0,                             // due-north velocity → speed 100, course 0°
        ve_ms: 0.0,
        fuel: 800.0,
        max_fuel: 1000.0,
        ..Default::default()
    };
    from_sim_state(&SimState {
        time: 42.0,
        platforms: vec![p],
        ..Default::default()
    })
}

#[test]
fn arksim_and_mock_are_contract_equivalent() {
    let mock = golden_mock_snapshot();
    let arksim = golden_arksim_snapshot();
    let result = snapshots_equivalent(&mock, &arksim, EquivalenceTolerance::default());
    assert!(result.is_ok(), "backends diverged: {:?}", result.err());
}

#[test]
fn divergent_position_is_detected() {
    let mock = golden_mock_snapshot();
    let mut arksim = golden_arksim_snapshot();
    arksim.platforms[0].pose.lat_deg = 31.0; // 1° off
    let result = snapshots_equivalent(&mock, &arksim, EquivalenceTolerance::default());
    assert!(result.is_err(), "should have detected position divergence");
}
