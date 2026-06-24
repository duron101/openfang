//! CCA tactical role controller (ABMS role-driven behavior).
//!
//! A Collaborative Combat Aircraft (CCA) receives a tactical *role* from its
//! commander (see [`CcaRole`]). The role — not per-tick human tasking — drives
//! the autonomous posture: emissions control (EMCON), sensor posture, jamming
//! posture, weapon safing intent, and formation intent.
//!
//! This module is intentionally self-contained and side-effect free: it maps a
//! role to a [`RolePosture`], and a `(state, posture)` pair to concrete
//! [`PlatformCommand`]s. Actual weapon *release* is never produced here — that
//! stays behind the CommandGate / ROE interlock (the Iron Law). This layer only
//! ever *safes* weapons; arming/firing is decided downstream.

use openfang_types::platform::{Affiliation, CcaRole, PlatformCommand, PlatformState};
use std::sync::{Arc, Mutex};

/// Emissions-control posture, from most to least covert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmconLevel {
    /// Full emissions silence — no radar, no active comms.
    Silent,
    /// Receive-only / passive sensors; comms on a tight leash.
    Restricted,
    /// Normal emissions allowed.
    Normal,
    /// Deliberately conspicuous (decoy) — emit to be seen.
    Active,
}

/// Concrete posture derived from a role. Describes *intent*; downstream gates
/// still arbitrate safety.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolePosture {
    pub role: CcaRole,
    pub emcon: EmconLevel,
    /// Active radar permitted (vs passive ESM/EOIR only).
    pub radar_active: bool,
    /// Active comms / data link permitted.
    pub comm_active: bool,
    /// Jamming posture (EW attack) is part of this role.
    pub jammer_active: bool,
    /// Whether weapons should be held safe for this posture.
    pub weapons_safed: bool,
    /// Formation intent string (matches `PlatformCommand::FormUp` types), if any.
    pub formation: Option<&'static str>,
    pub description: &'static str,
}

/// Map a role to its baseline posture (ABMS behavior matrix).
pub fn posture_for(role: CcaRole) -> RolePosture {
    use CcaRole::*;
    use EmconLevel::*;
    let (emcon, radar, comm, jam, safed, formation, desc): (
        EmconLevel,
        bool,
        bool,
        bool,
        bool,
        Option<&'static str>,
        &'static str,
    ) = match role {
        Recon => (
            Restricted,
            false,
            true,
            false,
            true,
            None,
            "passive ISR collection, weapons safe",
        ),
        Designator => (
            Restricted,
            true,
            true,
            false,
            true,
            None,
            "illuminate/designate for shooters",
        ),
        Relay => (
            Restricted,
            false,
            true,
            false,
            true,
            Some("column"),
            "comm/data relay node",
        ),
        Striker => (
            Normal,
            true,
            true,
            false,
            false,
            Some("echelon_left"),
            "offensive strike, weapons available",
        ),
        Decoy => (
            Active,
            true,
            true,
            false,
            true,
            None,
            "conspicuous decoy, draws attention",
        ),
        Intercept => (
            Normal,
            true,
            true,
            false,
            false,
            None,
            "air intercept, weapons available",
        ),
        Patrol => (
            Restricted,
            false,
            true,
            false,
            true,
            Some("line_abreast"),
            "routine patrol, low emissions",
        ),
        Escort => (
            Normal,
            true,
            true,
            false,
            false,
            Some("vee"),
            "escort protected asset",
        ),
        Surveil => (
            Restricted,
            false,
            true,
            false,
            true,
            None,
            "persistent area surveillance",
        ),
        Leader => (
            Normal,
            true,
            true,
            false,
            true,
            Some("vee"),
            "formation lead / C2 node",
        ),
        Adaptive => (
            Restricted,
            false,
            true,
            false,
            true,
            None,
            "adaptive — re-roles from picture",
        ),
        EwProtection => (
            Restricted,
            false,
            true,
            false,
            true,
            None,
            "defensive EW for the package",
        ),
        EwJamming => (
            Active,
            false,
            true,
            true,
            true,
            None,
            "electronic attack / jamming",
        ),
    };
    RolePosture {
        role,
        emcon,
        radar_active: radar,
        comm_active: comm,
        jammer_active: jam,
        weapons_safed: safed,
        formation,
        description: desc,
    }
}

/// Generate the concrete platform commands that bring `state` into `posture`.
///
/// Only emits commands where the current state diverges from the desired
/// posture, and only ever *reduces* lethality (safe weapons, stop jammers when
/// not an EW role). Never emits a fire command.
pub fn posture_commands(state: &PlatformState, posture: &RolePosture) -> Vec<PlatformCommand> {
    let mut cmds = Vec::new();
    let pid = state.id.clone();

    // Comms follow EMCON (Silent ⇒ off).
    if posture.comm_active {
        cmds.push(PlatformCommand::CommOn {
            platform_id: pid.clone(),
        });
    } else {
        cmds.push(PlatformCommand::CommOff {
            platform_id: pid.clone(),
        });
    }

    // Jammers: only EW-jamming roles keep them up; otherwise stop any active beam.
    if !posture.jammer_active {
        for jammer in &state.onboard_jammers {
            if jammer.is_active {
                cmds.push(PlatformCommand::JamStop {
                    platform_id: pid.clone(),
                    jammer_id: jammer.jammer_id.clone(),
                });
            }
        }
    }

    // Weapon safing intent (never an arm/fire — Iron Law).
    if posture.weapons_safed && !state.onboard_weapons.is_empty() {
        cmds.push(PlatformCommand::WeaponSafeAll {
            platform_id: pid.clone(),
        });
    }

    cmds
}

/// Stateful controller holding the currently-assigned role for one CCA.
pub struct CcaRoleController {
    platform_id: String,
    role: Arc<Mutex<CcaRole>>,
}

impl CcaRoleController {
    pub fn new(platform_id: impl Into<String>, initial: CcaRole) -> Self {
        Self {
            platform_id: platform_id.into(),
            role: Arc::new(Mutex::new(initial)),
        }
    }

    pub fn platform_id(&self) -> &str {
        &self.platform_id
    }

    /// Commander assigns a new role.
    pub fn assign(&self, role: CcaRole) {
        *self.role.lock().unwrap() = role;
    }

    pub fn current(&self) -> CcaRole {
        *self.role.lock().unwrap()
    }

    /// Effective posture. For [`CcaRole::Adaptive`] the role is resolved against
    /// the tactical picture first (without mutating the assigned role).
    pub fn posture(&self, snapshot_self: &PlatformState) -> RolePosture {
        let role = match self.current() {
            CcaRole::Adaptive => adapt_role(snapshot_self),
            other => other,
        };
        posture_for(role)
    }

    /// Commands to drive the platform into its current effective posture.
    pub fn posture_commands(&self, snapshot_self: &PlatformState) -> Vec<PlatformCommand> {
        posture_commands(snapshot_self, &self.posture(snapshot_self))
    }
}

/// Resolve [`CcaRole::Adaptive`] into a concrete role from the tactical picture.
///
/// Priority: a close hostile track ⇒ `Intercept`; protecting a commander/asset
/// ⇒ `Escort`; otherwise `Patrol`.
pub fn adapt_role(state: &PlatformState) -> CcaRole {
    const THREAT_RANGE_M: f64 = 60_000.0;

    let hostile_close = state.tracks.iter().any(|t| {
        !t.stale
            && matches!(t.affiliation, Affiliation::Red | Affiliation::Foe)
            && t.range_m.map(|r| r <= THREAT_RANGE_M).unwrap_or(false)
    });
    if hostile_close {
        return CcaRole::Intercept;
    }
    if state.commander.is_some() {
        return CcaRole::Escort;
    }
    CcaRole::Patrol
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::platform::{
        Affiliation, Domain, FuelStatus, JammerState, Pose, SensorState, SensorType, Track,
        Velocity, WeaponState,
    };

    fn base_state(id: &str) -> PlatformState {
        PlatformState {
            id: id.into(),
            name: id.into(),
            platform_type: "cca".into(),
            affiliation: Affiliation::Blue,
            domain: Domain::Air,
            pose: Pose {
                lat_deg: 30.0,
                lon_deg: 120.0,
                alt_m: 3000.0,
                heading_deg: 0.0,
                pitch_deg: 0.0,
                roll_deg: 0.0,
            },
            velocity: Velocity {
                speed_ms: 200.0,
                vertical_rate_ms: 0.0,
                course_deg: 0.0,
            },
            fuel: FuelStatus {
                remaining_kg: 500.0,
                max_kg: 1000.0,
                consumption_rate_kg_s: 0.05,
            },
            damage: 0.0,
            tracks: vec![],
            onboard_sensors: vec![SensorState {
                sensor_id: "radar-1".into(),
                sensor_type: SensorType::Radar,
                mode: "active".into(),
                frequency_hz: None,
                bandwidth_hz: None,
                azimuth_fov_deg: None,
                elevation_fov_deg: None,
                range_max_m: None,
                damage: 0.0,
                host_platform_id: id.into(),
            }],
            onboard_weapons: vec![WeaponState {
                weapon_id: "aam-1".into(),
                weapon_type: "aam".into(),
                quantity_remaining: 4.0,
                max_range_m: None,
                min_range_m: None,
                guidance_type: None,
                speed_ms: None,
                is_ready: true,
                quantity_from_snapshot: true,
            }],
            onboard_jammers: vec![JammerState {
                jammer_id: "jam-1".into(),
                host_id: id.into(),
                is_active: true,
                beams: vec![],
            }],
            current_target: None,
            commander: None,
            survivability: None,
            emcon: None,
            link: None,
        }
    }

    fn hostile_track(range_m: f64) -> Track {
        Track {
            track_id: "trk-1".into(),
            target_name: String::new(),
            classification: "aircraft".into(),
            affiliation: Affiliation::Red,
            iff: "foe".into(),
            position_lla: None,
            heading_deg: None,
            speed_ms: None,
            range_m: Some(range_m),
            bearing_deg: None,
            elevation_deg: None,
            quality: 0.9,
            stale: false,
            last_update_s: 0.0,
            is_active: true,
        }
    }

    #[test]
    fn recon_is_emcon_restricted_and_safes_weapons() {
        let posture = posture_for(CcaRole::Recon);
        assert_eq!(posture.emcon, EmconLevel::Restricted);
        assert!(!posture.radar_active);
        assert!(posture.weapons_safed);
    }

    #[test]
    fn striker_keeps_weapons_available() {
        let posture = posture_for(CcaRole::Striker);
        assert!(!posture.weapons_safed);
        assert!(posture.radar_active);
    }

    #[test]
    fn recon_posture_leaves_sensors_to_sms_and_stops_jammer_and_safes() {
        let state = base_state("cca-1");
        let cmds = posture_commands(&state, &posture_for(CcaRole::Recon));
        assert!(!cmds.iter().any(|c| matches!(
            c,
            PlatformCommand::SensorOn { .. }
                | PlatformCommand::SensorOff { .. }
                | PlatformCommand::SensorSetMode { .. }
        )));
        assert!(cmds
            .iter()
            .any(|c| matches!(c, PlatformCommand::JamStop { .. })));
        assert!(cmds
            .iter()
            .any(|c| matches!(c, PlatformCommand::WeaponSafeAll { .. })));
        // Never a fire command.
        assert!(!cmds
            .iter()
            .any(|c| matches!(c, PlatformCommand::FireAtTarget { .. })));
    }

    #[test]
    fn ew_jamming_keeps_jammer_up() {
        let state = base_state("cca-1");
        let cmds = posture_commands(&state, &posture_for(CcaRole::EwJamming));
        assert!(!cmds
            .iter()
            .any(|c| matches!(c, PlatformCommand::JamStop { .. })));
    }

    #[test]
    fn adaptive_picks_intercept_when_threat_close() {
        let mut state = base_state("cca-1");
        state.tracks.push(hostile_track(40_000.0));
        assert_eq!(adapt_role(&state), CcaRole::Intercept);
    }

    #[test]
    fn adaptive_picks_escort_when_commander_present() {
        let mut state = base_state("cca-1");
        state.commander = Some("mothership-1".into());
        assert_eq!(adapt_role(&state), CcaRole::Escort);
    }

    #[test]
    fn adaptive_defaults_to_patrol() {
        let state = base_state("cca-1");
        assert_eq!(adapt_role(&state), CcaRole::Patrol);
    }

    #[test]
    fn controller_assign_and_adaptive_posture() {
        let ctrl = CcaRoleController::new("cca-1", CcaRole::Adaptive);
        let mut state = base_state("cca-1");
        state.tracks.push(hostile_track(30_000.0));
        // Adaptive resolves to Intercept ⇒ radar active.
        assert!(ctrl.posture(&state).radar_active);
        ctrl.assign(CcaRole::Recon);
        assert_eq!(ctrl.current(), CcaRole::Recon);
        assert!(!ctrl.posture(&state).radar_active);
    }
}
