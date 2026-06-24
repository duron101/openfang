//! Track 2 (mothership) end-to-end test.
//!
//! Drives the fleet picture carried on `WorldSnapshot.fleet` through the
//! [`FleetManager`] and verifies the corrective commands it proposes: recall a
//! low-fuel child and re-assign a lost child's mission to an available sibling.

use openfang_runtime::fleet_manager::FleetManager;
use openfang_types::platform::{
    FleetSnapshot, PlatformCommand, UavMission, UavState, UavStatus, WorldSnapshot,
};

fn child(id: &str, status: UavStatus, fuel: f64, since_contact: f64) -> UavState {
    UavState {
        uav_id: id.into(),
        uav_type: "cca".into(),
        status,
        fuel_pct: fuel,
        seconds_since_contact: since_contact,
        mission: None,
    }
}

#[test]
fn fleet_snapshot_drives_corrective_commands() {
    // A world poll that carries a mothership fleet picture.
    let mut lost = child("cca-lost", UavStatus::Lost, 0.0, 99.0);
    lost.mission = Some(UavMission {
        mission_id: "m-strike".into(),
        mission_type: "strike".into(),
        role: None,
        params_json: "{\"tgt\":\"trk-7\"}".into(),
        target_track_id: Some("trk-7".into()),
    });

    let snapshot = WorldSnapshot {
        timestamp: 10.0,
        platforms: vec![],
        active_munitions: vec![],
        events: vec![],
        fleet: Some(FleetSnapshot {
            mothership_id: "ms-1".into(),
            uavs: vec![
                child("cca-low", UavStatus::OnMission, 0.05, 1.0), // low fuel → recall
                lost,                                              // lost w/ mission → reassign
                child("cca-free", UavStatus::Airborne, 0.9, 1.0),  // available sibling
            ],
        }),
    };

    let fm = FleetManager::new("ms-1");
    fm.ingest(snapshot.fleet.expect("snapshot carries fleet"));

    let actions = fm.evaluate();

    // Low-fuel child recalled.
    assert!(
        actions.iter().any(|a| matches!(
            &a.command,
            PlatformCommand::ReturnToBase { uav_id } if uav_id == "cca-low"
        )),
        "low-fuel child should be recalled: {actions:?}"
    );

    // Lost child's mission re-assigned to the available sibling.
    assert!(
        actions.iter().any(|a| matches!(
            &a.command,
            PlatformCommand::AssignMission { uav_id, mission_type, .. }
                if uav_id == "cca-free" && mission_type == "strike"
        )),
        "orphaned mission should move to the free sibling: {actions:?}"
    );

    // Post-state: free sibling now on mission, low-fuel child returning.
    let after = fm.snapshot();
    assert_eq!(after.get("cca-free").unwrap().status, UavStatus::OnMission);
    assert_eq!(after.get("cca-low").unwrap().status, UavStatus::Returning);
}
