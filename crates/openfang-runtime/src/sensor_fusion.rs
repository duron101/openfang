//! Sensor Fusion — multi-sensor track correlation, Kalman filtering,
//! and threat assessment. Pure Rust, real-time capable.
//!
//! # Architecture
//! - `SensorFusion` maintains a track database
//! - Correlates new sensor contacts with existing tracks
//! - Runs Kalman prediction/update cycles
//! - Assesses threat levels based on kinematics, IFF, and ROE

use openfang_types::platform::*;
use std::collections::HashMap;

/// Threat level assigned to each track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ThreatLevel {
    None = 0,
    Low = 1,
    Medium = 2,
    High = 3,
    Critical = 4,
}

/// Fused track with Kalman state.
#[derive(Debug, Clone)]
pub struct FusedTrack {
    pub track_id: String,
    pub classification: String,
    pub affiliation: Affiliation,
    pub position: (f64, f64, f64), // LLA
    pub velocity: (f64, f64, f64), // NED m/s
    pub heading_deg: f64,
    pub speed_ms: f64,
    pub quality: f64,
    pub threat_level: ThreatLevel,
    pub last_update_s: f64,
    pub age_s: f64,
    pub update_count: u32,
    // Kalman covariance (simplified: position variance)
    pub position_variance: f64,
}

/// Sensor fusion engine.

const MEASUREMENT_NOISE: f64 = 10.0;
const PROCESS_NOISE: f64 = 1.0;
pub struct SensorFusion {
    /// Track database: track_id → FusedTrack
    tracks: HashMap<String, FusedTrack>,
    /// Correlation gate: max distance (m) for associating a new contact with existing track
    correlation_gate_m: f64,
    /// Track timeout: remove tracks not updated within this time (seconds)
    track_timeout_s: f64,
    /// Process noise for Kalman prediction
    process_noise: f64,
    /// Measurement noise for Kalman update
    measurement_noise: f64,
}

#[derive(Debug, Clone)]
pub struct FusionOutput {
    pub fused_tracks: Vec<FusedTrack>,
    pub threats: Vec<(String, ThreatLevel)>,
    pub alerts: Vec<FusionAlert>,
}

#[derive(Debug, Clone)]
pub enum FusionAlert {
    NewTrack {
        track_id: String,
    },
    TrackLost {
        track_id: String,
    },
    ThreatEscalated {
        track_id: String,
        from: ThreatLevel,
        to: ThreatLevel,
    },
    IffConflict {
        track_id: String,
    },
}

impl SensorFusion {
    pub fn new() -> Self {
        Self {
            tracks: HashMap::new(),
            correlation_gate_m: 5000.0,
            track_timeout_s: 120.0,
            process_noise: 1.0,
            measurement_noise: 10.0,
        }
    }

    /// Process a new world snapshot: update all tracks and assess threats.
    pub fn update(&mut self, snapshot: &WorldSnapshot) -> FusionOutput {
        let now = snapshot.timestamp;
        let mut alerts = Vec::new();

        // 1. Collect all raw tracks from all own-platform sensors
        let raw_tracks: Vec<(&Track, &str)> = snapshot
            .platforms
            .iter()
            .flat_map(|p| p.tracks.iter().map(move |t| (t, p.id.as_str())))
            .collect();

        // 2. Predict all existing tracks forward in time
        for track in self.tracks.values_mut() {
            let dt = now - track.last_update_s;
            if dt > 0.0 {
                // Simple constant-velocity prediction
                track.position.0 += track.velocity.0 * dt;
                track.position.1 += track.velocity.1 * dt;
                track.position.2 += track.velocity.2 * dt;
                track.position_variance += PROCESS_NOISE * dt;
                track.age_s += dt;
                track.last_update_s = now;
            }
        }

        // 3. Correlate raw tracks with existing fused tracks
        for (raw, source_id) in &raw_tracks {
            if raw.stale {
                continue;
            }

            let correlated = self.correlate(raw);

            match correlated {
                Some(track_id) => {
                    // Update existing track
                    let entry = self.tracks.get_mut(&track_id);
                    if let Some(track) = entry {
                        Self::kalman_update(track, raw, now);
                    }
                }
                None => {
                    // New track
                    let fused = self.create_track(raw, now);
                    let track_id = fused.track_id.clone();
                    self.tracks.insert(track_id.clone(), fused);
                    alerts.push(FusionAlert::NewTrack { track_id });
                }
            }
        }

        // 4. Remove stale tracks
        let timeout = self.track_timeout_s;
        let stale: Vec<String> = self
            .tracks
            .iter()
            .filter(|(_, t)| now - t.last_update_s > timeout)
            .map(|(id, _)| id.clone())
            .collect();

        for id in &stale {
            self.tracks.remove(id);
            alerts.push(FusionAlert::TrackLost {
                track_id: id.clone(),
            });
        }

        // 5. Assess threats
        let threats = self.assess_threats();

        // 6. Build output
        let fused_tracks: Vec<FusedTrack> = self.tracks.values().cloned().collect();

        FusionOutput {
            fused_tracks,
            threats,
            alerts,
        }
    }

    /// Correlate a raw track with the closest existing fused track.
    fn correlate(&self, raw: &Track) -> Option<String> {
        if let Some(pos) = raw.position_lla {
            let mut best_id = None;
            let mut best_dist = self.correlation_gate_m;

            for (id, fused) in &self.tracks {
                let dx = pos.0 - fused.position.0;
                let dy = pos.1 - fused.position.1;
                let dist = (dx * dx + dy * dy).sqrt() * 111_000.0; // approx deg→m
                if dist < best_dist {
                    best_dist = dist;
                    best_id = Some(id.clone());
                }
            }
            best_id
        } else if !raw.track_id.is_empty() {
            // Correlate by track ID (from simulation)
            if self.tracks.contains_key(&raw.track_id) {
                Some(raw.track_id.clone())
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Create a new fused track from a raw track.
    fn create_track(&self, raw: &Track, now: f64) -> FusedTrack {
        let pos = raw.position_lla.unwrap_or((0.0, 0.0, 0.0));
        let spd = raw.speed_ms.unwrap_or(0.0);
        let hdg = raw.heading_deg.unwrap_or(0.0);

        let vn = spd * hdg.to_radians().cos();
        let ve = spd * hdg.to_radians().sin();

        FusedTrack {
            track_id: if raw.track_id.is_empty() {
                format!("trk-{}", now as u64)
            } else {
                raw.track_id.clone()
            },
            classification: raw.classification.clone(),
            affiliation: raw.affiliation,
            position: pos,
            velocity: (vn, ve, 0.0),
            heading_deg: hdg,
            speed_ms: spd,
            quality: raw.quality,
            threat_level: ThreatLevel::None,
            last_update_s: now,
            age_s: 0.0,
            update_count: 1,
            position_variance: MEASUREMENT_NOISE,
        }
    }

    /// Simple 1D Kalman update for position.
    fn kalman_update(track: &mut FusedTrack, raw: &Track, now: f64) {
        if let Some(pos) = raw.position_lla {
            let dt = now - track.last_update_s;
            if dt <= 0.0 {
                return;
            }

            // Prediction step
            track.position.0 += track.velocity.0 * dt;
            track.position.1 += track.velocity.1 * dt;
            track.position_variance += PROCESS_NOISE * dt;

            // Update step (Kalman gain)
            let innovation_var = track.position_variance + MEASUREMENT_NOISE;
            let k = track.position_variance / innovation_var;

            // Update position
            track.position.0 += k * (pos.0 - track.position.0);
            track.position.1 += k * (pos.1 - track.position.1);
            track.position.2 += k * (pos.2 - track.position.2);

            // Update covariance
            track.position_variance *= 1.0 - k;

            // Update velocity from raw track
            if let Some(hdg) = raw.heading_deg {
                track.heading_deg = hdg;
            }
            if let Some(spd) = raw.speed_ms {
                track.speed_ms = spd;
                track.velocity.0 = spd * track.heading_deg.to_radians().cos();
                track.velocity.1 = spd * track.heading_deg.to_radians().sin();
            }
        }

        track.last_update_s = now;
        track.quality = raw.quality.max(track.quality);
        track.update_count += 1;
    }

    /// Assess threat levels for all tracks.
    fn assess_threats(&mut self) -> Vec<(String, ThreatLevel)> {
        let mut threats = Vec::new();

        for track in self.tracks.values_mut() {
            let old = track.threat_level;

            // Threat criteria:
            // 1. Hostile affiliation → at least Medium
            // 2. High speed approaching → escalate
            // 3. Close range → escalate

            let mut level = ThreatLevel::None;

            if matches!(track.affiliation, Affiliation::Red | Affiliation::Foe) {
                level = ThreatLevel::Medium;
            }

            // Speed > 50 m/s (180 km/h) → escalate
            if track.speed_ms > 50.0 {
                level = level.max(ThreatLevel::High);
            }

            // Range < 10 km and speed > 10 m/s → possible threat
            let dist_approx =
                (track.position.0.powi(2) + track.position.1.powi(2)).sqrt() * 111_000.0;
            if dist_approx < 10_000.0 && track.speed_ms > 10.0 {
                level = level.max(ThreatLevel::High);
            }

            // Range < 3 km → critical
            if dist_approx < 3_000.0 {
                level = level.max(ThreatLevel::Critical);
            }

            track.threat_level = level;
            threats.push((track.track_id.clone(), level));
        }

        threats
    }

    /// Get the total number of tracked objects.
    pub fn track_count(&self) -> usize {
        self.tracks.len()
    }

    /// Get a specific track by ID.
    pub fn get_track(&self, id: &str) -> Option<&FusedTrack> {
        self.tracks.get(id)
    }
}

impl Default for SensorFusion {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_track_creation() {
        let mut fusion = SensorFusion::new();
        let snapshot = WorldSnapshot {
            timestamp: 100.0,
            platforms: vec![PlatformState {
                id: "usv-01".into(),
                name: "USV".into(),
                platform_type: "usv".into(),
                affiliation: Affiliation::Blue,
                domain: Domain::Surface,
                pose: Pose {
                    lat_deg: 30.0,
                    lon_deg: 120.0,
                    alt_m: 0.0,
                    heading_deg: 0.0,
                    pitch_deg: 0.0,
                    roll_deg: 0.0,
                },
                velocity: Velocity {
                    speed_ms: 10.0,
                    vertical_rate_ms: 0.0,
                    course_deg: 0.0,
                },
                fuel: FuelStatus {
                    remaining_kg: 100.0,
                    max_kg: 100.0,
                    consumption_rate_kg_s: 0.0,
                },
                damage: 0.0,
                tracks: vec![Track {
                    track_id: "trk-1".into(),
                    target_name: String::new(),
                    classification: "destroyer".into(),
                    affiliation: Affiliation::Red,
                    iff: "foe".into(),
                    position_lla: Some((30.1, 120.1, 0.0)),
                    heading_deg: Some(90.0),
                    speed_ms: Some(15.0),
                    range_m: Some(15000.0),
                    bearing_deg: Some(45.0),
                    elevation_deg: None,
                    quality: 0.8,
                    stale: false,
                    last_update_s: 100.0,
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
        };

        let output = fusion.update(&snapshot);
        assert!(!output.fused_tracks.is_empty());
        assert_eq!(output.fused_tracks[0].track_id, "trk-1");
        assert_eq!(output.fused_tracks[0].affiliation, Affiliation::Red);
    }

    #[test]
    fn fusion_processes_tracks_from_non_blue_sensor_platforms() {
        let mut fusion = SensorFusion::new();
        let mut platform = PlatformState::minimal("red-controller");
        platform.affiliation = Affiliation::Red;
        platform.tracks = vec![Track {
            track_id: "blue-contact".into(),
            target_name: String::new(),
            classification: "uav".into(),
            affiliation: Affiliation::Blue,
            iff: "foe".into(),
            position_lla: Some((30.1, 120.1, 0.0)),
            heading_deg: Some(180.0),
            speed_ms: Some(20.0),
            range_m: Some(10_000.0),
            bearing_deg: Some(0.0),
            elevation_deg: None,
            quality: 0.7,
            stale: false,
            last_update_s: 100.0,
            is_active: true,
        }];

        let output = fusion.update(&WorldSnapshot {
            timestamp: 100.0,
            platforms: vec![platform],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        });

        assert_eq!(output.fused_tracks.len(), 1);
        assert_eq!(output.fused_tracks[0].track_id, "blue-contact");
    }

    #[test]
    fn test_threat_assessment() {
        let mut fusion = SensorFusion::new();
        // Directly insert a close-range hostile track
        fusion.tracks.insert(
            "trk-hostile".into(),
            FusedTrack {
                track_id: "trk-hostile".into(),
                classification: "fighter".into(),
                affiliation: Affiliation::Red,
                position: (30.01, 120.01, 5000.0),
                velocity: (200.0, 0.0, 0.0),
                heading_deg: 90.0,
                speed_ms: 200.0,
                quality: 0.9,
                threat_level: ThreatLevel::None,
                last_update_s: 100.0,
                age_s: 10.0,
                update_count: 5,
                position_variance: 5.0,
            },
        );

        let threats = fusion.assess_threats();
        let (_, level) = threats.iter().find(|(id, _)| id == "trk-hostile").unwrap();
        assert!(*level >= ThreatLevel::High);
    }
}
