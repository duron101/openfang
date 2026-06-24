use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::RwLock;
use tracing::{debug, warn};

use crate::{AdapterType, PlatformAdapter, PlatformError};
use openfang_types::platform::{
    CommandResult, PlatformCapabilities, PlatformCommand, WorldSnapshot,
};

/// Manages multiple platform adapters with platform-id-based routing.
///
/// Supports three deployment modes:
/// - **simulation**: single primary adapter (e.g. ArkSim)
/// - **hardware**: single primary adapter (e.g. DDS)
/// - **hybrid**: primary + secondary adapters, routed by platform_id
pub struct AdapterRegistry {
    /// Primary adapter — receives all undirected commands
    primary: RwLock<Option<Box<dyn PlatformAdapter>>>,
    /// Secondary adapters — receive commands for specific platforms
    secondary: DashMap<String, Box<dyn PlatformAdapter>>,
    /// Platform → adapter routing map
    platform_routing: DashMap<String, String>,
}

impl AdapterRegistry {
    pub fn new() -> Self {
        Self {
            primary: RwLock::new(None),
            secondary: DashMap::new(),
            platform_routing: DashMap::new(),
        }
    }

    /// Set the primary adapter (simulation or main hardware bus)
    pub fn set_primary(&self, adapter: Box<dyn PlatformAdapter>) {
        *self.primary.write().unwrap() = Some(adapter);
    }

    /// Take the primary adapter out (for reconfiguration)
    pub fn take_primary(&self) -> Option<Box<dyn PlatformAdapter>> {
        self.primary.write().unwrap().take()
    }

    /// Register a secondary adapter for specific platforms (hybrid mode)
    pub fn add_secondary(&self, adapter: Box<dyn PlatformAdapter>) {
        let id = adapter.adapter_id().to_string();
        self.secondary.insert(id, adapter);
    }

    /// Map a platform to a specific adapter
    pub fn route_platform(&self, platform_id: &str, adapter_id: &str) {
        self.platform_routing
            .insert(platform_id.to_string(), adapter_id.to_string());
    }

    /// Resolve which adapter handles a given platform
    pub fn adapter_for_platform(&self, platform_id: &str) -> Option<AdapterType> {
        // Check routing map first
        if let Some(adapter_id) = self.platform_routing.get(platform_id) {
            if self.secondary.contains_key(adapter_id.value()) {
                return Some(AdapterType::Custom("secondary"));
            }
        }
        // Fall back to primary
        if self.primary.read().unwrap().is_some() {
            return self
                .primary
                .read()
                .unwrap()
                .as_ref()
                .map(|a| a.adapter_type());
        }
        None
    }

    /// Resolve the secondary adapter id (if any) responsible for a platform.
    fn secondary_for(&self, platform_id: &str) -> Option<String> {
        let adapter_id = self.platform_routing.get(platform_id)?;
        let id = adapter_id.value().clone();
        if self.secondary.contains_key(&id) {
            Some(id)
        } else {
            None
        }
    }

    /// Snapshot of secondary adapter ids (sync; no lock held across await).
    fn secondary_ids(&self) -> Vec<String> {
        self.secondary.iter().map(|e| e.key().clone()).collect()
    }

    /// Connect the primary and all secondary adapters. Errors are aggregated;
    /// the first failure is returned after attempting every adapter.
    ///
    /// Adapters are moved out of their lock for the duration of the (async)
    /// `connect` call and put back afterward, so the returned future stays
    /// `Send` and is callable from `#[async_trait]` contexts.
    pub async fn connect_all(&self) -> Result<(), PlatformError> {
        let mut first_err: Option<PlatformError> = None;

        if let Some(mut p) = self.take_primary() {
            if !p.is_connected() {
                if let Err(e) = p.connect().await {
                    first_err.get_or_insert(e);
                }
            }
            self.set_primary(p);
        }

        for id in self.secondary_ids() {
            if let Some((key, mut a)) = self.secondary.remove(&id) {
                if !a.is_connected() {
                    if let Err(e) = a.connect().await {
                        first_err.get_or_insert(e);
                    }
                }
                self.secondary.insert(key, a);
            }
        }

        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Disconnect the primary and all secondary adapters (best effort).
    pub async fn disconnect_all(&self) {
        if let Some(mut p) = self.take_primary() {
            let _ = p.disconnect().await;
            self.set_primary(p);
        }
        for id in self.secondary_ids() {
            if let Some((key, mut a)) = self.secondary.remove(&id) {
                let _ = a.disconnect().await;
                self.secondary.insert(key, a);
            }
        }
    }

    /// Route a batch of commands to the appropriate adapters.
    ///
    /// Commands are grouped by their target adapter (resolved via
    /// `platform_routing` → secondary adapter, else the primary), then each
    /// group is dispatched and the results are aggregated. This honors hybrid
    /// deployments (e.g. MAVLink for motion, DDS for payload).
    pub async fn route_commands(
        &self,
        commands: &[PlatformCommand],
    ) -> Result<CommandResult, PlatformError> {
        if commands.is_empty() {
            return Ok(CommandResult::all_accepted(0));
        }

        // Partition commands: primary group + per-secondary-adapter groups.
        let mut primary_group: Vec<PlatformCommand> = Vec::new();
        let mut secondary_groups: HashMap<String, Vec<PlatformCommand>> = HashMap::new();
        for cmd in commands {
            let pid = cmd.target_platform_id();
            match self.secondary_for(pid) {
                Some(adapter_id) => secondary_groups
                    .entry(adapter_id)
                    .or_default()
                    .push(cmd.clone()),
                None => primary_group.push(cmd.clone()),
            }
        }

        let mut total = CommandResult::all_accepted(0);

        // Dispatch the primary group (move adapter out for the await).
        if !primary_group.is_empty() {
            let mut p = self.take_primary().ok_or(PlatformError::NotConnected)?;
            if !p.is_connected() {
                self.set_primary(p);
                warn!("Primary adapter not connected, cannot send commands");
                return Err(PlatformError::NotConnected);
            }
            let res = p.send_commands(&primary_group).await;
            self.set_primary(p);
            merge_result(&mut total, &res?);
        }

        // Dispatch each secondary group.
        for (adapter_id, group) in secondary_groups {
            if group.is_empty() {
                continue;
            }
            let (key, mut a) = self
                .secondary
                .remove(&adapter_id)
                .ok_or(PlatformError::NotConnected)?;
            if !a.is_connected() {
                self.secondary.insert(key, a);
                warn!(adapter = %adapter_id, "Secondary adapter not connected");
                return Err(PlatformError::NotConnected);
            }
            let res = a.send_commands(&group).await;
            self.secondary.insert(key, a);
            merge_result(&mut total, &res?);
        }

        debug!(
            accepted = total.accepted,
            rejected = total.rejected,
            "Commands routed"
        );
        Ok(total)
    }

    /// Poll all adapters for world state and merge them into one snapshot.
    /// The primary supplies the base timestamp; secondary platforms / tracks /
    /// munitions / events are appended.
    pub async fn poll_all(&self) -> Result<WorldSnapshot, PlatformError> {
        let mut merged = {
            let mut p = self.take_primary().ok_or(PlatformError::NotConnected)?;
            if !p.is_connected() {
                self.set_primary(p);
                return Err(PlatformError::NotConnected);
            }
            let res = p.poll_state().await;
            self.set_primary(p);
            res?
        };

        for id in self.secondary_ids() {
            if let Some((key, mut a)) = self.secondary.remove(&id) {
                let res = if a.is_connected() {
                    a.poll_state().await.ok()
                } else {
                    None
                };
                self.secondary.insert(key, a);
                if let Some(snap) = res {
                    merge_snapshot(&mut merged, snap);
                }
            }
        }

        Ok(merged)
    }

    /// Number of registered secondary adapters
    pub fn secondary_count(&self) -> usize {
        self.secondary.len()
    }

    /// Whether a primary adapter has been set.
    pub fn has_primary(&self) -> bool {
        self.primary.read().unwrap().is_some()
    }

    /// Whether any adapter is connected
    pub fn any_connected(&self) -> bool {
        self.primary
            .read()
            .unwrap()
            .as_ref()
            .map(|a| a.is_connected())
            .unwrap_or(false)
    }

    /// Aggregate capabilities across all configured adapters.
    ///
    /// The command gate consumes one capability bitmap for a tick. In hybrid
    /// deployments, a command may be routable through any adapter, so this is
    /// the union of the primary and secondary capabilities.
    pub fn combined_capabilities(&self) -> PlatformCapabilities {
        let mut caps = PlatformCapabilities::default();
        if let Some(ref adapter) = *self.primary.read().unwrap() {
            merge_capabilities(&mut caps, &adapter.capabilities());
        }
        for entry in self.secondary.iter() {
            merge_capabilities(&mut caps, &entry.value().capabilities());
        }
        caps
    }

    /// List all adapter IDs
    pub fn list_adapters(&self) -> Vec<String> {
        let mut ids = Vec::new();
        if let Some(ref a) = *self.primary.read().unwrap() {
            ids.push(format!("primary:{}", a.adapter_id()));
        }
        for entry in self.secondary.iter() {
            ids.push(format!("secondary:{}", entry.key()));
        }
        ids
    }
}

impl Default for AdapterRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Accumulate a per-adapter [`CommandResult`] into a running total.
fn merge_result(total: &mut CommandResult, res: &CommandResult) {
    total.accepted += res.accepted;
    total.rejected += res.rejected;
    total.errors.extend(res.errors.iter().cloned());
}

fn merge_capabilities(total: &mut PlatformCapabilities, caps: &PlatformCapabilities) {
    total.supports_motion_control |= caps.supports_motion_control;
    total.supports_sensor_control |= caps.supports_sensor_control;
    total.supports_weapon_control |= caps.supports_weapon_control;
    total.supports_jammer_control |= caps.supports_jammer_control;
    total.supports_comm_control |= caps.supports_comm_control;
    total.supports_uav_launch_recovery |= caps.supports_uav_launch_recovery;
    total.supports_formation_control |= caps.supports_formation_control;
    total.supports_handoff |= caps.supports_handoff;
    total.max_platforms = total.max_platforms.max(caps.max_platforms);
    total.supports_simulation |= caps.supports_simulation;
    total.supports_hardware |= caps.supports_hardware;
}

/// Merge a secondary adapter's snapshot into the primary-based merged snapshot.
/// Platforms, munitions, and events are appended; the latest timestamp wins.
fn merge_snapshot(base: &mut WorldSnapshot, other: WorldSnapshot) {
    base.timestamp = base.timestamp.max(other.timestamp);
    base.platforms.extend(other.platforms);
    base.active_munitions.extend(other.active_munitions);
    base.events.extend(other.events);
    // Merge fleet pictures: keep the base if present, otherwise adopt the other.
    match (&mut base.fleet, other.fleet) {
        (Some(base_fleet), Some(other_fleet)) => {
            base_fleet.uavs.extend(other_fleet.uavs);
        }
        (slot @ None, Some(other_fleet)) => {
            *slot = Some(other_fleet);
        }
        _ => {}
    }
}

#[cfg(test)]
mod routing_tests {
    use super::*;
    use crate::MockAdapter;
    use openfang_types::platform::{PlatformState, WorldSnapshot};

    fn snapshot_with_platform(ts: f64, pid: &str) -> WorldSnapshot {
        WorldSnapshot {
            timestamp: ts,
            platforms: vec![PlatformState::minimal(pid)],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        }
    }

    #[tokio::test]
    async fn route_commands_splits_primary_and_secondary() {
        let reg = AdapterRegistry::new();
        let primary = MockAdapter::new("primary");
        let primary_log = primary.sent_handle();
        let secondary = MockAdapter::new("dds-2");
        let secondary_log = secondary.sent_handle();
        reg.set_primary(Box::new(primary));
        reg.add_secondary(Box::new(secondary));
        reg.route_platform("uav-9", "dds-2");
        reg.connect_all().await.unwrap();

        let cmds = vec![
            PlatformCommand::SetSpeed {
                platform_id: "usv-1".into(),
                speed_ms: 5.0,
                acceleration_ms2: None,
            },
            PlatformCommand::ReturnToBase {
                uav_id: "uav-9".into(),
            },
        ];
        let res = reg.route_commands(&cmds).await.unwrap();
        assert_eq!(res.accepted, 2);
        assert_eq!(
            primary_log.lock().unwrap().len(),
            1,
            "primary got the usv cmd"
        );
        assert_eq!(
            secondary_log.lock().unwrap().len(),
            1,
            "secondary got the routed uav cmd"
        );
    }

    #[tokio::test]
    async fn poll_all_merges_secondary_platforms() {
        let reg = AdapterRegistry::new();
        reg.set_primary(Box::new(
            MockAdapter::new("primary").with_snapshot(snapshot_with_platform(10.0, "usv-1")),
        ));
        reg.add_secondary(Box::new(
            MockAdapter::new("dds-2").with_snapshot(snapshot_with_platform(12.0, "uav-9")),
        ));
        reg.connect_all().await.unwrap();

        let snap = reg.poll_all().await.unwrap();
        assert_eq!(snap.timestamp, 12.0);
        assert_eq!(snap.platforms.len(), 2);
    }
}
