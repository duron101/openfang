//! FleetManager — mothership-side fleet orchestration (Track 2).
//!
//! Maintains the [`FleetSnapshot`] of child UAVs and turns fleet-level health
//! events into concrete [`PlatformCommand`]s:
//! - a child with critically low fuel or lost link is recalled (`ReturnToBase`);
//! - a child **lost** mid-mission has its mission **re-assigned** to an
//!   available sibling so the tasking is not dropped.
//!
//! Like every other autonomy layer, the FleetManager only ever *proposes*
//! commands; they still flow through the kernel CommandGate. It never fires
//! weapons and never bypasses safety arbitration.

use std::sync::{Arc, Mutex};

use openfang_types::platform::{
    CcaRole, FleetSnapshot, PlatformCommand, UavMission, UavState, UavStatus,
};

/// Default fuel reserve below which a child is recalled (fraction 0..1).
pub const DEFAULT_MIN_FUEL_PCT: f64 = 0.15;

/// A fleet-level decision, paired with a human-readable reason for audit.
#[derive(Debug, Clone)]
pub struct FleetAction {
    pub command: PlatformCommand,
    pub reason: String,
}

// ─────────────────────────────────────────────
// Federation: lead-side role allocation + member degradation
// ─────────────────────────────────────────────

/// Subsystem capability of a formation member, inferred from its platform type.
/// Federated role allocation must never assign a role a member cannot perform
/// (e.g. an unarmed LSUAV can never be `EwJamming` or `Striker`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemberCapability {
    pub can_jam: bool,
    pub can_strike: bool,
    pub can_relay: bool,
}

impl MemberCapability {
    /// Infer capability from a UAV platform type string. Matches the bundled
    /// agent profiles: `cca` is armed + jammer; `lsuav` is unarmed ISR/relay.
    pub fn from_uav_type(uav_type: &str) -> Self {
        match uav_type.to_ascii_lowercase().as_str() {
            "cca" => Self {
                can_jam: true,
                can_strike: true,
                can_relay: true,
            },
            // Conservative default for lsuav / unknown: ISR & relay only.
            _ => Self {
                can_jam: false,
                can_strike: false,
                can_relay: true,
            },
        }
    }
}

/// A federated role assignment the lead distributes to a member instance over
/// OFP/A2A. The member adopts `role` via its own brain (`set_own_role`) and
/// drives its own cerebellum lanes — the contract is identical to the in-process
/// brain→cerebellum path.
#[derive(Debug, Clone, PartialEq)]
pub struct RoleAssignment {
    pub member_id: String,
    pub role: CcaRole,
    pub reason: String,
}

/// Allocate member roles for a fired formation workflow, capability-gated.
/// Members that cannot perform the primary role are given a supporting role
/// (relay/recon) rather than left idle. Pure function — easy to unit test.
pub fn allocate_roles(
    workflow: &str,
    members: &[(String, MemberCapability)],
) -> Vec<RoleAssignment> {
    let mk = |id: &str, role: CcaRole, why: &str| RoleAssignment {
        member_id: id.to_string(),
        role,
        reason: format!("{workflow}: {why}"),
    };
    match workflow {
        "SEAD" | "ElectronicAttack" => {
            let mut jammer_assigned = false;
            let mut protector_assigned = false;
            members
                .iter()
                .map(|(id, cap)| {
                    if cap.can_jam && !jammer_assigned {
                        jammer_assigned = true;
                        mk(id, CcaRole::EwJamming, "primary jammer")
                    } else if cap.can_jam && !protector_assigned {
                        protector_assigned = true;
                        mk(id, CcaRole::EwProtection, "defensive EW")
                    } else if cap.can_relay {
                        mk(id, CcaRole::Relay, "ISR/relay support")
                    } else {
                        mk(id, CcaRole::Recon, "recon support")
                    }
                })
                .collect()
        }
        "Decoy" => {
            let mut decoy_assigned = false;
            members
                .iter()
                .map(|(id, cap)| {
                    if !decoy_assigned {
                        decoy_assigned = true;
                        mk(id, CcaRole::Decoy, "decoy")
                    } else if cap.can_relay {
                        mk(id, CcaRole::Relay, "relay")
                    } else {
                        mk(id, CcaRole::Recon, "recon")
                    }
                })
                .collect()
        }
        // Recon-to-strike: a recon/ISR member opens the picture and designates;
        // the first strike-capable member prosecutes. Capability-gated so an
        // unarmed member is never given the striker role.
        "ReconToStrike" => {
            let mut recon_assigned = false;
            let mut strike_assigned = false;
            members
                .iter()
                .map(|(id, cap)| {
                    if !recon_assigned {
                        recon_assigned = true;
                        mk(id, CcaRole::Recon, "recon/designate")
                    } else if cap.can_strike && !strike_assigned {
                        strike_assigned = true;
                        mk(id, CcaRole::Striker, "strike")
                    } else if cap.can_relay {
                        mk(id, CcaRole::Relay, "relay support")
                    } else {
                        mk(id, CcaRole::Recon, "recon support")
                    }
                })
                .collect()
        }
        // Coordinated strike: one coordinator (designator) sequences time-on-
        // target; remaining strike-capable members are strikers, others relay.
        "CoordinatedStrike" => {
            let mut coordinator_assigned = false;
            members
                .iter()
                .map(|(id, cap)| {
                    if !coordinator_assigned {
                        coordinator_assigned = true;
                        mk(id, CcaRole::Designator, "strike coordinator")
                    } else if cap.can_strike {
                        mk(id, CcaRole::Striker, "strike")
                    } else if cap.can_relay {
                        mk(id, CcaRole::Relay, "relay support")
                    } else {
                        mk(id, CcaRole::Recon, "recon support")
                    }
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Mission type string for a formation workflow's `AssignMission` command.
pub fn workflow_mission_type(workflow: &str) -> String {
    match workflow {
        "SEAD" => "sead",
        "ElectronicAttack" => "electronic_attack",
        "Decoy" => "decoy",
        "ReconToStrike" => "recon_to_strike",
        "CoordinatedStrike" => "coordinated_strike",
        _ => "patrol",
    }
    .to_string()
}

/// Member-side self-degradation: the role a member adopts when it loses the link
/// to the lead. Low-emission, weapons-safe; jammer-capable members keep
/// defensive EW up. No single point of failure — the member never goes idle.
pub fn degraded_role(cap: MemberCapability) -> CcaRole {
    if cap.can_jam {
        CcaRole::EwProtection
    } else {
        CcaRole::Recon
    }
}

/// Mothership fleet orchestrator.
pub struct FleetManager {
    fleet: Arc<Mutex<FleetSnapshot>>,
    min_fuel_pct: f64,
}

impl FleetManager {
    pub fn new(mothership_id: impl Into<String>) -> Self {
        Self {
            fleet: Arc::new(Mutex::new(FleetSnapshot::new(mothership_id))),
            min_fuel_pct: DEFAULT_MIN_FUEL_PCT,
        }
    }

    pub fn with_min_fuel_pct(mut self, pct: f64) -> Self {
        self.min_fuel_pct = pct;
        self
    }

    /// Replace the entire fleet picture (e.g. from a fresh `WorldSnapshot.fleet`).
    pub fn ingest(&self, snapshot: FleetSnapshot) {
        *self.fleet.lock().unwrap() = snapshot;
    }

    /// Insert or update a single child's state.
    pub fn upsert_uav(&self, uav: UavState) {
        let mut fleet = self.fleet.lock().unwrap();
        match fleet.uavs.iter_mut().find(|u| u.uav_id == uav.uav_id) {
            Some(existing) => *existing = uav,
            None => fleet.uavs.push(uav),
        }
    }

    /// Current fleet picture (clone).
    pub fn snapshot(&self) -> FleetSnapshot {
        self.fleet.lock().unwrap().clone()
    }

    pub fn uav_count(&self) -> usize {
        self.fleet.lock().unwrap().uavs.len()
    }

    /// Assign a mission to a specific child, returning the command to dispatch.
    /// Returns `None` if the child is unknown.
    pub fn assign_mission(&self, uav_id: &str, mission: UavMission) -> Option<PlatformCommand> {
        let mut fleet = self.fleet.lock().unwrap();
        let uav = fleet.uavs.iter_mut().find(|u| u.uav_id == uav_id)?;
        let cmd = PlatformCommand::AssignMission {
            uav_id: uav_id.to_string(),
            mission_type: mission.mission_type.clone(),
            params_json: mission.params_json.clone(),
        };
        uav.status = UavStatus::OnMission;
        uav.mission = Some(mission);
        Some(cmd)
    }

    /// Member-side acknowledgement that a mission assignment was received.
    /// Returns false when the member or mission does not match the current
    /// fleet picture, allowing callers to retry or degrade.
    pub fn acknowledge_mission(&self, uav_id: &str, mission_id: &str) -> bool {
        let mut fleet = self.fleet.lock().unwrap();
        let Some(uav) = fleet.uavs.iter_mut().find(|u| u.uav_id == uav_id) else {
            return false;
        };
        let matches_current = uav
            .mission
            .as_ref()
            .map(|mission| mission.mission_id == mission_id)
            .unwrap_or(false);
        if !matches_current {
            return false;
        }
        uav.status = UavStatus::OnMission;
        uav.seconds_since_contact = 0.0;
        true
    }

    /// Lead-side federation: allocate member roles for a fired formation
    /// workflow, gated by each available member's inferred capability, and stamp
    /// the assigned role onto the member's mission. Returns the assignments to
    /// distribute to member instances (over OFP/A2A) and audit.
    pub fn allocate_formation_roles(&self, workflow: &str) -> Vec<RoleAssignment> {
        let members: Vec<(String, MemberCapability)> = {
            let fleet = self.fleet.lock().unwrap();
            fleet
                .uavs
                .iter()
                .filter(|u| u.is_available())
                .map(|u| {
                    (
                        u.uav_id.clone(),
                        MemberCapability::from_uav_type(&u.uav_type),
                    )
                })
                .collect()
        };
        let assignments = allocate_roles(workflow, &members);
        // Stamp the role onto each member's mission so the fleet picture reflects
        // the federated tasking.
        for a in &assignments {
            let mission = UavMission {
                mission_id: format!("{workflow}:{}", a.member_id),
                mission_type: workflow_mission_type(workflow),
                role: Some(a.role),
                params_json: "{}".into(),
                target_track_id: None,
            };
            self.assign_mission(&a.member_id, mission);
        }
        assignments
    }

    /// Evaluate fleet health and produce corrective actions:
    /// recall low-fuel / lost-link children, and re-assign missions orphaned by
    /// a lost child to an available sibling.
    pub fn evaluate(&self) -> Vec<FleetAction> {
        let mut actions = Vec::new();
        let mut fleet = self.fleet.lock().unwrap();

        // 1. Recall children that need attention (low fuel or lost link).
        let recalls: Vec<String> = fleet
            .uavs
            .iter()
            .filter(|u| {
                matches!(u.status, UavStatus::Airborne | UavStatus::OnMission)
                    && (!u.is_in_contact() || u.fuel_pct < self.min_fuel_pct)
            })
            .map(|u| u.uav_id.clone())
            .collect();
        for uav_id in recalls {
            let uav = fleet.uavs.iter_mut().find(|u| u.uav_id == uav_id).unwrap();
            let reason = if !uav.is_in_contact() {
                format!(
                    "{uav_id}: link lost ({:.0}s) — recall",
                    uav.seconds_since_contact
                )
            } else {
                format!(
                    "{uav_id}: fuel {:.0}% < reserve — recall",
                    uav.fuel_pct * 100.0
                )
            };
            uav.status = UavStatus::Returning;
            actions.push(FleetAction {
                command: PlatformCommand::ReturnToBase {
                    uav_id: uav_id.clone(),
                },
                reason,
            });
        }

        // 2. Re-assign missions orphaned by a lost child.
        let orphaned: Vec<(String, UavMission)> = fleet
            .uavs
            .iter()
            .filter(|u| u.status == UavStatus::Lost)
            .filter_map(|u| u.mission.clone().map(|m| (u.uav_id.clone(), m)))
            .collect();

        for (lost_id, mission) in orphaned {
            // Find an available sibling (airborne, in contact, no mission).
            let candidate = fleet
                .uavs
                .iter()
                .find(|u| u.is_available() && u.mission.is_none())
                .map(|u| u.uav_id.clone());

            // Clear the lost child's mission so we don't re-trigger.
            if let Some(lost) = fleet.uavs.iter_mut().find(|u| u.uav_id == lost_id) {
                lost.mission = None;
            }

            if let Some(target_id) = candidate {
                let cmd = PlatformCommand::AssignMission {
                    uav_id: target_id.clone(),
                    mission_type: mission.mission_type.clone(),
                    params_json: mission.params_json.clone(),
                };
                if let Some(target) = fleet.uavs.iter_mut().find(|u| u.uav_id == target_id) {
                    target.status = UavStatus::OnMission;
                    target.mission = Some(mission);
                }
                actions.push(FleetAction {
                    command: cmd,
                    reason: format!("{lost_id} lost — mission re-assigned to {target_id}"),
                });
            }
        }

        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uav(id: &str, status: UavStatus, fuel: f64, since_contact: f64) -> UavState {
        UavState {
            uav_id: id.into(),
            uav_type: "cca".into(),
            status,
            fuel_pct: fuel,
            seconds_since_contact: since_contact,
            mission: None,
        }
    }

    fn mission(id: &str) -> UavMission {
        UavMission {
            mission_id: id.into(),
            mission_type: "strike".into(),
            role: None,
            params_json: "{}".into(),
            target_track_id: Some("trk-1".into()),
        }
    }

    #[test]
    fn upsert_and_count() {
        let fm = FleetManager::new("ms-1");
        fm.upsert_uav(uav("u1", UavStatus::Airborne, 0.9, 0.0));
        fm.upsert_uav(uav("u1", UavStatus::OnMission, 0.8, 0.0)); // update, not dup
        fm.upsert_uav(uav("u2", UavStatus::Airborne, 0.5, 0.0));
        assert_eq!(fm.uav_count(), 2);
    }

    #[test]
    fn low_fuel_child_is_recalled() {
        let fm = FleetManager::new("ms-1");
        fm.upsert_uav(uav("u1", UavStatus::OnMission, 0.05, 0.0));
        let actions = fm.evaluate();
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            actions[0].command,
            PlatformCommand::ReturnToBase { .. }
        ));
        assert_eq!(
            fm.snapshot().get("u1").unwrap().status,
            UavStatus::Returning
        );
    }

    #[test]
    fn lost_link_child_is_recalled() {
        let fm = FleetManager::new("ms-1");
        fm.upsert_uav(uav("u1", UavStatus::Airborne, 0.9, 45.0)); // > COMM_LOSS_S
        let actions = fm.evaluate();
        assert_eq!(actions.len(), 1);
        assert!(actions[0].reason.contains("link lost"));
    }

    #[test]
    fn healthy_fleet_yields_no_actions() {
        let fm = FleetManager::new("ms-1");
        fm.upsert_uav(uav("u1", UavStatus::Airborne, 0.9, 1.0));
        fm.upsert_uav(uav("u2", UavStatus::OnMission, 0.7, 2.0));
        assert!(fm.evaluate().is_empty());
    }

    #[test]
    fn lost_child_mission_reassigned_to_available_sibling() {
        let fm = FleetManager::new("ms-1");
        let mut lost = uav("u1", UavStatus::Lost, 0.0, 99.0);
        lost.mission = Some(mission("m-1"));
        fm.upsert_uav(lost);
        fm.upsert_uav(uav("u2", UavStatus::Airborne, 0.9, 1.0)); // available sibling

        let actions = fm.evaluate();
        let reassign = actions
            .iter()
            .find(|a| matches!(a.command, PlatformCommand::AssignMission { .. }))
            .expect("orphaned mission should be re-assigned");
        match &reassign.command {
            PlatformCommand::AssignMission {
                uav_id,
                mission_type,
                ..
            } => {
                assert_eq!(uav_id, "u2");
                assert_eq!(mission_type, "strike");
            }
            _ => unreachable!(),
        }
        // Sibling now carries the mission; lost child no longer does.
        assert_eq!(
            fm.snapshot().get("u2").unwrap().status,
            UavStatus::OnMission
        );
        assert!(fm.snapshot().get("u1").unwrap().mission.is_none());
    }

    #[test]
    fn assign_mission_sets_status_and_emits_command() {
        let fm = FleetManager::new("ms-1");
        fm.upsert_uav(uav("u1", UavStatus::Airborne, 0.9, 0.0));
        let cmd = fm.assign_mission("u1", mission("m-1")).unwrap();
        assert!(matches!(cmd, PlatformCommand::AssignMission { .. }));
        assert_eq!(
            fm.snapshot().get("u1").unwrap().status,
            UavStatus::OnMission
        );
        assert!(fm.assign_mission("ghost", mission("m-2")).is_none());
    }

    #[test]
    fn member_ack_confirms_current_mission_and_refreshes_contact() {
        let fm = FleetManager::new("ms-1");
        let mut member = uav("u1", UavStatus::Airborne, 0.9, 12.0);
        member.mission = Some(mission("m-1"));
        fm.upsert_uav(member);

        assert!(fm.acknowledge_mission("u1", "m-1"));
        let snapshot = fm.snapshot();
        let uav = snapshot.get("u1").unwrap();
        assert_eq!(uav.status, UavStatus::OnMission);
        assert_eq!(uav.seconds_since_contact, 0.0);
        assert!(!fm.acknowledge_mission("u1", "wrong-mission"));
        assert!(!fm.acknowledge_mission("ghost", "m-1"));
    }

    // ── Federation: role allocation + degradation ──

    fn typed_uav(id: &str, uav_type: &str) -> UavState {
        UavState {
            uav_id: id.into(),
            uav_type: uav_type.into(),
            status: UavStatus::Airborne,
            fuel_pct: 0.9,
            seconds_since_contact: 1.0,
            mission: None,
        }
    }

    #[test]
    fn member_capability_inferred_from_type() {
        assert!(MemberCapability::from_uav_type("cca").can_jam);
        assert!(MemberCapability::from_uav_type("cca").can_strike);
        // LSUAV is unarmed ISR/relay.
        let ls = MemberCapability::from_uav_type("lsuav");
        assert!(!ls.can_jam);
        assert!(!ls.can_strike);
        assert!(ls.can_relay);
    }

    #[test]
    fn sead_allocation_never_jams_with_unarmed_member() {
        let members = vec![
            (
                "lsuav-1".to_string(),
                MemberCapability::from_uav_type("lsuav"),
            ),
            ("cca-1".to_string(), MemberCapability::from_uav_type("cca")),
        ];
        let roles = allocate_roles("SEAD", &members);
        let lsuav = roles.iter().find(|r| r.member_id == "lsuav-1").unwrap();
        let cca = roles.iter().find(|r| r.member_id == "cca-1").unwrap();
        // Unarmed LSUAV must NOT be assigned the jammer role.
        assert_ne!(lsuav.role, CcaRole::EwJamming);
        assert_eq!(lsuav.role, CcaRole::Relay);
        // The capable CCA takes the primary jammer role.
        assert_eq!(cca.role, CcaRole::EwJamming);
    }

    #[test]
    fn allocate_formation_roles_stamps_member_missions() {
        let fm = FleetManager::new("lead-1");
        fm.upsert_uav(typed_uav("cca-1", "cca"));
        fm.upsert_uav(typed_uav("lsuav-1", "lsuav"));
        let assignments = fm.allocate_formation_roles("SEAD");
        assert_eq!(assignments.len(), 2);
        // Roles are stamped onto the fleet picture.
        let cca = fm.snapshot().get("cca-1").unwrap().clone();
        assert_eq!(cca.mission.unwrap().role, Some(CcaRole::EwJamming));
    }

    #[test]
    fn unavailable_members_excluded_from_allocation() {
        let fm = FleetManager::new("lead-1");
        let mut lost = typed_uav("cca-1", "cca");
        lost.seconds_since_contact = 99.0; // out of contact
        fm.upsert_uav(lost);
        assert!(fm.allocate_formation_roles("SEAD").is_empty());
    }

    #[test]
    fn recon_to_strike_allocation_gates_striker_on_capability() {
        let members = vec![
            (
                "lsuav-1".to_string(),
                MemberCapability::from_uav_type("lsuav"),
            ),
            ("cca-1".to_string(), MemberCapability::from_uav_type("cca")),
        ];
        let roles = allocate_roles("ReconToStrike", &members);
        let lsuav = roles.iter().find(|r| r.member_id == "lsuav-1").unwrap();
        let cca = roles.iter().find(|r| r.member_id == "cca-1").unwrap();
        // First member opens as recon/designator; the capable CCA strikes.
        assert_eq!(lsuav.role, CcaRole::Recon);
        assert_eq!(cca.role, CcaRole::Striker);
        assert_eq!(workflow_mission_type("ReconToStrike"), "recon_to_strike");
    }

    #[test]
    fn coordinated_strike_assigns_one_coordinator() {
        let members = vec![
            ("cca-1".to_string(), MemberCapability::from_uav_type("cca")),
            ("cca-2".to_string(), MemberCapability::from_uav_type("cca")),
        ];
        let roles = allocate_roles("CoordinatedStrike", &members);
        let coordinators = roles
            .iter()
            .filter(|r| r.role == CcaRole::Designator)
            .count();
        let strikers = roles.iter().filter(|r| r.role == CcaRole::Striker).count();
        assert_eq!(coordinators, 1, "exactly one coordinator");
        assert_eq!(strikers, 1, "remaining capable member strikes");
    }

    #[test]
    fn member_degrades_to_safe_role_on_link_loss() {
        assert_eq!(
            degraded_role(MemberCapability::from_uav_type("cca")),
            CcaRole::EwProtection
        );
        assert_eq!(
            degraded_role(MemberCapability::from_uav_type("lsuav")),
            CcaRole::Recon
        );
    }
}
