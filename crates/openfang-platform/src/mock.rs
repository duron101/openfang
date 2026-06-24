//! MockAdapter — a deterministic in-memory [`PlatformAdapter`] for tests and
//! the read-only state closed loop.
//!
//! It returns scripted [`WorldSnapshot`]s from `poll_state` and *records* every
//! command passed to `send_commands` (without acting on them). Tests use the
//! recorded list to assert, e.g., that a read-only tool path never produced a
//! command, or that the gate emitted exactly the expected commands.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use openfang_types::platform::{
    CommandResult, PlatformCapabilities, PlatformCommand, WorldSnapshot,
};

use crate::{AdapterType, PlatformAdapter, PlatformError};

/// A scriptable, introspectable platform adapter.
pub struct MockAdapter {
    id: String,
    connected: bool,
    /// Scripted snapshots returned in order; when empty, `fallback` is returned.
    scripted: VecDeque<WorldSnapshot>,
    fallback: WorldSnapshot,
    /// Every command ever passed to `send_commands`, in order.
    sent: Arc<Mutex<Vec<PlatformCommand>>>,
    caps: PlatformCapabilities,
}

impl MockAdapter {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            connected: false,
            scripted: VecDeque::new(),
            fallback: empty_snapshot(),
            sent: Arc::new(Mutex::new(Vec::new())),
            caps: default_caps(),
        }
    }

    /// Set the fallback snapshot returned once the scripted queue is exhausted.
    pub fn with_snapshot(mut self, snapshot: WorldSnapshot) -> Self {
        self.fallback = snapshot;
        self
    }

    /// Push a scripted snapshot to be returned on the next `poll_state`.
    pub fn push_snapshot(&mut self, snapshot: WorldSnapshot) {
        self.scripted.push_back(snapshot);
    }

    /// Override the declared capabilities.
    pub fn with_capabilities(mut self, caps: PlatformCapabilities) -> Self {
        self.caps = caps;
        self
    }

    /// Shared handle to the recorded command log (for assertions).
    pub fn sent_handle(&self) -> Arc<Mutex<Vec<PlatformCommand>>> {
        self.sent.clone()
    }

    /// Number of commands recorded so far.
    pub fn sent_count(&self) -> usize {
        self.sent.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Clone of the recorded commands.
    pub fn sent_commands(&self) -> Vec<PlatformCommand> {
        self.sent.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[async_trait]
impl PlatformAdapter for MockAdapter {
    fn adapter_id(&self) -> &str {
        &self.id
    }

    fn adapter_type(&self) -> AdapterType {
        AdapterType::Custom("mock")
    }

    async fn connect(&mut self) -> Result<(), PlatformError> {
        self.connected = true;
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), PlatformError> {
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
        Ok(self
            .scripted
            .pop_front()
            .unwrap_or_else(|| self.fallback.clone()))
    }

    async fn send_commands(
        &mut self,
        commands: &[PlatformCommand],
    ) -> Result<CommandResult, PlatformError> {
        if !self.connected {
            return Err(PlatformError::NotConnected);
        }
        self.sent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(commands);
        Ok(CommandResult::all_accepted(commands.len() as u32))
    }

    fn capabilities(&self) -> PlatformCapabilities {
        self.caps.clone()
    }
}

fn empty_snapshot() -> WorldSnapshot {
    WorldSnapshot {
        timestamp: 0.0,
        platforms: vec![],
        active_munitions: vec![],
        events: vec![],
        fleet: None,
    }
}

fn default_caps() -> PlatformCapabilities {
    PlatformCapabilities {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn poll_returns_scripted_then_fallback() {
        let fallback = WorldSnapshot {
            timestamp: 99.0,
            platforms: vec![],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        };
        let mut a = MockAdapter::new("m1").with_snapshot(fallback);
        a.push_snapshot(WorldSnapshot {
            timestamp: 1.0,
            platforms: vec![],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        });
        a.connect().await.unwrap();
        assert_eq!(a.poll_state().await.unwrap().timestamp, 1.0);
        assert_eq!(a.poll_state().await.unwrap().timestamp, 99.0);
    }

    #[tokio::test]
    async fn records_sent_commands() {
        let mut a = MockAdapter::new("m1");
        a.connect().await.unwrap();
        assert_eq!(a.sent_count(), 0);
        let cmd = PlatformCommand::SetHeading {
            platform_id: "usv-01".into(),
            heading_deg: 90.0,
            speed_ms: None,
            turn_direction: None,
        };
        a.send_commands(&[cmd]).await.unwrap();
        assert_eq!(a.sent_count(), 1);
    }

    #[tokio::test]
    async fn poll_before_connect_errors() {
        let mut a = MockAdapter::new("m1");
        assert!(a.poll_state().await.is_err());
    }
}
