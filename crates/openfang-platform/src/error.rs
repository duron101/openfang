use thiserror::Error;

/// Unified error type for platform adapter operations.
#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("connection failed: {0}")]
    ConnectionFailed(String),

    #[error("disconnect failed: {0}")]
    DisconnectFailed(String),

    #[error("not connected")]
    NotConnected,

    #[error("already connected")]
    AlreadyConnected,

    #[error("state poll failed: {0}")]
    PollFailed(String),

    #[error("command send failed: {0}")]
    SendFailed(String),

    #[error("unsupported command: {0}")]
    UnsupportedCommand(String),

    #[error("timeout: {0}")]
    Timeout(String),

    #[error("internal error: {0}")]
    Internal(String),

    #[error("serialization error: {0}")]
    Serialization(String),
}

impl PlatformError {
    pub fn conn(msg: impl Into<String>) -> Self {
        Self::ConnectionFailed(msg.into())
    }

    pub fn poll(msg: impl Into<String>) -> Self {
        Self::PollFailed(msg.into())
    }

    pub fn send(msg: impl Into<String>) -> Self {
        Self::SendFailed(msg.into())
    }
}
