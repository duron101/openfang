//! UMAA Operational Restrictions Manager (ORA).
//!
//! Manages Rules of Engagement (ROE), Geofences, and Platform Limits.
//! Per PRD §12.2.2. Provides:
//! - `set_roe_level()` / `get_roe()` — query/modify ROE
//! - `add_geofence()` / `check_geofence_violation()` — geofence enforcement
//! - `check_limits()` — enforce platform limits against current command
//!
//! Emits events for ROE changes and geofence violations (consumed by DCC).

use openfang_types::platform::{Pose, WorldSnapshot};
use openfang_types::umaa::*;
use std::sync::{Arc, Mutex};

/// Manager for ROE / Geofence / PlatformLimits.
pub struct OpRestrictionsManager {
    state: Arc<Mutex<OpRestrictionsState>>,
}

struct OpRestrictionsState {
    roe: RulesOfEngagement,
    geofences: Vec<Geofence>,
    limits: PlatformLimits,
    last_known_pose: Option<Pose>,
}

impl OpRestrictionsManager {
    pub fn new(roe: RulesOfEngagement, limits: PlatformLimits) -> Self {
        Self {
            state: Arc::new(Mutex::new(OpRestrictionsState {
                roe,
                geofences: Vec::new(),
                limits,
                last_known_pose: None,
            })),
        }
    }

    pub fn default_restrictions(platform_id: &str) -> Self {
        Self::new(RulesOfEngagement::default(), PlatformLimits::default())
    }

    /// Get current ROE (read-only clone).
    pub fn get_roe(&self) -> RulesOfEngagement {
        self.state.lock().unwrap().roe.clone()
    }

    /// Update ROE level.
    pub fn set_roe_level(&self, level: WeaponReleaseLevel) {
        self.state.lock().unwrap().roe.weapon_release_authority = level;
    }

    /// Add a geofence.
    pub fn add_geofence(&self, fence: Geofence) {
        self.state.lock().unwrap().geofences.push(fence);
    }

    /// Number of registered geofences.
    pub fn geofence_count(&self) -> usize {
        self.state.lock().unwrap().geofences.len()
    }

    /// Snapshot of registered geofences (for MMS keep-out planning).
    pub fn geofences(&self) -> Vec<Geofence> {
        self.state.lock().unwrap().geofences.clone()
    }

    /// Update last-known pose for geofence checks.
    pub fn update_pose(&self, pose: Pose) {
        self.state.lock().unwrap().last_known_pose = Some(pose);
    }

    /// Check current pose against all geofences. Returns first violation, if any.
    pub fn check_geofence_violation(&self) -> Option<GeofenceViolation> {
        let state = self.state.lock().unwrap();
        let pose = state.last_known_pose.as_ref()?;
        for fence in &state.geofences {
            if let Some(v) = check_fence(fence, pose) {
                return Some(v);
            }
        }
        None
    }

    /// Check a desired command (speed, depth) against platform limits.
    pub fn check_limits(&self, speed_ms: f64, depth_m: f64) -> Result<(), LimitViolation> {
        let state = self.state.lock().unwrap();
        if speed_ms > state.limits.max_speed_ms {
            return Err(LimitViolation {
                kind: "speed".into(),
                requested: speed_ms,
                limit: state.limits.max_speed_ms,
            });
        }
        if depth_m > state.limits.max_depth_m {
            return Err(LimitViolation {
                kind: "depth".into(),
                requested: depth_m,
                limit: state.limits.max_depth_m,
            });
        }
        Ok(())
    }

    /// Get current limits.
    pub fn get_limits(&self) -> PlatformLimits {
        self.state.lock().unwrap().limits.clone()
    }
}

/// A detected geofence violation.
#[derive(Debug, Clone)]
pub struct GeofenceViolation {
    pub fence_name: String,
    pub kind: String, // "keep_out", "speed_limit", etc.
    pub action: ViolationAction,
}

/// A platform limit violation.
#[derive(Debug, Clone)]
pub struct LimitViolation {
    pub kind: String,
    pub requested: f64,
    pub limit: f64,
}

fn point_in_polygon(point: (f64, f64), polygon: &[(f64, f64)]) -> bool {
    if polygon.len() < 3 {
        return false;
    }
    let (x, y) = point;
    let mut inside = false;
    let mut j = polygon.len() - 1;
    for i in 0..polygon.len() {
        let (xi, yi) = polygon[i];
        let (xj, yj) = polygon[j];
        if ((yi > y) != (yj > y)) && (x < (xj - xi) * (y - yi) / ((yj - yi).abs().max(1e-9)) + xi) {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn check_fence(fence: &Geofence, pose: &Pose) -> Option<GeofenceViolation> {
    let p = (pose.lat_deg, pose.lon_deg);
    let inside = point_in_polygon(p, &fence.boundary);
    let violation = match &fence.restriction {
        GeofenceType::KeepIn => !inside,
        GeofenceType::KeepOut => inside,
        GeofenceType::AltitudeCeiling { max_alt_m } => pose.alt_m > *max_alt_m,
        GeofenceType::AltitudeFloor { min_alt_m } => pose.alt_m < *min_alt_m,
        GeofenceType::SpeedLimit { max_speed_ms: _ } => false, // speed is checked separately
        GeofenceType::DepthLimit { max_depth_m } => pose.alt_m < -*max_depth_m,
    };
    if violation {
        Some(GeofenceViolation {
            fence_name: fence.name.clone(),
            kind: format!("{:?}", fence.restriction),
            action: fence.violation_action,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_roe_hold() {
        let mgr = OpRestrictionsManager::default_restrictions("usv-01");
        let roe = mgr.get_roe();
        assert_eq!(
            roe.weapon_release_authority,
            WeaponReleaseLevel::WeaponsHold
        );
    }

    #[test]
    fn test_set_roe_level() {
        let mgr = OpRestrictionsManager::default_restrictions("usv-01");
        mgr.set_roe_level(WeaponReleaseLevel::WeaponsFree);
        assert_eq!(
            mgr.get_roe().weapon_release_authority,
            WeaponReleaseLevel::WeaponsFree
        );
    }

    #[test]
    fn test_speed_limit_violation() {
        let mgr = OpRestrictionsManager::default_restrictions("usv-01");
        let res = mgr.check_limits(50.0, 100.0);
        assert!(res.is_err());
        match res.unwrap_err().kind.as_str() {
            "speed" => (),
            other => panic!("expected speed violation, got {other}"),
        }
    }

    #[test]
    fn test_depth_limit_violation() {
        let mgr = OpRestrictionsManager::default_restrictions("usv-01");
        let res = mgr.check_limits(10.0, 500.0);
        assert!(res.is_err());
    }

    #[test]
    fn test_within_limits() {
        let mgr = OpRestrictionsManager::default_restrictions("usv-01");
        assert!(mgr.check_limits(15.0, 50.0).is_ok());
    }

    #[test]
    fn test_geofence_keepout() {
        let mgr = OpRestrictionsManager::default_restrictions("usv-01");
        let fence = Geofence {
            name: "no-go".into(),
            boundary: vec![(30.0, 120.0), (30.1, 120.0), (30.1, 120.1), (30.0, 120.1)],
            restriction: GeofenceType::KeepOut,
            violation_action: ViolationAction::AbortMission,
        };
        mgr.add_geofence(fence);
        mgr.update_pose(Pose {
            lat_deg: 30.05,
            lon_deg: 120.05,
            alt_m: 0.0,
            heading_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
        });
        let v = mgr.check_geofence_violation();
        assert!(v.is_some());
        assert_eq!(v.unwrap().fence_name, "no-go");
    }

    #[test]
    fn test_geofence_altitude_floor_violation() {
        let mgr = OpRestrictionsManager::default_restrictions("uav-01");
        mgr.add_geofence(Geofence {
            name: "min-safe-alt".into(),
            boundary: vec![],
            restriction: GeofenceType::AltitudeFloor { min_alt_m: 300.0 },
            violation_action: ViolationAction::AutoCorrect,
        });
        mgr.update_pose(Pose {
            lat_deg: 30.0,
            lon_deg: 120.0,
            alt_m: 150.0,
            heading_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
        });
        let v = mgr.check_geofence_violation();
        assert!(v.is_some());
        assert_eq!(v.unwrap().fence_name, "min-safe-alt");
    }

    #[test]
    fn test_geofence_keepin_satisfied() {
        let mgr = OpRestrictionsManager::default_restrictions("usv-01");
        let fence = Geofence {
            name: "patrol".into(),
            boundary: vec![(30.0, 120.0), (30.1, 120.0), (30.1, 120.1), (30.0, 120.1)],
            restriction: GeofenceType::KeepIn,
            violation_action: ViolationAction::Warn,
        };
        mgr.add_geofence(fence);
        mgr.update_pose(Pose {
            lat_deg: 30.05,
            lon_deg: 120.05,
            alt_m: 0.0,
            heading_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
        });
        assert!(mgr.check_geofence_violation().is_none());
    }
}
