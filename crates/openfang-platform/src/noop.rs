//! NoopAdapter — accepts gate-approved commands and discards them.
//!
//! Used for the Phase 2 safe command closed loop: the full pipeline (intent →
//! compose → gate → audit → adapter) runs end to end, but nothing actuates a
//! real effector. It records a count so tests can assert exactly which commands
//! reached the "actuator" boundary after passing the gate.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use openfang_types::platform::{
    CommandResult, PlatformCapabilities, PlatformCommand, WorldSnapshot,
};

use crate::{AdapterType, PlatformAdapter, PlatformError};

/// A do-nothing adapter that always accepts commands.
#[derive(Default)]
pub struct NoopAdapter {
    accepted: Arc<AtomicU64>,
    caps: PlatformCapabilities,
}

impl NoopAdapter {
    pub fn new() -> Self {
        Self {
            accepted: Arc::new(AtomicU64::new(0)),
            caps: PlatformCapabilities {
                supports_motion_control: true,
                supports_sensor_control: true,
                supports_weapon_control: true,
                supports_jammer_control: true,
                supports_comm_control: true,
                supports_uav_launch_recovery: true,
                supports_formation_control: true,
                supports_handoff: true,
                max_platforms: 64,
                supports_simulation: true,
                supports_hardware: false,
            },
        }
    }

    /// Total commands accepted across the adapter's lifetime.
    pub fn accepted_count(&self) -> u64 {
        self.accepted.load(Ordering::SeqCst)
    }

    /// Shared counter handle (survives moving the adapter into a registry).
    pub fn counter(&self) -> Arc<AtomicU64> {
        self.accepted.clone()
    }
}

#[async_trait]
impl PlatformAdapter for NoopAdapter {
    fn adapter_id(&self) -> &str {
        "noop"
    }

    fn adapter_type(&self) -> AdapterType {
        AdapterType::Custom("noop")
    }

    async fn connect(&mut self) -> Result<(), PlatformError> {
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), PlatformError> {
        Ok(())
    }

    fn is_connected(&self) -> bool {
        true
    }

    async fn poll_state(&mut self) -> Result<WorldSnapshot, PlatformError> {
        Ok(WorldSnapshot {
            timestamp: 0.0,
            platforms: vec![],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        })
    }

    async fn send_commands(
        &mut self,
        commands: &[PlatformCommand],
    ) -> Result<CommandResult, PlatformError> {
        self.accepted
            .fetch_add(commands.len() as u64, Ordering::SeqCst);
        Ok(CommandResult::all_accepted(commands.len() as u32))
    }

    fn capabilities(&self) -> PlatformCapabilities {
        self.caps.clone()
    }
}
