use openfang_runtime::wms_policy::WmsPolicyEngine;
use openfang_types::umaa::WeaponReleaseLevel;
use openfang_types::wms::{ReattackMode, WmsDisposition};

#[test]
fn default_policy_classifies_usv_task_weapon_types() {
    let policy = WmsPolicyEngine::default();

    let isr = policy.class_for_weapon("scout_uav_slot").unwrap();
    assert_eq!(isr.id, "isr_deploy");
    assert_eq!(isr.reattack_mode, ReattackMode::CooldownOnly);

    let j7 = policy.class_for_weapon("J7_UAV_WEAPON").unwrap();
    assert_eq!(j7.id, "isr_deploy");
    assert_eq!(j7.reattack_mode, ReattackMode::CooldownOnly);

    let kinetic = policy.class_for_weapon("loiter_wave3").unwrap();
    assert_eq!(kinetic.id, "kinetic_strike");
    assert_eq!(kinetic.reattack_mode, ReattackMode::FireOnce);

    let countermeasure = policy.class_for_weapon("chaff_launcher").unwrap();
    assert_eq!(countermeasure.id, "self_defense_countermeasure");
    assert_eq!(countermeasure.reattack_mode, ReattackMode::CooldownOnly);
}

#[test]
fn default_policy_routes_by_autonomy_profile_and_roe() {
    let policy = WmsPolicyEngine::default();

    assert_eq!(
        policy.disposition_for(
            "supervised_autonomy",
            WeaponReleaseLevel::WeaponsTight,
            "scout_uav_slot"
        ),
        WmsDisposition::Auto
    );
    assert_eq!(
        policy.disposition_for(
            "supervised_autonomy",
            WeaponReleaseLevel::WeaponsTight,
            "J7_UAV_WEAPON"
        ),
        WmsDisposition::Auto
    );
    assert_eq!(
        policy.disposition_for(
            "supervised_autonomy",
            WeaponReleaseLevel::WeaponsTight,
            "loiter_wave3"
        ),
        WmsDisposition::Pending
    );
    assert_eq!(
        policy.disposition_for(
            "supervised_autonomy",
            WeaponReleaseLevel::WeaponsHold,
            "loiter_wave3"
        ),
        WmsDisposition::Deny
    );
}
