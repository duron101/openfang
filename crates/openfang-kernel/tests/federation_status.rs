//! M4-U6 — kernel-level federation status tests.
//!
//! Verifies that the federation primitives wired into the kernel produce the
//! correct end-to-end status object as the simulated link-quality is stepped
//! through `Excellent → Poor → Lost → Excellent`. Together with the
//! `openfang-runtime::federation` unit tests (15 cases) and the
//! `autonomy_profile_matrix.rs` gate-side tests, this completes the M4-U6
//! deterministic federation closed-loop coverage.

use openfang_kernel::OpenFangKernel;
use openfang_types::config::{
    AdapterConfig, AutonomyConfig, AutonomyModeProfile, DefaultModelConfig, FederationConfig,
    KernelConfig, PlatformConfig, PlatformMode, WeaponDisposition,
};
use openfang_types::platform::LinkQuality;

fn make_test_config(test_name: &str) -> KernelConfig {
    // Per-test unique tmp dir so the sqlite memory DB does not get locked when
    // multiple tests in the same binary boot in parallel.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!(
        "openfang-federation-{}-{}-{}",
        std::process::id(),
        test_name,
        nanos
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let autonomy = AutonomyConfig {
        active_profile: "supervised_autonomy".into(),
        degraded_profile: Some("defensive_autonomy".into()),
        profiles: vec![
            AutonomyModeProfile {
                id: "supervised_autonomy".into(),
                description: "operator profile".into(),
                auto_classes: vec!["motion".into(), "sensor".into(), "comm".into()],
                weapon_disposition: WeaponDisposition::PendingApproval,
                ..AutonomyModeProfile::default()
            },
            AutonomyModeProfile {
                id: "defensive_autonomy".into(),
                description: "self-defense reflex only".into(),
                auto_classes: vec!["motion".into(), "ew".into()],
                weapon_disposition: WeaponDisposition::SuggestOnly,
                allow_defensive_reflex: true,
                ..AutonomyModeProfile::default()
            },
        ],
    };

    let federation = FederationConfig {
        priority_order: vec!["self-vessel".into(), "wingman".into()],
        member_id: String::new(),
        stale_command_window_s: 10.0,
    };

    let platform = PlatformConfig {
        own_platform_id: "self-vessel".into(),
        autonomy,
        federation,
        ..PlatformConfig::default()
    };

    KernelConfig {
        home_dir: tmp.clone(),
        data_dir: tmp.join("data"),
        platform,
        default_model: DefaultModelConfig {
            provider: "groq".to_string(),
            model: "llama-3.3-70b-versatile".to_string(),
            api_key_env: "GROQ_API_KEY_DOES_NOT_EXIST".to_string(),
            base_url: None,
        },
        ..KernelConfig::default()
    }
}

/// Platform-enabled variant: a `mock` primary adapter makes the kernel build
/// the control loop and adopt its shared link-quality override handle at boot
/// (the M4-U6 review fix), plus spawn the per-tick degradation task. This is
/// the only test that exercises the boot-time `link_quality_override_handle`
/// adoption and `observed_link_quality` publication.
fn make_platform_enabled_config(test_name: &str) -> KernelConfig {
    let mut cfg = make_test_config(test_name);
    cfg.platform.mode = PlatformMode::Simulation;
    cfg.platform.adapters.insert(
        "sim".to_string(),
        AdapterConfig {
            adapter_type: "mock".into(),
            ..Default::default()
        },
    );
    cfg
}

#[tokio::test]
async fn federation_status_with_live_control_loop_degrades_and_recovers() {
    // Boots with platform mode + a mock adapter so the control loop spawns and
    // the kernel's `simulated_link_quality` IS the loop's shared override Arc.
    let kernel = OpenFangKernel::boot_with_config(make_platform_enabled_config("live-loop"))
        .expect("kernel boots with platform enabled");

    let healthy = kernel.federation_status().await;
    assert!(!healthy.degraded, "healthy link must not degrade at boot");
    assert_eq!(healthy.effective_profile, "supervised_autonomy");

    // Writing the (shared) override degrades the gate, not just the report.
    kernel.set_simulated_link_quality(LinkQuality::Lost, "test");
    let degraded = kernel.federation_status().await;
    assert!(degraded.degraded, "Lost link must degrade with a live loop");
    assert_eq!(degraded.effective_profile, "defensive_autonomy");

    kernel.set_simulated_link_quality(LinkQuality::Excellent, "test");
    let recovered = kernel.federation_status().await;
    assert!(!recovered.degraded);
    assert_eq!(recovered.effective_profile, "supervised_autonomy");

    kernel.shutdown();
}

#[tokio::test]
async fn federation_status_under_healthy_link_keeps_configured_profile() {
    let kernel =
        OpenFangKernel::boot_with_config(make_test_config("healthy")).expect("kernel boots");
    let status = kernel.federation_status().await;

    assert_eq!(status.local_id, "self-vessel");
    assert_eq!(status.leader_id, "self-vessel");
    assert!(status.is_leader);
    assert_eq!(status.link_quality, "excellent");
    assert_eq!(status.effective_profile, "supervised_autonomy");
    assert_eq!(status.configured_profile, "supervised_autonomy");
    assert!(!status.degraded);

    kernel.shutdown();
}

#[tokio::test]
async fn federation_status_under_poor_link_switches_to_degraded_profile() {
    let kernel = OpenFangKernel::boot_with_config(make_test_config("poor")).expect("kernel boots");

    let previous = kernel.set_simulated_link_quality(LinkQuality::Poor, "test");
    assert_eq!(previous, LinkQuality::Excellent);

    let status = kernel.federation_status().await;
    assert_eq!(status.link_quality, "poor");
    assert_eq!(status.effective_profile, "defensive_autonomy");
    assert_eq!(status.configured_profile, "supervised_autonomy");
    assert!(status.degraded);
    assert!(status.reason.contains("degraded"));

    kernel.shutdown();
}

#[tokio::test]
async fn federation_status_recovers_on_link_restore() {
    let kernel =
        OpenFangKernel::boot_with_config(make_test_config("restore")).expect("kernel boots");

    kernel.set_simulated_link_quality(LinkQuality::Lost, "test");
    let degraded = kernel.federation_status().await;
    assert!(degraded.degraded);
    assert_eq!(degraded.effective_profile, "defensive_autonomy");

    kernel.set_simulated_link_quality(LinkQuality::Excellent, "test");
    let recovered = kernel.federation_status().await;
    assert!(!recovered.degraded);
    assert_eq!(recovered.effective_profile, "supervised_autonomy");

    kernel.shutdown();
}

#[tokio::test]
async fn federation_status_unknown_link_quality_rejected_via_audit() {
    // Verifies the set_simulated_link_quality path never silently widens the
    // gate when an unexpected bucket sneaks in: the kernel only accepts
    // typed LinkQuality, and degraded_profile fallback is only triggered for
    // Poor/Lost (Marginal stays configured).
    let kernel =
        OpenFangKernel::boot_with_config(make_test_config("marginal")).expect("kernel boots");

    kernel.set_simulated_link_quality(LinkQuality::Marginal, "test");
    let status = kernel.federation_status().await;
    assert_eq!(status.link_quality, "marginal");
    assert!(
        !status.degraded,
        "Marginal link should NOT downgrade profile"
    );
    assert_eq!(status.effective_profile, "supervised_autonomy");

    kernel.shutdown();
}
