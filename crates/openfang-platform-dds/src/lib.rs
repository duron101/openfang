//! DDS (Data Distribution Service) platform adapter.
//!
//! Implements `PlatformAdapter` for real-time hardware buses used in unmanned systems.
//! Uses a pluggable `DdsTransport` trait so the actual DDS implementation
//! (rustdds, eCAL, RTI Connector, etc.) can be swapped without changing the adapter.
//!
//! # Architecture
//! - `DdsAdapter` — implements `PlatformAdapter` trait
//! - `DdsTransport` — abstract DDS I/O (pluggable backend)
//! - `publisher` — `PlatformCommand` → DDS topic writes
//! - `subscriber` — DDS topic reads → `WorldSnapshot`

pub mod loopback;
pub mod publisher;
pub mod subscriber;
pub mod types;

pub use loopback::{LoopbackMetrics, LoopbackTransport};

use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use openfang_platform::{AdapterType, PlatformAdapter, PlatformCapabilities, PlatformError};
use openfang_types::platform::{CommandResult, PlatformCommand, WorldSnapshot};
use tracing::info;

use types::DdsQosProfile;

// ── DDS Transport Trait (pluggable backend) ──

/// Abstract DDS I/O — implemented by concrete DDS libraries (rustdds, eCAL, RTI, etc.)
#[async_trait]
pub trait DdsTransport: Send + Sync {
    /// Connect to the DDS domain.
    async fn connect(&mut self, domain_id: u16) -> Result<(), String>;

    /// Disconnect from the DDS domain.
    async fn disconnect(&mut self) -> Result<(), String>;

    /// Check if connected.
    fn is_connected(&self) -> bool;

    /// Publish a message to a DDS topic.
    async fn publish(&self, topic: &str, qos: &DdsQosProfile, payload: &[u8])
        -> Result<(), String>;

    /// Subscribe to a DDS topic and return the latest cached sample.
    async fn take_next(&self, topic: &str) -> Result<Option<Vec<u8>>, String>;

    /// Subscribe with a callback (for high-frequency topics like nav position).
    async fn subscribe_callback(
        &self,
        topic: &str,
        qos: &DdsQosProfile,
        callback: Box<dyn Fn(Vec<u8>) + Send + Sync>,
    ) -> Result<(), String>;
}

/// No-op transport for testing and simulation without actual DDS hardware.
pub struct NoopTransport;

#[async_trait]
impl DdsTransport for NoopTransport {
    async fn connect(&mut self, _domain_id: u16) -> Result<(), String> {
        Ok(())
    }
    async fn disconnect(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn is_connected(&self) -> bool {
        true
    }
    async fn publish(
        &self,
        _topic: &str,
        _qos: &DdsQosProfile,
        _payload: &[u8],
    ) -> Result<(), String> {
        tracing::trace!("DDS noop publish");
        Ok(())
    }
    async fn take_next(&self, _topic: &str) -> Result<Option<Vec<u8>>, String> {
        Ok(None)
    }
    async fn subscribe_callback(
        &self,
        _topic: &str,
        _qos: &DdsQosProfile,
        _callback: Box<dyn Fn(Vec<u8>) + Send + Sync>,
    ) -> Result<(), String> {
        Ok(())
    }
}

// ── DdsAdapter ──

/// DDS platform adapter — bridges Agent to real-time DDS hardware bus.
pub struct DdsAdapter {
    transport: Box<dyn DdsTransport>,
    domain_id: u16,
    connected: bool,
    /// Latest cached WorldSnapshot from DDS subscribers — wrapped in Arc so callbacks
    /// (which may run on a DDS event thread) can hold a clone of the handle.
    latest_snapshot: Arc<DashMap<String, Vec<u8>>>,
}

#[derive(Debug, Clone)]
pub struct DdsConfig {
    pub domain_id: u16,
    /// QoS profiles for different topic categories
    pub nav_qos: DdsQosProfile,
    pub sensor_qos: DdsQosProfile,
    pub weapon_qos: DdsQosProfile,
}

impl Default for DdsConfig {
    fn default() -> Self {
        Self {
            domain_id: 0,
            nav_qos: DdsQosProfile::reliable_keep_last(10),
            sensor_qos: DdsQosProfile::best_effort_keep_last(5),
            weapon_qos: DdsQosProfile::reliable_transient_local(),
        }
    }
}

impl DdsAdapter {
    /// Create with a specific transport backend.
    pub fn with_transport(transport: Box<dyn DdsTransport>, config: DdsConfig) -> Self {
        Self {
            transport,
            domain_id: config.domain_id,
            connected: false,
            latest_snapshot: Arc::new(DashMap::new()),
        }
    }

    /// Create with noop transport (for testing).
    pub fn new_noop() -> Self {
        Self::with_transport(Box::new(NoopTransport), DdsConfig::default())
    }

    async fn subscribe_cached(
        &self,
        topic: &'static str,
        qos: DdsQosProfile,
    ) -> Result<(), PlatformError> {
        let snap = Arc::clone(&self.latest_snapshot);
        self.transport
            .subscribe_callback(
                topic,
                &qos,
                Box::new(move |data| {
                    snap.insert(topic.into(), data);
                }),
            )
            .await
            .map_err(PlatformError::Internal)
    }

    async fn take_or_cached(&self, topic: &str) -> Result<Option<Vec<u8>>, PlatformError> {
        let live = self
            .transport
            .take_next(topic)
            .await
            .map_err(PlatformError::poll)?;

        if live.is_some() {
            return Ok(live);
        }

        Ok(self
            .latest_snapshot
            .get(topic)
            .map(|entry| entry.value().clone()))
    }
}

#[async_trait]
impl PlatformAdapter for DdsAdapter {
    fn adapter_id(&self) -> &str {
        "dds-primary"
    }

    fn adapter_type(&self) -> AdapterType {
        AdapterType::Dds
    }

    async fn connect(&mut self) -> Result<(), PlatformError> {
        self.transport
            .connect(self.domain_id)
            .await
            .map_err(PlatformError::conn)?;

        self.subscribe_cached("nav/NavPosition", DdsQosProfile::reliable_keep_last(10))
            .await?;
        self.subscribe_cached(
            "sensor/RadarTrack",
            DdsQosProfile::best_effort_keep_last(20),
        )
        .await?;
        self.subscribe_cached("platform/Heartbeat", DdsQosProfile::reliable_keep_last(5))
            .await?;

        self.connected = true;
        info!("DDS adapter connected to domain {}", self.domain_id);
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), PlatformError> {
        self.transport
            .disconnect()
            .await
            .map_err(PlatformError::Internal)?;
        self.connected = false;
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected
    }

    async fn poll_state(&mut self) -> Result<WorldSnapshot, PlatformError> {
        // Read latest nav position
        let nav_data = self.take_or_cached("nav/NavPosition").await?;

        // Read latest sensor tracks
        let track_data = self.take_or_cached("sensor/RadarTrack").await?;

        // Read heartbeat
        let hb_data = self.take_or_cached("platform/Heartbeat").await?;

        subscriber::build_snapshot_from_dds(nav_data, track_data, hb_data)
    }

    async fn send_commands(
        &mut self,
        commands: &[PlatformCommand],
    ) -> Result<CommandResult, PlatformError> {
        let mut accepted = 0u32;
        let mut rejected = 0u32;

        for cmd in commands {
            let result = publisher::publish_command(&*self.transport, cmd).await;
            match result {
                Ok(_) => accepted += 1,
                Err(e) => {
                    tracing::warn!("DDS publish failed for {:?}: {e}", cmd);
                    rejected += 1;
                }
            }
        }

        Ok(CommandResult {
            accepted,
            rejected,
            errors: vec![],
        })
    }

    fn capabilities(&self) -> PlatformCapabilities {
        // Honest declaration: only the command families with a real DDS topic
        // mapping in `publisher.rs` are advertised. Motion/sensor/weapon and the
        // fleet ops (launch-recovery/handoff via `fleet/FleetCommand`, Track 2
        // §2B) are typed. Jammer / comm / formation still fall through to the
        // generic `platform/AuxCommand` pass-through, so remain unsupported.
        PlatformCapabilities {
            supports_motion_control: true,
            supports_sensor_control: true,
            supports_weapon_control: true,
            supports_jammer_control: false,
            supports_comm_control: false,
            supports_uav_launch_recovery: true,
            supports_formation_control: false,
            supports_handoff: true,
            max_platforms: 50,
            supports_simulation: false,
            supports_hardware: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_noop_transport() {
        let adapter = DdsAdapter::new_noop();
        assert_eq!(adapter.adapter_id(), "dds-primary");
        assert_eq!(adapter.adapter_type(), AdapterType::Dds);
        assert!(adapter.capabilities().supports_hardware);
    }
}
