//! LoopbackTransport — a real in-process DDS transport for the smoke/HIL slice.
//!
//! Unlike [`crate::NoopTransport`], this transport actually moves bytes through
//! per-topic queues with DDS-like *keep-last* semantics, and it measures the
//! behaviours a real bus must be evaluated for:
//! - **latency**: publish→take elapsed time,
//! - **backpressure/loss**: bounded per-topic history; overflow drops oldest,
//! - **reconnect**: publishing while disconnected fails; reconnect restores it.
//!
//! It lets the full publish→subscribe→`WorldSnapshot` path run end to end and be
//! contract-checked against the ArkSim/Mock golden, ahead of wiring a real
//! `rustdds` participant (which additionally requires a live RTPS domain and is
//! gated behind the optional `rustdds-transport` feature).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dashmap::DashMap;

use crate::types::DdsQosProfile;
use crate::DdsTransport;

fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

struct Sample {
    payload: Vec<u8>,
    published_us: u64,
}

/// Measured behaviour of the loopback bus.
#[derive(Default)]
pub struct LoopbackMetrics {
    published: AtomicU64,
    delivered: AtomicU64,
    dropped: AtomicU64,
    total_latency_us: AtomicU64,
    reconnects: AtomicU64,
}

impl LoopbackMetrics {
    pub fn published(&self) -> u64 {
        self.published.load(Ordering::SeqCst)
    }
    pub fn delivered(&self) -> u64 {
        self.delivered.load(Ordering::SeqCst)
    }
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::SeqCst)
    }
    pub fn reconnects(&self) -> u64 {
        self.reconnects.load(Ordering::SeqCst)
    }
    /// Mean publish→take latency over delivered samples (microseconds).
    pub fn avg_latency_us(&self) -> f64 {
        let d = self.delivered.load(Ordering::SeqCst);
        if d == 0 {
            0.0
        } else {
            self.total_latency_us.load(Ordering::SeqCst) as f64 / d as f64
        }
    }
}

type Callback = Box<dyn Fn(Vec<u8>) + Send + Sync>;

/// In-process DDS transport with bounded keep-last queues per topic.
pub struct LoopbackTransport {
    connected: AtomicBool,
    history_depth: usize,
    topics: DashMap<String, Mutex<VecDeque<Sample>>>,
    callbacks: DashMap<String, Arc<Callback>>,
    metrics: Arc<LoopbackMetrics>,
}

impl LoopbackTransport {
    /// `history_depth` bounds each topic's queue (DDS KEEP_LAST depth).
    pub fn new(history_depth: usize) -> Self {
        Self {
            connected: AtomicBool::new(false),
            history_depth: history_depth.max(1),
            topics: DashMap::new(),
            callbacks: DashMap::new(),
            metrics: Arc::new(LoopbackMetrics::default()),
        }
    }

    /// Shared metrics handle (survives moving the transport into an adapter).
    pub fn metrics(&self) -> Arc<LoopbackMetrics> {
        self.metrics.clone()
    }
}

impl Default for LoopbackTransport {
    fn default() -> Self {
        Self::new(8)
    }
}

#[async_trait]
impl DdsTransport for LoopbackTransport {
    async fn connect(&mut self, _domain_id: u16) -> Result<(), String> {
        if self.connected.swap(true, Ordering::SeqCst) {
            // already connected
        } else if self.metrics.published() > 0 {
            // re-establishing after a prior disconnect
            self.metrics.reconnects.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), String> {
        self.connected.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    async fn publish(
        &self,
        topic: &str,
        _qos: &DdsQosProfile,
        payload: &[u8],
    ) -> Result<(), String> {
        if !self.is_connected() {
            return Err("loopback: not connected".into());
        }
        let entry = self
            .topics
            .entry(topic.to_string())
            .or_insert_with(|| Mutex::new(VecDeque::new()));
        {
            let mut q = entry.lock().unwrap_or_else(|e| e.into_inner());
            if q.len() >= self.history_depth {
                q.pop_front();
                self.metrics.dropped.fetch_add(1, Ordering::SeqCst);
            }
            q.push_back(Sample {
                payload: payload.to_vec(),
                published_us: now_us(),
            });
        }
        self.metrics.published.fetch_add(1, Ordering::SeqCst);
        if let Some(cb) = self.callbacks.get(topic) {
            (cb)(payload.to_vec());
        }
        Ok(())
    }

    async fn take_next(&self, topic: &str) -> Result<Option<Vec<u8>>, String> {
        if !self.is_connected() {
            return Err("loopback: not connected".into());
        }
        let Some(entry) = self.topics.get(topic) else {
            return Ok(None);
        };
        let mut q = entry.lock().unwrap_or_else(|e| e.into_inner());
        // KEEP_LAST: deliver the most recent sample, discard the rest.
        let latest = q.pop_back();
        q.clear();
        match latest {
            Some(s) => {
                let lat = now_us().saturating_sub(s.published_us);
                self.metrics
                    .total_latency_us
                    .fetch_add(lat, Ordering::SeqCst);
                self.metrics.delivered.fetch_add(1, Ordering::SeqCst);
                Ok(Some(s.payload))
            }
            None => Ok(None),
        }
    }

    async fn subscribe_callback(
        &self,
        topic: &str,
        _qos: &DdsQosProfile,
        callback: Box<dyn Fn(Vec<u8>) + Send + Sync>,
    ) -> Result<(), String> {
        self.callbacks.insert(topic.to_string(), Arc::new(callback));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_take_roundtrip_with_latency() {
        tokio_test::block_on(async {
            let mut t = LoopbackTransport::new(8);
            t.connect(0).await.unwrap();
            let qos = DdsQosProfile::reliable_keep_last(8);
            t.publish("nav/NavPosition", &qos, b"hello").await.unwrap();
            let got = t.take_next("nav/NavPosition").await.unwrap();
            assert_eq!(got.as_deref(), Some(&b"hello"[..]));
            let m = t.metrics();
            assert_eq!(m.published(), 1);
            assert_eq!(m.delivered(), 1);
        });
    }

    #[test]
    fn backpressure_drops_oldest() {
        tokio_test::block_on(async {
            let mut t = LoopbackTransport::new(2);
            t.connect(0).await.unwrap();
            let qos = DdsQosProfile::best_effort_keep_last(2);
            for i in 0..5u8 {
                t.publish("sensor/RadarTrack", &qos, &[i]).await.unwrap();
            }
            assert_eq!(t.metrics().dropped(), 3);
        });
    }

    #[test]
    fn publish_fails_while_disconnected_then_reconnects() {
        tokio_test::block_on(async {
            let mut t = LoopbackTransport::new(8);
            t.connect(0).await.unwrap();
            let qos = DdsQosProfile::default();
            t.publish("platform/Heartbeat", &qos, b"a").await.unwrap();
            t.disconnect().await.unwrap();
            assert!(t.publish("platform/Heartbeat", &qos, b"b").await.is_err());
            t.connect(0).await.unwrap();
            assert!(t.publish("platform/Heartbeat", &qos, b"c").await.is_ok());
            assert_eq!(t.metrics().reconnects(), 1);
        });
    }
}
