//! Snapshot pipeline — normalization, a read-only shared cache, and contract
//! equivalence checks.
//!
//! Every adapter (ArkSim, DDS, Mock, …) produces a [`WorldSnapshot`]. The
//! pipeline:
//!
//! 1. **Normalizes** the snapshot to a canonical form (angles wrapped into
//!    `[0,360)`, damage/quality clamped to `[0,1]`, entities deterministically
//!    ordered) so that two backends representing the same world produce
//!    structurally comparable snapshots.
//! 2. Stores the latest normalized snapshot in a [`SnapshotCache`] that hands
//!    out **read-only clones**. The cache holds no adapter reference, so a
//!    read-only query can never trigger a command send — the boundary is
//!    structural, not merely conventional.
//! 3. Provides [`snapshots_equivalent`] to assert that simulation and hardware
//!    backends are *contract-equivalent* (semantic equality within tolerance),
//!    rather than byte-identical.

use std::sync::RwLock;

use openfang_types::platform::{PlatformState, Track, WorldSnapshot};

// ─────────────────────────────────────────────
// Normalization
// ─────────────────────────────────────────────

/// Wrap an angle in degrees into the canonical `[0, 360)` range.
pub fn wrap_deg(deg: f64) -> f64 {
    if !deg.is_finite() {
        return 0.0;
    }
    let r = deg % 360.0;
    if r < 0.0 {
        r + 360.0
    } else {
        r
    }
}

fn clamp_unit(x: f64) -> f64 {
    if !x.is_finite() {
        return 0.0;
    }
    x.clamp(0.0, 1.0)
}

/// Normalize a snapshot in place to canonical form.
pub fn normalize(snapshot: &mut WorldSnapshot) {
    if !snapshot.timestamp.is_finite() || snapshot.timestamp < 0.0 {
        snapshot.timestamp = 0.0;
    }
    for p in &mut snapshot.platforms {
        normalize_platform(p);
    }
    snapshot.platforms.sort_by(|a, b| a.id.cmp(&b.id));
    snapshot
        .active_munitions
        .sort_by(|a, b| a.munition_id.cmp(&b.munition_id));
}

fn normalize_platform(p: &mut PlatformState) {
    p.pose.heading_deg = wrap_deg(p.pose.heading_deg);
    p.velocity.course_deg = wrap_deg(p.velocity.course_deg);
    p.damage = clamp_unit(p.damage);
    for t in &mut p.tracks {
        t.quality = clamp_unit(t.quality);
        if let Some(h) = t.heading_deg {
            t.heading_deg = Some(wrap_deg(h));
        }
    }
    p.tracks.sort_by(|a, b| a.track_id.cmp(&b.track_id));
    p.onboard_sensors
        .sort_by(|a, b| a.sensor_id.cmp(&b.sensor_id));
    p.onboard_weapons
        .sort_by(|a, b| a.weapon_id.cmp(&b.weapon_id));
    p.onboard_jammers
        .sort_by(|a, b| a.jammer_id.cmp(&b.jammer_id));
}

/// Normalize and return a snapshot (convenience for pipelines).
pub fn normalized(mut snapshot: WorldSnapshot) -> WorldSnapshot {
    normalize(&mut snapshot);
    snapshot
}

// ─────────────────────────────────────────────
// Read-only cache
// ─────────────────────────────────────────────

/// Holds the latest normalized world snapshot and hands out read-only clones.
///
/// This is the only thing the slow loop (LLM/workflow) reads from; it has no
/// way to issue commands, enforcing the "read-only path cannot actuate" rule.
pub struct SnapshotCache {
    latest: RwLock<Option<WorldSnapshot>>,
}

impl SnapshotCache {
    pub fn new() -> Self {
        Self {
            latest: RwLock::new(None),
        }
    }

    /// Normalize and store a freshly polled snapshot.
    pub fn update(&self, snapshot: WorldSnapshot) {
        let snap = normalized(snapshot);
        *self.latest.write().unwrap_or_else(|e| e.into_inner()) = Some(snap);
    }

    /// Read-only clone of the latest snapshot, if any.
    pub fn latest(&self) -> Option<WorldSnapshot> {
        self.latest
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Timestamp of the latest snapshot.
    pub fn timestamp(&self) -> Option<f64> {
        self.latest
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|s| s.timestamp)
    }

    /// Find a platform by id in the latest snapshot (read-only clone).
    pub fn find_platform(&self, id: &str) -> Option<PlatformState> {
        self.latest
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|s| s.find_platform(id).cloned())
    }

    /// Find a track by id across all platforms (read-only clone).
    pub fn find_track(&self, track_id: &str) -> Option<Track> {
        let guard = self.latest.read().unwrap_or_else(|e| e.into_inner());
        let snap = guard.as_ref()?;
        for p in &snap.platforms {
            if let Some(t) = p.tracks.iter().find(|t| t.track_id == track_id) {
                return Some(t.clone());
            }
        }
        None
    }

    /// One-line human-readable summary for tool output / logging.
    pub fn summary(&self) -> String {
        match self.latest() {
            None => "no snapshot yet".into(),
            Some(s) => {
                let tracks: usize = s.platforms.iter().map(|p| p.tracks.len()).sum();
                format!(
                    "t={:.2}s platforms={} tracks={} munitions={} events={}",
                    s.timestamp,
                    s.platforms.len(),
                    tracks,
                    s.active_munitions.len(),
                    s.events.len()
                )
            }
        }
    }
}

impl Default for SnapshotCache {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────────
// Contract equivalence
// ─────────────────────────────────────────────

/// Tolerances for [`snapshots_equivalent`].
#[derive(Debug, Clone, Copy)]
pub struct EquivalenceTolerance {
    pub time_s: f64,
    pub position_deg: f64,
    pub altitude_m: f64,
    pub angle_deg: f64,
    pub speed_ms: f64,
}

impl Default for EquivalenceTolerance {
    fn default() -> Self {
        Self {
            time_s: 1e-3,
            position_deg: 1e-6,
            altitude_m: 0.5,
            angle_deg: 0.5,
            speed_ms: 0.1,
        }
    }
}

fn angle_close(a: f64, b: f64, tol: f64) -> bool {
    let d = (wrap_deg(a) - wrap_deg(b)).abs();
    let d = d.min(360.0 - d);
    d <= tol
}

/// Assert two snapshots are *semantically* equivalent within tolerance.
///
/// Used to verify that the same world rendered by two backends (e.g. ArkSim and
/// a mock, or simulation and hardware) maps to the same contract. Returns
/// `Ok(())` or a description of the first divergence.
pub fn snapshots_equivalent(
    a: &WorldSnapshot,
    b: &WorldSnapshot,
    tol: EquivalenceTolerance,
) -> Result<(), String> {
    let a = normalized(a.clone());
    let b = normalized(b.clone());

    if (a.timestamp - b.timestamp).abs() > tol.time_s {
        return Err(format!("timestamp {} != {}", a.timestamp, b.timestamp));
    }
    if a.platforms.len() != b.platforms.len() {
        return Err(format!(
            "platform count {} != {}",
            a.platforms.len(),
            b.platforms.len()
        ));
    }
    for (pa, pb) in a.platforms.iter().zip(b.platforms.iter()) {
        if pa.id != pb.id {
            return Err(format!("platform id {} != {}", pa.id, pb.id));
        }
        if pa.affiliation != pb.affiliation {
            return Err(format!("{}: affiliation mismatch", pa.id));
        }
        if pa.domain != pb.domain {
            return Err(format!("{}: domain mismatch", pa.id));
        }
        if (pa.pose.lat_deg - pb.pose.lat_deg).abs() > tol.position_deg
            || (pa.pose.lon_deg - pb.pose.lon_deg).abs() > tol.position_deg
        {
            return Err(format!("{}: position mismatch", pa.id));
        }
        if (pa.pose.alt_m - pb.pose.alt_m).abs() > tol.altitude_m {
            return Err(format!("{}: altitude mismatch", pa.id));
        }
        if !angle_close(pa.pose.heading_deg, pb.pose.heading_deg, tol.angle_deg) {
            return Err(format!("{}: heading mismatch", pa.id));
        }
        if (pa.velocity.speed_ms - pb.velocity.speed_ms).abs() > tol.speed_ms {
            return Err(format!("{}: speed mismatch", pa.id));
        }
        let ta: Vec<&String> = pa.tracks.iter().map(|t| &t.track_id).collect();
        let tb: Vec<&String> = pb.tracks.iter().map(|t| &t.track_id).collect();
        if ta != tb {
            return Err(format!("{}: track id set mismatch", pa.id));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::platform::*;

    fn platform(id: &str, heading: f64) -> PlatformState {
        PlatformState {
            id: id.into(),
            name: id.into(),
            platform_type: "usv".into(),
            affiliation: Affiliation::Blue,
            domain: Domain::Surface,
            pose: Pose {
                lat_deg: 30.0,
                lon_deg: 120.0,
                alt_m: 0.0,
                heading_deg: heading,
                pitch_deg: 0.0,
                roll_deg: 0.0,
            },
            velocity: Velocity {
                speed_ms: 10.0,
                vertical_rate_ms: 0.0,
                course_deg: heading,
            },
            fuel: FuelStatus {
                remaining_kg: 100.0,
                max_kg: 100.0,
                consumption_rate_kg_s: 0.0,
            },
            damage: 0.0,
            tracks: vec![],
            onboard_sensors: vec![],
            onboard_weapons: vec![],
            onboard_jammers: vec![],
            current_target: None,
            commander: None,
            survivability: None,
            emcon: None,
            link: None,
        }
    }

    #[test]
    fn wrap_deg_handles_negative_and_overflow() {
        assert_eq!(wrap_deg(-90.0), 270.0);
        assert_eq!(wrap_deg(450.0), 90.0);
        assert_eq!(wrap_deg(0.0), 0.0);
        assert_eq!(wrap_deg(f64::NAN), 0.0);
    }

    #[test]
    fn normalize_wraps_and_sorts() {
        let mut snap = WorldSnapshot {
            timestamp: 5.0,
            platforms: vec![platform("usv-02", 370.0), platform("usv-01", -10.0)],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        };
        normalize(&mut snap);
        assert_eq!(snap.platforms[0].id, "usv-01");
        assert_eq!(snap.platforms[0].pose.heading_deg, 350.0);
        assert_eq!(snap.platforms[1].pose.heading_deg, 10.0);
    }

    #[test]
    fn cache_hands_out_readonly_clones() {
        let cache = SnapshotCache::new();
        assert!(cache.latest().is_none());
        cache.update(WorldSnapshot {
            timestamp: 1.0,
            platforms: vec![platform("usv-01", 90.0)],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        });
        assert_eq!(cache.timestamp(), Some(1.0));
        assert!(cache.find_platform("usv-01").is_some());
        assert!(cache.find_platform("nope").is_none());
        assert!(cache.summary().contains("platforms=1"));
    }

    #[test]
    fn equivalent_snapshots_from_different_encodings() {
        // Same world; one source reports heading as -10°, the other as 350°.
        let a = WorldSnapshot {
            timestamp: 10.0,
            platforms: vec![platform("usv-01", -10.0)],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        };
        let b = WorldSnapshot {
            timestamp: 10.0,
            platforms: vec![platform("usv-01", 350.0)],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        };
        assert!(snapshots_equivalent(&a, &b, EquivalenceTolerance::default()).is_ok());
    }

    #[test]
    fn divergent_snapshots_detected() {
        let a = WorldSnapshot {
            timestamp: 10.0,
            platforms: vec![platform("usv-01", 0.0)],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        };
        let mut p = platform("usv-01", 0.0);
        p.pose.lat_deg = 31.0;
        let b = WorldSnapshot {
            timestamp: 10.0,
            platforms: vec![p],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        };
        assert!(snapshots_equivalent(&a, &b, EquivalenceTolerance::default()).is_err());
    }
}
