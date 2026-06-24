//! ArkSIM 武器打击发令协议 — 对齐 `protobuf/test_fire_at_target.py` 分步序列。

use std::collections::HashSet;

use openfang_types::platform::PlatformCommand;

/// Apply track-id normalization before protobuf encoding.
pub fn normalize_commands(commands: &[PlatformCommand]) -> Vec<PlatformCommand> {
    use PlatformCommand::*;
    commands
        .iter()
        .map(|cmd| match cmd {
            FireAtTarget {
                platform_id,
                weapon_id,
                track_id,
            } => FireAtTarget {
                platform_id: platform_id.clone(),
                weapon_id: weapon_id.clone(),
                track_id: crate::track_id::normalize_track_id(track_id),
            },
            FireSalvo {
                platform_id,
                weapon_id,
                track_id,
                salvo_size,
            } => FireSalvo {
                platform_id: platform_id.clone(),
                weapon_id: weapon_id.clone(),
                track_id: crate::track_id::normalize_track_id(track_id),
                salvo_size: *salvo_size,
            },
            UpdateTarget {
                platform_id,
                track_id,
            } => UpdateTarget {
                platform_id: platform_id.clone(),
                track_id: crate::track_id::normalize_track_id(track_id),
            },
            JamStart {
                platform_id,
                jammer_id,
                frequency_hz,
                bandwidth_hz,
                target_track_id,
            } => JamStart {
                platform_id: platform_id.clone(),
                jammer_id: jammer_id.clone(),
                frequency_hz: *frequency_hz,
                bandwidth_hz: *bandwidth_hz,
                target_track_id: crate::track_id::normalize_track_id(target_track_id),
            },
            other => other.clone(),
        })
        .collect()
}

/// The platform a command acts on *and* which therefore must already be under
/// external control before the command is processed. Returns `None` for
/// commands that don't require (or establish) outside control, such as
/// `ReleaseOutsideControl` and non-ArkSIM UAV-level commands.
///
/// ArkSIM/AFSIM crashes (null deref in `WsfPlatformPartEvent::Execute`) if a
/// platform-part command (sensor, motion, weapon, comm, jam) is delivered for a
/// platform that has NOT first received `E_SetAgentOutsideControl`. The
/// validated walkthrough always issues `set_agent_outside_control` first.
fn controlled_platform(cmd: &PlatformCommand, allow_self: bool) -> Option<&str> {
    use PlatformCommand::*;
    fn concrete_platform_id(platform_id: &str, allow_self: bool) -> Option<&str> {
        // PlatformAuxData.index must match StateMessage.PlatformState.index for the
        // named platform (not 0). Name "self" is valid when the scenario platform
        // is literally named self; aux lookup still requires the correct index.
        if platform_id.is_empty() || (platform_id == "self" && !allow_self) {
            None
        } else {
            Some(platform_id)
        }
    }
    match cmd {
        SetOutsideControl { platform_id }
        | SetHeading { platform_id, .. }
        | SetSpeed { platform_id, .. }
        | SetAltitude { platform_id, .. }
        | GotoLocation { platform_id, .. }
        | FollowRoute { platform_id, .. }
        | SensorOn { platform_id, .. }
        | SensorOff { platform_id, .. }
        | SensorSetMode { platform_id, .. }
        | UpdateTarget { platform_id, .. }
        | FireAtTarget { platform_id, .. }
        | FireSalvo { platform_id, .. }
        | FireChaff { platform_id, .. }
        | JamStart { platform_id, .. }
        | JamSetMode { platform_id, .. }
        | JamStop { platform_id, .. }
        | WeaponSafeAll { platform_id } => concrete_platform_id(platform_id.as_str(), allow_self),
        // CommOn/CommOff and AuxCommand are NOT forwarded to ArkSIM (see
        // command_mapper) — comms lack a component id, and aux carries
        // OpenFang-internal names AFSIM can't resolve — so they neither require
        // nor establish outside control here.
        _ => None,
    }
}

/// Split commands into per-step batches in the Python-verified order:
/// 1. `SetOutsideControl` for every platform a control command targets (each its
///    own step, emitted FIRST so the platform is under external control before
///    any part command), then
/// 2. one batch of all non-weapon control commands, then
/// 3. each weapon strike (`FireAtTarget`/`FireSalvo`) in its own step.
///
/// This guarantees outside control is established before sensor/motion/weapon
/// commands — required or AFSIM null-derefs in `WsfPlatformPartEvent::Execute`.
pub fn partition_strike_batches(
    commands: &[PlatformCommand],
    auto_outside_control_self: bool,
) -> Vec<Vec<PlatformCommand>> {
    let mut batches = Vec::new();
    let mut outside_sent: HashSet<String> = HashSet::new();

    // Pass 1: establish external control first for every controlled platform.
    // `self` is allowed only when the caller opted in. `command_sanitize` still
    // verifies the latest StateMessage contains a real platform named `self`
    // before this reaches the wire, so scenario renames fail closed.
    for cmd in commands {
        if let Some(platform_id) = controlled_platform(cmd, auto_outside_control_self) {
            if outside_sent.insert(platform_id.to_string()) {
                batches.push(vec![PlatformCommand::SetOutsideControl {
                    platform_id: platform_id.to_string(),
                }]);
            }
        }
    }

    // Pass 2: non-weapon control commands grouped, weapons each in their own step.
    let mut pending_non_weapon = Vec::new();
    for cmd in commands {
        match cmd {
            // Already emitted in pass 1 (deduplicated) — don't duplicate.
            PlatformCommand::SetOutsideControl { .. } => {}
            PlatformCommand::FireAtTarget { .. } | PlatformCommand::FireSalvo { .. } => {
                if !pending_non_weapon.is_empty() {
                    batches.push(std::mem::take(&mut pending_non_weapon));
                }
                batches.push(vec![cmd.clone()]);
            }
            _ => pending_non_weapon.push(cmd.clone()),
        }
    }

    if !pending_non_weapon.is_empty() {
        batches.push(pending_non_weapon);
    }
    batches
}

pub fn batch_has_weapon(batch: &[PlatformCommand]) -> bool {
    batch.iter().any(|cmd| {
        matches!(
            cmd,
            PlatformCommand::FireAtTarget { .. } | PlatformCommand::FireSalvo { .. }
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weapon_gets_prefixed_outside_control_in_separate_batches() {
        let cmds = vec![PlatformCommand::FireAtTarget {
            platform_id: "red_usv_1".into(),
            weapon_id: "loiter_wave2".into(),
            track_id: "self.1".into(),
        }];
        let normalized = normalize_commands(&cmds);
        let batches = partition_strike_batches(&normalized, false);
        assert_eq!(batches.len(), 2);
        assert!(matches!(
            batches[0][0],
            PlatformCommand::SetOutsideControl { .. }
        ));
        assert!(matches!(
            batches[1][0],
            PlatformCommand::FireAtTarget { .. }
        ));
        if let PlatformCommand::FireAtTarget { track_id, .. } = &batches[1][0] {
            assert_eq!(track_id, "self:1");
        }
    }

    #[test]
    fn self_alias_does_not_emit_outside_control_preamble() {
        // Operators can disable self outside-control preambles for scenarios where
        // the ownship has been renamed and `self` is only an OpenFang alias.
        let cmds = vec![PlatformCommand::FireAtTarget {
            platform_id: "self".into(),
            weapon_id: "loiter_wave2".into(),
            track_id: "self.1".into(),
        }];
        let batches = partition_strike_batches(&normalize_commands(&cmds), false);
        assert_eq!(batches.len(), 1);
        assert!(matches!(
            batches[0][0],
            PlatformCommand::FireAtTarget { .. }
        ));
    }

    #[test]
    fn self_alias_auto_emits_outside_control_when_configured() {
        let cmds = vec![PlatformCommand::FireAtTarget {
            platform_id: "self".into(),
            weapon_id: "loiter_wave2".into(),
            track_id: "self.1".into(),
        }];
        let batches = partition_strike_batches(&normalize_commands(&cmds), true);
        assert_eq!(batches.len(), 2);
        assert!(matches!(
            batches[0][0],
            PlatformCommand::SetOutsideControl { .. }
        ));
        assert!(matches!(
            batches[1][0],
            PlatformCommand::FireAtTarget { .. }
        ));
    }

    #[test]
    fn non_weapon_commands_for_concrete_platform_are_prefixed_with_outside_control() {
        // A sensor/motion-only batch (the very first thing the DCC issues) MUST
        // still be preceded by SetOutsideControl, or AFSIM null-derefs.
        let cmds = vec![
            PlatformCommand::SensorOn {
                platform_id: "red_usv_1".into(),
                sensor_id: String::new(),
            },
            PlatformCommand::SetSpeed {
                platform_id: "red_usv_1".into(),
                speed_ms: 12.0,
                acceleration_ms2: None,
            },
        ];
        let batches = partition_strike_batches(&normalize_commands(&cmds), false);
        assert_eq!(
            batches.len(),
            2,
            "expect [SetOutsideControl] then [sensor+motion]"
        );
        assert!(matches!(
            batches[0][0],
            PlatformCommand::SetOutsideControl { .. }
        ));
        assert_eq!(batches[0].len(), 1);
        assert!(matches!(batches[1][0], PlatformCommand::SensorOn { .. }));
        assert!(matches!(batches[1][1], PlatformCommand::SetSpeed { .. }));
    }

    #[test]
    fn existing_outside_control_not_duplicated() {
        let cmds = vec![
            PlatformCommand::SetOutsideControl {
                platform_id: "red_usv_1".into(),
            },
            PlatformCommand::FireAtTarget {
                platform_id: "red_usv_1".into(),
                weapon_id: "loiter_wave2".into(),
                track_id: "self:1".into(),
            },
        ];
        let batches = partition_strike_batches(&normalize_commands(&cmds), false);
        assert_eq!(batches.len(), 2);
        assert!(matches!(
            batches[0][0],
            PlatformCommand::SetOutsideControl { .. }
        ));
        assert_eq!(batches[0].len(), 1);
        assert!(matches!(
            batches[1][0],
            PlatformCommand::FireAtTarget { .. }
        ));
    }
}
