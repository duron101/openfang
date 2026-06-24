//! UMAA Navigation Service — separates position estimation from route planning.
//!
//! Per PRD §12.2.3: GPS/INS/DeadReckoning fusion for the ownship.
//! Falls back gracefully as sensors degrade.

use serde::{Deserialize, Serialize};

/// Source of a position measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavSource {
    Gps,
    Ins,
    DeadReckoning,
    VisualOdometry,
    Fused,
}

/// A single sensor measurement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NavMeasurement {
    pub source: NavSource,
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub alt_m: f64,
    pub heading_deg: f64,
    pub speed_ms: f64,
    pub accuracy_cep_m: f64,
    pub timestamp: f64,
}

/// Fused position estimate (output).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionEstimate {
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub alt_m: f64,
    pub heading_deg: f64,
    pub speed_ms: f64,
    pub accuracy_cep_m: f64,
    pub source: NavSource,
    pub is_valid: bool,
    pub timestamp: f64,
}

/// Navigation service — owns the position estimation logic.
pub struct NavigationService {
    gps: Option<NavMeasurement>,
    ins: Option<NavMeasurement>,
    /// Last known good position
    last_fused: Option<PositionEstimate>,
    /// Last update time (for staleness detection)
    last_update: Option<f64>,
}

impl NavigationService {
    pub fn new() -> Self {
        Self {
            gps: None,
            ins: None,
            last_fused: None,
            last_update: None,
        }
    }

    /// Update with the latest GPS measurement.
    pub fn update_gps(&mut self, m: NavMeasurement) {
        self.gps = Some(m);
    }

    /// Update with the latest INS measurement.
    pub fn update_ins(&mut self, m: NavMeasurement) {
        self.ins = Some(m);
    }

    /// Drop GPS (e.g. jamming detected) — force fallback to INS.
    pub fn drop_gps(&mut self) {
        self.gps = None;
    }

    /// Compute fused position. Priority: GPS+INS > GPS > INS > DeadReckoning(last fused).
    pub fn fuse(&mut self, now: f64) -> PositionEstimate {
        // GPS available and accurate → use directly
        if let Some(gps) = &self.gps {
            if gps.accuracy_cep_m < 50.0 {
                let fused = PositionEstimate {
                    lat_deg: gps.lat_deg,
                    lon_deg: gps.lon_deg,
                    alt_m: gps.alt_m,
                    heading_deg: gps.heading_deg,
                    speed_ms: gps.speed_ms,
                    accuracy_cep_m: gps.accuracy_cep_m,
                    source: NavSource::Fused,
                    is_valid: true,
                    timestamp: now,
                };
                self.last_fused = Some(fused.clone());
                self.last_update = Some(now);
                return fused;
            }
        }
        // INS only
        if let Some(ins) = &self.ins {
            let fused = PositionEstimate {
                lat_deg: ins.lat_deg,
                lon_deg: ins.lon_deg,
                alt_m: ins.alt_m,
                heading_deg: ins.heading_deg,
                speed_ms: ins.speed_ms,
                accuracy_cep_m: ins.accuracy_cep_m,
                source: NavSource::Ins,
                is_valid: true,
                timestamp: now,
            };
            self.last_fused = Some(fused.clone());
            self.last_update = Some(now);
            return fused;
        }
        // Fallback: DeadReckoning from last fused
        match &self.last_fused {
            Some(last) => {
                // Drift estimate grows with time
                let drift = match self.last_update {
                    Some(t) => 10.0 * (now - t).max(0.0), // 10 m/s drift
                    None => 0.0,
                };
                let fused = PositionEstimate {
                    accuracy_cep_m: last.accuracy_cep_m + drift,
                    source: NavSource::DeadReckoning,
                    is_valid: true,
                    timestamp: now,
                    ..last.clone()
                };
                self.last_fused = Some(fused.clone());
                self.last_update = Some(now);
                fused
            }
            None => PositionEstimate {
                lat_deg: 0.0,
                lon_deg: 0.0,
                alt_m: 0.0,
                heading_deg: 0.0,
                speed_ms: 0.0,
                accuracy_cep_m: f64::INFINITY,
                source: NavSource::DeadReckoning,
                is_valid: false,
                timestamp: now,
            },
        }
    }

    pub fn last_fused(&self) -> Option<&PositionEstimate> {
        self.last_fused.as_ref()
    }
}

impl Default for NavigationService {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gps_meas() -> NavMeasurement {
        NavMeasurement {
            source: NavSource::Gps,
            lat_deg: 30.0,
            lon_deg: 120.0,
            alt_m: 0.0,
            heading_deg: 90.0,
            speed_ms: 15.0,
            accuracy_cep_m: 5.0,
            timestamp: 100.0,
        }
    }

    fn ins_meas() -> NavMeasurement {
        NavMeasurement {
            source: NavSource::Ins,
            lat_deg: 30.001,
            lon_deg: 120.001,
            alt_m: 0.0,
            heading_deg: 90.5,
            speed_ms: 15.0,
            accuracy_cep_m: 20.0,
            timestamp: 100.0,
        }
    }

    #[test]
    fn test_gps_only_uses_gps() {
        let mut nav = NavigationService::new();
        nav.update_gps(gps_meas());
        let p = nav.fuse(100.0);
        assert_eq!(p.source, NavSource::Fused);
        assert!((p.lat_deg - 30.0).abs() < 0.0001);
        assert!(p.is_valid);
    }

    #[test]
    fn test_gps_dropped_falls_back_to_ins() {
        let mut nav = NavigationService::new();
        nav.update_gps(gps_meas());
        nav.fuse(100.0); // initial fusion
        nav.drop_gps();
        nav.update_ins(ins_meas());
        let p = nav.fuse(101.0);
        assert_eq!(p.source, NavSource::Ins);
        assert!(p.is_valid);
    }

    #[test]
    fn test_no_sensors_returns_dead_reckoning() {
        let mut nav = NavigationService::new();
        nav.update_gps(gps_meas());
        let _ = nav.fuse(100.0);
        nav.drop_gps();
        let p = nav.fuse(120.0);
        assert_eq!(p.source, NavSource::DeadReckoning);
        assert!(p.accuracy_cep_m > 5.0, "drift should grow over time");
    }

    #[test]
    fn test_no_measurements_returns_invalid() {
        let mut nav = NavigationService::new();
        let p = nav.fuse(0.0);
        assert!(!p.is_valid);
        assert_eq!(p.accuracy_cep_m, f64::INFINITY);
    }
}
