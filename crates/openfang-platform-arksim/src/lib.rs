//! ArkSIM Platform Adapter — manual protobuf, no prost-build dependency.
//!
//! Implements `PlatformAdapter` for the ArkSIM simulation engine using
//! hand-coded protobuf encode/decode validated against ArkSIM 4.1 wire format.
//!
//! **态势默认：定制态势**（`changesituation.rate = 0`，`arksimproto.proto` /
//! JSON `customizedsituation`）。ArkSIM 当前统一通过 ArkService ZMQ
//! ROUTER/DEALER `60004` 端口承载仿真控制、实体控制和态势观测；
//! ArkService 回包解析见 [`situation`] 模块。

mod arkservice;
pub mod arksim_controller;
mod cmd_log;
pub mod command_mapper;
mod command_sanitize;
pub mod component_manifest;
pub mod proto_manual;
pub mod response_handler;
pub mod sim_control;
pub mod situation;
pub mod state_mapper;
mod strike_protocol;
mod track_id;
mod zmq_sim_bridge;

use async_trait::async_trait;
use openfang_platform::{AdapterType, PlatformAdapter, PlatformCapabilities, PlatformError};
use openfang_types::platform::{CommandResult, PlatformCommand, WorldSnapshot};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tracing::{debug, info, warn};

use arkservice::ArkServiceClient;
use zmq_sim_bridge::ZmqSimBridge;

/// How OpenFang talks to ArkSIM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArkSimTransport {
    /// ArkService JSON on 60004 — may `start` scenario or attach by uuid.
    ArkService,
    /// Warlock/mission ZMQ PAIR on 18000 — no ArkService, no uuid, no start.
    WarlockDirect,
}

impl ArkSimTransport {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("warlock_direct")
            | Some("warlock")
            | Some("zmq_pair")
            | Some("zmq")
            | Some("18000") => Self::WarlockDirect,
            Some("ark_service") | Some("arkservice") | Some("60004") => Self::ArkService,
            _ => Self::ArkService,
        }
    }

    /// Infer transport when `arksim_transport` is unset: scenario_path → ArkService,
    /// otherwise Warlock direct (manual Play, no uuid).
    pub fn resolve(
        explicit: Option<&str>,
        scenario_path: Option<&str>,
        arksim_uuid: Option<&str>,
    ) -> Self {
        if let Some(raw) = explicit.filter(|s| !s.is_empty()) {
            return Self::parse(Some(raw));
        }
        if arksim_uuid.is_some() || scenario_path.is_some() {
            Self::ArkService
        } else {
            Self::WarlockDirect
        }
    }
}

enum ArkSimBackend {
    ArkService(Arc<Mutex<ArkServiceClient>>),
    WarlockDirect(Arc<Mutex<ZmqSimBridge>>),
}

impl Clone for ArkSimBackend {
    fn clone(&self) -> Self {
        match self {
            Self::ArkService(c) => Self::ArkService(Arc::clone(c)),
            Self::WarlockDirect(z) => Self::WarlockDirect(Arc::clone(z)),
        }
    }
}

/// ArkSIM simulation adapter
pub struct ArkSimAdapter {
    backend: Option<ArkSimBackend>,
    latest_snapshot: Arc<Mutex<Option<WorldSnapshot>>>,
    receiver_stop: Option<Arc<AtomicBool>>,
    receiver_handle: Option<JoinHandle<()>>,
    config: ArkSimConfig,
    component_manifest: Option<component_manifest::ComponentManifest>,
    connected: bool,
}

#[derive(Debug, Clone)]
pub struct ArkSimConfig {
    pub host: String,
    /// Warlock direct ZMQ PAIR port (default 18000).
    pub port: u16,
    /// ArkService ZMQ ROUTER/DEALER port (default 60004). Only used when
    /// [`transport`] is [`ArkSimTransport::ArkService`].
    pub service_port: u16,
    pub transport: ArkSimTransport,
    /// 态势类型；默认 [`situation::SituationKind::Customized`]（ArkService 路径）。
    pub situation_kind: situation::SituationKind,
    /// 定制态势推送间隔（秒），对应 `customizedsituation.time`。
    pub situation_interval_secs: f64,
    /// ArkService session uuid — populated after connect (ArkService path only).
    pub session_uuid: Option<String>,
    /// ArkService attach uuid (ArkService path only). Ignored in Warlock direct mode.
    pub attach_session_uuid: Option<String>,
    /// Scenario path for ArkService `start` (not used in Warlock direct mode).
    pub scenario_path: Option<String>,
    /// ZMQ connect timeout (Warlock direct).
    pub connect_timeout_secs: u64,
    /// After weapon proto, send runstep (ArkService only; ZMQ step advances per send).
    pub runstep_after_weapon: bool,
    pub weapon_runstep_count: u32,
    pub weapon_advance_time_secs: Option<f64>,
    /// Whether to auto-prefix `SetOutsideControl(self)` before `self` part
    /// commands. The sanitizer only lets this reach the wire when the latest
    /// StateMessage contains a real platform named `self`.
    pub auto_outside_control_self: bool,
    /// Own platform id from kernel config — used to resolve the `"self"` alias
    /// against the latest StateMessage before weapon fires are encoded.
    pub own_platform_id: String,
    /// Optional stable scenario component tree manifest. It may describe
    /// platform parts such as sensors/weapons/comms, but must not describe
    /// dynamic track ids.
    pub component_manifest_path: Option<String>,
}

impl Default for ArkSimConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 18000,
            service_port: 60004,
            transport: ArkSimTransport::WarlockDirect,
            situation_kind: situation::SituationKind::Customized,
            situation_interval_secs: 3.0,
            session_uuid: None,
            attach_session_uuid: None,
            scenario_path: None,
            connect_timeout_secs: 30,
            runstep_after_weapon: true,
            weapon_runstep_count: 50,
            weapon_advance_time_secs: None,
            auto_outside_control_self: true,
            own_platform_id: "self".into(),
            component_manifest_path: None,
        }
    }
}

impl ArkSimAdapter {
    pub fn new(config: ArkSimConfig) -> Self {
        let component_manifest = config.component_manifest_path.as_deref().and_then(|path| {
            match component_manifest::ComponentManifest::load(path) {
                Ok(manifest) => Some(manifest),
                Err(err) => {
                    warn!(path, error = %err, "ArkSIM component manifest load failed");
                    None
                }
            }
        });
        Self {
            backend: None,
            latest_snapshot: Arc::new(Mutex::new(None)),
            receiver_stop: None,
            receiver_handle: None,
            config,
            component_manifest,
            connected: false,
        }
    }

    /// Session uuid learned from ArkService `start` (empty until connected).
    pub fn session_uuid(&self) -> Option<&str> {
        self.config.session_uuid.as_deref()
    }

    fn apply_component_manifest(&self, mut snapshot: WorldSnapshot) -> WorldSnapshot {
        if let Some(manifest) = &self.component_manifest {
            manifest.apply_to_snapshot(&mut snapshot);
        }
        snapshot
    }

    fn start_receiver(&mut self, service: Arc<Mutex<ArkServiceClient>>) {
        let latest = Arc::clone(&self.latest_snapshot);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !stop_for_thread.load(Ordering::Relaxed) {
                let snapshot = {
                    let client = match service.lock() {
                        Ok(client) => client,
                        Err(_) => {
                            debug!("ArkService receiver stopped: client mutex poisoned");
                            break;
                        }
                    };
                    client.recv_snapshot(Duration::from_millis(250))
                };

                match snapshot {
                    Ok(Some(snapshot)) => {
                        if let Ok(mut guard) = latest.lock() {
                            *guard = Some(snapshot);
                        } else {
                            debug!("ArkService receiver stopped: snapshot mutex poisoned");
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => debug!("ArkService receiver skipped message: {e}"),
                }
            }
        });

        self.receiver_stop = Some(stop);
        self.receiver_handle = Some(handle);
    }

    fn stop_receiver(&mut self) {
        if let Some(stop) = self.receiver_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        if let Some(handle) = self.receiver_handle.take() {
            let _ = handle.join();
        }
    }

    #[cfg(test)]
    fn connected_with_cached_snapshot_for_test(snapshot: WorldSnapshot) -> Self {
        Self {
            backend: None,
            latest_snapshot: Arc::new(Mutex::new(Some(snapshot))),
            receiver_stop: None,
            receiver_handle: None,
            config: ArkSimConfig::default(),
            component_manifest: None,
            connected: true,
        }
    }
}

#[async_trait]
impl PlatformAdapter for ArkSimAdapter {
    fn adapter_id(&self) -> &str {
        "arksim-primary"
    }

    fn adapter_type(&self) -> AdapterType {
        AdapterType::ArkSim
    }

    async fn connect(&mut self) -> Result<(), PlatformError> {
        let host = self.config.host.clone();
        let transport = self.config.transport;

        match transport {
            ArkSimTransport::WarlockDirect => {
                let port = self.config.port;
                let timeout = Duration::from_secs(self.config.connect_timeout_secs);
                let bridge = tokio::task::spawn_blocking(move || {
                    ZmqSimBridge::connect(&host, port, timeout)
                })
                .await
                .map_err(|e| PlatformError::conn(format!("Warlock direct task join: {e}")))?
                .map_err(PlatformError::conn)?;

                let endpoint = bridge.endpoint().to_string();
                let zmq = Arc::new(Mutex::new(bridge));
                self.backend = Some(ArkSimBackend::WarlockDirect(Arc::clone(&zmq)));
                self.connected = true;
                info!(
                    %endpoint,
                    transport = "warlock_direct",
                    "ArkSIM Warlock ZMQ PAIR connected (no ArkService / no uuid)"
                );
            }
            ArkSimTransport::ArkService => {
                let service_port = self.config.service_port;
                let interval = self.config.situation_interval_secs;
                let attach_uuid = self.config.attach_session_uuid.clone();
                let scenario_path = self.config.scenario_path.clone();

                let client = if let Some(session_uuid) = attach_uuid {
                    info!(
                        session = %session_uuid,
                        "ArkSIM ArkService attach mode"
                    );
                    tokio::task::spawn_blocking(move || {
                        ArkServiceClient::connect_attach(
                            &host,
                            service_port,
                            session_uuid,
                            interval,
                        )
                    })
                    .await
                    .map_err(|e| PlatformError::conn(format!("ArkService attach task join: {e}")))?
                    .map_err(PlatformError::conn)?
                } else {
                    let scenario_path = scenario_path.ok_or_else(|| {
                        PlatformError::conn(
                            "scenario_path required for ArkService start (or use arksim_transport = \"warlock_direct\")",
                        )
                    })?;
                    tokio::task::spawn_blocking(move || {
                        ArkServiceClient::connect(&host, service_port, scenario_path, interval)
                    })
                    .await
                    .map_err(|e| PlatformError::conn(format!("ArkService task join: {e}")))?
                    .map_err(PlatformError::conn)?
                };

                let endpoint = client.endpoint().to_string();
                let session = client.session_uuid().map(str::to_string).ok_or_else(|| {
                    PlatformError::conn("ArkService connect did not yield session uuid")
                })?;
                self.config.session_uuid = Some(session.clone());
                let service = Arc::new(Mutex::new(client));
                self.backend = Some(ArkSimBackend::ArkService(Arc::clone(&service)));
                self.start_receiver(service);
                self.connected = true;
                info!(
                    %endpoint,
                    session = %session,
                    transport = "ark_service",
                    attach = self.config.attach_session_uuid.is_some(),
                    "ArkSIM ArkService connected"
                );
            }
        }
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), PlatformError> {
        self.stop_receiver();
        self.backend = None;
        self.config.session_uuid = None;
        if let Ok(mut latest) = self.latest_snapshot.lock() {
            *latest = None;
        }
        self.connected = false;
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected
    }

    async fn poll_state(&mut self) -> Result<WorldSnapshot, PlatformError> {
        if !self.connected {
            return Err(PlatformError::NotConnected);
        }
        match &self.backend {
            Some(ArkSimBackend::WarlockDirect(zmq)) => {
                // The free-run driver thread continuously refreshes the cache.
                let snap = zmq
                    .lock()
                    .map_err(|_| PlatformError::poll("Warlock ZMQ mutex poisoned"))?
                    .cached_snapshot();
                match snap {
                    Some(s) => {
                        let s = self.apply_component_manifest(s);
                        if let Ok(mut cache) = self.latest_snapshot.lock() {
                            *cache = Some(s.clone());
                        }
                        Ok(s)
                    }
                    None => self
                        .latest_snapshot
                        .lock()
                        .map_err(|_| PlatformError::poll("snapshot cache mutex poisoned"))?
                        .clone()
                        .map(|s| self.apply_component_manifest(s))
                        .ok_or_else(|| {
                            PlatformError::poll("no StateMessage yet — driver still handshaking")
                        }),
                }
            }
            Some(ArkSimBackend::ArkService(_)) | None => self
                .latest_snapshot
                .lock()
                .map_err(|_| PlatformError::poll("ArkService snapshot cache mutex poisoned"))?
                .clone()
                .map(|s| self.apply_component_manifest(s))
                .ok_or_else(|| PlatformError::poll("no customized situation cached yet")),
        }
    }

    async fn send_commands(
        &mut self,
        commands: &[PlatformCommand],
    ) -> Result<CommandResult, PlatformError> {
        if self.backend.is_none() {
            return Err(PlatformError::NotConnected);
        }

        // Partition by what actually has an ArkSim wire mapping
        let mut accepted = 0u32;
        let mut rejected = 0u32;
        let mut errors = Vec::new();
        let mut supported = Vec::new();
        for (idx, cmd) in commands.iter().enumerate() {
            if command_mapper::is_supported(cmd) {
                accepted += 1;
                supported.push(cmd.clone());
            } else {
                rejected += 1;
                errors.push(openfang_types::platform::CommandError {
                    command_index: idx,
                    platform_id: cmd.target_platform_id().to_string(),
                    error: format!("{:?} has no ArkSim mapping", cmd.command_class()),
                });
            }
        }

        if !supported.is_empty() {
            let snapshot = self
                .latest_snapshot
                .lock()
                .map_err(|_| PlatformError::send("snapshot cache mutex poisoned"))?
                .clone()
                .map(|s| self.apply_component_manifest(s));
            let sanitized = command_sanitize::sanitize_commands(
                &supported,
                snapshot.as_ref(),
                &self.config.own_platform_id,
            );
            for reason in &sanitized.dropped {
                tracing::warn!(reason = %reason, "ArkSIM dropped unsafe command (snapshot validation)");
                cmd_log::log_drop(reason);
                rejected += 1;
                errors.push(openfang_types::platform::CommandError {
                    command_index: 0,
                    platform_id: self.config.own_platform_id.clone(),
                    error: reason.clone(),
                });
            }
            if sanitized.commands.is_empty() {
                return Ok(CommandResult {
                    accepted: accepted.saturating_sub(sanitized.dropped.len() as u32),
                    rejected,
                    errors,
                });
            }

            let normalized = strike_protocol::normalize_commands(&sanitized.commands);
            let batches = strike_protocol::partition_strike_batches(
                &normalized,
                self.config.auto_outside_control_self,
            );
            let runstep_after_weapon = self.config.runstep_after_weapon;
            let weapon_runstep_count = self.config.weapon_runstep_count;
            let weapon_advance_time_secs = self.config.weapon_advance_time_secs;
            let dispatch_weapon_advance = runstep_after_weapon
                && batches
                    .iter()
                    .any(|batch| strike_protocol::batch_has_weapon(batch));
            let backend = self.backend.clone().ok_or(PlatformError::NotConnected)?;
            let latest = Arc::clone(&self.latest_snapshot);

            let transport_label = match &self.backend {
                Some(ArkSimBackend::WarlockDirect(_)) => "warlock_direct",
                Some(ArkSimBackend::ArkService(_)) => "ark_service",
                None => "disconnected",
            };
            for (batch_index, batch) in batches.iter().enumerate() {
                let proto = command_mapper::to_proto_bytes(batch);
                cmd_log::log_dispatch(transport_label, batch_index, batch, &proto);
                for cmd in batch {
                    let audit = command_audit_fields(cmd);
                    // INFO (diagnostic): surface exactly which command class /
                    // platform / component / track is being issued so a Warlock
                    // crash can be correlated to the last command on the wire.
                    info!(
                        command_class = ?cmd.command_class(),
                        platform_id = audit.platform_id,
                        component_id = audit.component_id.unwrap_or("-"),
                        track_id = audit.track_id.unwrap_or("-"),
                        "ArkSIM queueing supported command (strike batch)"
                    );
                }
            }

            tokio::task::spawn_blocking(move || {
                match &backend {
                    ArkSimBackend::WarlockDirect(zmq) => {
                        // Enqueue one action proto per batch; the free-run driver
                        // thread sends each on its own step (mid_ark semantics).
                        let bridge = zmq
                            .lock()
                            .map_err(|_| "Warlock ZMQ mutex poisoned".to_string())?;
                        for batch in &batches {
                            let proto = command_mapper::to_proto_bytes(batch);
                            debug!(
                                payload_len = proto.len(),
                                batch_cmds = batch.len(),
                                "ArkSIM Warlock direct enqueue step (driver de-dups)"
                            );
                            bridge.enqueue_action(proto)?;
                        }
                        if let Some(snap) = bridge.cached_snapshot() {
                            if let Ok(mut cache) = latest.lock() {
                                *cache = Some(snap);
                            }
                        }
                    }
                    ArkSimBackend::ArkService(service) => {
                        let client = service
                            .lock()
                            .map_err(|_| "ArkService client mutex poisoned".to_string())?;
                        for batch in &batches {
                            let proto = command_mapper::to_proto_bytes(batch);
                            info!(
                                payload_len = proto.len(),
                                batch_cmds = batch.len(),
                                "ArkSIM ArkService proto batch"
                            );
                            client.send_actions(&proto)?;
                            if strike_protocol::batch_has_weapon(batch) {
                                std::thread::sleep(Duration::from_millis(100));
                            }
                        }
                        if dispatch_weapon_advance {
                            client.advance_simulation(
                                weapon_runstep_count,
                                weapon_advance_time_secs,
                            )?;
                        }
                    }
                }
                Ok::<(), String>(())
            })
            .await
            .map_err(|e| PlatformError::send(format!("ArkSIM send task join: {e}")))?
            .map_err(PlatformError::send)?;
        }

        Ok(CommandResult {
            accepted,
            rejected,
            errors,
        })
    }

    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities {
            supports_motion_control: true,
            supports_sensor_control: true,
            supports_weapon_control: true,
            supports_jammer_control: true,
            supports_comm_control: true,
            supports_uav_launch_recovery: false,
            supports_formation_control: false,
            supports_handoff: false,
            max_platforms: 200,
            supports_simulation: true,
            supports_hardware: false,
        }
    }
}

struct CommandAuditFields<'a> {
    platform_id: &'a str,
    component_id: Option<&'a str>,
    track_id: Option<&'a str>,
}

fn command_audit_fields(cmd: &PlatformCommand) -> CommandAuditFields<'_> {
    use PlatformCommand::*;
    match cmd {
        SensorOn {
            platform_id,
            sensor_id,
        }
        | SensorOff {
            platform_id,
            sensor_id,
        }
        | SensorSetMode {
            platform_id,
            sensor_id,
            ..
        } => CommandAuditFields {
            platform_id,
            component_id: Some(sensor_id),
            track_id: None,
        },
        FireAtTarget {
            platform_id,
            weapon_id,
            track_id,
        }
        | FireSalvo {
            platform_id,
            weapon_id,
            track_id,
            ..
        } => CommandAuditFields {
            platform_id,
            component_id: Some(weapon_id),
            track_id: Some(track_id),
        },
        UpdateTarget {
            platform_id,
            track_id,
        } => CommandAuditFields {
            platform_id,
            component_id: None,
            track_id: Some(track_id),
        },
        JamStart {
            platform_id,
            jammer_id,
            target_track_id,
            ..
        } => CommandAuditFields {
            platform_id,
            component_id: Some(jammer_id),
            track_id: Some(target_track_id),
        },
        JamStop {
            platform_id,
            jammer_id,
        }
        | JamSetMode {
            platform_id,
            jammer_id,
            ..
        } => CommandAuditFields {
            platform_id,
            component_id: Some(jammer_id),
            track_id: None,
        },
        SendMessage {
            from_platform_id,
            to_platform_id,
            ..
        } => CommandAuditFields {
            platform_id: from_platform_id,
            component_id: None,
            track_id: Some(to_platform_id),
        },
        ChangeCommander {
            platform_id,
            new_commander_id,
        } => CommandAuditFields {
            platform_id,
            component_id: None,
            track_id: Some(new_commander_id),
        },
        AuxCommand {
            platform_id, key, ..
        } => CommandAuditFields {
            platform_id,
            component_id: Some(key),
            track_id: None,
        },
        other => CommandAuditFields {
            platform_id: other.target_platform_id(),
            component_id: None,
            track_id: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_platform::PlatformAdapter;
    use std::time::{Duration, Instant};

    fn snapshot_at(timestamp: f64) -> WorldSnapshot {
        WorldSnapshot {
            timestamp,
            platforms: vec![],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        }
    }

    #[tokio::test]
    async fn poll_state_returns_cached_snapshot_without_waiting_for_arkservice() {
        let mut adapter = ArkSimAdapter::connected_with_cached_snapshot_for_test(snapshot_at(42.0));

        let started = Instant::now();
        let snapshot = adapter.poll_state().await.unwrap();

        assert_eq!(snapshot.timestamp, 42.0);
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "poll_state must read the cached latest snapshot, not wait on ZMQ"
        );
    }

    #[tokio::test]
    async fn disconnect_clears_cached_snapshot() {
        let mut adapter = ArkSimAdapter::connected_with_cached_snapshot_for_test(snapshot_at(7.0));

        adapter.disconnect().await.unwrap();

        assert!(adapter.poll_state().await.is_err());
    }

    #[test]
    fn transport_resolve_prefers_warlock_direct_by_default() {
        assert_eq!(
            ArkSimTransport::resolve(None, None, None),
            ArkSimTransport::WarlockDirect
        );
        assert_eq!(
            ArkSimTransport::resolve(Some("warlock_direct"), Some("/s.txt"), None),
            ArkSimTransport::WarlockDirect
        );
        assert_eq!(
            ArkSimTransport::resolve(None, Some("/s.txt"), None),
            ArkSimTransport::ArkService
        );
    }
}
