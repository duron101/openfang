//! Snapshot-backed validation before ArkSIM wire encode.
//!
//! AFSIM/Warlock null-derefs in `WsfPlatformPartEvent::Execute` when a weapon
//! or sensor command targets a component id that does not exist on the platform,
//! or when the track id is not held in the firing platform's track list.
//! Upstream planners may emit stale ids (e.g. `loiter_wave3` while the scenario
//! carries `loiter_wave2`); this module is the last fail-closed gate.

use openfang_types::mission_dsl::is_recon_uav_weapon_id;
use openfang_types::platform::{Affiliation, PlatformCommand, PlatformState, WorldSnapshot};

use crate::track_id::normalize_track_id;

#[derive(Debug, Default)]
pub struct SanitizeOutcome {
    pub commands: Vec<PlatformCommand>,
    pub dropped: Vec<String>,
}

/// Validate and rewrite platform commands against the latest snapshot.
///
/// Weapon fires with no resolvable weapon/track are **dropped** rather than
/// sent to the simulator. Non-weapon commands pass through unchanged.
pub fn sanitize_commands(
    commands: &[PlatformCommand],
    snapshot: Option<&WorldSnapshot>,
    own_platform_id: &str,
) -> SanitizeOutcome {
    let Some(snapshot) = snapshot else {
        let mut dropped = Vec::new();
        let mut kept = Vec::new();
        for cmd in commands {
            if is_weapon_strike(cmd) {
                dropped.push(format!(
                    "no snapshot yet — dropped {:?}",
                    cmd.command_class()
                ));
            } else if should_drop_outside_control_self(cmd) {
                dropped.push(
                    "SetOutsideControl(self): no snapshot — dropped (Warlock aux null path)"
                        .to_string(),
                );
            } else if should_drop_empty_sensor(cmd) {
                dropped.push(format!(
                    "{:?}: no snapshot — empty sensor_id dropped",
                    cmd.command_class()
                ));
            } else if is_sensor_part_command(cmd) {
                dropped.push(format!(
                    "{:?}: no snapshot — sensor component cannot be validated",
                    cmd.command_class()
                ));
            } else {
                kept.push(cmd.clone());
            }
        }
        return SanitizeOutcome {
            commands: kept,
            dropped,
        };
    };

    let mut out = SanitizeOutcome::default();
    for cmd in commands {
        match cmd {
            PlatformCommand::FireAtTarget {
                platform_id,
                weapon_id,
                track_id,
            } => {
                // ISR / reconnaissance-UAV releases ride the FireAtTarget wire op
                // but are NOT munition strikes: a scout-UAV slot is a launcher,
                // not an expendable round, and you deploy a scout precisely to go
                // *find* a target — so it must not be fail-closed on the kinetic
                // "ready round + held foe track" rules.
                let fixed = if is_recon_uav_weapon_id(weapon_id) {
                    sanitize_isr_release(
                        snapshot,
                        own_platform_id,
                        platform_id,
                        weapon_id,
                        track_id,
                    )
                } else {
                    sanitize_weapon_strike(
                        snapshot,
                        own_platform_id,
                        platform_id,
                        weapon_id,
                        track_id,
                    )
                };
                if let Some(fixed) = fixed {
                    out.commands.push(fixed);
                } else {
                    out.dropped.push(format!(
                        "FireAtTarget {platform_id}/{weapon_id}->{track_id}: no valid weapon/track in snapshot"
                    ));
                }
            }
            PlatformCommand::FireSalvo {
                platform_id,
                weapon_id,
                track_id,
                salvo_size,
            } => {
                let fixed = if is_recon_uav_weapon_id(weapon_id) {
                    sanitize_isr_release(
                        snapshot,
                        own_platform_id,
                        platform_id,
                        weapon_id,
                        track_id,
                    )
                } else {
                    sanitize_weapon_strike(
                        snapshot,
                        own_platform_id,
                        platform_id,
                        weapon_id,
                        track_id,
                    )
                };
                if let Some(fixed) = fixed {
                    if let PlatformCommand::FireAtTarget {
                        platform_id,
                        weapon_id,
                        track_id,
                    } = fixed
                    {
                        if *salvo_size > 1 {
                            out.commands.push(PlatformCommand::FireSalvo {
                                platform_id,
                                weapon_id,
                                track_id,
                                salvo_size: *salvo_size,
                            });
                        } else {
                            out.commands.push(PlatformCommand::FireAtTarget {
                                platform_id,
                                weapon_id,
                                track_id,
                            });
                        }
                    }
                } else {
                    out.dropped.push(format!(
                        "FireSalvo {platform_id}/{weapon_id}->{track_id}: no valid weapon/track in snapshot"
                    ));
                }
            }
            PlatformCommand::SetOutsideControl { platform_id } => {
                if can_send_outside_control(snapshot, platform_id, own_platform_id) {
                    out.commands.push(cmd.clone());
                } else {
                    out.dropped.push(format!(
                        "SetOutsideControl {platform_id}: platform not in snapshot"
                    ));
                }
            }
            PlatformCommand::SensorOn {
                platform_id,
                sensor_id,
            }
            | PlatformCommand::SensorOff {
                platform_id,
                sensor_id,
            }
            | PlatformCommand::SensorSetMode {
                platform_id,
                sensor_id,
                ..
            } => {
                if let Some(fixed) =
                    sanitize_sensor_part(snapshot, own_platform_id, platform_id, sensor_id, cmd)
                {
                    out.commands.push(fixed);
                } else {
                    out.dropped.push(format!(
                        "Sensor part {platform_id}/{sensor_id:?}: no valid sensor in snapshot"
                    ));
                }
            }
            other => out.commands.push(other.clone()),
        }
    }
    out
}

fn should_drop_outside_control_self(cmd: &PlatformCommand) -> bool {
    matches!(
        cmd,
        PlatformCommand::SetOutsideControl { platform_id }
            if platform_id == "self"
    )
}

fn can_send_outside_control(
    snapshot: &WorldSnapshot,
    platform_id: &str,
    own_platform_id: &str,
) -> bool {
    if platform_id.is_empty() {
        return false;
    }
    if platform_id == "self" {
        // `test_warlock_tcp_fire.py` validates SetOutsideControl("self") for
        // scenarios whose real platform name is `self`. Do not rely on the
        // generic single-platform fallback here: if a future scenario renames the
        // ownship, sending `self` through the part-event path can hit Warlock's
        // null-component crash path again.
        return snapshot
            .platforms
            .iter()
            .any(|p| p.id == "self" || p.name == "self");
    }
    find_platform(snapshot, platform_id, own_platform_id).is_some()
}

fn should_drop_empty_sensor(cmd: &PlatformCommand) -> bool {
    match cmd {
        PlatformCommand::SensorOn { sensor_id, .. }
        | PlatformCommand::SensorOff { sensor_id, .. }
        | PlatformCommand::SensorSetMode { sensor_id, .. } => sensor_id.is_empty(),
        _ => false,
    }
}

fn is_weapon_strike(cmd: &PlatformCommand) -> bool {
    matches!(
        cmd,
        PlatformCommand::FireAtTarget { .. } | PlatformCommand::FireSalvo { .. }
    )
}

fn is_sensor_part_command(cmd: &PlatformCommand) -> bool {
    matches!(
        cmd,
        PlatformCommand::SensorOn { .. }
            | PlatformCommand::SensorOff { .. }
            | PlatformCommand::SensorSetMode { .. }
    )
}

fn sanitize_weapon_strike(
    snapshot: &WorldSnapshot,
    own_platform_id: &str,
    platform_id: &str,
    weapon_id: &str,
    track_id: &str,
) -> Option<PlatformCommand> {
    let platform = find_platform(snapshot, platform_id, own_platform_id)?;
    let resolved_weapon = resolve_weapon_id(weapon_id, platform)?;
    let resolved_track = resolve_track_id(snapshot, platform, track_id)?;
    Some(PlatformCommand::FireAtTarget {
        platform_id: platform_id.to_string(),
        weapon_id: resolved_weapon,
        track_id: resolved_track,
    })
}

/// Validate a reconnaissance-UAV release (ISR deploy) for the wire.
///
/// ISR deploys still use `E_FireAtTarget` on the wire, but they are not kinetic
/// strikes: target track resolution is best-effort because a scout may be
/// launched to find a target. The launcher itself must still be a valid,
/// ready component with positive remaining inventory; live `quantityRemaining`
/// is authoritative and manifest-seeded quantity is applied before this layer.
fn sanitize_isr_release(
    snapshot: &WorldSnapshot,
    own_platform_id: &str,
    platform_id: &str,
    weapon_id: &str,
    track_id: &str,
) -> Option<PlatformCommand> {
    let platform = find_platform(snapshot, platform_id, own_platform_id)?;
    let resolved_weapon = resolve_ready_isr_launcher(weapon_id, platform)?;
    let resolved_track = resolve_track_id(snapshot, platform, track_id).unwrap_or_else(|| {
        if track_id.is_empty() {
            track_id.to_string()
        } else {
            normalize_track_id(track_id)
        }
    });
    Some(PlatformCommand::FireAtTarget {
        platform_id: platform_id.to_string(),
        weapon_id: resolved_weapon,
        track_id: resolved_track,
    })
}

/// Resolve the scout/recon-UAV launcher component id against the platform's
/// component tree. Exact id match is preferred, then any ready recon-UAV slot.
fn resolve_ready_isr_launcher(requested: &str, platform: &PlatformState) -> Option<String> {
    if let Some(weapon) = platform
        .onboard_weapons
        .iter()
        .find(|w| w.weapon_id == requested)
    {
        return launcher_is_ready(weapon).then(|| requested.to_string());
    }
    platform
        .onboard_weapons
        .iter()
        .find(|w| is_recon_uav_weapon_id(&w.weapon_id) || is_recon_uav_weapon_id(&w.weapon_type))
        .filter(|w| launcher_is_ready(w))
        .map(|w| w.weapon_id.clone())
}

fn launcher_is_ready(weapon: &openfang_types::platform::WeaponState) -> bool {
    weapon.is_ready && weapon.quantity_remaining > 0.0
}

fn sanitize_sensor_part(
    snapshot: &WorldSnapshot,
    own_platform_id: &str,
    platform_id: &str,
    sensor_id: &str,
    cmd: &PlatformCommand,
) -> Option<PlatformCommand> {
    if sensor_id.is_empty() {
        return None;
    }
    let platform = find_platform(snapshot, platform_id, own_platform_id)?;
    let resolved_sensor = resolve_sensor_id(sensor_id, platform)?;
    match cmd {
        PlatformCommand::SensorOn { platform_id, .. } => Some(PlatformCommand::SensorOn {
            platform_id: platform_id.clone(),
            sensor_id: resolved_sensor,
        }),
        PlatformCommand::SensorOff { platform_id, .. } => Some(PlatformCommand::SensorOff {
            platform_id: platform_id.clone(),
            sensor_id: resolved_sensor,
        }),
        PlatformCommand::SensorSetMode {
            platform_id, mode, ..
        } => Some(PlatformCommand::SensorSetMode {
            platform_id: platform_id.clone(),
            sensor_id: resolved_sensor,
            mode: mode.clone(),
        }),
        _ => None,
    }
}

fn find_platform<'a>(
    snapshot: &'a WorldSnapshot,
    platform_id: &str,
    own_platform_id: &str,
) -> Option<&'a PlatformState> {
    if !platform_id.is_empty() && platform_id != "self" {
        return snapshot
            .platforms
            .iter()
            .find(|p| p.id == platform_id || p.name == platform_id);
    }
    let hint = if own_platform_id.is_empty() {
        "self"
    } else {
        own_platform_id
    };
    snapshot
        .platforms
        .iter()
        .find(|p| p.id == hint || p.name == hint)
        .or_else(|| {
            // Single controllable platform fallback for the self alias.
            if snapshot.platforms.len() == 1 {
                Some(&snapshot.platforms[0])
            } else {
                None
            }
        })
}

fn resolve_weapon_id(requested: &str, platform: &PlatformState) -> Option<String> {
    let ready: Vec<_> = platform
        .onboard_weapons
        .iter()
        .filter(|w| w.is_ready && w.quantity_remaining > 0.0)
        .collect();
    if ready.is_empty() {
        return None;
    }
    if ready.iter().any(|w| w.weapon_id == requested) {
        return Some(requested.to_string());
    }
    let requested_cat = weapon_category(requested);
    if let Some(cat) = requested_cat {
        let mut candidates: Vec<_> = ready
            .iter()
            .filter(|w| {
                weapon_category(&w.weapon_id) == Some(cat)
                    || weapon_category(&w.weapon_type) == Some(cat)
            })
            .collect();
        candidates.sort_by_key(|w| {
            std::cmp::Reverse(recommended_weapon_rank(&w.weapon_id, &w.weapon_type))
        });
        if let Some(w) = candidates.first() {
            return Some(w.weapon_id.clone());
        }
    }
    ready
        .iter()
        .max_by_key(|w| recommended_weapon_rank(&w.weapon_id, &w.weapon_type))
        .map(|w| w.weapon_id.clone())
}

fn resolve_sensor_id(requested: &str, platform: &PlatformState) -> Option<String> {
    let usable: Vec<_> = platform
        .onboard_sensors
        .iter()
        .filter(|s| !s.sensor_id.is_empty() && s.damage < 1.0)
        .collect();
    if usable.is_empty() {
        return None;
    }
    if usable.iter().any(|s| s.sensor_id == requested) {
        return Some(requested.to_string());
    }
    let requested_cat = sensor_category(requested);
    if let Some(cat) = requested_cat {
        let mut candidates: Vec<_> = usable
            .iter()
            .filter(|s| {
                sensor_category(&s.sensor_id) == Some(cat)
                    || sensor_type_category(&s.sensor_type) == Some(cat)
            })
            .collect();
        candidates.sort_by_key(|s| {
            std::cmp::Reverse(recommended_sensor_rank(&s.sensor_id, &s.sensor_type))
        });
        if let Some(s) = candidates.first() {
            return Some(s.sensor_id.clone());
        }
    }
    usable
        .iter()
        .max_by_key(|s| recommended_sensor_rank(&s.sensor_id, &s.sensor_type))
        .map(|s| s.sensor_id.clone())
}

fn resolve_track_id(
    snapshot: &WorldSnapshot,
    platform: &PlatformState,
    raw_id: &str,
) -> Option<String> {
    if raw_id.is_empty() {
        return pick_foe_track(platform)
            .or_else(|| platform.tracks.first().map(|t| t.track_id.clone()));
    }
    let normalized = normalize_track_id(raw_id);
    if platform.tracks.iter().any(|t| t.track_id == normalized) {
        return Some(normalized);
    }
    for track in &platform.tracks {
        if !track.target_name.is_empty()
            && (track.target_name == raw_id || track.target_name == normalized)
        {
            return Some(track.track_id.clone());
        }
    }
    if let Some(resolved) = resolve_track_anywhere(snapshot, raw_id) {
        if platform.tracks.iter().any(|t| t.track_id == resolved) {
            return Some(resolved);
        }
    }
    pick_foe_track(platform).or_else(|| {
        platform
            .tracks
            .iter()
            .find(|t| !t.track_id.is_empty())
            .map(|t| t.track_id.clone())
    })
}

fn resolve_track_anywhere(snapshot: &WorldSnapshot, raw_id: &str) -> Option<String> {
    if raw_id.is_empty() {
        return None;
    }
    let normalized = normalize_track_id(raw_id);
    for platform in &snapshot.platforms {
        for track in &platform.tracks {
            if track.track_id == raw_id || track.track_id == normalized {
                return Some(track.track_id.clone());
            }
        }
    }
    for platform in &snapshot.platforms {
        for track in &platform.tracks {
            if !track.target_name.is_empty() && track.target_name == raw_id {
                return Some(track.track_id.clone());
            }
        }
    }
    None
}

fn pick_foe_track(platform: &PlatformState) -> Option<String> {
    platform
        .tracks
        .iter()
        .find(|t| side_is_foe(&t.affiliation, &t.iff))
        .map(|t| t.track_id.clone())
        .or_else(|| {
            platform
                .tracks
                .iter()
                .find(|t| !t.track_id.is_empty())
                .map(|t| t.track_id.clone())
        })
}

fn side_is_foe(affiliation: &Affiliation, iff: &str) -> bool {
    affiliation.is_hostile() || {
        let i = iff.to_ascii_lowercase();
        i.contains("foe") || i.contains("hostile") || i.contains("enemy")
    }
}

fn weapon_category(value: &str) -> Option<&'static str> {
    let v = value.to_ascii_lowercase();
    if v.contains("gun") || v.contains("cannon") || v.contains('炮') {
        Some("gun")
    } else if v.contains("loiter") || v.contains("巡飞") {
        Some("loiter")
    } else if v.contains("missile") || v.contains("rocket") || v.contains("导弹") {
        Some("missile")
    } else if v.contains("torpedo") || v.contains("鱼雷") {
        Some("torpedo")
    } else {
        None
    }
}

fn sensor_category(value: &str) -> Option<&'static str> {
    let v = value.to_ascii_lowercase();
    if v.contains("radar") || v.contains("雷达") {
        Some("radar")
    } else if v.contains("eoir") || v.contains("eo") || v.contains("ir") || v.contains("光电") {
        Some("eoir")
    } else if v.contains("esm") || v.contains("elint") || v.contains("侦收") {
        Some("esm")
    } else if v.contains("sonar") || v.contains("声纳") {
        Some("sonar")
    } else if v.contains("ais") {
        Some("ais")
    } else {
        None
    }
}

fn sensor_type_category(
    sensor_type: &openfang_types::platform::SensorType,
) -> Option<&'static str> {
    match sensor_type {
        openfang_types::platform::SensorType::Radar => Some("radar"),
        openfang_types::platform::SensorType::EOIR => Some("eoir"),
        openfang_types::platform::SensorType::ESM => Some("esm"),
        openfang_types::platform::SensorType::Sonar => Some("sonar"),
        openfang_types::platform::SensorType::AIS => Some("ais"),
        openfang_types::platform::SensorType::Lidar
        | openfang_types::platform::SensorType::Other => None,
    }
}

fn recommended_sensor_rank(
    sensor_id: &str,
    sensor_type: &openfang_types::platform::SensorType,
) -> u16 {
    let base = match sensor_type {
        openfang_types::platform::SensorType::Radar => 500,
        openfang_types::platform::SensorType::EOIR => 400,
        openfang_types::platform::SensorType::ESM => 300,
        openfang_types::platform::SensorType::Sonar => 250,
        openfang_types::platform::SensorType::AIS => 200,
        openfang_types::platform::SensorType::Lidar => 150,
        openfang_types::platform::SensorType::Other => 0,
    };
    base + trailing_number(sensor_id).unwrap_or(0).min(99)
}

fn recommended_weapon_rank(weapon_id: &str, weapon_type: &str) -> u16 {
    let text = format!("{weapon_id} {weapon_type}").to_ascii_lowercase();
    let base = if text.contains("loiter") || text.contains("munition") || text.contains("mun") {
        500
    } else if text.contains("uav") {
        400
    } else if text.contains("missile") || text.contains("rocket") || text.contains("torpedo") {
        300
    } else if text.contains("gun") || text.contains("bullet") || text.contains("cannon") {
        200
    } else {
        0
    };
    base + trailing_number(weapon_id).unwrap_or(0).min(99)
}

fn trailing_number(text: &str) -> Option<u16> {
    let digits: String = text
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto_manual::{SimPlatform, SimState, SimTrack, SimWeapon};
    use crate::state_mapper::from_sim_state;
    use openfang_types::platform::{SensorState, SensorType};

    fn snapshot_with_self_usv(
        weapons: Vec<(&str, f64)>,
        tracks: Vec<(&str, &str, &str)>,
    ) -> WorldSnapshot {
        let state = SimState {
            time: 1.0,
            end_time: 3600.0,
            platforms: vec![SimPlatform {
                name: "self".into(),
                side: "Red".into(),
                domain: "surface".into(),
                weapons: weapons
                    .into_iter()
                    .map(|(name, qty)| SimWeapon {
                        name: name.into(),
                        weapon_type: name.into(),
                        quantity_remaining: qty,
                        quantity_from_snapshot: true,
                    })
                    .collect(),
                tracks: tracks
                    .into_iter()
                    .map(|(track_id, target_name, side)| SimTrack {
                        track_id: track_id.into(),
                        target_name: target_name.into(),
                        side: side.into(),
                        iff: if side.eq_ignore_ascii_case("blue") {
                            "foe".into()
                        } else {
                            "friend".into()
                        },
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }],
            ..Default::default()
        };
        from_sim_state(&state)
    }

    #[test]
    fn remaps_missing_loiter_wave3_to_onboard_loiter_wave2() {
        let snap = snapshot_with_self_usv(
            vec![("loiter_wave2", 3.0)],
            vec![("self:1", "blue_patrol", "blue")],
        );
        let cmds = vec![PlatformCommand::FireAtTarget {
            platform_id: "self".into(),
            weapon_id: "loiter_wave3".into(),
            track_id: "self:5".into(),
        }];
        let out = sanitize_commands(&cmds, Some(&snap), "self");
        assert!(out.dropped.is_empty(), "{:?}", out.dropped);
        match &out.commands[0] {
            PlatformCommand::FireAtTarget {
                weapon_id,
                track_id,
                ..
            } => {
                assert_eq!(weapon_id, "loiter_wave2");
                assert_eq!(track_id, "self:1");
            }
            other => panic!("expected FireAtTarget, got {other:?}"),
        }
    }

    #[test]
    fn drops_weapon_fire_when_snapshot_missing() {
        let cmds = vec![PlatformCommand::FireAtTarget {
            platform_id: "self".into(),
            weapon_id: "loiter_wave2".into(),
            track_id: "self:1".into(),
        }];
        let out = sanitize_commands(&cmds, None, "self");
        assert!(out.commands.is_empty());
        assert_eq!(out.dropped.len(), 1);
    }

    #[test]
    fn drops_weapon_when_platform_has_no_weapons() {
        let snap = snapshot_with_self_usv(vec![], vec![]);
        let cmds = vec![PlatformCommand::FireAtTarget {
            platform_id: "self".into(),
            weapon_id: "loiter_wave2".into(),
            track_id: "self:1".into(),
        }];
        let out = sanitize_commands(&cmds, Some(&snap), "self");
        assert!(out.commands.is_empty());
        assert_eq!(out.dropped.len(), 1);
    }

    #[test]
    fn motion_commands_pass_without_snapshot() {
        let cmds = vec![PlatformCommand::SetSpeed {
            platform_id: "self".into(),
            speed_ms: 10.0,
            acceleration_ms2: None,
        }];
        let out = sanitize_commands(&cmds, None, "self");
        assert_eq!(out.commands.len(), 1);
        assert!(out.dropped.is_empty());
    }

    #[test]
    fn allows_set_outside_control_self_when_snapshot_has_real_self_platform() {
        let snap = snapshot_with_self_usv(vec![("loiter_wave2", 1.0)], vec![]);
        let cmds = vec![PlatformCommand::SetOutsideControl {
            platform_id: "self".into(),
        }];
        let out = sanitize_commands(&cmds, Some(&snap), "self");
        assert_eq!(out.commands.len(), 1);
        assert!(out.dropped.is_empty());
    }

    #[test]
    fn drops_set_outside_control_self_when_snapshot_lacks_real_self_platform() {
        let mut snap = snapshot_with_self_usv(vec![("loiter_wave2", 1.0)], vec![]);
        snap.platforms[0].id = "red_usv_1".into();
        snap.platforms[0].name = "red_usv_1".into();
        let cmds = vec![PlatformCommand::SetOutsideControl {
            platform_id: "self".into(),
        }];
        let out = sanitize_commands(&cmds, Some(&snap), "red_usv_1");
        assert!(out.commands.is_empty());
        assert_eq!(out.dropped.len(), 1);
    }

    #[test]
    fn drops_empty_sensor_on() {
        let snap = snapshot_with_self_usv(vec![("loiter_wave2", 1.0)], vec![]);
        let cmds = vec![PlatformCommand::SensorOn {
            platform_id: "self".into(),
            sensor_id: String::new(),
        }];
        let out = sanitize_commands(&cmds, Some(&snap), "self");
        assert!(out.commands.is_empty());
        assert_eq!(out.dropped.len(), 1);
    }

    #[test]
    fn drops_sensor_command_when_snapshot_missing() {
        let cmds = vec![PlatformCommand::SensorOn {
            platform_id: "self".into(),
            sensor_id: "surf_radar".into(),
        }];
        let out = sanitize_commands(&cmds, None, "self");
        assert!(out.commands.is_empty());
        assert_eq!(out.dropped.len(), 1);
    }

    #[test]
    fn drops_sensor_command_when_component_tree_missing() {
        let snap = snapshot_with_self_usv(vec![("loiter_wave2", 1.0)], vec![]);
        let cmds = vec![PlatformCommand::SensorOn {
            platform_id: "self".into(),
            sensor_id: "radar".into(),
        }];
        let out = sanitize_commands(&cmds, Some(&snap), "self");
        assert!(out.commands.is_empty());
        assert_eq!(out.dropped.len(), 1);
    }

    #[test]
    fn remaps_sensor_alias_to_onboard_component_tree_id() {
        let mut snap = snapshot_with_self_usv(vec![("loiter_wave2", 1.0)], vec![]);
        snap.platforms[0].onboard_sensors.push(SensorState {
            sensor_id: "surf_radar".into(),
            sensor_type: SensorType::Radar,
            mode: "SEARCH".into(),
            frequency_hz: None,
            bandwidth_hz: None,
            azimuth_fov_deg: None,
            elevation_fov_deg: None,
            range_max_m: None,
            damage: 0.0,
            host_platform_id: "self".into(),
        });
        let cmds = vec![PlatformCommand::SensorSetMode {
            platform_id: "self".into(),
            sensor_id: "radar".into(),
            mode: "TRACK".into(),
        }];
        let out = sanitize_commands(&cmds, Some(&snap), "self");
        assert!(out.dropped.is_empty(), "{:?}", out.dropped);
        match &out.commands[0] {
            PlatformCommand::SensorSetMode {
                sensor_id, mode, ..
            } => {
                assert_eq!(sensor_id, "surf_radar");
                assert_eq!(mode, "TRACK");
            }
            other => panic!("expected SensorSetMode, got {other:?}"),
        }
    }

    #[test]
    fn isr_release_drops_depleted_scout_uav_slot() {
        // The manifest records the real initial inventory and live telemetry is
        // authoritative once it reports quantityRemaining. A zero-quantity scout
        // slot must not be sent to Warlock as a release command.
        let snap = snapshot_with_self_usv(
            vec![("scout_uav_slot", 0.0)],
            vec![("self:1", "blue_command_post", "blue")],
        );
        let cmds = vec![PlatformCommand::FireAtTarget {
            platform_id: "self".into(),
            weapon_id: "scout_uav_slot".into(),
            track_id: "self:1".into(),
        }];
        let out = sanitize_commands(&cmds, Some(&snap), "self");
        assert!(out.commands.is_empty());
        assert_eq!(out.dropped.len(), 1);
    }

    #[test]
    fn isr_release_launches_without_a_held_track() {
        // No tracks held yet — a scout is deployed precisely to find a target.
        let snap = snapshot_with_self_usv(vec![("scout_uav_slot", 2.0)], vec![]);
        let cmds = vec![PlatformCommand::FireAtTarget {
            platform_id: "self".into(),
            weapon_id: "scout_uav_slot".into(),
            track_id: "self:1".into(),
        }];
        let out = sanitize_commands(&cmds, Some(&snap), "self");
        assert!(out.dropped.is_empty(), "{:?}", out.dropped);
        match &out.commands[0] {
            PlatformCommand::FireAtTarget {
                weapon_id,
                track_id,
                ..
            } => {
                assert_eq!(weapon_id, "scout_uav_slot");
                assert_eq!(track_id, "self:1");
            }
            other => panic!("expected FireAtTarget, got {other:?}"),
        }
    }

    #[test]
    fn isr_release_dropped_when_no_launcher_component_present() {
        // Fail-closed safety: if the platform genuinely has no recon-UAV slot,
        // do not send an unknown component id to Warlock.
        let snap = snapshot_with_self_usv(vec![("loiter_wave2", 3.0)], vec![]);
        let cmds = vec![PlatformCommand::FireAtTarget {
            platform_id: "self".into(),
            weapon_id: "scout_uav_slot".into(),
            track_id: "self:1".into(),
        }];
        let out = sanitize_commands(&cmds, Some(&snap), "self");
        assert!(out.commands.is_empty());
        assert_eq!(out.dropped.len(), 1);
    }

    #[test]
    fn resolves_fire_by_target_name_blue_patrol_3() {
        let snap = snapshot_with_self_usv(
            vec![("loiter_wave2", 2.0)],
            vec![("self:3", "blue_patrol_3", "blue")],
        );
        let cmds = vec![PlatformCommand::FireSalvo {
            platform_id: "self".into(),
            weapon_id: "loiter_wave3".into(),
            track_id: "blue_patrol_3".into(),
            salvo_size: 1,
        }];
        let out = sanitize_commands(&cmds, Some(&snap), "self");
        assert!(out.dropped.is_empty(), "{:?}", out.dropped);
        match &out.commands[0] {
            PlatformCommand::FireAtTarget {
                weapon_id,
                track_id,
                ..
            } => {
                assert_eq!(weapon_id, "loiter_wave2");
                assert_eq!(track_id, "self:3");
            }
            other => panic!("expected FireAtTarget, got {other:?}"),
        }
    }
}
