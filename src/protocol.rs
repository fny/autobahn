//! The agent wire protocol.
//!
//! A remote endpoint speaks to an agent process (the same binary run with
//! `autobahn agent`) over a byte stream — typically SSH's stdin/stdout. The
//! protocol is a strict request/response mirror of the [`Endpoint`] trait:
//! bincode-serialized, length-prefixed frames, preceded by a version
//! handshake. Both ends must be the same version (agents are expected to be
//! installed alongside the CLI on the remote host).
//!
//! [`Endpoint`]: crate::endpoint::Endpoint

use serde::{Deserialize, Serialize};

use crate::endpoint::{FileRequest, StagingNeed, TransferFrame, TransitionOutcome};
use crate::tree::{Change, Snapshot};

/// The protocol magic, checked during the handshake.
pub const MAGIC: u32 = 0x4142_4E31; // "ABN1"

/// The maximum permitted frame size (a defense against corrupt or
/// adversarial length prefixes, not a limit on transfer sizes — file content
/// streams as bounded batches of delta operations).
pub const MAXIMUM_FRAME_SIZE: u32 = 64 * 1024 * 1024;

/// The handshake sent by each side upon connection.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Handshake {
    /// The protocol magic.
    pub magic: u32,
    /// The speaker's version string, which must match exactly.
    pub version: String,
}

/// The initialization request sent by the controller after the handshake.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Initialize {
    /// The synchronization root path on the agent's filesystem.
    pub root: String,
    /// The session identifier (used to isolate staging state).
    pub session: String,
    /// Ignore patterns for scanning.
    pub ignores: Vec<String>,
}

/// A request from the controller to the agent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Request {
    /// Perform a scan.
    Scan,
    /// Begin staging for the specified files.
    StageBegin(Vec<FileRequest>),
    /// Open a supply stream for the specified needs.
    SupplyOpen(Vec<StagingNeed>),
    /// Pull up to the specified number of frames from the supply stream.
    SupplyPull(usize),
    /// Push frames into staging.
    StagePush(Vec<TransferFrame>),
    /// Apply transitions.
    Transition(Vec<Change>),
    /// Terminate the agent.
    Shutdown,
}

/// A response from the agent to the controller. Every response variant
/// corresponds to exactly one request variant; `Error` may answer any
/// request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Response {
    /// The result of initialization.
    Initialized,
    /// A scan result.
    Scan(Snapshot),
    /// The staging needs resulting from StageBegin.
    StageBegin(Vec<StagingNeed>),
    /// Acknowledgement of SupplyOpen.
    SupplyOpened,
    /// A batch of supply frames (empty when exhausted).
    SupplyPull(Vec<TransferFrame>),
    /// Acknowledgement of StagePush.
    StagePushed,
    /// The outcome of Transition.
    Transition(TransitionOutcome),
    /// A request-level failure.
    Error(String),
}

/// Returns the version string used for handshake validation.
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}
