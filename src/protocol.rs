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
/// Policy travels with it so both endpoints of a session always operate
/// under identical rules.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Initialize {
    /// The synchronization root path on the agent's filesystem.
    pub root: String,
    /// The session identifier (used to isolate staging state).
    pub session: String,
    /// Ignore patterns for scanning.
    pub ignores: Vec<String>,
    /// The treatment of symbolic links.
    pub symlink_mode: crate::scan::SymlinkMode,
    /// The permission bits for created non-executable files (`None` for the
    /// agent's default).
    pub file_mode: Option<u32>,
    /// The permission bits for created directories (`None` for the agent's
    /// default).
    pub directory_mode: Option<u32>,
    /// Which side of the session this endpoint is ("alpha" or "beta") —
    /// part of the agent's staging namespace, so the two sides of one
    /// session never share staging space even on one host.
    pub side: String,
    /// The staging placement on the agent's filesystem.
    pub staging: crate::endpoint::StagingMode,
    /// The per-file size limit (`None` for unlimited).
    pub max_file_size: Option<u64>,
    /// The per-root entry limit (`None` for unlimited).
    pub max_entry_count: Option<u64>,
    /// The owner (name or `id:N`) for created entries, resolved on the
    /// agent's host (`None` to leave ownership alone).
    pub default_owner: Option<String>,
    /// The group (name or `id:N`) for created entries, resolved on the
    /// agent's host (`None` to leave ownership alone).
    pub default_group: Option<String>,
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
    /// Block until content may have changed or the specified number of
    /// milliseconds elapses.
    AwaitChanges(u64),
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
    /// The scan produced exactly the snapshot this channel last sent, so
    /// the snapshot itself is not repeated. Answering an unchanged root
    /// this way is what keeps a heartbeat from costing a full snapshot
    /// serialization, transfer, and decode on every cycle.
    ScanUnchanged,
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
    /// Whether AwaitChanges observed a change.
    AwaitChanges(bool),
    /// A request-level failure.
    Error(String),
}

/// A controller-to-agent frame on a multiplexed connection.
///
/// One agent connection carries any number of sessions, each on its own
/// channel: channels are opened with an [`Initialize`], speak the ordinary
/// request/response protocol, and close independently. Channel identifiers
/// are assigned by the controller and never reused within a connection.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MuxRequest {
    /// Open a channel, initializing its endpoint. The agent answers on the
    /// channel with [`Response::Initialized`] or [`Response::Error`].
    Open {
        /// The new channel's identifier.
        channel: u32,
        /// The endpoint initialization.
        initialize: Initialize,
    },
    /// A request on an open channel.
    Request {
        /// The channel the request belongs to.
        channel: u32,
        /// The request itself.
        request: Request,
    },
    /// Close a channel (its endpoint is dropped; no answer is sent).
    Close {
        /// The channel to close.
        channel: u32,
    },
    /// Terminate the agent (all channels included).
    Shutdown,
}

/// An agent-to-controller frame on a multiplexed connection.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MuxResponse {
    /// The channel the response belongs to.
    pub channel: u32,
    /// The response itself.
    pub response: Response,
}

/// Returns the version string used for handshake validation.
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}
