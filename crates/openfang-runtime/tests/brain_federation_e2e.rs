//! Brain ↔ cerebellum ↔ federation end-to-end test.
//!
//! Validates the upgraded slow-loop architecture without a live daemon:
//! - **own scope**: a `ThreatEmitter` situation fires the `ElectronicAttack`
//!   workflow, which maps to the own-platform `EwJamming` role, whose posture is
//!   gated and fanned out through the cerebellum domain lanes.
//! - **formation scope**: on a `lead`, a `SEAD` command allocates capability-
//!   gated member roles (an unarmed LSUAV is never assigned the jammer role).
//! - **degradation**: a member that loses the link self-arbitrates to a safe,
//!   weapons-safe role — no single point of failure.

use std::sync::Arc;

use openfang_runtime::cca_role::posture_for;
use openfang_runtime::cerebellum::{Cerebellum, LaneKind};
use openfang_runtime::fleet_manager::{degraded_role, FleetManager, MemberCapability};
use openfang_runtime::op_restrictions::OpRestrictionsManager;
use openfang_runtime::workflow_trigger::{workflow_to_role, WorkflowTriggerManager};
use openfang_types::cognition::{OwnForceStatus, SituationAssessment, ThreatTrack};
use openfang_types::config::{
    FleetRole, WorkflowConfig, WorkflowScope, WorkflowTriggerConfig, WorkflowTriggerKind,
};
use openfang_types::platform::{
    Affiliation, CcaRole, Domain, FuelStatus, JammerState, PlatformCapabilities, PlatformCommand,
    PlatformState, Pose, SensorState, SensorType, UavState, UavStatus, Velocity, WeaponState,
};
use openfang_types::umaa::{PlatformLimits, RulesOfEngagement};

fn threat_emitter_assessment() -> SituationAssessment {
    SituationAssessment {
        timestamp: 1.0,
        threats: vec![ThreatTrack {
            track_id: "emitter-1".into(),
            platform_type: "sam".into(),
            distance_m: 30_000.0,
            closing_rate_ms: 0.0,
            threat_score: 0.92,
        }],
        opportunities: Vec::new(),
        own_force: OwnForceStatus {
            total_platforms: 1,
            average_damage: 0.0,
            average_fuel_pct: 0.9,
            link_status: "nominal".into(),
        },
        summary: "active SAM emitter".into(),
    }
}

fn ea_config() -> WorkflowConfig {
    WorkflowConfig {
        enabled: true,
        definitions_path: None,
        triggers: vec![WorkflowTriggerConfig {
            workflow: "ElectronicAttack".into(),
            scope: WorkflowScope::Own,
            trigger: WorkflowTriggerKind::Event,
            interval_secs: 0.0,
            event: Some("ThreatEmitter".into()),
            command: None,
            enabled: true,
        }],
    }
}

fn cca_self(id: &str) -> PlatformState {
    PlatformState {
        id: id.into(),
        name: id.into(),
        platform_type: "cca".into(),
        affiliation: Affiliation::Blue,
        domain: Domain::Air,
        pose: Pose {
            lat_deg: 30.0,
            lon_deg: 120.0,
            alt_m: 3000.0,
            heading_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
        },
        velocity: Velocity {
            speed_ms: 200.0,
            vertical_rate_ms: 0.0,
            course_deg: 0.0,
        },
        fuel: FuelStatus {
            remaining_kg: 500.0,
            max_kg: 1000.0,
            consumption_rate_kg_s: 0.05,
        },
        damage: 0.0,
        tracks: vec![],
        onboard_sensors: vec![SensorState {
            sensor_id: "radar-1".into(),
            sensor_type: SensorType::Radar,
            mode: "passive".into(),
            frequency_hz: None,
            bandwidth_hz: None,
            azimuth_fov_deg: None,
            elevation_fov_deg: None,
            range_max_m: None,
            damage: 0.0,
            host_platform_id: id.into(),
        }],
        onboard_weapons: vec![WeaponState {
            weapon_id: "aam-1".into(),
            weapon_type: "aam".into(),
            quantity_remaining: 4.0,
            max_range_m: None,
            min_range_m: None,
            guidance_type: None,
            speed_ms: None,
            is_ready: true,
            quantity_from_snapshot: true,
        }],
        onboard_jammers: vec![JammerState {
            jammer_id: "jam-1".into(),
            host_id: id.into(),
            is_active: false,
            beams: vec![],
        }],
        current_target: None,
        commander: None,
        survivability: None,
        emcon: None,
        link: None,
    }
}

fn full_caps() -> PlatformCapabilities {
    PlatformCapabilities {
        supports_motion_control: true,
        supports_sensor_control: true,
        supports_weapon_control: true,
        supports_jammer_control: true,
        supports_comm_control: true,
        ..PlatformCapabilities::default()
    }
}

fn typed_uav(id: &str, uav_type: &str) -> UavState {
    UavState {
        uav_id: id.into(),
        uav_type: uav_type.into(),
        status: UavStatus::Airborne,
        fuel_pct: 0.9,
        seconds_since_contact: 1.0,
        mission: None,
    }
}

#[test]
fn own_scope_threat_emitter_drives_ew_jamming_posture() {
    // Brain: ThreatEmitter event fires ElectronicAttack (own scope).
    let mut brain = WorkflowTriggerManager::new(&ea_config(), FleetRole::Standalone);
    let assessment = threat_emitter_assessment();
    let fired = brain.evaluate(assessment.timestamp, &assessment, None);
    assert_eq!(fired.len(), 1, "ElectronicAttack should fire");
    assert_eq!(fired[0].workflow, "ElectronicAttack");

    // Brain → role: ElectronicAttack maps to the own EwJamming role.
    let role = workflow_to_role(&fired[0].workflow).expect("role mapping");
    assert_eq!(role, CcaRole::EwJamming);

    // Brain → cerebellum: fan posture out; the EW lane keeps the jammer up while
    // the (jammer-capable) platform's posture enforces radar/comm/weapon slices.
    let restrictions = Arc::new(OpRestrictionsManager::new(
        RulesOfEngagement::default(),
        PlatformLimits::default(),
    ));
    let mut cer = Cerebellum::new(20.0, 64, restrictions);
    cer.set_capabilities(full_caps());
    cer.set_posture(posture_for(role));
    let intents = cer.posture_intents(&cca_self("cca-1"), assessment.timestamp);
    // EwJamming must NOT stop the jammer.
    assert!(
        !intents
            .iter()
            .any(|i| matches!(i.command, PlatformCommand::JamStop { .. })),
        "EwJamming posture must keep the jammer active"
    );
    // The EW lane is live on a jammer-capable platform.
    let ew = cer
        .lane_statuses()
        .into_iter()
        .find(|s| s.kind == LaneKind::Ew)
        .unwrap();
    assert!(ew.enabled);
}

#[test]
fn formation_scope_sead_allocates_capability_gated_roles() {
    let cfg = WorkflowConfig {
        enabled: true,
        definitions_path: None,
        triggers: vec![WorkflowTriggerConfig {
            workflow: "SEAD".into(),
            scope: WorkflowScope::Formation,
            trigger: WorkflowTriggerKind::Command,
            interval_secs: 0.0,
            event: None,
            command: Some("sead".into()),
            enabled: true,
        }],
    };
    let assessment = threat_emitter_assessment();

    // A member node may NOT originate a formation workflow.
    let mut member = WorkflowTriggerManager::new(&cfg, FleetRole::Member);
    assert!(member.evaluate(0.0, &assessment, Some("sead")).is_empty());

    // The lead fires SEAD and allocates capability-gated member roles.
    let mut lead = WorkflowTriggerManager::new(&cfg, FleetRole::Lead);
    let fired = lead.evaluate(0.0, &assessment, Some("sead"));
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].scope, WorkflowScope::Formation);

    let fm = FleetManager::new("lead-1");
    fm.upsert_uav(typed_uav("cca-1", "cca"));
    fm.upsert_uav(typed_uav("lsuav-1", "lsuav"));
    let assignments = fm.allocate_formation_roles(&fired[0].workflow);
    let cca = assignments.iter().find(|a| a.member_id == "cca-1").unwrap();
    let lsuav = assignments
        .iter()
        .find(|a| a.member_id == "lsuav-1")
        .unwrap();
    assert_eq!(cca.role, CcaRole::EwJamming, "armed CCA jams");
    assert_ne!(lsuav.role, CcaRole::EwJamming, "unarmed LSUAV never jams");
    assert_eq!(lsuav.role, CcaRole::Relay);
}

#[test]
fn member_self_degrades_on_link_loss() {
    // No single point of failure: a member that loses the link adopts a safe role.
    assert_eq!(
        degraded_role(MemberCapability::from_uav_type("cca")),
        CcaRole::EwProtection
    );
    assert_eq!(
        degraded_role(MemberCapability::from_uav_type("lsuav")),
        CcaRole::Recon
    );
}
