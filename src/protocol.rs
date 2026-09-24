//! The agent wire protocol.
//!
//! A remote endpoint speaks to an agent process (the same binary run with
//! `autobahn agent`) over a byte stream — typically SSH's stdin/stdout. The
//! protocol is a strict request/response mirror of the [`Endpoint`] trait:
//! bincode-serialized, length-prefixed frames, preceded by a version
//! handshake. Both ends must be the same version (agents are expected to be
//! installed alongside the CLI on the remote host). Beside the responses,
//! an agent reports each channel's work as it moves
//! ([`MuxResponse::Progress`]), so the controller can tell a slow request
//! from a stuck one.
//!
//! [`Endpoint`]: crate::endpoint::Endpoint

use serde::{Deserialize, Serialize};

use crate::endpoint::{FileRequest, StagingNeed, TransferFrame, TransitionOutcome};
use crate::tree::{Change, Digest};

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
    /// Whether mount points inside the root are left alone rather than
    /// walked.
    pub ignore_mounts: bool,
    /// The owner (name or `id:N`) for created entries, resolved on the
    /// agent's host (`None` to leave ownership alone).
    pub default_owner: Option<String>,
    /// The group (name or `id:N`) for created entries, resolved on the
    /// agent's host (`None` to leave ownership alone).
    pub default_group: Option<String>,
    /// Whether the session will never wait for a change — a single pass —
    /// so the agent registers no watcher for the root. Registering one
    /// walks the whole tree once more, for nothing.
    pub one_shot: bool,
}

/// Whether a string is a session identifier as
/// [`crate::session::session_identifier`] produces it: 32 lowercase hex
/// characters. Nothing else can name a directory of a session's own.
pub fn is_session_identifier(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl Initialize {
    /// Refuses identifiers that could not have come from a genuine
    /// controller. The session and side name the agent's staging and its
    /// ancestor copy, so this runs before anything touches the filesystem.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            is_session_identifier(&self.session),
            "refusing session identifier {:?}",
            self.session
        );
        anyhow::ensure!(
            matches!(self.side.as_str(), "alpha" | "beta"),
            "refusing side {:?}",
            self.side
        );
        Ok(())
    }
}

/// A request from the controller to the agent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Request {
    /// Perform a scan.
    Scan,
    /// Perform a scan with digest reuse disabled: every file's content is
    /// re-read, so content changed without its metadata moving becomes
    /// visible. The verify verb's scan.
    ScanVerified,
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
    /// milliseconds elapses. `since` names the generation the controller
    /// last saw (from a scan or a transition), so the wait is for anything
    /// after it; `None` waits from the endpoint's own last scan.
    AwaitChanges {
        milliseconds: u64,
        since: Option<u64>,
    },
    /// Pull the next batch of snapshot delta operations, after a
    /// `ScanDelta` response. An empty batch ends the stream.
    ScanPull,
    /// Repeat the last scan's result as a delta against *nothing*: every
    /// byte of the encoded snapshot as data operations. The controller's
    /// recovery when it cannot reproduce the baseline a `ScanDelta` named.
    ScanFull,
    /// Read one file's content, by root-relative path. `resolve` and `diff`
    /// use this to look at a conflict's sides; it is not a synchronization
    /// primitive, and reads nothing but a regular file.
    ReadFile(String),
    /// Move one entry aside, by root-relative path. Whatever is at the
    /// first path — a file, a symbolic link, or a whole tree — ends up at
    /// the second, which must not already exist.
    ///
    /// `resolve --keep both` uses this to preserve the losing side's
    /// version under another name before the winner's version arrives. A
    /// rename is the only operation that can do that for a directory
    /// without moving its content, and the losing side is the only place
    /// that content exists.
    Rename(String, String),
    /// Peering: present the controller's lease on this host, renewing it
    /// or learning that a newer leader holds it. Sent first on a channel
    /// by a controller in a peering mode, and again on every cycle. A
    /// channel that presents a term below the host's lease is fenced:
    /// every write it asks for is refused until it presents a term at
    /// least as high.
    Lease(crate::peering::Lease),
    /// Peering: the changes that took the leader's ancestor for this
    /// session from `generation - 1` to `generation`, so the host's copy
    /// stays level with the leader's. The answer carries the copy's
    /// generation; one that is not `generation` means the record was not
    /// applied and a checkpoint is due.
    AncestorRecord {
        generation: u64,
        changes: Vec<Change>,
    },
    /// Peering: the leader's whole ancestor for this session, replacing
    /// the host's copy at `generation`.
    AncestorCheckpoint {
        generation: u64,
        ancestor: Option<crate::tree::Node>,
    },
    /// Peering: a file a follower needs — `config.toml`, `name`, or
    /// `ignores/<file>` — written under the host's peering directory.
    /// Nothing else can be named.
    PutPeeringFile { name: String, bytes: Vec<u8> },
    /// Peering: what the host holds for this session.
    PeeringState,
}

/// The header of a snapshot sent as a delta.
///
/// A snapshot is never sent as one frame. The agent encodes it, runs the
/// rsync engine over those bytes against the encoding it last sent on this
/// channel, and streams the resulting operations in bounded batches; the
/// controller reassembles the new encoding from the old one. That removes
/// the ceiling a single frame's size cap put on tree size, and makes a
/// rescan of a large tree that changed a little cost a little.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScanDelta {
    /// The digest of the encoded snapshot the delta was computed against,
    /// or `None` when it was computed against nothing (a full stream).
    pub baseline: Option<Digest>,
    /// The digest of the encoded snapshot the stream reassembles to. The
    /// controller checks its reassembly against this, so a baseline the two
    /// sides disagree about is detected rather than decoded.
    pub digest: Digest,
    /// The length of that encoding, in bytes.
    pub length: u64,
    /// The block size the baseline's signature used, which the controller
    /// needs to build the matching signature over its own copy.
    pub block_size: u32,
    /// The generation of the root's observer the snapshot was taken at.
    pub generation: u64,
}

/// A response from the agent to the controller. Every response variant
/// corresponds to exactly one request variant; `Error` may answer any
/// request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Response {
    /// The result of initialization.
    Initialized,
    /// The staging needs resulting from StageBegin.
    StageBegin(Vec<StagingNeed>),
    /// Acknowledgement of SupplyOpen.
    SupplyOpened,
    /// A batch of supply frames (empty when exhausted).
    SupplyPull(Vec<TransferFrame>),
    /// Acknowledgement of StagePush.
    StagePushed,
    /// The outcome of Transition, and the generation the root's observer
    /// stands at after it: any change past this one is not the
    /// transition's own writes.
    Transition {
        outcome: TransitionOutcome,
        generation: u64,
    },
    /// The scan produced exactly the snapshot this channel last sent, so
    /// the snapshot itself is not repeated. Answering an unchanged root
    /// this way is what keeps a heartbeat from costing a full snapshot
    /// serialization, transfer, and decode on every cycle.
    ///
    /// Adding, removing, or reordering a variant changes the wire format —
    /// the encoding numbers them by declaration order — so any such change
    /// requires a version bump. Version equality is enforced by the
    /// handshake, which is what keeps two builds that disagree about this
    /// enum from ever exchanging a frame.
    /// ...which stands at this generation of the root's observer.
    ScanUnchanged { generation: u64 },
    /// Whether AwaitChanges observed a change — and whether the agent was
    /// watching the root at all. `watching` false means the root is on
    /// interval polling, so a quiet wait proves nothing.
    AwaitChanges { changed: bool, watching: bool },
    /// A request-level failure.
    Error(String),
    /// A scan result, sent as a delta: this header, then `ScanOps` batches
    /// in answer to `ScanPull` until an empty one.
    ScanDelta(ScanDelta),
    /// A batch of snapshot delta operations; empty when the stream is done.
    ScanOps(Vec<crate::rsync::Op>),
    /// A file's content (`None` when there is no regular file at the path).
    File(Option<Vec<u8>>),
    /// Acknowledgement of Rename.
    Written,
    /// The answer to a presented lease.
    Lease(crate::peering::LeaseAnswer),
    /// The generation the host's ancestor copy stands at after an
    /// `AncestorRecord` or an `AncestorCheckpoint`.
    Recorded { generation: u64 },
    /// What the host holds for the channel's session.
    PeeringState(crate::peering::State),
    /// How far a scan still running has got: sent every half second or
    /// so, only once it has run that long, and always before the scan's
    /// own answer. The controller counts it and keeps reading.
    ScanProgress { entries: u64, bytes: u64 },
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
pub enum MuxResponse {
    /// A response on a channel: the answer to its oldest outstanding
    /// request, or a scan's progress report ahead of that answer.
    Response {
        /// The channel the response belongs to.
        channel: u32,
        /// The response itself.
        response: Response,
    },
    /// The channel's work counter has moved since the last report: the
    /// request it is serving is still being worked on. Sent every few
    /// seconds, and only when the counter moved, so a channel whose work
    /// has stopped falls silent even while the agent's other threads keep
    /// running. It answers nothing; the controller only notes the time.
    Progress {
        /// The channel whose work moved.
        channel: u32,
        /// The channel's work counter.
        counter: u64,
    },
}

/// The compatibility epoch: bumped whenever safety-relevant behavior
/// changes without a wire-format change — a hardened transition path, a
/// stricter validation rule — so that a stale agent built from the same
/// package version cannot pass the handshake and silently run without the
/// fix. The epoch rides inside the version string, which means the
/// handshake comparison, the installed agent's filename, and the mismatch
/// diagnostic all enforce it with no protocol change at all: a mismatched
/// agent fails the handshake, and the installer places the new agent at a
/// path the old one never occupied.
pub const COMPATIBILITY_EPOCH: u32 = 15;

/// Returns the version string used for handshake validation and agent
/// installation: the package version qualified by the compatibility epoch.
pub fn version() -> String {
    format!("{}+e{}", env!("CARGO_PKG_VERSION"), COMPATIBILITY_EPOCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The version goes unquoted into the home-relative remote command
    /// that runs the agent (`transport::install`). That is safe only while
    /// it holds nothing a shell would interpret, so any change that lets
    /// it hold more fails here rather than on some remote host.
    #[test]
    fn the_version_holds_nothing_a_shell_would_interpret() {
        let version = version();
        assert!(!version.is_empty());
        assert!(
            version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte)),
            "the version {version:?} holds characters outside [0-9A-Za-z._+-]"
        );
    }

    /// An initialization that differs from a genuine one only where a
    /// test says.
    fn initialize(session: &str, side: &str) -> Initialize {
        Initialize {
            root: "/unused".into(),
            session: session.into(),
            ignores: Vec::new(),
            symlink_mode: crate::scan::SymlinkMode::Raw,
            file_mode: None,
            directory_mode: None,
            side: side.into(),
            staging: Default::default(),
            max_file_size: None,
            max_entry_count: None,
            ignore_mounts: true,
            default_owner: None,
            default_group: None,
            one_shot: false,
        }
    }

    #[test]
    fn a_genuine_identifier_is_accepted() {
        let session = crate::session::session_identifier("a", "b");
        assert!(is_session_identifier(&session));
        initialize(&session, "beta")
            .validate()
            .expect("a genuine session and side are accepted");
        initialize(&session, "alpha")
            .validate()
            .expect("a genuine session and side are accepted");
    }

    /// Anything a path could be made of beyond one hex name is refused:
    /// traversal, separators, absolute paths, the empty name, and the
    /// wrong length either way.
    #[test]
    fn a_session_that_is_not_32_hex_characters_is_refused() {
        let hex = "0123456789abcdef0123456789abcdef";
        for session in [
            "..",
            "../../..",
            "a/b",
            "/tmp/x",
            "",
            &hex[..31],
            &format!("{hex}0"),
            &format!("{}/", &hex[..31]),
        ] {
            assert!(!is_session_identifier(session), "{session:?}");
            let error = initialize(session, "beta")
                .validate()
                .expect_err("the session must be refused");
            assert!(
                format!("{error:#}").contains("refusing session identifier"),
                "{session:?}: {error:#}"
            );
        }
    }

    /// `session_identifier` never produces uppercase hex, so a controller
    /// that sends it is not a genuine one.
    #[test]
    fn an_uppercase_session_is_refused() {
        let session = crate::session::session_identifier("a", "b").to_uppercase();
        assert!(!is_session_identifier(&session));
        assert!(initialize(&session, "beta").validate().is_err());
    }

    #[test]
    fn an_unknown_side_is_refused() {
        let session = crate::session::session_identifier("a", "b");
        for side in ["gamma", "../alpha", "", "Beta"] {
            let error = initialize(&session, side)
                .validate()
                .expect_err("the side must be refused");
            assert!(
                format!("{error:#}").contains("refusing side"),
                "{side:?}: {error:#}"
            );
        }
    }
}
