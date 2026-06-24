//! UMAA Track Management — correlation / identification / quality.
//!
//! Per PRD §12.2.4: standardised interface separate from sensor_fusion.

use openfang_types::umaa::*;
use std::collections::HashMap;

/// A raw sensor contact.
#[derive(Debug, Clone)]
pub struct SensorContact {
    pub contact_id: String,
    pub classification: String,
    pub lat: f64,
    pub lon: f64,
    pub speed_ms: f64,
    pub heading_deg: f64,
    pub range_m: f64,
    pub bearing_deg: f64,
    pub quality: f64,
    pub timestamp: f64,
}

/// A fused track.
#[derive(Debug, Clone)]
pub struct Track {
    pub track_id: String,
    pub classification: String,
    pub lat: f64,
    pub lon: f64,
    pub speed_ms: f64,
    pub heading_deg: f64,
    pub quality: f64,
    pub last_update: f64,
    pub last_contact_id: Option<String>,
}

/// TrackManager — owns track correlation and identification.
pub struct TrackManager {
    tracks: HashMap<String, Track>,
    next_id: u32,
    /// Distance threshold (meters) for considering two contacts as the same track.
    correlation_radius_m: f64,
}

impl TrackManager {
    pub fn new() -> Self {
        Self {
            tracks: HashMap::new(),
            next_id: 1,
            correlation_radius_m: 1000.0,
        }
    }

    pub fn track_count(&self) -> usize {
        self.tracks.len()
    }

    /// Process a sensor contact: either correlate with an existing track, or create a new one.
    pub fn correlate(&mut self, contact: SensorContact) -> CorrelationResult {
        // Find nearest track within correlation radius
        let mut best: Option<(String, f64)> = None;
        for (id, track) in &self.tracks {
            let d = haversine(track.lat, track.lon, contact.lat, contact.lon);
            if d < self.correlation_radius_m {
                let score = 1.0 - (d / self.correlation_radius_m);
                let better = match &best {
                    Some((_, s)) => score > *s,
                    None => true,
                };
                if better {
                    best = Some((id.clone(), score));
                }
            }
        }
        match best {
            Some((track_id, score)) => {
                if let Some(t) = self.tracks.get_mut(&track_id) {
                    t.lat = contact.lat;
                    t.lon = contact.lon;
                    t.speed_ms = contact.speed_ms;
                    t.heading_deg = contact.heading_deg;
                    t.quality = (t.quality * 0.7 + contact.quality * 0.3).min(1.0);
                    t.last_update = contact.timestamp;
                    t.last_contact_id = Some(contact.contact_id.clone());
                }
                CorrelationResult {
                    track_id,
                    contact_id: contact.contact_id,
                    correlation_score: score,
                    is_new_track: false,
                }
            }
            None => {
                let track_id = format!("trk-{:05}", self.next_id);
                self.next_id += 1;
                let track = Track {
                    track_id: track_id.clone(),
                    classification: contact.classification.clone(),
                    lat: contact.lat,
                    lon: contact.lon,
                    speed_ms: contact.speed_ms,
                    heading_deg: contact.heading_deg,
                    quality: contact.quality,
                    last_update: contact.timestamp,
                    last_contact_id: Some(contact.contact_id.clone()),
                };
                self.tracks.insert(track_id.clone(), track);
                CorrelationResult {
                    track_id,
                    contact_id: contact.contact_id,
                    correlation_score: 1.0,
                    is_new_track: true,
                }
            }
        }
    }

    /// Manually mark a track's identification (e.g. after shore confirmation).
    pub fn mark_identification(
        &mut self,
        track_id: &str,
        classification: &str,
        confidence: f64,
    ) -> IdentificationResult {
        match self.tracks.get_mut(track_id) {
            Some(t) => {
                t.classification = classification.to_string();
                IdentificationResult {
                    track_id: track_id.to_string(),
                    classification: classification.to_string(),
                    confidence,
                    classification_source: "shore_confirmation".into(),
                }
            }
            None => IdentificationResult {
                track_id: track_id.to_string(),
                classification: classification.to_string(),
                confidence: 0.0,
                classification_source: "unknown_track".into(),
            },
        }
    }

    /// Get quality metrics for a track.
    pub fn quality(&self, track_id: &str, now: f64) -> Option<TrackQuality> {
        self.tracks.get(track_id).map(|t| {
            let age = now - t.last_update;
            let staleness = if age < 5.0 {
                TrackStaleness::Fresh
            } else if age < 30.0 {
                TrackStaleness::Aging
            } else if age < 120.0 {
                TrackStaleness::Stale
            } else {
                TrackStaleness::Lost
            };
            TrackQuality {
                existence_prob: t.quality,
                identification_confidence: t.quality,
                position_accuracy_cep_m: 50.0 / t.quality.max(0.01),
                age_s: age,
                update_rate_hz: if age > 0.0 { 1.0 / age } else { 0.0 },
                staleness,
            }
        })
    }

    pub fn get(&self, track_id: &str) -> Option<&Track> {
        self.tracks.get(track_id)
    }

    /// Evict tracks that have not been updated within `max_age_s`.
    pub fn evict_stale(&mut self, now: f64, max_age_s: f64) -> Vec<String> {
        let to_remove: Vec<String> = self
            .tracks
            .iter()
            .filter(|(_, t)| now - t.last_update > max_age_s)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &to_remove {
            self.tracks.remove(id);
        }
        to_remove
    }
}

impl Default for TrackManager {
    fn default() -> Self {
        Self::new()
    }
}

fn haversine(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6_371_000.0;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    r * 2.0 * a.sqrt().asin()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contact(lat: f64, lon: f64, quality: f64) -> SensorContact {
        SensorContact {
            contact_id: "ct-1".into(),
            classification: "unknown".into(),
            lat,
            lon,
            speed_ms: 10.0,
            heading_deg: 90.0,
            range_m: 5000.0,
            bearing_deg: 0.0,
            quality,
            timestamp: 100.0,
        }
    }

    #[test]
    fn test_first_contact_creates_track() {
        let mut tm = TrackManager::new();
        let r = tm.correlate(contact(30.0, 120.0, 0.8));
        assert!(r.is_new_track);
        assert_eq!(tm.track_count(), 1);
    }

    #[test]
    fn test_nearby_contact_correlates() {
        let mut tm = TrackManager::new();
        tm.correlate(contact(30.0, 120.0, 0.8));
        let r = tm.correlate(contact(30.0001, 120.0001, 0.9));
        assert!(!r.is_new_track);
        assert_eq!(tm.track_count(), 1);
    }

    #[test]
    fn test_distant_contact_creates_new_track() {
        let mut tm = TrackManager::new();
        tm.correlate(contact(30.0, 120.0, 0.8));
        let r = tm.correlate(contact(40.0, 130.0, 0.9));
        assert!(r.is_new_track);
        assert_eq!(tm.track_count(), 2);
    }

    #[test]
    fn test_mark_identification_updates_classification() {
        let mut tm = TrackManager::new();
        let r = tm.correlate(contact(30.0, 120.0, 0.8));
        let id = tm.mark_identification(&r.track_id, "destroyer", 0.95);
        assert_eq!(id.classification, "destroyer");
        assert_eq!(id.classification_source, "shore_confirmation");
        let t = tm.get(&r.track_id).unwrap();
        assert_eq!(t.classification, "destroyer");
    }

    #[test]
    fn test_quality_metrics() {
        let mut tm = TrackManager::new();
        let r = tm.correlate(contact(30.0, 120.0, 0.9));
        let q = tm.quality(&r.track_id, 100.5).unwrap();
        assert!(matches!(q.staleness, TrackStaleness::Fresh));
        assert!(q.existence_prob > 0.5);
    }

    #[test]
    fn test_evict_stale_tracks() {
        let mut tm = TrackManager::new();
        tm.correlate(contact(30.0, 120.0, 0.8));
        let removed = tm.evict_stale(1000.0, 60.0);
        assert_eq!(removed.len(), 1);
        assert_eq!(tm.track_count(), 0);
    }
}
