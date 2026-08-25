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

use anyhow::{bail, Context, Result};

use crate::endpoint::{Endpoint, FileRequest, TransitionOutcome};
use crate::tree::{
    apply, path_join, propagate_executability, reconcile, Change, Conflict, Content, Node, Problem,
    SyncMode,
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
}

impl CycleReport {
    /// Indicates whether or not the cycle applied any transitions.
    pub fn changed(&self) -> bool {
        self.alpha_transitions > 0 || self.beta_transitions > 0
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
    ancestor_path: PathBuf,
    /// The current ancestor hierarchy.
    ancestor: Option<Node>,
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
        let ancestor = load_ancestor(&ancestor_path)?;
        Ok(Session {
            alpha,
            beta,
            mode,
            ancestor_path,
            ancestor,
            _lock: lock,
        })
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

    /// Runs one synchronization cycle: scan both endpoints, reconcile,
    /// stage and apply transitions, and update the persisted ancestor.
    pub fn run_cycle(&mut self) -> Result<CycleReport> {
        let mut report = CycleReport::default();

        // Scan both endpoints in parallel.
        let (alpha_snapshot, beta_snapshot) = {
            let alpha = &mut self.alpha;
            let beta = &mut self.beta;
            std::thread::scope(|scope| {
                let alpha_scan = scope.spawn(move || alpha.scan());
                let beta_result = beta.scan();
                let alpha_result = alpha_scan.join().expect("scan thread panicked");
                (alpha_result, beta_result)
            })
        };
        let alpha_snapshot = alpha_snapshot.context("alpha scan failed")?;
        let beta_snapshot = beta_snapshot.context("beta scan failed")?;
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
        let reconciliation = reconcile(
            self.ancestor.as_ref(),
            alpha_root.as_ref(),
            beta_root.as_ref(),
            self.mode,
        );
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

        // Stage and transition each side. Content flowing to beta is
        // supplied by alpha and vice versa.
        let beta_outcome = if reconciliation.beta_transitions.is_empty() {
            None
        } else {
            stage(
                self.alpha.as_mut(),
                self.beta.as_mut(),
                &reconciliation.beta_transitions,
            )?;
            Some(
                self.beta
                    .transition(reconciliation.beta_transitions.clone())
                    .context("beta transition failed")?,
            )
        };
        let alpha_outcome = if reconciliation.alpha_transitions.is_empty() {
            None
        } else {
            stage(
                self.beta.as_mut(),
                self.alpha.as_mut(),
                &reconciliation.alpha_transitions,
            )?;
            Some(
                self.alpha
                    .transition(reconciliation.alpha_transitions.clone())
                    .context("alpha transition failed")?,
            )
        };

        // Fold transition results into ancestor changes: each transition's
        // achieved content becomes the ancestor's new content at that path.
        let mut ancestor_changes = reconciliation.ancestor_changes;
        let mut fold = |transitions: &[Change], outcome: &TransitionOutcome| {
            for (transition, result) in transitions.iter().zip(outcome.results.iter()) {
                ancestor_changes.push(Change {
                    path: transition.path.clone(),
                    old: None,
                    new: result.clone(),
                });
            }
        };
        if let Some(outcome) = &beta_outcome {
            fold(&reconciliation.beta_transitions, outcome);
            report.beta_transitions = reconciliation.beta_transitions.len();
            report.beta_transition_problems = outcome.problems.clone();
            report.missing_staged_files |= outcome.missing_staged_files;
        }
        if let Some(outcome) = &alpha_outcome {
            fold(&reconciliation.alpha_transitions, outcome);
            report.alpha_transitions = reconciliation.alpha_transitions.len();
            report.alpha_transition_problems = outcome.problems.clone();
            report.missing_staged_files |= outcome.missing_staged_files;
        }

        // Apply the ancestor changes, validate the result (the ancestor must
        // only ever contain synchronizable content — this is the safety net
        // against reconciliation defects reaching disk), and persist it.
        if !ancestor_changes.is_empty() {
            let new_ancestor = apply(self.ancestor.as_ref(), &ancestor_changes)
                .map_err(|message| anyhow::anyhow!("ancestor update failed: {message}"))?;
            if let Some(root) = &new_ancestor {
                root.validate(true)
                    .map_err(|message| anyhow::anyhow!("new ancestor is invalid: {message}"))?;
            }
            save_ancestor(&self.ancestor_path, new_ancestor.as_ref())?;
            self.ancestor = new_ancestor;
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
fn stage(
    source: &mut dyn Endpoint,
    destination: &mut dyn Endpoint,
    transitions: &[Change],
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
    source
        .supply_open(needs)
        .context("unable to open supply stream")?;
    // The pull and push halves run on separate threads with a small bounded
    // buffer between them, so the source's reads overlap the destination's
    // writes even when both endpoints share this process. (A remote
    // destination additionally keeps its own window of pushed batches in
    // flight, and stage_finish drains those acknowledgements.)
    let (batches, staged) = std::sync::mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        let pusher = scope.spawn(move || -> Result<()> {
            while let Ok(frames) = staged.recv() {
                destination
                    .stage_push_nowait(frames)
                    .context("unable to push file content")?;
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

/// Detects the emptied-root condition: the ancestor was a directory with
/// two or more immediate children, and exactly one side now presents a
/// childless (or absent) directory root while the other retains content.
fn one_side_emptied_root(
    ancestor: Option<&Node>,
    alpha: Option<&Node>,
    beta: Option<&Node>,
) -> bool {
    let ancestor_children = match ancestor {
        Some(node) if matches!(node.content, Content::Directory(_)) => node.children().len(),
        _ => return false,
    };
    if ancestor_children < 2 {
        return false;
    }
    let side_empty = |side: Option<&Node>| match side {
        None => true,
        Some(node) => node.children().is_empty(),
    };
    let alpha_empty = side_empty(alpha);
    let beta_empty = side_empty(beta);
    alpha_empty != beta_empty
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

/// Loads a persisted ancestor, treating a missing file as an absent
/// ancestor and failing on corruption (an unreadable ancestor must not be
/// silently discarded, since that would resurrect deletions).
fn load_ancestor(path: &PathBuf) -> Result<Option<Node>> {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("unable to read ancestor"),
    };
    let ancestor: Option<Node> =
        bincode::deserialize(&data).context("unable to decode ancestor")?;
    if let Some(root) = &ancestor {
        root.validate(true)
            .map_err(|message| anyhow::anyhow!("persisted ancestor is invalid: {message}"))?;
    }
    Ok(ancestor)
}

/// Persists the ancestor atomically (write-temporary-then-rename).
fn save_ancestor(path: &PathBuf, ancestor: Option<&Node>) -> Result<()> {
    let data = bincode::serialize(&ancestor.cloned()).context("unable to encode ancestor")?;
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, data).context("unable to write ancestor")?;
    fs::rename(&temporary, path).context("unable to publish ancestor")?;
    Ok(())
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
        let trivial = Node::directory("", vec![file("a", 1)]);
        assert!(!one_side_emptied_root(
            Some(&trivial),
            Some(&empty),
            Some(&trivial)
        ));
    }

    #[test]
    fn ancestor_persistence_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ancestor");
        let ancestor = Node::directory("", vec![file("a", 1)]);
        save_ancestor(&path, Some(&ancestor)).unwrap();
        let loaded = load_ancestor(&path).unwrap().unwrap();
        assert!(loaded.content_equal(&ancestor, true));
        save_ancestor(&path, None).unwrap();
        assert!(load_ancestor(&path).unwrap().is_none());
    }

    #[test]
    fn corrupt_ancestor_is_an_error_not_a_reset() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ancestor");
        fs::write(&path, b"garbage").unwrap();
        assert!(load_ancestor(&path).is_err());
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
            })
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
}
