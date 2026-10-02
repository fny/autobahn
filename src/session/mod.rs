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

use crate::endpoint::{
    ChangeActivity, Endpoint, FileRequest, StagingNeed, TransferFrame, TransitionOutcome,
};
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
    /// The primary root is not there at all. Nothing is synchronized, so the
    /// destination is not emptied to match a source that only looks empty
    /// — an unplugged drive, a dropped share, a mistyped path. Unlike the
    /// others this clears on its own: the next attempt finds the folder
    /// back and carries on.
    #[error("halted: the primary folder {0} is missing, so nothing was synchronized rather than emptying the other side to match; reconnect the drive or correct the path, and syncing resumes on its own")]
    PrimaryRootMissing(String),
    /// The session's ancestor cannot be read, and the two sides differ, so
    /// there is no safe way to tell a deletion from a creation or an edit
    /// from a stale copy. When they match it is rebuilt instead.
    #[error("halted: this session's record of what the two sides last agreed on cannot be read ({0}), and the two sides differ, so nothing was synchronized; without that record deletions would come back and edits could be overwritten. `autobahn doctor <group>` shows how they differ. Once they match it rebuilds on its own, or `autobahn reset <group>` merges them")]
    AncestorUnreadable(String),
    /// A directory that was a mount point, synchronized as part of the tree
    /// because mounts are not ignored, is now empty or gone where the
    /// ancestor says it held content: the signature of a filesystem that
    /// went away, which would otherwise propagate as deleting everything
    /// that was on it.
    #[error("halted: {1} on {0} was a mount point and is now empty or gone, so its content was not deleted on the other side to match; remount it, or delete the content on the other side yourself if it really is gone")]
    MountVanished(&'static str, String),
    /// The ancestor is damaged again after being rebuilt once. A disk that
    /// damages one will damage another; it is not rebuilt twice.
    #[error("halted: this session's record of what the two sides last agreed on cannot be read ({0}), and it was rebuilt once already after the same kind of damage; a disk that damages one will damage another, so it is not rebuilt again. Check the disk, then `autobahn reset <group>`")]
    AncestorDamagedAgain(String),
}

impl SafetyHalt {
    /// How long this halt must stand before it is worth waking someone,
    /// when that differs from a halt's usual "at once". A missing primary is
    /// often a drive that comes back with the laptop's wake.
    pub fn alert_after(&self) -> Option<std::time::Duration> {
        match self {
            SafetyHalt::PrimaryRootMissing(_) => Some(std::time::Duration::from_secs(120)),
            SafetyHalt::RootDeletion
            | SafetyHalt::RootEmptied
            | SafetyHalt::AncestorUnreadable(_)
            | SafetyHalt::AncestorDamagedAgain(_)
            | SafetyHalt::MountVanished(..) => None,
        }
    }
}

/// A report of one synchronization cycle.
#[derive(Debug, Default)]
pub struct CycleReport {
    /// The number of transitions applied to primary.
    pub primary_transitions: usize,
    /// The number of transitions applied to replica.
    pub replica_transitions: usize,
    /// Conflicts identified during reconciliation (left unresolved in safe
    /// modes).
    pub conflicts: Vec<Conflict>,
    /// Scan problems from primary.
    pub primary_scan_problems: Vec<Problem>,
    /// Scan problems from replica.
    pub replica_scan_problems: Vec<Problem>,
    /// Transition problems from primary.
    pub primary_transition_problems: Vec<Problem>,
    /// Transition problems from replica.
    pub replica_transition_problems: Vec<Problem>,
    /// Whether or not either endpoint reported missing staged content
    /// (warranting an immediate follow-up cycle).
    pub missing_staged_files: bool,
    /// Which content was confirmed absent from staging this cycle, by path
    /// and digest, across both endpoints. A follow-up that reports the same
    /// pair again is not looking at a changing file.
    pub missing_staged: Vec<crate::endpoint::FileRequest>,
    /// Whether primary's scan was skipped on the strength of a standing
    /// watch, its last snapshot standing in.
    pub primary_scan_skipped: bool,
    /// Whether replica's scan was skipped, in the same way.
    pub replica_scan_skipped: bool,
}

impl CycleReport {
    /// Indicates whether or not the cycle applied any transitions.
    pub fn changed(&self) -> bool {
        self.primary_transitions > 0 || self.replica_transitions > 0
    }

    /// Indicates that the cycle left nothing outstanding: nothing applied,
    /// nothing in conflict, nothing reported. Only after such a cycle can
    /// the next one treat unchanged scans as proof that the two sides are
    /// still synchronized.
    pub fn settled(&self) -> bool {
        !self.changed()
            && self.conflicts.is_empty()
            && !self.missing_staged_files
            && self.primary_scan_problems.is_empty()
            && self.replica_scan_problems.is_empty()
            && self.primary_transition_problems.is_empty()
            && self.replica_transition_problems.is_empty()
    }
}

/// A point in a cycle at which a test may act. See
/// [`Session::set_cycle_hook`].
///
/// The transition points fire only when that side has transitions to
/// apply; the others fire on every cycle that gets that far. A cycle that
/// took the "nothing changed" shortcut fires none of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CyclePoint {
    /// Both scans are in hand; nothing has been reconciled or moved.
    AfterScans,
    /// Replica's content is staged; its transition is about to be sent.
    BeforeReplicaTransition,
    /// Replica's transition has answered.
    AfterReplicaTransition,
    /// Primary's content is staged; its transition is about to be sent.
    BeforePrimaryTransition,
    /// Primary's transition has answered.
    AfterPrimaryTransition,
    /// Every transition has answered; the ancestor is about to be recorded.
    BeforeRecord,
}

/// What [`Session::set_cycle_hook`] installs.
pub type CycleHook = Box<dyn FnMut(CyclePoint) + Send>;

/// One of a session's two sides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Side {
    Primary,
    Replica,
}

/// What resolution does with one losing copy.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Retirement {
    /// Remove it, provided it still holds this — its synchronizable
    /// content as `resolve` scanned it. A transition removes it, so
    /// anything changed since is refused rather than destroyed.
    Remove(Node),
    /// Move it aside to this free name, keeping it.
    Aside(String),
}

/// One session's part in a resolution: the paths its ancestor forgets,
/// and the losing copies on its sides that are retired.
///
/// Forgetting is what makes retiring enough. With the ancestor silent at a
/// path, the next cycle sees the kept version as a creation and carries it
/// to every side, in every two-way mode, for a file, a link or a whole
/// tree; the gap the loser leaves can no longer read as a deletion against
/// an untouched copy.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Settlement {
    /// Root-relative paths the ancestor forgets.
    pub forget: Vec<String>,
    /// The losing copies, by side and path.
    pub retire: Vec<(Side, String, Retirement)>,
}

impl Settlement {
    /// Leaves out every path at, inside or around one in `refused`: where
    /// another part could not retire its copy, forgetting the path and
    /// retiring this one would hand that copy the win.
    pub fn spare(&mut self, refused: &[String]) {
        let spared = |path: &String| {
            refused.iter().any(|at| {
                at == path
                    || at.starts_with(&format!("{path}/"))
                    || path.starts_with(&format!("{at}/"))
            })
        };
        self.forget.retain(|path| !spared(path));
        self.retire.retain(|(_, path, _)| !spared(path));
    }
}

/// What applying a [`Settlement`] came to.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct SettlementOutcome {
    /// Paths whose losing copy was retired or moved aside.
    pub settled: Vec<String>,
    /// Losing copies kept under another name: the path, and the name.
    pub aside: Vec<(String, String)>,
    /// Paths left alone, and why: a copy that changed since `resolve`
    /// scanned it is refused by the transition, not destroyed.
    pub refused: Vec<(String, String)>,
}

/// A synchronization session between two endpoints.
pub struct Session {
    /// The primary endpoint.
    primary: Box<dyn Endpoint + Send>,
    /// The replica endpoint.
    replica: Box<dyn Endpoint + Send>,
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
    /// Raised by either endpoint's watch; what `await_change` sleeps on.
    wake: Arc<crate::endpoint::WakeSignal>,
    /// A test's hand in the cycle, if it asked for one. See [`CyclePoint`].
    hook: Option<CycleHook>,
    /// Whether the last cycle finished with the two sides synchronized and
    /// nothing outstanding — the precondition for skipping a cycle whose
    /// scans reproduce [`settled_primary`](Self::settled_primary) and
    /// [`settled_replica`](Self::settled_replica).
    quiesced: bool,
    /// Primary's hierarchy as of the last quiesced cycle.
    settled_primary: Option<Node>,
    /// Replica's hierarchy as of the last quiesced cycle.
    settled_replica: Option<Node>,
    /// P2P: what this session presents to its peer when its
    /// supervisor leads. `None` for a plain mode, and for a follower.
    leadership: Option<crate::p2p::Leadership>,
    /// When `present_lease` last had the lease accepted, for renewing it
    /// while the session waits between cycles.
    lease_presented_at: Option<std::time::Instant>,
    /// P2P: which side the peer is. The replica, except for the session
    /// a replica that leads runs against the attached primary.
    peer_side: crate::p2p::PeerSide,
    /// P2P: whether the replica's ancestor copy has been compared with
    /// this session's ancestor since the session connected. Done once,
    /// after the first accepted lease, so a replica that has no copy — or
    /// one from before a restart — gets a checkpoint even when the cycle
    /// itself changes nothing.
    copy_checked: bool,
    /// The exclusive lock on the session state directory, held for the
    /// session's lifetime (released when the file closes on drop).
    _lock: SessionLock,
    /// Where the ancestor lives, for a rebuild to set it aside.
    ancestor_path: std::path::PathBuf,
    /// The ancestor could not be read when the session opened: why, and
    /// whether it was a format another build wrote rather than damage. The
    /// first cycle either rebuilds it — when the two sides already match,
    /// which no history could change — or halts.
    unreadable: Option<(String, bool)>,
    /// Whether appends sync before acknowledging, remembered so a rebuilt
    /// store keeps what the configuration asked for.
    power_durability: bool,
    /// Whether mount points inside the roots are left alone (the default)
    /// or synchronized as part of the tree.
    ignore_mounts: bool,
    /// The directory size at or above which a one-sided disappearance is
    /// disbelieved. Carried here so both reconciliations use the same one.
    guard_dir_deletes_over: Option<usize>,
    /// Each side's last scanned root and the problems found in it, so the
    /// next cycle's search can skip what did not change
    /// ([`Node::problems_since`]).
    scan_problems: [Option<(Node, Vec<Problem>)>; 2],
    /// The last reconciliation's inputs and where it produced anything,
    /// for the next to skip what did not change.
    reconcile_memo: Option<crate::tree::ReconcileMemo>,
    /// The mount points each side reported, remembered across cycles (and
    /// runs, in the state directory) so a mount that goes away is known to
    /// have been one: see [`Session::account_for_mounts`].
    mounts: MountRecord,
}

/// The mount points last seen on each side, root-relative.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct MountRecord {
    primary: Vec<String>,
    replica: Vec<String>,
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
pub fn session_identifier(primary_spec: &str, replica_spec: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(primary_spec.as_bytes());
    hasher.update(&[0]);
    hasher.update(replica_spec.as_bytes());
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
        primary: Box<dyn Endpoint + Send>,
        replica: Box<dyn Endpoint + Send>,
        mode: SyncMode,
        state_directory: PathBuf,
    ) -> Result<Session> {
        Session::with_lock(
            primary,
            replica,
            mode,
            SessionLock::acquire(state_directory)?,
        )
    }

    /// Holds an exclusivity lock for this session's lifetime.
    pub fn hold(&mut self, lock: EndpointPairLock) {
        self.held.push(lock);
    }

    /// Whether mount points inside the roots are left alone (the default)
    /// or synchronized, as the configuration says. The endpoints scan
    /// accordingly; the session needs to know which to treat a vanished
    /// mount as.
    pub fn set_ignore_mounts(&mut self, ignore: bool) {
        self.ignore_mounts = ignore;
    }

    pub fn set_guard_dir_deletes_over(&mut self, over: Option<usize>) {
        self.guard_dir_deletes_over = over;
    }

    /// What reconciliation is asked to do: the mode, and how suspicious
    /// to be of a directory that disappears.
    fn policy(&self) -> crate::tree::Policy {
        crate::tree::Policy {
            mode: self.mode,
            guard_dir_deletes_over: self.guard_dir_deletes_over,
        }
    }

    /// Opts the ancestor store into power-loss durability: every journal
    /// append syncs before the cycle is acknowledged.
    pub fn set_power_durability(&mut self, enabled: bool) {
        self.power_durability = enabled;
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

    /// Applies this session's part in a resolution, between cycles and
    /// under the session's lock like any cycle: the ancestor forgets the
    /// paths, durably, and the losing copies are retired after.
    ///
    /// That order is the crash story. Stopped between the two, the
    /// ancestor is silent where both sides still hold their own versions,
    /// and the next cycle reports a conflict or records agreement. The
    /// other order, stopped between, leaves a gap against an ancestor that
    /// still records the kept version — read as a deletion, and carried.
    ///
    /// A side is scanned before its copies are removed (and after any are
    /// moved aside): a transition validates against its endpoint's own
    /// last scan, which a worker's may predate or not have at all. A copy
    /// that changed since `resolve` read it is then refused, by path, and
    /// left where it is.
    pub fn apply_settlement(&mut self, settlement: &Settlement) -> Result<SettlementOutcome> {
        if let Some((problem, _)) = &self.unreadable {
            bail!("the ancestor cannot be read, so nothing can be forgotten in it: {problem}");
        }
        self.ancestor = self
            .ancestor_store
            .forget(self.ancestor.as_ref(), &settlement.forget)?;
        // Both sides are about to change under the last settled cycle.
        self.quiesced = false;
        self.settled_primary = None;
        self.settled_replica = None;

        let mut outcome = SettlementOutcome::default();
        for side in [Side::Primary, Side::Replica] {
            let retire: Vec<&(Side, String, Retirement)> = settlement
                .retire
                .iter()
                .filter(|(at, ..)| *at == side)
                .collect();
            if retire.is_empty() {
                continue;
            }
            let endpoint = match side {
                Side::Primary => &mut self.primary,
                Side::Replica => &mut self.replica,
            };
            let mut removals = Vec::new();
            for (_, path, retirement) in retire {
                match retirement {
                    Retirement::Aside(to) => {
                        endpoint
                            .rename(path, to)
                            .with_context(|| format!("unable to move {path} aside"))?;
                        outcome.settled.push(path.clone());
                        outcome.aside.push((path.clone(), to.clone()));
                    }
                    Retirement::Remove(expectation) => removals.push(Change {
                        path: path.clone(),
                        old: Some(expectation.clone()),
                        new: None,
                    }),
                }
            }
            endpoint
                .scan()
                .context("unable to read the side being settled")?;
            if removals.is_empty() {
                continue;
            }
            let paths: Vec<String> = removals.iter().map(|change| change.path.clone()).collect();
            let result = endpoint
                .transition(removals)
                .context("unable to retire the losing version")?;
            // A refusal is not an error: the transition leaves the content
            // alone and says where. Excluded content a removal steps over
            // is no refusal — what remains is invisible, and settled.
            for problem in &result.problems {
                outcome
                    .refused
                    .push((problem.path.clone(), problem.message.clone()));
            }
            for path in paths {
                let refused = result.problems.iter().any(|problem| {
                    problem.path == path || problem.path.starts_with(&format!("{path}/"))
                });
                if !refused {
                    outcome.settled.push(path);
                }
            }
        }
        Ok(outcome)
    }

    /// Creates a session between the provided endpoints under an
    /// already-held state lock. This exists so that callers with expensive
    /// endpoint construction (spawning SSH, handshaking with an agent) can
    /// acquire the lock *first* and discover a conflicting session before
    /// incurring any of that work or its remote side effects.
    pub fn with_lock(
        primary: Box<dyn Endpoint + Send>,
        replica: Box<dyn Endpoint + Send>,
        mode: SyncMode,
        lock: SessionLock,
    ) -> Result<Session> {
        let ancestor_path = lock.state_directory().join("ancestor");
        // An ancestor that cannot be read is never discarded silently —
        // starting from nothing would bring deletions back — but it need
        // not stop the session either: when the two sides already match
        // there is nothing a history could change, and the first cycle
        // rebuilds it from them. Until it has looked, nothing is written.
        let (mut ancestor_store, mut ancestor, unresolved, unreadable) =
            match ancestor::AncestorStore::open(&ancestor_path) {
                Ok((store, ancestor, unresolved)) => (store, ancestor, unresolved, None),
                Err(error) => (
                    ancestor::AncestorStore::blank(&ancestor_path),
                    None,
                    Vec::new(),
                    Some((
                        format!("{error:#}"),
                        error.downcast_ref::<ancestor::UnknownFormat>().is_some(),
                    )),
                ),
            };
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
                    Some(_) if path.is_empty() => true,
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
        let remote_involved = primary.is_remote() || replica.is_remote();
        let mounts = std::fs::read(lock.state_directory().join("mounts"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Ok(Session {
            held: Vec::new(),
            #[cfg(test)]
            fail_before_record: false,
            verify_next: false,
            remote_involved,
            primary,
            replica,
            mode,
            ancestor_store,
            ancestor,
            progress: Arc::default(),
            wake: Arc::default(),
            hook: None,
            quiesced: false,
            settled_primary: None,
            settled_replica: None,
            leadership: None,
            lease_presented_at: None,
            peer_side: crate::p2p::PeerSide::Replica,
            copy_checked: false,
            _lock: lock,
            mounts,
            ancestor_path,
            unreadable,
            power_durability: false,
            ignore_mounts: true,
            guard_dir_deletes_over: None,
            scan_problems: [None, None],
            reconcile_memo: None,
        })
    }

    /// Squares the two scans with the mount points they found.
    ///
    /// Ignoring mounts (the default), a mount point is left alone on *both*
    /// sides: the scan already left it out where it is mounted, and here the
    /// same path is left out on the other side too, so a real directory
    /// there is neither copied into the mount nor deleted to match it. A
    /// mount point remembered from an earlier cycle whose folder is now
    /// empty or gone — a drive unplugged — stays left out until it holds
    /// something again, so unplugging moves nothing.
    ///
    /// Following mounts, the content is synchronized like any other; but a
    /// mount point that is now empty or gone where the ancestor says it held
    /// content halts, rather than carry the deletion of everything that was
    /// on it.
    fn account_for_mounts(
        &mut self,
        primary_root: Option<Node>,
        replica_root: Option<Node>,
        primary_found: &[String],
        replica_found: &[String],
    ) -> Result<(Option<Node>, Option<Node>)> {
        let hollow = |root: Option<&Node>, path: &str| match crate::tree::node_at(root, path) {
            None => true,
            Some(node) => {
                matches!(&node.content, Content::Directory(children) if children.is_empty())
            }
        };
        let mut record = MountRecord::default();
        for (side, found, root, remembered, kept) in [
            (
                "primary",
                primary_found,
                primary_root.as_ref(),
                &self.mounts.primary,
                &mut record.primary,
            ),
            (
                "replica",
                replica_found,
                replica_root.as_ref(),
                &self.mounts.replica,
                &mut record.replica,
            ),
        ] {
            kept.extend(found.iter().cloned());
            for path in remembered {
                if found.contains(path) || !hollow(root, path) {
                    continue;
                }
                if self.ignore_mounts {
                    kept.push(path.clone());
                } else if crate::tree::node_at(self.ancestor.as_ref(), path)
                    .is_some_and(|node| !node.children().is_empty())
                {
                    bail!(SafetyHalt::MountVanished(side, path.clone()));
                }
            }
            kept.sort();
            kept.dedup();
        }
        if record != self.mounts {
            let path = self.ancestor_path.with_file_name("mounts");
            if let Ok(bytes) = serde_json::to_vec(&record) {
                if let Err(error) = std::fs::write(&path, bytes) {
                    crate::complain!(
                        "unable to record mount points in {}: {error}",
                        path.display()
                    );
                }
            }
            self.mounts = record;
        }
        if !self.ignore_mounts {
            return Ok((primary_root, replica_root));
        }
        let mut excluded: Vec<&String> = self
            .mounts
            .primary
            .iter()
            .chain(&self.mounts.replica)
            .collect();
        excluded.sort();
        excluded.dedup();
        let leave_out = |root: Option<Node>| -> Result<Option<Node>> {
            let changes: Vec<Change> = excluded
                .iter()
                .filter_map(|path| {
                    let node = crate::tree::node_at(root.as_ref(), path)?;
                    (!matches!(node.content, Content::Untracked)).then(|| Change {
                        path: (*path).clone(),
                        old: Some(node.clone()),
                        new: Some(Node {
                            name: node.name.clone(),
                            content: Content::Untracked,
                        }),
                    })
                })
                .collect();
            if changes.is_empty() {
                return Ok(root);
            }
            apply(root.as_ref(), &changes)
                .map_err(|message| anyhow::anyhow!("unable to leave mount points out: {message}"))
        };
        Ok((leave_out(primary_root)?, leave_out(replica_root)?))
    }

    /// Answers an ancestor that could not be read, with both sides scanned.
    ///
    /// When the two sides already match — reconciling them with no history
    /// at all changes nothing and conflicts nowhere — the ancestor can only
    /// ever have said the same, so it is set aside (kept, never deleted)
    /// and this cycle records a fresh one from what both sides hold. That
    /// is a reset, taken only where a reset is free.
    ///
    /// When they differ it halts, and says how to make them match. Damage,
    /// as opposed to a format another build wrote, is rebuilt once per
    /// session: a disk that damages one ancestor will damage another, and a
    /// quiet retry would turn a hardware fault into a mystery.
    fn rebuild_or_halt(
        &mut self,
        problem: String,
        format: bool,
        primary: Option<&Node>,
        replica: Option<&Node>,
    ) -> Result<()> {
        let untouched = reconcile(None, primary, replica, self.policy());
        let matching = untouched.conflicts.is_empty()
            && untouched.primary_transitions.is_empty()
            && untouched.replica_transitions.is_empty();
        let marker = self.ancestor_path.with_file_name("ancestor.rebuilt");
        if !matching {
            self.unreadable = Some((problem.clone(), format));
            bail!(SafetyHalt::AncestorUnreadable(problem));
        }
        if !format && marker.exists() {
            self.unreadable = Some((problem.clone(), format));
            bail!(SafetyHalt::AncestorDamagedAgain(problem));
        }
        ancestor::AncestorStore::set_aside(&self.ancestor_path)?;
        if !format {
            std::fs::write(&marker, format!("{problem}\n"))
                .with_context(|| format!("unable to write {}", marker.display()))?;
        }
        let (mut store, ancestor, _) = ancestor::AncestorStore::open(&self.ancestor_path)?;
        store.set_power_durability(self.power_durability);
        self.ancestor_store = store;
        self.ancestor = ancestor;
        crate::complain!(
            "the record of what the two sides last agreed on could not be read ({problem}); \
             both sides match, so it was rebuilt from them. The old one is kept beside it \
             as {}.unreadable-*",
            self.ancestor_path.display()
        );
        Ok(())
    }

    /// P2P: adopts (or drops) the leadership this session presents to
    /// its replica. Set by a leading supervisor before every attempt, so a
    /// change of term reaches the next cycle.
    pub fn set_leadership(
        &mut self,
        leadership: Option<crate::p2p::Leadership>,
        side: crate::p2p::PeerSide,
    ) {
        if leadership != self.leadership || side != self.peer_side {
            self.copy_checked = false;
        }
        self.leadership = leadership;
        self.peer_side = side;
    }

    /// P2P: the endpoint on the peer's side.
    fn peer(&mut self) -> &mut Box<dyn Endpoint + Send> {
        match self.peer_side {
            crate::p2p::PeerSide::Primary => &mut self.primary,
            crate::p2p::PeerSide::Replica => &mut self.replica,
        }
    }

    /// P2P, with `manage_keys`: the peer host's p2p key and host
    /// keys, the key made if it had none.
    pub fn peer_keys(&mut self) -> Result<crate::peerkeys::HostKeys> {
        self.peer().p2p_keys()
    }

    /// P2P, with `manage_keys`: installs the other replicas' keys on the
    /// peer host.
    pub fn install_peers(&mut self, authorized: &[String], known_hosts: &[String]) -> Result<()> {
        self.peer().install_peers(authorized, known_hosts)
    }

    /// P2P: writes the files a follower needs onto the replica's host.
    pub fn push_p2p_files(&mut self, files: &[(String, Vec<u8>)]) -> Result<()> {
        for (name, bytes) in files {
            self.peer()
                .put_p2p_file(name, bytes)
                .with_context(|| format!("unable to push {name} to the peer"))?;
        }
        Ok(())
    }

    /// P2P, at the start of a cycle: presents the lease to the replica and
    /// stops the cycle if the host refused it. Then, once per session,
    /// makes sure the replica's ancestor copy matches this session's.
    pub fn present_lease(&mut self) -> Result<()> {
        let Some(leadership) = self.leadership.clone() else {
            return Ok(());
        };
        match self.peer().lease(&leadership.lease())? {
            crate::p2p::LeaseAnswer::Accepted => {
                self.lease_presented_at = Some(std::time::Instant::now());
            }
            crate::p2p::LeaseAnswer::Refused { current } => {
                self.lease_presented_at = None;
                return Err(crate::p2p::Fenced { current }.into());
            }
        }
        if !self.copy_checked {
            let generation = self.ancestor_store.generation();
            let state = self.peer().p2p_state()?;
            if state.generation != Some(generation) {
                let ancestor = self.ancestor.clone();
                self.peer()
                    .ancestor_checkpoint(generation, ancestor.as_ref())?;
            }
            self.copy_checked = true;
        }
        Ok(())
    }

    /// P2P: presents the lease again when a third of its lifetime has
    /// passed since it was last accepted. A leading session calls this
    /// while it waits between cycles, so an interval, a long idle spell or
    /// anything else between cycles never lets the lease lapse and hand a
    /// healthy leader's host to a follower. A refusal is `Fenced`.
    pub fn renew_lease_if_due(&mut self) -> Result<()> {
        let Some(leadership) = &self.leadership else {
            return Ok(());
        };
        let due = self
            .lease_presented_at
            .is_none_or(|at| at.elapsed() >= leadership.ttl / 3);
        match due {
            true => self.present_lease(),
            false => Ok(()),
        }
    }

    /// P2P: brings the peer's copy of the ancestor level with this
    /// session's — a checkpoint, when it stands at any other generation —
    /// and confirms it. What a handoff leaves the next leader to adopt must
    /// be this session's history as it stands, not one a record that failed
    /// to arrive left behind: records are replicated best effort, and a
    /// copy that lags by one is found out only by the next record.
    pub fn level_the_copy(&mut self) -> Result<()> {
        if self.leadership.is_none() {
            return Ok(());
        }
        let generation = self.ancestor_store.generation();
        if self.peer().p2p_state()?.generation == Some(generation) {
            return Ok(());
        }
        let ancestor = self.ancestor.clone();
        let reached = self
            .peer()
            .ancestor_checkpoint(generation, ancestor.as_ref())?;
        anyhow::ensure!(
            reached == generation,
            "the peer's ancestor copy stands at generation {reached}, not {generation}"
        );
        Ok(())
    }

    /// P2P, after the ancestor advanced: sends the record to the replica,
    /// and a checkpoint if the copy could not apply it. Best effort — the
    /// ancestor is already recorded here, and a copy that misses a record
    /// asks for a checkpoint on the next one, so a failure costs a
    /// message and a larger transfer later, never the cycle.
    fn replicate_ancestor(&mut self, changes: &[Change]) {
        if self.leadership.is_none() {
            return;
        }
        let generation = self.ancestor_store.generation();
        let ancestor = self.ancestor.clone();
        let outcome = match self.peer().ancestor_record(generation, changes) {
            Ok(reached) if reached == generation => Ok(()),
            Ok(_) => self
                .peer()
                .ancestor_checkpoint(generation, ancestor.as_ref())
                .map(|_| ()),
            Err(error) => Err(error),
        };
        if let Err(error) = outcome {
            crate::complain!("unable to replicate the ancestor to the replica: {error:#}");
        }
    }

    /// Installs a hook the cycle calls at each [`CyclePoint`] it passes.
    ///
    /// A test seam, and nothing else uses it: this is how a test lands a
    /// write on either root at an exact point of a cycle — between a
    /// side's scan and its publish, say — which no amount of real editing
    /// reaches on purpose. The hook runs on the cycle's own thread, so the
    /// cycle is stopped for exactly as long as it takes.
    pub fn set_cycle_hook(&mut self, hook: CycleHook) {
        self.hook = Some(hook);
    }

    /// Passes a point of the cycle to the hook, if there is one.
    fn at(&mut self, point: CyclePoint) {
        if let Some(hook) = &mut self.hook {
            hook(point);
        }
    }

    /// Adopts the supervisor's progress record, so that what this session
    /// is doing is visible while it does it. Each endpoint gets the handle
    /// for its own side.
    pub fn set_progress(&mut self, progress: Arc<crate::progress::Progress>) {
        self.primary.set_scan_progress(progress.primary.clone());
        self.replica.set_scan_progress(progress.replica.clone());
        self.progress = progress;
    }

    /// Blocks until either endpoint signals that content may have changed,
    /// or the timeout elapses; returns whether a change was signaled.
    ///
    /// Both endpoints watch at once. Each is asked to begin a watch that
    /// raises the session's one signal, the session sleeps on that signal,
    /// and the first side to raise it ends the wait: a local endpoint
    /// raises it from the filesystem watcher, a remote one from the thread
    /// that holds its watch request open on the agent. Nothing is
    /// cancelled — a watch the other side beat is left standing and polled
    /// again next time, still meaning "nothing here since it began".
    ///
    /// (The wait used to be sliced between the endpoints, each asked in
    /// turn for half of 250 ms. A change on one side then waited out the
    /// other's half-slice, which put a 125 ms step in the tail of every
    /// single-editor session: p90 of 124 ms against a p50 of 46.)
    ///
    /// The sleep is bounded by `SLICE` regardless, so an endpoint that
    /// cannot watch at all still leaves the caller on a heartbeat.
    pub fn await_change(&mut self, timeout: std::time::Duration) -> Result<bool> {
        const SLICE: std::time::Duration = std::time::Duration::from_millis(250);
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            self.primary
                .watch_begin(remaining, Arc::clone(&self.wake))?;
            self.replica
                .watch_begin(remaining, Arc::clone(&self.wake))?;
            if self.primary.watch_poll()? == Some(true) || self.replica.watch_poll()? == Some(true)
            {
                return Ok(true);
            }
            self.wake.wait(remaining.min(SLICE));
            if self.primary.watch_poll()? == Some(true) || self.replica.watch_poll()? == Some(true)
            {
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
    ///
    /// A file still open for writing is the one exception to "quiet means
    /// done". A safe save — write a temporary file, flush it, rename it over
    /// the original — goes silent during the flush, and a cycle then would
    /// copy a temporary file that is renamed away before it arrives. So
    /// while a side reports a file open for writing, the settle waits past
    /// `maximum`, up to [`crate::endpoint::WRITE_GRACE`], for it to close.
    /// A save that closes the moment it has written — most of them — waits
    /// no longer than before. A save that begins after the settle is left
    /// out by the scan itself ([`crate::scan::scan_deferring`]); waiting
    /// here spares the cycle that would find nothing else to do.
    pub fn settle(&mut self, maximum: std::time::Duration, quiet: std::time::Duration) {
        let started = std::time::Instant::now();
        let deadline = started + maximum;
        let writing_deadline = started + crate::endpoint::WRITE_GRACE.max(maximum);
        let sample = |session: &mut Self| {
            (
                session.primary.change_activity(),
                session.replica.change_activity(),
            )
        };
        let writing = |sampled: &(Option<ChangeActivity>, Option<ChangeActivity>)| {
            [sampled.0, sampled.1]
                .iter()
                .any(|side| side.is_some_and(|activity| activity.writing))
        };
        let mut previous = sample(self);
        loop {
            let limit = match writing(&previous) {
                true => writing_deadline,
                false => deadline,
            };
            let slice = quiet.min(limit.saturating_duration_since(std::time::Instant::now()));
            if slice.is_zero() {
                break;
            }
            std::thread::sleep(slice);
            let current = sample(self);
            // Neither side recorded anything new across the slice, and
            // nothing is still being written: whatever triggered this
            // settle has finished arriving.
            if current == previous && !writing(&current) {
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

        // P2P: the lease goes first. A host that refuses it ends the
        // cycle before a single byte moves.
        self.present_lease()?;

        // Scan both endpoints in parallel — unless a side's watch, begun
        // from the generation its last scan or transition left, is still
        // standing unanswered: nothing has changed there, so its last
        // snapshot is the truth and the round trip is saved. A verifying
        // scan is never skipped; that is its point.
        self.progress.enter(crate::progress::Phase::Scanning);
        let verify = std::mem::take(&mut self.verify_next);
        let primary_cached = (!verify && self.primary.unchanged_since_scan())
            .then(|| self.primary.cached_snapshot())
            .flatten();
        let replica_cached = (!verify && self.replica.unchanged_since_scan())
            .then(|| self.replica.cached_snapshot())
            .flatten();
        report.primary_scan_skipped = primary_cached.is_some();
        report.replica_scan_skipped = replica_cached.is_some();
        let (primary_snapshot, replica_snapshot) = {
            let primary = &mut self.primary;
            let replica = &mut self.replica;
            std::thread::scope(|scope| {
                let primary_scan =
                    crate::threads::spawn_deep_scoped(scope, move || match primary_cached {
                        Some(snapshot) => Ok(snapshot),
                        None if verify => primary.scan_verified(),
                        None => primary.scan(),
                    });
                let replica_result = match replica_cached {
                    Some(snapshot) => Ok(snapshot),
                    None if verify => replica.scan_verified(),
                    None => replica.scan(),
                };
                let primary_result = primary_scan.join().expect("scan thread panicked");
                (primary_result, replica_result)
            })
        };
        let primary_snapshot = primary_snapshot.context("primary scan failed")?;
        let replica_snapshot = replica_snapshot.context("replica scan failed")?;

        // If both sides produced the very same hierarchies as the last cycle
        // — the same storage, not merely equal content — then nothing can
        // have changed since that cycle reconciled them, and reconciling
        // again would reach the same conclusion by the same full-tree walk.
        // The cycle that established this state is what makes the shortcut
        // sound: it is only taken after a cycle that had nothing left to do,
        // so "the same as last time" means "still synchronized".
        if self.quiesced
            && crate::tree::nodes_share_storage(
                self.settled_primary.as_ref(),
                primary_snapshot.root.as_ref(),
            )
            && crate::tree::nodes_share_storage(
                self.settled_replica.as_ref(),
                replica_snapshot.root.as_ref(),
            )
        {
            return Ok(report);
        }

        self.at(CyclePoint::AfterScans);

        for (side, root, problems) in [
            (0, &primary_snapshot.root, &mut report.primary_scan_problems),
            (1, &replica_snapshot.root, &mut report.replica_scan_problems),
        ] {
            let Some(root) = root else {
                self.scan_problems[side] = None;
                continue;
            };
            *problems = root.problems_since(
                self.scan_problems[side]
                    .as_ref()
                    .map(|(root, problems)| (root, problems.as_slice())),
            );
            self.scan_problems[side] = Some((root.clone(), problems.clone()));
        }

        // On a side whose filesystem can't preserve executability bits, the
        // scanned bits are noise; replace them from trusted references — the
        // other side (when it preserves bits) for byte-identical files, and
        // the ancestor otherwise — so reconciliation sees only real content
        // changes rather than phantom permission churn.
        let primary_root = if primary_snapshot.preserves_executability {
            primary_snapshot.root.clone()
        } else {
            let peer = replica_snapshot
                .preserves_executability
                .then_some(replica_snapshot.root.as_ref())
                .flatten();
            propagate_executability(self.ancestor.as_ref(), peer, primary_snapshot.root.as_ref())
        };
        let replica_root = if replica_snapshot.preserves_executability {
            replica_snapshot.root.clone()
        } else {
            let peer = primary_snapshot
                .preserves_executability
                .then_some(primary_snapshot.root.as_ref())
                .flatten();
            propagate_executability(self.ancestor.as_ref(), peer, replica_snapshot.root.as_ref())
        };

        let (primary_root, replica_root) = self.account_for_mounts(
            primary_root,
            replica_root,
            &primary_snapshot.mount_points,
            &replica_snapshot.mount_points,
        )?;

        if let Some((problem, format)) = self.unreadable.take() {
            self.rebuild_or_halt(
                problem,
                format,
                primary_root.as_ref(),
                replica_root.as_ref(),
            )?;
        }

        // Safety: if the ancestor root was a directory with non-trivial
        // content and exactly one side now presents an absent root, or one
        // holding nothing synchronizable, then halt rather than propagate
        // what is more likely an unmounted or wiped filesystem than an
        // intentional mass deletion.
        if one_side_emptied_root(
            self.ancestor.as_ref(),
            primary_root.as_ref(),
            replica_root.as_ref(),
        ) {
            bail!(SafetyHalt::RootEmptied);
        }

        // Reconcile.
        self.progress.enter(crate::progress::Phase::Reconciling);
        // Against the last reconciliation's inputs, so only what changed
        // since is walked: see `ReconcileMemo`.
        let reconciliation = crate::tree::reconcile_since(
            self.ancestor.as_ref(),
            primary_root.as_ref(),
            replica_root.as_ref(),
            self.policy(),
            self.reconcile_memo.as_ref(),
        );
        self.reconcile_memo = Some(crate::tree::ReconcileMemo::of(
            self.ancestor.as_ref(),
            primary_root.as_ref(),
            replica_root.as_ref(),
            &reconciliation,
        ));
        report.conflicts = reconciliation.conflicts;

        // Safety: refuse to propagate a root deletion.
        let contains_root_deletion = reconciliation
            .primary_transitions
            .iter()
            .chain(reconciliation.replica_transitions.iter())
            .any(Change::is_root_deletion);
        if contains_root_deletion {
            bail!(SafetyHalt::RootDeletion);
        }

        // What this cycle is about to touch, announced in the journal
        // *after* the first side's staging but before its transition. If
        // the process dies between the announcement and the achieved
        // record, the next run finds the intent unresolved and drops these
        // paths' provenance — a crash mid-cycle costs surfaced conflicts,
        // never a silent overwrite of a revert made while the tool was
        // down. That first staging is deliberately outside the announced
        // window: it mutates neither tree, it is the longest phase of a
        // large cycle, and a crash there must recover as the clean
        // propagation it still is rather than as conflict noise. When both
        // sides have transitions, primary's staging runs after replica's
        // transition, and so inside the window.
        let intended: Vec<String> = reconciliation
            .primary_transitions
            .iter()
            .chain(reconciliation.replica_transitions.iter())
            .map(|change| change.path.clone())
            .collect();
        let mut intent_recorded = false;

        // Stage and transition each side. Content flowing to replica is
        // supplied by primary and vice versa.
        let replica_outcome = if reconciliation.replica_transitions.is_empty() {
            None
        } else {
            stage(
                self.primary.as_mut(),
                self.replica.as_mut(),
                &reconciliation.replica_transitions,
                &self.progress,
            )?;
            if !intent_recorded {
                self.ancestor_store
                    .intend(&intended, self.remote_involved)?;
                intent_recorded = true;
            }
            self.progress
                .begin_applying(reconciliation.replica_transitions.len() as u64);
            self.at(CyclePoint::BeforeReplicaTransition);
            let outcome = self
                .replica
                .transition(reconciliation.replica_transitions.clone())
                .context("replica transition failed")?;
            self.at(CyclePoint::AfterReplicaTransition);
            self.progress
                .applied_reached(reconciliation.replica_transitions.len() as u64);
            Some(outcome)
        };
        let primary_outcome = if reconciliation.primary_transitions.is_empty() {
            None
        } else {
            stage(
                self.replica.as_mut(),
                self.primary.as_mut(),
                &reconciliation.primary_transitions,
                &self.progress,
            )?;
            if !intent_recorded {
                self.ancestor_store
                    .intend(&intended, self.remote_involved)?;
            }
            self.progress
                .begin_applying(reconciliation.primary_transitions.len() as u64);
            self.at(CyclePoint::BeforePrimaryTransition);
            let outcome = self
                .primary
                .transition(reconciliation.primary_transitions.clone())
                .context("primary transition failed")?;
            self.at(CyclePoint::AfterPrimaryTransition);
            self.progress
                .applied_reached(reconciliation.primary_transitions.len() as u64);
            Some(outcome)
        };
        self.at(CyclePoint::BeforeRecord);

        #[cfg(test)]
        if self.fail_before_record {
            bail!("test seam: crashed after transitions, before the achieved record");
        }

        // Fold transition results into ancestor changes: each transition's
        // achieved content becomes the ancestor's new content at that path.
        let mut ancestor_changes = reconciliation.ancestor_changes;
        let mut fold = |transitions: &[Change], outcome: &TransitionOutcome| -> Result<()> {
            ancestor_changes.extend(crate::endpoint::achieved_changes(transitions, outcome)?);
            Ok(())
        };
        if let Some(outcome) = &replica_outcome {
            fold(&reconciliation.replica_transitions, outcome)
                .context("replica transition failed")?;
            report.replica_transitions = reconciliation.replica_transitions.len();
            report.replica_transition_problems = outcome.problems.clone();
            report.missing_staged_files |= outcome.missing_staged_files;
            report
                .missing_staged
                .extend(outcome.missing_staged.iter().cloned());
        }
        if let Some(outcome) = &primary_outcome {
            fold(&reconciliation.primary_transitions, outcome)
                .context("primary transition failed")?;
            report.primary_transitions = reconciliation.primary_transitions.len();
            report.primary_transition_problems = outcome.problems.clone();
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
            // P2P: the replica's copy follows, after this side's record
            // is durable — a copy ahead of the truth is the one order
            // that can mislead a leader later.
            self.replicate_ancestor(&ancestor_changes);
        }

        // A cycle that applied nothing, hit no conflicts, and saw no
        // problems leaves the two sides synchronized as scanned. Recording
        // that storage lets the next cycle recognize an untouched pair
        // without walking either tree.
        self.quiesced = report.settled();
        if self.quiesced {
            self.settled_primary = primary_snapshot.root.clone();
            self.settled_replica = replica_snapshot.root.clone();
        } else {
            self.settled_primary = None;
            self.settled_replica = None;
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

/// The total size of the content a transfer is about to move.
///
/// A staging need names a path and a digest, not a length; the lengths are
/// on the nodes the transitions carry, so they are collected from there and
/// matched by digest — which is what a need is addressed by.
fn staged_bytes(transitions: &[Change], needs: &[StagingNeed]) -> u64 {
    let sizes = file_sizes(transitions);
    needs
        .iter()
        .map(|need| sizes.get(&need.request.digest).copied().unwrap_or(0))
        .sum()
}

/// Files no larger than this are sent to a remote destination before it
/// has said whether it needs them. An edit's content is new by
/// construction, so the answer is nearly always yes, and waiting for it
/// was a round trip on every edit; the waste when it is no — a small file
/// the destination already held under another name — is the file's own
/// size, once. Larger files wait for the answer, which for them may be a
/// signature that turns a transfer into a delta, or a "no" that saves the
/// whole thing.
const SPECULATIVE_MAX_BYTES: u64 = 64 * 1024;

/// Sizes of the files the transitions introduce, by digest.
fn file_sizes(transitions: &[Change]) -> std::collections::HashMap<Digest, u64> {
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
    sizes
}

/// Stages on `destination` the content its transitions need, from
/// `source`.
///
/// A destination that answers "what do you need?" on the spot — one in
/// this process — is asked and then supplied. One whose answer is a round
/// trip away is asked, and while the answer is in flight the small files
/// go anyway, in full; when the answer comes, whatever it still names is
/// supplied as it asked, deltas included. Content it turned out not to
/// need is read to the end on its side and dropped.
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
    let sizes = file_sizes(transitions);
    let needs = match destination
        .stage_begin_nowait(requests.clone())
        .context("unable to begin staging")?
    {
        Some(needs) => needs,
        None => {
            let mut speculated = std::collections::HashSet::new();
            let speculative: Vec<StagingNeed> = requests
                .iter()
                .filter(|request| {
                    sizes.get(&request.digest).copied().unwrap_or(u64::MAX) <= SPECULATIVE_MAX_BYTES
                })
                .filter(|request| speculated.insert(request.digest))
                .map(|request| StagingNeed {
                    request: request.clone(),
                    signature: crate::rsync::Signature::default(),
                })
                .collect();
            if !speculative.is_empty() {
                progress.begin_staging(
                    speculative.len() as u64,
                    staged_bytes(transitions, &speculative),
                );
                pump(source, destination, speculative, progress)?;
            }
            // When everything was sent, the answer cannot name anything
            // more to send, so there is nothing to wait for: the transition
            // goes out right behind the content, and the answer is read
            // along with the pushes' acknowledgements when the transition's
            // own answer is. That is the one round trip an edit costs.
            if requests
                .iter()
                .all(|request| speculated.contains(&request.digest))
            {
                return destination
                    .stage_finish()
                    .context("unable to complete staging");
            }
            let needs = destination
                .stage_begin_finish()
                .context("unable to begin staging")?;
            needs
                .into_iter()
                .filter(|need| !speculated.contains(&need.request.digest))
                .collect()
        }
    };
    if !needs.is_empty() {
        // What this transfer will move, announced before it starts so that
        // its progress can be read as a fraction rather than as a total
        // that only grows. The sizes come from the transitions themselves:
        // a need names a path and a digest, not a length.
        progress.begin_staging(needs.len() as u64, staged_bytes(transitions, &needs));
        pump(source, destination, needs, progress)?;
    }
    destination
        .stage_finish()
        .context("unable to complete staging")
}

/// Supplies `needs` from `source` and pushes them into `destination`.
fn pump(
    source: &mut dyn Endpoint,
    destination: &mut dyn Endpoint,
    needs: Vec<StagingNeed>,
    progress: &crate::progress::Progress,
) -> Result<()> {
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
                        TransferFrame::Op(_) | TransferFrame::Begin { .. } => {}
                    }
                }
                destination
                    .stage_push_nowait(frames)
                    .context("unable to push file content")?;
                progress.staged(files, bytes);
            }
            Ok(())
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
/// non-trivial, and exactly one side now presents an absent root, or one
/// holding nothing synchronizable, while the other retains content. An
/// ignored entry left behind (the `.DS_Store` of a bare mount point, the
/// `.git` of a wiped checkout) does not make a root any less emptied. Emptied directories *below* the
/// root are reconciliation's concern, and only where
/// `guard_dir_deletes_over` asks for it; a separate whole-tree pass here
/// measured at twenty milliseconds per cycle on a sixty-thousand-entry
/// tree, dominating the latency of every edit. The ancestor count — the
/// only expensive part — runs lazily, only when the rare one-side-empty
/// trigger fires.
fn one_side_emptied_root(
    ancestor: Option<&Node>,
    primary: Option<&Node>,
    replica: Option<&Node>,
) -> bool {
    let ancestor = match ancestor {
        Some(node) if matches!(node.content, Content::Directory(_)) => node,
        _ => return false,
    };
    let gone = |side: Option<&Node>| match side {
        None => true,
        Some(node) => !node.holds_synchronizable(),
    };
    if gone(primary) == gone(replica) {
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
/// trees in opposite directions conflict as well; a fan-out (one primary,
/// many replicas) and a relay (one's replica, another's primary) key differently
/// and stay legal.
pub struct EndpointPairLock {
    _lock: SessionLock,
}

impl EndpointPairLock {
    /// Acquires the pair lock for two endpoint identities.
    pub fn acquire(primary_identity: &str, replica_identity: &str) -> Result<EndpointPairLock> {
        // The *default* state root, deliberately — this directory must not
        // move with the overrides whose divergence it exists to catch.
        let root = crate::paths::default_state_root()?.join("endpoint-locks");
        EndpointPairLock::acquire_in(&root, primary_identity, replica_identity)
    }

    /// The lock directory name for a pair of endpoint identities, in either
    /// order. Exposed so `clean` can tell which lock directories belong to a
    /// configured pair and which are left over from pairs that no longer
    /// exist.
    pub fn key(primary_identity: &str, replica_identity: &str) -> String {
        let (first, second) = if primary_identity <= replica_identity {
            (primary_identity, replica_identity)
        } else {
            (replica_identity, primary_identity)
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
        primary_identity: &str,
        replica_identity: &str,
    ) -> Result<EndpointPairLock> {
        let (first, second) = if primary_identity <= replica_identity {
            (primary_identity, replica_identity)
        } else {
            (replica_identity, primary_identity)
        };
        let directory = root.join(EndpointPairLock::key(primary_identity, replica_identity));
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
        let path = state_directory.join("lock");
        let deadline = std::time::Instant::now() + LOCK_ACQUISITION_TIMEOUT;
        loop {
            fs::create_dir_all(&state_directory).with_context(|| {
                format!(
                    "unable to create session state directory {}",
                    state_directory.display()
                )
            })?;
            let file = File::options()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)
                .with_context(|| format!("unable to open session lock {}", path.display()))?;
            // Waits for the lock on this file, or gives up at the deadline.
            loop {
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                    break;
                }
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::WouldBlock {
                    return Err(error).with_context(|| {
                        format!("unable to lock session state {}", path.display())
                    });
                }
                if std::time::Instant::now() >= deadline {
                    return Err(SessionLockHeld {
                        state_directory: state_directory.display().to_string(),
                    }
                    .into());
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            // The lock is on the file this handle opened. Whoever held it
            // before may have removed that file (as `clean` does) while
            // this waited, and a lock on a removed file excludes nobody: a
            // newcomer would create a fresh file and lock that too. So the
            // lock counts only while the path still names the locked file.
            if lock_still_named(&file, &path) {
                return Ok(SessionLock {
                    state_directory,
                    _file: file,
                });
            }
            if std::time::Instant::now() >= deadline {
                return Err(SessionLockHeld {
                    state_directory: state_directory.display().to_string(),
                }
                .into());
            }
        }
    }

    /// Returns the locked state directory.
    pub fn state_directory(&self) -> &Path {
        &self.state_directory
    }
}

/// Whether `path` still names the file `file` was opened from: same device
/// and inode. False when the file was removed or replaced since.
fn lock_still_named(file: &File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (file.metadata(), fs::metadata(path)) {
        (Ok(held), Ok(named)) => held.dev() == named.dev() && held.ino() == named.ino(),
        _ => false,
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

    /// A lock whose file is removed while another waits for it is not
    /// handed to the waiter: the waiter ends up holding the lock on the file
    /// the path names now. Otherwise two holders could coexist, one on the
    /// removed file and one on a fresh one.
    #[test]
    fn a_lock_removed_while_waited_for_is_taken_afresh() {
        let keep = tempfile::tempdir().expect("tempdir");
        let directory = keep.path().join("pair");
        let first = SessionLock::acquire(directory.clone()).expect("first lock");
        let waiter = {
            let directory = directory.clone();
            std::thread::spawn(move || SessionLock::acquire(directory))
        };
        std::thread::sleep(std::time::Duration::from_millis(200));
        // What `clean` does: remove the lock's file while holding it.
        fs::remove_file(directory.join("lock")).expect("remove the lock file");
        drop(first);
        let second = waiter
            .join()
            .expect("the waiter finishes")
            .expect("the waiter gets the lock");
        assert!(lock_still_named(&second._file, &directory.join("lock")));
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
        // shape the original guard missed: reproduced deleting replica's
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

        // Emptiness is judged by what synchronizes. A bare mount point
        // keeps its `.DS_Store`, a wipe leaves the `.git`: the root has
        // lost everything that syncs, and an ignored entry left in it
        // must not make it read as a root that merely lost files.
        let untracked = |name: &str| Node {
            name: name.into(),
            content: Content::Untracked,
        };
        let ds_store = Node::directory("", vec![untracked(".DS_Store")]);
        assert!(one_side_emptied_root(
            Some(&ancestor),
            Some(&ds_store),
            Some(&ancestor)
        ));
        let git = Node::directory("", vec![untracked(".git")]);
        assert!(one_side_emptied_root(
            Some(&ancestor),
            Some(&ancestor),
            Some(&git)
        ));
        // One real file beside the ignored entry is a root that lost
        // files, not an emptied one.
        let kept = Node::directory("", vec![file("a", 1), untracked(".DS_Store")]);
        assert!(!one_side_emptied_root(
            Some(&ancestor),
            Some(&kept),
            Some(&ancestor)
        ));
    }

    /// Reproduced before the fix: an ancestor of twenty files, primary down
    /// to one untracked `.DS_Store`, replica untouched. The halt did not fire,
    /// because primary's root had a child, and reconciliation deleted all
    /// twenty files from replica — in every mode, the paranoid one included.
    #[test]
    fn a_root_emptied_down_to_an_ignored_entry_halts_in_every_mode() {
        let full = || Node::directory("", (1..=20).map(|i| file(&format!("f{i}"), i)).collect());
        let emptied = || {
            Node::directory(
                "",
                vec![Node {
                    name: ".DS_Store".into(),
                    content: Content::Untracked,
                }],
            )
        };
        for mode in [
            SyncMode::TwoWaySafe,
            SyncMode::TwoWayResolved,
            SyncMode::TwoWayStrict,
            SyncMode::OneWaySafe,
            SyncMode::OneWayMirror,
        ] {
            let state = tempfile::tempdir().unwrap();
            let transitions = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let primary = ScriptedEndpoint::new(vec![scripted(full()), scripted(emptied())]);
            let replica = CountingEndpoint {
                inner: ScriptedEndpoint::new(vec![scripted(full())]),
                transitions: std::sync::Arc::clone(&transitions),
            };
            let mut session = Session::new(
                Box::new(primary),
                Box::new(replica),
                mode,
                state.path().to_path_buf(),
            )
            .unwrap();
            session.run_cycle().expect("the first cycle converges");
            let settled = transitions.load(std::sync::atomic::Ordering::SeqCst);
            let error = session
                .run_cycle()
                .expect_err("a root emptied down to an ignored entry must halt");
            assert!(
                matches!(
                    error.downcast_ref::<SafetyHalt>(),
                    Some(SafetyHalt::RootEmptied)
                ),
                "{mode:?}: {error:#}"
            );
            assert_eq!(
                transitions.load(std::sync::atomic::Ordering::SeqCst),
                settled,
                "{mode:?}: replica must not be transitioned"
            );
        }
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
        /// What each `change_activity` sample returns, in turn; the last
        /// one repeats. Empty reports nothing, as a remote endpoint does.
        activity: std::collections::VecDeque<ChangeActivity>,
    }

    impl ScriptedEndpoint {
        fn new(snapshots: Vec<crate::tree::Snapshot>) -> ScriptedEndpoint {
            ScriptedEndpoint {
                snapshots: snapshots.into(),
                last: None,
                activity: std::collections::VecDeque::new(),
            }
        }
    }

    impl Endpoint for ScriptedEndpoint {
        fn change_activity(&mut self) -> Option<ChangeActivity> {
            match self.activity.len() {
                0 => None,
                1 => self.activity.front().copied(),
                _ => self.activity.pop_front(),
            }
        }

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

    /// A handoff brings the peer's copy of the ancestor level first: a peer
    /// already at the leader's generation is sent nothing, one elsewhere is
    /// sent a checkpoint at it, and one that does not arrive there is an
    /// error — the automatic handback then waits for the next settled cycle.
    /// A session that does not lead replicates nothing.
    #[test]
    fn leveling_the_copy_checkpoints_a_peer_that_is_behind() {
        use std::sync::{Arc, Mutex};
        #[derive(Default)]
        struct Copy {
            reports: u64,
            answers: Option<u64>,
            checkpoints: Vec<u64>,
        }
        struct Peer(ScriptedEndpoint, Arc<Mutex<Copy>>);
        impl Endpoint for Peer {
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
            fn p2p_state(&mut self) -> Result<crate::p2p::State> {
                Ok(crate::p2p::State {
                    lease: None,
                    generation: Some(self.1.lock().unwrap().reports),
                })
            }
            fn ancestor_checkpoint(&mut self, generation: u64, _: Option<&Node>) -> Result<u64> {
                let mut copy = self.1.lock().unwrap();
                copy.checkpoints.push(generation);
                Ok(copy.answers.unwrap_or(generation))
            }
        }

        let keep = tempfile::tempdir().unwrap();
        let state = keep.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let base = || Node::directory("", vec![file("x", 1)]);
        let copy = Arc::new(Mutex::new(Copy::default()));
        let mut session = Session::new(
            Box::new(ScriptedEndpoint::new(vec![scripted(base())])),
            Box::new(Peer(
                ScriptedEndpoint::new(vec![scripted(base())]),
                copy.clone(),
            )),
            SyncMode::TwoWaySafe,
            state,
        )
        .unwrap();
        session.run_cycle().expect("the first cycle converges");
        let generation = session.ancestor_store.generation();
        assert!(generation > 0);

        session
            .level_the_copy()
            .expect("nothing to level without leading");
        assert!(copy.lock().unwrap().checkpoints.is_empty());

        session.set_leadership(
            Some(crate::p2p::Leadership {
                leader: crate::p2p::PRIMARY.into(),
                term: 1,
                ttl: std::time::Duration::from_secs(30),
            }),
            crate::p2p::PeerSide::Replica,
        );
        copy.lock().unwrap().reports = generation;
        session.level_the_copy().expect("already level");
        assert!(copy.lock().unwrap().checkpoints.is_empty());

        copy.lock().unwrap().reports = generation - 1;
        session.level_the_copy().expect("brought level");
        assert_eq!(copy.lock().unwrap().checkpoints, [generation]);

        copy.lock().unwrap().answers = Some(0);
        let error = session
            .level_the_copy()
            .expect_err("the copy did not arrive");
        assert!(
            format!("{error:#}").contains("stands at generation 0"),
            "{error:#}"
        );
    }

    /// Finding L-19: an endpoint — a remote one, or a buggy one — that
    /// reports fewer results than it was given transitions used to have
    /// the rest dropped by a `zip`, and the ancestor recorded less than was
    /// attempted. The mismatch fails the cycle, and the ancestor stands.
    #[test]
    fn an_endpoint_short_of_results_fails_the_cycle_and_leaves_the_ancestor() {
        struct ShortOfResults(ScriptedEndpoint);
        impl Endpoint for ShortOfResults {
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
                let mut outcome = self.0.transition(transitions)?;
                outcome.results.truncate(1);
                Ok(outcome)
            }
        }

        let keep = tempfile::tempdir().unwrap();
        let state = keep.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let base = || Node::directory("", vec![file("x", 1)]);
        let grown = || Node::directory("", vec![file("a", 2), file("b", 3), file("x", 1)]);
        let primary = ScriptedEndpoint::new(vec![scripted(base()), scripted(grown())]);
        let replica = ShortOfResults(ScriptedEndpoint::new(vec![scripted(base())]));
        let mut session = Session::new(
            Box::new(primary),
            Box::new(replica),
            SyncMode::TwoWaySafe,
            state.clone(),
        )
        .unwrap();
        session.run_cycle().expect("the first cycle converges");

        let error = match session.run_cycle() {
            Ok(report) => panic!(
                "two transitions and one result must fail the cycle, not record {} \
                 replica transitions",
                report.replica_transitions
            ),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("results"), "{error:#}");
        let held = session.ancestor.clone().expect("the ancestor stands");
        assert!(held.content_equal(&base(), true), "the ancestor moved");
        drop(session);
        let (_, stored, unresolved) =
            ancestor::AncestorStore::open(&state.join("ancestor")).unwrap();
        assert!(stored.expect("stored").content_equal(&base(), true));
        assert_eq!(
            unresolved.len(),
            2,
            "both transitions keep their taint: neither is known to have landed"
        );
    }

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
        // new content to replica and then "crashes" at the seam.
        {
            let primary =
                ScriptedEndpoint::new(vec![scripted(old_content()), scripted(new_content())]);
            let replica = ScriptedEndpoint::new(vec![scripted(old_content())]);
            let mut session = Session::new(
                Box::new(primary),
                Box::new(replica),
                SyncMode::TwoWaySafe,
                state.clone(),
            )
            .unwrap();
            session.run_cycle().expect("first cycle converges");
            session.fail_before_record = true;
            let error = session.run_cycle().expect_err("the seam must fire");
            assert!(format!("{error:#}").contains("test seam"), "{error:#}");
        }

        // While the tool was down, the user deliberately reverted primary.
        // Replica holds the propagated new content (the transition landed).
        let transitions = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let primary = CountingEndpoint {
            inner: ScriptedEndpoint::new(vec![scripted(old_content())]),
            transitions: std::sync::Arc::clone(&transitions),
        };
        let replica = ScriptedEndpoint::new(vec![scripted(new_content())]);
        let mut session = Session::new(
            Box::new(primary),
            Box::new(replica),
            SyncMode::TwoWaySafe,
            state,
        )
        .unwrap();
        let report = session.run_cycle().expect("the recovery cycle runs");
        assert!(
            !report.conflicts.is_empty(),
            "unknown provenance must surface as a conflict"
        );
        assert_eq!(
            transitions.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the revert must not be overwritten: without the intent record, \
             the stale ancestor read primary as unchanged and replica as modified \
             and pushed the new content back over the revert"
        );
    }

    /// Builds a snapshot around a root, sharing the root's storage across
    /// clones — which is what an unchanged rescan produces.
    fn scripted(root: Node) -> crate::tree::Snapshot {
        crate::tree::Snapshot {
            root: Some(root),
            preserves_executability: true,
            ..crate::tree::Snapshot::default()
        }
    }

    fn settling(primary_activity: Vec<ChangeActivity>) -> (Session, tempfile::TempDir) {
        let root = || scripted(Node::directory("", vec![file("shared", 1)]));
        let mut primary = ScriptedEndpoint::new(vec![root()]);
        primary.activity = primary_activity.into();
        let state = tempfile::tempdir().unwrap();
        let session = Session::new(
            Box::new(primary),
            Box::new(ScriptedEndpoint::new(vec![root()])),
            SyncMode::TwoWaySafe,
            state.path().to_path_buf(),
        )
        .expect("session should be creatable");
        (session, state)
    }

    fn activity(events: u64, writing: bool) -> ChangeActivity {
        ChangeActivity {
            paths: 1,
            incomplete: false,
            events,
            writing,
        }
    }

    /// A safe save goes quiet while its temporary file is flushed: a quiet
    /// slice then is not the end of the save while the file is still open
    /// for writing. The settle waits for it to close, past its usual
    /// ceiling — here twenty slices of a file flushing, then the close.
    #[test]
    fn a_file_still_open_for_writing_holds_the_settle_until_it_closes() {
        let mut samples = vec![activity(1, true); 20];
        samples.push(activity(2, false));
        let (mut session, _state) = settling(samples);
        let started = std::time::Instant::now();
        session.settle(
            std::time::Duration::from_millis(25),
            std::time::Duration::from_millis(5),
        );
        let waited = started.elapsed();
        assert!(
            waited >= std::time::Duration::from_millis(100),
            "{waited:?}"
        );
        assert!(waited < crate::endpoint::WRITE_GRACE, "{waited:?}");

        // Nothing open: quiet after one slice, as before.
        let (mut session, _state) = settling(vec![activity(1, false)]);
        let started = std::time::Instant::now();
        session.settle(
            std::time::Duration::from_millis(25),
            std::time::Duration::from_millis(5),
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(25));
    }

    /// A file that stays open — a log, a database — holds a settle back no
    /// longer than the grace.
    #[test]
    fn a_file_that_never_closes_holds_the_settle_for_the_grace_at_most() {
        let (mut session, _state) = settling(vec![activity(1, true)]);
        let started = std::time::Instant::now();
        session.settle(
            std::time::Duration::from_millis(25),
            std::time::Duration::from_millis(5),
        );
        let waited = started.elapsed();
        assert!(waited >= crate::endpoint::WRITE_GRACE, "{waited:?}");
        assert!(
            waited < crate::endpoint::WRITE_GRACE + std::time::Duration::from_millis(200),
            "{waited:?}"
        );
    }

    #[test]
    fn an_unchanged_pair_skips_reconciliation_only_after_a_settled_cycle() {
        let primary_root = Node::directory("", vec![file("shared", 1)]);
        let replica_root = Node::directory("", vec![file("shared", 1)]);
        // Every scan reproduces the same storage, as an unchanged rescan
        // does. The first cycle must still reconcile — nothing has settled
        // yet — and later ones may skip.
        let primary = ScriptedEndpoint::new(vec![scripted(primary_root)]);
        let replica = ScriptedEndpoint::new(vec![scripted(replica_root)]);
        let state = tempfile::tempdir().unwrap();
        let mut session = Session::new(
            Box::new(primary),
            Box::new(replica),
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
        // change on primary, which must not be skipped.
        let primary = ScriptedEndpoint::new(vec![
            scripted(settled.clone()),
            scripted(settled.clone()),
            scripted(edited),
        ]);
        let applied = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let replica = CountingEndpoint {
            inner: ScriptedEndpoint::new(vec![scripted(settled.clone()), scripted(settled)]),
            transitions: std::sync::Arc::clone(&applied),
        };
        let state = tempfile::tempdir().unwrap();
        let mut session = Session::new(
            Box::new(primary),
            Box::new(replica),
            SyncMode::TwoWaySafe,
            state.path().to_path_buf(),
        )
        .expect("session should be creatable");

        assert!(!session.run_cycle().expect("cycle should succeed").changed());
        assert!(!session.run_cycle().expect("cycle should succeed").changed());
        assert!(session.quiesced, "two identical cycles should settle");

        // Primary's edit arrives on a settled session: the shortcut must not
        // fire, because primary's storage no longer matches what settled.
        let report = session.run_cycle().expect("cycle should succeed");
        assert_eq!(report.replica_transitions, 1, "the edit was not propagated");
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
        let primary = ScriptedEndpoint::new(vec![scripted(problematic.clone())]);
        let replica = ScriptedEndpoint::new(vec![scripted(problematic)]);
        let state = tempfile::tempdir().unwrap();
        let mut session = Session::new(
            Box::new(primary),
            Box::new(replica),
            SyncMode::TwoWaySafe,
            state.path().to_path_buf(),
        )
        .expect("session should be creatable");

        for cycle in 0..3 {
            let report = session.run_cycle().expect("cycle should succeed");
            assert_eq!(
                report.primary_scan_problems.len(),
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

        // Primary preserves executability; replica doesn't, and after the first
        // cycle its scans report the file spuriously executable (the classic
        // FAT-style noise). The third primary scan carries a *real*
        // executability change.
        let primary = ScriptedEndpoint::new(vec![
            snapshot(Node::directory("", vec![tool(false)]), true),
            snapshot(Node::directory("", vec![tool(false)]), true),
            snapshot(Node::directory("", vec![tool(true)]), true),
        ]);
        let replica = ScriptedEndpoint::new(vec![
            snapshot(Node::directory("", vec![]), false),
            snapshot(Node::directory("", vec![tool(true)]), false),
            snapshot(Node::directory("", vec![tool(true)]), false),
        ]);

        let state = tempfile::tempdir().unwrap();
        let mut session = Session::new(
            Box::new(primary),
            Box::new(replica),
            SyncMode::TwoWaySafe,
            state.path().join("session"),
        )
        .expect("the session should construct");

        // Cycle 1 creates the file on replica and establishes the ancestor.
        let report = session.run_cycle().expect("cycle 1");
        assert_eq!(report.replica_transitions, 1);

        // Cycle 2: replica's spurious bit is grafted away; nothing propagates
        // (without propagation this would emit a transition to primary).
        let report = session.run_cycle().expect("cycle 2");
        assert!(!report.changed(), "{report:?}");
        assert!(report.conflicts.is_empty());

        // Cycle 3: primary makes a *real* executability change. Replica's copy
        // holds the same bytes, so the preserving peer vouches for the new
        // bit directly — the sides agree immediately, with no transition
        // and no conflict (the case a purely ancestor-based graft would
        // have reported as a false conflict).
        let report = session.run_cycle().expect("cycle 3");
        assert!(!report.changed(), "{report:?}");
        assert!(report.conflicts.is_empty(), "{report:?}");
    }

    /// Keeping a version that has not changed since the last sync: the
    /// losing copy is retired, and the ancestor forgets the path, so the
    /// next cycle carries the kept version as a creation. Without the
    /// forgetting, the same retirement reads as a deletion against an
    /// untouched copy, and the kept version is lost from both sides.
    #[test]
    fn a_settlement_keeps_an_unchanged_winner_only_by_forgetting() {
        use crate::endpoint::local::LocalEndpoint;

        for forget in [true, false] {
            let keep = tempfile::tempdir().unwrap();
            let endpoint = |name: &str| -> Box<dyn crate::endpoint::Endpoint + Send> {
                Box::new(
                    LocalEndpoint::new(
                        keep.path().join(name),
                        keep.path().join(format!("staging-{name}")),
                        crate::endpoint::local::EndpointOptions::default(),
                    )
                    .unwrap(),
                )
            };
            for name in ["primary", "replica"] {
                fs::create_dir_all(keep.path().join(name)).unwrap();
            }
            fs::write(keep.path().join("primary/keep.txt"), "original").unwrap();
            fs::write(keep.path().join("primary/other.txt"), "other").unwrap();
            let mut session = Session::new(
                endpoint("primary"),
                endpoint("replica"),
                SyncMode::TwoWaySafe,
                keep.path().join("state"),
            )
            .unwrap();
            cycle_to_quiescence(&mut session);
            fs::write(keep.path().join("replica/keep.txt"), "replica's edit").unwrap();
            // Long enough for the session's watch to hear of it, as it has
            // long since when someone resolves a conflict they can see.
            std::thread::sleep(std::time::Duration::from_millis(200));

            // Replica's copy as resolve would read it.
            let mut reader = endpoint("replica");
            let scanned = reader.scan().unwrap().root;
            let expectation = crate::tree::node_at(scanned.as_ref(), "keep.txt")
                .and_then(Node::synchronizable_subtree)
                .unwrap();
            let outcome = session
                .apply_settlement(&Settlement {
                    forget: match forget {
                        true => vec!["keep.txt".into()],
                        false => Vec::new(),
                    },
                    retire: vec![(
                        Side::Replica,
                        "keep.txt".into(),
                        Retirement::Remove(expectation),
                    )],
                })
                .unwrap();
            assert_eq!(outcome.settled, vec!["keep.txt".to_string()], "{outcome:?}");
            for _ in 0..3 {
                session.run_cycle().unwrap();
            }
            let read = |side: &str| fs::read_to_string(keep.path().join(side).join("keep.txt"));
            if forget {
                assert_eq!(read("primary").unwrap(), "original");
                assert_eq!(read("replica").unwrap(), "original");
            } else {
                assert!(read("primary").is_err() && read("replica").is_err());
            }
        }
    }

    /// A copy that changed after `resolve` read it is refused by path and
    /// left alone; the ancestor has already forgotten the path, so the two
    /// versions surface as a conflict rather than one silently winning.
    #[test]
    fn a_settlement_refuses_a_copy_changed_since_it_was_read() {
        use crate::endpoint::local::LocalEndpoint;

        let keep = tempfile::tempdir().unwrap();
        let endpoint = |name: &str| -> Box<dyn crate::endpoint::Endpoint + Send> {
            Box::new(
                LocalEndpoint::new(
                    keep.path().join(name),
                    keep.path().join(format!("staging-{name}")),
                    crate::endpoint::local::EndpointOptions::default(),
                )
                .unwrap(),
            )
        };
        for name in ["primary", "replica"] {
            fs::create_dir_all(keep.path().join(name)).unwrap();
        }
        fs::write(keep.path().join("primary/keep.txt"), "original").unwrap();
        fs::write(keep.path().join("primary/other.txt"), "other").unwrap();
        let mut session = Session::new(
            endpoint("primary"),
            endpoint("replica"),
            SyncMode::TwoWaySafe,
            keep.path().join("state"),
        )
        .unwrap();
        cycle_to_quiescence(&mut session);
        fs::write(keep.path().join("replica/keep.txt"), "replica's edit").unwrap();
        let mut reader = endpoint("replica");
        let scanned = reader.scan().unwrap().root;
        let expectation = crate::tree::node_at(scanned.as_ref(), "keep.txt")
            .and_then(Node::synchronizable_subtree)
            .unwrap();
        fs::write(
            keep.path().join("replica/keep.txt"),
            "replica's later edit!",
        )
        .unwrap();

        let outcome = session
            .apply_settlement(&Settlement {
                forget: vec!["keep.txt".into()],
                retire: vec![(
                    Side::Replica,
                    "keep.txt".into(),
                    Retirement::Remove(expectation),
                )],
            })
            .unwrap();
        assert!(outcome.settled.is_empty(), "{outcome:?}");
        assert_eq!(outcome.refused.len(), 1, "{outcome:?}");
        let report = session.run_cycle().unwrap();
        assert_eq!(report.conflicts.len(), 1, "{report:?}");
        assert_eq!(
            fs::read_to_string(keep.path().join("replica/keep.txt")).unwrap(),
            "replica's later edit!"
        );
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
            let settled = report.primary_transitions == 0
                && report.replica_transitions == 0
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
    fn a_crash_at(primary_fault: Option<Fault>, replica_fault: Option<Fault>) {
        let keep = tempfile::tempdir().unwrap();
        let primary_root = keep.path().join("primary");
        let replica_root = keep.path().join("replica");
        let primary_staging = keep.path().join("staging-primary");
        let replica_staging = keep.path().join("staging-replica");
        let state = keep.path().join("state");
        std::fs::create_dir_all(&primary_root).unwrap();
        std::fs::create_dir_all(&replica_root).unwrap();
        std::fs::create_dir_all(&state).unwrap();

        let old_modify = bytes(1, 96 * 1024);
        let new_modify = bytes(2, 120 * 1024);
        let old_delete = bytes(3, 48 * 1024);
        let created = bytes(4, 160 * 1024);
        let own = bytes(5, 24 * 1024);

        // Converge on the initial content.
        std::fs::write(primary_root.join("modify.txt"), &old_modify).unwrap();
        std::fs::write(primary_root.join("delete.txt"), &old_delete).unwrap();
        std::fs::write(primary_root.join("keep.txt"), b"keep").unwrap();
        {
            let mut session = Session::new(
                lifecycle_endpoint(&primary_root, &primary_staging, None),
                lifecycle_endpoint(&replica_root, &replica_staging, None),
                SyncMode::TwoWaySafe,
                state.clone(),
            )
            .unwrap();
            let report = cycle_to_quiescence(&mut session);
            assert!(report.conflicts.is_empty(), "{:?}", report.conflicts);
        }
        assert_eq!(
            read_or_absent(&replica_root, "keep.txt").as_deref(),
            Some(&b"keep"[..])
        );

        // The divergence the crashed cycle will be propagating.
        std::fs::write(primary_root.join("modify.txt"), &new_modify).unwrap();
        std::fs::write(primary_root.join("created.bin"), &created).unwrap();
        std::fs::remove_file(primary_root.join("delete.txt")).unwrap();
        std::fs::write(replica_root.join("replica_own.txt"), &own).unwrap();

        // The crash.
        {
            let mut session = Session::new(
                lifecycle_endpoint(&primary_root, &primary_staging, primary_fault),
                lifecycle_endpoint(&replica_root, &replica_staging, replica_fault),
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
            assert_untorn(root, "replica_own.txt", &[None, Some(own.clone())]);
        };
        untorn(&primary_root);
        untorn(&replica_root);

        // Recovery: a fresh session over the same state, no faults.
        let report = {
            let mut session = Session::new(
                lifecycle_endpoint(&primary_root, &primary_staging, None),
                lifecycle_endpoint(&replica_root, &replica_staging, None),
                SyncMode::TwoWaySafe,
                state,
            )
            .unwrap();
            cycle_to_quiescence(&mut session)
        };

        // Still nothing torn, crash noise stays on the crashed cycle's own
        // paths, and every unconflicted path agrees between the sides.
        untorn(&primary_root);
        untorn(&replica_root);
        let conflicted: Vec<&str> = report
            .conflicts
            .iter()
            .map(|conflict| conflict.root.as_str())
            .collect();
        for path in &conflicted {
            assert!(
                ["modify.txt", "created.bin", "delete.txt", "replica_own.txt"].contains(path),
                "a conflict appeared off the crashed cycle's paths: {path}"
            );
        }
        for path in [
            "modify.txt",
            "created.bin",
            "delete.txt",
            "keep.txt",
            "replica_own.txt",
        ] {
            if !conflicted.contains(&path) {
                assert_eq!(
                    read_or_absent(&primary_root, path),
                    read_or_absent(&replica_root, path),
                    "{path} is unconflicted but the sides disagree"
                );
            }
        }
        // The edit made on replica while the crash was in flight is never lost.
        assert_eq!(read_or_absent(&replica_root, "replica_own.txt"), Some(own));

        // Faults during staging precede the intent record, so recovery owes
        // full convergence with no conflict noise at all.
        let staging_fault = matches!(
            primary_fault.or(replica_fault),
            Some(Fault::StageBegin | Fault::StagePushTruncate | Fault::SupplyPull)
        );
        if staging_fault {
            assert!(report.conflicts.is_empty(), "{:?}", report.conflicts);
            assert_eq!(
                read_or_absent(&primary_root, "modify.txt"),
                Some(new_modify.clone())
            );
            assert_eq!(
                read_or_absent(&replica_root, "modify.txt"),
                Some(new_modify)
            );
            assert_eq!(read_or_absent(&replica_root, "created.bin"), Some(created));
            assert_eq!(read_or_absent(&replica_root, "delete.txt"), None);
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
        let primary_root = keep.path().join("primary");
        let replica_root = keep.path().join("replica");
        let state = keep.path().join("state");
        std::fs::create_dir_all(&primary_root).unwrap();
        std::fs::create_dir_all(&replica_root).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let content = bytes(9, 200 * 1024);
        std::fs::write(primary_root.join("payload.bin"), &content).unwrap();

        let mut session = Session::new(
            lifecycle_endpoint(&primary_root, &keep.path().join("staging-primary"), None),
            lifecycle_endpoint(
                &replica_root,
                &keep.path().join("staging-replica"),
                Some(Fault::CorruptFrames),
            ),
            SyncMode::TwoWaySafe,
            state,
        )
        .unwrap();

        // The corrupted cycle: staging discards the mismatched content, so
        // the transition reports it missing and nothing lands on replica —
        // wrong bytes above all.
        let report = session.run_cycle().expect("a corrupted cycle still runs");
        assert_untorn(&replica_root, "payload.bin", &[None, Some(content.clone())]);
        assert!(
            report.missing_staged_files || read_or_absent(&replica_root, "payload.bin").is_some(),
            "the corrupted transfer neither landed nor was reported missing"
        );

        // The corruption stops (the armed fault is consumed by replacing
        // the endpoint), and the next session converges on correct bytes.
        drop(session);
        let mut session = Session::new(
            lifecycle_endpoint(&primary_root, &keep.path().join("staging-primary"), None),
            lifecycle_endpoint(&replica_root, &keep.path().join("staging-replica"), None),
            SyncMode::TwoWaySafe,
            keep.path().join("state"),
        )
        .unwrap();
        let report = cycle_to_quiescence(&mut session);
        assert!(report.conflicts.is_empty(), "{:?}", report.conflicts);
        assert_eq!(read_or_absent(&replica_root, "payload.bin"), Some(content));
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
            let primary = ScriptedEndpoint::new(vec![scripted(content())]);
            let replica: Box<dyn Endpoint + Send> = if remote {
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
                Session::new(Box::new(primary), replica, SyncMode::TwoWaySafe, state).unwrap();
            session.run_cycle().expect("the cycle runs");
            session.ancestor_store.append_syncs
        };
        assert_eq!(cycle_syncs(true), 1, "a remote session syncs its intent");
        assert_eq!(cycle_syncs(false), 0, "a local session does not");
    }

    /// A snapshot whose scan reported mount points.
    fn mounted(root: Node, mounts: &[&str]) -> crate::tree::Snapshot {
        crate::tree::Snapshot {
            mount_points: mounts.iter().map(|path| path.to_string()).collect(),
            ..scripted(root)
        }
    }

    fn untracked(name: &str) -> Node {
        Node {
            name: name.into(),
            content: Content::Untracked,
        }
    }

    #[test]
    fn a_mount_is_left_alone_on_both_sides_and_unplugging_it_moves_nothing() {
        // Primary has something mounted at `mnt`, which its scan left out;
        // replica has a real directory there with its own file in it.
        let primary_first = mounted(
            Node::directory("", vec![file("a", 1), untracked("mnt")]),
            &["mnt"],
        );
        // Unplugged: the mount point is an empty directory again.
        let primary_unplugged = scripted(Node::directory(
            "",
            vec![file("a", 1), Node::directory("mnt", Vec::new())],
        ));
        let replica = scripted(Node::directory(
            "",
            vec![file("a", 1), Node::directory("mnt", vec![file("own", 2)])],
        ));
        let applied = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let state = tempfile::tempdir().unwrap();
        let mut session = Session::new(
            Box::new(ScriptedEndpoint::new(vec![
                primary_first,
                primary_unplugged,
            ])),
            Box::new(CountingEndpoint {
                inner: ScriptedEndpoint::new(vec![replica]),
                transitions: std::sync::Arc::clone(&applied),
            }),
            SyncMode::TwoWaySafe,
            state.path().to_path_buf(),
        )
        .unwrap();
        let report = session.run_cycle().expect("mounted");
        assert!(report.conflicts.is_empty(), "{:?}", report.conflicts);
        assert_eq!(
            report.primary_transitions + report.replica_transitions,
            0,
            "{report:?}"
        );
        let report = session.run_cycle().expect("unplugged");
        assert!(report.conflicts.is_empty(), "{:?}", report.conflicts);
        assert_eq!(
            report.primary_transitions + report.replica_transitions,
            0,
            "an unplugged mount copies nothing into its empty mount point: {report:?}"
        );
        assert_eq!(applied.load(std::sync::atomic::Ordering::Relaxed), 0);
        // Remembered where the next run will find it.
        let recorded = std::fs::read_to_string(state.path().join("mounts")).unwrap();
        assert!(recorded.contains("mnt"), "{recorded}");
    }

    #[test]
    fn a_followed_mount_that_vanishes_halts_instead_of_deleting_its_content() {
        let full = Node::directory("mnt", vec![file("data", 3)]);
        let primary_mounted = mounted(Node::directory("", vec![full.clone()]), &["mnt"]);
        let primary_gone = scripted(Node::directory(
            "",
            vec![Node::directory("mnt", Vec::new())],
        ));
        let replica = scripted(Node::directory("", vec![full]));
        let state = tempfile::tempdir().unwrap();
        let mut session = Session::new(
            Box::new(ScriptedEndpoint::new(vec![primary_mounted, primary_gone])),
            Box::new(ScriptedEndpoint::new(vec![replica])),
            SyncMode::TwoWaySafe,
            state.path().to_path_buf(),
        )
        .unwrap();
        session.set_ignore_mounts(false);
        session
            .run_cycle()
            .expect("the mounted content syncs like any other");
        let error = session.run_cycle().expect_err("a vanished mount halts");
        assert!(
            matches!(
                error.downcast_ref::<SafetyHalt>(),
                Some(SafetyHalt::MountVanished("primary", path)) if path == "mnt"
            ),
            "{error:#}"
        );
    }
}
