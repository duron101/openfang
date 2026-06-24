//! MAVLink platform adapter.
//!
//! Implements [`PlatformAdapter`] for a single UAV (CCA / LSUAV) speaking
//! MAVLink to an autopilot (PX4 / ArduPilot, SITL or real). A bare autopilot
//! link carries **motion + telemetry only** — there is no weapon or electronic-
//! warfare contract over MAVLink, so those capabilities are reported `false` and
//! such commands are rejected (never silently dropped). Weapon authority stays
//! exclusively behind the kernel CommandGate (the Iron Law).
//!
//! # Architecture
//! - [`MavlinkAdapter`] — implements [`PlatformAdapter`]
//! - [`MavlinkTransport`] — pluggable byte transport (UDP/serial/SITL)
//! - [`LoopbackTransport`] — in-process transport for the vertical-slice tests
//! - [`codec`] — `PlatformCommand` ⇄ MAVLink frame / telemetry mapping

pub mod codec;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use openfang_platform::{AdapterType, PlatformAdapter, PlatformCapabilities, PlatformError};
use openfang_types::platform::{CommandResult, PlatformCommand, WorldSnapshot};
use tracing::info;

pub use codec::{MavFrame, MavTelemetry};

/// Pluggable MAVLink byte transport. Concrete impls wrap UDP (SITL), serial
/// radio links, etc.
#[async_trait]
pub trait MavlinkTransport: Send + Sync {
    async fn connect(&mut self) -> Result<(), String>;
    async fn disconnect(&mut self) -> Result<(), String>;
    fn is_connected(&self) -> bool;
    /// Send one encoded uplink frame.
    async fn send_frame(&self, frame: &MavFrame) -> Result<(), String>;
    /// Read the latest telemetry sample, if any.
    async fn latest_telemetry(&self) -> Result<Option<MavTelemetry>, String>;
}

/// In-process transport for tests / SITL-less vertical slices. Records sent
/// frames and serves an injectable telemetry sample.
#[derive(Default)]
pub struct LoopbackTransport {
    connected: Mutex<bool>,
    sent: Arc<Mutex<Vec<MavFrame>>>,
    telemetry: Arc<Mutex<Option<MavTelemetry>>>,
}

impl LoopbackTransport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Handle to inspect sent frames (test/debug).
    pub fn sent_frames(&self) -> Vec<MavFrame> {
        self.sent.lock().unwrap().clone()
    }

    /// Inject a telemetry sample that the next `poll_state` will observe.
    pub fn set_telemetry(&self, t: MavTelemetry) {
        *self.telemetry.lock().unwrap() = Some(t);
    }
}

#[async_trait]
impl MavlinkTransport for LoopbackTransport {
    async fn connect(&mut self) -> Result<(), String> {
        *self.connected.lock().unwrap() = true;
        Ok(())
    }
    async fn disconnect(&mut self) -> Result<(), String> {
        *self.connected.lock().unwrap() = false;
        Ok(())
    }
    fn is_connected(&self) -> bool {
        *self.connected.lock().unwrap()
    }
    async fn send_frame(&self, frame: &MavFrame) -> Result<(), String> {
        self.sent.lock().unwrap().push(frame.clone());
        Ok(())
    }
    async fn latest_telemetry(&self) -> Result<Option<MavTelemetry>, String> {
        Ok(self.telemetry.lock().unwrap().clone())
    }
}

/// MAVLink adapter — bridges the Agent to a single autopilot.
pub struct MavlinkAdapter {
    adapter_id: String,
    platform_id: String,
    transport: Box<dyn MavlinkTransport>,
    connected: bool,
}

impl MavlinkAdapter {
    pub fn with_transport(
        adapter_id: impl Into<String>,
        platform_id: impl Into<String>,
        transport: Box<dyn MavlinkTransport>,
    ) -> Self {
        Self {
            adapter_id: adapter_id.into(),
            platform_id: platform_id.into(),
            transport,
            connected: false,
        }
    }

    /// Loopback-backed adapter for tests / SITL-less slices.
    pub fn new_loopback(platform_id: impl Into<String>) -> Self {
        Self::with_transport(
            "mavlink-loopback",
            platform_id,
            Box::new(LoopbackTransport::new()),
        )
    }
}

#[async_trait]
impl PlatformAdapter for MavlinkAdapter {
    fn adapter_id(&self) -> &str {
        &self.adapter_id
    }

    fn adapter_type(&self) -> AdapterType {
        AdapterType::Mavlink
    }

    async fn connect(&mut self) -> Result<(), PlatformError> {
        self.transport
            .connect()
            .await
            .map_err(PlatformError::conn)?;
        self.connected = true;
        info!("MAVLink adapter '{}' connected", self.adapter_id);
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), PlatformError> {
        self.transport
            .disconnect()
            .await
            .map_err(PlatformError::DisconnectFailed)?;
        self.connected = false;
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected
    }

    async fn poll_state(&mut self) -> Result<WorldSnapshot, PlatformError> {
        let telem = self
            .transport
            .latest_telemetry()
            .await
            .map_err(PlatformError::poll)?;
        match telem {
            Some(t) => Ok(codec::telemetry_to_snapshot(&t, &self.platform_id)),
            None => Ok(WorldSnapshot {
                timestamp: 0.0,
                platforms: vec![],
                active_munitions: vec![],
                events: vec![],
                fleet: None,
            }),
        }
    }

    async fn send_commands(
        &mut self,
        commands: &[PlatformCommand],
    ) -> Result<CommandResult, PlatformError> {
        let mut accepted = 0u32;
        let mut rejected = 0u32;
        let mut errors = Vec::new();

        for (idx, cmd) in commands.iter().enumerate() {
            match codec::command_to_mavlink(cmd) {
                Some(frame) => match self.transport.send_frame(&frame).await {
                    Ok(_) => accepted += 1,
                    Err(e) => {
                        rejected += 1;
                        errors.push(openfang_types::platform::CommandError {
                            command_index: idx,
                            platform_id: self.platform_id.clone(),
                            error: e,
                        });
                    }
                },
                None => {
                    // Honest rejection: not part of the MAVLink autopilot contract.
                    rejected += 1;
                    errors.push(openfang_types::platform::CommandError {
                        command_index: idx,
                        platform_id: self.platform_id.clone(),
                        error: "command unsupported over MAVLink autopilot link".into(),
                    });
                }
            }
        }

        Ok(CommandResult {
            accepted,
            rejected,
            errors,
        })
    }

    fn capabilities(&self) -> PlatformCapabilities {
        // Bare autopilot link: motion + telemetry + heartbeat only.
        PlatformCapabilities {
            supports_motion_control: true,
            supports_sensor_control: false,
            supports_weapon_control: false,
            supports_jammer_control: false,
            supports_comm_control: true,
            supports_uav_launch_recovery: false,
            supports_formation_control: false,
            supports_handoff: false,
            max_platforms: 1,
            supports_simulation: true,
            supports_hardware: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_send_motion_and_reject_weapon() {
        let mut adapter = MavlinkAdapter::new_loopback("uav-1");
        adapter.connect().await.unwrap();
        assert!(adapter.is_connected());

        let cmds = vec![
            PlatformCommand::SetHeading {
                platform_id: "uav-1".into(),
                heading_deg: 270.0,
                speed_ms: Some(140.0),
                turn_direction: None,
            },
            PlatformCommand::FireAtTarget {
                platform_id: "uav-1".into(),
                weapon_id: "aam".into(),
                track_id: "trk".into(),
            },
        ];
        let res = adapter.send_commands(&cmds).await.unwrap();
        assert_eq!(res.accepted, 1);
        assert_eq!(res.rejected, 1);
    }

    #[tokio::test]
    async fn poll_builds_air_snapshot_from_telemetry() {
        let transport = LoopbackTransport::new();
        transport.set_telemetry(MavTelemetry {
            lat_deg: 30.0,
            lon_deg: 120.0,
            alt_m: 1800.0,
            heading_deg: 10.0,
            groundspeed_ms: 130.0,
            climb_ms: 2.0,
            energy_remaining_pct: 0.6,
            ..Default::default()
        });
        let mut adapter = MavlinkAdapter::with_transport("mav", "uav-9", Box::new(transport));
        adapter.connect().await.unwrap();
        let snap = adapter.poll_state().await.unwrap();
        assert_eq!(snap.platforms.len(), 1);
        assert_eq!(snap.platforms[0].id, "uav-9");
        assert_eq!(
            snap.platforms[0].domain,
            openfang_types::platform::Domain::Air
        );
    }

    #[test]
    fn capabilities_have_no_weapons_or_ew() {
        let adapter = MavlinkAdapter::new_loopback("uav-1");
        let caps = adapter.capabilities();
        assert!(caps.supports_motion_control);
        assert!(!caps.supports_weapon_control);
        assert!(!caps.supports_jammer_control);
    }
}
