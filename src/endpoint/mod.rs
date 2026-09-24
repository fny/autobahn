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
pub mod observer;
pub mod remote;

use std::sync::{Arc, Condvar, Mutex};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::rsync::Signature;
use crate::tree::{Change, Digest, Node, Problem, Snapshot};

/// Where an endpoint keeps its staging directory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum StagingMode {
    /// In the endpoint's state area: the session state directory for local
    /// endpoints, `~/.autobahn/staging/<session>` for agents.
    #[default]
    State,
    /// A sibling of the synchronization root — on the root's parent
    /// filesystem, which normally guarantees that publishing staged files
    /// is a same-device rename rather than a copy, and charges staged
    /// content to the root's own volume.
    BesideRoot,
    /// Inside the synchronization root itself (as a scan-excluded hidden
    /// directory) — the only placement guaranteed to share the root's
    /// filesystem even when the root is a mount point, and the right choice
    /// when the root is the only writable, persistent location on its host.
    InsideRoot,
}

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

/// One frame of a file transfer stream. Each file's frames are a begin
/// frame naming its digest, its delta operations, and a single end-of-file
/// frame. Files are named rather than positional so that content can be
/// sent before the destination has said which files it needs: a file it
/// did not ask for is simply not kept.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TransferFrame {
    /// The start of one file's stream: the digest of the content it
    /// carries, which is what the destination asked for it by.
    Begin {
        /// The digest of the file about to be streamed.
        digest: Digest,
    },
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
    /// Which content was confirmed absent from staging, by path and digest.
    ///
    /// The identity is what separates a busy tree from a broken one. Source
    /// churn requests a *fresh* digest on every follow-up, because the
    /// cycle rescans and sees the rewritten file. The same path and digest
    /// going missing twice running therefore cannot be churn: staging is
    /// failing to produce that content at all.
    pub missing_staged: Vec<FileRequest>,
}

/// Expresses a transition's outcome as changes: each transition's path now
/// holds whatever the attempt actually achieved — the target content on
/// success, the old content on refusal, partial content where a directory
/// was only partly created.
///
/// Both the endpoints' own records and the ancestor are updated from this
/// one rendering, so they cannot disagree about what a cycle accomplished.
///
/// An outcome must carry exactly one result per transition. One that does
/// not — from a remote endpoint, a protocol error; from a local one, a
/// defect — is refused whole: pairing what there is would leave the rest
/// recorded as never attempted, and the transitions that matter are
/// exactly those whose outcome is unknown.
pub fn achieved_changes(
    transitions: &[Change],
    outcome: &TransitionOutcome,
) -> Result<Vec<Change>> {
    if outcome.results.len() != transitions.len() {
        anyhow::bail!(
            "the endpoint reported {} results for {} transitions",
            outcome.results.len(),
            transitions.len()
        );
    }
    Ok(transitions
        .iter()
        .zip(outcome.results.iter())
        .map(|(transition, result)| Change {
            path: transition.path.clone(),
            old: None,
            new: result.clone(),
        })
        .collect())
}

/// Folds a transition's achieved results into a snapshot of the endpoint
/// that applied them, returning the endpoint's state as it now stands.
///
/// The results describe what is actually on disk after the attempt —
/// including refusals and partial applications — so grafting them onto the
/// pre-transition snapshot yields a faithful record without rescanning.
/// Both the endpoint and any controller modelling it fold with this same
/// function on the same inputs, so their views cannot drift apart.
/// `None` means the graft failed (which real results should not produce)
/// and the caller must fall back to reading the filesystem.
pub fn fold_transition(
    snapshot: &Snapshot,
    transitions: &[Change],
    outcome: &TransitionOutcome,
) -> Option<Snapshot> {
    let root = crate::tree::apply(
        snapshot.root.as_ref(),
        &achieved_changes(transitions, outcome).ok()?,
    )
    .ok()?;
    let mut folded = snapshot.clone();
    folded.root = root;
    crate::scan::recount(&mut folded);
    Some(folded)
}

/// A synchronization endpoint.
///
/// Methods are `&mut self`: the controller serializes endpoint operations
/// within a cycle. The staging flow is: the controller calls `stage_begin`
/// on the destination (which filters out already-staged content and returns
/// signatures for the rest), `supply_open` on the source, then pumps batches
/// from `supply_pull` into `stage_push` until the source reports exhaustion.
/// Endpoints are `Send` so the controller can drive the two sides of that
/// pump from separate threads.
pub trait Endpoint: Send {
    /// Whether this endpoint's tree lives on another machine. A remote
    /// tree's mutations persist independently of this machine's page
    /// cache, which changes what must be synced before acting (see the
    /// session's intent handling).
    fn is_remote(&self) -> bool {
        false
    }

    /// Adopts a handle on which this endpoint's scans publish their
    /// running counts. Endpoints that cannot report progress — a remote
    /// one, whose scan happens inside a single request on the far side —
    /// ignore it, and the session reports only that they are scanning.
    fn set_scan_progress(&mut self, _progress: Arc<crate::progress::SideProgress>) {}

    /// Performs a filesystem scan, returning the current snapshot. Endpoints
    /// accelerate rescans internally (via node-resident metadata from prior
    /// snapshots); callers just get a fresh, consistent snapshot.
    fn scan(&mut self) -> Result<Snapshot>;

    /// Scans with digest reuse disabled: every file is re-read, making
    /// content that changed without its metadata moving visible. The
    /// default cannot do better than an ordinary scan.
    fn scan_verified(&mut self) -> Result<Snapshot> {
        self.scan()
    }

    /// Begins staging on this (destination) endpoint for the requested
    /// files, returning the subset that actually needs transfer along with
    /// base signatures. Requests satisfiable locally (already-staged content
    /// from an interrupted cycle, or identical content elsewhere in the
    /// root) are staged immediately and omitted from the result.
    fn stage_begin(&mut self, files: Vec<FileRequest>) -> Result<Vec<StagingNeed>>;

    /// Like [`stage_begin`](Endpoint::stage_begin), for an endpoint whose
    /// answer costs a round trip: the request is sent and `None` comes back
    /// at once, so the caller can push what it is sure the endpoint needs
    /// while the answer is in flight, and collect the answer with
    /// [`stage_begin_finish`](Endpoint::stage_begin_finish) afterwards.
    /// The default answers on the spot.
    fn stage_begin_nowait(&mut self, files: Vec<FileRequest>) -> Result<Option<Vec<StagingNeed>>> {
        self.stage_begin(files).map(Some)
    }

    /// The answer to a [`stage_begin_nowait`](Endpoint::stage_begin_nowait)
    /// that returned `None`.
    fn stage_begin_finish(&mut self) -> Result<Vec<StagingNeed>> {
        Err(anyhow::anyhow!("no staging request is awaiting an answer"))
    }

    /// Opens a supply stream on this (source) endpoint for the specified
    /// needs.
    fn supply_open(&mut self, needs: Vec<StagingNeed>) -> Result<()>;

    /// Pulls the next batch of transfer frames from an open supply stream.
    /// An empty result indicates the stream is exhausted.
    fn supply_pull(&mut self, max_frames: usize) -> Result<Vec<TransferFrame>>;

    /// Pushes a batch of transfer frames into this (destination) endpoint's
    /// staging, applying them incrementally.
    fn stage_push(&mut self, frames: Vec<TransferFrame>) -> Result<()>;

    /// Pushes a batch of transfer frames without requiring completion
    /// acknowledgement, letting implementations keep several batches in
    /// flight over high-latency transports. Errors may be deferred to
    /// [`stage_finish`](Endpoint::stage_finish); callers must invoke it
    /// after the final batch. A failure from either method settles all
    /// in-flight batches first, so the endpoint remains usable for
    /// subsequent operations. The default is simply the synchronous push.
    fn stage_push_nowait(&mut self, frames: Vec<TransferFrame>) -> Result<()> {
        self.stage_push(frames)
    }

    /// Completes a sequence of [`stage_push_nowait`] batches, surfacing any
    /// deferred failure. The default (synchronous pushes) has nothing to
    /// wait for.
    ///
    /// [`stage_push_nowait`]: Endpoint::stage_push_nowait
    fn stage_finish(&mut self) -> Result<()> {
        Ok(())
    }

    /// Applies transitions to this endpoint's filesystem, sourcing file
    /// content from staged data. Refusals (due to concurrent modification)
    /// are reported as problems and reflected in the returned results, not
    /// as errors.
    fn transition(&mut self, transitions: Vec<Change>) -> Result<TransitionOutcome>;

    /// Blocks until content beneath the root may have changed or the timeout
    /// elapses, returning whether a change was signaled. `false` after the
    /// timeout means only that nothing was *observed* — callers must still
    /// cycle on a heartbeat, since watching is inherently best-effort.
    ///
    /// The default implementation cannot watch: it waits out the timeout and
    /// reports nothing observed, degrading callers to pure interval polling.
    fn await_change(&mut self, timeout: std::time::Duration) -> Result<bool> {
        std::thread::sleep(timeout);
        Ok(false)
    }

    /// Like [`await_change`](Endpoint::await_change), waiting for anything
    /// after generation `since` of the root (or after the endpoint's own
    /// last scan when `None`), and saying whether the root was being
    /// watched at all — a quiet wait on an unwatched root proves nothing.
    fn await_change_since(
        &mut self,
        _since: Option<u64>,
        timeout: std::time::Duration,
    ) -> Result<(bool, bool)> {
        Ok((self.await_change(timeout)?, false))
    }

    /// The generation of the root's observer that this endpoint's last
    /// scan or transition left it at, when it has one to name.
    fn generation(&self) -> Option<u64> {
        None
    }

    /// Whether a watch begun after the last scan is still standing —
    /// nothing changed on this side since — so the last scan's snapshot
    /// is still the truth and a cycle can use it without asking again.
    /// Conservative: `false` whenever that cannot be known.
    fn unchanged_since_scan(&mut self) -> bool {
        false
    }

    /// The last scan's snapshot, for a cycle that
    /// [`unchanged_since_scan`](Endpoint::unchanged_since_scan) let skip
    /// the scan.
    fn cached_snapshot(&self) -> Option<Snapshot> {
        None
    }

    /// Starts watching for a change without blocking. `signal` is raised
    /// when there is something to poll for: a change, or the end of the
    /// watch. A watch already under way is left as it is — one is enough,
    /// and it still stands for "nothing has changed since it began".
    ///
    /// This is how a session waits on both of its endpoints at once: each
    /// is asked to watch, the session sleeps on the one signal, and the
    /// first side to raise it ends the wait. The blocking
    /// [`await_change`](Endpoint::await_change) stays for callers with a
    /// single endpoint.
    ///
    /// The default implementation cannot watch and raises nothing;
    /// [`watch_poll`](Endpoint::watch_poll) then reports the watch as
    /// ended without a change, and the caller falls back to its heartbeat.
    fn watch_begin(
        &mut self,
        _timeout: std::time::Duration,
        _signal: Arc<WakeSignal>,
    ) -> Result<()> {
        Ok(())
    }

    /// Whether the watch has ended, and if so whether it observed a
    /// change. `None` while it is still under way.
    fn watch_poll(&mut self) -> Result<Option<bool>> {
        Ok(Some(false))
    }

    /// Reads one regular file's content by root-relative path, `None` when
    /// there is no regular file there. For looking at a conflict's sides;
    /// not a synchronization primitive.
    fn read_file(&mut self, _path: &str) -> Result<Option<Vec<u8>>> {
        anyhow::bail!("this endpoint cannot read individual files")
    }

    /// Moves one entry aside, by root-relative path: whatever is at `from`
    /// — a file, a symbolic link, or a whole tree — ends up at `to`, which
    /// must not already exist.
    ///
    /// For keeping a conflict's losing version under another name. A
    /// rename is the only way to do that for a directory without copying
    /// its content, and the losing side is the only place that content
    /// exists; not a synchronization primitive.
    fn rename(&mut self, _from: &str, _to: &str) -> Result<()> {
        anyhow::bail!("this endpoint cannot move entries")
    }

    /// Peering: presents the controller's lease on this endpoint's host.
    /// Only an agent-backed endpoint can hold one; a local endpoint is the
    /// controller's own machine, which never fences itself.
    fn lease(&mut self, _lease: &crate::peering::Lease) -> Result<crate::peering::LeaseAnswer> {
        anyhow::bail!("this endpoint does not take part in peering")
    }

    /// Peering: sends the leader's ancestor record for this session, and
    /// learns the generation the host's copy stands at afterwards.
    fn ancestor_record(&mut self, _generation: u64, _changes: &[Change]) -> Result<u64> {
        anyhow::bail!("this endpoint does not take part in peering")
    }

    /// Peering: replaces the host's ancestor copy for this session.
    fn ancestor_checkpoint(&mut self, _generation: u64, _ancestor: Option<&Node>) -> Result<u64> {
        anyhow::bail!("this endpoint does not take part in peering")
    }

    /// Peering: writes one of the files a follower needs on the host.
    fn put_peering_file(&mut self, _name: &str, _bytes: &[u8]) -> Result<()> {
        anyhow::bail!("this endpoint does not take part in peering")
    }

    /// Peering: what the host holds for this session.
    fn peering_state(&mut self) -> Result<crate::peering::State> {
        anyhow::bail!("this endpoint does not take part in peering")
    }

    /// A monotone measure of how much change this endpoint has recorded but
    /// not yet had consumed by a scan, used to tell a burst of writes from a
    /// single one. Two samples that agree mean nothing arrived in between.
    ///
    /// The default implementation cannot observe this and reports `None`,
    /// which callers read as "no evidence of an ongoing burst" — settling
    /// then falls back to its lower bound rather than its upper one.
    fn change_activity(&mut self) -> Option<ChangeActivity> {
        None
    }
}

/// How much unconsumed change an endpoint has recorded. Compared between
/// samples; the values themselves carry no meaning beyond inequality.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChangeActivity {
    /// Distinct paths recorded since the last scan consumed them.
    pub paths: usize,
    /// Whether the record was abandoned in favour of a full rescan, which
    /// is itself a change in state worth noticing.
    pub incomplete: bool,
}

/// What a watching endpoint raises to wake the session sleeping on it.
///
/// One per session, shared by both of its endpoints and by whatever
/// threads serve them, so that a change on either side — or the end of a
/// remote watch — wakes the same sleeper. A raise that arrives between a
/// poll and the sleep is not lost: it is held until the next wait
/// consumes it, which then returns at once.
#[derive(Default)]
pub struct WakeSignal {
    raised: Mutex<bool>,
    wake: Condvar,
}

impl WakeSignal {
    /// Wakes the sleeper, now or at its next wait.
    pub fn raise(&self) {
        let mut raised = self.raised.lock().unwrap_or_else(|e| e.into_inner());
        *raised = true;
        self.wake.notify_all();
    }

    /// Sleeps until raised or until `timeout` passes, consuming the raise.
    pub fn wait(&self, timeout: std::time::Duration) {
        let deadline = std::time::Instant::now() + timeout;
        let mut raised = self.raised.lock().unwrap_or_else(|e| e.into_inner());
        while !*raised {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return;
            }
            raised = self
                .wake
                .wait_timeout(raised, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        *raised = false;
    }
}
