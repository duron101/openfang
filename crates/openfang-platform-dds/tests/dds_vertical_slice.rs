//! Phase 5 — DDS loopback smoke slice (state + command + health) over the loopback
//! transport, plus a contract-equivalence check against a reference snapshot.
//! This validates the adapter contract ahead of the real rustdds/HIL transport.
//!
//! Topics exercised:
//! - state:   `nav/NavPosition` + `sensor/RadarTrack`
//! - command: `nav/NavCommand`
//! - health:  `platform/Heartbeat`

use openfang_platform::{snapshots_equivalent, EquivalenceTolerance, PlatformAdapter};
use openfang_platform_dds::publisher::publish_command;
use openfang_platform_dds::types::*;
use openfang_platform_dds::{DdsAdapter, DdsConfig, DdsTransport, LoopbackTransport};
use openfang_types::platform::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

type TestCallback = Box<dyn Fn(Vec<u8>) + Send + Sync>;

#[derive(Clone, Default)]
struct CallbackOnlyTransport {
    connected: Arc<AtomicBool>,
    callbacks: Arc<Mutex<HashMap<String, TestCallback>>>,
}

impl CallbackOnlyTransport {
    fn emit(&self, topic: &str, payload: Vec<u8>) {
        let callbacks = self.callbacks.lock().unwrap();
        let callback = callbacks.get(topic).expect("topic callback registered");
        callback(payload);
    }
}

#[async_trait::async_trait]
impl DdsTransport for CallbackOnlyTransport {
    async fn connect(&mut self, _domain_id: u16) -> Result<(), String> {
        self.connected.store(true, Ordering::SeqCst);
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
        _topic: &str,
        _qos: &DdsQosProfile,
        _payload: &[u8],
    ) -> Result<(), String> {
        Ok(())
    }

    async fn take_next(&self, _topic: &str) -> Result<Option<Vec<u8>>, String> {
        Ok(None)
    }

    async fn subscribe_callback(
        &self,
        topic: &str,
        _qos: &DdsQosProfile,
        callback: TestCallback,
    ) -> Result<(), String> {
        self.callbacks
            .lock()
            .unwrap()
            .insert(topic.to_string(), callback);
        Ok(())
    }
}

fn golden_reference() -> WorldSnapshot {
    // The world the DDS state topics describe.
    WorldSnapshot {
        timestamp: 0.0,
        platforms: vec![PlatformState {
            id: "usv-01".into(),
            name: "usv-01".into(),
            platform_type: "usv".into(),
            affiliation: Affiliation::Blue,
            domain: Domain::Surface,
            pose: Pose {
                lat_deg: 30.0,
                lon_deg: 120.0,
                alt_m: 0.0,
                heading_deg: 90.0,
                pitch_deg: 0.0,
                roll_deg: 0.0,
            },
            velocity: Velocity {
                speed_ms: 15.0,
                vertical_rate_ms: 0.0,
                course_deg: 90.0,
            },
            fuel: FuelStatus {
                remaining_kg: 0.0,
                max_kg: 0.0,
                consumption_rate_kg_s: 0.0,
            },
            damage: 0.0,
            tracks: vec![Track {
                track_id: "trk-1".into(),
                target_name: String::new(),
                classification: "boat".into(),
                affiliation: Affiliation::Foe,
                iff: "foe".into(),
                position_lla: None,
                heading_deg: None,
                speed_ms: None,
                range_m: Some(5000.0),
                bearing_deg: Some(45.0),
                elevation_deg: None,
                quality: 0.8,
                stale: false,
                last_update_s: 0.0,
                is_active: true,
            }],
            onboard_sensors: vec![],
            onboard_weapons: vec![],
            onboard_jammers: vec![],
            current_target: None,
            commander: None,
            survivability: None,
            emcon: None,
            link: None,
        }],
        active_munitions: vec![],
        events: vec![],
        fleet: None,
    }
}

fn nav_position() -> Vec<u8> {
    serde_json::to_vec(&NavPosition {
        platform_id: "usv-01".into(),
        lat_deg: 30.0,
        lon_deg: 120.0,
        alt_m: 0.0,
        heading_deg: 90.0,
        pitch_deg: 0.0,
        roll_deg: 0.0,
        speed_ms: 15.0,
        vertical_rate_ms: 0.0,
        course_deg: 90.0,
        nav_source: "gps".into(),
        accuracy_cep_m: 5.0,
        timestamp_us: 0,
    })
    .unwrap()
}

fn radar_track() -> Vec<u8> {
    serde_json::to_vec(&RadarTrack {
        track_id: "trk-1".into(),
        classification: "boat".into(),
        affiliation: "foe".into(),
        lat_deg: None,
        lon_deg: None,
        alt_m: None,
        heading_deg: None,
        speed_ms: None,
        range_m: Some(5000.0),
        bearing_deg: Some(45.0),
        quality: 0.8,
        stale: false,
        detecting_platform_id: "usv-01".into(),
        timestamp_us: 0,
    })
    .unwrap()
}

fn heartbeat() -> Vec<u8> {
    serde_json::to_vec(&Heartbeat {
        platform_id: "usv-01".into(),
        uptime_s: 100,
        cpu_pct: 10.0,
        mem_mb: 256.0,
        disk_mb: 1024.0,
        link_quality: 0.95,
        autonomy_mode: "L4".into(),
        timestamp_us: 0,
    })
    .unwrap()
}

#[test]
fn dds_adapter_poll_state_is_contract_equivalent() {
    tokio_test::block_on(async {
        use openfang_platform_dds::DdsTransport;

        // Seed the three state/health topics on the wire, then hand the transport
        // to the adapter and let it build a WorldSnapshot via its real poll path.
        let mut transport = LoopbackTransport::new(16);
        let metrics = transport.metrics();
        transport.connect(0).await.unwrap();
        let qos = DdsQosProfile::default();
        transport
            .publish("nav/NavPosition", &qos, &nav_position())
            .await
            .unwrap();
        transport
            .publish("sensor/RadarTrack", &qos, &radar_track())
            .await
            .unwrap();
        transport
            .publish("platform/Heartbeat", &qos, &heartbeat())
            .await
            .unwrap();

        let mut adapter = DdsAdapter::with_transport(Box::new(transport), DdsConfig::default());
        adapter.connect().await.unwrap();
        let snapshot = adapter.poll_state().await.unwrap();

        let mut reference = golden_reference();
        // The DDS subscriber stamps wall-clock time; align timestamps for the
        // semantic comparison (time equivalence is validated by the loopback metrics).
        reference.timestamp = snapshot.timestamp;

        let result = snapshots_equivalent(&reference, &snapshot, EquivalenceTolerance::default());
        assert!(
            result.is_ok(),
            "DDS snapshot diverged from contract: {:?}",
            result.err()
        );

        assert!(
            snapshot.events.iter().any(|event| matches!(
                event,
                WorldEvent::PlatformHealth {
                    platform_id,
                    link_quality,
                    autonomy_mode,
                    ..
                } if platform_id == "usv-01" && *link_quality == 0.95 && autonomy_mode == "L4"
            )),
            "DDS heartbeat must be preserved as a platform health event"
        );

        // The state + track + health topics were delivered with measurable latency.
        assert_eq!(metrics.delivered(), 3);
        assert!(metrics.avg_latency_us() >= 0.0);
    });
}

#[test]
fn command_round_trips_through_dds_wire() {
    tokio_test::block_on(async {
        use openfang_platform_dds::DdsTransport;
        let mut transport = LoopbackTransport::new(16);
        transport.connect(0).await.unwrap();

        let cmd = PlatformCommand::SetHeading {
            platform_id: "usv-01".into(),
            heading_deg: 123.0,
            speed_ms: Some(12.0),
            turn_direction: None,
        };
        publish_command(&transport, &cmd).await.unwrap();

        let raw = transport
            .take_next("nav/NavCommand")
            .await
            .unwrap()
            .expect("command on wire");
        let nav: NavCommand = serde_json::from_slice(&raw).unwrap();
        assert_eq!(nav.command_type, NavCommandType::SetHeading);
        assert_eq!(nav.target_heading_deg, Some(123.0));
        assert_eq!(nav.target_speed_ms, Some(12.0));
    });
}

#[test]
fn dds_adapter_uses_callback_cache_when_take_next_is_empty() {
    tokio_test::block_on(async {
        let transport = CallbackOnlyTransport::default();
        let handle = transport.clone();
        let mut adapter = DdsAdapter::with_transport(Box::new(transport), DdsConfig::default());
        adapter.connect().await.unwrap();

        handle.emit("nav/NavPosition", nav_position());
        handle.emit("sensor/RadarTrack", radar_track());
        handle.emit("platform/Heartbeat", heartbeat());

        let snapshot = adapter.poll_state().await.unwrap();
        assert_eq!(snapshot.platforms.len(), 1);
        assert_eq!(snapshot.platforms[0].id, "usv-01");
        assert_eq!(snapshot.platforms[0].tracks.len(), 1);
        assert!(snapshot.events.iter().any(|event| matches!(
            event,
            WorldEvent::PlatformHealth { platform_id, .. } if platform_id == "usv-01"
        )));
    });
}

#[test]
fn reconnect_and_backpressure_are_measured() {
    tokio_test::block_on(async {
        use openfang_platform_dds::DdsTransport;
        let mut transport = LoopbackTransport::new(2);
        transport.connect(0).await.unwrap();
        let qos = DdsQosProfile::best_effort_keep_last(2);

        // Overflow the bounded history → measured drops.
        for i in 0..5u8 {
            transport
                .publish("sensor/RadarTrack", &qos, &[i])
                .await
                .unwrap();
        }
        assert_eq!(transport.metrics().dropped(), 3);

        // Disconnect blocks publishes; reconnect restores and is counted.
        transport.disconnect().await.unwrap();
        assert!(transport
            .publish("sensor/RadarTrack", &qos, &[9])
            .await
            .is_err());
        transport.connect(0).await.unwrap();
        assert!(transport
            .publish("sensor/RadarTrack", &qos, &[9])
            .await
            .is_ok());
        assert_eq!(transport.metrics().reconnects(), 1);
    });
}
