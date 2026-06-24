//! ArkService ZMQ response handler — Rust port of `protobuf/arkcomm/response_handler.py`.
//!
//! All socket send/recv/poll operations run on **one dedicated thread** (DEALER +
//! ROUTER/DEALER `60004`). Callers enqueue commands; inbound frames are classified
//! and the latest customized situation is cached (buffer size 1).

use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::Value;
use tracing::{debug, error, info, warn};

use crate::situation;

const POLL_TIMEOUT_MS: i64 = 100;

enum IoCommand {
    Send(Value, SyncSender<Result<(), String>>),
    SetRoutingId(String, SyncSender<Result<(), String>>),
    Stop,
}

/// Background ArkService I/O — mirrors Python `ResponseHandler`.
pub struct ResponseHandler {
    endpoint: String,
    socket_id: String,
    cmd_tx: Sender<IoCommand>,
    join: Option<JoinHandle<()>>,
    session_uuid: Arc<Mutex<Option<String>>>,
    latest_situation: Arc<Mutex<Option<Value>>>,
    latest_command: Arc<Mutex<Option<Value>>>,
}

impl ResponseHandler {
    /// Spawn the I/O thread and connect to `tcp://{host}:{port}`.
    pub fn spawn(host: &str, port: u16, socket_id: impl Into<String>) -> Result<Self, String> {
        let endpoint = format!("tcp://{host}:{port}");
        let socket_id = socket_id.into();
        let session_uuid = Arc::new(Mutex::new(None));
        let latest_situation = Arc::new(Mutex::new(None));
        let latest_command = Arc::new(Mutex::new(None));
        let (cmd_tx, cmd_rx) = mpsc::channel();

        let endpoint_for_thread = endpoint.clone();
        let socket_id_for_thread = socket_id.clone();
        let session_for_thread = Arc::clone(&session_uuid);
        let latest_for_thread = Arc::clone(&latest_situation);
        let command_for_thread = Arc::clone(&latest_command);

        let join = thread::Builder::new()
            .name("arksim-response-handler".into())
            .spawn(move || {
                io_loop(
                    &endpoint_for_thread,
                    &socket_id_for_thread,
                    cmd_rx,
                    session_for_thread,
                    latest_for_thread,
                    command_for_thread,
                );
            })
            .map_err(|e| format!("spawn response handler: {e}"))?;

        Ok(Self {
            endpoint,
            socket_id,
            cmd_tx,
            join: Some(join),
            session_uuid,
            latest_situation,
            latest_command,
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn socket_id(&self) -> &str {
        &self.socket_id
    }

    pub fn session_uuid(&self) -> Option<String> {
        self.session_uuid.lock().ok()?.clone()
    }

    pub fn latest_situation_message(&self) -> Option<Value> {
        self.latest_situation.lock().ok()?.clone()
    }

    pub fn latest_command_message(&self) -> Option<Value> {
        self.latest_command.lock().ok()?.clone()
    }

    pub fn clear_latest_command_message(&self) {
        if let Ok(mut guard) = self.latest_command.lock() {
            *guard = None;
        }
    }

    pub fn wait_session_uuid(&self, timeout: Duration) -> Result<String, String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(uuid) = self.session_uuid() {
                return Ok(uuid);
            }
            thread::sleep(Duration::from_millis(50));
        }
        Err("ArkService session uuid not received within timeout".into())
    }

    pub fn send_json(&self, value: Value) -> Result<(), String> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.cmd_tx
            .send(IoCommand::Send(value, tx))
            .map_err(|e| format!("response handler send queue closed: {e}"))?;
        rx.recv()
            .map_err(|e| format!("response handler send ack: {e}"))?
    }

    pub fn set_routing_id(&self, uuid: &str) -> Result<(), String> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.cmd_tx
            .send(IoCommand::SetRoutingId(uuid.to_string(), tx))
            .map_err(|e| format!("response handler routing queue closed: {e}"))?;
        rx.recv()
            .map_err(|e| format!("response handler routing ack: {e}"))?
    }
}

impl Drop for ResponseHandler {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(IoCommand::Stop);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn io_loop(
    endpoint: &str,
    socket_id: &str,
    cmd_rx: Receiver<IoCommand>,
    session_uuid: Arc<Mutex<Option<String>>>,
    latest_situation: Arc<Mutex<Option<Value>>>,
    latest_command: Arc<Mutex<Option<Value>>>,
) {
    let ctx = zmq::Context::new();
    let socket = match ctx.socket(zmq::DEALER) {
        Ok(s) => s,
        Err(e) => {
            error!("ArkService DEALER socket: {e}");
            return;
        }
    };
    if socket.set_linger(0).is_err()
        || socket.set_identity(socket_id.as_bytes()).is_err()
        || socket.connect(endpoint).is_err()
    {
        error!("ArkService connect failed: {endpoint} id={socket_id}");
        return;
    }
    info!(%endpoint, socket_id, "ArkService response handler connected");

    let mut running = true;
    while running {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                IoCommand::Send(value, ack) => {
                    let res = send_json_frame(&socket, &value);
                    let _ = ack.send(res);
                }
                IoCommand::SetRoutingId(uuid, ack) => {
                    let res = socket
                        .set_identity(uuid.as_bytes())
                        .map(|()| {
                            if let Ok(mut guard) = session_uuid.lock() {
                                *guard = Some(uuid.clone());
                            }
                            info!(session = %uuid, "ArkService routing id updated");
                        })
                        .map_err(|e| format!("set routing id: {e}"));
                    let _ = ack.send(res);
                }
                IoCommand::Stop => running = false,
            }
        }

        let mut items = [socket.as_poll_item(zmq::POLLIN)];
        match zmq::poll(&mut items, POLL_TIMEOUT_MS) {
            Ok(0) => continue,
            Ok(_) => {}
            Err(e) => {
                warn!("ArkService poll error: {e}");
                continue;
            }
        }
        if !items[0].is_readable() {
            continue;
        }

        let Ok(frames) = socket.recv_multipart(0) else {
            continue;
        };
        let Some(text) = frames
            .iter()
            .rev()
            .find_map(|f| std::str::from_utf8(f).ok())
        else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        let Ok(mut message) = serde_json::from_str::<Value>(text) else {
            debug!("ArkService non-JSON frame ({} bytes)", text.len());
            continue;
        };

        learn_session_uuid(&session_uuid, &message);

        match situation::classify_message(&mut message) {
            situation::ArkMessageKind::CustomizedSituation => {
                if let Ok(mut guard) = latest_situation.lock() {
                    *guard = Some(message);
                }
            }
            situation::ArkMessageKind::Command => {
                if let Some(err) = ark_command_error(&message) {
                    error!("ArkService command error: {err}");
                } else {
                    debug!("ArkService command frame: {}", summarize_keys(&message));
                }
                if let Ok(mut guard) = latest_command.lock() {
                    *guard = Some(message);
                }
            }
            situation::ArkMessageKind::Progress => {
                debug!("ArkService progress frame");
            }
            situation::ArkMessageKind::Scenarios => {
                debug!("ArkService scenarios frame");
            }
        }
    }
}

fn send_json_frame(socket: &zmq::Socket, value: &Value) -> Result<(), String> {
    let raw = serde_json::to_string(value).map_err(|e| format!("json encode: {e}"))?;
    socket
        .send(raw.as_bytes(), 0)
        .map_err(|e| format!("send json: {e}"))
}

fn learn_session_uuid(session_uuid: &Arc<Mutex<Option<String>>>, message: &Value) {
    let Ok(mut guard) = session_uuid.lock() else {
        return;
    };
    if guard.is_some() {
        return;
    }
    let uuid = message.get("uuid").and_then(|v| v.as_str()).or_else(|| {
        message
            .get("data")
            .and_then(|d| d.get("uuid"))
            .and_then(|v| v.as_str())
    });
    if let Some(uuid) = uuid {
        *guard = Some(uuid.to_string());
    }
}

fn summarize_keys(message: &Value) -> String {
    message
        .as_object()
        .map(|o| o.keys().cloned().collect::<Vec<_>>().join(","))
        .unwrap_or_else(|| "?".into())
}

pub fn ark_command_error(message: &Value) -> Option<String> {
    if let Some(code) = message.get("code").and_then(|v| v.as_i64()) {
        if code != 0 && code != 200 {
            return Some(format!("code={code}, message={}", command_message(message)));
        }
    }
    if let Some(status) = message.get("status").and_then(|v| v.as_str()) {
        if matches!(
            status.to_ascii_lowercase().as_str(),
            "error" | "failed" | "fail"
        ) {
            return Some(format!(
                "status={status}, message={}",
                command_message(message)
            ));
        }
    }
    if let Some(success) = message.get("success").and_then(|v| v.as_bool()) {
        if !success {
            return Some(format!(
                "success=false, message={}",
                command_message(message)
            ));
        }
    }
    if let Some(data) = message.get("data") {
        return ark_command_error(data);
    }
    None
}

fn command_message(message: &Value) -> String {
    message
        .get("message")
        .or_else(|| message.get("msg"))
        .or_else(|| message.get("error"))
        .and_then(|v| v.as_str())
        .unwrap_or("no detail")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handler_spawn_and_stop_cleanly() {
        let handler = ResponseHandler::spawn("127.0.0.1", 1, "test-socket").unwrap();
        assert_eq!(handler.socket_id(), "test-socket");
    }

    #[test]
    fn learn_session_uuid_from_top_level_or_data() {
        let slot = Arc::new(Mutex::new(None));
        learn_session_uuid(&slot, &serde_json::json!({"uuid": "abc123"}));
        assert_eq!(slot.lock().unwrap().as_deref(), Some("abc123"));

        let slot2 = Arc::new(Mutex::new(None));
        learn_session_uuid(&slot2, &serde_json::json!({"data": {"uuid": "def456"}}));
        assert_eq!(slot2.lock().unwrap().as_deref(), Some("def456"));
    }

    #[test]
    fn command_error_detects_common_error_shapes() {
        assert!(
            ark_command_error(&serde_json::json!({"code": 500, "message": "bad proto"}))
                .unwrap()
                .contains("500")
        );
        assert!(
            ark_command_error(&serde_json::json!({"status": "failed", "error": "no target"}))
                .unwrap()
                .contains("failed")
        );
        assert!(ark_command_error(
            &serde_json::json!({"data": {"success": false, "msg": "denied"}})
        )
        .unwrap()
        .contains("denied"));
        assert!(ark_command_error(&serde_json::json!({"code": 200})).is_none());
    }
}
