//! Persistent audit log of every command dispatched to ArkSIM.
//!
//! Default path: `{OPENFANG_ROOT}/log/arksim_cmd.log`, or `log/arksim_cmd.log`
//! relative to the process current working directory when `OPENFANG_ROOT` is unset.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use openfang_types::platform::PlatformCommand;

static WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Resolved log file path.
pub fn log_path() -> PathBuf {
    default_log_path()
}

fn default_log_path() -> PathBuf {
    if let Ok(root) = std::env::var("OPENFANG_ROOT") {
        return PathBuf::from(root).join("log").join("arksim_cmd.log");
    }
    PathBuf::from("log").join("arksim_cmd.log")
}

fn timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}", dur.as_secs(), dur.subsec_millis())
}

fn append_line(line: &str) {
    let _guard = WRITE_LOCK.get_or_init(|| Mutex::new(())).lock();
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{line}");
        let _ = file.flush();
    }
}

/// One logical command was dropped before encode (snapshot validation).
pub fn log_drop(reason: &str) {
    append_line(&format!("{} DROP {reason}", timestamp()));
}

/// A batch is about to be encoded and sent/enqueued to the simulator backend.
pub fn log_dispatch(transport: &str, batch_index: usize, cmds: &[PlatformCommand], proto: &[u8]) {
    let summary: Vec<String> = cmds.iter().map(describe_command).collect();
    let hex = hex::encode(proto);
    append_line(&format!(
        "{} DISPATCH transport={transport} batch={batch_index} cmds=[{}] proto_len={} proto_hex={hex}",
        timestamp(),
        summary.join("; "),
        proto.len(),
    ));
}

/// Warlock driver actually sent bytes on the ZMQ socket (after de-dup).
/// Empty keep-alive steps are omitted to avoid flooding the log.
pub fn log_wire(label: &str, payload: &[u8]) {
    if payload.is_empty() {
        return;
    }
    append_line(&format!(
        "{} WIRE {label} len={} hex={}",
        timestamp(),
        payload.len(),
        hex::encode(payload),
    ));
}

/// Human-readable one-liner for a [`PlatformCommand`].
pub fn describe_command(cmd: &PlatformCommand) -> String {
    use PlatformCommand::*;
    match cmd {
        SetOutsideControl { platform_id } => format!("SetOutsideControl({platform_id})"),
        ReleaseOutsideControl { platform_id } => format!("ReleaseOutsideControl({platform_id})"),
        SetHeading {
            platform_id,
            heading_deg,
            speed_ms,
            ..
        } => format!("SetHeading({platform_id}, hdg={heading_deg:.1}, spd={speed_ms:?})"),
        SetSpeed {
            platform_id,
            speed_ms,
            ..
        } => format!("SetSpeed({platform_id}, {speed_ms:.2} m/s)"),
        SetAltitude {
            platform_id,
            altitude_m,
            ..
        } => format!("SetAltitude({platform_id}, {altitude_m:.1} m)"),
        GotoLocation {
            platform_id,
            lat,
            lon,
            alt,
            ..
        } => {
            format!("GotoLocation({platform_id}, {lat:.5},{lon:.5}, alt={alt:?})")
        }
        FollowRoute {
            platform_id,
            waypoints,
            ..
        } => format!("FollowRoute({platform_id}, waypoints={})", waypoints.len()),
        SensorOn {
            platform_id,
            sensor_id,
        } => format!("SensorOn({platform_id}, sensor={sensor_id:?})"),
        SensorOff {
            platform_id,
            sensor_id,
        } => format!("SensorOff({platform_id}, sensor={sensor_id:?})"),
        SensorSetMode {
            platform_id,
            sensor_id,
            mode,
        } => format!("SensorSetMode({platform_id}, sensor={sensor_id}, mode={mode})"),
        FireAtTarget {
            platform_id,
            weapon_id,
            track_id,
        } => format!("FireAtTarget({platform_id}, {weapon_id}->{track_id})"),
        FireSalvo {
            platform_id,
            weapon_id,
            track_id,
            salvo_size,
        } => format!("FireSalvo({platform_id}, {weapon_id}->{track_id}, n={salvo_size})"),
        UpdateTarget {
            platform_id,
            track_id,
        } => format!("UpdateTarget({platform_id}, {track_id})"),
        JamStart {
            platform_id,
            jammer_id,
            target_track_id,
            ..
        } => format!("JamStart({platform_id}, {jammer_id}->{target_track_id})"),
        JamStop {
            platform_id,
            jammer_id,
        } => format!("JamStop({platform_id}, {jammer_id})"),
        JamSetMode {
            platform_id,
            jammer_id,
            ..
        } => format!("JamSetMode({platform_id}, {jammer_id})"),
        WeaponSafeAll { platform_id } => format!("WeaponSafeAll({platform_id})"),
        FireChaff { platform_id, .. } => format!("FireChaff({platform_id})"),
        SendMessage {
            from_platform_id,
            to_platform_id,
            ..
        } => format!("SendMessage({from_platform_id}->{to_platform_id})"),
        ChangeCommander {
            platform_id,
            new_commander_id,
        } => format!("ChangeCommander({platform_id}, cmdr={new_commander_id})"),
        other => format!(
            "{:?}({})",
            other.command_class(),
            other.target_platform_id()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn append_creates_log_under_project_log_dir() {
        let root =
            std::env::temp_dir().join(format!("openfang_cmd_log_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        env::set_var("OPENFANG_ROOT", root.to_string_lossy().to_string());

        log_drop("unit test drop");
        let path = log_path();
        assert_eq!(path, root.join("log").join("arksim_cmd.log"));
        let text = std::fs::read_to_string(&path).expect("log file");
        assert!(text.contains("DROP unit test drop"));

        env::remove_var("OPENFANG_ROOT");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn describe_fire_at_target() {
        let cmd = PlatformCommand::FireAtTarget {
            platform_id: "self".into(),
            weapon_id: "loiter_wave2".into(),
            track_id: "self:1".into(),
        };
        assert_eq!(
            describe_command(&cmd),
            "FireAtTarget(self, loiter_wave2->self:1)"
        );
    }
}
