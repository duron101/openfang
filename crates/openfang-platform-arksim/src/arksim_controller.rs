//! ArkSIM simulation controller — Rust port of `protobuf/arkcmd/controller/Arksim_controller.py`.
//!
//! Builds JSON command envelopes (`fn` + `uuid` + `args`) for ArkService. Wire
//! transport is handled by [`crate::response_handler::ResponseHandler`].

use serde_json::{json, Value};

use crate::situation::{default_situation_commands, SituationKind};

/// Simulation start parameters (mirrors Python `SimulationConfig`).
#[derive(Debug, Clone)]
pub struct SimulationConfig {
    pub exec: i32,
    pub offscreen: bool,
    pub random_seed: i32,
    pub realtime: bool,
    pub scenarios: Vec<String>,
    pub sim_type: i32,
}

impl Default for SimulationConfig {
    fn default() -> Self {
        Self {
            exec: 1,
            offscreen: false,
            random_seed: 0,
            realtime: false,
            scenarios: vec![],
            sim_type: 0,
        }
    }
}

/// High-level ArkSIM lifecycle + entity control command builders.
#[derive(Debug, Default)]
pub struct ArkSimController;

impl ArkSimController {
    pub fn start_instance(&self, config: &SimulationConfig) -> Value {
        json!({
            "fn": "start",
            "args": {
                "exec": config.exec,
                "offscreen": config.offscreen,
                "randomSeed": config.random_seed,
                "realtime": config.realtime,
                "scenarios": config.scenarios,
                "simType": config.sim_type,
            }
        })
    }

    pub fn pause_simulation(&self, instance_uuid: &str) -> Value {
        json!({ "fn": "pause", "uuid": instance_uuid })
    }

    pub fn resume_simulation(&self, instance_uuid: &str) -> Value {
        json!({ "fn": "resume", "uuid": instance_uuid })
    }

    pub fn stop_simulation(&self, instance_uuid: &str) -> Value {
        json!({ "fn": "exit", "uuid": instance_uuid })
    }

    pub fn restart_simulation(&self, instance_uuid: &str) -> Value {
        json!({ "fn": "restart", "uuid": instance_uuid })
    }

    pub fn run_step(&self, instance_uuid: &str, step: u32) -> Value {
        json!({
            "fn": "runstep",
            "args": { "step": step },
            "uuid": instance_uuid,
        })
    }

    pub fn advance_to_time(&self, instance_uuid: &str, target_time: f64) -> Value {
        json!({
            "fn": "advance_to_time",
            "args": { "time": target_time },
            "uuid": instance_uuid,
        })
    }

    pub fn set_clock_rate(&self, instance_uuid: &str, rate: f64) -> Value {
        json!({
            "fn": "set_clock_rate",
            "args": { "rate": rate },
            "uuid": instance_uuid,
        })
    }

    pub fn send_entity_command(&self, instance_uuid: &str, proto_str: &str) -> Value {
        json!({
            "fn": "proto",
            "proto": proto_str,
            "uuid": instance_uuid,
        })
    }

    pub fn switch_situation_type(&self, instance_uuid: &str, kind: SituationKind) -> Value {
        json!({
            "fn": "changesituation",
            "rate": kind.rate(),
            "uuid": instance_uuid,
        })
    }

    pub fn toggle_simulation_time_output(&self, instance_uuid: &str, enable: bool) -> Value {
        json!({
            "fn": "simulationtimeswitch",
            "rate": enable,
            "uuid": instance_uuid,
        })
    }

    pub fn get_instance_status(&self, instance_uuid: &str) -> Value {
        json!({ "fn": "get_status", "uuid": instance_uuid })
    }

    pub fn set_custom_situation_interval(&self, instance_uuid: &str, interval: f64) -> Value {
        json!({
            "fn": "customizedsituation",
            "time": interval,
            "uuid": instance_uuid,
        })
    }

    /// Default customized situation (`rate=0` + interval) — mirrors Python
    /// `apply_default_situation`.
    pub fn apply_default_situation(&self, instance_uuid: &str, interval: f64) -> Vec<Value> {
        default_situation_commands(instance_uuid, interval)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_matches_interface_new_json_shape() {
        let ctrl = ArkSimController;
        let msg = ctrl.start_instance(&SimulationConfig {
            scenarios: vec!["/path/scenario.txt".into()],
            offscreen: true,
            ..Default::default()
        });
        assert_eq!(msg["fn"], "start");
        assert_eq!(msg["args"]["offscreen"], true);
        assert_eq!(msg["args"]["scenarios"][0], "/path/scenario.txt");
    }

    #[test]
    fn entity_proto_command_shape() {
        let ctrl = ArkSimController;
        let msg = ctrl.send_entity_command("abc", "proto-bytes");
        assert_eq!(msg["fn"], "proto");
        assert_eq!(msg["uuid"], "abc");
        assert_eq!(msg["proto"], "proto-bytes");
    }
}
