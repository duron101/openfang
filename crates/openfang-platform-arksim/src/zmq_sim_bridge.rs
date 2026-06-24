//! Warlock / mission 直连 — ZMQ PAIR + 裸 protobuf（port 18000）。
//!
//! 复刻 mid_ark 同步 step 循环（`afsim_multi_agent._run` → `msg_handler.step`）：
//! 仿真端是 WSF_ZMQ_PROCESSOR，**收到一条 action 才推进一步并回一个 StateMessage**。
//! 因此必须像 mid_ark 一样**每个步长无条件发一条 action**（有战术命令就带上，
//! 否则发空 `ActionsFromOutside`），不发则仿真停滞、也收不到态势。
//!
//! 设计：一个后台 driver 线程独占 socket，free-run 紧循环
//! `send(action) → recv(StateMessage)`，节奏由 sim 的 recv 阻塞自然决定。
//! 调用方通过命令队列入队真实 action；`latest_snapshot` 缓存最新态势。

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use openfang_types::platform::WorldSnapshot;
use tracing::{debug, info, warn};
use zmq::{Context, PAIR};

use crate::cmd_log;

/// Background-driven ZMQ PAIR client for Warlock direct control.
pub struct ZmqSimBridge {
    endpoint: String,
    action_tx: Sender<Vec<u8>>,
    latest_snapshot: Arc<Mutex<Option<WorldSnapshot>>>,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl ZmqSimBridge {
    /// Connect to `tcp://{host}:{port}` and start the free-run step driver.
    ///
    /// Matches `protobuf/test_warlock_tcp_fire.py`: first wait for the TCP port
    /// to listen, then create a ZMQ PAIR socket. Do not send an empty handshake
    /// step here; the first real command must be `E_SetAgentOutsideControl`.
    pub fn connect(host: &str, port: u16, connect_timeout: Duration) -> Result<Self, String> {
        wait_for_tcp_listener(host, port, connect_timeout)?;
        let endpoint = format!("tcp://{host}:{port}");
        let display = format!("zmq://{host}:{port}");
        let latest_snapshot: Arc<Mutex<Option<WorldSnapshot>>> = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (action_tx, action_rx) = mpsc::channel::<Vec<u8>>();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);

        let latest_for_thread = Arc::clone(&latest_snapshot);
        let stop_for_thread = Arc::clone(&stop);
        let endpoint_for_thread = endpoint.clone();

        let join = std::thread::Builder::new()
            .name("arksim-warlock-driver".into())
            .spawn(move || {
                driver_loop(
                    &endpoint_for_thread,
                    action_rx,
                    latest_for_thread,
                    stop_for_thread,
                    ready_tx,
                );
            })
            .map_err(|e| format!("spawn warlock driver: {e}"))?;

        // Wait until the driver owns a connected ZMQ socket. The first StateMessage
        // is expected only after the first queued action is sent.
        match ready_rx.recv_timeout(connect_timeout + Duration::from_secs(2)) {
            Ok(Ok(())) => Ok(Self {
                endpoint: display,
                action_tx,
                latest_snapshot,
                stop,
                join: Some(join),
            }),
            Ok(Err(e)) => {
                stop.store(true, Ordering::Relaxed);
                let _ = join.join();
                Err(e)
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                let _ = join.join();
                Err(format!(
                    "Warlock direct {display}: driver did not connect within {connect_timeout:?} — \
                     确认 Warlock 已加载想定并 Play、18000 在监听"
                ))
            }
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Enqueue one action proto to be sent on the next free-run step.
    /// Each blob advances the simulation exactly one step (mid_ark semantics).
    pub fn enqueue_action(&self, proto_bytes: Vec<u8>) -> Result<(), String> {
        self.action_tx
            .send(proto_bytes)
            .map_err(|_| "Warlock driver thread is gone".to_string())
    }

    pub fn cached_snapshot(&self) -> Option<WorldSnapshot> {
        self.latest_snapshot.lock().ok()?.clone()
    }
}

fn wait_for_tcp_listener(host: &str, port: u16, timeout: Duration) -> Result<(), String> {
    let deadline = std::time::Instant::now() + timeout;
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve {host}:{port}: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("resolve {host}:{port}: no socket addresses"));
    }

    let probe_timeout = Duration::from_millis(750);
    while std::time::Instant::now() < deadline {
        if addrs
            .iter()
            .any(|addr| TcpStream::connect_timeout(addr, probe_timeout).is_ok())
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    Err(format!(
        "Warlock direct tcp://{host}:{port}: port not listening within {timeout:?}"
    ))
}

impl Drop for ZmqSimBridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn driver_loop(
    endpoint: &str,
    action_rx: Receiver<Vec<u8>>,
    latest_snapshot: Arc<Mutex<Option<WorldSnapshot>>>,
    stop: Arc<AtomicBool>,
    ready_tx: mpsc::SyncSender<Result<(), String>>,
) {
    let context = Context::new();
    let socket = match context.socket(PAIR) {
        Ok(s) => s,
        Err(e) => {
            let _ = ready_tx.send(Err(format!("ZMQ PAIR socket: {e}")));
            return;
        }
    };
    let _ = socket.set_linger(0);
    // ZMTP heartbeats — match mid_ark MsgHandler.connect (IVL=100, TIMEOUT/TTL=1000).
    let _ = socket.set_heartbeat_ivl(100);
    let _ = socket.set_heartbeat_timeout(1000);
    let _ = socket.set_heartbeat_ttl(1000);
    let _ = socket.set_rcvtimeo(5_000);
    let _ = socket.set_sndtimeo(10_000);
    if let Err(e) = socket.connect(endpoint) {
        let _ = ready_tx.send(Err(format!("ZMQ connect {endpoint}: {e}")));
        return;
    }
    let _ = ready_tx.send(Ok(()));

    // Returns whether the sim reports the run has reached its end time. Once the
    // scenario ends we must STOP stepping: pushing more actions drives AFSIM into
    // its run-teardown (WsfSimulation::~WsfSimulation), which crashes in plugin
    // component destructors (std::terminate). mid_ark does the same — it breaks
    // its `_run` loop when `state["time"] == state["endTime"]`.
    let update_snapshot = |payload: &[u8]| -> bool {
        if let Some(sim) = crate::proto_manual::parse_state_message(payload) {
            let ended = sim.end_time > 0.0 && sim.time + 1e-6 >= sim.end_time;
            let snapshot = crate::state_mapper::from_sim_state(&sim);
            if let Ok(mut guard) = latest_snapshot.lock() {
                *guard = Some(snapshot);
            }
            ended
        } else {
            debug!(
                bytes = payload.len(),
                "Warlock driver: StateMessage parse failed; skipped"
            );
            false
        }
    };

    // ── Steady loop: strict send(action) → recv(state) lockstep, mid_ark `_run`.
    //
    // De-dup / coalesce: the kernel cognitive loop re-issues the *same* platform
    // commands every tick (~18/s). Re-sending a byte-identical action to a
    // blocking sim every step is both pointless and fatal (it crashed Warlock —
    // e.g. firing the same weapon dozens of times/sec). So we drain all queued
    // actions, drop any that are byte-identical to the last one actually sent,
    // and only forward genuinely new/changed commands. A distinct ordered
    // sequence (e.g. SetOutsideControl → FireAtTarget) is preserved.
    //
    // Pacing: a *new* command is sent the instant it arrives; when nothing new is
    // queued we emit at most one empty keep-alive step per IDLE_TICK to advance
    // the sim clock without flooding it.
    //
    // Lockstep: after every send we wait for that step's reply (retry recv on
    // timeout, never resend) so the PAIR pipe never accumulates unmatched sends.
    let idle_tick = Duration::from_millis(200);
    let mut last_cycle: Vec<Vec<u8>> = Vec::new();
    let mut scenario_ended = false;
    while !stop.load(Ordering::Relaxed) {
        // Stop stepping once the run is over — see update_snapshot. Continuing to
        // send actions tears the sim down and crashes Warlock.
        if scenario_ended {
            warn!(
                %endpoint,
                "Warlock direct: scenario reached endTime — driver停止步进（继续发令会触发 AFSIM 运行结束析构崩溃）"
            );
            break;
        }
        // Collect any queued actions this cycle (block briefly so idle steps are
        // paced), then drain the rest non-blocking.
        let mut pending: Vec<Vec<u8>> = Vec::new();
        match action_rx.recv_timeout(idle_tick) {
            Ok(a) => {
                pending.push(a);
                while let Ok(b) = action_rx.try_recv() {
                    pending.push(b);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        // De-duplicate this cycle's blobs, preserving first-occurrence order.
        // The kernel re-issues the same ordered batch sequence (e.g.
        // [SetOutsideControl, sensor+motion]) every cognitive tick, so several
        // ticks accumulate as [SOC, SM, SOC, SM, ...]. Collapsing only
        // *consecutive* duplicates would leave the interleaved repeats and
        // re-send them; de-duping the whole cycle yields the true distinct
        // command sequence [SOC, SM] exactly once.
        let mut distinct: Vec<Vec<u8>> = Vec::new();
        for a in pending {
            if !distinct.contains(&a) {
                distinct.push(a);
            }
        }

        // If the desired command sequence is unchanged from the last cycle, the
        // sim already has it — just keep the clock advancing with an empty step.
        // Only forward when the command set genuinely changes.
        let to_send: Vec<Vec<u8>> = if distinct.is_empty() || distinct == last_cycle {
            vec![Vec::new()]
        } else {
            last_cycle = distinct.clone();
            distinct
        };

        for action in to_send {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            // INFO (diagnostic): dump the exact wire bytes of every NON-empty
            // action actually sent (after de-dup), so a Warlock crash can be
            // pinned to the precise protobuf on the wire just before it.
            cmd_log::log_wire("warlock_zmq", &action);
            if !action.is_empty() {
                let hex: String = action.iter().take(96).map(|b| format!("{b:02x}")).collect();
                info!(
                    len = action.len(),
                    hex = %hex,
                    "Warlock direct: sending non-empty action on wire"
                );
            }
            if let Err(e) = socket.send(&action, 0) {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                warn!("Warlock driver send failed: {e}");
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }

            // Block for this step's reply; retry recv (no resend) until it arrives.
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                match socket.recv_bytes(0) {
                    Ok(payload) => {
                        if update_snapshot(&payload) {
                            scenario_ended = true;
                        }
                        break;
                    }
                    Err(zmq::Error::EAGAIN) => {
                        debug!("Warlock driver: awaiting StateMessage reply (no resend)...");
                    }
                    Err(e) => {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        warn!("Warlock driver recv failed: {e}");
                        std::thread::sleep(Duration::from_millis(200));
                        break;
                    }
                }
            }

            // Run ended mid-batch — don't send the remaining actions into teardown.
            if scenario_ended {
                break;
            }
        }
    }

    debug!(%endpoint, "Warlock driver loop exited");
}
