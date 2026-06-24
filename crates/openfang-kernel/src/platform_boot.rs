//! Platform adapter boot wiring.
//!
//! Translates the declarative [`PlatformConfig`] into a live [`AdapterRegistry`]:
//! constructs each configured adapter, designates the primary, registers
//! secondaries, and installs the platform→adapter routing table used by
//! [`AdapterRegistry::route_commands`].
//!
//! Concrete backends (DDS, ArkSIM) are compiled in behind crate features
//! (`dds`, `arksim`, both on by default). When a configured backend is not
//! compiled in, the builder falls back to a [`MockAdapter`] and logs a warning,
//! so a deployment still boots (in a degraded, non-actuating mode) rather than
//! failing hard.

use openfang_platform::{AdapterRegistry, MockAdapter, NoopAdapter, PlatformAdapter};
use openfang_types::config::{AdapterConfig, PlatformConfig};
use tracing::{info, warn};

/// Construct an [`AdapterRegistry`] from configuration.
///
/// When the platform layer is disabled or no adapters are declared, an empty
/// registry is returned (the kernel boots with no platform wiring).
pub fn build_registry(cfg: &PlatformConfig) -> AdapterRegistry {
    let registry = AdapterRegistry::new();

    if !cfg.is_enabled() {
        return registry;
    }

    let Some(primary_name) = cfg.primary_name() else {
        warn!("platform layer enabled but no adapters resolved; registry left empty");
        return registry;
    };

    // Deterministic ordering for logging / primary resolution.
    let mut names: Vec<&String> = cfg.adapters.keys().collect();
    names.sort();

    for name in names {
        let adapter_cfg = &cfg.adapters[name];
        let adapter = build_adapter(name, adapter_cfg, &cfg.own_platform_id);
        let adapter_id = adapter.adapter_id().to_string();

        if *name == primary_name {
            info!(adapter = %name, kind = %adapter_cfg.adapter_type, "platform: primary adapter");
            registry.set_primary(adapter);
        } else {
            info!(adapter = %name, kind = %adapter_cfg.adapter_type, "platform: secondary adapter");
            registry.add_secondary(adapter);
            // Route each declared platform id to this secondary adapter.
            for pid in &adapter_cfg.platforms {
                registry.route_platform(pid, &adapter_id);
            }
        }
    }

    registry
}

/// Construct a single adapter from its config. Unknown / not-compiled-in types
/// fall back to a mock so the registry still has a structurally valid adapter.
fn build_adapter(
    name: &str,
    cfg: &AdapterConfig,
    own_platform_id: &str,
) -> Box<dyn PlatformAdapter> {
    match cfg.adapter_type.as_str() {
        "mock" => Box::new(MockAdapter::new(name)),
        "noop" => Box::new(NoopAdapter::new()),

        #[cfg(feature = "arksim")]
        "arksim" => {
            let transport = openfang_platform_arksim::ArkSimTransport::resolve(
                cfg.arksim_transport.as_deref(),
                cfg.scenario_path.as_deref(),
                cfg.arksim_uuid.as_deref(),
            );
            Box::new(openfang_platform_arksim::ArkSimAdapter::new(
                openfang_platform_arksim::ArkSimConfig {
                    host: cfg.host.clone(),
                    port: if cfg.port != 0 { cfg.port } else { 18000 },
                    service_port: cfg.arksim_service_port,
                    transport,
                    situation_kind: openfang_platform_arksim::situation::SituationKind::Customized,
                    situation_interval_secs: cfg.situation_interval_secs,
                    session_uuid: None,
                    attach_session_uuid: cfg.arksim_uuid.clone(),
                    scenario_path: cfg.scenario_path.clone(),
                    connect_timeout_secs: cfg.step_timeout_secs.max(5),
                    runstep_after_weapon: transport
                        == openfang_platform_arksim::ArkSimTransport::ArkService,
                    weapon_runstep_count: 50,
                    weapon_advance_time_secs: None,
                    auto_outside_control_self: cfg.auto_outside_control_self,
                    own_platform_id: own_platform_id.to_string(),
                    component_manifest_path: cfg.component_manifest_path.clone(),
                },
            ))
        }

        #[cfg(feature = "mavlink")]
        "mavlink" => {
            // Single autopilot link. The platform id it represents is the first
            // declared platform, falling back to the adapter name.
            let platform_id = cfg
                .platforms
                .first()
                .cloned()
                .unwrap_or_else(|| name.to_string());
            Box::new(openfang_platform_mavlink::MavlinkAdapter::new_loopback(
                platform_id,
            ))
        }

        #[cfg(feature = "dds")]
        "dds" => {
            // The real RTPS transport is gated in the DDS crate (HIL rig). The
            // default build wires the in-process loopback transport so the
            // pipeline runs end-to-end without external infrastructure.
            let dds_cfg = openfang_platform_dds::DdsConfig {
                domain_id: cfg.domain_id as u16,
                ..Default::default()
            };
            Box::new(openfang_platform_dds::DdsAdapter::with_transport(
                Box::new(openfang_platform_dds::LoopbackTransport::default()),
                dds_cfg,
            ))
        }

        other => {
            warn!(
                adapter = %name,
                kind = %other,
                "platform adapter type not available in this build; using mock fallback"
            );
            Box::new(MockAdapter::new(name))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::config::{AdapterConfig, PlatformConfig, PlatformMode};
    use std::collections::HashMap;

    fn cfg_with(adapters: Vec<(&str, AdapterConfig)>, mode: PlatformMode) -> PlatformConfig {
        let mut map = HashMap::new();
        for (k, v) in adapters {
            map.insert(k.to_string(), v);
        }
        PlatformConfig {
            mode,
            adapters: map,
            ..Default::default()
        }
    }

    fn mock_cfg() -> AdapterConfig {
        AdapterConfig {
            adapter_type: "mock".into(),
            ..Default::default()
        }
    }

    #[test]
    fn disabled_config_yields_empty_registry() {
        let reg = build_registry(&PlatformConfig::default());
        assert!(!reg.has_primary());
    }

    #[test]
    fn single_mock_becomes_primary() {
        let cfg = cfg_with(vec![("sim", mock_cfg())], PlatformMode::Simulation);
        let reg = build_registry(&cfg);
        assert!(reg.has_primary());
        assert_eq!(reg.secondary_count(), 0);
    }

    #[tokio::test]
    async fn live_tool_path_config_to_adapter() {
        // Mirrors OpenFangKernel::dispatch_platform_command end to end:
        // config → build_registry → connect_all → map tool → route_commands.
        let cfg = cfg_with(vec![("sim", mock_cfg())], PlatformMode::Simulation);
        let reg = build_registry(&cfg);
        reg.connect_all().await.unwrap();

        let args = serde_json::json!({"platform_id": "self", "heading_deg": 270.0});
        let cmd =
            openfang_runtime::platform_tools::map_tool_to_command("platform_set_heading", &args)
                .unwrap()
                .unwrap();
        let res = reg.route_commands(&[cmd]).await.unwrap();
        assert_eq!(
            res.accepted, 1,
            "tool-mapped command should reach the adapter"
        );
    }

    #[test]
    fn hybrid_registers_secondary_and_routes() {
        let mut sec = mock_cfg();
        sec.platforms = vec!["uav-1".into()];
        let cfg = cfg_with(
            vec![("aaa_primary", mock_cfg()), ("zzz_secondary", sec)],
            PlatformMode::Hybrid,
        );
        let reg = build_registry(&cfg);
        assert!(reg.has_primary());
        assert_eq!(reg.secondary_count(), 1);
    }
}
