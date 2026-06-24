//! ArkService session client — orchestrates [`ResponseHandler`] + [`ArkSimController`].
//!
//! Replaces the earlier dual-thread socket sharing with the Python reference
//! architecture: one I/O thread owns the DEALER socket; this type only enqueues
//! JSON commands and reads the latest customized-situation cache.

use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::Value;

use crate::arksim_controller::{ArkSimController, SimulationConfig};
use crate::response_handler::{ark_command_error, ResponseHandler};
use crate::situation;

/// Blocking ArkService client (used from `spawn_blocking` in the adapter).
pub struct ArkServiceClient {
    handler: ResponseHandler,
    session_uuid: Option<String>,
    #[allow(dead_code)]
    situation_interval_secs: f64,
}

impl ArkServiceClient {
    /// Connect to ArkService, `start` the scenario, and bind routing from the
    /// returned session uuid (never read from static config).
    pub fn connect(
        host: &str,
        service_port: u16,
        scenario_path: String,
        situation_interval_secs: f64,
    ) -> Result<Self, String> {
        let socket_id = format!("ark_ctrl_{}", uuid::Uuid::new_v4().simple());
        let handler = ResponseHandler::spawn(host, service_port, socket_id)?;
        let controller = ArkSimController;

        let start = controller.start_instance(&SimulationConfig {
            offscreen: true,
            scenarios: vec![scenario_path],
            ..Default::default()
        });
        handler.send_json(start)?;
        let uuid = handler.wait_session_uuid(Duration::from_secs(30))?;

        handler.set_routing_id(&uuid)?;

        for cmd in controller.apply_default_situation(&uuid, situation_interval_secs) {
            handler.send_json(cmd)?;
        }
        handler.send_json(controller.resume_simulation(&uuid))?;

        Ok(Self {
            handler,
            session_uuid: Some(uuid),
            situation_interval_secs,
        })
    }

    /// Attach to an already-running Warlock / mission instance (manual start).
    /// Skips `start`; binds ZMQ ROUTING_ID to the given session uuid and
    /// subscribes to customized situation only.
    pub fn connect_attach(
        host: &str,
        service_port: u16,
        session_uuid: String,
        situation_interval_secs: f64,
    ) -> Result<Self, String> {
        let socket_id = format!("ark_attach_{}", uuid::Uuid::new_v4().simple());
        let handler = ResponseHandler::spawn(host, service_port, socket_id)?;
        handler.set_routing_id(&session_uuid)?;

        let controller = ArkSimController;
        for cmd in controller.apply_default_situation(&session_uuid, situation_interval_secs) {
            handler.send_json(cmd)?;
        }

        Ok(Self {
            handler,
            session_uuid: Some(session_uuid),
            situation_interval_secs,
        })
    }

    /// Advance simulation after weapon proto (runstep + optional advance_to_time).
    pub fn advance_simulation(
        &self,
        runstep_count: u32,
        advance_to_time_secs: Option<f64>,
    ) -> Result<(), String> {
        let uuid = self
            .session_uuid
            .as_deref()
            .ok_or_else(|| "ArkService session uuid is not known".to_string())?;
        let controller = ArkSimController;
        if runstep_count > 0 {
            self.handler
                .send_json(controller.run_step(uuid, runstep_count))?;
            std::thread::sleep(Duration::from_millis(200));
        }
        if let Some(time) = advance_to_time_secs {
            self.handler
                .send_json(controller.advance_to_time(uuid, time))?;
        }
        Ok(())
    }

    pub fn endpoint(&self) -> &str {
        self.handler.endpoint()
    }

    pub fn session_uuid(&self) -> Option<&str> {
        self.session_uuid.as_deref()
    }

    #[allow(dead_code)]
    pub fn situation_interval_secs(&self) -> f64 {
        self.situation_interval_secs
    }

    pub fn recv_snapshot(
        &self,
        timeout: Duration,
    ) -> Result<Option<openfang_types::platform::WorldSnapshot>, String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(msg) = self.handler.latest_situation_message() {
                if let Some(snapshot) = situation::snapshot_from_arkservice_message(&msg) {
                    return Ok(Some(snapshot));
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(None)
    }

    pub fn send_actions(&self, proto_bytes: &[u8]) -> Result<(), String> {
        let uuid = self
            .session_uuid
            .as_deref()
            .ok_or_else(|| "ArkService session uuid is not known".to_string())?;
        let proto = proto_bytes_to_json_string(proto_bytes);
        let controller = ArkSimController;
        self.handler.clear_latest_command_message();
        self.handler
            .send_json(controller.send_entity_command(uuid, &proto))?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Some(message) = self.handler.latest_command_message() {
                if let Some(err) = ark_command_error(&message) {
                    return Err(format!("ArkService rejected proto action: {err}"));
                }
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        tracing::warn!(
            payload_len = proto_bytes.len(),
            "ArkService did not return a command acknowledgement for proto action"
        );
        Ok(())
    }

    /// Re-subscribe customized situation (e.g. after attach or interval change).
    #[allow(dead_code)]
    pub fn refresh_situation_subscription(&self) -> Result<(), String> {
        let uuid = self
            .session_uuid
            .as_deref()
            .ok_or_else(|| "session uuid required".to_string())?;
        let controller = ArkSimController;
        for cmd in controller.apply_default_situation(uuid, self.situation_interval_secs) {
            self.handler.send_json(cmd)?;
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn send_sim_command(&self, command: Value) -> Result<(), String> {
        self.handler.send_json(command)
    }
}

fn proto_bytes_to_json_string(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proto_bytes_use_standard_base64() {
        let raw = [
            0x4a, 0x20, 0x08, 0x0c, 0x12, 0x10, 0x0a, 0x04, 0x73, 0x65, 0x6c, 0x66, 0x12, 0x08,
            0x67, 0x75, 0x6e, 0x5f, 0x33, 0x30, 0x6d, 0x6d, 0x1a, 0x0a, 0x78, 0x71, 0x35, 0x38,
            0x61, 0x5f, 0x62, 0x31, 0x3a, 0x31,
        ];
        let encoded = proto_bytes_to_json_string(&raw);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&encoded)
            .unwrap();
        assert_eq!(decoded, raw);
    }
}
