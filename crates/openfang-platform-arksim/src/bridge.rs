//! ArkSIM TCP bridge — LE length-prefixed frames.
//! Uses hand-coded protobuf (proto_manual) to avoid prost-build dependency.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::proto_manual;

/// Length-prefixed protobuf frame transport over TCP.
/// Frame format: [4 bytes LE u32 payload_len][protobuf payload]
pub struct ArkSimBridge {
    stream: TcpStream,
    read_buf: Vec<u8>,
}

impl ArkSimBridge {
    pub fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            read_buf: vec![0u8; 65536],
        }
    }

    /// Consume the 10-byte ArkSIM handshake.
    pub async fn read_handshake(&mut self) -> Result<(), String> {
        let mut buf = [0u8; 10];
        self.stream
            .read_exact(&mut buf)
            .await
            .map_err(|e| format!("handshake read: {e}"))?;
        tracing::debug!("ArkSIM handshake: {:02x?}", buf);
        Ok(())
    }

    /// Read a complete length-prefixed frame and parse as SimState.
    pub async fn recv_state(&mut self) -> Result<proto_manual::SimState, String> {
        // Read 4-byte length prefix (little-endian)
        let mut len_buf = [0u8; 4];
        self.stream
            .read_exact(&mut len_buf)
            .await
            .map_err(|e| format!("read len: {e}"))?;

        let payload_len = u32::from_le_bytes(len_buf) as usize;

        if payload_len > self.read_buf.len() {
            self.read_buf.resize(payload_len, 0);
        }

        self.stream
            .read_exact(&mut self.read_buf[..payload_len])
            .await
            .map_err(|e| format!("read {}B payload: {e}", payload_len))?;

        proto_manual::parse_state_message(&self.read_buf[..payload_len])
            .ok_or_else(|| "Failed to parse StateMessage".to_string())
    }

    /// Send encoded ActionsFromOutside protobuf with length prefix.
    pub async fn send_actions_raw(&mut self, proto_bytes: &[u8]) -> Result<(), String> {
        let len = proto_bytes.len() as u32;
        let mut frame = Vec::with_capacity(4 + proto_bytes.len());
        frame.extend_from_slice(&len.to_le_bytes());
        frame.extend_from_slice(proto_bytes);

        self.stream
            .write_all(&frame)
            .await
            .map_err(|e| format!("send actions: {e}"))?;

        Ok(())
    }
}

/// Convenience: send SetOutsideControl for a platform.
#[allow(dead_code)]
pub async fn send_set_outside_control(
    bridge: &mut ArkSimBridge,
    agent_id: &str,
) -> Result<(), String> {
    let proto = proto_manual::encode_set_outside_control(agent_id);
    bridge.send_actions_raw(&proto).await
}

/// Convenience: send DesiredHeading for a platform.
#[allow(dead_code)]
pub async fn send_desired_heading(
    bridge: &mut ArkSimBridge,
    agent_id: &str,
    heading_rad: f64,
) -> Result<(), String> {
    let proto = proto_manual::encode_desired_heading_cmd(agent_id, heading_rad);
    bridge.send_actions_raw(&proto).await
}
