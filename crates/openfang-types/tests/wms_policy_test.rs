use openfang_types::umaa::WeaponReleaseLevel;
use openfang_types::wms::{ReattackMode, WmsDisposition, WmsPolicyConfig, WmsRule, WmsWeaponClass};

#[test]
fn wms_policy_config_deserializes_weapon_classes_and_rules() {
    let raw = r#"
[[weapon_classes]]
id = "isr_deploy"
match_weapon_ids = ["scout_uav*", "recon_uav*"]
reattack_mode = "cooldown_only"
cooldown_secs = 2.0

[[rules]]
autonomy_profile = "supervised_autonomy"
roe = "weapons_tight"
weapon_class = "isr_deploy"
disposition = "auto"
"#;

    let cfg: WmsPolicyConfig = toml::from_str(raw).unwrap();

    assert_eq!(
        cfg.weapon_classes,
        vec![WmsWeaponClass {
            id: "isr_deploy".into(),
            match_weapon_ids: vec!["scout_uav*".into(), "recon_uav*".into()],
            reattack_mode: ReattackMode::CooldownOnly,
            cooldown_secs: 2.0,
        }]
    );
    assert_eq!(
        cfg.rules,
        vec![WmsRule {
            autonomy_profile: "supervised_autonomy".into(),
            roe: WeaponReleaseLevel::WeaponsTight,
            weapon_class: "isr_deploy".into(),
            disposition: WmsDisposition::Auto,
        }]
    );
}
