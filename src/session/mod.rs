//! The session controller.
//!
//! A session synchronizes two endpoints through repeated cycles of scan →
//! reconcile → stage → transition → ancestor update, with the ancestor (the
//! last fully synchronized state) persisted between runs so that three-way
//! reconciliation can distinguish "changed on one side" from "changed on the
//! other". The controller is the hub: endpoints never communicate directly,
//! so either side may be local or remote.

use std::fs::{self, File};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use crate::endpoint::{Endpoint, FileRequest, StagingNeed, TransferFrame, TransitionOutcome};
pub mod ancestor;

use crate::tree::{
    apply, path_join, propagate_executability, reconcile, Change, Conflict, Content, Digest, Node,
    Problem, SyncMode,
};

/// The number of transfer frames pumped between endpoints per round trip
/// during staging. Larger batches amortize protocol round trips; each frame
/// is bounded by the rsync maximum data operation size.
const SUPPLY_BATCH_SIZE: usize = 16_384;

/// A synchronization halt requiring explicit user intervention, raised when
/// a cycle would perform a change so sweeping that it more likely reflects
/// an accident (or a vanished filesystem) than intent.
#[derive(Debug, thiserror::Error)]
pub enum SafetyHalt {
    /// A synchronization root was deleted in its entirety.
    #[error("halted: the synchronization root was deleted on one side; delete the other side manually (or recreate the root) and run again")]
    RootDeletion,
    /// A previously non-trivial synchronization root was emptied on exactly
    /// one side.
    #[error("halted: one side's synchronization root was emptied; propagate the deletion manually or restore the content, then run again")]
    RootEmptied,
    /// A directory below the root is present on both sides but empty on
    /// one, where the ancestor records a substantial tree — the signature
    /// of a filesystem that went away and left its mountpoint behind.
    #[error(
        "halted: {path} is empty on {side} but holds {entries} entries on the other side. \
         A mounted filesystem there is probably not mounted — check before doing anything. \
         If the emptying was deliberate, empty the other side too and it will resume"
    )]
    SubtreeEmptied {
        /// The directory's root-relative path.
        path: String,
        /// The side it is empty on.
        side: &'static str,
        /// How many entries the ancestor records beneath it.
        entries: usize,
    },
}

/// A report of one synchronization cycle.
#[derive(Debug, Default)]
pub struct CycleReport {
    /// The number of transitions applied to alpha.
    pub alpha_transitions: usize,
    /// The number of transitions applied to beta.
    pub beta_transitions: usize,
    /// Conflicts identified during reconciliation (left unresolved in safe
    /// modes).
    pub conflicts: Vec<Conflict>,
    /// Scan problems from alpha.
    pub alpha_scan_problems: Vec<Problem>,
    /// Scan problems from beta.
    pub beta_scan_problems: Vec<Problem>,
    /// Transition problems from alpha.
    pub alpha_transition_problems: Vec<Problem>,
    /// Transition problems from beta.
    pub beta_transition_problems: Vec<Problem>,
    /// Whether or not either endpoint reported missing staged content
    /// (warranting an immediate follow-up cycle).
    pub missing_staged_files: bool,
    /// Which content was confirmed absent from staging this cycle, by path
    /// and digest, across both endpoints. A follow-up that reports the same
    /// pair again is not looking at a changing file.
    pub missing_staged: Vec<crate::endpoint::FileRequest>,
}

impl CycleReport {
    /// Indicates whether or not the cycle applied any transitions.
    pub fn changed(&self) -> bool {
        self.alpha_transitions > 0 || self.beta_transitions > 0
    }

    /// Indicates that the cycle left nothing outstanding: nothing applied,
    /// nothing in conflict, nothing reported. Only after such a cycle can
    /// the next one treat unchanged scans as proof that the two sides are
    /// still synchronized.
    fn settled(&self) -> bool {
        !self.changed()
            && self.conflicts.is_empty()
            && !self.missing_staged_files
            && self.alpha_scan_problems.is_empty()
            && self.beta_scan_problems.is_empty()
            && self.alpha_transition_problems.is_empty()
            && self.beta_transition_problems.is_empty()
    }
}

/// A synchronization session between two endpoints.
pub struct Session {
    /// The alpha endpoint.
    alpha: Box<dyn Endpoint + Send>,
    /// The beta endpoint.
    beta: Box<dyn Endpoint + Send>,
    /// The synchronization mode.
    mode: SyncMode,
    /// The persisted ancestor path.
    ancestor_store: ancestor::AncestorStore,
    /// Exclusivity locks held for this session's lifetime (the endpoint
    /// pair lock, when the caller acquired one).
    held: Vec<EndpointPairLock>,
    /// Simulates a crash between the transitions and the achieved record.
    #[cfg(test)]
    pub(crate) fail_before_record: bool,
    /// When set, the next cycle's scans re-read every file's content —
    /// the verify verb's request.
    verify_next: bool,
    /// Whether either endpoint's tree lives on another machine, which is
    /// what makes intent records worth a sync on every mutating cycle
    /// (see [`ancestor::AncestorStore::intend`]).
    remote_involved: bool,
    /// The current ancestor hierarchy.
    ancestor: Option<Node>,
    /// Where this session announces what it is doing, when a supervisor is
    /// watching. A session with no observer announces into a handle nobody
    /// reads, which keeps the cycle free of conditionals.
    progress: Arc<crate::progress::Progress>,
    /// Whether the last cycle finished with the two sides synchronized and
    /// nothing outstanding — the precondition for skipping a cycle whose
    /// scans reproduce [`settled_alpha`](Self::settled_alpha) and
    /// [`settled_beta`](Self::settled_beta).
    quiesced: bool,
    /// Alpha's hierarchy as of the last quiesced cycle.
    settled_alpha: Option<Node>,
    /// Beta's hierarchy as of the last quiesced cycle.
    settled_beta: Option<Node>,
    /// The exclusive lock on the session state directory, held for the
    /// session's lifetime (released when the file closes on drop).
    _lock: SessionLock,
}

/// The error raised when a session's state directory is locked by another
/// live session. Callers that race for sessions legitimately (a supervisor
/// coexisting with another supervisor or a manual `sync`) can detect this
/// case by downcasting, and must treat the session's shared state — its
/// status file included — as owned by the lock holder.
#[derive(Debug, thiserror::Error)]
#[error(
    "another autobahn process is already synchronizing this session \
     (state directory {state_directory})"
)]
pub struct SessionLockHeld {
    /// The state directory that was found locked.
    pub state_directory: String,
}

/// Computes a stable session identifier from the two endpoint
/// specifications, used to isolate persisted state.
pub fn session_identifier(alpha_spec: &str, beta_spec: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(alpha_spec.as_bytes());
    hasher.update(&[0]);
    hasher.update(beta_spec.as_bytes());
    let digest = hasher.finalize();
    let bytes = digest.as_bytes();
    let mut identifier = String::with_capacity(32);
    for byte in &bytes[..16] {
        identifier.push_str(&format!("{byte:02x}"));
    }
    identifier
}

impl Session {
    /// Creates a session between the provided endpoints, acquiring the state
    /// directory's exclusive lock and loading any persisted ancestor (the
    /// directory is created if needed).
    pub fn new(
        alpha: Box<dyn Endpoint + Send>,
        beta: Box<dyn Endpoint + Send>,
        mode: SyncMode,
        state_directory: PathBuf,
    ) -> Result<Session> {
        Session::with_lock(alpha, beta, mode, SessionLock::acquire(state_directory)?)
    }

    /// Holds an exclusivity lock for this session's lifetime.
    pub fn hold(&mut self, lock: EndpointPairLock) {
        self.held.push(lock);
    }

    /// Opts the ancestor store into power-loss durability: every journal
    /// append syncs before the cycle is acknowledged.
    pub fn set_power_durability(&mut self, enabled: bool) {
        self.ancestor_store.set_power_durability(enabled);
    }

    /// Requests that the next cycle re-read every file's content instead
    /// of trusting recorded digests, making content changed without its
    /// metadata moving visible.
    pub fn request_verify(&mut self) {
        self.verify_next = true;
        // The quiesced shortcut would skip the very walk being requested.
        self.quiesced = false;
    }

    /// Creates a session between the provided endpoints under an
    /// already-held state lock. This exists so that callers with expensive
    /// endpoint construction (spawning SSH, handshaking with an agent) can
    /// acquire the lock *first* and discover a conflicting session before
    /// incurring any of that work or its remote side effects.
    pub fn with_lock(
        alpha: Box<dyn Endpoint + Send>,
        beta: Box<dyn Endpoint + Send>,
        mode: SyncMode,
        lock: SessionLock,
    ) -> Result<Session> {
        let ancestor_path = lock.state_directory().join("ancestor");
        let (mut ancestor_store, mut ancestor, unresolved) =
            ancestor::AncestorStore::open(&ancestor_path)?;
        if !unresolved.is_empty() {
            // A previous run crashed between announcing transitions and
            // recording what they achieved, so provenance at these paths is
            // unknown: the writes may or may not have landed. The honest
            // resolution is to *drop* the ancestor there and persist that
            // drop immediately — with no ancestor, a difference between the
            // sides surfaces as a conflict instead of one side silently
            // overwriting what might be a deliberate revert, and agreement
            // simply re-records itself. Persisting first keeps the
            // in-memory ancestor and the store's replay identical, and
            // consumes the intent so a later restart does not re-taint.
            let mut drops: Vec<Change> = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for path in unresolved {
                if !seen.insert(path.clone()) {
                    continue;
                }
                let exists = match &ancestor {
                    Some(root) if path.is_empty() => {
                        let _ = root;
                        true
                    }
                    Some(root) => {
                        let mut node = Some(root);
                        for part in path.split('/') {
                            node = node.and_then(|n| n.child(part));
                        }
                        node.is_some()
                    }
                    None => false,
                };
                if exists {
                    drops.push(Change {
                        path,
                        old: None,
                        new: None,
                    });
                }
            }
            if drops.is_empty() {
                // Nothing to drop, but the intent must still be consumed.
                ancestor_store.record(&[], ancestor.as_ref())?;
            } else {
                let tainted = apply(ancestor.as_ref(), &drops).map_err(|message| {
                    anyhow::anyhow!("unable to taint the ancestor: {message}")
                })?;
                ancestor_store.record(&drops, tainted.as_ref())?;
                ancestor = tainted;
            }
        }
        let remote_involved = alpha.is_remote() || beta.is_remote();
        Ok(Session {
            held: Vec::new(),
            #[cfg(test)]
            fail_before_record: false,
            verify_next: false,
            remote_involved,
            alpha,
            beta,
            mode,
            ancestor_store,
            ancestor,
            progress: Arc::default(),
            quiesced: false,
            settled_alpha: None,
            settled_beta: None,
            _lock: lock,
        })
    }

    /// Adopts the supervisor's progress record, so that what this session
    /// is doing is visible while it does it. Each endpoint gets the handle
    /// for its own side.
    pub fn set_progress(&mut self, progress: Arc<crate::progress::Progress>) {
        self.alpha.set_scan_progress(progress.alpha.clone());
        self.beta.set_scan_progress(progress.beta.clone());
        self.progress = progress;
    }

    /// Blocks until either endpoint signals that content may have changed,
    /// or the timeout elapses; returns whether a change was signaled.
    ///
    /// The wait is sliced between the endpoints rather than run in parallel:
    /// each endpoint is asked to watch for half a slice at a time, so a
    /// change on either side is noticed within one slice without any
    /// cross-thread cancellation machinery. Remote endpoints answer each
    /// slice with one lightweight round trip — far cheaper than the scan
    /// that pure interval polling would run instead.
    pub fn await_change(&mut self, timeout: std::time::Duration) -> Result<bool> {
        const SLICE: std::time::Duration = std::time::Duration::from_millis(250);
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            let half = remaining.min(SLICE) / 2;
            if self.alpha.await_change(half)? {
                return Ok(true);
            }
            if self.beta.await_change(half)? {
                return Ok(true);
            }
        }
    }

    /// Waits out a write burst before cycling, and no longer.
    ///
    /// A fixed settle delay is paid by every change equally, so an isolated
    /// edit — the common case for a person typing in an editor — waits the
    /// full window for a burst that never comes. That delay dominated
    /// measured propagation latency: essentially all of it was waiting, not
    /// working.
    ///
    /// Instead, sample how much change each endpoint has recorded, wait a
    /// short quiet slice, and sample again. Growth means writing is still in
    /// progress and is worth coalescing; two agreeing samples mean the burst
    /// is over and there is nothing left to wait for. `maximum` still caps
    /// the total, so a sustained burst settles exactly as before and the
    /// worst case is unchanged.
    ///
    /// An endpoint that cannot report activity (any remote one) contributes
    /// no evidence of a burst, which shortens the settle rather than
    /// lengthening it: cycles are idempotent, so the cost of cycling a
    /// little eagerly is work, never correctness.
    pub fn settle(&mut self, maximum: std::time::Duration, quiet: std::time::Duration) {
        let deadline = std::time::Instant::now() + maximum;
        let sample = |session: &mut Self| {
            (
                session.alpha.change_activity(),
                session.beta.change_activity(),
            )
        };
        let mut previous = sample(self);
        while std::time::Instant::now() < deadline {
            let slice = quiet.min(deadline.saturating_duration_since(std::time::Instant::now()));
            if slice.is_zero() {
                break;
            }
            std::thread::sleep(slice);
            let current = sample(self);
            // Neither side recorded anything new across the slice: whatever
            // triggered this settle has finished arriving.
            if current == previous {
                break;
            }
            previous = current;
        }
    }

    /// Runs one synchronization cycle: scan both endpoints, reconcile,
    /// stage and apply transitions, and update the persisted ancestor.
    ///
    /// When both scans reproduce the hierarchies of a previous cycle that
    /// left nothing outstanding, the cycle returns an empty report without
    /// reconciling — the two sides cannot have diverged since the cycle
    /// that established that state. An empty report therefore means "found
    /// nothing to do", whether or not the work of looking was performed.
    pub fn run_cycle(&mut self) -> Result<CycleReport> {
        let mut report = CycleReport::default();

        // Scan both endpoints in parallel.
        self.progress.enter(crate::progress::Phase::Scanning);
        let verify = std::mem::take(&mut self.verify_next);
        let (alpha_snapshot, beta_snapshot) = {
            let alpha = &mut self.alpha;
            let beta = &mut self.beta;
            std::thread::scope(|scope| {
                let alpha_scan = scope.spawn(move || {
                    if verify {
                        alpha.scan_verified()
                    } else {
                        alpha.scan()
                    }
                });
                let beta_result = if verify {
                    beta.scan_verified()
                } else {
                    beta.scan()
                };
                let alpha_result = alpha_scan.join().expect("scan thread panicked");
                (alpha_result, beta_result)
            })
        };
        let alpha_snapshot = alpha_snapshot.context("alpha scan failed")?;
        let beta_snapshot = beta_snapshot.context("beta scan failed")?;

        // If both sides produced the very same hierarchies as the last cycle
        // — the same storage, not merely equal content — then nothing can
        // have changed since that cycle reconciled them, and reconciling
        // again would reach the same conclusion by the same full-tree walk.
        // The cycle that established this state is what makes the shortcut
        // sound: it is only taken after a cycle that had nothing left to do,
        // so "the same as last time" means "still synchronized".
        if self.quiesced
            && crate::tree::nodes_share_storage(
                self.settled_alpha.as_ref(),
                alpha_snapshot.root.as_ref(),
            )
            && crate::tree::nodes_share_storage(
                self.settled_beta.as_ref(),
                beta_snapshot.root.as_ref(),
            )
        {
            return Ok(report);
        }

        if let Some(root) = &alpha_snapshot.root {
            report.alpha_scan_problems = root.problems();
        }
        if let Some(root) = &beta_snapshot.root {
            report.beta_scan_problems = root.problems();
        }

        // On a side whose filesystem can't preserve executability bits, the
        // scanned bits are noise; replace them from trusted references — the
        // other side (when it preserves bits) for byte-identical files, and
        // the ancestor otherwise — so reconciliation sees only real content
        // changes rather than phantom permission churn.
        let alpha_root = if alpha_snapshot.preserves_executability {
            alpha_snapshot.root.clone()
        } else {
            let peer = beta_snapshot
                .preserves_executability
                .then_some(beta_snapshot.root.as_ref())
                .flatten();
            propagate_executability(self.ancestor.as_ref(), peer, alpha_snapshot.root.as_ref())
        };
        let beta_root = if beta_snapshot.preserves_executability {
            beta_snapshot.root.clone()
        } else {
            let peer = alpha_snapshot
                .preserves_executability
                .then_some(alpha_snapshot.root.as_ref())
                .flatten();
            propagate_executability(self.ancestor.as_ref(), peer, beta_snapshot.root.as_ref())
        };

        // Safety: if the ancestor root was a directory with non-trivial
        // content and exactly one side now presents an empty (or absent)
        // root, then halt rather than propagate what is more likely an
        // unmounted or wiped filesystem than an intentional mass deletion.
        if one_side_emptied_root(
            self.ancestor.as_ref(),
            alpha_root.as_ref(),
            beta_root.as_ref(),
        ) {
            bail!(SafetyHalt::RootEmptied);
        }

        // Reconcile.
        self.progress.enter(crate::progress::Phase::Reconciling);
        let reconciliation = reconcile(
            self.ancestor.as_ref(),
            alpha_root.as_ref(),
            beta_root.as_ref(),
            self.mode,
        );
        if let Some(emptied) = &reconciliation.emptied_subtree {
            bail!(SafetyHalt::SubtreeEmptied {
                path: emptied.path.clone(),
                side: emptied.side,
                entries: emptied.entries,
            });
        }
        report.conflicts = reconciliation.conflicts;

        // Safety: refuse to propagate a root deletion.
        let contains_root_deletion = reconciliation
            .alpha_transitions
            .iter()
            .chain(reconciliation.beta_transitions.iter())
            .any(Change::is_root_deletion);
        if contains_root_deletion {
            bail!(SafetyHalt::RootDeletion);
        }

        // What this cycle is about to touch, announced in the journal
        // *after* staging but before the first transition. If the process
        // dies between the announcement and the achieved record, the next
        // run finds the intent unresolved and drops these paths'
        // provenance — a crash mid-cycle costs surfaced conflicts, never a
        // silent overwrite of a revert made while the tool was down.
        // Staging is deliberately outside the announced window: it mutates
        // neither tree, it is the longest phase of a large cycle, and a
        // crash there must recover as the clean propagation it still is
        // rather than as conflict noise.
        let intended: Vec<String> = reconciliation
            .alpha_transitions
            .iter()
            .chain(reconciliation.beta_transitions.iter())
            .map(|change| change.path.clone())
            .collect();
        let mut intent_recorded = false;

        // Stage and transition each side. Content flowing to beta is
        // supplied by alpha and vice versa.
        let beta_outcome = if reconciliation.beta_transitions.is_empty() {
            None
        } else {
            stage(
                self.alpha.as_mut(),
                self.beta.as_mut(),
                &reconciliation.beta_transitions,
                &self.progress,
            )?;
            if !intent_recorded {
                self.ancestor_store
                    .intend(&intended, self.remote_involved)?;
                intent_recorded = true;
            }
            self.progress
                .begin_applying(reconciliation.beta_transitions.len() as u64);
            let outcome = self
                .beta
                .transition(reconciliation.beta_transitions.clone())
                .context("beta transition failed")?;
            self.progress
                .applied_reached(reconciliation.beta_transitions.len() as u64);
            Some(outcome)
        };
        let alpha_outcome = if reconciliation.alpha_transitions.is_empty() {
            None
        } else {
            stage(
                self.beta.as_mut(),
                self.alpha.as_mut(),
                &reconciliation.alpha_transitions,
                &self.progress,
            )?;
            if !intent_recorded {
                self.ancestor_store
                    .intend(&intended, self.remote_involved)?;
                intent_recorded = true;
            }
            self.progress
                .begin_applying(reconciliation.alpha_transitions.len() as u64);
            let outcome = self
                .alpha
                .transition(reconciliation.alpha_transitions.clone())
                .context("alpha transition failed")?;
            self.progress
                .applied_reached(reconciliation.alpha_transitions.len() as u64);
            Some(outcome)
        };
        let _ = intent_recorded;

        #[cfg(test)]
        if self.fail_before_record {
            bail!("test seam: crashed after transitions, before the achieved record");
        }

        // Fold transition results into ancestor changes: each transition's
        // achieved content becomes the ancestor's new content at that path.
        let mut ancestor_changes = reconciliation.ancestor_changes;
        let mut fold = |transitions: &[Change], outcome: &TransitionOutcome| {
            ancestor_changes.extend(crate::endpoint::achieved_changes(transitions, outcome));
        };
        if let Some(outcome) = &beta_outcome {
            fold(&reconciliation.beta_transitions, outcome);
            report.beta_transitions = reconciliation.beta_transitions.len();
            report.beta_transition_problems = outcome.problems.clone();
            report.missing_staged_files |= outcome.missing_staged_files;
            report
                .missing_staged
                .extend(outcome.missing_staged.iter().cloned());
        }
        if let Some(outcome) = &alpha_outcome {
            fold(&reconciliation.alpha_transitions, outcome);
            report.alpha_transitions = reconciliation.alpha_transitions.len();
            report.alpha_transition_problems = outcome.problems.clone();
            report.missing_staged_files |= outcome.missing_staged_files;
            report
                .missing_staged
                .extend(outcome.missing_staged.iter().cloned());
        }

        // Apply the ancestor changes, validate the result (the ancestor must
        // only ever contain synchronizable content — this is the safety net
        // against reconciliation defects reaching disk), and persist it.
        if !ancestor_changes.is_empty() {
            self.progress.enter(crate::progress::Phase::Saving);
            let new_ancestor = apply(self.ancestor.as_ref(), &ancestor_changes)
                .map_err(|message| anyhow::anyhow!("ancestor update failed: {message}"))?;
            if let Some(root) = &new_ancestor {
                // Validated against the ancestor it was built from, which
                // was itself validated before it was installed. apply()
                // keeps the storage of every subtree it did not touch, so
                // this walks the changed paths rather than the hierarchy.
                root.validate_against(self.ancestor.as_ref(), true)
                    .map_err(|message| anyhow::anyhow!("new ancestor is invalid: {message}"))?;
            }
            // The ancestor is written synchronously, and a failure fails
            // the cycle. Unlike the scan caches — which only ever save work
            // — the ancestor carries *provenance*: it is what distinguishes
            // "this side changed" from "the other side did". A stale
            // ancestor is therefore not merely out of date, it is
            // misleading, and one case makes that concrete: content
            // deliberately reverted to an earlier state is indistinguishable
            // from content that never changed. Reconciled against a stale
            // ancestor, that revert reads as "unchanged" while the peer
            // reads as "modified", and the peer's content silently
            // overwrites the revert — in every mode. Deferring this write
            // would widen the window in which that can happen from the gap
            // between two statements to however long encoding and writing
            // the whole hierarchy takes: ample time for someone to make
            // exactly that edit.
            self.ancestor_store
                .record(&ancestor_changes, new_ancestor.as_ref())?;
            self.ancestor = new_ancestor;
        }

        // A cycle that applied nothing, hit no conflicts, and saw no
        // problems leaves the two sides synchronized as scanned. Recording
        // that storage lets the next cycle recognize an untouched pair
        // without walking either tree.
        self.quiesced = report.settled();
        if self.quiesced {
            self.settled_alpha = alpha_snapshot.root.clone();
            self.settled_beta = beta_snapshot.root.clone();
        } else {
            self.settled_alpha = None;
            self.settled_beta = None;
        }

        Ok(report)
    }
}

/// Collects the file content dependencies of a transition list: the paths
/// and digests of every file in the transitions' target content, excluding
/// files whose transition only changes executability (which transitions
/// perform in place without staged content).
pub fn transition_dependencies(transitions: &[Change]) -> Vec<FileRequest> {
    fn collect(path: &str, node: &Node, requests: &mut Vec<FileRequest>) {
        match &node.content {
            Content::File { digest, .. } => requests.push(FileRequest {
                path: path.to_owned(),
                digest: *digest,
            }),
            Content::Directory(children) => {
                for child in children.iter() {
                    collect(&path_join(path, &child.name), child, requests);
                }
            }
            _ => {}
        }
    }
    let mut requests = Vec::new();
    for transition in transitions {
        if let (
            Some(Node {
                content:
                    Content::File {
                        digest: old_digest, ..
                    },
                ..
            }),
            Some(Node {
                content:
                    Content::File {
                        digest: new_digest, ..
                    },
                ..
            }),
        ) = (&transition.old, &transition.new)
        {
            if old_digest == new_digest {
                continue;
            }
        }
        if let Some(new) = &transition.new {
            collect(&transition.path, new, &mut requests);
        }
    }
    requests
}

/// Stages the content needed by `transitions` onto the destination endpoint,
/// supplying it from the source endpoint in streamed batches.
/// The total size of the content a transfer is about to move.
///
/// A staging need names a path and a digest, not a length; the lengths are
/// on the nodes the transitions carry, so they are collected from there and
/// matched by digest — which is what a need is addressed by.
fn staged_bytes(transitions: &[Change], needs: &[StagingNeed]) -> u64 {
    fn collect(node: &Node, sizes: &mut std::collections::HashMap<Digest, u64>) {
        match &node.content {
            Content::File {
                digest, metadata, ..
            } => {
                sizes.insert(*digest, metadata.size);
            }
            Content::Directory(children) => {
                for child in children.iter() {
                    collect(child, sizes);
                }
            }
            _ => {}
        }
    }
    let mut sizes = std::collections::HashMap::new();
    for change in transitions {
        if let Some(node) = &change.new {
            collect(node, &mut sizes);
        }
    }
    needs
        .iter()
        .map(|need| sizes.get(&need.request.digest).copied().unwrap_or(0))
        .sum()
}

fn stage(
    source: &mut dyn Endpoint,
    destination: &mut dyn Endpoint,
    transitions: &[Change],
    progress: &crate::progress::Progress,
) -> Result<()> {
    let requests = transition_dependencies(transitions);
    if requests.is_empty() {
        return Ok(());
    }
    let needs = destination
        .stage_begin(requests)
        .context("unable to begin staging")?;
    if needs.is_empty() {
        return Ok(());
    }
    // What this transfer will move, announced before it starts so that its
    // progress can be read as a fraction rather than as a total that only
    // grows. The sizes come from the transitions themselves: a need names
    // a path and a digest, not a length.
    progress.begin_staging(needs.len() as u64, staged_bytes(transitions, &needs));
    source
        .supply_open(needs)
        .context("unable to open supply stream")?;
    // The pull and push halves run on separate threads with a small bounded
    // buffer between them, so the source's reads overlap the destination's
    // writes even when both endpoints share this process. (A remote
    // destination additionally keeps its own window of pushed batches in
    // flight, and stage_finish drains those acknowledgements.)
    let (batches, staged) = std::sync::mpsc::sync_channel::<Vec<TransferFrame>>(1);
    std::thread::scope(|scope| {
        let pusher = scope.spawn(move || -> Result<()> {
            while let Ok(frames) = staged.recv() {
                // Counted in this half rather than in the pulling one. The
                // channel between them holds a single batch, so a pass
                // over the frames before the send sits squarely between
                // the source's read and the destination's write, with
                // nothing to overlap it; here it runs while the puller is
                // already fetching the next batch. (Measured: two passes
                // before the send cost 2.5 ms of p50.)
                let mut files = 0u64;
                let mut bytes = 0u64;
                for frame in frames.iter() {
                    match frame {
                        TransferFrame::EndOfFile { .. } => files += 1,
                        TransferFrame::Op(crate::rsync::Op::Data(data)) => {
                            bytes += data.len() as u64
                        }
                        TransferFrame::Op(_) => {}
                    }
                }
                destination
                    .stage_push_nowait(frames)
                    .context("unable to push file content")?;
                progress.staged(files, bytes);
            }
            destination
                .stage_finish()
                .context("unable to complete staging")
        });
        let mut pull_error = None;
        loop {
            match source.supply_pull(SUPPLY_BATCH_SIZE) {
                Ok(frames) => {
                    // Emptiness signals exhaustion; a send failure means the
                    // pusher stopped early, and its error is reported below.
                    if frames.is_empty() || batches.send(frames).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    pull_error = Some(error.context("unable to pull file content"));
                    break;
                }
            }
        }
        // Closing the channel lets the pusher drain and finish.
        drop(batches);
        let pushed = pusher.join().expect("the staging pusher never panics");
        match pull_error {
            Some(error) => Err(error),
            None => pushed,
        }
    })
}

/// Detects the emptied-root condition at the root itself: the ancestor was
/// non-trivial, and exactly one side now presents an absent or childless
/// root while the other retains content. Emptied directories *below* the
/// root are detected by reconciliation during the walk it already performs
/// (`Reconciliation::emptied_subtree`); a separate whole-tree pass here
/// measured at twenty milliseconds per cycle on a sixty-thousand-entry
/// tree, dominating the latency of every edit. The ancestor count — the
/// only expensive part — runs lazily, only when the rare one-side-empty
/// trigger fires.
fn one_side_emptied_root(
    ancestor: Option<&Node>,
    alpha: Option<&Node>,
    beta: Option<&Node>,
) -> bool {
    let ancestor = match ancestor {
        Some(node) if matches!(node.content, Content::Directory(_)) => node,
        _ => return false,
    };
    let gone = |side: Option<&Node>| match side {
        None => true,
        Some(node) => node.children().is_empty(),
    };
    if gone(alpha) == gone(beta) {
        return false;
    }
    fn entries_below(node: &Node) -> usize {
        node.children()
            .iter()
            .map(|child| 1 + entries_below(child))
            .sum()
    }
    entries_below(ancestor) >= 2
}

/// An exclusive lock on a *pair of endpoint identities*, machine-wide for
/// this user, independent of any `--state-root` or `--state-dir` override.
///
/// The session lock guards a state directory, which is airtight only while
/// every process selects the same state root. The supported overrides break
/// that quietly: two state directories each hold their own ancestor for the
/// same pair of trees, the two sessions reconcile from contradictory
/// provenance, and content silently swaps sides before one of the versions
/// is lost. This lock keys on what is actually being synchronized rather
/// than on where its state lives. The pair is unordered, so the same two
/// trees in opposite directions conflict as well; a fan-out (one alpha,
/// many betas) and a relay (one's beta, another's alpha) key differently
/// and stay legal.
pub struct EndpointPairLock {
    _lock: SessionLock,
}

impl EndpointPairLock {
    /// Acquires the pair lock for two endpoint identities.
    pub fn acquire(alpha_identity: &str, beta_identity: &str) -> Result<EndpointPairLock> {
        // The *default* state root, deliberately — this directory must not
        // move with the overrides whose divergence it exists to catch.
        let root = crate::paths::default_state_root()?.join("endpoint-locks");
        EndpointPairLock::acquire_in(&root, alpha_identity, beta_identity)
    }

    /// The lock directory name for a pair of endpoint identities, in either
    /// order. Exposed so `clean` can tell which lock directories belong to a
    /// configured pair and which are left over from pairs that no longer
    /// exist.
    pub fn key(alpha_identity: &str, beta_identity: &str) -> String {
        let (first, second) = if alpha_identity <= beta_identity {
            (alpha_identity, beta_identity)
        } else {
            (beta_identity, alpha_identity)
        };
        let mut hasher = blake3::Hasher::new();
        hasher.update(first.as_bytes());
        hasher.update(&[0]);
        hasher.update(second.as_bytes());
        let digest = hasher.finalize();
        let mut key = String::with_capacity(32);
        for byte in &digest.as_bytes()[..16] {
            key.push_str(&format!("{byte:02x}"));
        }
        key
    }

    /// Acquires the pair lock under an explicit lock root (the seam tests
    /// use so the mechanism can be exercised without touching the user's
    /// real home directory).
    fn acquire_in(
        root: &Path,
        alpha_identity: &str,
        beta_identity: &str,
    ) -> Result<EndpointPairLock> {
        let (first, second) = if alpha_identity <= beta_identity {
            (alpha_identity, beta_identity)
        } else {
            (beta_identity, alpha_identity)
        };
        let directory = root.join(EndpointPairLock::key(alpha_identity, beta_identity));
        let lock = SessionLock::acquire(directory).map_err(|error| {
            anyhow::anyhow!(
                "another session is already synchronizing these roots \
                 ({first} and {second}), possibly under a different state \
                 directory: {error:#}"
            )
        })?;
        Ok(EndpointPairLock { _lock: lock })
    }
}

/// How long lock acquisition keeps retrying before concluding the session
/// genuinely belongs to someone else. See [`SessionLock::acquire`].
const LOCK_ACQUISITION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// An exclusive advisory lock on a session state directory, held for the
/// owning session's lifetime and released when dropped.
///
/// Two sessions over the same state would race each other's ancestor,
/// staging, and status writes — with different modes, destructively. The
/// lock makes that structurally impossible whether the second session comes
/// from the same process (a duplicated configuration) or another one (two
/// supervisors, or a supervisor plus a manual `sync`). Advisory `flock` is
/// exactly right here: every path into the state directory goes through
/// this lock, and the lock dies with its process, so a crash can never
/// leave a stale lock behind.
pub struct SessionLock {
    /// The locked state directory.
    state_directory: PathBuf,
    /// The open, locked file (the lock releases when it closes).
    _file: File,
}

impl SessionLock {
    /// Acquires the lock on a state directory, creating the directory if
    /// needed. A directory locked by a live session yields a
    /// [`SessionLockHeld`] error (downcastable through the chain).
    ///
    /// Acquisition retries briefly before reporting a conflict: releasing a
    /// `flock` is delayed if a concurrently forked child (an agent or SSH
    /// subprocess spawned by another session's worker) inherited the lock
    /// file descriptor in the instant between fork and exec — the
    /// descriptors are close-on-exec, so the delay is microseconds, but a
    /// back-to-back release-and-reacquire can land inside it. A lock still
    /// held after the timeout is a real concurrent session, not that window.
    pub fn acquire(state_directory: PathBuf) -> Result<SessionLock> {
        fs::create_dir_all(&state_directory).with_context(|| {
            format!(
                "unable to create session state directory {}",
                state_directory.display()
            )
        })?;
        let path = state_directory.join("lock");
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("unable to open session lock {}", path.display()))?;
        let deadline = std::time::Instant::now() + LOCK_ACQUISITION_TIMEOUT;
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(SessionLock {
                    state_directory,
                    _file: file,
                });
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::WouldBlock {
                return Err(error)
                    .with_context(|| format!("unable to lock session state {}", path.display()));
            }
            if std::time::Instant::now() >= deadline {
                return Err(SessionLockHeld {
                    state_directory: state_directory.display().to_string(),
                }
                .into());
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    /// Returns the locked state directory.
    pub fn state_directory(&self) -> &Path {
        &self.state_directory
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Digest, FileMetadata};

    fn file(name: &str, digest_byte: u8) -> Node {
        Node {
            name: name.into(),
            content: Content::File {
                digest: [digest_byte; 32] as Digest,
                executable: false,
                metadata: FileMetadata::default(),
            },
        }
    }

    #[test]
    fn transition_dependencies_collects_files_and_skips_executability_changes() {
        let mut executable_only = file("a", 1);
        if let Content::File { executable, .. } = &mut executable_only.content {
            *executable = true;
        }
        let transitions = vec![
            Change {
                path: "a".into(),
                old: Some(file("a", 1)),
                new: Some(executable_only),
            },
            Change {
                path: "d".into(),
                old: None,
                new: Some(Node::directory("d", vec![file("x", 2), file("y", 3)])),
            },
        ];
        let requests = transition_dependencies(&transitions);
        let paths: Vec<&str> = requests.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["d/x", "d/y"]);
    }

    #[test]
    fn emptied_root_detection() {
        let ancestor = Node::directory("", vec![file("a", 1), file("b", 2)]);
        let empty = Node::directory("", vec![]);
        assert!(one_side_emptied_root(
            Some(&ancestor),
            Some(&empty),
            Some(&ancestor)
        ));
        assert!(!one_side_emptied_root(
            Some(&ancestor),
            Some(&empty),
            Some(&empty)
        ));
        assert!(!one_side_emptied_root(
            Some(&ancestor),
            Some(&ancestor),
            Some(&ancestor)
        ));
        // A root holding everything under one directory is exactly the
        // shape the original guard missed: reproduced deleting beta's
        // whole copy before the guard counted recursive entries.
        let single = Node::directory(
            "",
            vec![Node::directory(
                "data",
                (1..=5).map(|i| file(&format!("f{i}"), i)).collect(),
            )],
        );
        assert!(one_side_emptied_root(
            Some(&single),
            Some(&empty),
            Some(&single)
        ));
        // An absent root gets the same protection as an emptied one.
        assert!(one_side_emptied_root(Some(&single), None, Some(&single)));
        // A truly trivial tree still empties freely at the root.
        let trivial = Node::directory("", vec![file("a", 1)]);
        assert!(!one_side_emptied_root(
            Some(&trivial),
            Some(&empty),
            Some(&trivial)
        ));
    }

    /// Emptied directories *below* the root are reconciliation's to spot,
    /// during the walk it already performs.
    #[test]
    fn emptied_subtree_detection_rides_reconciliation() {
        let big_data =
            |n: u8| Node::directory("data", (1..=n).map(|i| file(&format!("f{i}"), i)).collect());
        let with_mount = Node::directory("", vec![file("readme", 9), big_data(9)]);
        let mount_emptied =
            Node::directory("", vec![file("readme", 9), Node::directory("data", vec![])]);
        // A vanished mount: data/ exists but is empty on one side only.
        let result = crate::tree::reconcile(
            Some(&with_mount),
            Some(&mount_emptied),
            Some(&with_mount),
            SyncMode::TwoWaySafe,
        );
        let emptied = result.emptied_subtree.expect("an emptied mount halts");
        assert_eq!(emptied.path, "data");
        assert_eq!(emptied.side, "alpha", "the side it is empty on is named");
        assert_eq!(emptied.entries, 9, "and how much the ancestor recorded");

        // A directory that vanished *entirely* is a different signature
        // with a different cause, and it propagates.
        //
        // A mountpoint belongs to the parent filesystem, so a filesystem
        // going away leaves the directory behind, empty — the case above.
        // A directory that is gone was removed by someone, and removing a
        // directory is something people do. Guarding this shape too was
        // tried, and it cost a halt on every deliberate deletion to catch
        // the minority of mountpoints that are removed on eject.
        let mount_deleted = Node::directory("", vec![file("readme", 9)]);
        let result = crate::tree::reconcile(
            Some(&with_mount),
            Some(&mount_deleted),
            Some(&with_mount),
            SyncMode::TwoWaySafe,
        );
        assert!(
            result.emptied_subtree.is_none(),
            "a deleted directory propagates rather than halting"
        );
        assert!(
            !result.beta_transitions.is_empty(),
            "and the deletion actually reaches the other side"
        );

        // A small directory's outright deletion still propagates without
        // ceremony.
        let with_small_data = Node::directory("", vec![file("readme", 9), big_data(3)]);
        let small_deleted = Node::directory("", vec![file("readme", 9)]);
        let result = crate::tree::reconcile(
            Some(&with_small_data),
            Some(&small_deleted),
            Some(&with_small_data),
            SyncMode::TwoWaySafe,
        );
        assert!(result.emptied_subtree.is_none());

        // Small directories may be emptied without ceremony.
        let with_small = Node::directory(
            "",
            vec![
                file("readme", 9),
                Node::directory("queue", vec![file("job", 1), file("job2", 2)]),
            ],
        );
        let small_emptied = Node::directory(
            "",
            vec![file("readme", 9), Node::directory("queue", vec![])],
        );
        let result = crate::tree::reconcile(
            Some(&with_small),
            Some(&small_emptied),
            Some(&with_small),
            SyncMode::TwoWaySafe,
        );
        assert!(result.emptied_subtree.is_none());
    }

    /// `clean` decides which lock directories are live by recomputing their
    /// names from the configuration, so the name `acquire` uses and the name
    /// `key` reports must be the same function of the same inputs — in
    /// either order, since a pair has no direction.
    #[test]
    fn the_pair_lock_key_names_the_directory_acquire_uses() {
        let root = tempfile::tempdir().unwrap();
        let _held = EndpointPairLock::acquire_in(root.path(), "/x", "/y").unwrap();
        let expected = root.path().join(EndpointPairLock::key("/y", "/x"));
        assert!(expected.is_dir(), "acquire and key disagree on the name");
        assert_eq!(
            EndpointPairLock::key("/x", "/y"),
            EndpointPairLock::key("/y", "/x")
        );
    }

    #[test]
    fn the_pair_lock_is_unordered_and_pair_scoped() {
        let keep = tempfile::tempdir().unwrap();
        let root = keep.path().join("locks");
        let held = EndpointPairLock::acquire_in(&root, "/tree/a", "/tree/b")
            .expect("first acquisition succeeds");
        // The same pair, reversed, is the same two trees.
        assert!(
            EndpointPairLock::acquire_in(&root, "/tree/b", "/tree/a").is_err(),
            "the reversed pair must conflict"
        );
        // Sharing one endpoint is a different pair.
        EndpointPairLock::acquire_in(&root, "/tree/a", "/tree/c")
            .expect("a fan-out pair must not conflict");
        drop(held);
        EndpointPairLock::acquire_in(&root, "/tree/b", "/tree/a")
            .expect("the pair is free once released");
    }

    #[test]
    fn ancestor_persistence_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ancestor");
        let ancestor = Node::directory("", vec![file("a", 1)]);

        let (mut store, empty, _) = ancestor::AncestorStore::open(&path).unwrap();
        assert!(empty.is_none());
        let change = Change {
            path: String::new(),
            old: None,
            new: Some(ancestor.clone()),
        };
        store.record(&[change], Some(&ancestor)).unwrap();

        let (mut store, loaded, _) = ancestor::AncestorStore::open(&path).unwrap();
        assert!(loaded.unwrap().content_equal(&ancestor, true));

        let deletion = Change {
            path: String::new(),
            old: Some(ancestor),
            new: None,
        };
        store.record(&[deletion], None).unwrap();
        let (_, loaded, _) = ancestor::AncestorStore::open(&path).unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn corrupt_ancestor_is_an_error_not_a_reset() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ancestor");
        fs::write(&path, b"garbage").unwrap();
        assert!(ancestor::AncestorStore::open(&path).is_err());
    }

    /// An endpoint that replays a queue of snapshots (repeating the last)
    /// and acknowledges transitions as fully achieved.
    struct ScriptedEndpoint {
        snapshots: std::collections::VecDeque<crate::tree::Snapshot>,
        last: Option<crate::tree::Snapshot>,
    }

    impl ScriptedEndpoint {
        fn new(snapshots: Vec<crate::tree::Snapshot>) -> ScriptedEndpoint {
            ScriptedEndpoint {
                snapshots: snapshots.into(),
                last: None,
            }
        }
    }

    impl Endpoint for ScriptedEndpoint {
        fn scan(&mut self) -> Result<crate::tree::Snapshot> {
            if let Some(next) = self.snapshots.pop_front() {
                self.last = Some(next);
            }
            Ok(self.last.clone().expect("a snapshot should be scripted"))
        }

        fn stage_begin(
            &mut self,
            _files: Vec<FileRequest>,
        ) -> Result<Vec<crate::endpoint::StagingNeed>> {
            Ok(Vec::new())
        }

        fn supply_open(&mut self, _needs: Vec<crate::endpoint::StagingNeed>) -> Result<()> {
            unreachable!("no staging needs are ever reported")
        }

        fn supply_pull(
            &mut self,
            _max_frames: usize,
        ) -> Result<Vec<crate::endpoint::TransferFrame>> {
            unreachable!("no staging needs are ever reported")
        }

        fn stage_push(&mut self, _frames: Vec<crate::endpoint::TransferFrame>) -> Result<()> {
            unreachable!("no staging needs are ever reported")
        }

        fn transition(&mut self, transitions: Vec<Change>) -> Result<TransitionOutcome> {
            Ok(TransitionOutcome {
                results: transitions.iter().map(|t| t.new.clone()).collect(),
                problems: Vec::new(),
                missing_staged_files: false,
                missing_staged: Vec::new(),
            })
        }
    }

    /// A scripted endpoint that counts how often it was asked to
    /// transition, so a test can tell a skipped cycle from a cycle that ran
    /// and found nothing.
    struct CountingEndpoint {
        inner: ScriptedEndpoint,
        transitions: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Endpoint for CountingEndpoint {
        fn scan(&mut self) -> Result<crate::tree::Snapshot> {
            self.inner.scan()
        }
        fn stage_begin(
            &mut self,
            files: Vec<FileRequest>,
        ) -> Result<Vec<crate::endpoint::StagingNeed>> {
            self.inner.stage_begin(files)
        }
        fn supply_open(&mut self, needs: Vec<crate::endpoint::StagingNeed>) -> Result<()> {
            self.inner.supply_open(needs)
        }
        fn supply_pull(
            &mut self,
            max_frames: usize,
        ) -> Result<Vec<crate::endpoint::TransferFrame>> {
            self.inner.supply_pull(max_frames)
        }
        fn stage_push(&mut self, frames: Vec<crate::endpoint::TransferFrame>) -> Result<()> {
            self.inner.stage_push(frames)
        }
        fn transition(&mut self, transitions: Vec<Change>) -> Result<TransitionOutcome> {
            self.transitions
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.transition(transitions)
        }
    }

    /// Builds a snapshot around a root, sharing the root's storage across
    /// clones — which is what an unchanged rescan produces.
    /// The intent record's whole purpose: a crash between the transitions
    /// and the achieved record must never let the stale ancestor authorize
    /// overwriting a revert made while the tool was down. The recovered
    /// session drops provenance for the intended paths, so the difference
    /// surfaces as a conflict and neither side is touched.
    #[test]
    fn a_crash_between_transition_and_record_ends_in_conflict_not_overwrite() {
        let keep = tempfile::tempdir().unwrap();
        let state = keep.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let old_content = || Node::directory("", vec![file("target", 1)]);
        let new_content = || Node::directory("", vec![file("target", 2)]);

        // Cycle one converges on the old content; cycle two propagates the
        // new content to beta and then "crashes" at the seam.
        {
            let alpha =
                ScriptedEndpoint::new(vec![scripted(old_content()), scripted(new_content())]);
            let beta = ScriptedEndpoint::new(vec![scripted(old_content())]);
            let mut session = Session::new(
                Box::new(alpha),
                Box::new(beta),
                SyncMode::TwoWaySafe,
                state.clone(),
            )
            .unwrap();
            session.run_cycle().expect("first cycle converges");
            session.fail_before_record = true;
            let error = session.run_cycle().expect_err("the seam must fire");
            assert!(format!("{error:#}").contains("test seam"), "{error:#}");
        }

        // While the tool was down, the user deliberately reverted alpha.
        // Beta holds the propagated new content (the transition landed).
        let transitions = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let alpha = CountingEndpoint {
            inner: ScriptedEndpoint::new(vec![scripted(old_content())]),
            transitions: std::sync::Arc::clone(&transitions),
        };
        let beta = ScriptedEndpoint::new(vec![scripted(new_content())]);
        let mut session =
            Session::new(Box::new(alpha), Box::new(beta), SyncMode::TwoWaySafe, state).unwrap();
        let report = session.run_cycle().expect("the recovery cycle runs");
        assert!(
            !report.conflicts.is_empty(),
            "unknown provenance must surface as a conflict"
        );
        assert_eq!(
            transitions.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the revert must not be overwritten: without the intent record, \
             the stale ancestor read alpha as unchanged and beta as modified \
             and pushed the new content back over the revert"
        );
    }

    fn scripted(root: Node) -> crate::tree::Snapshot {
        crate::tree::Snapshot {
            root: Some(root),
            preserves_executability: true,
            ..crate::tree::Snapshot::default()
        }
    }

    #[test]
    fn an_unchanged_pair_skips_reconciliation_only_after_a_settled_cycle() {
        let alpha_root = Node::directory("", vec![file("shared", 1)]);
        let beta_root = Node::directory("", vec![file("shared", 1)]);
        // Every scan reproduces the same storage, as an unchanged rescan
        // does. The first cycle must still reconcile — nothing has settled
        // yet — and later ones may skip.
        let alpha = ScriptedEndpoint::new(vec![scripted(alpha_root)]);
        let beta = ScriptedEndpoint::new(vec![scripted(beta_root)]);
        let state = tempfile::tempdir().unwrap();
        let mut session = Session::new(
            Box::new(alpha),
            Box::new(beta),
            SyncMode::TwoWaySafe,
            state.path().to_path_buf(),
        )
        .expect("session should be creatable");

        for cycle in 0..4 {
            let report = session.run_cycle().expect("cycle should succeed");
            assert!(!report.changed(), "cycle {cycle} applied transitions");
            assert!(report.conflicts.is_empty(), "cycle {cycle} conflicted");
        }
        // The shortcut engaged after the first cycle established the state.
        assert!(session.quiesced);
    }

    #[test]
    fn a_change_after_a_settled_cycle_is_still_propagated() {
        let settled = Node::directory("", vec![file("shared", 1)]);
        let edited = Node::directory("", vec![file("shared", 2)]);
        // Two identical scans settle the session; the third carries a real
        // change on alpha, which must not be skipped.
        let alpha = ScriptedEndpoint::new(vec![
            scripted(settled.clone()),
            scripted(settled.clone()),
            scripted(edited),
        ]);
        let applied = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let beta = CountingEndpoint {
            inner: ScriptedEndpoint::new(vec![scripted(settled.clone()), scripted(settled)]),
            transitions: std::sync::Arc::clone(&applied),
        };
        let state = tempfile::tempdir().unwrap();
        let mut session = Session::new(
            Box::new(alpha),
            Box::new(beta),
            SyncMode::TwoWaySafe,
            state.path().to_path_buf(),
        )
        .expect("session should be creatable");

        assert!(!session.run_cycle().expect("cycle should succeed").changed());
        assert!(!session.run_cycle().expect("cycle should succeed").changed());
        assert!(session.quiesced, "two identical cycles should settle");

        // Alpha's edit arrives on a settled session: the shortcut must not
        // fire, because alpha's storage no longer matches what settled.
        let report = session.run_cycle().expect("cycle should succeed");
        assert_eq!(report.beta_transitions, 1, "the edit was not propagated");
        assert_eq!(applied.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(
            !session.quiesced,
            "a cycle that applied work is not settled"
        );
    }

    #[test]
    fn an_unsettled_cycle_does_not_arm_the_shortcut() {
        // A cycle that reports a scan problem has *not* settled, so an
        // identical pair of scans afterwards must still be reconciled —
        // the problem has to be reported again rather than skipped into
        // silence.
        let problematic = Node::directory(
            "",
            vec![Node {
                name: "broken".into(),
                content: Content::Problematic {
                    message: "unreadable".into(),
                },
            }],
        );
        let alpha = ScriptedEndpoint::new(vec![scripted(problematic.clone())]);
        let beta = ScriptedEndpoint::new(vec![scripted(problematic)]);
        let state = tempfile::tempdir().unwrap();
        let mut session = Session::new(
            Box::new(alpha),
            Box::new(beta),
            SyncMode::TwoWaySafe,
            state.path().to_path_buf(),
        )
        .expect("session should be creatable");

        for cycle in 0..3 {
            let report = session.run_cycle().expect("cycle should succeed");
            assert_eq!(
                report.alpha_scan_problems.len(),
                1,
                "cycle {cycle} stopped reporting the problem"
            );
            assert!(!session.quiesced);
        }
    }

    #[test]
    fn executability_noise_from_non_preserving_filesystems_is_suppressed() {
        use crate::tree::Snapshot;

        let tool = |executable: bool| Node {
            name: "tool".into(),
            content: Content::File {
                digest: [9u8; 32] as Digest,
                executable,
                metadata: FileMetadata::default(),
            },
        };
        let snapshot = |root: Node, preserves: bool| Snapshot {
            root: Some(root),
            preserves_executability: preserves,
            ..Snapshot::default()
        };

        // Alpha preserves executability; beta doesn't, and after the first
        // cycle its scans report the file spuriously executable (the classic
        // FAT-style noise). The third alpha scan carries a *real*
        // executability change.
        let alpha = ScriptedEndpoint::new(vec![
            snapshot(Node::directory("", vec![tool(false)]), true),
            snapshot(Node::directory("", vec![tool(false)]), true),
            snapshot(Node::directory("", vec![tool(true)]), true),
        ]);
        let beta = ScriptedEndpoint::new(vec![
            snapshot(Node::directory("", vec![]), false),
            snapshot(Node::directory("", vec![tool(true)]), false),
            snapshot(Node::directory("", vec![tool(true)]), false),
        ]);

        let state = tempfile::tempdir().unwrap();
        let mut session = Session::new(
            Box::new(alpha),
            Box::new(beta),
            SyncMode::TwoWaySafe,
            state.path().join("session"),
        )
        .expect("the session should construct");

        // Cycle 1 creates the file on beta and establishes the ancestor.
        let report = session.run_cycle().expect("cycle 1");
        assert_eq!(report.beta_transitions, 1);

        // Cycle 2: beta's spurious bit is grafted away; nothing propagates
        // (without propagation this would emit a transition to alpha).
        let report = session.run_cycle().expect("cycle 2");
        assert!(!report.changed(), "{report:?}");
        assert!(report.conflicts.is_empty());

        // Cycle 3: alpha makes a *real* executability change. Beta's copy
        // holds the same bytes, so the preserving peer vouches for the new
        // bit directly — the sides agree immediately, with no transition
        // and no conflict (the case a purely ancestor-based graft would
        // have reported as a false conflict).
        let report = session.run_cycle().expect("cycle 3");
        assert!(!report.changed(), "{report:?}");
        assert!(report.conflicts.is_empty(), "{report:?}");
    }

    #[test]
    fn a_state_directory_admits_only_one_session_at_a_time() {
        use crate::endpoint::local::LocalEndpoint;

        let keep = tempfile::tempdir().unwrap();
        let state = keep.path().join("state");
        let endpoint = |name: &str| -> Box<dyn crate::endpoint::Endpoint + Send> {
            let root = keep.path().join(name);
            fs::create_dir_all(&root).unwrap();
            Box::new(
                LocalEndpoint::new(
                    root,
                    keep.path().join(format!("staging-{name}")),
                    crate::endpoint::local::EndpointOptions::default(),
                )
                .unwrap(),
            )
        };

        let held = Session::new(
            endpoint("a1"),
            endpoint("b1"),
            SyncMode::TwoWaySafe,
            state.clone(),
        )
        .expect("the first session should acquire the lock");

        let error = Session::new(
            endpoint("a2"),
            endpoint("b2"),
            SyncMode::TwoWaySafe,
            state.clone(),
        )
        .err()
        .expect("a concurrent session over the same state must be refused");
        assert!(
            format!("{error:#}").contains("another autobahn process"),
            "{error:#}"
        );

        // Dropping the first session releases the lock.
        drop(held);
        Session::new(endpoint("a3"), endpoint("b3"), SyncMode::TwoWaySafe, state)
            .expect("the lock should be free again");
    }

    // ── the staging and transition lifecycle, under injected faults ──
    //
    // Real endpoints over real trees, with a crash injected at each
    // boundary of the staging and transition lifecycle. After every crash
    // the same things must hold: nothing on either disk is ever torn (every
    // file is bytewise one of its legitimate versions), a fresh session
    // over the same state recovers to quiescence, conflicts surface only on
    // the crashed cycle's own paths, and everything unconflicted agrees.

    /// The boundaries a cycle can die at.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Fault {
        /// Before staging begins on the destination.
        StageBegin,
        /// Mid-receive: half of a frame batch lands, then the crash.
        StagePushTruncate,
        /// Mid-supply on the source.
        SupplyPull,
        /// At the transition, before anything is applied.
        TransitionBefore,
        /// Mid-transition: half of the changes land, then the crash.
        TransitionPartial,
        /// After the whole transition, before the cycle records it.
        TransitionAfter,
        /// Not a crash: every literal byte in every frame is silently
        /// corrupted. The digest gate on the receiving side must discard
        /// the content, and the session must still converge with the
        /// correct bytes — this is the end-to-end proof that nothing
        /// unverified can reach the staging store.
        CorruptFrames,
    }

    /// A local endpoint that dies once, at its armed boundary.
    struct FaultEndpoint {
        inner: crate::endpoint::local::LocalEndpoint,
        fault: Option<Fault>,
        pulls: usize,
    }

    impl FaultEndpoint {
        fn fires(&mut self, at: Fault) -> bool {
            if self.fault == Some(at) {
                self.fault = None;
                return true;
            }
            false
        }
    }

    impl Endpoint for FaultEndpoint {
        fn scan(&mut self) -> Result<crate::tree::Snapshot> {
            self.inner.scan()
        }
        fn stage_begin(
            &mut self,
            files: Vec<FileRequest>,
        ) -> Result<Vec<crate::endpoint::StagingNeed>> {
            if self.fires(Fault::StageBegin) {
                anyhow::bail!("injected fault: stage_begin");
            }
            self.inner.stage_begin(files)
        }
        fn supply_open(&mut self, needs: Vec<crate::endpoint::StagingNeed>) -> Result<()> {
            self.inner.supply_open(needs)
        }
        fn supply_pull(
            &mut self,
            max_frames: usize,
        ) -> Result<Vec<crate::endpoint::TransferFrame>> {
            self.pulls += 1;
            if self.pulls > 1 && self.fires(Fault::SupplyPull) {
                anyhow::bail!("injected fault: supply_pull");
            }
            self.inner.supply_pull(max_frames)
        }
        fn stage_push(&mut self, frames: Vec<crate::endpoint::TransferFrame>) -> Result<()> {
            self.stage_push_nowait(frames)
        }
        fn stage_push_nowait(
            &mut self,
            mut frames: Vec<crate::endpoint::TransferFrame>,
        ) -> Result<()> {
            if self.fires(Fault::StagePushTruncate) {
                frames.truncate(frames.len() / 2);
                let _ = self.inner.stage_push_nowait(frames);
                anyhow::bail!("injected fault: stage_push");
            }
            if self.fault == Some(Fault::CorruptFrames) {
                // Not one-shot: every batch of the cycle is corrupted.
                for frame in &mut frames {
                    if let crate::endpoint::TransferFrame::Op(crate::rsync::Op::Data(data)) = frame
                    {
                        for byte in data.iter_mut() {
                            *byte ^= 0x55;
                        }
                    }
                }
            }
            self.inner.stage_push_nowait(frames)
        }
        fn stage_finish(&mut self) -> Result<()> {
            self.inner.stage_finish()
        }
        fn transition(&mut self, mut transitions: Vec<Change>) -> Result<TransitionOutcome> {
            if self.fires(Fault::TransitionBefore) {
                anyhow::bail!("injected fault: transition (nothing applied)");
            }
            if self.fires(Fault::TransitionPartial) {
                transitions.truncate(transitions.len().div_ceil(2));
                let _ = self.inner.transition(transitions);
                anyhow::bail!("injected fault: transition (partially applied)");
            }
            if self.fault == Some(Fault::TransitionAfter) {
                self.fault = None;
                let _ = self.inner.transition(transitions);
                anyhow::bail!("injected fault: transition (fully applied)");
            }
            self.inner.transition(transitions)
        }
    }

    fn lifecycle_endpoint(
        root: &std::path::Path,
        staging: &std::path::Path,
        fault: Option<Fault>,
    ) -> Box<dyn Endpoint + Send> {
        let inner = crate::endpoint::local::LocalEndpoint::new(
            root.to_path_buf(),
            staging.to_path_buf(),
            crate::endpoint::local::EndpointOptions::default(),
        )
        .expect("endpoints open");
        Box::new(FaultEndpoint {
            inner,
            fault,
            pulls: 0,
        })
    }

    /// Deterministic content large enough to span several transfer frames.
    fn bytes(seed: u8, length: usize) -> Vec<u8> {
        (0..length)
            .map(|index| (index as u64).wrapping_mul(31).wrapping_add(seed as u64) as u8)
            .collect()
    }

    fn read_or_absent(root: &std::path::Path, name: &str) -> Option<Vec<u8>> {
        std::fs::read(root.join(name)).ok()
    }

    /// The torn-bytes check: whatever is on disk at this path is byte-for-
    /// byte one of its legitimate versions — never a truncated transfer,
    /// never staged garbage published early.
    fn assert_untorn(root: &std::path::Path, name: &str, candidates: &[Option<Vec<u8>>]) {
        let actual = read_or_absent(root, name);
        assert!(
            candidates.contains(&actual),
            "{name} in {} holds none of its legitimate versions \
             (found {:?} bytes)",
            root.display(),
            actual.map(|bytes| bytes.len())
        );
    }

    fn cycle_to_quiescence(session: &mut Session) -> CycleReport {
        let mut last = None;
        for _ in 0..6 {
            let report = session.run_cycle().expect("recovery cycles run");
            let settled = report.alpha_transitions == 0
                && report.beta_transitions == 0
                && !report.missing_staged_files;
            last = Some(report);
            if settled {
                return last.unwrap();
            }
        }
        panic!("the session did not quiesce: {last:?}");
    }

    /// The full lifecycle: converge, diverge, crash at the armed boundary,
    /// verify nothing is torn, recover, verify the recovered state.
    fn a_crash_at(alpha_fault: Option<Fault>, beta_fault: Option<Fault>) {
        let keep = tempfile::tempdir().unwrap();
        let alpha_root = keep.path().join("alpha");
        let beta_root = keep.path().join("beta");
        let alpha_staging = keep.path().join("staging-alpha");
        let beta_staging = keep.path().join("staging-beta");
        let state = keep.path().join("state");
        std::fs::create_dir_all(&alpha_root).unwrap();
        std::fs::create_dir_all(&beta_root).unwrap();
        std::fs::create_dir_all(&state).unwrap();

        let old_modify = bytes(1, 96 * 1024);
        let new_modify = bytes(2, 120 * 1024);
        let old_delete = bytes(3, 48 * 1024);
        let created = bytes(4, 160 * 1024);
        let own = bytes(5, 24 * 1024);

        // Converge on the initial content.
        std::fs::write(alpha_root.join("modify.txt"), &old_modify).unwrap();
        std::fs::write(alpha_root.join("delete.txt"), &old_delete).unwrap();
        std::fs::write(alpha_root.join("keep.txt"), b"keep").unwrap();
        {
            let mut session = Session::new(
                lifecycle_endpoint(&alpha_root, &alpha_staging, None),
                lifecycle_endpoint(&beta_root, &beta_staging, None),
                SyncMode::TwoWaySafe,
                state.clone(),
            )
            .unwrap();
            let report = cycle_to_quiescence(&mut session);
            assert!(report.conflicts.is_empty(), "{:?}", report.conflicts);
        }
        assert_eq!(
            read_or_absent(&beta_root, "keep.txt").as_deref(),
            Some(&b"keep"[..])
        );

        // The divergence the crashed cycle will be propagating.
        std::fs::write(alpha_root.join("modify.txt"), &new_modify).unwrap();
        std::fs::write(alpha_root.join("created.bin"), &created).unwrap();
        std::fs::remove_file(alpha_root.join("delete.txt")).unwrap();
        std::fs::write(beta_root.join("beta_own.txt"), &own).unwrap();

        // The crash.
        {
            let mut session = Session::new(
                lifecycle_endpoint(&alpha_root, &alpha_staging, alpha_fault),
                lifecycle_endpoint(&beta_root, &beta_staging, beta_fault),
                SyncMode::TwoWaySafe,
                state.clone(),
            )
            .unwrap();
            let error = session.run_cycle().expect_err("the injected fault fires");
            assert!(format!("{error:#}").contains("injected fault"), "{error:#}");
        }

        // Nothing is torn while the tool is down.
        let untorn = |root: &std::path::Path| {
            assert_untorn(
                root,
                "modify.txt",
                &[Some(old_modify.clone()), Some(new_modify.clone())],
            );
            assert_untorn(root, "created.bin", &[None, Some(created.clone())]);
            assert_untorn(root, "delete.txt", &[None, Some(old_delete.clone())]);
            assert_untorn(root, "keep.txt", &[Some(b"keep".to_vec())]);
            assert_untorn(root, "beta_own.txt", &[None, Some(own.clone())]);
        };
        untorn(&alpha_root);
        untorn(&beta_root);

        // Recovery: a fresh session over the same state, no faults.
        let report = {
            let mut session = Session::new(
                lifecycle_endpoint(&alpha_root, &alpha_staging, None),
                lifecycle_endpoint(&beta_root, &beta_staging, None),
                SyncMode::TwoWaySafe,
                state,
            )
            .unwrap();
            cycle_to_quiescence(&mut session)
        };

        // Still nothing torn, crash noise stays on the crashed cycle's own
        // paths, and every unconflicted path agrees between the sides.
        untorn(&alpha_root);
        untorn(&beta_root);
        let conflicted: Vec<&str> = report
            .conflicts
            .iter()
            .map(|conflict| conflict.root.as_str())
            .collect();
        for path in &conflicted {
            assert!(
                ["modify.txt", "created.bin", "delete.txt", "beta_own.txt"].contains(path),
                "a conflict appeared off the crashed cycle's paths: {path}"
            );
        }
        for path in [
            "modify.txt",
            "created.bin",
            "delete.txt",
            "keep.txt",
            "beta_own.txt",
        ] {
            if !conflicted.contains(&path) {
                assert_eq!(
                    read_or_absent(&alpha_root, path),
                    read_or_absent(&beta_root, path),
                    "{path} is unconflicted but the sides disagree"
                );
            }
        }
        // The edit made on beta while the crash was in flight is never lost.
        assert_eq!(read_or_absent(&beta_root, "beta_own.txt"), Some(own));

        // Faults during staging precede the intent record, so recovery owes
        // full convergence with no conflict noise at all.
        let staging_fault = matches!(
            alpha_fault.or(beta_fault),
            Some(Fault::StageBegin | Fault::StagePushTruncate | Fault::SupplyPull)
        );
        if staging_fault {
            assert!(report.conflicts.is_empty(), "{:?}", report.conflicts);
            assert_eq!(
                read_or_absent(&alpha_root, "modify.txt"),
                Some(new_modify.clone())
            );
            assert_eq!(read_or_absent(&beta_root, "modify.txt"), Some(new_modify));
            assert_eq!(read_or_absent(&beta_root, "created.bin"), Some(created));
            assert_eq!(read_or_absent(&beta_root, "delete.txt"), None);
        }
    }

    #[test]
    fn a_crash_before_staging_recovers_cleanly() {
        a_crash_at(None, Some(Fault::StageBegin));
    }

    #[test]
    fn a_truncated_staging_transfer_recovers_cleanly() {
        a_crash_at(None, Some(Fault::StagePushTruncate));
    }

    #[test]
    fn a_source_that_dies_mid_supply_recovers_cleanly() {
        a_crash_at(Some(Fault::SupplyPull), None);
    }

    #[test]
    fn a_crash_before_any_transition_recovers_safely() {
        a_crash_at(None, Some(Fault::TransitionBefore));
    }

    #[test]
    fn a_partially_applied_transition_recovers_safely() {
        a_crash_at(None, Some(Fault::TransitionPartial));
    }

    #[test]
    fn a_crash_after_transition_before_record_recovers_safely() {
        a_crash_at(None, Some(Fault::TransitionAfter));
    }

    /// Corrupted transfer content never reaches either tree: the receive
    /// digest gate discards it, and once the frames flow clean again the
    /// session converges on the correct bytes.
    #[test]
    fn corrupted_frames_are_discarded_never_published() {
        let keep = tempfile::tempdir().unwrap();
        let alpha_root = keep.path().join("alpha");
        let beta_root = keep.path().join("beta");
        let state = keep.path().join("state");
        std::fs::create_dir_all(&alpha_root).unwrap();
        std::fs::create_dir_all(&beta_root).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let content = bytes(9, 200 * 1024);
        std::fs::write(alpha_root.join("payload.bin"), &content).unwrap();

        let mut session = Session::new(
            lifecycle_endpoint(&alpha_root, &keep.path().join("staging-alpha"), None),
            lifecycle_endpoint(
                &beta_root,
                &keep.path().join("staging-beta"),
                Some(Fault::CorruptFrames),
            ),
            SyncMode::TwoWaySafe,
            state,
        )
        .unwrap();

        // The corrupted cycle: staging discards the mismatched content, so
        // the transition reports it missing and nothing lands on beta —
        // wrong bytes above all.
        let report = session.run_cycle().expect("a corrupted cycle still runs");
        assert_untorn(&beta_root, "payload.bin", &[None, Some(content.clone())]);
        assert!(
            report.missing_staged_files || read_or_absent(&beta_root, "payload.bin").is_some(),
            "the corrupted transfer neither landed nor was reported missing"
        );

        // The corruption stops (the armed fault is consumed by replacing
        // the endpoint), and the next session converges on correct bytes.
        drop(session);
        let mut session = Session::new(
            lifecycle_endpoint(&alpha_root, &keep.path().join("staging-alpha"), None),
            lifecycle_endpoint(&beta_root, &keep.path().join("staging-beta"), None),
            SyncMode::TwoWaySafe,
            keep.path().join("state"),
        )
        .unwrap();
        let report = cycle_to_quiescence(&mut session);
        assert!(report.conflicts.is_empty(), "{:?}", report.conflicts);
        assert_eq!(read_or_absent(&beta_root, "payload.bin"), Some(content));
    }

    /// A session with a remote endpoint asks for durable intents: the
    /// peer's machine persists transitions independently of this
    /// machine's page cache, so an unsynced intent is no ordering at all.
    #[test]
    fn a_remote_endpoint_makes_the_intent_durable() {
        struct RemoteFlagged(ScriptedEndpoint);
        impl Endpoint for RemoteFlagged {
            fn is_remote(&self) -> bool {
                true
            }
            fn scan(&mut self) -> Result<crate::tree::Snapshot> {
                self.0.scan()
            }
            fn stage_begin(
                &mut self,
                files: Vec<FileRequest>,
            ) -> Result<Vec<crate::endpoint::StagingNeed>> {
                self.0.stage_begin(files)
            }
            fn supply_open(&mut self, needs: Vec<crate::endpoint::StagingNeed>) -> Result<()> {
                self.0.supply_open(needs)
            }
            fn supply_pull(
                &mut self,
                max_frames: usize,
            ) -> Result<Vec<crate::endpoint::TransferFrame>> {
                self.0.supply_pull(max_frames)
            }
            fn stage_push(&mut self, frames: Vec<crate::endpoint::TransferFrame>) -> Result<()> {
                self.0.stage_push(frames)
            }
            fn transition(&mut self, transitions: Vec<Change>) -> Result<TransitionOutcome> {
                self.0.transition(transitions)
            }
        }

        let cycle_syncs = |remote: bool| {
            let keep = tempfile::tempdir().unwrap();
            let state = keep.path().join("state");
            std::fs::create_dir_all(&state).unwrap();
            let content = || Node::directory("", vec![file("target", 1)]);
            let alpha = ScriptedEndpoint::new(vec![scripted(content())]);
            let beta: Box<dyn Endpoint + Send> = if remote {
                Box::new(RemoteFlagged(ScriptedEndpoint::new(vec![scripted(
                    Node::directory("", vec![]),
                )])))
            } else {
                Box::new(ScriptedEndpoint::new(vec![scripted(Node::directory(
                    "",
                    vec![],
                ))]))
            };
            let mut session =
                Session::new(Box::new(alpha), beta, SyncMode::TwoWaySafe, state).unwrap();
            session.run_cycle().expect("the cycle runs");
            session.ancestor_store.append_syncs
        };
        assert_eq!(cycle_syncs(true), 1, "a remote session syncs its intent");
        assert_eq!(cycle_syncs(false), 0, "a local session does not");
    }
}
