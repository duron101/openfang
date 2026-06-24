//! UMAA Health Monitoring Service (HMA).
//!
//! Tracks component-level health, schedules periodic BIT (Built-In Test),
//! generates UMAA-compatible HealthReport. Per PRD §12.2.1.

use dashmap::DashMap;
use openfang_types::umaa::*;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Per-component health monitor entry.
struct ComponentEntry {
    health: ComponentHealth,
}

/// Health Monitor — tracks every registered component's health and BIT results.
pub struct HealthMonitor {
    platform_id: String,
    components: DashMap<String, ComponentEntry>,
    bit_interval: Duration,
    last_bit_run: Option<Instant>,
}

impl HealthMonitor {
    pub fn new(platform_id: impl Into<String>) -> Self {
        Self {
            platform_id: platform_id.into(),
            components: DashMap::new(),
            bit_interval: Duration::from_secs(60),
            last_bit_run: None,
        }
    }

    /// Register a component for health tracking.
    pub fn register_component(&self, name: impl Into<String>) {
        let name = name.into();
        self.components.insert(
            name.clone(),
            ComponentEntry {
                health: ComponentHealth {
                    component: name,
                    status: HealthStatus::Unknown,
                    last_bit_result: None,
                    error_count_since_boot: 0,
                    uptime_s: 0.0,
                    resource_usage: ResourceUsage::default(),
                },
            },
        );
    }

    /// Update a component's status.
    pub fn set_status(&self, component: &str, status: HealthStatus) {
        if let Some(mut entry) = self.components.get_mut(component) {
            entry.health.status = status;
        }
    }

    /// Record a BIT result.
    pub fn record_bit(&self, component: &str, bit: BitResult) {
        if let Some(mut entry) = self.components.get_mut(component) {
            if bit.passed {
                // Passing BIT promotes the component to Nominal (unless it was already Inoperable)
                if entry.health.status != HealthStatus::Inoperable {
                    entry.health.status = HealthStatus::Nominal;
                }
            } else {
                entry.health.error_count_since_boot += 1;
                entry.health.status = HealthStatus::Degraded;
            }
            entry.health.last_bit_result = Some(bit);
        }
    }

    /// Update resource usage for a component.
    pub fn update_resources(&self, component: &str, usage: ResourceUsage) {
        if let Some(mut entry) = self.components.get_mut(component) {
            entry.health.resource_usage = usage;
        }
    }

    /// Tick the monitor (call periodically). Returns true if a BIT run is due.
    pub fn tick(&mut self) -> bool {
        let now = Instant::now();
        let due = self
            .last_bit_run
            .map(|t| now.duration_since(t) >= self.bit_interval)
            .unwrap_or(true);
        if due {
            self.last_bit_run = Some(now);
            // Update uptime for each component (monotonic since register)
            for mut entry in self.components.iter_mut() {
                entry.health.uptime_s += self.bit_interval.as_secs_f64();
            }
        }
        due
    }

    /// Generate a UMAA HealthReport.
    pub fn report(&self) -> HealthReport {
        let mut components: Vec<ComponentHealth> =
            self.components.iter().map(|e| e.health.clone()).collect();
        // Worst-of-all status
        let overall = components
            .iter()
            .map(|c| c.status)
            .max_by_key(|s| match s {
                HealthStatus::Nominal => 0,
                HealthStatus::Degraded => 1,
                HealthStatus::Maintenance => 2,
                HealthStatus::Unknown => 3,
                HealthStatus::Inoperable => 4,
            })
            .unwrap_or(HealthStatus::Unknown);
        components.sort_by(|a, b| a.component.cmp(&b.component));
        HealthReport {
            platform_id: self.platform_id.clone(),
            overall_status: overall,
            components,
            active_alerts: vec![],
            generated_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
        }
    }

    /// Quick predicate: is every required component operational?
    pub fn all_operational(&self) -> bool {
        self.components
            .iter()
            .all(|e| e.health.status.is_operational())
    }

    pub fn component_count(&self) -> usize {
        self.components.len()
    }
}

/// Builder for BitResult.
pub fn bit_passed(component: &str, test_name: &str) -> BitResult {
    BitResult {
        component: component.to_string(),
        test_name: test_name.to_string(),
        passed: true,
        fault_code: None,
        timestamp: now_f64(),
        recommended_action: None,
    }
}

pub fn bit_failed(component: &str, test_name: &str, fault_code: &str, action: &str) -> BitResult {
    BitResult {
        component: component.to_string(),
        test_name: test_name.to_string(),
        passed: false,
        fault_code: Some(fault_code.to_string()),
        timestamp: now_f64(),
        recommended_action: Some(action.to_string()),
    }
}

fn now_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// Re-export for convenience
pub use std::sync::Arc as _Arc;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_register_and_report() {
        let hm = HealthMonitor::new("usv-01");
        hm.register_component("nav");
        hm.register_component("sensor");
        hm.register_component("weapon");
        let r = hm.report();
        assert_eq!(r.platform_id, "usv-01");
        assert_eq!(r.components.len(), 3);
        assert_eq!(r.overall_status, HealthStatus::Unknown);
    }

    #[test]
    fn test_health_set_status_propagates() {
        let hm = HealthMonitor::new("usv-01");
        hm.register_component("nav");
        hm.register_component("sensor");
        hm.set_status("nav", HealthStatus::Nominal);
        hm.set_status("sensor", HealthStatus::Inoperable);
        let r = hm.report();
        assert_eq!(r.overall_status, HealthStatus::Inoperable);
    }

    #[test]
    fn test_bit_failure_marks_degraded() {
        let hm = HealthMonitor::new("usv-01");
        hm.register_component("weapon");
        hm.record_bit(
            "weapon",
            bit_failed("weapon", "self_test", "F001", "Restart"),
        );
        let r = hm.report();
        assert_eq!(r.components[0].status, HealthStatus::Degraded);
        assert_eq!(r.overall_status, HealthStatus::Degraded);
    }

    #[test]
    fn test_bit_pass_keeps_operational() {
        let hm = HealthMonitor::new("usv-01");
        hm.register_component("weapon");
        hm.record_bit("weapon", bit_passed("weapon", "self_test"));
        assert!(hm.all_operational());
    }

    #[test]
    fn test_tick_records_bit_due() {
        let mut hm = HealthMonitor::new("usv-01");
        hm.register_component("nav");
        let due = hm.tick();
        assert!(due, "first tick should be due");
        let not_due = hm.tick();
        assert!(!not_due, "second tick should not be due");
    }
}
