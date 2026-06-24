use openfang_runtime::cerebellum_services::{CerebellumService, ServiceContext};
use openfang_runtime::sensor_management::SensorManagementService;
use openfang_types::config::{
    AutonomyModeProfile, EmconSensorRule, SensorDisposition, SensorEmconLevel, SensorPolicyConfig,
};
use openfang_types::platform::{
    Affiliation, CcaRole, Domain, FuelStatus, LinkQuality, LinkStatusReport, PlatformCapabilities,
    PlatformCommand, PlatformState, Pose, SensorState, SensorType, Track, Velocity,
};

fn active_radar_platform() -> PlatformState {
    let mut state = PlatformState::minimal("self");
    state.affiliation = Affiliation::Blue;
    state.domain = Domain::Surface;
    state.pose = Pose {
        lat_deg: 30.0,
        lon_deg: 120.0,
        alt_m: 0.0,
        heading_deg: 0.0,
        pitch_deg: 0.0,
        roll_deg: 0.0,
    };
    state.velocity = Velocity {
        speed_ms: 8.0,
        vertical_rate_ms: 0.0,
        course_deg: 0.0,
    };
    state.fuel = FuelStatus {
        remaining_kg: 80.0,
        max_kg: 100.0,
        consumption_rate_kg_s: 0.1,
    };
    state.onboard_sensors = vec![SensorState {
        sensor_id: "surf_radar".into(),
        sensor_type: SensorType::Radar,
        mode: "active".into(),
        frequency_hz: None,
        bandwidth_hz: None,
        azimuth_fov_deg: None,
        elevation_fov_deg: None,
        range_max_m: Some(30_000.0),
        damage: 0.0,
        host_platform_id: "self".into(),
    }];
    state
}

fn hostile_track(range_m: f64, quality: f64, stale: bool) -> Track {
    Track {
        track_id: "threat-1".into(),
        target_name: "fast_contact".into(),
        classification: "surface".into(),
        affiliation: Affiliation::Red,
        iff: "foe".into(),
        position_lla: None,
        heading_deg: None,
        speed_ms: Some(25.0),
        range_m: Some(range_m),
        bearing_deg: None,
        elevation_deg: None,
        quality,
        stale,
        last_update_s: 0.0,
        is_active: true,
    }
}

fn caps() -> PlatformCapabilities {
    PlatformCapabilities {
        supports_sensor_control: true,
        ..PlatformCapabilities::default()
    }
}

fn ctx<'a>(
    own: &'a PlatformState,
    caps: &'a PlatformCapabilities,
    autonomy: &'a AutonomyModeProfile,
    now: f64,
) -> ServiceContext<'a> {
    ServiceContext {
        snapshot: None,
        own_platform: Some(own),
        fused_tracks: &[],
        autonomy: Some(autonomy),
        capabilities: caps,
        posture: CcaRole::Recon,
        now,
        own_platform_id: &own.id,
    }
}

#[test]
fn recon_restricted_emcon_turns_active_radar_off() {
    let own = active_radar_platform();
    let caps = caps();
    let autonomy = AutonomyModeProfile::default();
    let mut service = SensorManagementService::default();

    let out = service.evaluate(&ctx(&own, &caps, &autonomy, 5.0));

    assert!(out.intents.iter().any(|intent| matches!(
        &intent.command,
        PlatformCommand::SensorOff {
            platform_id,
            sensor_id
        } if platform_id == "self" && sensor_id == "surf_radar"
    )));
    assert!(out
        .audit_hints
        .iter()
        .any(|hint| hint.event == "sensor_force_off"));
}

#[test]
fn operator_override_prevents_restricted_emcon_shutdown_until_expiry() {
    let own = active_radar_platform();
    let caps = caps();
    let autonomy = AutonomyModeProfile::default();
    let mut service = SensorManagementService::default();
    service.note_operator_sensor_intent("surf_radar", "on", 5.0);

    let out = service.evaluate(&ctx(&own, &caps, &autonomy, 6.0));

    assert!(!out
        .intents
        .iter()
        .any(|intent| matches!(intent.command, PlatformCommand::SensorOff { .. })));
    assert!(out
        .audit_hints
        .iter()
        .any(|hint| hint.event == "sensor_operator_override"));
}

#[test]
fn status_reports_current_expected_policy_and_reason() {
    let own = active_radar_platform();
    let caps = caps();
    let autonomy = AutonomyModeProfile::default();
    let mut service = SensorManagementService::default();

    let _ = service.evaluate(&ctx(&own, &caps, &autonomy, 5.0));
    let statuses = service.status_for(Some(&own));

    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].sensor_id, "surf_radar");
    assert_eq!(statuses[0].current_mode, "active");
    assert_eq!(statuses[0].expected_mode.as_deref(), Some("off"));
    assert_eq!(
        statuses[0].recent_reason.as_deref(),
        Some("emcon_restricted")
    );
}

#[test]
fn damaged_sensor_is_forced_off_even_with_operator_override() {
    let mut own = active_radar_platform();
    own.onboard_sensors[0].damage = 0.8;
    let caps = caps();
    let autonomy = AutonomyModeProfile::default();
    let mut service = SensorManagementService::default();
    service.note_operator_sensor_intent("surf_radar", "on", 5.0);

    let out = service.evaluate(&ctx(&own, &caps, &autonomy, 6.0));

    assert!(out
        .intents
        .iter()
        .any(|intent| matches!(intent.command, PlatformCommand::SensorOff { .. })));
    let statuses = service.status_for(Some(&own));
    assert_eq!(statuses[0].recent_reason.as_deref(), Some("sensor_health"));
}

#[test]
fn lost_link_close_threat_forces_radar_on_for_survival() {
    let mut own = active_radar_platform();
    own.onboard_sensors[0].mode = "standby".into();
    own.tracks.push(hostile_track(800.0, 0.9, false));
    own.link = Some(LinkStatusReport {
        quality: LinkQuality::Lost,
        ..Default::default()
    });
    let caps = caps();
    let autonomy = AutonomyModeProfile::default();
    let mut service = SensorManagementService::default();

    let out = service.evaluate(&ctx(&own, &caps, &autonomy, 6.0));

    assert!(out.intents.iter().any(|intent| matches!(
        &intent.command,
        PlatformCommand::SensorOn { sensor_id, .. } if sensor_id == "surf_radar"
    )));
}

#[test]
fn stale_track_quality_drives_eoir_track_refresh() {
    let mut own = active_radar_platform();
    own.onboard_sensors = vec![SensorState {
        sensor_id: "eo1".into(),
        sensor_type: SensorType::EOIR,
        mode: "search".into(),
        frequency_hz: None,
        bandwidth_hz: None,
        azimuth_fov_deg: None,
        elevation_fov_deg: None,
        range_max_m: Some(10_000.0),
        damage: 0.0,
        host_platform_id: "self".into(),
    }];
    own.tracks.push(hostile_track(5_000.0, 0.25, true));
    let caps = caps();
    let autonomy = AutonomyModeProfile::default();
    let mut service = SensorManagementService::default();

    let out = service.evaluate(&ctx(&own, &caps, &autonomy, 6.0));

    assert!(out.intents.iter().any(|intent| matches!(
        &intent.command,
        PlatformCommand::SensorSetMode { sensor_id, mode, .. }
            if sensor_id == "eo1" && mode == "track"
    )));
}

#[test]
fn damaged_primary_failover_turns_on_redundant_sensor() {
    let mut own = active_radar_platform();
    own.onboard_sensors.push(SensorState {
        sensor_id: "surf_radar_backup".into(),
        sensor_type: SensorType::Radar,
        mode: "standby".into(),
        frequency_hz: None,
        bandwidth_hz: None,
        azimuth_fov_deg: None,
        elevation_fov_deg: None,
        range_max_m: Some(30_000.0),
        damage: 0.0,
        host_platform_id: "self".into(),
    });
    own.onboard_sensors[0].damage = 0.8;
    let caps = caps();
    let autonomy = AutonomyModeProfile::default();
    let mut service = SensorManagementService::default();

    let out = service.evaluate(&ctx(&own, &caps, &autonomy, 6.0));

    assert!(out.intents.iter().any(|intent| matches!(
        &intent.command,
        PlatformCommand::SensorOn { sensor_id, .. } if sensor_id == "surf_radar_backup"
    )));
    assert!(out
        .audit_hints
        .iter()
        .any(|hint| hint.event == "sensor_failover"));
}

#[test]
fn esm_threat_drives_radar_on_at_l4() {
    let mut own = active_radar_platform();
    own.onboard_sensors[0].mode = "standby".into();
    own.onboard_sensors.push(SensorState {
        sensor_id: "esm1".into(),
        sensor_type: SensorType::ESM,
        mode: "passive".into(),
        frequency_hz: None,
        bandwidth_hz: None,
        azimuth_fov_deg: None,
        elevation_fov_deg: None,
        range_max_m: None,
        damage: 0.0,
        host_platform_id: "self".into(),
    });
    own.tracks.push(hostile_track(4_000.0, 0.2, true));
    own.link = Some(LinkStatusReport {
        quality: LinkQuality::Poor,
        ..Default::default()
    });
    let caps = caps();
    let autonomy = AutonomyModeProfile::default();
    let mut service = SensorManagementService::default();

    let out = service.evaluate(&ctx(&own, &caps, &autonomy, 6.0));

    assert!(out.intents.iter().any(|intent| matches!(
        &intent.command,
        PlatformCommand::SensorOn { sensor_id, .. } if sensor_id == "surf_radar"
    )));
}

#[test]
fn esm_threat_blocks_autonomous_radar_on_at_l3() {
    let mut own = active_radar_platform();
    own.onboard_sensors[0].mode = "standby".into();
    own.onboard_sensors.push(SensorState {
        sensor_id: "esm1".into(),
        sensor_type: SensorType::ESM,
        mode: "passive".into(),
        frequency_hz: None,
        bandwidth_hz: None,
        azimuth_fov_deg: None,
        elevation_fov_deg: None,
        range_max_m: None,
        damage: 0.0,
        host_platform_id: "self".into(),
    });
    own.tracks.push(hostile_track(4_000.0, 0.2, true));
    own.link = Some(LinkStatusReport {
        quality: LinkQuality::Good,
        ..Default::default()
    });
    let caps = caps();
    let autonomy = AutonomyModeProfile::default();
    let mut service = SensorManagementService::default();

    let out = service.evaluate(&ctx(&own, &caps, &autonomy, 6.0));

    assert!(!out
        .intents
        .iter()
        .any(|intent| matches!(intent.command, PlatformCommand::SensorOn { .. })));
    assert!(out
        .audit_hints
        .iter()
        .any(|hint| hint.event == "sensor_pending_approval_l3"));
}

#[test]
fn operator_override_actively_enforces_desired_mode() {
    // Radar is currently off but the operator demanded "on": SMS must actively
    // drive it on, not merely refrain from turning it off.
    let mut own = active_radar_platform();
    own.onboard_sensors[0].mode = "standby".into();
    let caps = caps();
    let autonomy = AutonomyModeProfile::default();
    let mut service = SensorManagementService::default();
    service.note_operator_sensor_intent("surf_radar", "on", 5.0);

    let out = service.evaluate(&ctx(&own, &caps, &autonomy, 6.0));

    assert!(out.intents.iter().any(|intent| matches!(
        &intent.command,
        PlatformCommand::SensorOn { sensor_id, .. } if sensor_id == "surf_radar"
    )));
    assert!(out
        .audit_hints
        .iter()
        .any(|hint| hint.event == "sensor_operator_override"));
}

#[test]
fn operator_override_is_idempotent_when_mode_matches() {
    // Radar already active and operator wants "on": no redundant command.
    let own = active_radar_platform();
    let caps = caps();
    let autonomy = AutonomyModeProfile::default();
    let mut service = SensorManagementService::default();
    service.note_operator_sensor_intent("surf_radar", "on", 5.0);

    let out = service.evaluate(&ctx(&own, &caps, &autonomy, 6.0));

    assert!(out.intents.is_empty());
    assert!(out
        .audit_hints
        .iter()
        .any(|hint| hint.event == "sensor_operator_override"));
}

#[test]
fn override_ttl_is_configurable_and_expires_per_config() {
    // A 1s TTL means the override is gone by the time we re-evaluate at +2s,
    // so EMCON restriction reasserts and the radar is forced off.
    let own = active_radar_platform();
    let caps = caps();
    let autonomy = AutonomyModeProfile::default();
    let cfg = SensorPolicyConfig {
        override_ttl_s: 1.0,
        ..SensorPolicyConfig::default()
    };
    let mut service = SensorManagementService::from_config(&cfg);
    service.note_operator_sensor_intent("surf_radar", "on", 5.0);

    let out = service.evaluate(&ctx(&own, &caps, &autonomy, 7.0));

    assert!(out
        .intents
        .iter()
        .any(|intent| matches!(intent.command, PlatformCommand::SensorOff { .. })));
}

#[test]
fn policy_pending_approval_blocks_autonomous_activation() {
    // Off radar under a policy whose active-emitter disposition is
    // PendingApproval: SMS records the gap and raises an approval audit, but
    // emits no autonomous on/off intent.
    let mut own = active_radar_platform();
    own.onboard_sensors[0].mode = "standby".into();
    let caps = caps();
    let autonomy = AutonomyModeProfile::default();
    let cfg = SensorPolicyConfig {
        active_radar_disposition: SensorDisposition::PendingApproval,
        emcon_rules: vec![EmconSensorRule {
            emcon: SensorEmconLevel::Restricted,
            sensor_type: SensorType::Radar,
            disposition: SensorDisposition::PendingApproval,
        }],
        ..SensorPolicyConfig::default()
    };
    let mut service = SensorManagementService::from_config(&cfg);

    let out = service.evaluate(&ctx(&own, &caps, &autonomy, 6.0));

    assert!(out.intents.is_empty());
    assert!(out
        .audit_hints
        .iter()
        .any(|hint| hint.event == "sensor_pending_approval"));
}
