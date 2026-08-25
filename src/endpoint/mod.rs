//! Synchronization endpoints.
//!
//! An [`Endpoint`] provides the operations the session controller needs from
//! each side of a synchronization: scanning, staging (receiving file content
//! as rsync deltas), supplying (producing those deltas), and transitioning
//! (applying reconciled changes to disk). The controller is the hub — the
//! two endpoints never talk to each other directly, so an endpoint can live
//! in-process ([`local::LocalEndpoint`]) or behind a byte stream on the far
//! side of an SSH connection ([`remote::RemoteEndpoint`], speaking to an
//! agent running the same binary).

pub mod local;
pub mod remote;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::rsync::Signature;
use crate::tree::{Change, Digest, Node, Problem, Snapshot};

/// A request for a file's content, identified by its transition path and
/// expected digest.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileRequest {
    /// The root-relative path at which the content is needed.
    pub path: String,
    /// The expected content digest.
    pub digest: Digest,
}

/// A staging need reported by a destination endpoint: a requested file that
/// isn't already available locally, along with the rsync signature of
/// whatever base content currently exists at its path (empty for none).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StagingNeed {
    /// The file request being staged.
    pub request: FileRequest,
    /// The rsync signature of the destination's current base content.
    pub signature: Signature,
}

/// One frame of a file transfer stream. Frames for the needed files flow in
/// need-list order; each file's frames are its delta operations followed by
/// a single end-of-file frame.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TransferFrame {
    /// A delta operation for the current file.
    Op(crate::rsync::Op),
    /// The end of the current file's stream. If an error message is carried,
    /// the file could not be supplied and its partial content must be
    /// discarded by the receiver.
    EndOfFile {
        /// The supply error for this file, if any.
        error: Option<String>,
    },
}

/// The outcome of a transition operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransitionOutcome {
    /// The achieved content for each transition, in request order. Entries
    /// reflect what is actually on disk after the attempt: the target
    /// content on success, the old content on refusal, or partial content
    /// for partially applied directory operations.
    pub results: Vec<Option<Node>>,
    /// Problems encountered during transitioning.
    pub problems: Vec<Problem>,
    /// Whether or not any staged content was missing during transitioning
    /// (indicating concurrent modification between staging and transition,
    /// warranting an immediate follow-up cycle).
    pub missing_staged_files: bool,
}

/// A synchronization endpoint.
///
/// Methods are `&mut self`: the controller serializes endpoint operations
/// within a cycle. The staging flow is: the controller calls `stage_begin`
/// on the destination (which filters out already-staged content and returns
/// signatures for the rest), `supply_open` on the source, then pumps batches
/// from `supply_pull` into `stage_push` until the source reports exhaustion.
pub trait Endpoint {
    /// Performs a filesystem scan, returning the current snapshot. Endpoints
    /// accelerate rescans internally (via node-resident metadata from prior
    /// snapshots); callers just get a fresh, consistent snapshot.
    fn scan(&mut self) -> Result<Snapshot>;

    /// Begins staging on this (destination) endpoint for the requested
    /// files, returning the subset that actually needs transfer along with
    /// base signatures. Requests satisfiable locally (already-staged content
    /// from an interrupted cycle, or identical content elsewhere in the
    /// root) are staged immediately and omitted from the result.
    fn stage_begin(&mut self, files: Vec<FileRequest>) -> Result<Vec<StagingNeed>>;

    /// Opens a supply stream on this (source) endpoint for the specified
    /// needs.
    fn supply_open(&mut self, needs: Vec<StagingNeed>) -> Result<()>;

    /// Pulls the next batch of transfer frames from an open supply stream.
    /// An empty result indicates the stream is exhausted.
    fn supply_pull(&mut self, max_frames: usize) -> Result<Vec<TransferFrame>>;

    /// Pushes a batch of transfer frames into this (destination) endpoint's
    /// staging, applying them incrementally.
    fn stage_push(&mut self, frames: Vec<TransferFrame>) -> Result<()>;

    /// Applies transitions to this endpoint's filesystem, sourcing file
    /// content from staged data. Refusals (due to concurrent modification)
    /// are reported as problems and reflected in the returned results, not
    /// as errors.
    fn transition(&mut self, transitions: Vec<Change>) -> Result<TransitionOutcome>;
}
