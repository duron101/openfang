use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use openfang_platform_arksim::{command_mapper, proto_manual, state_mapper};
use openfang_types::platform::{PlatformCommand, WorldSnapshot};
use zmq::{Context, PAIR};

#[derive(Debug, Clone)]
struct Args {
    host: String,
    port: u16,
    agent: String,
    weapon: String,
    track: String,
    salvo_size: u32,
    wait_secs: u64,
    step_timeout_secs: i32,
    post_steps: usize,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 18000,
            agent: "self".into(),
            weapon: "loiter_wave2".into(),
            track: "self:1".into(),
            salvo_size: 2,
            wait_secs: 120,
            step_timeout_secs: 45,
            post_steps: 3,
        }
    }
}

fn main() -> Result<(), String> {
    let args = parse_args()?;
    println!("Warlock Direct Rust probe");
    println!(
        "  agent='{}' weapon='{}' track='{}' salvo={}",
        args.agent, args.weapon, args.track, args.salvo_size
    );

    wait_for_tcp_listener(&args.host, args.port, Duration::from_secs(args.wait_secs))?;
    let endpoint = format!("tcp://{}:{}", args.host, args.port);
    let context = Context::new();
    let socket = context
        .socket(PAIR)
        .map_err(|e| format!("ZMQ PAIR socket: {e}"))?;
    socket.set_linger(0).ok();
    socket.set_rcvtimeo(args.step_timeout_secs * 1000).ok();
    socket.set_sndtimeo(10_000).ok();
    socket
        .connect(&endpoint)
        .map_err(|e| format!("ZMQ connect {endpoint}: {e}"))?;
    println!("ZMQ PAIR connected {endpoint} (no empty handshake)");

    let mut recv_count = 0usize;
    let mut last_snapshot: Option<WorldSnapshot> = None;

    step(
        &socket,
        "E_SetAgentOutsideControl",
        &[PlatformCommand::SetOutsideControl {
            platform_id: args.agent.clone(),
        }],
        &mut recv_count,
        &mut last_snapshot,
    )?;

    step(
        &socket,
        "E_SetDesiredHeading",
        &[PlatformCommand::SetHeading {
            platform_id: args.agent.clone(),
            heading_deg: 90.0,
            speed_ms: Some(12.0),
            turn_direction: None,
        }],
        &mut recv_count,
        &mut last_snapshot,
    )?;

    step(
        &socket,
        "E_SetDesiredVelocity",
        &[PlatformCommand::SetSpeed {
            platform_id: args.agent.clone(),
            speed_ms: 12.0,
            acceleration_ms2: Some(0.5),
        }],
        &mut recv_count,
        &mut last_snapshot,
    )?;

    let fire_track = pick_track(&last_snapshot, &args.agent).unwrap_or_else(|| args.track.clone());
    println!("  resolved fire track='{fire_track}'");

    step(
        &socket,
        "E_FireAtTarget",
        &[PlatformCommand::FireAtTarget {
            platform_id: args.agent.clone(),
            weapon_id: args.weapon.clone(),
            track_id: fire_track.clone(),
        }],
        &mut recv_count,
        &mut last_snapshot,
    )?;

    for i in 0..args.post_steps {
        step(
            &socket,
            &format!("post_fire_{}", i + 1),
            &[PlatformCommand::SetSpeed {
                platform_id: args.agent.clone(),
                speed_ms: 12.0,
                acceleration_ms2: Some(0.5),
            }],
            &mut recv_count,
            &mut last_snapshot,
        )?;
    }

    step(
        &socket,
        "E_FireSlavoAtTarget",
        &[PlatformCommand::FireSalvo {
            platform_id: args.agent.clone(),
            weapon_id: args.weapon.clone(),
            track_id: fire_track,
            salvo_size: args.salvo_size,
        }],
        &mut recv_count,
        &mut last_snapshot,
    )?;

    for i in 0..args.post_steps {
        step(
            &socket,
            &format!("post_salvo_{}", i + 1),
            &[PlatformCommand::SetSpeed {
                platform_id: args.agent.clone(),
                speed_ms: 12.0,
                acceleration_ms2: Some(0.5),
            }],
            &mut recv_count,
            &mut last_snapshot,
        )?;
    }

    println!("PASS: Rust Warlock Direct closed loop recv={recv_count}");
    Ok(())
}

fn step(
    socket: &zmq::Socket,
    label: &str,
    commands: &[PlatformCommand],
    recv_count: &mut usize,
    last_snapshot: &mut Option<WorldSnapshot>,
) -> Result<(), String> {
    let bytes = command_mapper::to_proto_bytes(commands);
    let hex: String = bytes.iter().take(80).map(|b| format!("{b:02x}")).collect();
    println!("  >> {label} ({} bytes) hex={hex}", bytes.len());
    socket
        .send(&bytes, 0)
        .map_err(|e| format!("{label}: send failed: {e}"))?;
    let payload = socket
        .recv_bytes(0)
        .map_err(|e| format!("{label}: recv failed: {e}"))?;
    *recv_count += 1;

    let sim = proto_manual::parse_state_message(&payload).ok_or_else(|| {
        format!(
            "{label}: StateMessage parse failed ({} bytes)",
            payload.len()
        )
    })?;
    let snapshot = state_mapper::from_sim_state(&sim);
    println!(
        "  << {label} #{} recv={}B: {}",
        recv_count,
        payload.len(),
        summarize(&snapshot)
    );
    *last_snapshot = Some(snapshot);
    Ok(())
}

fn summarize(snapshot: &WorldSnapshot) -> String {
    let own = snapshot
        .platforms
        .iter()
        .find(|platform| platform.id == "self" || platform.name == "self");
    let own_summary = own.map_or_else(
        || "self=<missing>".to_string(),
        |platform| {
            format!(
                "self hdg={:.1}deg vel={:.1}m/s tracks={} weapons={}",
                platform.pose.heading_deg,
                platform.velocity.speed_ms,
                platform.tracks.len(),
                platform.onboard_weapons.len()
            )
        },
    );
    format!(
        "t={:.1}s platforms={} active_munitions={} | {}",
        snapshot.timestamp,
        snapshot.platforms.len(),
        snapshot.active_munitions.len(),
        own_summary
    )
}

fn pick_track(snapshot: &Option<WorldSnapshot>, agent: &str) -> Option<String> {
    snapshot
        .as_ref()?
        .platforms
        .iter()
        .find(|platform| platform.id == agent || platform.name == agent)
        .and_then(|platform| platform.tracks.first())
        .map(|track| track.track_id.clone())
}

fn wait_for_tcp_listener(host: &str, port: u16, timeout: Duration) -> Result<(), String> {
    println!(
        "waiting for tcp://{host}:{port} up to {}s...",
        timeout.as_secs()
    );
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve {host}:{port}: {e}"))?
        .collect();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if addrs
            .iter()
            .any(|addr| TcpStream::connect_timeout(addr, Duration::from_millis(750)).is_ok())
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Err(format!(
        "tcp://{host}:{port} not listening within {timeout:?}"
    ))
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let mut value = || {
            iter.next()
                .ok_or_else(|| format!("missing value for {flag}"))
        };
        match flag.as_str() {
            "--host" => args.host = value()?,
            "--port" => args.port = value()?.parse().map_err(|e| format!("bad --port: {e}"))?,
            "--agent" => args.agent = value()?,
            "--weapon" => args.weapon = value()?,
            "--track" => args.track = value()?,
            "--salvo-size" => {
                args.salvo_size = value()?
                    .parse()
                    .map_err(|e| format!("bad --salvo-size: {e}"))?;
            }
            "--wait" => {
                args.wait_secs = value()?.parse().map_err(|e| format!("bad --wait: {e}"))?
            }
            "--step-timeout" => {
                args.step_timeout_secs = value()?
                    .parse()
                    .map_err(|e| format!("bad --step-timeout: {e}"))?;
            }
            "--post-steps" => {
                args.post_steps = value()?
                    .parse()
                    .map_err(|e| format!("bad --post-steps: {e}"))?;
            }
            "--help" | "-h" => {
                println!(
                    "Usage: cargo run -p openfang-platform-arksim --example warlock_direct_probe -- [--agent self] [--weapon loiter_wave2] [--track self:1] [--salvo-size 2]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(args)
}
