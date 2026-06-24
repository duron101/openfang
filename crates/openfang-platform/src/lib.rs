//! Platform Adapter Layer — protocol-agnostic abstraction for simulation & hardware backends.
//!
//! This crate defines:
//! - `PlatformAdapter` trait: common interface for all backends
//! - `AdapterRegistry`: multi-backend management with platform-id routing
//! - `PlatformError`: unified error type

mod capabilities;
mod error;
pub mod mock;
pub mod noop;
mod registry;
pub mod snapshot;

pub use capabilities::PlatformCapabilities;
pub use error::PlatformError;
pub use mock::MockAdapter;
pub use noop::NoopAdapter;
pub use registry::AdapterRegistry;
pub use snapshot::{
    normalize, normalized, snapshots_equivalent, EquivalenceTolerance, SnapshotCache,
};

use async_trait::async_trait;
use openfang_types::platform::{CommandResult, PlatformCommand, WorldSnapshot};

/// A platform adapter bridges the Agent decision layer to a specific backend
/// (simulation engine, DDS hardware bus, etc.).
///
/// Each adapter implementation translates between the protocol-agnostic
/// domain types and the backend-specific wire format.
#[async_trait]
pub trait PlatformAdapter: Send + Sync {
    /// Unique identifier for this adapter instance
    fn adapter_id(&self) -> &str;

    /// Type tag: "arksim", "dds", "can", etc.
    fn adapter_type(&self) -> AdapterType;

    // ── Lifecycle ──

    /// Establish connection to the backend
    async fn connect(&mut self) -> Result<(), PlatformError>;

    /// Graceful disconnect
    async fn disconnect(&mut self) -> Result<(), PlatformError>;

    /// Whether the adapter is currently connected
    fn is_connected(&self) -> bool;

    // ── Inbound: State Polling ──

    /// Poll the backend for current world state.
    /// In simulation mode this blocks until the next frame.
    /// In hardware mode this returns the latest cached snapshot.
    async fn poll_state(&mut self) -> Result<WorldSnapshot, PlatformError>;

    // ── Outbound: Command Dispatch ──

    /// Send control commands to the backend.
    /// The adapter is responsible for:
    ///   1. Translating commands to backend-specific format
    ///   2. Deduplication/merging (e.g. last SetHeading wins)
    ///   3. Transport-level delivery
    async fn send_commands(
        &mut self,
        commands: &[PlatformCommand],
    ) -> Result<CommandResult, PlatformError>;

    // ── Capability Declaration ──

    /// Return this adapter's supported capabilities.
    /// Used to inform the Agent which operations are available.
    fn capabilities(&self) -> PlatformCapabilities;
}

/// Type tag for adapter identification and routing
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterType {
    ArkSim,
    Dds,
    Can,
    Mavlink,
    Custom(&'static str),
}

impl AdapterType {
    pub fn as_str(&self) -> &str {
        match self {
            Self::ArkSim => "arksim",
            Self::Dds => "dds",
            Self::Can => "can",
            Self::Mavlink => "mavlink",
            Self::Custom(s) => s,
        }
    }
}
