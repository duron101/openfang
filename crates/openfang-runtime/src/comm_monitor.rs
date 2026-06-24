//! Communication Monitor — tracks shore connectivity and triggers autonomy mode changes.
//!
//! Periodically pings configured peer nodes via OFP. When consecutive failures
//! exceed the threshold, publishes `LinkLost` event. When connectivity recovers,
//! publishes `LinkRestored`. The event triggers can be wired to:
//! - TriggerEngine → TCA Agent switches autonomy mode
//! - ApprovalManager → auto_approve_autonomous = true

use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tracing::{info, warn};

/// Communication link health status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkStatus {
    /// All monitored peers reachable
    Connected,
    /// Some peers unreachable but not yet threshold
    Degraded,
    /// All peers unreachable beyond threshold
    Lost,
}

/// Re-export the canonical [`LinkQuality`] enum from `openfang-types` under
/// the old name (`LinkQualityBucket`) for backwards source compatibility.
/// New code should reach for `openfang_types::platform::LinkQuality` directly.
pub use openfang_types::platform::LinkQuality as LinkQualityBucket;

/// Compute the [`LinkQualityBucket`] from a [`LinkStatus`] and the live
/// per-peer failure counts. Pure function; safe to call from the hot path.
pub fn assess_link_quality(
    status: LinkStatus,
    failure_counts: &[(String, u32)],
    failure_threshold: u32,
) -> LinkQualityBucket {
    use openfang_types::platform::LinkQuality;
    match status {
        LinkStatus::Lost => LinkQuality::Lost,
        LinkStatus::Degraded => {
            let at_threshold = failure_counts.iter().any(|(_, c)| *c >= failure_threshold);
            let multi_degraded = failure_counts.iter().filter(|(_, c)| *c > 0).count() >= 2;
            if at_threshold || multi_degraded {
                LinkQuality::Poor
            } else {
                LinkQuality::Marginal
            }
        }
        LinkStatus::Connected => {
            if failure_counts.iter().any(|(_, c)| *c > 0) {
                LinkQuality::Good
            } else {
                LinkQuality::Excellent
            }
        }
    }
}

/// Configuration for the communication monitor
#[derive(Debug, Clone)]
pub struct CommMonitorConfig {
    /// Interval between ping attempts (seconds)
    pub ping_interval_secs: u64,
    /// Number of consecutive failures before declaring LinkLost
    pub failure_threshold: u32,
    /// Peer node IDs to monitor
    pub peer_ids: Vec<String>,
}

impl Default for CommMonitorConfig {
    fn default() -> Self {
        Self {
            ping_interval_secs: 30,
            failure_threshold: 5,
            peer_ids: vec!["shore_command".into(), "hec".into()],
        }
    }
}

/// Communication monitor — runs as a background tokio task.
pub struct CommunicationMonitor {
    config: CommMonitorConfig,
    /// Per-peer failure counters
    failures: DashMap<String, u32>,
    /// Current link status
    status: std::sync::RwLock<LinkStatus>,
    /// Whether the monitor is running
    running: AtomicBool,
}

impl CommunicationMonitor {
    pub fn new(config: CommMonitorConfig) -> Self {
        Self {
            config,
            failures: DashMap::new(),
            status: std::sync::RwLock::new(LinkStatus::Connected),
            running: AtomicBool::new(false),
        }
    }

    /// Start the monitor as a background task.
    /// Returns a JoinHandle that can be awaited for graceful shutdown.
    pub fn start(
        self: Arc<Self>,
        mut shutdown_rx: watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        self.running.store(true, Ordering::SeqCst);
        let self_ref = Arc::clone(&self);

        tokio::spawn(async move {
            let interval = Duration::from_secs(self_ref.config.ping_interval_secs);
            info!(
                interval_s = self_ref.config.ping_interval_secs,
                peers = ?self_ref.config.peer_ids,
                "Communication monitor started"
            );

            loop {
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {
                        self_ref.tick().await;
                    }
                    _ = shutdown_rx.changed() => {
                        info!("Communication monitor received shutdown signal");
                        self_ref.running.store(false, Ordering::SeqCst);
                        break;
                    }
                }
            }
        })
    }

    /// Perform one monitoring tick: ping all peers, update status.
    async fn tick(&self) {
        let mut all_failed = true;

        for peer_id in &self.config.peer_ids {
            let reachable = self.ping_peer(peer_id).await;

            if reachable {
                all_failed = false;
                let prev = self.failures.remove(peer_id);
                if prev.is_some() {
                    info!(peer = %peer_id, "Peer recovered");
                }
            } else {
                let count = self
                    .failures
                    .entry(peer_id.clone())
                    .and_modify(|c| *c += 1)
                    .or_insert(1);
                let c = *count;
                if c == 1 {
                    warn!(peer = %peer_id, "Peer unreachable");
                } else if c >= self.config.failure_threshold {
                    warn!(peer = %peer_id, count = c, "Peer exceeded failure threshold");
                }
            }
        }

        let new_status = if all_failed
            && !self.config.peer_ids.is_empty()
            && self
                .failures
                .iter()
                .all(|f| *f.value() >= self.config.failure_threshold)
        {
            LinkStatus::Lost
        } else if self.failures.is_empty() {
            LinkStatus::Connected
        } else {
            LinkStatus::Degraded
        };

        let old_status = *self.status.read().unwrap();
        if new_status != old_status {
            *self.status.write().unwrap() = new_status;

            match new_status {
                LinkStatus::Connected => {
                    info!("Link status: Connected");
                    // Event publishing handled by kernel integration
                }
                LinkStatus::Degraded => {
                    warn!("Link status: Degraded");
                }
                LinkStatus::Lost => {
                    warn!("Link status: Lost — triggering autonomous mode");
                    // ApprovalManager::auto_approve_autonomous = true
                    // (handled by kernel integration via EventBus)
                }
            }
        }
    }

    /// Ping a peer — stub implementation.
    /// In production, this sends an OFP Ping message and waits for Pong.
    async fn ping_peer(&self, _peer_id: &str) -> bool {
        // TODO: Integrate with OFP PeerNode::ping()
        // For now, assume always reachable
        true
    }

    /// Get current link status
    pub fn link_status(&self) -> LinkStatus {
        *self.status.read().unwrap()
    }

    /// Whether the monitor is running
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Get failure counts per peer
    pub fn failure_counts(&self) -> Vec<(String, u32)> {
        self.failures
            .iter()
            .map(|e| (e.key().clone(), *e.value()))
            .collect()
    }

    /// Derived link-quality bucket — combines `link_status` and per-peer
    /// failure counters into a single canonical value the CMS lane and the
    /// dashboard can consume.
    pub fn link_quality(&self) -> LinkQualityBucket {
        let status = self.link_status();
        let counts = self.failure_counts();
        assess_link_quality(status, &counts, self.config.failure_threshold)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_initial_status_connected() {
        let monitor = Arc::new(CommunicationMonitor::new(CommMonitorConfig::default()));
        assert_eq!(monitor.link_status(), LinkStatus::Connected);
    }

    #[tokio::test]
    async fn test_start_stop() {
        let monitor = Arc::new(CommunicationMonitor::new(CommMonitorConfig {
            ping_interval_secs: 1,
            ..Default::default()
        }));
        let (tx, rx) = watch::channel(false);

        let handle = monitor.clone().start(rx);
        assert!(monitor.is_running());

        // Signal shutdown
        tx.send(true).ok();
        handle.await.ok();

        assert!(!monitor.is_running());
    }

    #[test]
    fn test_link_status_transitions() {
        let status = LinkStatus::Connected;
        assert_eq!(status, LinkStatus::Connected);

        let status = LinkStatus::Lost;
        assert_eq!(status, LinkStatus::Lost);
    }

    #[test]
    fn link_quality_excellent_when_clean() {
        let bucket = assess_link_quality(LinkStatus::Connected, &[], 5);
        assert_eq!(bucket, LinkQualityBucket::Excellent);
    }

    #[test]
    fn link_quality_good_with_intermittent_failure() {
        let counts = vec![("shore".into(), 1u32)];
        let bucket = assess_link_quality(LinkStatus::Connected, &counts, 5);
        assert_eq!(bucket, LinkQualityBucket::Good);
    }

    #[test]
    fn link_quality_marginal_when_one_peer_only_degraded() {
        let counts = vec![("shore".into(), 1u32)];
        let bucket = assess_link_quality(LinkStatus::Degraded, &counts, 5);
        assert_eq!(bucket, LinkQualityBucket::Marginal);
    }

    #[test]
    fn link_quality_poor_when_threshold_reached() {
        let counts = vec![("shore".into(), 5u32)];
        let bucket = assess_link_quality(LinkStatus::Degraded, &counts, 5);
        assert_eq!(bucket, LinkQualityBucket::Poor);
    }

    #[test]
    fn link_quality_poor_when_multiple_degraded() {
        let counts = vec![("shore".into(), 1u32), ("hec".into(), 2u32)];
        let bucket = assess_link_quality(LinkStatus::Degraded, &counts, 5);
        assert_eq!(bucket, LinkQualityBucket::Poor);
    }

    #[test]
    fn link_quality_lost_short_circuits() {
        let bucket = assess_link_quality(LinkStatus::Lost, &[], 5);
        assert_eq!(bucket, LinkQualityBucket::Lost);
    }

    #[test]
    fn link_quality_force_defensive_predicate() {
        assert!(!LinkQualityBucket::Excellent.should_force_defensive());
        assert!(!LinkQualityBucket::Good.should_force_defensive());
        assert!(!LinkQualityBucket::Marginal.should_force_defensive());
        assert!(LinkQualityBucket::Poor.should_force_defensive());
        assert!(LinkQualityBucket::Lost.should_force_defensive());
    }

    #[test]
    fn link_quality_as_str_stable() {
        assert_eq!(LinkQualityBucket::Excellent.as_str(), "excellent");
        assert_eq!(LinkQualityBucket::Lost.as_str(), "lost");
    }

    #[test]
    fn monitor_link_quality_reflects_state() {
        let monitor = Arc::new(CommunicationMonitor::new(CommMonitorConfig::default()));
        assert_eq!(monitor.link_quality(), LinkQualityBucket::Excellent);
    }
}
