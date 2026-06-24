//! Platform allocation abstraction.
//!
//! A [`PlatformAllocator`] maps a Play's required roles to concrete platforms.
//! The single-platform autonomous phase uses [`SelfPlatformAllocator`], which
//! binds every role to the own platform and serializes multiple roles into
//! ordered *phases* (e.g. recon first, strike later). Multi-platform / federated
//! allocation is reserved (see `fleet_allocator` stub) and not wired this phase.

use openfang_types::platform::{CcaRole, WorldSnapshot};

use crate::play_registry::PlayDef;

/// One role bound to one platform, with a serialization phase (lower = earlier).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleAssignment {
    /// Logical role name from the play (e.g. "recon", "strike").
    pub logical_role: String,
    pub role: CcaRole,
    pub platform_id: String,
    pub phase: u32,
}

/// Inputs to an allocation pass.
#[derive(Debug, Clone)]
pub struct AllocationRequest<'a> {
    pub play: &'a PlayDef,
    pub snapshot: &'a WorldSnapshot,
    pub own_platform_id: &'a str,
    /// Operator-specified platforms (subset of controlled). May be empty.
    pub explicit_platforms: &'a [String],
}

/// Pluggable platform allocation. Implemented by [`SelfPlatformAllocator`] now;
/// a future `FleetAllocator` will distribute roles across peers.
pub trait PlatformAllocator: Send + Sync {
    fn allocate(&self, req: &AllocationRequest<'_>) -> Vec<RoleAssignment>;
}

/// Deterministic ordering of logical roles into execution phases.
fn role_phase(logical_role: &str) -> u32 {
    match logical_role {
        "recon" | "patrol" => 0,
        "coordinator" | "designator" => 1,
        "strike" => 2,
        _ => 3,
    }
}

/// Map a logical role + own capability to a concrete [`CcaRole`]. Downgrades a
/// strike role to recon when the own platform has no ready weapon.
fn concrete_role(logical_role: &str, has_weapon: bool) -> CcaRole {
    match logical_role {
        "recon" => CcaRole::Recon,
        "patrol" => CcaRole::Patrol,
        "coordinator" | "designator" => CcaRole::Designator,
        "strike" => {
            if has_weapon {
                CcaRole::Striker
            } else {
                // Infeasible strike on this platform → degrade to recon.
                CcaRole::Recon
            }
        }
        "ew" | "jamming" => CcaRole::EwJamming,
        "decoy" => CcaRole::Decoy,
        _ => CcaRole::Adaptive,
    }
}

/// Whether the own platform has at least one ready weapon with ammo.
fn own_has_weapon(snapshot: &WorldSnapshot, own_platform_id: &str) -> bool {
    snapshot
        .find_platform(own_platform_id)
        .map(|p| {
            p.onboard_weapons
                .iter()
                .any(|w| w.is_ready && w.quantity_remaining > 0.0)
        })
        .unwrap_or(false)
}

/// Single-platform allocator: binds all roles to the own platform, serialized
/// into phases. Multiple required roles become sequential stages on one body.
#[derive(Debug, Default, Clone)]
pub struct SelfPlatformAllocator;

impl SelfPlatformAllocator {
    pub fn new() -> Self {
        Self
    }
}

impl PlatformAllocator for SelfPlatformAllocator {
    fn allocate(&self, req: &AllocationRequest<'_>) -> Vec<RoleAssignment> {
        // Prefer the first operator-specified platform if provided, else own id.
        let platform_id = req
            .explicit_platforms
            .first()
            .cloned()
            .unwrap_or_else(|| req.own_platform_id.to_string());
        let has_weapon = own_has_weapon(req.snapshot, &platform_id);

        let mut roles: Vec<&String> = req.play.required_roles.keys().collect();
        // Deterministic order: by phase then name.
        roles.sort_by(|a, b| {
            role_phase(a)
                .cmp(&role_phase(b))
                .then_with(|| a.as_str().cmp(b.as_str()))
        });

        if roles.is_empty() {
            // A play with no declared roles still runs on the own platform.
            return vec![RoleAssignment {
                logical_role: "execute".into(),
                role: CcaRole::Adaptive,
                platform_id,
                phase: 0,
            }];
        }

        roles
            .into_iter()
            .map(|logical| RoleAssignment {
                logical_role: logical.clone(),
                role: concrete_role(logical, has_weapon),
                platform_id: platform_id.clone(),
                phase: role_phase(logical),
            })
            .collect()
    }
}

/// Reserved multi-platform / federated allocator (next-step key work).
///
/// This is an interface placeholder for distributing a play's roles across
/// formation members (lead → member over OFP/A2A). It is intentionally **not**
/// wired into the compile path this phase: when the fleet picture is empty or
/// federation is disabled it degrades to single-platform behaviour by delegating
/// to [`SelfPlatformAllocator`], so callers can adopt it without behavioural
/// change. Cross-node dispatch (member `PlatformControlLoop` tasking) is future
/// work; see [`crate::fleet_manager::allocate_roles`] for the role-mapping
/// contract this will build on.
#[derive(Debug, Default, Clone)]
pub struct FleetAllocator {
    /// Available formation member ids (id, is strike-capable). Empty → degrade
    /// to the own platform.
    pub members: Vec<(String, bool)>,
}

impl FleetAllocator {
    pub fn new(members: Vec<(String, bool)>) -> Self {
        Self { members }
    }
}

impl PlatformAllocator for FleetAllocator {
    fn allocate(&self, req: &AllocationRequest<'_>) -> Vec<RoleAssignment> {
        // No federation members available → behave as single-platform.
        if self.members.is_empty() {
            return SelfPlatformAllocator.allocate(req);
        }

        let mut roles: Vec<&String> = req.play.required_roles.keys().collect();
        roles.sort_by(|a, b| {
            role_phase(a)
                .cmp(&role_phase(b))
                .then_with(|| a.as_str().cmp(b.as_str()))
        });
        if roles.is_empty() {
            return SelfPlatformAllocator.allocate(req);
        }

        // Round-robin members onto roles, gating strike on capability. This is a
        // deterministic stub for the reserved interface — not yet dispatched.
        let mut idx = 0usize;
        roles
            .into_iter()
            .map(|logical| {
                let needs_strike = logical == "strike";
                // Find next member that satisfies the capability requirement.
                let mut chosen = req.own_platform_id.to_string();
                for _ in 0..self.members.len() {
                    let (id, can_strike) = &self.members[idx % self.members.len()];
                    idx += 1;
                    if !needs_strike || *can_strike {
                        chosen = id.clone();
                        break;
                    }
                }
                let has_weapon =
                    needs_strike && self.members.iter().any(|(id, cs)| *id == chosen && *cs);
                RoleAssignment {
                    logical_role: logical.clone(),
                    role: concrete_role(logical, has_weapon),
                    platform_id: chosen,
                    phase: role_phase(logical),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openfang_types::platform::{PlatformState, WeaponState};
    use std::collections::HashMap;

    fn play(roles: &[(&str, &str)]) -> PlayDef {
        PlayDef {
            name: "TestPlay".into(),
            preconditions: vec![],
            effect_model: HashMap::new(),
            risk_model: HashMap::new(),
            expected_roi: 0.5,
            functions: vec![],
            required_roles: roles
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            steps: vec![],
        }
    }

    fn snapshot_with_weapon(own: &str, armed: bool) -> WorldSnapshot {
        let mut p = PlatformState::minimal(own);
        if armed {
            p.onboard_weapons = vec![WeaponState {
                weapon_id: "w1".into(),
                weapon_type: "missile".into(),
                quantity_remaining: 2.0,
                max_range_m: Some(10_000.0),
                min_range_m: Some(0.0),
                guidance_type: None,
                speed_ms: None,
                is_ready: true,
                quantity_from_snapshot: true,
            }];
        }
        WorldSnapshot {
            timestamp: 0.0,
            platforms: vec![p],
            active_munitions: vec![],
            events: vec![],
            fleet: None,
        }
    }

    #[test]
    fn binds_all_roles_to_own_platform_with_phases() {
        let play = play(&[("recon", "lsuav"), ("strike", "cca")]);
        let snap = snapshot_with_weapon("self", true);
        let assignments = SelfPlatformAllocator.allocate(&AllocationRequest {
            play: &play,
            snapshot: &snap,
            own_platform_id: "self",
            explicit_platforms: &[],
        });
        assert_eq!(assignments.len(), 2);
        // recon comes before strike.
        assert_eq!(assignments[0].logical_role, "recon");
        assert_eq!(assignments[0].role, CcaRole::Recon);
        assert_eq!(assignments[0].phase, 0);
        assert_eq!(assignments[1].logical_role, "strike");
        assert_eq!(assignments[1].role, CcaRole::Striker);
        assert!(assignments[1].phase > assignments[0].phase);
        assert!(assignments.iter().all(|a| a.platform_id == "self"));
    }

    #[test]
    fn strike_downgrades_to_recon_without_weapon() {
        let play = play(&[("strike", "cca")]);
        let snap = snapshot_with_weapon("self", false);
        let assignments = SelfPlatformAllocator.allocate(&AllocationRequest {
            play: &play,
            snapshot: &snap,
            own_platform_id: "self",
            explicit_platforms: &[],
        });
        assert_eq!(assignments[0].role, CcaRole::Recon);
    }

    #[test]
    fn fleet_allocator_empty_degrades_to_self() {
        let play = play(&[("recon", "lsuav"), ("strike", "cca")]);
        let snap = snapshot_with_weapon("self", true);
        let assignments = FleetAllocator::default().allocate(&AllocationRequest {
            play: &play,
            snapshot: &snap,
            own_platform_id: "self",
            explicit_platforms: &[],
        });
        // Degrades to single-platform: all roles on own id.
        assert!(assignments.iter().all(|a| a.platform_id == "self"));
        assert_eq!(assignments.len(), 2);
    }

    #[test]
    fn fleet_allocator_routes_strike_to_capable_member() {
        let play = play(&[("recon", "lsuav"), ("strike", "cca")]);
        let snap = snapshot_with_weapon("self", true);
        let alloc = FleetAllocator::new(vec![
            ("lsuav-1".to_string(), false),
            ("cca-1".to_string(), true),
        ]);
        let assignments = alloc.allocate(&AllocationRequest {
            play: &play,
            snapshot: &snap,
            own_platform_id: "self",
            explicit_platforms: &[],
        });
        let strike = assignments
            .iter()
            .find(|a| a.logical_role == "strike")
            .unwrap();
        // Strike must be routed to the strike-capable member and stay Striker.
        assert_eq!(strike.platform_id, "cca-1");
        assert_eq!(strike.role, CcaRole::Striker);
    }

    #[test]
    fn uses_explicit_platform_when_provided() {
        let play = play(&[("recon", "any")]);
        let snap = snapshot_with_weapon("uav-2", false);
        let assignments = SelfPlatformAllocator.allocate(&AllocationRequest {
            play: &play,
            snapshot: &snap,
            own_platform_id: "self",
            explicit_platforms: &["uav-2".to_string()],
        });
        assert_eq!(assignments[0].platform_id, "uav-2");
    }
}
