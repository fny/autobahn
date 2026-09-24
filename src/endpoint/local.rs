//! The local (in-process) endpoint.
//!
//! This is the only module in autobahn that mutates a user's filesystem, and
//! every design decision here follows from that: the endpoint refuses and
//! reports rather than guesses, it never follows a symbolic link while
//! validating or removing content, and it publishes new content only by
//! renaming a fully written temporary into place.
//!
//! Three pieces of state make the whole thing work:
//!
//! - **The last scan.** [`scan`](Endpoint::scan) retains its snapshot, which
//!   serves both as the digest cache for the next scan (node-resident
//!   metadata — see [`crate::scan`]) and as the record that transitions
//!   validate the filesystem against. The controller's cycle guarantees the
//!   ordering that makes the latter sound: the transitions handed to
//!   [`transition`](Endpoint::transition) were reconciled from the snapshot
//!   returned by the immediately preceding scan, so "matches the last scan"
//!   is exactly "unchanged since reconciliation decided this was safe".
//! - **Content-addressed staging.** Received (and locally reused) file
//!   content lands at `staging_root/<digest-hex>` once it has been verified.
//!   Staging is therefore idempotent, resumable across interrupted cycles,
//!   and deduplicated between paths that share content, all for free.
//! - **Temporaries.** Every intermediate file this module creates is named
//!   with the [`TEMPORARY_PREFIX`] that scans skip, so an in-flight (or
//!   abandoned) transition is never mistaken for synchronizable content.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, Metadata, OpenOptions, Permissions};
use std::io::{self, Cursor, ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{symlink, DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};

use super::{Endpoint, FileRequest, StagingNeed, TransferFrame, TransitionOutcome};
use crate::rsync::{self, Signature};
// `TEMPORARY_PREFIX` names every temporary this module creates. It is the
// scanner's own constant, so temporaries are invisible to the
// synchronization hierarchy wherever they live, and the refusal in
// `validate_name` cannot drift from what scans hide.
use crate::scan::{
    self, recompose, validate_portable_target, FilesystemBehavior, IgnoreSet, SymlinkMode,
    TEMPORARY_PREFIX,
};
use crate::tree::{path_join, Change, Content, Digest, FileMetadata, Node, Problem, Snapshot};

/// The default permission bits applied to created directories. The default
/// is deliberately conservative (owner-only, matching Mutagen): synchronized
/// trees frequently hold credentials, and a too-tight mode is an
/// inconvenience while a too-loose one is an exposure.
const DEFAULT_DIRECTORY_MODE: u32 = 0o700;

/// The default permission bits applied to created non-executable files.
const DEFAULT_FILE_MODE: u32 = 0o600;

/// The size of the buffer used to stream local staging copies.
const COPY_BUFFER_SIZE: usize = 64 * 1024;

/// The payload size a supply batch aims for. Small files produce tiny
/// frames, and a purely frame-counted batch would carry under a megabyte —
/// hundreds of round trips for a large tree. Sizing batches by bytes keeps
/// each round trip carrying real payload while staying far inside the
/// protocol's frame cap.
pub const SUPPLY_TARGET_BYTES: usize = 8 * 1024 * 1024;

/// Estimates the wire weight of a transfer frame.
fn frame_weight(frame: &TransferFrame) -> usize {
    match frame {
        TransferFrame::Begin { .. } => 40,
        TransferFrame::Op(crate::rsync::Op::Data(data)) => data.len() + 16,
        // Supply error messages embed paths and OS error text of unbounded
        // length, so they must count toward the byte budget too.
        TransferFrame::EndOfFile {
            error: Some(message),
        } => message.len() + 24,
        _ => 24,
    }
}

/// The most a `ReadFile` request returns. It serves `diff`, which shows a
/// person two versions of one file, so the cap sits far below the
/// protocol's frame limit rather than at it.
const MAXIMUM_READ_FILE_SIZE: u64 = 256 * 1024 * 1024;

/// The counter that uniquifies temporary file names within a process.
static TEMPORARY_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The policy options governing a local endpoint's behavior.
#[derive(Debug, Default)]
pub struct EndpointOptions {
    /// The ignore set applied to scans.
    pub ignores: IgnoreSet,
    /// The treatment of symbolic links.
    pub symlink_mode: SymlinkMode,
    /// The permission bits for created non-executable files (`None` for the
    /// default).
    pub file_mode: Option<u32>,
    /// The permission bits for created directories (`None` for the default).
    pub directory_mode: Option<u32>,
    /// The per-file size limit: larger files are scanned as untracked
    /// content, excluded from synchronization (`None` for unlimited).
    pub max_file_size: Option<u64>,
    /// The per-root entry limit: a scan exceeding it fails (`None` for
    /// unlimited).
    pub max_entry_count: Option<u64>,
    /// Whether this endpoint will never wait for a change — a one-shot
    /// `sync` — so no filesystem watcher is registered for its root.
    /// Registering one walks the whole tree once more; on a large tree
    /// that was a third of a one-shot's time, for nothing.
    pub one_shot: bool,
    /// Whether directories on another device than the root — mount
    /// points — are left alone rather than walked. On unless configured
    /// otherwise; see `scan::scan`.
    pub ignore_mounts: bool,
    /// The owner (name or `id:N`) applied to created entries (`None` to
    /// leave ownership alone). Resolved on this endpoint's host.
    pub default_owner: Option<String>,
    /// The group (name or `id:N`) applied to created entries (`None` to
    /// leave ownership alone). Resolved on this endpoint's host.
    pub default_group: Option<String>,
}

/// A local filesystem endpoint.
pub struct LocalEndpoint {
    /// The synchronization root (which need not exist).
    root: PathBuf,
    /// The directory holding staged content and staging temporaries.
    staging_root: PathBuf,
    /// The treatment of symbolic links, applied when creating them.
    symlink_mode: SymlinkMode,
    /// The ignore set scans apply, kept so a directory's removal can tell
    /// content a pattern excludes from content excluded for what it is.
    ignores: IgnoreSet,
    /// The permission bits for created non-executable files.
    file_mode: u32,
    /// The permission bits for created directories.
    directory_mode: u32,
    /// The per-root entry limit (`None` for unlimited).
    max_entry_count: Option<u64>,
    /// The owner ID applied to created entries (`None` to leave alone).
    owner: Option<u32>,
    /// The group ID applied to created entries (`None` to leave alone).
    group: Option<u32>,
    /// The shared observation of this root: one watcher, one scan, one
    /// cache, however many sessions synchronize it. See
    /// [`observer`](crate::endpoint::observer) for why.
    observer: Arc<crate::endpoint::observer::RootObserver>,
    /// Where this endpoint's scans publish their running counts, when a
    /// supervisor is watching. Shared with one side of one session: an
    /// observer serving several sessions walks once, and the session that
    /// performs the walk is the one that reports it.
    progress: Option<Arc<crate::progress::SideProgress>>,
    /// A test seam between the transition's announcement and its writes —
    /// the window in which a sharing session's scan can consume the
    /// announced dirty marks and still read the old bytes.
    #[cfg(test)]
    pub(crate) between_announce_and_writes: Option<Box<dyn Fn() + Send>>,
    /// The generation this endpoint's last scan reflects, so it waits only
    /// for changes it has not already seen.
    seen_generation: u64,
    /// The most recent scan, used as the digest cache for the next scan, as
    /// the local-content index for staging, and as the record that
    /// transitions validate against.
    last_snapshot: Option<Snapshot>,
    /// The open supply stream, if any.
    supply: Option<SupplyState>,
    /// The receive state established by the last [`stage_begin`], if any.
    ///
    /// [`stage_begin`]: Endpoint::stage_begin
    receive: Option<ReceiveState>,
    /// The digests requested since the last sweep, as staging names. A
    /// staged blob no request in the cycle referenced is dead: everything
    /// asked for was published, or discarded with its refusal reported.
    /// What is left is the previous version of a file that changed while
    /// its transfer was in flight — re-requested under a new digest, and
    /// never referenced again. Nothing else ever removed those, and a busy
    /// tree with large files accumulated gigabytes of them.
    requested: HashSet<String>,
}

/// The number of changed paths a watcher will accumulate before giving up
/// on tracking them individually. Past this point a full scan is cheaper
/// than an incremental one anyway, so the paths are discarded and the next
/// scan reads everything.
const MAXIMUM_PENDING_PATHS: usize = 8192;

/// The longest an endpoint will go on incremental scans alone. Watching is
/// best-effort — events can be missed when a directory is created and
/// populated faster than a recursive watch can follow it, and network
/// The changed paths accumulated by a watcher since the last scan.
#[derive(Default)]
struct PendingChanges {
    /// The absolute paths reported as changed.
    paths: Vec<PathBuf>,
    /// Whether the record is incomplete — too many paths, an event the
    /// backend flagged for rescan, or a watcher error. The next scan must
    /// then read everything.
    incomplete: bool,
}

impl PendingChanges {
    /// Records that the change record can no longer be trusted, releasing
    /// the paths accumulated so far (a full scan supersedes them).
    fn give_up(&mut self) {
        self.incomplete = true;
        self.paths.clear();
        self.paths.shrink_to_fit();
    }
}

/// Watches `start` and every directory beneath it that the scanner would
/// visit, one non-recursive watch each, skipping ignored directories and
/// never following symbolic links.
///
/// A directory that vanished or cannot be read is skipped, exactly as the
/// backend's own recursive walk skips it. Anything else — the kernel's
/// watch limit above all — fails the whole watch, so the observer falls
/// back to polling and says so, rather than watching part of the tree in
/// silence.
#[cfg(target_os = "linux")]
fn watch_tree(
    watcher: &Mutex<notify::RecommendedWatcher>,
    root: &Path,
    start: &Path,
    ignores: &crate::scan::IgnoreSet,
    device: Option<u64>,
) -> Result<()> {
    use notify::Watcher;
    let relative = |path: &Path| -> Option<String> {
        let mut parts = Vec::new();
        for component in path.strip_prefix(root).ok()?.components() {
            let std::path::Component::Normal(name) = component else {
                return None;
            };
            parts.push(name.to_str()?.to_owned());
        }
        Some(parts.join("/"))
    };
    let mut stack = vec![start.to_path_buf()];
    while let Some(dir) = stack.pop() {
        // Ignores are consulted on the way in, as the scanner does. A name
        // that cannot be expressed cannot be matched, and is watched — the
        // safe direction.
        if dir != root {
            if let Some(relative) = relative(&dir) {
                if ignores.ignored(&relative, true) {
                    continue;
                }
            }
        }
        if let Err(error) = watcher
            .lock()
            .expect("the watcher lock is never poisoned")
            .watch(&dir, notify::RecursiveMode::NonRecursive)
        {
            let skippable = match &error.kind {
                notify::ErrorKind::PathNotFound => true,
                notify::ErrorKind::Io(io) => matches!(
                    io.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ),
                _ => false,
            };
            if skippable {
                continue;
            }
            return Err(anyhow::Error::new(error))
                .with_context(|| format!("unable to watch {}", dir.display()));
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // `symlink_metadata`, so a link to a directory is not a
            // directory here: the watch never follows links.
            if fs::symlink_metadata(&path).is_ok_and(|metadata| {
                metadata.is_dir() && device.is_none_or(|device| metadata.dev() == device)
            }) {
                stack.push(path);
            }
        }
    }
    Ok(())
}

/// A filesystem watcher over the synchronization root, recording changed
/// paths for incremental scanning and signaling waiters.
///
/// On Linux the watch is built here rather than by the backend, and it
/// skips ignored directories. The backend's recursive mode walks every
/// directory under the root and takes one kernel watch for each, ignored
/// or not: on a root of 325,509 directories of which 39,252 are scanned,
/// that walk took ~20 seconds and then failed on the kernel's watch limit
/// deep inside a `.venv` — and the failure retried every 30 seconds,
/// inside the scan request, for days, reported to a discarded stderr.
/// Watching only what the scanner would visit keeps the count at what the
/// tree actually needs, and keeps writes into build output out of the
/// change record entirely.
pub(crate) struct ChangeWatcher {
    /// The watcher itself, retained for its lifetime side effect. On Linux
    /// it is shared with the dispatch thread that extends it to
    /// directories appearing after the watch was built.
    #[cfg(target_os = "linux")]
    _watcher: Arc<Mutex<notify::RecommendedWatcher>>,
    #[cfg(not(target_os = "linux"))]
    _watcher: notify::RecommendedWatcher,
    /// The changed paths recorded since the last scan consumed them.
    pending: Arc<Mutex<PendingChanges>>,
}

impl PendingChanges {
    /// Records one backend event: its paths, or the fact that the record
    /// can no longer be trusted.
    fn record(&mut self, event: notify::Result<notify::Event>) {
        match event {
            // A backend that lost events (a kernel queue overflow) flags the
            // fact rather than reporting the paths.
            Ok(event) if event.need_rescan() => self.give_up(),
            Ok(event) => {
                if self.paths.len() + event.paths.len() > MAXIMUM_PENDING_PATHS {
                    self.give_up();
                } else if !self.incomplete {
                    self.paths.extend(event.paths);
                }
            }
            Err(_) => self.give_up(),
        }
    }
}

impl ChangeWatcher {
    /// Establishes a watch over `root`, calling `notify` whenever an event
    /// lands.
    ///
    /// The callback carries no payload: the paths accumulate in `pending`,
    /// and what a waiter needs to know is only that *something* happened.
    /// It runs on the watcher's own thread, so it must not block — the
    /// observer's signal takes a lock it holds for a counter increment and
    /// nothing more.
    #[cfg(not(target_os = "linux"))]
    pub(crate) fn new(
        root: &Path,
        _ignores: crate::scan::IgnoreSet,
        _ignore_mounts: bool,
        notify: impl Fn() + Send + 'static,
    ) -> Result<ChangeWatcher> {
        use notify::Watcher;
        let pending = Arc::new(Mutex::new(PendingChanges::default()));
        let recorder = Arc::clone(&pending);
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                recorder
                    .lock()
                    .expect("the pending lock is never poisoned")
                    .record(event);
                notify();
            })
            .context("unable to create a filesystem watcher")?;
        // FSEvents watches a tree natively, with one registration; there
        // is nothing to prune.
        watcher
            .watch(root, notify::RecursiveMode::Recursive)
            .with_context(|| format!("unable to watch {}", root.display()))?;
        Ok(ChangeWatcher {
            _watcher: watcher,
            pending,
        })
    }

    /// Establishes a watch over `root`, calling `notify` whenever an event
    /// lands. See the type's documentation for why the walk is done here.
    #[cfg(target_os = "linux")]
    pub(crate) fn new(
        root: &Path,
        ignores: crate::scan::IgnoreSet,
        ignore_mounts: bool,
        notify: impl Fn() + Send + 'static,
    ) -> Result<ChangeWatcher> {
        // Mounts inside the root are not scanned, so they are not watched:
        // a mounted network share would otherwise take a kernel watch per
        // directory for content nothing reads.
        let device = match ignore_mounts {
            true => Some(
                fs::metadata(root)
                    .with_context(|| format!("unable to probe {}", root.display()))?
                    .dev(),
            ),
            false => None,
        };
        let pending = Arc::new(Mutex::new(PendingChanges::default()));
        // Events cross a channel to a thread that owns the watcher.
        // Extending the watch to a directory that just appeared needs the
        // watcher, and the backend's callback cannot reach it.
        let (sender, events) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let _ = sender.send(event);
        })
        .context("unable to create a filesystem watcher")?;
        let watcher = Arc::new(Mutex::new(watcher));
        watch_tree(&watcher, root, root, &ignores, device)?;

        let recorder = Arc::clone(&pending);
        let extender = Arc::downgrade(&watcher);
        let root = root.to_path_buf();
        std::thread::Builder::new()
            .name("autobahn-watch".into())
            .spawn(move || {
                while let Ok(event) = events.recv() {
                    // Ends with the watcher: dropping the `ChangeWatcher`
                    // drops the last strong reference, the backend and its
                    // sender with it, and the receive above then fails.
                    let Some(watcher) = extender.upgrade() else {
                        break;
                    };
                    // A directory that appeared, or arrived by rename, is
                    // watched before its event is recorded, so the scan the
                    // event provokes runs with the watch already in place.
                    // A tree created faster than events arrive is walked on
                    // the way in, which is what makes an untar safe.
                    if let Ok(event) = &event {
                        if matches!(
                            event.kind,
                            notify::EventKind::Create(_)
                                | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
                        ) {
                            for path in &event.paths {
                                let is_directory = std::fs::symlink_metadata(path)
                                    .map(|metadata| metadata.is_dir())
                                    .unwrap_or(false);
                                if is_directory {
                                    let _ = watch_tree(&watcher, &root, path, &ignores, device);
                                }
                            }
                        }
                    }
                    recorder
                        .lock()
                        .expect("the pending lock is never poisoned")
                        .record(event);
                    notify();
                }
            })
            .context("unable to start the watch dispatch thread")?;
        Ok(ChangeWatcher {
            _watcher: watcher,
            pending,
        })
    }

    /// The raw paths recorded so far, for tests of what the watch sees.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn recorded(&self) -> Vec<PathBuf> {
        self.pending
            .lock()
            .expect("the pending lock is never poisoned")
            .paths
            .clone()
    }

    /// Records paths this process is about to change, exactly as the
    /// backend's own callback would. The operating system's events for
    /// these writes arrive on their own schedule; a scan racing that
    /// delivery must still find the paths dirty, or it adopts its baseline
    /// for a file that no longer matches the disk — and publishes the
    /// result as current.
    pub(crate) fn mark_pending(&self, paths: impl IntoIterator<Item = PathBuf>) {
        let mut pending = self
            .pending
            .lock()
            .expect("the pending lock is never poisoned");
        for path in paths {
            if pending.paths.len() >= MAXIMUM_PENDING_PATHS {
                pending.give_up();
                return;
            }
            if !pending.incomplete {
                pending.paths.push(path);
            }
        }
    }

    /// Takes the changes recorded since the last call, as paths to re-read.
    ///
    /// Returns `None` when the record cannot be trusted — a kernel queue
    /// overflow, too many paths to hold, or a path that cannot be expressed
    /// in the hierarchy's naming — which asks the caller for a full walk.
    /// Consuming here rather than after the walk means a change arriving
    /// during a scan stays pending for the next one: at worst repeated
    /// work, never a dropped notification.
    pub(crate) fn take_dirty(
        &mut self,
        root: &Path,
        behavior: &FilesystemBehavior,
    ) -> Option<scan::DirtyPaths> {
        let changes = self.take();
        if changes.incomplete {
            return None;
        }
        let mut dirty = scan::DirtyPaths::default();
        for path in &changes.paths {
            // A path outside the root, or one that cannot be expressed in
            // the hierarchy's naming, cannot be marked — and silently
            // ignoring it would be a missed change.
            let relative = path.strip_prefix(root).ok()?;
            let mut components = Vec::new();
            for component in relative.components() {
                let std::path::Component::Normal(component) = component else {
                    return None;
                };
                let component = component.to_str()?;
                // The hierarchy carries NFC names; a decomposing volume
                // reports NFD ones.
                components.push(if behavior.decomposes_unicode {
                    scan::recompose(component)
                } else {
                    component.to_owned()
                });
            }
            dirty.mark(&components.join("/"));
        }
        Some(dirty)
    }

    /// Takes the changes recorded since the last call.
    fn take(&self) -> PendingChanges {
        std::mem::take(
            &mut *self
                .pending
                .lock()
                .expect("the pending lock is never poisoned"),
        )
    }

    /// Indicates whether any change is currently recorded.
    /// The current size of the unconsumed change record.
    pub(crate) fn activity(&self) -> crate::endpoint::ChangeActivity {
        let pending = self
            .pending
            .lock()
            .expect("the pending lock is never poisoned");
        crate::endpoint::ChangeActivity {
            paths: pending.paths.len(),
            incomplete: pending.incomplete,
        }
    }
}

impl LocalEndpoint {
    /// Creates a local endpoint for the specified synchronization root,
    /// with staging state isolated under `staging_root` (which will be
    /// created if needed).
    ///
    /// The synchronization root itself is neither created nor required to
    /// exist: a missing root is a legitimate synchronization state (and one
    /// that a transition may resolve by creating it).
    pub fn new(
        root: PathBuf,
        staging_root: PathBuf,
        options: EndpointOptions,
    ) -> Result<LocalEndpoint> {
        // The staging directory is created on first use (stage_begin), not
        // here: an inside-root placement would otherwise conjure a missing
        // synchronization root into existence as an empty directory —
        // reading as an emptied root to safety checks, or as an authoritative
        // empty source to mirroring modes.
        // Ownership specifications resolve here, against this endpoint's
        // own user and group databases, so a bad name is a construction
        // error rather than a per-entry surprise at transition time.
        let owner = options
            .default_owner
            .as_deref()
            .map(crate::ownership::resolve_user)
            .transpose()?;
        let group = options
            .default_group
            .as_deref()
            .map(crate::ownership::resolve_group)
            .transpose()?;
        // The scan cache belongs to the observation, not to a session:
        // one file per observed root rather than one per destination.
        let cache_path = staging_root.with_extension("scancache");
        let observer_root = root.clone();
        let observer_ignores = options.ignores.clone();
        // A network filesystem answers stats from a client cache, delivers
        // no (or partial) change events, and gives advisory locks whatever
        // semantics the server chooses — which quietly voids the
        // assumptions scanning, destructive validation, and locking are
        // built on. Synchronizing one is best-effort, single-writer
        // territory, and the person configuring it should know that.
        warn_if_network_filesystem(&root);

        Ok(LocalEndpoint {
            root,
            staging_root,
            symlink_mode: options.symlink_mode,
            ignores: options.ignores.clone(),
            file_mode: options.file_mode.unwrap_or(DEFAULT_FILE_MODE) & 0o777,
            directory_mode: options.directory_mode.unwrap_or(DEFAULT_DIRECTORY_MODE) & 0o777,
            max_entry_count: options.max_entry_count,
            owner,
            group,
            observer: {
                let observer = crate::endpoint::observer::observer_for(
                    crate::endpoint::observer::ObserverKey {
                        root: crate::endpoint::observer::canonical_root(&observer_root),
                        ignores: observer_ignores.key(),
                        symlink_mode: options.symlink_mode,
                        max_file_size: options.max_file_size,
                        ignore_mounts: options.ignore_mounts,
                    },
                    observer_ignores,
                    cache_path,
                );
                if !options.one_shot {
                    observer.want_watching();
                }
                observer
            },
            progress: None,
            #[cfg(test)]
            between_announce_and_writes: None,
            seen_generation: 0,
            requested: HashSet::new(),
            last_snapshot: None,
            supply: None,
            receive: None,
        })
    }

    /// Returns the endpoint's most recent snapshot — the last scan, with
    /// any subsequent transition's achieved results folded in. This is the
    /// tree an unchanged rescan will adopt.
    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.last_snapshot.as_ref()
    }

    /// Blocks until pending state writes have completed. The cycle path
    /// never calls this — the whole point of the background writer is that
    /// it does not — but a caller that needs to observe the state on disk
    /// (a test, an orderly shutdown) can wait for it.
    pub fn flush_state(&self) {
        self.observer.flush_state();
    }

    /// Where this endpoint's observation persists its scan cache.
    pub fn scan_cache_path(&self) -> PathBuf {
        self.observer.cache_path().to_path_buf()
    }

    /// Overrides the probed filesystem behavior for tests.
    #[cfg(test)]
    fn force_behavior(&self, behavior: FilesystemBehavior) {
        self.observer.force_behavior(behavior);
    }

    /// Returns the path at which content with the specified digest lives once
    /// it has been fully received and verified.
    fn staged_path(&self, digest: &Digest) -> PathBuf {
        staged_path(&self.staging_root, digest)
    }

    /// Removes staged blobs that no request since the last sweep referenced,
    /// then forgets the requests.
    ///
    /// Only names that are a digest are candidates. Anything else in the
    /// directory is a temporary mid-write, and belongs to whoever is
    /// writing it. Removal failures are ignored: this is a cache, and a
    /// blob that will not go today goes on a later cycle.
    ///
    /// Called only at the end of a transition, never between a stage and
    /// its transition, so a blob staged this cycle is never swept before it
    /// is published — and content surviving an interrupted run is kept,
    /// because a crash means no transition ran to sweep it.
    fn sweep_staging(&mut self) {
        let requested = std::mem::take(&mut self.requested);
        let Ok(entries) = fs::read_dir(&self.staging_root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let is_digest = name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit());
            if !is_digest || requested.contains(name) {
                continue;
            }
            if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    /// Decides whether the local-content index is worth building for a
    /// batch of requests.
    ///
    /// Only requests for paths that hold no file today can plausibly be
    /// satisfied from elsewhere in the root: a *modification* asks for
    /// content that is new by definition, so indexing to look for it is
    /// wasted work. Creations — the second half of a copy or a rename, and
    /// every entry of a cold synchronization — are where the index pays.
    ///
    /// The index costs one visit and one path allocation per file in the
    /// root, while each request it satisfies saves a transfer. Requiring
    /// the potential savings to outweigh that walk keeps a single renamed
    /// file from indexing half a million entries, while a bulk copy (or a
    /// cold start) still indexes as before.
    fn local_reuse_is_worthwhile(&self, files: &[FileRequest]) -> bool {
        /// The number of index visits that one saved transfer is worth
        /// paying for. A transfer costs orders of magnitude more than
        /// visiting a node, so this is deliberately conservative.
        const VISITS_PER_SAVED_TRANSFER: u64 = 1_000;
        let candidates = files
            .iter()
            .filter(|request| !self.snapshot_records_file(&request.path))
            .count() as u64;
        if candidates == 0 {
            return false;
        }
        let indexed_files = self
            .last_snapshot
            .as_ref()
            .map(|snapshot| snapshot.files)
            .unwrap_or(0);
        candidates.saturating_mul(VISITS_PER_SAVED_TRANSFER) >= indexed_files
    }

    /// Builds an index from content digest to root-relative path over the
    /// last scan's file nodes, enabling requests to be satisfied by content
    /// that already exists somewhere in the root.
    fn digest_index(&self) -> HashMap<Digest, String> {
        fn collect(node: &Node, path: &str, index: &mut HashMap<Digest, String>) {
            match &node.content {
                Content::File { digest, .. } => {
                    // The first path recorded for a digest wins; any of them
                    // would do, and this keeps the walk allocation-free for
                    // duplicated content.
                    index.entry(*digest).or_insert_with(|| path.to_owned());
                }
                Content::Directory(children) => {
                    for child in children.iter() {
                        collect(child, &path_join(path, &child.name), index);
                    }
                }
                _ => {}
            }
        }
        let mut index = HashMap::new();
        if let Some(root) = self.last_snapshot.as_ref().and_then(|s| s.root.as_ref()) {
            collect(root, "", &mut index);
        }
        index
    }

    /// Determines the changed paths for an incremental scan, or `None` when
    /// this scan must read the whole hierarchy.
    ///
    /// A full scan is required when there is no baseline to adopt from, when
    /// no watcher is established (or one was just established, whose record
    /// begins after changes that may already have happened), when the
    /// watcher's record is incomplete, and periodically regardless — see
    /// [`FULL_SCAN_INTERVAL`].
    /// The digest and scan metadata the last scan recorded for a regular
    /// file at a root-relative path, or `None` when it recorded anything
    /// else, or nothing.
    ///
    /// The scanner records a file only after `lstat` shows a regular file,
    /// and never descends through a symbolic link or into an ignored
    /// directory, so a path this answers for lies inside the root, holds no
    /// `..`, crosses no symlinked parent and is not ignored. That makes it
    /// the gate for anything a peer names: supply, and base signatures.
    fn snapshot_file(&self, path: &str) -> Option<(&Digest, &FileMetadata)> {
        let mut node = self.last_snapshot.as_ref()?.root.as_ref()?;
        if path.is_empty() {
            return None;
        }
        for component in path.split('/') {
            node = node.child(component)?;
        }
        match &node.content {
            Content::File {
                digest, metadata, ..
            } => Some((digest, metadata)),
            _ => None,
        }
    }

    /// Reports whether the last scan recorded a regular file at a
    /// root-relative path — the gate for base-signature computation, saving
    /// a filesystem probe for every path known to hold nothing usable.
    fn snapshot_records_file(&self, path: &str) -> bool {
        self.snapshot_file(path).is_some()
    }

    /// Attempts to satisfy a content request from a file that already exists
    /// in the root, streaming it into staging while digesting it. Returns
    /// whether the content was staged: a digest mismatch (the file changed
    /// since the scan that indexed it) is an ordinary negative result, not an
    /// error, and leaves the request to be transferred normally.
    fn stage_locally(&self, source: &str, digest: &Digest) -> Result<bool> {
        // The source came from this side's own scan, but it passes the same
        // gate as a peer's request: a regular file still holding the scanned
        // inode and size, never a link swapped in since.
        let mut input = self
            .open_scanned(source, digest)
            .map_err(|error| anyhow::anyhow!(error))?;
        let (temporary, output) = self.staging_temporary("copy")?;
        let mut output = DigestingWriter::new(output);
        let mut buffer = vec![0u8; COPY_BUFFER_SIZE];
        let copied = loop {
            let count = match input.read(&mut buffer) {
                Ok(0) => break output.flush().map(|()| output.digest() == *digest),
                Ok(count) => count,
                Err(error) => break Err(error),
            };
            if let Err(error) = output.write_all(&buffer[..count]) {
                break Err(error);
            }
        };
        drop(output);
        let copied = match copied {
            Ok(copied) => copied,
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                return Err(error).with_context(|| format!("unable to copy {source} into staging"));
            }
        };
        if !copied {
            let _ = fs::remove_file(&temporary);
            return Ok(false);
        }
        let staged = self.staged_path(digest);
        if let Err(error) = fs::rename(&temporary, &staged) {
            let _ = fs::remove_file(&temporary);
            return Err(error).context("unable to publish staged content");
        }
        Ok(true)
    }

    /// Produces the supply stream's next frame, or `None` once every need
    /// has been supplied.
    ///
    /// A file streams: each call reads or receives only its next
    /// operation, so the supplier holds about one batch however large the
    /// file (see [`SupplySource`]). A need begins by choosing where its
    /// content comes from, before its begin frame goes out; once it has,
    /// a failure ends the file with an error rather than trying elsewhere.
    fn next_supply_frame(&self, state: &mut SupplyState) -> Option<TransferFrame> {
        if let Some(source) = state.current.as_mut() {
            let next = source.next_op();
            return Some(match next {
                Ok(Some(op)) => TransferFrame::Op(op),
                // A failed supply still terminates the file's stream, so
                // that the receiver discards its partial content and moves
                // on rather than desynchronizing from the need list.
                ended => {
                    state.current = None;
                    TransferFrame::EndOfFile { error: ended.err() }
                }
            });
        }
        let need = state.needs.get_mut(state.next)?;
        state.next += 1;
        let digest = need.request.digest;
        state.current = Some(self.begin_supply(need).unwrap_or_else(SupplySource::Failed));
        Some(TransferFrame::Begin { digest })
    }

    /// Chooses and opens the source of one need's content.
    ///
    /// The requested path supplies first; if it can't (vanished, become
    /// unreadable, changed, or refused by [`open_scanned`](Self::open_scanned)),
    /// any other scanned path recording the same digest holds identical
    /// content and is tried in its place — so one bad path never starves
    /// the paths that share its content. This is the only point at which
    /// another path can be tried: nothing of the file has been sent yet.
    /// (The receiver verifies the digest regardless, so a stale candidate
    /// merely fails staging as it would have anyway.)
    ///
    /// The need's signature moves into the source, which is its last user.
    fn begin_supply(&self, need: &mut StagingNeed) -> Result<SupplySource, String> {
        let digest = need.request.digest;
        let (path, file) = match self.open_scanned(&need.request.path, &digest) {
            Ok(file) => (need.request.path.clone(), file),
            Err(primary_error) => self
                .digest_paths(&digest, &need.request.path)
                .into_iter()
                .find_map(|candidate| {
                    let file = self.open_scanned(&candidate, &digest).ok()?;
                    Some((candidate, file))
                })
                .ok_or(primary_error)?,
        };
        let signature = std::mem::take(&mut need.signature);
        if signature.is_empty() {
            return Ok(SupplySource::Whole { path, file });
        }
        // A delta is computed on a helper thread, which blocks once the
        // channel holds a few batches' worth and ends when it is dropped.
        let (sender, operations) = mpsc::sync_channel(SUPPLY_CHANNEL_DEPTH);
        let failed_path = path.clone();
        std::thread::Builder::new()
            .name("autobahn-supply".into())
            .spawn(move || {
                let result = rsync::deltify(file, &signature, &mut |op| {
                    sender
                        .send(DeltaMessage::Op(op))
                        .map_err(|_| anyhow::anyhow!("the supply stream was closed"))
                });
                let last = match result {
                    Ok(()) => DeltaMessage::Done,
                    Err(error) => DeltaMessage::Failed(format!(
                        "unable to compute a delta for {failed_path}: {error:#}"
                    )),
                };
                let _ = sender.send(last);
            })
            .map_err(|error| format!("unable to start a delta for {path}: {error}"))?;
        Ok(SupplySource::Delta { path, operations })
    }

    /// Opens a file a peer asked for, if and only if the last scan recorded
    /// a regular file at `path` with `digest`, and it is still that file.
    ///
    /// A peer names the path, so the last scan must record a regular file
    /// there with exactly the requested digest (see
    /// [`snapshot_file`](Self::snapshot_file)): nothing outside the root,
    /// through a symbolic link, ignored, or other than what was asked for
    /// can leave. The file is then opened without following a final
    /// symbolic link and without blocking on a FIFO, and must still be the
    /// regular file of the scanned inode and size. The reader stops at the
    /// scanned size, and fails if the file ends before it (see
    /// [`ScannedFile`]).
    fn open_scanned(&self, path: &str, digest: &Digest) -> Result<ScannedFile, String> {
        validate_path(path).map_err(|error| format!("refused {path:?}: {error}"))?;
        let (recorded, scanned) = self
            .snapshot_file(path)
            .ok_or_else(|| format!("refused {path:?}: not a file the last scan recorded"))?;
        if recorded != digest {
            return Err(format!(
                "refused {path:?}: its scanned content is not the requested content"
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(self.root.join(path))
            .map_err(|error| format!("unable to open {path}: {error}"))?;
        let now = file
            .metadata()
            .map_err(|error| format!("unable to inspect {path}: {error}"))?;
        if !now.file_type().is_file()
            || (scanned.inode != 0 && now.ino() != scanned.inode)
            || now.len() != scanned.size
        {
            return Err(format!("{path} changed since the scan"));
        }
        Ok(ScannedFile {
            file,
            remaining: scanned.size,
        })
    }

    /// Collects every root-relative path (other than the excluded one) whose
    /// scanned content records the given digest. Only consulted when a
    /// supply attempt fails, so the walk stays off the hot path.
    fn digest_paths(&self, digest: &Digest, exclude: &str) -> Vec<String> {
        fn collect(
            node: &Node,
            path: &str,
            digest: &Digest,
            exclude: &str,
            paths: &mut Vec<String>,
        ) {
            match &node.content {
                Content::File {
                    digest: recorded, ..
                } if recorded == digest && path != exclude => paths.push(path.to_owned()),
                Content::Directory(children) => {
                    for child in children.iter() {
                        collect(child, &path_join(path, &child.name), digest, exclude, paths);
                    }
                }
                _ => {}
            }
        }
        let mut paths = Vec::new();
        if let Some(root) = self.last_snapshot.as_ref().and_then(|s| s.root.as_ref()) {
            collect(root, "", digest, exclude, &mut paths);
        }
        paths
    }

    /// Applies a batch of transfer frames to the receive state.
    fn push_frames(&self, state: &mut ReceiveState, frames: Vec<TransferFrame>) -> Result<()> {
        for frame in frames {
            match frame {
                TransferFrame::Begin { digest } => {
                    // A file whose stream ended without its end-of-file is
                    // not kept; the begin frame of the next one says so.
                    if let Some(Receiving::File { file, .. }) = state.current.take() {
                        file.discard();
                    }
                    state.current = Some(match state.needs.get(&digest) {
                        Some(need) => Receiving::File {
                            file: Box::new(self.open_receive_file(need)?),
                            need: need.clone(),
                        },
                        None => Receiving::Sink,
                    });
                }
                TransferFrame::Op(op) => match state.current.as_mut() {
                    Some(Receiving::File { need, file }) => {
                        // A patch that fails — most often because the base
                        // changed or went short since its signature was
                        // taken — fails this file alone, as a failed supply
                        // does: its partial content is discarded, the rest
                        // of its frames are read and dropped, and it stays
                        // needed, so the transition reports it missing and
                        // the next cycle transfers it again. Only a framing
                        // error ends the stream.
                        if rsync::patch(&mut file.base, &need.signature, &op, &mut file.writer)
                            .is_err()
                        {
                            if let Some(Receiving::File { file, .. }) =
                                state.current.replace(Receiving::Sink)
                            {
                                file.discard();
                            }
                        }
                    }
                    Some(Receiving::Sink) => {}
                    None => bail!("received a transfer frame outside a file"),
                },
                TransferFrame::EndOfFile { error } => match state.current.take() {
                    Some(Receiving::File { need, file }) => {
                        if error.is_some() {
                            file.discard();
                        } else {
                            self.finish_receive(*file, &need)?;
                            state.needs.remove(&need.request.digest);
                        }
                    }
                    Some(Receiving::Sink) => {}
                    None => bail!("received an end of file outside a file"),
                },
            }
        }
        Ok(())
    }

    /// Creates a new staging temporary for `purpose`, readable only by
    /// this user, returning its path and the file open for writing.
    ///
    /// It opens through [`private_file`](crate::fsutil::private_file), so
    /// an existing name — a symbolic link planted at a predicted one
    /// included — is refused rather than followed or reused, and the next
    /// name is tried. The error names no path: it can cross the wire to
    /// the peer, which has no business learning where staging lives.
    fn staging_temporary(&self, purpose: &str) -> Result<(PathBuf, File)> {
        /// How many taken names are passed over before giving up.
        const ATTEMPTS: usize = 8;
        let mut attempts = 0;
        loop {
            let temporary = self.staging_root.join(temporary_name(purpose));
            match crate::fsutil::private_file(&temporary) {
                Ok(file) => return Ok((temporary, file)),
                Err(error) => {
                    let cause = error.root_cause();
                    let taken = cause
                        .downcast_ref::<io::Error>()
                        .is_some_and(|cause| cause.kind() == ErrorKind::AlreadyExists);
                    attempts += 1;
                    if !taken || attempts == ATTEMPTS {
                        bail!("unable to create a staging file: {cause}");
                    }
                }
            }
        }
    }

    fn open_receive_file(&self, need: &StagingNeed) -> Result<ReceiveFile> {
        let (temporary, output) = self.staging_temporary("recv")?;
        // An empty signature means the delta can only carry literal data —
        // no block operation can reference a base — so the target needn't
        // be probed or opened at all (the common case on a cold
        // destination).
        let base = if need.signature.is_empty() {
            PatchBase::empty()
        } else {
            match open_base(&self.root, &need.request.path) {
                Some(file) => PatchBase::File(file),
                None => PatchBase::empty(),
            }
        };
        Ok(ReceiveFile {
            temporary,
            writer: DigestingWriter::new(output),
            base,
        })
    }

    /// Completes a received file: flushes it, compares the digest accumulated
    /// during patching against the requested one, and publishes the content
    /// into staging on a match. A mismatch means the file changed on the
    /// source mid-transfer, which is an ordinary occurrence — the partial
    /// content is discarded and the next cycle transfers the new content.
    fn finish_receive(&self, file: ReceiveFile, need: &StagingNeed) -> Result<()> {
        let ReceiveFile {
            temporary,
            mut writer,
            ..
        } = file;
        if let Err(error) = writer.flush() {
            let _ = fs::remove_file(&temporary);
            return Err(error).context("unable to flush a staging file");
        }
        let digest = writer.digest();
        drop(writer);
        if digest != need.request.digest {
            let _ = fs::remove_file(&temporary);
            return Ok(());
        }
        let staged = self.staged_path(&digest);
        if let Err(error) = fs::rename(&temporary, &staged) {
            let _ = fs::remove_file(&temporary);
            return Err(error).context("unable to publish staged content");
        }
        Ok(())
    }
}

/// Whether this filesystem treats different names as the same entry.
fn folds_names(behavior: &crate::scan::probes::FilesystemBehavior) -> bool {
    behavior.case_insensitive || behavior.normalization_insensitive || behavior.decomposes_unicode
}

/// The key under which this filesystem files a name.
///
/// Two names with the same key denote one directory entry here, however
/// different their bytes.
fn folded_name(name: &str, behavior: &crate::scan::probes::FilesystemBehavior) -> String {
    // Case-insensitive lookups use Unicode case *folding* (`Σ`, `σ`, and
    // `ς` all collide), not mere lowercasing; folding can emit decomposed
    // sequences, so recomposition follows it.
    let mut key = if behavior.case_insensitive {
        caseless::default_case_fold_str(name)
    } else {
        name.to_owned()
    };
    if folds_names(behavior) {
        key = recompose(&key);
    }
    key
}

/// The entry already in `parent` that this filesystem cannot tell apart
/// from `name`, when its bytes differ.
///
/// A creation refused because "something is already there" is usually
/// exactly this: the destination holds a name spelled differently — a
/// combining accent where the other side has a precomposed one, or a
/// different case — and the local filesystem files both under one entry.
/// Saying so is the difference between a message someone can act on and
/// one that sends them looking for a file that appears not to exist.
///
/// The decision rests on what the directory holds, not on the probed
/// behaviour. The caller has already had a path resolve to something; if
/// no entry is spelled exactly that way, this filesystem folded it into
/// one that is, whatever the flags in hand happen to say. Those flags can
/// be a default that claims names never fold — the observer probes when
/// the root exists, and a session that started before it did carries the
/// default forward.
fn folded_twin(parent: &Path, name: &str) -> Option<(String, &'static str)> {
    let names: Vec<String> = fs::read_dir(parent)
        .ok()?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    // Spelled exactly this way, so nothing was folded.
    if names.iter().any(|other| other == name) {
        return None;
    }
    let recomposed = recompose(name);
    let folded = recompose(&caseless::default_case_fold_str(name));
    names.into_iter().find_map(|other| {
        // Which rule folded them. Recomposition alone settling it means
        // the two spell one name; otherwise it took case folding.
        if recompose(&other) == recomposed {
            Some((other, "unicode collision"))
        } else if recompose(&caseless::default_case_fold_str(&other)) == folded {
            Some((other, "casing collision"))
        } else {
            None
        }
    })
}

impl Endpoint for LocalEndpoint {
    fn set_scan_progress(&mut self, progress: Arc<crate::progress::SideProgress>) {
        self.progress = Some(progress);
    }

    fn scan(&mut self) -> Result<Snapshot> {
        // The observation is shared: one watcher, one walk and one cache
        // per root, however many sessions synchronize it. What this
        // endpoint keeps is the *lease* — the exact snapshot this scan
        // returned — because transitions validate against the scan they
        // were reconciled from, not against whatever the observer has
        // published since.
        let (snapshot, generation) = self
            .observer
            .scan(self.max_entry_count, self.progress.as_deref())?;
        self.seen_generation = generation;
        self.last_snapshot = Some(snapshot.clone());
        Ok(snapshot)
    }

    fn scan_verified(&mut self) -> Result<Snapshot> {
        let (snapshot, generation) = self
            .observer
            .scan_rehash(self.max_entry_count, self.progress.as_deref())?;
        self.seen_generation = generation;
        self.last_snapshot = Some(snapshot.clone());
        Ok(snapshot)
    }

    fn stage_begin(&mut self, files: Vec<FileRequest>) -> Result<Vec<StagingNeed>> {
        // Any receive state left over from a previous staging operation
        // belongs to a stream that will never be continued, whether or not
        // this one begins.
        if let Some(state) = self.receive.take() {
            state.discard();
        }

        // A request's path names where its content will be published, and
        // is used below to find a base to sign and, later, to patch
        // against. A genuine controller builds requests from scans, which
        // never produce these; one that sends them is broken or hostile,
        // and nothing in its batch is worth salvaging.
        for request in &files {
            validate_path(&request.path).map_err(|error| {
                anyhow::anyhow!("refusing a staging request for {:?}: {error}", request.path)
            })?;
            if request.path.is_empty() {
                bail!("refusing a staging request for the synchronization root itself");
            }
            if request
                .path
                .split('/')
                .any(|component| component.starts_with(TEMPORARY_PREFIX))
            {
                bail!(
                    "refusing a staging request for {:?}: it uses a name reserved for autobahn",
                    request.path
                );
            }
        }

        prepare_staging_root(&self.staging_root, &self.root)?;

        // One directory read inventories what previous cycles left staged,
        // replacing a per-request stat (on a cold destination, 40k stats
        // against an empty directory).
        let inventory: std::collections::HashSet<String> = fs::read_dir(&self.staging_root)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .filter_map(|entry| entry.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default();
        let mut staged: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Building the local-content index walks every file node in the
        // last scan and allocates a path for each, so it is built only when
        // the batch stands to gain more than the walk costs.
        let index: Option<HashMap<Digest, String>> = self
            .local_reuse_is_worthwhile(&files)
            .then(|| self.digest_index());
        let mut needs = Vec::new();
        for request in files {
            // Already staged by an interrupted previous cycle, satisfied
            // locally earlier in this batch, or already scheduled for
            // transfer by an earlier request (staging is content-addressed,
            // so one transfer serves every path sharing the digest). If the
            // one scheduled transfer then fails, the digest's paths go
            // unpublished this cycle — but the transition reports the
            // missing content, and the immediate follow-up cycle re-plans
            // from fresh scans, which drops any vanished or changed source
            // path from consideration.
            let hex = digest_hex(&request.digest);
            // Recorded before the de-duplication below: a digest asked for
            // twice is still one this cycle needs.
            self.requested.insert(hex.clone());
            if !staged.insert(hex.clone()) {
                continue;
            }
            if inventory.contains(&hex) {
                // A survivor from an interrupted earlier run carries only
                // its name's claim to the content, and a crash can leave a
                // correctly named file with truncated or missing bytes —
                // this very content was mid-write when the run died.
                // Rehash before trusting it; the read is paid only on
                // reuse hits. A mismatch discards the file and transfers.
                let survivor = staged_path(&self.staging_root, &request.digest);
                if staged_content_matches(&survivor, &request.digest) {
                    continue;
                }
                let _ = fs::remove_file(&survivor);
            }

            // Identical content elsewhere in the root is faster to copy (and
            // verify) than to transfer — but only when the index is worth
            // building at all (see `local_reuse_is_worthwhile`).
            let source = index
                .as_ref()
                .and_then(|index| index.get(&request.digest))
                .cloned();
            if let Some(source) = source {
                // A digest mismatch (the file changed since the scan that
                // indexed it) or a read failure just means the content has to
                // come from the source endpoint after all.
                if let Ok(true) = self.stage_locally(&source, &request.digest) {
                    continue;
                }
            }

            // The content has to be transferred, so describe whatever base
            // content exists at the target path for the source to delta
            // against. The last scan already knows whether the path holds a
            // regular file; anything else yields an empty signature without
            // touching the filesystem (the common case on a cold
            // destination).
            let signature = if self.snapshot_records_file(&request.path) {
                open_base(&self.root, &request.path)
                    .map(base_signature)
                    .unwrap_or_default()
            } else {
                Signature::default()
            };
            needs.push(StagingNeed { request, signature });
        }

        self.receive = Some(ReceiveState {
            needs: needs
                .iter()
                .map(|need| (need.request.digest, need.clone()))
                .collect(),
            current: None,
        });
        Ok(needs)
    }

    fn supply_open(&mut self, needs: Vec<StagingNeed>) -> Result<()> {
        self.supply = Some(SupplyState {
            needs,
            next: 0,
            current: None,
        });
        Ok(())
    }

    fn supply_pull(&mut self, max_frames: usize) -> Result<Vec<TransferFrame>> {
        let Some(mut state) = self.supply.take() else {
            bail!("no supply stream is open");
        };
        // A zero-frame request would otherwise be indistinguishable from
        // exhaustion, which would silently truncate the transfer.
        let limit = max_frames.max(1);

        let mut frames = Vec::new();
        let mut bytes = 0usize;
        while frames.len() < limit && bytes < SUPPLY_TARGET_BYTES {
            let Some(frame) = self.next_supply_frame(&mut state) else {
                break;
            };
            bytes += frame_weight(&frame);
            frames.push(frame);
        }

        // An empty batch signals exhaustion, at which point the stream is
        // closed rather than left open for a pull that will never come.
        if !frames.is_empty() {
            self.supply = Some(state);
        }
        Ok(frames)
    }

    fn stage_push(&mut self, frames: Vec<TransferFrame>) -> Result<()> {
        let Some(mut state) = self.receive.take() else {
            bail!("no staging operation is in progress");
        };
        let result = self.push_frames(&mut state, frames);
        self.receive = Some(state);
        result
    }

    fn await_change(&mut self, timeout: std::time::Duration) -> Result<bool> {
        // The observer holds one watcher for the root and advances a
        // generation on every event. Waiting on the generation this
        // endpoint last scanned at means it wakes for changes it has not
        // seen — including ones that landed while it was busy elsewhere,
        // which a wake token could have lost.
        let observed = self
            .observer
            .await_change_seen(self.seen_generation, timeout);
        // An endpoint that has never scanned is a watcher on a controller's
        // behalf (the agent's second channel for a session, whose scans go
        // through the first), and has no scan to measure the next wait
        // from. What it has is the last change it reported: the controller
        // scanned for that one, so the next wait is for anything after it.
        // An endpoint that scans measures from its scan, as before.
        if let (Some(generation), None) = (observed, &self.last_snapshot) {
            self.seen_generation = generation;
        }
        Ok(observed.is_some())
    }

    fn await_change_since(
        &mut self,
        since: Option<u64>,
        timeout: std::time::Duration,
    ) -> Result<(bool, bool)> {
        // An unwatched root answers at once: waiting would only delay the
        // news that a quiet wait means nothing here.
        if !self.observer.is_watching() {
            return Ok((false, false));
        }
        match since {
            Some(since) => Ok((
                self.observer.await_change_seen(since, timeout).is_some(),
                true,
            )),
            None => Ok((self.await_change(timeout)?, true)),
        }
    }

    fn generation(&self) -> Option<u64> {
        Some(self.seen_generation)
    }

    fn watch_begin(
        &mut self,
        _timeout: std::time::Duration,
        signal: Arc<crate::endpoint::WakeSignal>,
    ) -> Result<()> {
        self.observer.subscribe(&signal);
        Ok(())
    }

    fn watch_poll(&mut self) -> Result<Option<bool>> {
        // Nothing is outstanding: the subscription raises the signal, and
        // the verdict is one comparison against the generation last
        // scanned at — the same test the blocking wait makes.
        Ok(Some(self.observer.generation() > self.seen_generation))
    }

    fn change_activity(&mut self) -> Option<crate::endpoint::ChangeActivity> {
        self.observer.activity()
    }

    fn read_file(&mut self, path: &str) -> Result<Option<Vec<u8>>> {
        let full = resolve_confined(&self.root, path)?;
        // Opened before its type is checked, so the check describes the
        // very file that is read. `O_NOFOLLOW` turns a symbolic link in
        // the final component into ELOOP — not a file, so `None`, as
        // before — and `O_NONBLOCK` keeps a FIFO from stalling the open.
        let file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&full)
        {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) if error.raw_os_error() == Some(libc::ELOOP) => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("unable to read {}", full.display()))
            }
        };
        let metadata = file
            .metadata()
            .with_context(|| format!("unable to read {}", full.display()))?;
        if !metadata.file_type().is_file() {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        file.take(MAXIMUM_READ_FILE_SIZE + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("unable to read {}", full.display()))?;
        if bytes.len() as u64 > MAXIMUM_READ_FILE_SIZE {
            bail!(
                "{path} is larger than {} MiB, too large to read whole",
                MAXIMUM_READ_FILE_SIZE / (1024 * 1024)
            );
        }
        Ok(Some(bytes))
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let source = resolve_confined(&self.root, from)?;
        let target = create_confined_parents(&self.root, to, self.directory_mode)?;
        // Refused rather than overwritten. The caller is preserving
        // something, so a name that is already taken means the caller has
        // guessed wrong about what is free — and `fs::rename` would
        // replace the occupant without a word. This check gives the
        // common case its message; the no-replace rename below is what
        // holds against a name taken after it.
        if fs::symlink_metadata(&target).is_ok() {
            bail!("{to} already exists; move it out of the way first");
        }
        // Both paths are announced before and after, exactly as a
        // transition's writes are: a scan racing this must not publish
        // either name's old state as current.
        self.observer.invalidate([from, to]);
        #[cfg(test)]
        if let Some(hook) = &self.between_announce_and_writes {
            hook();
        }
        let result = publish_rename(&source, &target, false).map_err(|error| {
            if error.kind() == ErrorKind::AlreadyExists {
                anyhow!("{to} already exists; move it out of the way first")
            } else {
                anyhow::Error::new(error).context(format!(
                    "unable to move {} to {}",
                    source.display(),
                    target.display()
                ))
            }
        });
        self.observer.invalidate([from, to]);
        result
    }

    fn transition(&mut self, transitions: Vec<Change>) -> Result<TransitionOutcome> {
        // Validation is performed against the last scan, which the
        // controller's cycle guarantees is the very scan these transitions
        // were reconciled from. (The snapshot is refreshed only *after* all
        // changes are applied, from the achieved results — see below.)
        // Count how many publishes each staged digest could at most serve
        // in this batch, so that a digest's final publish can move the
        // staged file into place instead of copying it — halving the write
        // volume of large (especially cold) transfers.
        let mut staged_uses = HashMap::new();
        for change in &transitions {
            if let Some(node) = &change.new {
                count_staged_uses(node, &mut staged_uses);
            }
        }
        // Publishing is spread over threads (see `apply_helpers`), and the
        // count a publish decrements has to be the same count on every
        // thread, so the tally becomes atomic before the first write.
        let staged_uses: HashMap<Digest, AtomicUsize> = staged_uses
            .into_iter()
            .map(|(digest, uses)| (digest, AtomicUsize::new(uses)))
            .collect();
        let helpers = AtomicUsize::new(apply_helpers());
        let swept = Mutex::new(HashSet::new());
        // The generation of the scan this endpoint's lease came from. The
        // fold offered at the end is built from that lease, so it is
        // offered at that generation: a baseline some sharing session has
        // advanced since must refuse it.
        let lease_generation = self.seen_generation;
        // The observation is about to stop describing the tree, so it is
        // invalidated *before* the first write rather than after the last,
        // and the paths about to change ride along: the watcher's own
        // events for these writes may arrive late, and a scan racing that
        // delivery would otherwise take its incremental path, find nothing
        // dirty, and publish a tree that no longer exists — which every
        // other session sharing this root would then reconcile against.
        self.observer
            .invalidate(transitions.iter().map(|change| change.path.as_str()));
        #[cfg(test)]
        if let Some(hook) = &self.between_announce_and_writes {
            hook();
        }

        let mut transitioner = Transitioner {
            root: &self.root,
            staging_root: &self.staging_root,
            // Validation runs against this endpoint's own lease: the exact
            // scan these transitions were reconciled from, not whatever the
            // observer has published since. That is what keeps "matches the
            // last scan" meaning "unchanged since reconciliation decided
            // this was safe" when a root is shared.
            scanned: self.last_snapshot.as_ref().and_then(|s| s.root.as_ref()),
            behavior: self.observer.behavior(),
            symlink_mode: self.symlink_mode,
            ignores: &self.ignores,
            file_mode: self.file_mode,
            directory_mode: self.directory_mode,
            owner: self.owner,
            group: self.group,
            staged_uses: &staged_uses,
            helpers: &helpers,
            #[cfg(test)]
            after_use_counted: None,
            swept: &swept,
            problems: Vec::new(),
            missing_staged_files: false,
            missing_staged: Vec::new(),
        };
        // Deletions apply before creations and replacements. On a volume
        // with name equivalence rules, a rename that only changes case
        // arrives as a deletion of one spelling and a creation of the
        // other; in emitted (name-sorted) order the creation can run
        // first, find the old spelling through the folded lookup, refuse —
        // and the deletion then leaves the file existing under *neither*
        // name for a cycle. Deleting first frees the folded name. Results
        // are still reported in input order: the controller matches them
        // to transitions by position.
        let mut slots: Vec<Option<Option<Node>>> = vec![None; transitions.len()];
        let (removals, arrivals): (Vec<usize>, Vec<usize>) =
            (0..transitions.len()).partition(|&index| transitions[index].new.is_none());
        for index in removals {
            // Each change is applied independently: a refusal at one path
            // must never abort the rest of the transition.
            slots[index] = Some(transitioner.apply(&transitions[index]));
            if let Some(progress) = &self.progress {
                progress.change_applied();
            }
        }
        // Creations and replacements are independent of each other — the
        // reconciler never emits two changes with one inside the other —
        // so they spread over threads. Not on a volume that folds names:
        // there, two changes whose names fold together are ordered by the
        // refusal the second one meets on disk, and that order is the
        // input's.
        let spread = arrivals.len() >= APPLY_SPREAD_MINIMUM && !folds_names(&transitioner.behavior);
        if !spread {
            for index in arrivals {
                slots[index] = Some(transitioner.apply(&transitions[index]));
                if let Some(progress) = &self.progress {
                    progress.change_applied();
                }
            }
        } else {
            let applied = transitioner.spread(arrivals.len(), |forked, range| {
                arrivals[range]
                    .iter()
                    .map(|&index| (index, forked.apply(&transitions[index])))
                    .collect()
            });
            for (index, result) in applied {
                slots[index] = Some(result);
                if let Some(progress) = &self.progress {
                    progress.change_applied();
                }
            }
        }
        let results: Vec<Option<Node>> = slots
            .into_iter()
            .map(|slot| slot.expect("every transition slot is filled"))
            .collect();
        let outcome = TransitionOutcome {
            results,
            problems: transitioner.problems,
            missing_staged_files: transitioner.missing_staged_files,
            missing_staged: transitioner.missing_staged,
        };

        // The paths are announced *again* now that the writes are done.
        // The pre-write announcement keeps a racing scan from adopting its
        // baseline; but such a scan consumes the announced dirty marks and
        // can still read the old bytes before they change, publishing them
        // at the announced generation. Only this post-write announcement
        // deterministically outdates that publication — the kernel's own
        // events do the same job, but they arrive on their own schedule
        // and never arrive at all under the polling fallback.
        let announced = self
            .observer
            .invalidate(transitions.iter().map(|change| change.path.as_str()));
        // The generation this endpoint has seen moves past its own two
        // announcements, and only when nothing else advanced it since the
        // lease was scanned: a wait from here then wakes for the next
        // change, not for the writes just made. (The next scan still
        // re-reads these paths: the announcements mark them, and marks
        // are not generations.) Anything else that advanced it — a
        // sharing session's writes, an external change, or the kernel's
        // events for these very writes — was never scanned, so the
        // generation stays at the lease's and the next wait wakes at once.
        // For our own kernel events that costs one extra cycle, the one
        // TODO-SPEED.md measured and accepted.
        if announced == lease_generation + 2 {
            self.seen_generation = announced;
        }

        // A disagreement means the filesystem differed from the snapshot the
        // transition was validated against, so the snapshot is known to be
        // wrong somewhere. Incremental scanning trusts the snapshot for
        // everything a watcher has not flagged, and a watcher notification
        // may not even have been delivered yet — so the next scan reads
        // everything rather than adopting a record already proven stale.
        if outcome.problems.iter().any(|problem| problem.disagreement) {
            // Shared, so every session over this root is told: the baseline
            // they would all adopt is the one proven wrong. Only a
            // *disagreement* earns this. A refusal the snapshot predicted —
            // a permission, a folded name — proves nothing about the
            // snapshot, and one that stands for weeks would otherwise force
            // a full walk of the root every cycle for as long as it stood.
            self.observer.distrust_baseline();
        }

        // Fold the achieved results into the retained snapshot and its
        // persisted cache. The results carry the metadata of the entries as
        // created, so the next scan (in this process or the next) re-digests
        // only what changed *after* the transition instead of treating every
        // published file as unknown — on a large cold sync, that's the
        // difference between a metadata sweep and rehashing the whole tree.
        // Refusals and partial applications are safe to fold too: they
        // describe what is actually on disk.
        if let Some(snapshot) = self.last_snapshot.as_ref() {
            match super::fold_transition(snapshot, &transitions, &outcome) {
                Some(folded) => {
                    // Offered to the observer as the next scan's starting
                    // point. It advances the baseline but not the published
                    // generation, so the next scan still runs — it simply
                    // starts from a tree that already knows about this
                    // write instead of re-digesting what was just
                    // published. Offered at the lease's generation, which
                    // is what it was built from: if a sharing session's
                    // scan has moved the baseline past that, the offer is
                    // refused rather than rolling back what that scan saw.
                    self.observer
                        .offer_baseline(folded.clone(), lease_generation);
                    self.last_snapshot = Some(folded);
                }
                // A graft failure (which real transition results shouldn't
                // produce) just drops the baseline, degrading the next scan
                // to a full walk.
                None => self.last_snapshot = None,
            }
        }
        // Everything this cycle asked for has now been published or
        // discarded, so any blob still in staging that no request named is
        // a leftover — and this is the one point where that is known for
        // certain, on both sides, with no message needed.
        self.sweep_staging();
        Ok(outcome)
    }
}

/// The state of an open supply stream: the needs being supplied, the index
/// of the next need to begin, and the source of the one being streamed.
struct SupplyState {
    /// The needs to supply, in order.
    needs: Vec<StagingNeed>,
    /// The index of the next need to begin.
    next: usize,
    /// Where the rest of the current need's content comes from, once it
    /// has begun.
    current: Option<SupplySource>,
}

/// How many operations a delta's helper thread may run ahead of the
/// stream: at most [`SUPPLY_TARGET_BYTES`] of literal data, so a supply
/// holds about two batches — the one being built and this — whatever the
/// file's size.
const SUPPLY_CHANNEL_DEPTH: usize = SUPPLY_TARGET_BYTES / rsync::MAXIMUM_DATA_OPERATION_SIZE;

/// Where the rest of one supplied file's operations come from.
enum SupplySource {
    /// No base to delta against: the file itself, read one operation-sized
    /// chunk per frame.
    Whole {
        /// The root-relative path being read, for errors.
        path: String,
        /// The file.
        file: ScannedFile,
    },
    /// A delta against the destination's base, computed on a helper thread
    /// into a bounded channel. Dropping the receiver ends the thread.
    Delta {
        /// The root-relative path being read, for errors.
        path: String,
        /// The thread's output.
        operations: mpsc::Receiver<DeltaMessage>,
    },
    /// No source could be opened: the file ends with this error.
    Failed(String),
}

impl SupplySource {
    /// The file's next operation, `None` at its end, or the error that
    /// ends it.
    fn next_op(&mut self) -> Result<Option<rsync::Op>, String> {
        match self {
            SupplySource::Whole { path, file } => {
                // Read directly into owned operation-sized chunks — no
                // shared scratch buffer to zero, and no copy out of it.
                // Files within the operation size limit (the vast
                // majority) arrive as a single chunk.
                let mut chunk = Vec::with_capacity(rsync::MAXIMUM_DATA_OPERATION_SIZE);
                match Read::by_ref(file)
                    .take(rsync::MAXIMUM_DATA_OPERATION_SIZE as u64)
                    .read_to_end(&mut chunk)
                {
                    Err(error) => Err(format!("unable to read {path}: {error}")),
                    Ok(0) => Ok(None),
                    Ok(read) => {
                        // Small files would otherwise pin a full-sized
                        // allocation through the batch pipeline.
                        if read < rsync::MAXIMUM_DATA_OPERATION_SIZE / 2 {
                            chunk.shrink_to_fit();
                        }
                        Ok(Some(rsync::Op::Data(chunk)))
                    }
                }
            }
            SupplySource::Delta { path, operations } => match operations.recv() {
                Ok(DeltaMessage::Op(op)) => Ok(Some(op)),
                Ok(DeltaMessage::Done) => Ok(None),
                Ok(DeltaMessage::Failed(error)) => Err(error),
                Err(mpsc::RecvError) => Err(format!("the delta for {path} stopped")),
            },
            SupplySource::Failed(error) => Err(std::mem::take(error)),
        }
    }
}

/// What a delta's helper thread sends: operations, then how it ended.
enum DeltaMessage {
    /// The next operation.
    Op(rsync::Op),
    /// The delta is complete.
    Done,
    /// The delta failed, and the file ends with this error.
    Failed(String),
}

/// A file opened for supply by [`open_scanned`](LocalEndpoint::open_scanned),
/// read no further than the size its scan recorded — a file growing since
/// cannot stream forever — and failing if it ends before that size: a file
/// truncated mid-supply is an error the receiver is told of, not a short
/// file it discovers by digest.
struct ScannedFile {
    /// The open file.
    file: File,
    /// The bytes of the scanned size not yet read.
    remaining: u64,
}

impl Read for ScannedFile {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let wanted = buffer
            .len()
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        if wanted == 0 {
            return Ok(0);
        }
        let read = self.file.read(&mut buffer[..wanted])?;
        if read == 0 {
            return Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                "the file was truncated since the scan",
            ));
        }
        self.remaining -= read as u64;
        Ok(read)
    }
}

/// The state of an in-progress staging operation, running in parallel with
/// the need list returned by the last `stage_begin`: the controller pushes
/// exactly the frames the source pulls, in need order, so the current index
/// alone identifies the file each frame belongs to.
struct ReceiveState {
    /// What this endpoint asked for, by digest. A file leaves the map as
    /// it lands; frames for a digest not in it — content the sender chose
    /// to send before asking, that turned out not to be needed, or a file
    /// sent twice — are not kept.
    needs: HashMap<Digest, StagingNeed>,
    /// The file frames are landing in, if one is open.
    current: Option<Receiving>,
}

/// Where the frames of the current file go.
enum Receiving {
    /// A file this endpoint asked for. The file is boxed: it holds the
    /// digesting writer's buffers, and would otherwise make every state,
    /// the sink included, thousands of bytes.
    File {
        need: StagingNeed,
        file: Box<ReceiveFile>,
    },
    /// Content this endpoint did not ask for: read to the end and dropped.
    Sink,
}

impl ReceiveState {
    /// Discards any partially received content.
    fn discard(self) {
        if let Some(Receiving::File { file, .. }) = self.current {
            file.discard();
        }
    }
}

/// A partially received file: the staging temporary being written (through a
/// digesting writer, so that verification costs nothing beyond the write it
/// already performs) and the base its delta applies against.
struct ReceiveFile {
    /// The temporary being written.
    temporary: PathBuf,
    /// The digesting writer over the temporary.
    writer: DigestingWriter<File>,
    /// The base content for block operations.
    base: PatchBase,
}

impl ReceiveFile {
    /// Discards the partially received content, best-effort.
    fn discard(self) {
        let temporary = self.temporary;
        drop(self.writer);
        let _ = fs::remove_file(temporary);
    }
}

/// The base content that delta operations are applied against: the current
/// content at a need's path, or an empty stream when the path holds nothing
/// usable.
enum PatchBase {
    /// An open regular file.
    File(File),
    /// An empty base.
    Empty(Cursor<&'static [u8]>),
}

impl PatchBase {
    /// Creates an empty base.
    fn empty() -> PatchBase {
        PatchBase::Empty(Cursor::new(b""))
    }
}

impl Read for PatchBase {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            PatchBase::File(file) => file.read(buffer),
            PatchBase::Empty(cursor) => cursor.read(buffer),
        }
    }
}

impl Seek for PatchBase {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        match self {
            PatchBase::File(file) => file.seek(position),
            PatchBase::Empty(cursor) => cursor.seek(position),
        }
    }
}

/// A writer that digests everything it passes through, so that received
/// content is verified without a second pass over it.
struct DigestingWriter<W: Write> {
    /// The underlying writer.
    inner: W,
    /// The digest of everything written so far.
    hasher: blake3::Hasher,
}

impl<W: Write> DigestingWriter<W> {
    /// Wraps a writer in a digester.
    fn new(inner: W) -> DigestingWriter<W> {
        DigestingWriter {
            inner,
            hasher: blake3::Hasher::new(),
        }
    }

    /// Returns the digest of everything written so far.
    fn digest(&self) -> Digest {
        *self.hasher.finalize().as_bytes()
    }
}

impl<W: Write> Write for DigestingWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        // Only the bytes the underlying writer accepted are digested, so a
        // short write can't desynchronize the digest from the content.
        let written = self.inner.write(buffer)?;
        self.hasher.update(&buffer[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// The state of one transition operation: the endpoint's paths and scanned
/// record, plus the problems and flags accumulated across changes.
struct Transitioner<'a> {
    /// The synchronization root.
    root: &'a Path,
    /// The staging directory holding content to be applied.
    staging_root: &'a Path,
    /// The last scan's hierarchy, which all validation is performed against.
    scanned: Option<&'a Node>,
    /// The behavior of the root's filesystem, governing how on-disk names
    /// are matched against the hierarchy's (NFC, case-exact) names.
    behavior: FilesystemBehavior,
    /// The treatment of symbolic links.
    symlink_mode: SymlinkMode,
    /// The endpoint's ignore set: what a pattern excludes, which alone is
    /// removed along with a deleted directory.
    ignores: &'a IgnoreSet,
    /// The permission bits for created non-executable files.
    file_mode: u32,
    /// The permission bits for created directories.
    directory_mode: u32,
    /// The owner ID applied to created entries (`None` to leave alone).
    owner: Option<u32>,
    /// The group ID applied to created entries (`None` to leave alone).
    group: Option<u32>,
    /// Per digest, how many publishes this batch could still require. A
    /// count reaching zero marks a staged file's last possible use, letting
    /// it be moved into place rather than copied. Counts are upper bounds
    /// (a refusal skips publishes without decrementing), which only ever
    /// turns a move into a copy, never the reverse.
    staged_uses: &'a HashMap<Digest, AtomicUsize>,
    /// Threads not currently applying a slice of this transition, shared
    /// by every transitioner of it. See [`apply_helpers`].
    helpers: &'a AtomicUsize,
    /// A test seam between a publish counting its use of staged content
    /// and acting on it — the window in which another publish of the same
    /// digest can take the last use and move the staged file away.
    #[cfg(test)]
    after_use_counted: Option<&'a (dyn Fn() + Sync)>,
    /// The directories already swept of leftover publish temporaries in
    /// this transition, shared by every transitioner of it, so each is
    /// listed once. See [`Transitioner::sweep_leftovers`].
    swept: &'a Mutex<HashSet<PathBuf>>,
    /// The problems accumulated so far.
    problems: Vec<Problem>,
    /// Whether or not any staged content was found missing.
    missing_staged_files: bool,
    /// The content confirmed absent from staging, by path and digest.
    missing_staged: Vec<crate::endpoint::FileRequest>,
}

impl<'a> Transitioner<'a> {
    /// A transitioner for a slice of the work, to run on another thread:
    /// the same configuration and the same shared tallies, its own
    /// problems to report.
    fn fork(&self) -> Transitioner<'a> {
        Transitioner {
            root: self.root,
            staging_root: self.staging_root,
            scanned: self.scanned,
            behavior: self.behavior,
            symlink_mode: self.symlink_mode,
            ignores: self.ignores,
            file_mode: self.file_mode,
            directory_mode: self.directory_mode,
            owner: self.owner,
            group: self.group,
            staged_uses: self.staged_uses,
            helpers: self.helpers,
            #[cfg(test)]
            after_use_counted: self.after_use_counted,
            swept: self.swept,
            problems: Vec::new(),
            missing_staged_files: false,
            missing_staged: Vec::new(),
        }
    }

    /// Folds what a forked transitioner found into this one.
    fn absorb(&mut self, forked: Transitioner<'a>) {
        self.problems.extend(forked.problems);
        self.missing_staged_files |= forked.missing_staged_files;
        self.missing_staged.extend(forked.missing_staged);
    }

    /// Runs `work` over `count` items in contiguous slices, one per thread
    /// this transitioner can claim plus its own, and returns every slice's
    /// results in item order. Problems are folded back in that order too,
    /// so a report reads as it would from one thread. With no helper free
    /// — or nothing to share — the whole range runs here.
    fn spread<T: Send>(
        &mut self,
        count: usize,
        work: impl Fn(&mut Transitioner<'a>, std::ops::Range<usize>) -> Vec<T> + Sync,
    ) -> Vec<T> {
        // Claim helpers one at a time; each claim is one more slice.
        let mut claimed = 0;
        while claimed + 1 < count
            && self
                .helpers
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |free| {
                    free.checked_sub(1)
                })
                .is_ok()
        {
            claimed += 1;
        }
        if claimed == 0 {
            return work(self, 0..count);
        }
        let slices = claimed + 1;
        let width = count.div_ceil(slices);
        let ranges: Vec<std::ops::Range<usize>> = (0..slices)
            .map(|slice| (slice * width).min(count)..((slice + 1) * width).min(count))
            .collect();
        let work = &work;
        let mut results = Vec::with_capacity(count);
        std::thread::scope(|scope| {
            let handles: Vec<_> = ranges[1..]
                .iter()
                .cloned()
                .map(|range| {
                    let mut forked = self.fork();
                    scope.spawn(move || {
                        let out = work(&mut forked, range);
                        // Free for the next slice as soon as this one is
                        // done, not once it is joined.
                        forked.helpers.fetch_add(1, Ordering::AcqRel);
                        (out, forked)
                    })
                })
                .collect();
            results.extend(work(self, ranges[0].clone()));
            for handle in handles {
                let (out, forked) = handle.join().expect("apply thread panicked");
                results.extend(out);
                self.absorb(forked);
            }
        });
        results
    }

    /// Records a problem at a root-relative path.
    ///
    /// Every problem here is a place where the filesystem did not match
    /// what the last scan recorded — a refusal to act on stale
    /// expectations, or an operation that could not complete. Either way
    /// the snapshot no longer describes reality at that path, which the
    /// endpoint uses to decide that its next scan must read rather than
    /// adopt.
    fn problem(&mut self, path: &str, message: impl Into<String>) {
        self.problems.push(Problem {
            path: path.to_owned(),
            message: message.into(),
            disagreement: false,
        });
    }

    /// Records a problem that proves the snapshot wrong: the disk holds
    /// something other than what the last scan recorded at this path. See
    /// [`Problem::disagreement`] for why this is kept apart from a refusal.
    fn disagreement(&mut self, path: &str, message: impl Into<String>) {
        self.problems.push(Problem {
            path: path.to_owned(),
            message: message.into(),
            disagreement: true,
        });
    }

    /// Applies one change, returning the content actually achieved at its
    /// path: the target content on success, the surviving old content on
    /// refusal, or a partial hierarchy where only part of the change landed.
    fn apply(&mut self, change: &Change) -> Option<Node> {
        // Paths arrive from a peer, so they're checked before they're allowed
        // anywhere near the filesystem. Scanning can't produce a component
        // that escapes the root, so a path that contains one is either a
        // defect or an attack; either way it isn't acted upon.
        if let Err(message) = validate_path(&change.path) {
            self.problem(
                &change.path,
                format!("refusing to act on this path: {message}"),
            );
            return sanitize(change.old.clone());
        }
        let result = match (&change.old, &change.new) {
            (None, None) => None,
            (None, Some(new)) => self.create_change(&change.path, new),
            (Some(old), None) => self.remove_change(&change.path, old),
            (Some(old), Some(new)) => self.replace_change(&change.path, old, new),
        };
        sanitize(result)
    }

    /// Resolves the on-disk directory containing `path`, descending only real
    /// directories: every component is verified with `symlink_metadata`, so a
    /// symbolic link anywhere along the way is a refusal rather than a
    /// redirection.
    fn resolve_parent<'p>(&mut self, path: &'p str) -> Option<(PathBuf, &'p str)> {
        let (parent, name) = match path.rfind('/') {
            Some(index) => (&path[..index], &path[index + 1..]),
            None => ("", path),
        };
        let mut current = self.root.to_path_buf();
        if let Err(message) = verify_directory(&current) {
            self.problem(path, format!("unable to resolve path: {message}"));
            return None;
        }
        if !parent.is_empty() {
            for component in parent.split('/') {
                current.push(component);
                if let Err(message) = verify_directory(&current) {
                    self.problem(path, format!("unable to resolve path: {message}"));
                    return None;
                }
            }
        }
        Some((current, name))
    }

    /// Returns the node the last scan recorded at a root-relative path.
    fn scanned_node(&self, path: &str) -> Option<&Node> {
        let mut current = self.scanned?;
        if path.is_empty() {
            return Some(current);
        }
        for component in path.split('/') {
            current = current.child(component)?;
        }
        Some(current)
    }

    /// Validates that the content on disk at `path` is the regular file the
    /// last scan recorded there, and that the scan recorded the expected
    /// content. This is the check that stands between a stale transition and
    /// somebody else's data: the digest ties the file to what reconciliation
    /// decided about, and the metadata ties that decision to content that
    /// hasn't moved since.
    fn validate_file(
        &self,
        path: &str,
        metadata: &Metadata,
        expected: &Digest,
    ) -> Result<(), String> {
        if !metadata.file_type().is_file() {
            return Err("expected a regular file, but found other content".into());
        }
        let Some(node) = self.scanned_node(path) else {
            return Err("the last scan recorded no content at this path".into());
        };
        let Content::File {
            digest,
            metadata: recorded,
            ..
        } = &node.content
        else {
            return Err("the last scan did not record a regular file at this path".into());
        };
        if digest != expected {
            return Err("the content differs from the expected content".into());
        }
        if file_metadata(metadata) != *recorded {
            return Err("the file has been modified since the last scan".into());
        }
        Ok(())
    }

    /// Applies a creation.
    fn create_change(&mut self, path: &str, new: &Node) -> Option<Node> {
        // Creating the synchronization root itself: the root has no parent to
        // resolve within, and it's the one path where creating over an
        // existing (empty) directory is expected rather than suspicious.
        if path.is_empty() {
            let Content::Directory(children) = &new.content else {
                self.problem(
                    path,
                    "refusing to create a non-directory synchronization root",
                );
                return None;
            };
            let root = self.root;
            if let Err(error) = fs::create_dir_all(root) {
                self.problem(
                    path,
                    format!("unable to create the synchronization root: {error}"),
                );
                return None;
            }
            // The configured directory mode applies to the root itself just
            // like any other created directory (parents above the root stay
            // untouched: they aren't synchronization content).
            if let Err(error) =
                fs::set_permissions(root, Permissions::from_mode(self.directory_mode))
            {
                self.problem(
                    path,
                    format!("unable to set the synchronization root's permissions: {error}"),
                );
            }
            self.apply_ownership(path, root);
            // The root did not exist when the observer probed, so the
            // behavior in hand is a default that claims names never fold.
            // Creating children under that assumption on a case- or
            // normalization-insensitive volume published colliding
            // siblings as two successes when only one directory entry
            // existed — and the fabricated sibling later read as a
            // deletion and propagated back to the source. Probe the real
            // filesystem now that it exists.
            self.behavior = crate::scan::probes::probe(root);
            let created = self.create_children(path, root, children);
            return Some(Node::directory(new.name.clone(), created));
        }

        let (parent, name) = self.resolve_parent(path)?;
        if matches!(new.content, Content::File { .. }) {
            self.sweep_leftovers(&parent);
        }
        // Creating over existing content would destroy something nobody
        // asked to destroy: the change carries no expectation about what's
        // there, so there's nothing to validate it against.
        if fs::symlink_metadata(parent.join(name)).is_ok() {
            // The kind goes last, because that is the part read as the
            // cause when these are grouped: twenty files that collided the
            // same way are one problem, and the name in front of it
            // differs for every one of them.
            match folded_twin(&parent, name) {
                Some((twin, kind)) => self.problem(
                    path,
                    format!("{twin:?} is already here under one entry: {kind}"),
                ),
                None => self.problem(path, "refusing to create over existing content"),
            }
            return None;
        }
        self.create_node(path, &parent, name, new)
    }

    /// Creates one node inside an already-resolved directory, recursing into
    /// directory contents. The returned node describes what was actually
    /// created, which for a partially created directory is a partial
    /// hierarchy.
    fn create_node(&mut self, path: &str, parent: &Path, name: &str, node: &Node) -> Option<Node> {
        let target = parent.join(name);
        match &node.content {
            Content::Directory(children) => {
                if let Err(error) = fs::create_dir(&target) {
                    self.problem(path, format!("unable to create directory: {error}"));
                    return None;
                }
                if let Err(error) =
                    fs::set_permissions(&target, Permissions::from_mode(self.directory_mode))
                {
                    // The directory exists and is usable; only its mode is
                    // off, so this is reported without abandoning its
                    // contents.
                    self.problem(
                        path,
                        format!("unable to set directory permissions: {error}"),
                    );
                }
                self.apply_ownership(path, &target);
                let created = self.create_children(path, &target, children);
                Some(Node::directory(name, created))
            }
            Content::File {
                digest, executable, ..
            } => {
                let metadata =
                    self.publish_file(path, parent, &target, digest, *executable, false)?;
                Some(Node {
                    name: name.to_owned(),
                    content: Content::File {
                        digest: *digest,
                        executable: *executable,
                        metadata,
                    },
                })
            }
            Content::Symlink { target: link } => {
                // Symlink policy is enforced on creation as well as at scan
                // time: content arriving from a peer must satisfy the same
                // rules this endpoint's own scans would apply.
                match self.symlink_mode {
                    SymlinkMode::Ignore => {
                        self.problem(
                            path,
                            "refusing to create a symbolic link: symbolic links are ignored \
                             by configuration",
                        );
                        return None;
                    }
                    SymlinkMode::Portable => {
                        if let Err(message) = validate_portable_target(path, link) {
                            self.problem(
                                path,
                                format!("refusing to create a symbolic link: {message}"),
                            );
                            return None;
                        }
                    }
                    SymlinkMode::Raw => {}
                }
                if let Err(error) = symlink(link, &target) {
                    self.problem(path, format!("unable to create symbolic link: {error}"));
                    return None;
                }
                self.apply_ownership(path, &target);
                Some(Node {
                    name: name.to_owned(),
                    content: node.content.clone(),
                })
            }
            Content::Untracked | Content::Problematic { .. } => {
                self.problem(path, "refusing to create unsynchronizable content");
                None
            }
        }
    }

    /// Creates a directory's children, returning those that were actually
    /// created. A child that can't be created is reported and skipped, so
    /// that its siblings still land.
    fn create_children(&mut self, path: &str, directory: &Path, children: &[Node]) -> Vec<Node> {
        let mut created = Vec::with_capacity(children.len());
        // On a volume with name equivalence rules — case-insensitive
        // lookups, or Unicode-normalization-insensitive ones (decomposing
        // volumes are the latter by construction) — sibling names that fold
        // together denote a single on-disk entry; creating the second would
        // silently replace the first, so it's refused up front.
        let behavior = self.behavior;
        let folds_names = folds_names(&behavior);
        let fold = move |name: &str| folded_name(name, &behavior);
        let mut folded: HashMap<String, ()> = HashMap::new();
        let mut accepted: Vec<(&Node, String)> = Vec::with_capacity(children.len());
        for child in children {
            let child_path = path_join(path, &child.name);
            if let Err(message) = validate_name(&child.name) {
                self.problem(
                    &child_path,
                    format!("refusing to create this name: {message}"),
                );
                continue;
            }
            if folds_names && folded.insert(fold(&child.name), ()).is_some() {
                self.problem(
                    &child_path,
                    "refusing to create this entry: its name collides with a sibling's \
                     under this filesystem's name equivalence rules",
                );
                continue;
            }
            accepted.push((child, child_path));
        }
        // The names are settled and distinct, so the entries are created
        // independently — and, when there are enough to matter, on more
        // than one thread. A subtree among them spreads again on its own
        // once inside, so one large directory among small siblings still
        // fills the machine.
        let spread = accepted.len() >= APPLY_SPREAD_MINIMUM
            || accepted
                .iter()
                .any(|(child, _)| matches!(child.content, Content::Directory(_)));
        if !spread || accepted.len() < 2 {
            for (child, child_path) in accepted {
                if let Some(node) = self.create_node(&child_path, directory, &child.name, child) {
                    created.push(node);
                }
            }
            return created;
        }
        let accepted = &accepted;
        created.extend(self.spread(accepted.len(), |forked, range| {
            accepted[range]
                .iter()
                .filter_map(|(child, child_path)| {
                    forked.create_node(child_path, directory, &child.name, child)
                })
                .collect()
        }));
        created
    }

    /// Publishes staged content at a target path. A digest's last use sets
    /// the staged file's permissions in place and renames it directly onto
    /// the target — two syscalls, no data movement. Earlier uses (and
    /// cross-filesystem staging roots, where the rename fails) copy to a
    /// temporary beside the target and rename that into place, keeping the
    /// staged content available for the paths that still share it. Either
    /// way the target's transition from old content to new is atomic.
    fn publish_file(
        &mut self,
        path: &str,
        parent: &Path,
        target: &Path,
        digest: &Digest,
        executable: bool,
        replace: bool,
    ) -> Option<FileMetadata> {
        let staged = staged_path(self.staging_root, digest);
        let mode = creation_mode(self.file_mode, executable);
        // The staged file is opened *before* this publish counts its use.
        // Once counted, the publish that takes the count to zero may move
        // the file away at any moment, and an earlier use that opened only
        // afterwards found nothing and scheduled a needless retransfer.
        // Holding the handle, an earlier use copies the content it opened,
        // wherever the name has gone since.
        let opened = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&staged);
        let last_use = match self.staged_uses.get(digest) {
            Some(count) => {
                // Counted down atomically: the publish that takes the
                // count to zero is the last use, whichever thread it is on.
                let before = count
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |uses| {
                        Some(uses.saturating_sub(1))
                    })
                    .unwrap_or(0);
                before <= 1
            }
            None => false,
        };
        #[cfg(test)]
        if let Some(hook) = self.after_use_counted {
            hook();
        }

        // A missing staged file surfaces as NotFound from whichever
        // operation touches it first: the content was never supplied or has
        // since vanished, and the controller runs another cycle immediately.
        let missing = |error: &io::Error| error.kind() == ErrorKind::NotFound;
        let mut input = match opened {
            Ok(input) => input,
            Err(error) if missing(&error) => {
                self.retransfer(
                    path,
                    digest,
                    "staged content is unavailable; it will be retransferred on the next \
                     cycle",
                );
                return None;
            }
            Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
                // Staging only ever writes regular files there, so a link
                // is not content; it goes, and the content comes again.
                let _ = fs::remove_file(&staged);
                self.retransfer(
                    path,
                    digest,
                    "staged content is not a regular file; it will be retransferred on the \
                     next cycle",
                );
                return None;
            }
            Err(error) => {
                self.problem(path, format!("unable to stage content into place: {error}"));
                return None;
            }
        };

        // The achieved metadata is captured from the *staged* file, after
        // its permissions are final and before the rename publishes it.
        // Rename preserves inode, size, and modification time, so this is
        // the same metadata a stat of the target would return when nothing
        // interferes — with one decisive difference: it can never describe
        // a foreign file. Stat'ing the target after the rename could catch
        // an editor's save landing in that window, and the achieved node
        // then paired the *requested* digest with the *editor's* metadata.
        // Folded into the ancestor and the baseline, that pair made the
        // editor's content invisible to every later scan and let a
        // validated transition overwrite it.
        // The move is taken only for a staged entry that is still a
        // regular file whose bytes still match the digest, and whose name
        // still leads to the file this publish opened. Receive verified
        // those bytes once, but the digest-named path is addressable
        // between then and now, and a rename would promote whatever sits
        // there into the tree *as* the verified content — with the
        // achieved record then pairing the requested digest with the
        // impostor's own metadata, hiding it from every later scan.
        // Anything doubtful falls through to the copy path, which digests
        // what it moves and turns a mismatch into a retransfer.
        let mut published: Option<FileMetadata> = None;
        let moved = last_use
            && input.set_permissions(Permissions::from_mode(mode)).is_ok()
            && match input.metadata() {
                Ok(opened) if opened.file_type().is_file() => {
                    published = Some(file_metadata(&opened));
                    content_matches(&mut input, digest)
                        && fs::symlink_metadata(&staged).is_ok_and(|named| {
                            (named.dev(), named.ino()) == (opened.dev(), opened.ino())
                        })
                }
                _ => false,
            }
            && publish_rename(&staged, target, replace).is_ok();
        if !moved {
            let temporary = parent.join(temporary_name("apply"));
            // The copy digests what it moves: staged content is normally
            // verified when it is received, but a file surviving from an
            // interrupted earlier run carries only its name's claim, and a
            // crash can leave a correctly named file with truncated bytes.
            // Publishing that would install content matching nothing and
            // then model it as correct.
            let copied = match input
                .seek(SeekFrom::Start(0))
                .with_context(|| format!("unable to read {}", staged.display()))
                .and_then(|_| copy_into_private(&mut input, &staged, &temporary, digest))
            {
                Ok(true) => Ok(()),
                Ok(false) => {
                    let _ = fs::remove_file(&temporary);
                    let _ = fs::remove_file(&staged);
                    self.retransfer(
                        path,
                        digest,
                        "staged content does not match its digest; it will be \
                         retransferred on the next cycle",
                    );
                    return None;
                }
                Err(error) => Err(error),
            };
            if let Err(error) = copied {
                let error = match error.downcast::<io::Error>() {
                    Ok(io_error) => io_error,
                    Err(other) => {
                        let _ = fs::remove_file(&temporary);
                        self.problem(
                            path,
                            format!("unable to stage content into place: {other:#}"),
                        );
                        return None;
                    }
                };
                let _ = fs::remove_file(&temporary);
                // NotFound can also mean the target's parent vanished
                // concurrently, so the staged side is confirmed missing
                // (specifically absent, not merely unprobeable) before
                // scheduling a retransfer.
                let staged_absent = matches!(
                    fs::symlink_metadata(&staged),
                    Err(ref probe) if probe.kind() == ErrorKind::NotFound
                );
                if missing(&error) && staged_absent {
                    self.retransfer(
                        path,
                        digest,
                        "staged content is unavailable; it will be retransferred on the next \
                         cycle",
                    );
                } else {
                    self.problem(path, format!("unable to stage content into place: {error}"));
                }
                return None;
            }
            if let Err(error) = fs::set_permissions(&temporary, Permissions::from_mode(mode)) {
                let _ = fs::remove_file(&temporary);
                self.problem(path, format!("unable to set file permissions: {error}"));
                return None;
            }
            match fs::symlink_metadata(&temporary) {
                Ok(metadata) => published = Some(file_metadata(&metadata)),
                Err(error) => {
                    let _ = fs::remove_file(&temporary);
                    self.problem(path, format!("unable to probe staged content: {error}"));
                    return None;
                }
            }
            if let Err(error) = publish_rename(&temporary, target, replace) {
                let _ = fs::remove_file(&temporary);
                if !replace && error.kind() == ErrorKind::AlreadyExists {
                    // A creation carries no expectation about existing
                    // content, so anything that appeared since the absence
                    // check is someone else's work and must not be
                    // replaced. The next cycle reconciles the newcomer.
                    self.problem(
                        path,
                        "refusing to create over content that appeared concurrently",
                    );
                } else {
                    self.problem(path, format!("unable to publish content: {error}"));
                }
                return None;
            }
        }

        self.apply_ownership(path, target);
        published
    }

    /// Removes what a crashed copy-publish left in `directory`: an
    /// `.autobahn-tmp-apply-*` file old enough that no copy is still
    /// writing it, whose process is gone (or which is old enough that its
    /// process identifier has surely been reused). Scans hide such files
    /// and the staging sweep never looks here, so without this a large
    /// file's leftover leaked its size in disk for good.
    ///
    /// Called before publishing into a directory that already existed;
    /// each directory is listed once per transition.
    fn sweep_leftovers(&mut self, directory: &Path) {
        if !self
            .swept
            .lock()
            .expect("the swept set is never poisoned")
            .insert(directory.to_path_buf())
        {
            return;
        }
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if stale_publish_leftover(name, &entry) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    /// Records that the staged content for `path` is gone or unusable, so
    /// the controller transfers it again, with a problem saying why.
    fn retransfer(&mut self, path: &str, digest: &Digest, message: &str) {
        self.missing_staged_files = true;
        self.missing_staged.push(crate::endpoint::FileRequest {
            path: path.to_owned(),
            digest: *digest,
        });
        self.problem(path, message);
    }

    /// Applies the configured ownership to a created entry (best-effort:
    /// a failure is a reported problem, not a reason to abandon content
    /// that is already correctly in place). `lchown` never follows the
    /// final path component, so it is safe for every entry kind.
    fn apply_ownership(&mut self, path: &str, disk_path: &Path) {
        if self.owner.is_none() && self.group.is_none() {
            return;
        }
        if let Err(error) = std::os::unix::fs::lchown(disk_path, self.owner, self.group) {
            self.problem(path, format!("unable to set ownership: {error}"));
        }
    }

    /// Applies a removal.
    fn remove_change(&mut self, path: &str, expectation: &Node) -> Option<Node> {
        if path.is_empty() {
            // The session layer halts before a root deletion reaches an
            // endpoint; this is the second lock on the same door.
            self.problem(path, "refusing to remove the synchronization root");
            return Some(expectation.clone());
        }
        let Some((parent, name)) = self.resolve_parent(path) else {
            return Some(expectation.clone());
        };
        self.remove_entry(path, &parent.join(name), expectation)
    }

    /// Removes one entry, validating it against the last scan first and
    /// recursing bottom-up through directories. Returns the content that
    /// survived: `None` when the entry is gone, and a partial hierarchy when
    /// some of it had to be left in place.
    fn remove_entry(&mut self, path: &str, target: &Path, expectation: &Node) -> Option<Node> {
        let metadata = match fs::symlink_metadata(target) {
            Ok(metadata) => metadata,
            // Already absent: the intended state, reached by other means.
            Err(error) if error.kind() == ErrorKind::NotFound => return None,
            Err(error) => {
                self.problem(path, format!("unable to probe content: {error}"));
                return Some(expectation.clone());
            }
        };

        match &expectation.content {
            Content::File { digest, .. } => {
                if let Err(message) = self.validate_file(path, &metadata, digest) {
                    self.disagreement(path, format!("refusing to remove this file: {message}"));
                    return Some(expectation.clone());
                }
                match fs::remove_file(target) {
                    Ok(()) => None,
                    Err(error) => {
                        self.problem(path, format!("unable to remove file: {error}"));
                        Some(expectation.clone())
                    }
                }
            }
            Content::Symlink { target: expected } => {
                if !metadata.file_type().is_symlink() {
                    self.disagreement(
                        path,
                        "refusing to remove this entry: expected a symbolic link, but found other content",
                    );
                    return Some(expectation.clone());
                }
                match fs::read_link(target) {
                    Ok(actual) if actual.to_str() == Some(expected.as_str()) => {}
                    Ok(_) => {
                        self.disagreement(
                            path,
                            "refusing to remove this symbolic link: it has been retargeted since the last scan",
                        );
                        return Some(expectation.clone());
                    }
                    Err(error) => {
                        self.problem(path, format!("unable to read symbolic link: {error}"));
                        return Some(expectation.clone());
                    }
                }
                match fs::remove_file(target) {
                    Ok(()) => None,
                    Err(error) => {
                        self.problem(path, format!("unable to remove symbolic link: {error}"));
                        Some(expectation.clone())
                    }
                }
            }
            Content::Directory(_) => self.remove_directory(path, target, &metadata, expectation),
            Content::Untracked | Content::Problematic { .. } => {
                self.problem(path, "refusing to remove unsynchronizable content");
                Some(expectation.clone())
            }
        }
    }

    /// Removes a directory bottom-up. Every entry present on disk must be
    /// accounted for by the expectation: content that reconciliation never
    /// saw is content nobody decided to delete, so it's left in place (which
    /// necessarily leaves its parents in place too).
    fn remove_directory(
        &mut self,
        path: &str,
        target: &Path,
        metadata: &Metadata,
        expectation: &Node,
    ) -> Option<Node> {
        if !metadata.file_type().is_dir() {
            self.disagreement(
                path,
                "refusing to remove this entry: expected a directory, but found other content",
            );
            return Some(expectation.clone());
        }
        let entries = match fs::read_dir(target) {
            Ok(entries) => entries,
            Err(error) => {
                self.problem(path, format!("unable to list directory: {error}"));
                return Some(expectation.clone());
            }
        };

        let mut survivors = Vec::new();
        let mut unexpected = false;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    self.problem(path, format!("unable to list directory: {error}"));
                    unexpected = true;
                    continue;
                }
            };
            let raw_name = entry.file_name();
            let Some(name) = raw_name.to_str() else {
                // Scanning records non-UTF-8 names under a marked, lossy name
                // that can't be matched back to a directory entry, so such an
                // entry is never something this removal expected.
                self.problem(
                    path,
                    "refusing to remove this directory: it contains an entry whose name is not valid UTF-8",
                );
                unexpected = true;
                continue;
            };
            // On a decomposing volume the on-disk name is NFD while the
            // expectation (like every hierarchy name) is NFC; recompose
            // before matching, or every non-ASCII name would read as
            // unexpected content.
            let name = if self.behavior.decomposes_unicode {
                recompose(name)
            } else {
                name.to_owned()
            };
            let name = name.as_str();
            let child_path = path_join(path, name);
            match expectation.child(name) {
                Some(child) => {
                    if let Some(survivor) = self.remove_entry(&child_path, &entry.path(), child) {
                        survivors.push(survivor);
                    }
                }
                None if scan::autobahn_temporary(name) => {
                    // Autobahn's own litter — a crashed publish's
                    // temporary, most likely. Scans never record it, so it
                    // is always unaccounted for; treated as unexpected it
                    // kept the directory and reported a disagreement,
                    // which forced a full walk every cycle, forever. No
                    // peer can create such a name (see `validate_name`),
                    // so it goes with the directory.
                    let removed = match entry.file_type() {
                        Ok(kind) if kind.is_dir() => fs::remove_dir_all(entry.path()),
                        _ => fs::remove_file(entry.path()),
                    };
                    if let Err(error) = removed {
                        self.problem(
                            &child_path,
                            format!("unable to remove a leftover temporary: {error}"),
                        );
                        unexpected = true;
                    }
                }
                None => {
                    // Two very different things reach here. An expectation
                    // never mentions excluded content, so an ignored entry
                    // is *always* unaccounted for; the last scan is what
                    // tells them apart, since it records excluded entries
                    // as untracked nodes and knows nothing of an entry that
                    // arrived after it ran.
                    let excluded = matches!(
                        self.scanned_node(&child_path).map(|node| &node.content),
                        Some(Content::Untracked)
                    );
                    if !excluded {
                        self.disagreement(
                            &child_path,
                            "refusing to remove unexpected content that appeared since the last scan",
                        );
                        unexpected = true;
                        continue;
                    }
                    // Content a pattern excludes goes with the directory
                    // around it.
                    //
                    // An ignore says which files synchronization carries,
                    // not which files exist. Deleting a directory is an
                    // instruction about the directory, and honouring it
                    // halfway — taking the source and leaving the `.git`
                    // and the `node_modules` — obeys neither reading: the
                    // tree is not deleted, and what remains is litter that
                    // nobody asked for and that synchronization can never
                    // clear.
                    //
                    // Nothing here was ever scanned, so there is no
                    // expectation to validate against and none is
                    // pretended. The protection for the case that matters
                    // — another session's root sitting inside an ignored
                    // path — is that session's own root-deletion halt,
                    // which stops it before it carries the loss any
                    // further.
                    //
                    // The scan records far more than ignored entries as
                    // untracked, though: a file over the size limit, a
                    // FIFO or socket or device, a symbolic link under the
                    // `ignore` symlink mode, a mount point. Nobody asked
                    // to leave any of those out, and the other side has
                    // never seen them, so a deletion there would be the
                    // only copy lost. They stay, and so does the directory
                    // holding them.
                    let kind = entry.file_type().ok();
                    let is_directory = kind.is_some_and(|kind| kind.is_dir());
                    if !self.pattern_ignored(&child_path, is_directory) {
                        let reason = match kind {
                            Some(kind) if kind.is_file() => "excluded by size",
                            Some(kind) if kind.is_symlink() => "excluded by the symlink mode",
                            Some(kind) if kind.is_dir() => "excluded as a mount point",
                            _ => "excluded by type",
                        };
                        self.problem(
                            &child_path,
                            format!("left in place, with its directory: {reason}"),
                        );
                        unexpected = true;
                        continue;
                    }
                    let removed = match is_directory {
                        true => fs::remove_dir_all(entry.path()),
                        false => fs::remove_file(entry.path()),
                    };
                    if let Err(error) = removed {
                        self.problem(
                            &child_path,
                            format!("unable to remove excluded content: {error}"),
                        );
                        unexpected = true;
                    }
                }
            }
        }

        // Anything known to have survived keeps the directory itself alive.
        if !survivors.is_empty() || unexpected {
            return Some(Node::directory(expectation.name.clone(), survivors));
        }
        match fs::remove_dir(target) {
            Ok(()) => None,
            Err(error) => {
                // The directory acquired content between the listing and the
                // removal; `remove_dir` refuses to recurse, so nothing was
                // lost.
                self.problem(path, format!("unable to remove directory: {error}"));
                Some(Node::directory(expectation.name.clone(), Vec::new()))
            }
        }
    }

    /// Whether an ignore pattern excludes the entry at `path`: the entry
    /// itself, or a directory above it — inside an ignored directory that a
    /// negation opened for walking, everything not re-included is ignored.
    fn pattern_ignored(&self, path: &str, is_directory: bool) -> bool {
        self.ignores.ignored(path, is_directory)
            || path
                .match_indices('/')
                .any(|(index, _)| self.ignores.ignored(&path[..index], true))
    }

    /// Gives a validated file new permission bits, returning its achieved
    /// metadata.
    ///
    /// In place when the file has one link, through the handle this opens
    /// (checked to be the file validated as `seen`). A file with more than
    /// one link is published anew instead — copied beside itself, given
    /// the mode, renamed over — because a chmod changes the inode, and the
    /// inode's other names may lie outside the root: a local user could
    /// hardlink a file they cannot change into the tree, and a sync would
    /// then change that file's mode for them. Breaking the link leaves the
    /// outside name as it was.
    fn change_mode(
        &mut self,
        path: &str,
        parent: &Path,
        target: &Path,
        seen: &Metadata,
        digest: &Digest,
        mode: u32,
    ) -> Option<FileMetadata> {
        let opened = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(target);
        let mut file = match opened {
            Ok(file) => file,
            // A file this user may not read can still be theirs to chmod,
            // as it always could; with a single link, nothing else shares
            // the inode.
            Err(error) if error.kind() == ErrorKind::PermissionDenied && seen.nlink() == 1 => {
                if let Err(error) = fs::set_permissions(target, Permissions::from_mode(mode)) {
                    self.problem(path, format!("unable to set file permissions: {error}"));
                    return None;
                }
                return Some(match fs::symlink_metadata(target) {
                    Ok(metadata) => file_metadata(&metadata),
                    Err(error) => {
                        self.problem(path, format!("unable to probe the modified file: {error}"));
                        FileMetadata::default()
                    }
                });
            }
            Err(error) => {
                self.problem(
                    path,
                    format!("unable to open the file to set its permissions: {error}"),
                );
                return None;
            }
        };
        let opened = match file.metadata() {
            Ok(opened) => opened,
            Err(error) => {
                self.problem(path, format!("unable to probe content: {error}"));
                return None;
            }
        };
        if (opened.dev(), opened.ino()) != (seen.dev(), seen.ino()) {
            self.disagreement(
                path,
                "refusing to set this file's permissions: it was replaced as it was checked",
            );
            return None;
        }

        if opened.nlink() <= 1 {
            if let Err(error) = file.set_permissions(Permissions::from_mode(mode)) {
                self.problem(path, format!("unable to set file permissions: {error}"));
                return None;
            }
            return Some(match file.metadata() {
                Ok(metadata) => file_metadata(&metadata),
                Err(error) => {
                    self.problem(path, format!("unable to probe the modified file: {error}"));
                    FileMetadata::default()
                }
            });
        }

        let temporary = parent.join(temporary_name("apply"));
        match copy_into_private(&mut file, target, &temporary, digest) {
            Ok(true) => {}
            Ok(false) => {
                let _ = fs::remove_file(&temporary);
                self.disagreement(
                    path,
                    "refusing to set this file's permissions: the file has been modified since \
                     the last scan",
                );
                return None;
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                self.problem(
                    path,
                    format!("unable to copy a linked file to set its permissions: {error:#}"),
                );
                return None;
            }
        }
        let published = fs::set_permissions(&temporary, Permissions::from_mode(mode))
            .and_then(|()| fs::symlink_metadata(&temporary))
            .and_then(|metadata| {
                publish_rename(&temporary, target, true).map(|()| file_metadata(&metadata))
            });
        match published {
            Ok(metadata) => {
                self.apply_ownership(path, target);
                Some(metadata)
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                self.problem(path, format!("unable to set file permissions: {error}"));
                None
            }
        }
    }

    /// Applies a replacement.
    fn replace_change(&mut self, path: &str, old: &Node, new: &Node) -> Option<Node> {
        if path.is_empty() {
            // A root replacement would require removing the root, which is
            // never permitted. Reconciliation can't produce one (both sides'
            // roots are directories, which agree shallowly), so this is a
            // guard rather than a case.
            self.problem(
                path,
                "refusing to replace the synchronization root, which would require removing it",
            );
            return Some(old.clone());
        }
        let Some((parent, name)) = self.resolve_parent(path) else {
            return Some(old.clone());
        };
        if matches!(new.content, Content::File { .. }) {
            self.sweep_leftovers(&parent);
        }
        let target = parent.join(name);

        // File-to-file replacements are performed in place, which is both
        // faster and safer than a removal followed by a creation: the path
        // never transiently ceases to exist.
        if let (
            Content::File {
                digest: old_digest, ..
            },
            Content::File {
                digest: new_digest,
                executable,
                ..
            },
        ) = (&old.content, &new.content)
        {
            let metadata = match fs::symlink_metadata(&target) {
                Ok(metadata) => metadata,
                Err(error) => {
                    self.problem(path, format!("unable to probe content: {error}"));
                    return Some(old.clone());
                }
            };
            if let Err(message) = self.validate_file(path, &metadata, old_digest) {
                self.disagreement(path, format!("refusing to replace this file: {message}"));
                return Some(old.clone());
            }

            if old_digest == new_digest {
                // Only executability differs, so the content is left entirely
                // alone: this is a permission change, not a rewrite.
                let mode = creation_mode(self.file_mode, *executable);
                let Some(metadata) =
                    self.change_mode(path, &parent, &target, &metadata, new_digest, mode)
                else {
                    return Some(old.clone());
                };
                return Some(Node {
                    name: name.to_owned(),
                    content: Content::File {
                        digest: *new_digest,
                        executable: *executable,
                        metadata,
                    },
                });
            }

            let Some(metadata) =
                self.publish_file(path, &parent, &target, new_digest, *executable, true)
            else {
                return Some(old.clone());
            };
            return Some(Node {
                name: name.to_owned(),
                content: Content::File {
                    digest: *new_digest,
                    executable: *executable,
                    metadata,
                },
            });
        }

        // Everything else (type changes, and anything involving a directory)
        // is a validated removal followed by a creation. If the removal
        // refuses, the creation must not proceed — the old content is still
        // there.
        if let Some(survivor) = self.remove_entry(path, &target, old) {
            self.problem(
                path,
                "refusing to create replacement content: the existing content could not be removed",
            );
            return Some(survivor);
        }
        self.create_node(path, &parent, name, new)
    }
}

/// Filters unsynchronizable content out of a transition result.
///
/// Results become ancestor content, and the ancestor may only ever contain
/// synchronizable content — the session validates this and treats a violation
/// as fatal to the whole cycle. Reconciliation never puts unsynchronizable
/// content into a transition's expectation, so this is a guard rather than a
/// transformation; it exists so that a defect anywhere upstream degrades to a
/// path the ancestor simply doesn't describe, rather than to a failed cycle.
fn sanitize(result: Option<Node>) -> Option<Node> {
    result.as_ref().and_then(Node::synchronizable_subtree)
}

/// Computes the permission bits for a created file: the configured file
/// mode, with executability granted (where readability already is) for
/// executable files.
fn creation_mode(file_mode: u32, executable: bool) -> u32 {
    if executable {
        file_mode | ((file_mode & 0o444) >> 2)
    } else {
        file_mode
    }
}

/// Returns the staging path for content with the specified digest.
/// Accumulates, per digest, how many file publishes the given hierarchy
/// could at most require.
/// The fewest independent changes, or entries of one directory, worth
/// spreading over threads: below this, the threads cost more than they
/// return.
const APPLY_SPREAD_MINIMUM: usize = 8;

/// The most threads one transition spreads over, itself included.
///
/// Publishing a file reads it back to verify its digest before the rename
/// (see `publish_file`), so a large arrival — a cold sync, an unpacked
/// archive — is bound by hashing as much as by the filesystem, and both
/// spread by entry. The cap keeps a wide host from being taken over by one
/// transition; the budget is further cut to the cores actually present.
const APPLY_THREADS_MAX: usize = 8;

/// How many threads a transition may add beside the one it runs on.
fn apply_helpers() -> usize {
    std::thread::available_parallelism()
        .map(|cores| cores.get())
        .unwrap_or(1)
        .min(APPLY_THREADS_MAX)
        .saturating_sub(1)
}

fn count_staged_uses(node: &Node, uses: &mut HashMap<Digest, usize>) {
    match &node.content {
        Content::File { digest, .. } => *uses.entry(*digest).or_insert(0) += 1,
        Content::Directory(children) => {
            for child in children.iter() {
                count_staged_uses(child, uses);
            }
        }
        _ => {}
    }
}

/// Computes the staging root for a placement mode. `state_staging` is the
/// state-area location used by [`StagingMode::State`]; the root-relative
/// placements build a hidden, scan-excluded directory name from the session
/// identifier and side so that concurrent sessions (and the two sides of
/// one session) never share staging space.
pub fn staging_root_for(
    mode: crate::endpoint::StagingMode,
    root: &Path,
    state_staging: PathBuf,
    session: &str,
    side: &str,
) -> Result<PathBuf> {
    use crate::endpoint::StagingMode;
    let name = || format!("{TEMPORARY_PREFIX}-staging-{session}-{side}");
    match mode {
        StagingMode::State => Ok(state_staging),
        StagingMode::BesideRoot => {
            let parent = root
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .with_context(|| {
                    format!(
                        "the synchronization root {} has no parent to stage beside",
                        root.display()
                    )
                })?;
            Ok(parent.join(name()))
        }
        StagingMode::InsideRoot => Ok(root.join(name())),
    }
}

/// Makes the staging directory ready to receive into: a real directory,
/// owned by this user, that only this user can use.
///
/// Inside the root, the staging directory's name is one a peer could once
/// create — as a symbolic link to anywhere, which a plain `create_dir_all`
/// accepted and staging then wrote through. [`private_dir`] refuses a link,
/// a non-directory and another user's directory, creates with mode `0700`,
/// and tightens an older, looser one rather than refusing it, so staging
/// directories made by earlier versions keep working.
///
/// A missing parent is created, owner-only: for the state placement that
/// is the state area's own `staging` directory, and for the inside-root
/// one the root itself, which staging has always been able to create
/// before the transition that fills it. The root is then verified without
/// following links, as the transition path verifies it; the other parents
/// lie outside the synchronized tree, where a peer cannot put a link, and
/// may legitimately be reached through one.
///
/// [`private_dir`]: crate::fsutil::private_dir
fn prepare_staging_root(staging_root: &Path, root: &Path) -> Result<()> {
    let parent = staging_root
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .with_context(|| {
            format!(
                "the staging directory {} has no parent",
                staging_root.display()
            )
        })?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .with_context(|| format!("unable to create {}", parent.display()))?;
    if parent == root {
        verify_directory(parent).map_err(|error| anyhow!("unable to stage: {error}"))?;
    } else if !fs::metadata(parent)
        .with_context(|| format!("unable to inspect {}", parent.display()))?
        .is_dir()
    {
        bail!("unable to stage: {} is not a directory", parent.display());
    }
    crate::fsutil::private_dir(staging_root).context("unable to prepare the staging directory")
}

/// Renders a digest as the lowercase hex name its staged content lives under.
fn digest_hex(digest: &Digest) -> String {
    use std::fmt::Write;
    let mut name = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(name, "{byte:02x}");
    }
    name
}

fn staged_path(staging_root: &Path, digest: &Digest) -> PathBuf {
    staging_root.join(digest_hex(digest))
}

/// Generates a unique temporary file name carrying the scan-invisible prefix.
/// Names are unique within a process and, through the process identifier,
/// between concurrent processes sharing a directory. The trailing token is
/// a keyed hash of the counter under a per-process random key, so another
/// local user who can see the process identifier still cannot predict the
/// next name to plant something there.
fn temporary_name(purpose: &str) -> String {
    temporary_name_at(purpose, TEMPORARY_COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// The name [`temporary_name`] gives `purpose` when the counter stands at
/// `count`: tests use it to plant something at a name about to be used.
fn temporary_name_at(purpose: &str, count: u64) -> String {
    static KEY: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    let key = KEY.get_or_init(|| {
        let seed = crate::fsutil::random_hex(16).unwrap_or_else(|_| {
            // No /dev/urandom: the names stay unique, and the private
            // creation of every temporary still refuses a planted entry.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            format!("{}-{}", std::process::id(), now.as_nanos())
        });
        *blake3::hash(seed.as_bytes()).as_bytes()
    });
    let token = blake3::keyed_hash(key, &count.to_le_bytes());
    format!(
        "{TEMPORARY_PREFIX}-{purpose}-{}-{count}-{}",
        std::process::id(),
        &token.to_hex()[..16]
    )
}

/// Warns when a synchronization root lives on a filesystem whose caching
/// and event semantics undermine local-filesystem assumptions. Detection is
/// best-effort (Linux and macOS); the probe walks up to the deepest
/// existing ancestor so a missing root is still classified by the volume
/// it will be created on.
fn warn_if_network_filesystem(root: &Path) {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut probe = root.to_path_buf();
        while !probe.exists() {
            match probe.parent() {
                Some(parent) => probe = parent.to_path_buf(),
                None => return,
            }
        }
        let Ok(path) = std::ffi::CString::new(probe.as_os_str().as_bytes()) else {
            return;
        };
        let mut stats: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(path.as_ptr(), &mut stats) } != 0 {
            return;
        }
        // macOS names the filesystem instead of numbering it.
        let name = unsafe { std::ffi::CStr::from_ptr(stats.f_fstypename.as_ptr()) };
        let Ok(name) = name.to_str() else { return };
        let lowered = name.to_ascii_lowercase();
        let kind = match lowered.as_str() {
            "nfs" => "NFS",
            "smbfs" => "SMB",
            "cifs" => "CIFS",
            "webdav" => "WebDAV",
            "afpfs" => "AFP",
            _ if lowered.contains("fuse") => "FUSE",
            _ => return,
        };
        eprintln!(
            "warning: {} is on {kind}; synchronization of network filesystems is \
             best-effort and assumes this client is the only writer — attribute \
             caching can hide another client's changes from both scanning and \
             the checks that guard destructive operations",
            root.display()
        );
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut probe = root.to_path_buf();
        while !probe.exists() {
            match probe.parent() {
                Some(parent) => probe = parent.to_path_buf(),
                None => return,
            }
        }
        let Ok(path) = std::ffi::CString::new(probe.as_os_str().as_bytes()) else {
            return;
        };
        let mut stats: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(path.as_ptr(), &mut stats) } != 0 {
            return;
        }
        let kind = match stats.f_type {
            0x6969 => "NFS",
            0x517b => "SMB",
            0xff53_4d42 => "CIFS",
            0xfe53_4d42 => "SMB2",
            0x6573_5546 => "FUSE",
            _ => return,
        };
        eprintln!(
            "warning: {} is on {kind}; synchronization of network filesystems is \
             best-effort and assumes this client is the only writer — attribute \
             caching can hide another client's changes from both scanning and \
             the checks that guard destructive operations",
            root.display()
        );
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = root;
}

/// Resolves a root-relative path to its on-disk location, refusing any
/// path that is not one scanning could have produced, or whose root or
/// parent components are not real directories: a symbolic link along the
/// way is a refusal, never a redirection out of the root. The final
/// component is returned unresolved; callers decide whether to follow it.
fn resolve_confined(root: &Path, path: &str) -> Result<PathBuf> {
    validate_path(path)
        .map_err(|error| anyhow!("{path:?} is not a plain root-relative path: {error}"))?;
    if path.is_empty() {
        bail!("the synchronization root itself cannot be named here");
    }
    let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
    let mut current = root.to_path_buf();
    verify_directory(&current).map_err(|error| anyhow!("unable to resolve {path:?}: {error}"))?;
    for component in parent.split('/').filter(|component| !component.is_empty()) {
        current.push(component);
        verify_directory(&current)
            .map_err(|error| anyhow!("unable to resolve {path:?}: {error}"))?;
    }
    Ok(current.join(name))
}

/// Like [`resolve_confined`], but creates missing parent directories, one
/// component at a time with the endpoint's directory mode, verifying each
/// as it goes: `create_dir_all` would follow a symbolic link anywhere
/// along the way.
fn create_confined_parents(root: &Path, path: &str, directory_mode: u32) -> Result<PathBuf> {
    validate_path(path)
        .map_err(|error| anyhow!("{path:?} is not a plain root-relative path: {error}"))?;
    if path.is_empty() {
        bail!("the synchronization root itself cannot be named here");
    }
    let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
    let mut current = root.to_path_buf();
    verify_directory(&current).map_err(|error| anyhow!("unable to resolve {path:?}: {error}"))?;
    for component in parent.split('/').filter(|component| !component.is_empty()) {
        current.push(component);
        match fs::DirBuilder::new().mode(directory_mode).create(&current) {
            Ok(()) => {
                // The mode given at creation is narrowed by the umask; the
                // configured mode is what every created directory gets.
                fs::set_permissions(&current, Permissions::from_mode(directory_mode))
                    .with_context(|| {
                        format!("unable to set permissions on {}", current.display())
                    })?;
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("unable to create {}", current.display()))
            }
        }
        verify_directory(&current)
            .map_err(|error| anyhow!("unable to resolve {path:?}: {error}"))?;
    }
    Ok(current.join(name))
}

/// Whether a staged file's bytes hash to the digest its name claims. Used
/// before trusting content that survived from an earlier run; a fresh
/// transfer is verified as it is received and never needs this.
fn staged_content_matches(path: &Path, digest: &Digest) -> bool {
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; 128 * 1024];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                hasher.update(&buffer[..count]);
            }
            Err(_) => return false,
        }
    }
    hasher.finalize().as_bytes() == digest
}

/// How long a copy-publish temporary must have gone unmodified before it is
/// taken for a crash's leftover. A copy in progress writes continuously,
/// so its modification time stays current; this only has to outlast a
/// stalled write.
const LEFTOVER_MINIMUM_AGE: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Whether a directory entry is a copy-publish temporary that nothing will
/// finish: see [`Transitioner::sweep_leftovers`].
fn stale_publish_leftover(name: &str, entry: &fs::DirEntry) -> bool {
    if !scan::autobahn_temporary(name) {
        return false;
    }
    let Some(pid) = name
        .strip_prefix(TEMPORARY_PREFIX)
        .and_then(|rest| rest.strip_prefix("-apply-"))
        .and_then(|rest| rest.split('-').next())
        .and_then(|pid| pid.parse::<libc::pid_t>().ok())
    else {
        return false;
    };
    let Ok(metadata) = entry.metadata() else {
        return false;
    };
    let age = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .unwrap_or_default();
    metadata.file_type().is_file()
        && age >= LEFTOVER_MINIMUM_AGE
        && (!process_running(pid) || age >= crate::fsutil::TMP_MAX_AGE)
}

/// Whether a process with this identifier exists on this host.
fn process_running(pid: libc::pid_t) -> bool {
    if pid <= 0 {
        return false;
    }
    if pid as u32 == std::process::id() {
        return true;
    }
    // SAFETY: signal 0 only checks that the process exists and may be
    // signalled; nothing is sent.
    let signalled = unsafe { libc::kill(pid, 0) };
    signalled == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Whether an open file's bytes, read from its start, hash to `digest`.
fn content_matches(file: &mut File, digest: &Digest) -> bool {
    if file.seek(SeekFrom::Start(0)).is_err() {
        return false;
    }
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; 128 * 1024];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                hasher.update(&buffer[..count]);
            }
            Err(_) => return false,
        }
    }
    hasher.finalize().as_bytes() == digest
}

/// Renames staged content onto its target. A replacement uses the ordinary
/// overwrite-capable rename; a *creation* refuses to replace anything: it
/// carries no expectation about existing content, so a file that appeared
/// between the absence check and this rename — an editor's save, most
/// plainly — belongs to someone else. Linux enforces that atomically with
/// `RENAME_NOREPLACE` and macOS with `renamex_np(RENAME_EXCL)`; elsewhere
/// the check-then-rename window remains and is documented as residual.
fn publish_rename(source: &Path, target: &Path, replace: bool) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    if !replace {
        use std::os::unix::ffi::OsStrExt;
        let source_c = std::ffi::CString::new(source.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(ErrorKind::InvalidInput))?;
        let target_c = std::ffi::CString::new(target.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(ErrorKind::InvalidInput))?;
        let result =
            unsafe { libc::renamex_np(source_c.as_ptr(), target_c.as_ptr(), libc::RENAME_EXCL) };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        // Filesystems without RENAME_EXCL support (some network and FUSE
        // volumes) report ENOTSUP or EINVAL; falling back to the plain
        // rename there keeps the old (windowed) behavior rather than
        // failing every creation.
        if error.raw_os_error() != Some(libc::ENOTSUP) && error.raw_os_error() != Some(libc::EINVAL)
        {
            return Err(error);
        }
    }
    #[cfg(target_os = "linux")]
    if !replace {
        use std::os::unix::ffi::OsStrExt;
        let source_c = std::ffi::CString::new(source.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(ErrorKind::InvalidInput))?;
        let target_c = std::ffi::CString::new(target.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(ErrorKind::InvalidInput))?;
        // Invoked as a raw syscall rather than through libc's wrapper:
        // musl did not export `renameat2` until 1.2.5, so linking the
        // wrapper fails outright on the static musl targets the Linux
        // agents are built for. The syscall number is stable.
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                source_c.as_ptr(),
                libc::AT_FDCWD,
                target_c.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        // A filesystem without RENAME_NOREPLACE support reports EINVAL, and
        // a kernel older than 3.15 has no such call at all (ENOSYS); some
        // stacks answer EOPNOTSUPP. Falling back to the plain rename in
        // those cases keeps the documented check-then-rename window rather
        // than failing every creation — the residual RETAINED.md section 2
        // describes.
        if !matches!(
            error.raw_os_error(),
            Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP)
        ) {
            return Err(error);
        }
    }
    let _ = replace;
    fs::rename(source, target)
}

/// Streams already-open content into a new private temporary while
/// digesting it, returning whether the content matched the expected
/// digest. The temporary is created with [`private_file`]: `0600` until
/// publication gives it its configured mode, and never through anything
/// already at that name, a planted symbolic link included.
///
/// [`private_file`]: crate::fsutil::private_file
fn copy_into_private(
    input: &mut File,
    source: &Path,
    temporary: &Path,
    digest: &Digest,
) -> Result<bool> {
    let mut output = crate::fsutil::private_file(temporary)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; COPY_BUFFER_SIZE];
    loop {
        let count = input
            .read(&mut buffer)
            .with_context(|| format!("unable to read {}", source.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        output
            .write_all(&buffer[..count])
            .with_context(|| format!("unable to write {}", temporary.display()))?;
    }
    output
        .flush()
        .with_context(|| format!("unable to flush {}", temporary.display()))?;
    Ok(hasher.finalize().as_bytes() == digest)
}

/// Opens the regular file at a root-relative path as a delta base, or
/// `None` when there is none to use.
///
/// The base's signature goes back to the supplier, so whatever this opens
/// is readable, block by block, by the peer that named the path: it must be
/// inside the root. Every parent must be a real directory, not a symbolic
/// link to one, and the file opens with `O_NOFOLLOW | O_NONBLOCK` and is
/// checked through its own descriptor, so a final symbolic link is refused
/// and a FIFO neither blocks the open nor serves as a base. The path must
/// already be validated: no `..`, not absolute.
fn open_base(root: &Path, path: &str) -> Option<File> {
    let mut parent = root.to_path_buf();
    let mut components = path.split('/').peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        parent.push(component);
        if !fs::symlink_metadata(&parent).ok()?.file_type().is_dir() {
            return None;
        }
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(root.join(path))
        .ok()?;
    file.metadata().ok()?.file_type().is_file().then_some(file)
}

/// Computes the rsync signature of an opened base (see [`open_base`]).
///
/// No base, or an unreadable one, yields an empty signature, which is
/// exactly right: with no usable base, delta generation degenerates to
/// streaming the content, and correctness never depends on the base being
/// what the destination expected.
fn base_signature(file: File) -> Signature {
    let Ok(metadata) = file.metadata() else {
        return Signature::default();
    };
    rsync::signature(file, rsync::optimal_block_size(metadata.len())).unwrap_or_default()
}

/// Verifies that a path is a real directory, without following symbolic
/// links.
fn verify_directory(path: &Path) -> Result<(), String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if !metadata.file_type().is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    Ok(())
}

/// Extracts the metadata recorded on file nodes, matching what the scanner
/// records so that the two can be compared directly.
fn file_metadata(metadata: &Metadata) -> FileMetadata {
    FileMetadata {
        mtime_seconds: metadata.mtime(),
        mtime_nanos: metadata.mtime_nsec() as u32,
        size: metadata.size(),
        inode: metadata.ino(),
        mode: metadata.mode(),
    }
}

/// Validates a single path component: it must be a name that scanning could
/// have produced, which in particular excludes anything that would escape the
/// synchronization root.
fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("empty path component".into());
    }
    if name == "." || name == ".." {
        return Err("dot path component".into());
    }
    if name.contains('/') || name.contains('\0') {
        return Err("path component contains a separator or NUL".into());
    }
    // Scanning hides autobahn's own temporaries and reports every other
    // name in the reserved space as unsynchronizable, so no genuine
    // controller proposes one. A peer that could create one would make
    // an entry every later scan skips — or, named as an inside-root
    // staging directory, a symbolic link that staging writes through.
    if name.starts_with(TEMPORARY_PREFIX) {
        return Err("path component uses a name reserved for autobahn".into());
    }
    Ok(())
}

/// Validates a root-relative path component by component. The empty path (the
/// synchronization root itself) is valid.
fn validate_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Ok(());
    }
    for component in path.split('/') {
        validate_name(component)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::symlink;
    use tempfile::{tempdir, TempDir};

    use crate::rsync::Op;
    use crate::session::transition_dependencies;
    use crate::tree::diff;

    /// A pair of endpoints over two roots, with isolated staging.
    struct Fixture {
        _keep: TempDir,
        alpha_root: PathBuf,
        beta_root: PathBuf,
        alpha: LocalEndpoint,
        beta: LocalEndpoint,
    }

    impl Fixture {
        fn new() -> Fixture {
            let keep = tempdir().expect("temporary directory should be creatable");
            let alpha_root = keep.path().join("alpha");
            let beta_root = keep.path().join("beta");
            fs::create_dir_all(&alpha_root).expect("alpha root should be creatable");
            fs::create_dir_all(&beta_root).expect("beta root should be creatable");
            let alpha = endpoint(&alpha_root, &keep.path().join("staging-alpha"));
            let beta = endpoint(&beta_root, &keep.path().join("staging-beta"));
            Fixture {
                _keep: keep,
                alpha_root,
                beta_root,
                alpha,
                beta,
            }
        }

        /// Scans both endpoints and returns the changes that would bring beta
        /// into agreement with alpha.
        fn beta_transitions(&mut self) -> Vec<Change> {
            let alpha = self.alpha.scan().expect("alpha scan should succeed");
            let beta = self.beta.scan().expect("beta scan should succeed");
            diff(beta.root.as_ref(), alpha.root.as_ref())
        }

        /// Drives a complete staging exchange from alpha into beta, exactly
        /// as the session controller does (with a deliberately small batch
        /// size, so that per-file buffers are drained across several pulls).
        fn stage(&mut self, transitions: &[Change]) -> Vec<StagingNeed> {
            let requests = transition_dependencies(transitions);
            let needs = self
                .beta
                .stage_begin(requests)
                .expect("staging should begin");
            if needs.is_empty() {
                return needs;
            }
            self.alpha
                .supply_open(needs.clone())
                .expect("supply should open");
            loop {
                let frames = self.alpha.supply_pull(3).expect("supply should pull");
                if frames.is_empty() {
                    break;
                }
                self.beta.stage_push(frames).expect("staging should accept");
            }
            needs
        }
    }

    #[test]
    fn oversized_files_scan_as_untracked_and_are_never_digested() {
        let keep = tempdir().expect("temporary directory should be creatable");
        let root = keep.path().join("root");
        fs::create_dir_all(&root).expect("root should be creatable");
        fs::write(root.join("small.txt"), b"fits").expect("file should be writable");
        fs::write(root.join("large.bin"), vec![7u8; 4096]).expect("file should be writable");

        let mut endpoint = LocalEndpoint::new(
            root,
            keep.path().join("staging"),
            EndpointOptions {
                max_file_size: Some(1024),
                ..EndpointOptions::default()
            },
        )
        .expect("endpoint should be creatable");
        let snapshot = endpoint.scan().expect("scan should succeed");
        assert_eq!(snapshot.files, 1);
        let root_node = snapshot.root.as_ref().expect("root should exist");
        assert!(matches!(
            root_node.child("large.bin").expect("recorded").content,
            Content::Untracked
        ));
        assert!(matches!(
            root_node.child("small.txt").expect("recorded").content,
            Content::File { .. }
        ));
    }

    #[test]
    fn exceeding_the_entry_limit_fails_the_scan() {
        let keep = tempdir().expect("temporary directory should be creatable");
        let root = keep.path().join("root");
        fs::create_dir_all(&root).expect("root should be creatable");
        for index in 0..5 {
            fs::write(root.join(format!("file{index}.txt")), b"x")
                .expect("file should be writable");
        }
        let mut endpoint = LocalEndpoint::new(
            root,
            keep.path().join("staging"),
            EndpointOptions {
                max_entry_count: Some(3),
                ..EndpointOptions::default()
            },
        )
        .expect("endpoint should be creatable");
        let error = format!("{:#}", endpoint.scan().expect_err("the scan must fail"));
        assert!(error.contains("exceeding the configured limit"), "{error}");
    }

    #[test]
    fn staging_placements_compute_scan_excluded_locations() {
        use crate::endpoint::StagingMode;
        let root = Path::new("/data/project");
        let state = PathBuf::from("/state/staging-beta");
        assert_eq!(
            staging_root_for(StagingMode::State, root, state.clone(), "s1", "beta").unwrap(),
            state
        );
        let beside =
            staging_root_for(StagingMode::BesideRoot, root, state.clone(), "s1", "beta").unwrap();
        assert_eq!(beside, PathBuf::from("/data/.autobahn-tmp-staging-s1-beta"));
        let inside =
            staging_root_for(StagingMode::InsideRoot, root, state.clone(), "s1", "beta").unwrap();
        assert_eq!(
            inside,
            PathBuf::from("/data/project/.autobahn-tmp-staging-s1-beta")
        );
        // The root of the filesystem has nothing to stage beside.
        assert!(
            staging_root_for(StagingMode::BesideRoot, Path::new("/"), state, "s1", "beta").is_err()
        );
    }

    #[test]
    fn inside_root_staging_is_invisible_to_synchronization() {
        use crate::endpoint::StagingMode;
        let keep = tempdir().expect("temporary directory should be creatable");
        let alpha_root = keep.path().join("alpha");
        let beta_root = keep.path().join("beta");
        fs::create_dir_all(&alpha_root).expect("alpha root should be creatable");
        fs::create_dir_all(&beta_root).expect("beta root should be creatable");
        fs::write(alpha_root.join("file.txt"), b"content").expect("file should be writable");

        let staging = |root: &Path, side: &str| {
            staging_root_for(
                StagingMode::InsideRoot,
                root,
                PathBuf::new(),
                "session-x",
                side,
            )
            .expect("staging root should compute")
        };
        let mut alpha = LocalEndpoint::new(
            alpha_root.clone(),
            staging(&alpha_root, "alpha"),
            EndpointOptions::default(),
        )
        .expect("endpoint should be creatable");
        let mut beta = LocalEndpoint::new(
            beta_root.clone(),
            staging(&beta_root, "beta"),
            EndpointOptions::default(),
        )
        .expect("endpoint should be creatable");

        // The staging directories live inside the roots, but never appear
        // in a scan.
        let snapshot = alpha.scan().expect("alpha scan should succeed");
        let root_node = snapshot.root.as_ref().expect("root should exist");
        assert_eq!(root_node.children().len(), 1);
        let snapshot = beta.scan().expect("beta scan should succeed");
        assert_eq!(
            snapshot
                .root
                .as_ref()
                .expect("root should exist")
                .children()
                .len(),
            0
        );
    }

    #[test]
    fn created_entries_receive_the_configured_ownership() {
        // Chown to one's own IDs is permitted without privileges, so the
        // application path is exercised for real, if tautologically.
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        let keep = tempdir().expect("temporary directory should be creatable");
        let alpha_root = keep.path().join("alpha");
        let beta_root = keep.path().join("beta");
        fs::create_dir_all(&alpha_root).expect("alpha root should be creatable");
        fs::create_dir_all(&beta_root).expect("beta root should be creatable");
        write(&alpha_root, "dir/file.txt", "content");

        let mut alpha = endpoint(&alpha_root, &keep.path().join("staging-alpha"));
        let mut beta = LocalEndpoint::new(
            beta_root.clone(),
            keep.path().join("staging-beta"),
            EndpointOptions {
                default_owner: Some(format!("id:{uid}")),
                default_group: Some(format!("id:{gid}")),
                ..EndpointOptions::default()
            },
        )
        .expect("endpoint should be creatable");

        let alpha_snapshot = alpha.scan().expect("alpha scan should succeed");
        let beta_snapshot = beta.scan().expect("beta scan should succeed");
        let changes = crate::tree::reconcile(
            None,
            alpha_snapshot.root.as_ref(),
            beta_snapshot.root.as_ref(),
            crate::tree::SyncMode::TwoWaySafe,
        )
        .beta_transitions;
        let needs = beta
            .stage_begin(transition_dependencies(&changes))
            .expect("staging should begin");
        alpha.supply_open(needs).expect("supply should open");
        loop {
            let frames = alpha.supply_pull(usize::MAX).expect("supply should pull");
            if frames.is_empty() {
                break;
            }
            beta.stage_push(frames).expect("push should succeed");
        }
        let outcome = beta.transition(changes).expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        let metadata =
            fs::symlink_metadata(beta_root.join("dir/file.txt")).expect("file should exist");
        assert_eq!(metadata.uid(), uid);
        assert_eq!(metadata.gid(), gid);
    }

    /// The verify escape hatch: content rewritten with its metadata
    /// restored — same length, same mtime, same inode — is invisible to
    /// every ordinary scan by design (the founding trade of scan-based
    /// synchronization). A verified scan re-reads everything and sees it.
    #[test]
    fn a_verified_scan_sees_what_metadata_hides() {
        let keep = tempdir().expect("temporary directory");
        let root = keep.path().join("root");
        let staging = keep.path().join("staging");
        fs::create_dir_all(&root).expect("root");
        let path = root.join("forged.txt");
        fs::write(&path, b"first version").expect("writes");
        // Old enough that the racy-timestamp rule trusts the digest.
        let moment = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
        let set_mtime = || {
            fs::File::options()
                .write(true)
                .open(&path)
                .expect("opens")
                .set_modified(moment)
                .expect("mtime");
        };
        set_mtime();

        let mut endpoint = LocalEndpoint::new(root.clone(), staging, EndpointOptions::default())
            .expect("endpoint");
        let first = endpoint.scan().expect("scans");
        let digest_of = |snapshot: &Snapshot| match &snapshot
            .root
            .as_ref()
            .and_then(|root| root.child("forged.txt"))
            .expect("present")
            .content
        {
            Content::File { digest, .. } => *digest,
            _ => panic!("expected a file"),
        };
        let original = digest_of(&first);

        // The forgery: same length, restored mtime, same inode.
        fs::File::options()
            .write(true)
            .open(&path)
            .expect("opens")
            .write_all(b"forgd version")
            .expect("writes");
        set_mtime();

        let ordinary = endpoint.scan().expect("scans");
        assert_eq!(
            digest_of(&ordinary),
            original,
            "an ordinary scan must miss the forgery — that miss is the \
             documented design trade this verb exists to answer"
        );

        let verified = endpoint.scan_verified().expect("verifies");
        assert_ne!(
            digest_of(&verified),
            original,
            "the verified scan must see the true content"
        );
        // And having seen it once, ordinary scans stay correct: the
        // verified snapshot is the published baseline now.
        let after = endpoint.scan().expect("scans");
        assert_eq!(digest_of(&after), digest_of(&verified));
    }

    /// A creation must refuse to replace content that appeared after its
    /// absence check, on every platform that can express that atomically:
    /// `RENAME_NOREPLACE` on Linux, `renamex_np(RENAME_EXCL)` on macOS.
    /// Platforms with neither — FreeBSD among them — keep the plain
    /// rename and therefore the documented check-then-rename window
    /// (RETAINED.md section 2); this pins which behavior each gets rather
    /// than assuming the atomic one everywhere.
    #[test]
    fn a_creation_rename_refuses_to_replace() {
        let keep = tempdir().expect("temporary directory");
        let source = keep.path().join("source");
        let target = keep.path().join("target");
        fs::write(&source, b"staged").expect("writes");
        fs::write(&target, b"an editor's save").expect("writes");

        let atomic_no_replace = cfg!(any(target_os = "linux", target_os = "macos"));
        let refused = publish_rename(&source, &target, false);
        if !atomic_no_replace {
            // The residual, pinned as a residual: the rename lands, and
            // the platform is one this project documents as windowed.
            assert!(refused.is_ok(), "the fallback rename must still work");
            assert_eq!(fs::read(&target).expect("reads"), b"staged");
            return;
        }
        assert!(refused.is_err(), "a creation replaced existing content");
        assert_eq!(
            fs::read(&target).expect("reads"),
            b"an editor's save",
            "the concurrent content must survive"
        );

        let replaced = publish_rename(&source, &target, true);
        assert!(replaced.is_ok(), "a replacement must still replace");
        assert_eq!(fs::read(&target).expect("reads"), b"staged");
    }

    /// A root created by a transition is probed before its children are:
    /// the observer's behavior for a missing root is a default, and
    /// creating children under a wrong default published colliding names
    /// as two successes on folding volumes — or, in this inverted fixture,
    /// refused non-colliding names on a case-sensitive one.
    #[test]
    fn a_created_root_is_probed_before_its_children() {
        let mut fixture = Fixture::new();
        // The beta root goes missing, so its next scan probes nothing and
        // a root-creating transition is planned.
        fs::remove_dir_all(&fixture.beta_root).expect("beta root removes");
        write(&fixture.alpha_root, "a", "lower");
        write(&fixture.alpha_root, "A", "UPPER");
        // The wrong conclusion a stale default would carry: the real
        // filesystem is case-sensitive, so `a` and `A` are distinct — but
        // a transitioner trusting this behavior refuses the second as a
        // fold collision.
        fixture.beta.force_behavior(FilesystemBehavior {
            case_insensitive: true,
            ..FilesystemBehavior::default()
        });
        let transitions = fixture.beta_transitions();
        fixture.stage(&transitions);
        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("transition should apply");
        assert!(
            outcome.problems.is_empty(),
            "distinct names on a case-sensitive volume were refused: {:?}",
            outcome.problems
        );
        assert!(fixture.beta_root.join("a").exists() && fixture.beta_root.join("A").exists());
    }

    /// A staged survivor from an interrupted run is rehashed before its
    /// name is trusted: a crash can leave a correctly named file holding
    /// the wrong bytes, and publishing it would install content matching
    /// nothing while modelling it as correct.
    #[test]
    fn a_corrupt_staged_survivor_is_retransferred_not_trusted() {
        let keep = tempdir().expect("temporary directory");
        let root = keep.path().join("root");
        let staging = keep.path().join("staging");
        fs::create_dir_all(&root).expect("root");
        fs::create_dir_all(&staging).expect("staging");
        let mut endpoint =
            LocalEndpoint::new(root.clone(), staging.clone(), EndpointOptions::default())
                .expect("endpoint");
        let _ = endpoint.scan().expect("scan");

        // The digest names honest content; the file holds something else.
        let digest = *blake3::hash(b"the real content").as_bytes();
        fs::write(staging.join(digest_hex(&digest)), b"crash-damaged bytes!!").expect("writes");

        let needs = endpoint
            .stage_begin(vec![crate::endpoint::FileRequest {
                path: "file.txt".into(),
                digest,
            }])
            .expect("stage_begin");
        assert_eq!(
            needs.len(),
            1,
            "a corrupt survivor must be scheduled for transfer, not trusted"
        );
        assert!(
            !staging.join(digest_hex(&digest)).exists(),
            "the corrupt survivor must be discarded"
        );
    }

    fn endpoint(root: &Path, staging: &Path) -> LocalEndpoint {
        LocalEndpoint::new(
            root.to_path_buf(),
            staging.to_path_buf(),
            EndpointOptions::default(),
        )
        .expect("endpoint should be creatable")
    }

    fn write(root: &Path, path: &str, contents: &str) {
        let full = root.join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("parent should be creatable");
        }
        fs::write(&full, contents).expect("file should be writable");
    }

    fn read(root: &Path, path: &str) -> String {
        fs::read_to_string(root.join(path)).expect("file should be readable")
    }

    fn executable(root: &Path, path: &str) -> bool {
        fs::symlink_metadata(root.join(path))
            .expect("file should exist")
            .mode()
            & 0o111
            != 0
    }

    fn inode(root: &Path, path: &str) -> u64 {
        fs::symlink_metadata(root.join(path))
            .expect("file should exist")
            .ino()
    }

    /// Returns the node at a root-relative path within a snapshot.
    fn node_at(snapshot: &Snapshot, path: &str) -> Node {
        let mut current = snapshot.root.as_ref().expect("root should exist");
        if !path.is_empty() {
            for component in path.split('/') {
                current = current
                    .child(component)
                    .unwrap_or_else(|| panic!("{path} should exist in the snapshot"));
            }
        }
        current.clone()
    }

    /// Generates deterministic pseudo-random content.
    fn pseudo_random(length: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        let mut data = Vec::with_capacity(length + 8);
        while data.len() < length {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            data.extend_from_slice(&state.to_le_bytes());
        }
        data.truncate(length);
        data
    }

    #[test]
    fn published_content_moves_out_of_staging_on_its_last_use() {
        let mut fixture = Fixture::new();
        write(&fixture.alpha_root, "one.txt", "shared content");
        write(&fixture.alpha_root, "two.txt", "shared content");
        write(&fixture.alpha_root, "three.txt", "unique content");

        let transitions = fixture.beta_transitions();
        fixture.stage(&transitions);
        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);

        // Every target holds its content...
        assert_eq!(read(&fixture.beta_root, "one.txt"), "shared content");
        assert_eq!(read(&fixture.beta_root, "two.txt"), "shared content");
        assert_eq!(read(&fixture.beta_root, "three.txt"), "unique content");
        // ...and each digest's last publish consumed its staged file, so
        // the staging root retains no content (only, possibly, empty
        // bookkeeping entries such as the scan cache).
        let leftovers: Vec<_> = fs::read_dir(fixture._keep.path().join("staging-beta"))
            .expect("staging root should be readable")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.metadata().map(|m| m.len() > 0).unwrap_or(false))
            .map(|entry| entry.file_name())
            .collect();
        assert!(
            leftovers.is_empty(),
            "staged content remained: {leftovers:?}"
        );
    }

    #[test]
    fn supply_recovers_from_an_alternate_path_sharing_the_digest() {
        let mut fixture = Fixture::new();
        write(&fixture.alpha_root, "a.txt", "shared content");
        write(&fixture.alpha_root, "b.txt", "shared content");
        let transitions = fixture.beta_transitions();
        // The first path vanishes after the scan; its content must still
        // be supplied from the surviving duplicate.
        fs::remove_file(fixture.alpha_root.join("a.txt")).expect("file should be removable");
        fixture.stage(&transitions);
        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        assert!(!outcome.missing_staged_files);
        assert_eq!(read(&fixture.beta_root, "a.txt"), "shared content");
        assert_eq!(read(&fixture.beta_root, "b.txt"), "shared content");
    }

    #[test]
    fn transition_folds_achieved_results_into_the_snapshot() {
        let mut fixture = Fixture::new();
        write(&fixture.alpha_root, "a.txt", "alpha content");
        write(&fixture.alpha_root, "dir/b.txt", "nested content");

        let transitions = fixture.beta_transitions();
        fixture.stage(&transitions);
        fixture
            .beta
            .transition(transitions)
            .expect("transition should succeed");

        // The retained snapshot describes the achieved state — counters
        // included — and agrees with what a fresh scan observes, which is
        // what lets that scan skip re-digesting the published content.
        let retained = fixture
            .beta
            .last_snapshot
            .clone()
            .expect("a snapshot should be retained");
        assert_eq!(retained.files, 2);
        assert_eq!(retained.directories, 2);
        let rescanned = fixture.beta.scan().expect("scan should succeed");
        assert!(retained.content_equal(&rescanned));
    }

    #[test]
    fn staging_and_transition_round_trip() {
        let mut fixture = Fixture::new();
        write(&fixture.alpha_root, "top.txt", "top level");
        write(&fixture.alpha_root, "dir/inner.txt", "inner content");
        write(&fixture.alpha_root, "dir/tool.sh", "#!/bin/sh\n");
        write(&fixture.alpha_root, "dir/empty.txt", "");
        fs::set_permissions(
            fixture.alpha_root.join("dir/tool.sh"),
            Permissions::from_mode(0o755),
        )
        .expect("permissions should be settable");
        symlink("inner.txt", fixture.alpha_root.join("dir/link"))
            .expect("symlink should be creatable");
        fs::create_dir_all(fixture.alpha_root.join("empty"))
            .expect("directory should be creatable");

        let transitions = fixture.beta_transitions();
        let needs = fixture.stage(&transitions);
        // Every file needs transferring: beta is empty, so nothing can be
        // satisfied locally. (The empty file is a need too, and arrives as a
        // bare end-of-file frame.)
        assert_eq!(needs.len(), 4);
        assert!(needs.iter().all(|need| need.signature.is_empty()));

        let outcome = fixture
            .beta
            .transition(transitions.clone())
            .expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        assert!(!outcome.missing_staged_files);
        assert_eq!(outcome.results.len(), transitions.len());
        assert!(outcome.results.iter().all(Option::is_some));

        // The destination now matches the source, byte for byte.
        assert_eq!(read(&fixture.beta_root, "top.txt"), "top level");
        assert_eq!(read(&fixture.beta_root, "dir/inner.txt"), "inner content");
        assert_eq!(read(&fixture.beta_root, "dir/empty.txt"), "");
        assert!(executable(&fixture.beta_root, "dir/tool.sh"));
        assert!(!executable(&fixture.beta_root, "dir/inner.txt"));
        assert_eq!(
            fs::read_link(fixture.beta_root.join("dir/link")).expect("link should be readable"),
            Path::new("inner.txt")
        );
        assert!(fixture.beta_root.join("empty").is_dir());

        // A rescan of beta agrees with alpha's hierarchy, and a further
        // reconciliation has nothing left to do.
        let further = fixture.beta_transitions();
        assert!(further.is_empty(), "{further:?}");
    }

    #[test]
    fn missing_root_is_created_by_transition() {
        let mut fixture = Fixture::new();
        fs::remove_dir_all(&fixture.beta_root).expect("beta root should be removable");
        write(&fixture.alpha_root, "dir/inner.txt", "inner");

        let transitions = fixture.beta_transitions();
        assert_eq!(transitions.len(), 1);
        assert!(transitions[0].path.is_empty());
        fixture.stage(&transitions);
        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        assert_eq!(read(&fixture.beta_root, "dir/inner.txt"), "inner");
    }

    #[test]
    fn delta_transfer_reuses_the_destination_base() {
        let mut fixture = Fixture::new();
        let shared = pseudo_random(200_000, 0x1234);
        let mut alpha_content = shared.clone();
        alpha_content.extend_from_slice(b"alpha tail");
        let mut beta_content = shared.clone();
        beta_content.extend_from_slice(b"beta tail, which differs");
        fs::write(fixture.alpha_root.join("big.bin"), &alpha_content)
            .expect("file should be writable");
        fs::write(fixture.beta_root.join("big.bin"), &beta_content)
            .expect("file should be writable");

        let transitions = fixture.beta_transitions();
        assert_eq!(transitions.len(), 1);
        let requests = transition_dependencies(&transitions);
        let needs = fixture
            .beta
            .stage_begin(requests)
            .expect("staging should begin");
        assert_eq!(needs.len(), 1);
        // The destination's existing content is described, so the transfer
        // can be a delta rather than a copy.
        assert!(!needs[0].signature.is_empty());
        assert!(!needs[0].signature.hashes.is_empty());

        fixture
            .alpha
            .supply_open(needs)
            .expect("supply should open");
        let mut blocks = 0u64;
        let mut data = 0usize;
        loop {
            let frames = fixture.alpha.supply_pull(3).expect("supply should pull");
            if frames.is_empty() {
                break;
            }
            assert!(frames.len() <= 3);
            for frame in &frames {
                match frame {
                    TransferFrame::Begin { .. } => {}
                    TransferFrame::Op(Op::Blocks { count, .. }) => blocks += count,
                    TransferFrame::Op(Op::Data(bytes)) => data += bytes.len(),
                    TransferFrame::EndOfFile { error } => assert!(error.is_none()),
                }
            }
            fixture
                .beta
                .stage_push(frames)
                .expect("staging should accept");
        }
        // The shared prefix transferred as block references, not as data.
        assert!(blocks > 0, "expected block reuse");
        assert!(data < shared.len() / 2, "expected a small literal payload");

        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        assert_eq!(
            fs::read(fixture.beta_root.join("big.bin")).expect("file should be readable"),
            alpha_content
        );
    }

    #[test]
    fn identical_content_elsewhere_is_staged_locally() {
        let mut fixture = Fixture::new();
        write(&fixture.alpha_root, "new/copy.txt", "shared content");
        write(&fixture.alpha_root, "original.txt", "shared content");
        write(&fixture.beta_root, "original.txt", "shared content");

        let transitions = fixture.beta_transitions();
        let requests = transition_dependencies(&transitions);
        assert_eq!(requests.len(), 1);
        let digest = requests[0].digest;
        let needs = fixture
            .beta
            .stage_begin(requests)
            .expect("staging should begin");
        // The content already exists in beta's root, so nothing is needed
        // from alpha at all.
        assert!(needs.is_empty(), "{needs:?}");
        assert!(fixture.beta.staged_path(&digest).exists());

        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        assert_eq!(read(&fixture.beta_root, "new/copy.txt"), "shared content");
    }

    /// A refusal the snapshot predicted is not a disagreement. The parent
    /// directory refuses the write, and the snapshot said nothing wrong
    /// about it — so this must not be recorded as proof that the snapshot
    /// is stale, which is what used to force a full walk of the root on
    /// every cycle for as long as a refusal like this stood.
    #[test]
    fn a_refused_write_is_not_a_disagreement() {
        use std::os::unix::fs::PermissionsExt;
        let mut fixture = Fixture::new();
        write(&fixture.alpha_root, "locked/new.txt", "arriving");
        std::fs::create_dir_all(fixture.beta_root.join("locked")).unwrap();
        let alpha = fixture.alpha.scan().expect("scan should succeed");
        fixture.beta.scan().expect("scan should succeed");
        let expectation = node_at(&alpha, "locked/new.txt");
        let locked = fixture.beta_root.join("locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();

        let outcome = fixture
            .beta
            .transition(vec![Change {
                path: "locked/new.txt".into(),
                old: None,
                new: Some(expectation),
            }])
            .expect("transition should succeed");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(outcome.problems.len(), 1, "{:?}", outcome.problems);
        assert!(
            !outcome.problems[0].disagreement,
            "a permission refusal was recorded as a disagreement: {}",
            outcome.problems[0].message
        );
    }

    #[test]
    fn refuses_to_remove_a_file_modified_since_the_scan() {
        let mut fixture = Fixture::new();
        write(&fixture.beta_root, "keep.txt", "original content");
        let snapshot = fixture.beta.scan().expect("scan should succeed");
        let expectation = node_at(&snapshot, "keep.txt");

        // The file changes after the scan that the transition was reconciled
        // from, which is precisely the race the validation exists for.
        write(&fixture.beta_root, "keep.txt", "content changed underneath");

        let outcome = fixture
            .beta
            .transition(vec![Change {
                path: "keep.txt".into(),
                old: Some(expectation),
                new: None,
            }])
            .expect("transition should succeed");

        assert_eq!(outcome.problems.len(), 1);
        assert_eq!(outcome.problems[0].path, "keep.txt");
        assert!(
            outcome.problems[0]
                .message
                .contains("modified since the last scan"),
            "{}",
            outcome.problems[0].message
        );
        // The disk disagreed with the snapshot: that is what earns a full
        // rescan, and it is recorded as such.
        assert!(outcome.problems[0].disagreement);
        // The content survives, and the result reflects that.
        assert_eq!(
            read(&fixture.beta_root, "keep.txt"),
            "content changed underneath"
        );
        assert!(outcome.results[0].is_some());
    }

    #[test]
    fn refuses_to_remove_a_directory_containing_unexpected_content() {
        let mut fixture = Fixture::new();
        write(&fixture.beta_root, "dir/known.txt", "known");
        let snapshot = fixture.beta.scan().expect("scan should succeed");
        let expectation = node_at(&snapshot, "dir");

        // Content that reconciliation never saw appears after the scan.
        write(&fixture.beta_root, "dir/extra.txt", "created concurrently");

        let outcome = fixture
            .beta
            .transition(vec![Change {
                path: "dir".into(),
                old: Some(expectation),
                new: None,
            }])
            .expect("transition should succeed");

        assert_eq!(outcome.problems.len(), 1);
        assert_eq!(outcome.problems[0].path, "dir/extra.txt");
        assert!(
            outcome.problems[0].message.contains("unexpected content"),
            "{}",
            outcome.problems[0].message
        );
        // The unexpected file and its directory both survive; the expected
        // child is gone, and the result says so.
        assert!(fixture.beta_root.join("dir").is_dir());
        assert_eq!(
            read(&fixture.beta_root, "dir/extra.txt"),
            "created concurrently"
        );
        assert!(!fixture.beta_root.join("dir/known.txt").exists());
        let result = outcome.results[0].as_ref().expect("the directory survives");
        assert!(matches!(result.content, Content::Directory(_)));
        assert!(result.children().is_empty());
    }

    #[test]
    fn executability_only_changes_are_applied_in_place() {
        let mut fixture = Fixture::new();
        write(&fixture.beta_root, "tool.sh", "#!/bin/sh\n");
        let snapshot = fixture.beta.scan().expect("scan should succeed");
        let old = node_at(&snapshot, "tool.sh");
        let before = inode(&fixture.beta_root, "tool.sh");

        let Content::File {
            digest, metadata, ..
        } = old.content
        else {
            panic!("expected a file");
        };
        let new = Node {
            name: "tool.sh".into(),
            content: Content::File {
                digest,
                executable: true,
                metadata,
            },
        };
        let old = Node {
            name: "tool.sh".into(),
            content: Content::File {
                digest,
                executable: false,
                metadata,
            },
        };

        // A pure executability change carries no content dependency at all.
        let transitions = vec![Change {
            path: "tool.sh".into(),
            old: Some(old),
            new: Some(new),
        }];
        assert!(transition_dependencies(&transitions).is_empty());

        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        assert!(executable(&fixture.beta_root, "tool.sh"));
        assert_eq!(read(&fixture.beta_root, "tool.sh"), "#!/bin/sh\n");
        // The inode proves the file was chmod'ed rather than rewritten.
        assert_eq!(inode(&fixture.beta_root, "tool.sh"), before);
    }

    #[test]
    fn file_content_replacement_validates_the_old_content() {
        let mut fixture = Fixture::new();
        write(&fixture.alpha_root, "file.txt", "alpha content");
        write(&fixture.beta_root, "file.txt", "beta content");
        let transitions = fixture.beta_transitions();
        fixture.stage(&transitions);

        // A concurrent modification between staging and transitioning must
        // stop the replacement.
        write(&fixture.beta_root, "file.txt", "beta content, edited again");
        let outcome = fixture
            .beta
            .transition(transitions.clone())
            .expect("transition should succeed");
        assert_eq!(outcome.problems.len(), 1);
        assert!(
            outcome.problems[0].message.contains("refusing to replace"),
            "{}",
            outcome.problems[0].message
        );
        assert_eq!(
            read(&fixture.beta_root, "file.txt"),
            "beta content, edited again"
        );

        // Rescanning legitimizes the current content, after which the same
        // transition (whose expectation now matches) applies.
        let transitions = fixture.beta_transitions();
        fixture.stage(&transitions);
        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        assert_eq!(read(&fixture.beta_root, "file.txt"), "alpha content");
    }

    #[test]
    fn missing_staged_content_is_reported_and_creation_is_partial() {
        let mut fixture = Fixture::new();
        write(&fixture.alpha_root, "dir/present.txt", "present");
        write(&fixture.alpha_root, "dir/absent.txt", "absent");
        let transitions = fixture.beta_transitions();
        let requests = transition_dependencies(&transitions);
        fixture.stage(&transitions);

        // Remove one file's staged content, simulating content that vanished
        // (or was never supplied) between staging and transitioning.
        let absent = requests
            .iter()
            .find(|request| request.path == "dir/absent.txt")
            .expect("the request should exist");
        fs::remove_file(fixture.beta.staged_path(&absent.digest))
            .expect("staged content should be removable");

        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("transition should succeed");
        assert!(outcome.missing_staged_files);
        assert_eq!(outcome.problems.len(), 1);
        assert_eq!(outcome.problems[0].path, "dir/absent.txt");

        // The rest of the directory was still created, and the result
        // describes exactly what landed.
        assert_eq!(read(&fixture.beta_root, "dir/present.txt"), "present");
        assert!(!fixture.beta_root.join("dir/absent.txt").exists());
        let result = outcome.results[0]
            .as_ref()
            .expect("the directory should have been created");
        let names: Vec<&str> = result.children().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["present.txt"]);
    }

    #[test]
    fn refuses_to_create_over_existing_content() {
        let mut fixture = Fixture::new();
        write(&fixture.alpha_root, "file.txt", "alpha content");
        let transitions = fixture.beta_transitions();
        fixture.stage(&transitions);

        // Content appears at the target path after reconciliation decided
        // there was nothing there.
        write(&fixture.beta_root, "file.txt", "appeared concurrently");
        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("transition should succeed");
        assert_eq!(outcome.problems.len(), 1);
        assert!(
            outcome.problems[0]
                .message
                .contains("refusing to create over"),
            "{}",
            outcome.problems[0].message
        );
        assert!(outcome.results[0].is_none());
        assert_eq!(
            read(&fixture.beta_root, "file.txt"),
            "appeared concurrently"
        );
    }

    #[test]
    fn refuses_root_deletion_and_unsafe_paths() {
        let mut fixture = Fixture::new();
        write(&fixture.beta_root, "file.txt", "content");
        let snapshot = fixture.beta.scan().expect("scan should succeed");
        let root = node_at(&snapshot, "");
        let file = node_at(&snapshot, "file.txt");

        let outcome = fixture
            .beta
            .transition(vec![
                Change {
                    path: String::new(),
                    old: Some(root),
                    new: None,
                },
                Change {
                    path: "../escape".into(),
                    old: None,
                    new: Some(file),
                },
            ])
            .expect("transition should succeed");

        assert_eq!(outcome.problems.len(), 2);
        assert!(
            outcome.problems[0]
                .message
                .contains("refusing to remove the synchronization root"),
            "{}",
            outcome.problems[0].message
        );
        assert!(
            outcome.problems[1]
                .message
                .contains("refusing to act on this path"),
            "{}",
            outcome.problems[1].message
        );
        assert_eq!(read(&fixture.beta_root, "file.txt"), "content");
        assert!(outcome.results[0].is_some());
    }

    #[test]
    fn symbolic_links_are_never_followed_when_removing() {
        let mut fixture = Fixture::new();
        // A file outside the root, reachable through a symbolic link inside
        // it. Removing the link must never touch the target.
        let outside = fixture
            .beta_root
            .parent()
            .expect("parent")
            .join("outside.txt");
        fs::write(&outside, "outside content").expect("file should be writable");
        symlink(&outside, fixture.beta_root.join("link")).expect("symlink should be creatable");
        let snapshot = fixture.beta.scan().expect("scan should succeed");
        let expectation = node_at(&snapshot, "link");

        let outcome = fixture
            .beta
            .transition(vec![Change {
                path: "link".into(),
                old: Some(expectation),
                new: None,
            }])
            .expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        assert!(outcome.results[0].is_none());
        assert!(!fixture.beta_root.join("link").exists());
        assert_eq!(
            fs::read_to_string(&outside).expect("the target should survive"),
            "outside content"
        );
    }

    #[test]
    fn a_retargeted_symbolic_link_is_not_removed() {
        let mut fixture = Fixture::new();
        write(&fixture.beta_root, "a.txt", "a");
        write(&fixture.beta_root, "b.txt", "b");
        symlink("a.txt", fixture.beta_root.join("link")).expect("symlink should be creatable");
        let snapshot = fixture.beta.scan().expect("scan should succeed");
        let expectation = node_at(&snapshot, "link");

        fs::remove_file(fixture.beta_root.join("link")).expect("link should be removable");
        symlink("b.txt", fixture.beta_root.join("link")).expect("symlink should be creatable");

        let outcome = fixture
            .beta
            .transition(vec![Change {
                path: "link".into(),
                old: Some(expectation),
                new: None,
            }])
            .expect("transition should succeed");
        assert_eq!(outcome.problems.len(), 1);
        assert!(
            outcome.problems[0].message.contains("retargeted"),
            "{}",
            outcome.problems[0].message
        );
        assert!(fixture.beta_root.join("link").exists());
    }

    #[test]
    fn supply_batches_are_sized_by_bytes_not_just_frames() {
        let mut fixture = Fixture::new();
        // A 20MB file yields far more payload than one batch should carry.
        fs::write(
            fixture.alpha_root.join("big.bin"),
            pseudo_random(20 * 1024 * 1024, 0xBEEF),
        )
        .expect("file should be writable");
        let transitions = fixture.beta_transitions();
        let requests = transition_dependencies(&transitions);
        let needs = fixture
            .beta
            .stage_begin(requests)
            .expect("staging should begin");
        fixture
            .alpha
            .supply_open(needs)
            .expect("supply should open");

        let frames = fixture
            .alpha
            .supply_pull(usize::MAX)
            .expect("supply should pull");
        let bytes: usize = frames
            .iter()
            .map(|frame| match frame {
                TransferFrame::Op(crate::rsync::Op::Data(data)) => data.len(),
                _ => 0,
            })
            .sum();
        // The batch stops near the byte target rather than swallowing the
        // whole file (one frame of overshoot is permitted).
        assert!(
            (SUPPLY_TARGET_BYTES..SUPPLY_TARGET_BYTES + 128 * 1024).contains(&bytes),
            "batch carried {bytes} bytes"
        );
    }

    #[test]
    fn supply_reports_unreadable_files_without_failing_the_stream() {
        let mut fixture = Fixture::new();
        write(&fixture.alpha_root, "gone.txt", "content");
        write(&fixture.alpha_root, "present.txt", "content that stays");
        let transitions = fixture.beta_transitions();
        let requests = transition_dependencies(&transitions);
        let needs = fixture
            .beta
            .stage_begin(requests)
            .expect("staging should begin");
        assert_eq!(needs.len(), 2);

        // The file disappears from the source between staging and supply.
        fs::remove_file(fixture.alpha_root.join("gone.txt")).expect("file should be removable");
        fixture
            .alpha
            .supply_open(needs.clone())
            .expect("supply should open");
        let mut errors = 0;
        loop {
            let frames = fixture.alpha.supply_pull(2).expect("supply should pull");
            if frames.is_empty() {
                break;
            }
            for frame in &frames {
                if let TransferFrame::EndOfFile { error: Some(_) } = frame {
                    errors += 1;
                }
            }
            fixture
                .beta
                .stage_push(frames)
                .expect("staging should accept");
        }
        assert_eq!(errors, 1);

        // The surviving file is staged; the vanished one simply isn't, and
        // the transition reports it as missing rather than failing.
        let gone = needs
            .iter()
            .find(|need| need.request.path == "gone.txt")
            .expect("the need should exist");
        let present = needs
            .iter()
            .find(|need| need.request.path == "present.txt")
            .expect("the need should exist");
        assert!(!fixture.beta.staged_path(&gone.request.digest).exists());
        assert!(fixture.beta.staged_path(&present.request.digest).exists());

        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("transition should succeed");
        assert!(outcome.missing_staged_files);
        assert_eq!(
            read(&fixture.beta_root, "present.txt"),
            "content that stays"
        );
    }

    #[test]
    fn staged_content_survives_an_interrupted_cycle() {
        let mut fixture = Fixture::new();
        write(&fixture.alpha_root, "file.txt", "content");
        let transitions = fixture.beta_transitions();
        fixture.stage(&transitions);

        // A second staging pass (as a fresh cycle would perform) finds the
        // content already staged and asks for nothing.
        let requests = transition_dependencies(&transitions);
        let needs = fixture
            .beta
            .stage_begin(requests)
            .expect("staging should begin");
        assert!(needs.is_empty(), "{needs:?}");
    }

    /// The leak this closes: a blob for a version of a file that changed
    /// while in flight is never referenced again, and used to stay
    /// forever. A stray blob no request names is removed once the cycle's
    /// transition completes; a temporary that is not a digest is not
    /// touched, because it is someone's write in progress.
    #[test]
    fn unreferenced_staged_content_is_swept_after_a_cycle() {
        let mut fixture = Fixture::new();
        let staging = fixture._keep.path().join("staging-beta");
        write(&fixture.alpha_root, "file.txt", "content");
        let transitions = fixture.beta_transitions();
        fixture.stage(&transitions);

        let stray = staging.join("ab".repeat(32));
        fs::write(&stray, b"a previous version nobody will ask for").expect("stray");
        let temporary = staging.join(".autobahn-tmp-in-flight");
        fs::write(&temporary, b"mid-write").expect("temporary");

        fixture
            .beta
            .transition(transitions)
            .expect("transition should succeed");

        assert_eq!(read(&fixture.beta_root, "file.txt"), "content", "published");
        assert!(!stray.exists(), "the unreferenced blob is swept");
        assert!(temporary.exists(), "a non-digest name is left alone");
    }

    /// A blob a request named this cycle survives the sweep even when the
    /// cycle published nothing — the survivor path relies on it being
    /// there next time. Once a cycle passes without naming it, it goes.
    #[test]
    fn content_requested_this_cycle_is_kept_and_forgotten_content_is_not() {
        let mut fixture = Fixture::new();
        let staging = fixture._keep.path().join("staging-beta");
        write(&fixture.alpha_root, "file.txt", "content");
        let transitions = fixture.beta_transitions();
        fixture.stage(&transitions);
        let blob = staging.join(digest_hex(&transition_dependencies(&transitions)[0].digest));
        assert!(blob.exists(), "staged");

        // A fresh cycle asks for the same content, finds it staged, and
        // then transitions nothing: the blob was requested, so it stays.
        let requests = transition_dependencies(&transitions);
        let needs = fixture
            .beta
            .stage_begin(requests)
            .expect("staging should begin");
        assert!(needs.is_empty(), "{needs:?}");
        fixture
            .beta
            .transition(Vec::new())
            .expect("an empty transition");
        assert!(blob.exists(), "requested this cycle, so kept");

        // A cycle that never asks for it is the signal it is dead.
        fixture
            .beta
            .transition(Vec::new())
            .expect("an empty transition");
        assert!(!blob.exists(), "unreferenced for a whole cycle, so swept");
    }

    #[test]
    fn type_changing_replacements_remove_then_create() {
        let mut fixture = Fixture::new();
        write(&fixture.alpha_root, "entry", "now a file");
        write(&fixture.beta_root, "entry/inner.txt", "was a directory");

        let transitions = fixture.beta_transitions();
        assert_eq!(transitions.len(), 1);
        assert!(transitions[0].old.is_some() && transitions[0].new.is_some());
        fixture.stage(&transitions);
        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        assert_eq!(read(&fixture.beta_root, "entry"), "now a file");
    }

    #[test]
    fn creation_modes_default_conservatively_and_are_configurable() {
        // Defaults: 0600 files, 0700 directories, 0700 executables.
        let mut fixture = Fixture::new();
        write(&fixture.alpha_root, "dir/plain.txt", "content");
        write(&fixture.alpha_root, "dir/tool.sh", "#!/bin/sh\n");
        fs::set_permissions(
            fixture.alpha_root.join("dir/tool.sh"),
            Permissions::from_mode(0o755),
        )
        .expect("permissions should be settable");
        let transitions = fixture.beta_transitions();
        fixture.stage(&transitions);
        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        let mode = |path: &str| {
            fs::symlink_metadata(fixture.beta_root.join(path))
                .expect("entry should exist")
                .mode()
                & 0o777
        };
        assert_eq!(mode("dir"), 0o700);
        assert_eq!(mode("dir/plain.txt"), 0o600);
        assert_eq!(mode("dir/tool.sh"), 0o700);

        // Configured modes: 0644/0755, with executability derived (0755).
        let keep = tempdir().expect("temporary directory should be creatable");
        let root = keep.path().join("beta");
        fs::create_dir_all(&root).expect("root should be creatable");
        let mut beta = LocalEndpoint::new(
            root.clone(),
            keep.path().join("staging"),
            EndpointOptions {
                file_mode: Some(0o644),
                directory_mode: Some(0o755),
                ..EndpointOptions::default()
            },
        )
        .expect("endpoint should be creatable");
        beta.scan().expect("scan should succeed");
        let digest = *blake3::hash(b"content").as_bytes();
        fs::create_dir_all(&beta.staging_root).expect("staging should be creatable");
        fs::write(beta.staged_path(&digest), b"content").expect("staged content");
        let outcome = beta
            .transition(vec![Change {
                path: "d".into(),
                old: None,
                new: Some(Node::directory(
                    "d",
                    vec![Node {
                        name: "run.sh".into(),
                        content: Content::File {
                            digest,
                            executable: true,
                            metadata: FileMetadata::default(),
                        },
                    }],
                )),
            }])
            .expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        let mode = |path: &str| {
            fs::symlink_metadata(root.join(path))
                .expect("entry should exist")
                .mode()
                & 0o777
        };
        assert_eq!(mode("d"), 0o755);
        assert_eq!(mode("d/run.sh"), 0o755);
    }

    #[test]
    fn symlink_policy_is_enforced_at_creation() {
        let mut fixture = Fixture::new();
        fixture.beta.symlink_mode = SymlinkMode::Portable;
        fixture.beta.scan().expect("scan should succeed");
        let link = |name: &str, target: &str| Change {
            path: name.into(),
            old: None,
            new: Some(Node {
                name: name.into(),
                content: Content::Symlink {
                    target: target.into(),
                },
            }),
        };
        let outcome = fixture
            .beta
            .transition(vec![link("good", "file.txt"), link("bad", "/etc/passwd")])
            .expect("transition should succeed");
        assert_eq!(outcome.problems.len(), 1, "{:?}", outcome.problems);
        assert!(outcome.problems[0].message.contains("absolute"));
        assert!(fixture.beta_root.join("good").is_symlink());
        assert!(!fixture.beta_root.join("bad").is_symlink());

        // Ignore mode refuses symlink creation outright.
        fixture.beta.symlink_mode = SymlinkMode::Ignore;
        let outcome = fixture
            .beta
            .transition(vec![link("also-good", "file.txt")])
            .expect("transition should succeed");
        assert_eq!(outcome.problems.len(), 1);
        assert!(outcome.problems[0]
            .message
            .contains("ignored by configuration"));
    }

    #[test]
    fn normalization_equivalent_siblings_are_refused_on_folding_volumes() {
        // NFC and NFD spellings denote one entry on a decomposing (or
        // normalization-insensitive) volume; creating both would silently
        // replace the first with the second.
        let mut fixture = Fixture::new();
        fixture.beta.force_behavior(FilesystemBehavior {
            decomposes_unicode: true,
            normalization_insensitive: true,
            ..FilesystemBehavior::default()
        });
        fixture.beta.scan().expect("scan should succeed");

        let digest = *blake3::hash(b"content").as_bytes();
        fs::create_dir_all(&fixture.beta.staging_root).expect("staging root");
        fs::write(fixture.beta.staged_path(&digest), b"content").expect("staged content");
        let child = |name: &str| Node {
            name: name.into(),
            content: Content::File {
                digest,
                executable: false,
                metadata: FileMetadata::default(),
            },
        };
        let outcome = fixture
            .beta
            .transition(vec![Change {
                path: "d".into(),
                old: None,
                new: Some(Node::directory(
                    "d",
                    vec![child("caf\u{00E9}.txt"), child("cafe\u{0301}.txt")],
                )),
            }])
            .expect("transition should succeed");
        assert_eq!(outcome.problems.len(), 1, "{:?}", outcome.problems);
        assert!(
            outcome.problems[0].message.contains("equivalence"),
            "{}",
            outcome.problems[0].message
        );
        // Exactly one spelling landed, and the result reports only it — the
        // ancestor will never carry a phantom sibling.
        let result = outcome.results[0].as_ref().expect("directory result");
        assert_eq!(result.children().len(), 1);
    }

    #[test]
    fn decomposed_on_disk_names_match_nfc_expectations() {
        let mut fixture = Fixture::new();
        // NFD on disk simulates a decomposing volume on our byte-preserving
        // test filesystem.
        write(&fixture.beta_root, "dir/cafe\u{0301}.txt", "content");
        fixture.beta.force_behavior(FilesystemBehavior {
            decomposes_unicode: true,
            ..FilesystemBehavior::default()
        });
        let snapshot = fixture.beta.scan().expect("scan should succeed");
        // The scan records the NFC spelling.
        let expectation = node_at(&snapshot, "dir");
        assert!(expectation.child("caf\u{00E9}.txt").is_some());

        // Removing the directory must match the NFD dirent against the NFC
        // expectation; without recomposition this would refuse with
        // "unexpected content".
        let outcome = fixture
            .beta
            .transition(vec![Change {
                path: "dir".into(),
                old: Some(expectation),
                new: None,
            }])
            .expect("transition should succeed");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        assert!(!fixture.beta_root.join("dir").exists());
    }

    #[test]
    fn case_collisions_are_refused_on_case_insensitive_volumes() {
        let mut fixture = Fixture::new();
        fixture.beta.force_behavior(FilesystemBehavior {
            case_insensitive: true,
            ..FilesystemBehavior::default()
        });
        fixture.beta.scan().expect("scan should succeed");

        let digest = *blake3::hash(b"content").as_bytes();
        let child = |name: &str| Node {
            name: name.into(),
            content: Content::File {
                digest,
                executable: false,
                metadata: FileMetadata::default(),
            },
        };
        // Stage the content so creation can proceed for the survivor.
        fs::create_dir_all(&fixture.beta.staging_root).expect("staging root");
        fs::write(fixture.beta.staged_path(&digest), b"content").expect("staged content");

        let outcome = fixture
            .beta
            .transition(vec![Change {
                path: "d".into(),
                old: None,
                new: Some(Node::directory(
                    "d",
                    vec![child("File.txt"), child("file.txt")],
                )),
            }])
            .expect("transition should succeed");
        assert_eq!(outcome.problems.len(), 1, "{:?}", outcome.problems);
        assert!(
            outcome.problems[0].message.contains("equivalence"),
            "{}",
            outcome.problems[0].message
        );
        // Exactly one of the pair landed, and the result says which.
        let result = outcome.results[0].as_ref().expect("directory result");
        assert_eq!(result.children().len(), 1);
    }

    #[test]
    fn scan_cache_seeds_cold_starts_and_yields_to_metadata_changes() {
        let keep = tempdir().expect("temporary directory should be creatable");
        let root = keep.path().join("root");
        let staging = keep.path().join("staging");
        fs::create_dir_all(&root).expect("root should be creatable");
        write(&root, "file.txt", "content");
        // Backdated so the racy-timestamp rule does not (correctly) refuse
        // reuse of a digest recorded for a just-written file.
        fs::File::options()
            .write(true)
            .open(root.join("file.txt"))
            .expect("file should open")
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(60))
            .expect("mtime should be settable");

        // A first endpoint scans and persists the cache.
        let mut first =
            LocalEndpoint::new(root.clone(), staging.clone(), EndpointOptions::default())
                .expect("endpoint should be creatable");
        let snapshot = first.scan().expect("scan should succeed");
        first.flush_state();
        let cache_path = first.scan_cache_path();
        assert!(cache_path.exists(), "the cache should persist");
        drop(first);

        // Poison the cached digest for the (unchanged) file. A fresh
        // endpoint's first scan must consume the cache as its baseline —
        // proven by the poisoned digest surviving the full-metadata match,
        // exactly as an in-process baseline hint would.
        let mut cached: Snapshot =
            bincode::deserialize(&fs::read(&cache_path).expect("cache should read"))
                .expect("cache should decode");
        let root_node = cached.root.as_mut().expect("root should exist");
        {
            let children = std::sync::Arc::make_mut(match &mut root_node.content {
                Content::Directory(children) => children,
                _ => panic!("expected a directory"),
            });
            match &mut children[0].content {
                Content::File { digest, .. } => *digest = [0xAB; 32],
                _ => panic!("expected a file"),
            }
        }
        fs::write(&cache_path, bincode::serialize(&cached).expect("encode"))
            .expect("cache should be writable");

        let mut second =
            LocalEndpoint::new(root.clone(), staging.clone(), EndpointOptions::default())
                .expect("endpoint should be creatable");
        let reloaded = second.scan().expect("scan should succeed");
        let digest_of = |snapshot: &Snapshot| match &snapshot
            .root
            .as_ref()
            .and_then(|root| root.child("file.txt"))
            .expect("file should exist")
            .content
        {
            Content::File { digest, .. } => *digest,
            _ => panic!("expected a file"),
        };
        assert_eq!(digest_of(&reloaded), [0xAB; 32]);
        drop(second);

        // Change the file (moving its mtime/size): the poisoned hint no
        // longer matches the metadata, so the content is re-read and the
        // digest is honest again.
        write(&root, "file.txt", "changed content");
        let mut third =
            LocalEndpoint::new(root.clone(), staging.clone(), EndpointOptions::default())
                .expect("endpoint should be creatable");
        let rescanned = third.scan().expect("scan should succeed");
        assert_eq!(
            digest_of(&rescanned),
            *blake3::hash(b"changed content").as_bytes()
        );
        drop(third);

        // A corrupt cache is ignored, and the scan still succeeds.
        fs::write(&cache_path, b"garbage").expect("cache should be writable");
        let mut fourth = LocalEndpoint::new(root.clone(), staging, EndpointOptions::default())
            .expect("endpoint should be creatable");
        let recovered = fourth.scan().expect("scan should succeed");
        assert_eq!(
            digest_of(&recovered),
            *blake3::hash(b"changed content").as_bytes()
        );
        assert!(recovered.root.is_some());
        let _ = snapshot;
    }

    /// Content the destination did not ask for — sent before its answer,
    /// and declined — is read to the end and dropped, and what it did ask
    /// for lands beside it.
    #[test]
    fn unrequested_content_is_dropped_and_requested_content_lands() {
        let fixture = Fixture::new();
        fs::write(fixture.alpha_root.join("wanted.txt"), b"wanted").unwrap();
        fs::write(fixture.alpha_root.join("held.txt"), b"held").unwrap();
        // Beta already holds `held`, so it will decline that digest.
        fs::write(fixture.beta_root.join("held.txt"), b"held").unwrap();
        let mut alpha = fixture.alpha;
        let mut beta = fixture.beta;
        alpha.scan().unwrap();
        beta.scan().unwrap();
        let wanted = *blake3::hash(b"wanted").as_bytes();
        let held = *blake3::hash(b"held").as_bytes();
        let request = |path: &str, digest| FileRequest {
            path: path.into(),
            digest,
        };
        let needs = beta
            .stage_begin(vec![
                request("wanted.txt", wanted),
                request("held.txt", held),
            ])
            .unwrap();
        assert_eq!(
            needs.len(),
            1,
            "held content is staged locally, not needed: {needs:?}"
        );
        assert_eq!(needs[0].request.digest, wanted);

        // Supply both anyway, in full, as a sender that did not wait would.
        alpha
            .supply_open(vec![
                StagingNeed {
                    request: request("held.txt", held),
                    signature: Signature::default(),
                },
                StagingNeed {
                    request: request("wanted.txt", wanted),
                    signature: Signature::default(),
                },
            ])
            .unwrap();
        let mut frames = Vec::new();
        loop {
            let batch = alpha.supply_pull(64).unwrap();
            if batch.is_empty() {
                break;
            }
            frames.extend(batch);
        }
        assert!(matches!(frames[0], TransferFrame::Begin { digest } if digest == held));
        beta.stage_push(frames).unwrap();
        beta.stage_finish().unwrap();

        // `wanted` landed from the stream; `held` was never needed and its
        // unrequested copy went nowhere — beta's own is untouched.
        let outcome = beta
            .transition(vec![Change {
                path: "wanted.txt".into(),
                old: None,
                new: alpha
                    .snapshot()
                    .and_then(|s| s.root.as_ref()?.child("wanted.txt").cloned()),
            }])
            .unwrap();
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        assert_eq!(
            fs::read(fixture.beta_root.join("wanted.txt")).unwrap(),
            b"wanted"
        );
        assert_eq!(
            fs::read(fixture.beta_root.join("held.txt")).unwrap(),
            b"held"
        );
    }

    #[test]
    fn await_change_observes_writes() {
        use std::time::{Duration, Instant};

        let mut fixture = Fixture::new();
        // A quiet root waits out the timeout. Watch startup can replay the
        // fixture's own creation on some platforms (FSEvents most of all),
        // and await_change reports change relative to the last *scan* — so
        // settling means scanning to consume whatever dust arrived, then
        // waiting, until one full window passes quietly.
        let mut quiet = false;
        for _ in 0..20 {
            fixture.alpha.scan().expect("the settling scan runs");
            if !fixture
                .alpha
                .await_change(Duration::from_millis(50))
                .expect("await should succeed")
            {
                quiet = true;
                break;
            }
        }
        assert!(quiet, "the root never went quiet");

        // A write arriving mid-wait is observed well before the timeout.
        let root = fixture.alpha_root.clone();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                fs::write(root.join("new.txt"), b"content").expect("write should succeed");
            });
            let start = Instant::now();
            assert!(fixture
                .alpha
                .await_change(Duration::from_secs(10))
                .expect("await should succeed"));
            assert!(start.elapsed() < Duration::from_secs(5));
        });
    }

    #[test]
    fn path_validation_rejects_escapes() {
        assert!(validate_path("").is_ok());
        assert!(validate_path("a/b/c.txt").is_ok());
        assert!(validate_path("..").is_err());
        assert!(validate_path("a/../b").is_err());
        assert!(validate_path("a//b").is_err());
        assert!(validate_path("a/./b").is_err());
    }

    /// Finding I1-A: a sharing session's scan that lands between a
    /// transition's announcement and its writes reads the old bytes at the
    /// announced generation. The transition's completion must outdate that
    /// publication — under the polling fallback, where no kernel event
    /// will ever do it instead.
    #[test]
    fn a_scan_racing_a_transition_cannot_outlive_the_writes() {
        use std::sync::atomic::Ordering;
        let mut fixture = Fixture::new();
        fixture
            .beta
            .observer
            .suppress_watching
            .store(true, Ordering::SeqCst);

        // Converge on the old content first.
        write(&fixture.alpha_root, "file.txt", "the old contents");
        let transitions = fixture.beta_transitions();
        fixture.stage(&transitions);
        fixture
            .beta
            .transition(transitions)
            .expect("the converge transition applies");

        // The change the racing scan will straddle. The write is announced
        // to alpha's observer the way any writer in the tool would: without
        // the announcement, this scan can legitimately serve alpha's
        // published snapshot while the kernel's event is still in flight,
        // the diff comes back empty, and the test races itself instead of
        // the transition.
        write(&fixture.alpha_root, "file.txt", "the new contents!!");
        fixture.alpha.observer.invalidate(["file.txt"]);
        let transitions = fixture.beta_transitions();
        assert!(
            !transitions.is_empty(),
            "the announced change must be visible to reconciliation"
        );
        fixture.stage(&transitions);

        // The sharing session scans inside the announce window: after the
        // paths are invalidated, before any byte is written.
        let observer = std::sync::Arc::clone(&fixture.beta.observer);
        let racing = std::sync::Arc::new(std::sync::Mutex::new(None));
        let stash = std::sync::Arc::clone(&racing);
        fixture.beta.between_announce_and_writes = Some(Box::new(move || {
            *stash.lock().unwrap() = Some(observer.scan(None, None).expect("the racing scan runs"));
        }));
        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("the raced transition applies");
        fixture.beta.between_announce_and_writes = None;
        assert!(
            outcome.problems.is_empty(),
            "the raced transition refused: {:?}",
            outcome.problems
        );
        assert!(
            !outcome.missing_staged_files,
            "the raced transition lost its staged content: {:?}",
            outcome.missing_staged
        );

        let (stale_snapshot, stale_generation) =
            racing.lock().unwrap().take().expect("the seam fired");
        let digest_of = |snapshot: &Snapshot| match &snapshot
            .root
            .as_ref()
            .and_then(|root| root.child("file.txt"))
            .expect("file.txt is recorded")
            .content
        {
            Content::File { digest, .. } => *digest,
            other => panic!("file.txt is {other:?}"),
        };
        // The racing scan legitimately read the old bytes...
        assert_eq!(
            digest_of(&stale_snapshot),
            *blake3::hash(b"the old contents").as_bytes()
        );
        // ...but the completed transition must have outdated its
        // publication: nothing else ever will in the polling fallback.
        assert!(
            stale_generation < fixture.beta.observer.generation(),
            "the stale racing scan still claims the current generation"
        );
        // And a scan now sees what is actually on disk.
        let fresh = fixture.beta.scan().expect("the follow-up scan runs");
        assert_eq!(
            digest_of(&fresh),
            *blake3::hash(b"the new contents!!").as_bytes()
        );
    }

    /// Two endpoints over one root, sharing one observer, as two sessions
    /// synchronizing the same source do. `watching` chooses between a
    /// live watcher and the polling fallback.
    fn sharing_pair(watching: bool) -> (TempDir, PathBuf, LocalEndpoint, LocalEndpoint) {
        use std::sync::atomic::Ordering;
        let keep = tempdir().expect("temporary directory should be creatable");
        let root = keep.path().join("root");
        fs::create_dir_all(&root).expect("root should be creatable");
        let first = endpoint(&root, &keep.path().join("staging-a"));
        let second = endpoint(&root, &keep.path().join("staging-b"));
        assert!(
            Arc::ptr_eq(&first.observer, &second.observer),
            "two endpoints over one root must share one observer"
        );
        first
            .observer
            .suppress_watching
            .store(!watching, Ordering::SeqCst);
        (keep, root, first, second)
    }

    /// The deletion of `path` as recorded in `endpoint`'s own lease.
    fn deletion(endpoint: &LocalEndpoint, path: &str) -> Change {
        let lease = endpoint.last_snapshot.as_ref().expect("a lease is held");
        Change {
            path: path.to_string(),
            old: Some(node_at(lease, path)),
            new: None,
        }
    }

    fn records(snapshot: &Snapshot, path: &str) -> bool {
        let mut current = snapshot.root.as_ref();
        for component in path.split('/') {
            current = current.and_then(|node| node.child(component));
        }
        current.is_some()
    }

    /// Scans until the observer is quiet: every event the kernel had in
    /// flight has been consumed by a scan, so nothing still pending can
    /// rescue a baseline that later forgets a change.
    fn settle(endpoint: &mut LocalEndpoint) {
        endpoint.scan().expect("the settling scan runs");
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if endpoint.observer.generation() == endpoint.seen_generation {
                return;
            }
            endpoint.scan().expect("the settling scan runs");
        }
        panic!("the root never went quiet");
    }

    /// Finding H-3: a transition offered its fold labelled with the
    /// observer's *current* generation, although the fold was built from
    /// its older lease. A sharing session that had scanned since — and
    /// consumed the dirty marks of its own deletion — then had that
    /// deletion rolled back out of the baseline, and its next scan
    /// reported the deleted path as present.
    fn a_stale_lease_cannot_roll_back_a_sharing_sessions_deletion(watching: bool) {
        let (_keep, root, mut first, mut second) = sharing_pair(watching);
        write(&root, "left/x", "x");
        write(&root, "right/y", "y");

        // Both sessions see both paths.
        first.scan().expect("the first session scans");
        settle(&mut second);
        let lease = first.last_snapshot.clone().expect("a lease is held");
        assert!(records(&lease, "left/x") && records(&lease, "right/y"));

        // The second session deletes right/y and refreshes the shared scan,
        // consuming every mark its deletion left.
        let removal = deletion(&second, "right/y");
        let outcome = second.transition(vec![removal]).expect("deletes");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        settle(&mut second);
        assert!(!records(
            second.last_snapshot.as_ref().expect("scanned"),
            "right/y"
        ));

        // The first session deletes the unrelated left/x from its older
        // lease — which still records right/y.
        let removal = deletion(&first, "left/x");
        let outcome = first.transition(vec![removal]).expect("deletes");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);

        let rescanned = second.scan().expect("the second session scans");
        assert!(
            !records(&rescanned, "right/y"),
            "a deleted path came back from a stale lease's fold"
        );
        assert!(!records(&rescanned, "left/x"));
        assert!(!root.join("right/y").exists() && !root.join("left/x").exists());
    }

    #[test]
    fn a_stale_lease_cannot_roll_back_a_sharing_sessions_deletion_when_watched() {
        a_stale_lease_cannot_roll_back_a_sharing_sessions_deletion(true);
    }

    #[test]
    fn a_stale_lease_cannot_roll_back_a_sharing_sessions_deletion_when_polled() {
        a_stale_lease_cannot_roll_back_a_sharing_sessions_deletion(false);
    }

    /// Finding H-3, second half: a change a sharing session announced
    /// between this session's lease scan and its transition must still
    /// wake this session — not be folded into the generation it has seen
    /// along with its own writes. Polled, so no kernel event arrives to
    /// wake it by accident.
    #[test]
    fn a_foreign_change_during_a_transition_still_wakes_the_session() {
        let (_keep, root, mut first, mut second) = sharing_pair(false);
        write(&root, "left/x", "x");
        write(&root, "right/y", "y");
        first.scan().expect("the first session scans");
        second.scan().expect("the second session scans");

        // The foreign change, after the first session's lease scan.
        let removal = deletion(&second, "right/y");
        second.transition(vec![removal]).expect("deletes");

        let removal = deletion(&first, "left/x");
        first.transition(vec![removal]).expect("deletes");
        assert_eq!(
            first.watch_poll().expect("polls"),
            Some(true),
            "a change this session never scanned was folded into what it has seen"
        );

        // With nothing foreign in between, a session's own writes leave
        // it caught up: the wait is for the next change.
        write(&root, "left/z", "z");
        first.observer.invalidate(["left/z"]);
        let lease = first.scan().expect("the first session scans");
        let removal = Change {
            path: "left/z".to_string(),
            old: Some(node_at(&lease, "left/z")),
            new: None,
        };
        first.transition(vec![removal]).expect("deletes");
        assert_eq!(first.watch_poll().expect("polls"), Some(false));
    }

    /// The same, for a real external write under a live watcher.
    #[test]
    fn an_external_write_during_a_transition_still_wakes_the_session() {
        let (_keep, root, mut first, _second) = sharing_pair(true);
        write(&root, "left/x", "x");
        settle(&mut first);

        write(&root, "elsewhere.txt", "external");
        assert!(
            first
                .observer
                .await_change(first.seen_generation, std::time::Duration::from_secs(10)),
            "the external write was never observed"
        );
        let removal = deletion(&first, "left/x");
        first.transition(vec![removal]).expect("deletes");
        assert_eq!(first.watch_poll().expect("polls"), Some(true));
    }

    /// Finding I3-A: the last-use publish path renames staged content
    /// straight into the tree. Content tampered with after its receive
    /// verification must not ride that rename under the original digest —
    /// the record would then suppress every later scan's detection and
    /// propagate the tampered bytes as verified.
    #[test]
    fn tampered_staged_content_is_never_published_by_the_move_path() {
        let mut fixture = Fixture::new();
        let genuine: Vec<u8> = (0..96 * 1024u32).map(|i| (i % 249) as u8).collect();
        let tampered: Vec<u8> = (0..96 * 1024u32).map(|i| (i % 247) as u8).collect();
        let root = fixture.alpha_root.join("payload.bin");
        fs::write(&root, &genuine).expect("alpha content is writable");

        let transitions = fixture.beta_transitions();
        fixture.stage(&transitions);
        // The tamper: same length, wrong bytes, at the digest-named path.
        let digest = *blake3::hash(&genuine).as_bytes();
        let staged = staged_path(&fixture.beta.staging_root, &digest);
        fs::write(&staged, &tampered).expect("the staged file is writable");

        let outcome = fixture
            .beta
            .transition(transitions)
            .expect("the transition itself runs");
        let landed = fs::read(fixture.beta_root.join("payload.bin")).ok();
        assert_ne!(
            landed.as_deref(),
            Some(tampered.as_slice()),
            "tampered bytes were published under the genuine digest"
        );
        assert!(
            outcome.missing_staged_files,
            "the tamper must schedule a retransfer"
        );

        // The retransfer converges on the genuine bytes.
        let transitions = fixture.beta_transitions();
        fixture.stage(&transitions);
        fixture
            .beta
            .transition(transitions)
            .expect("the follow-up transition runs");
        assert_eq!(
            fs::read(fixture.beta_root.join("payload.bin"))
                .ok()
                .as_deref(),
            Some(genuine.as_slice())
        );
    }

    /// The symlink variant of the same finding: a staged entry swapped for
    /// a symlink — even one pointing at content with the right bytes —
    /// must never be renamed into the tree as if it were the verified
    /// regular file.
    #[test]
    fn a_staged_symlink_is_never_published_by_the_move_path() {
        let mut fixture = Fixture::new();
        let genuine: Vec<u8> = (0..64 * 1024u32).map(|i| (i % 233) as u8).collect();
        fs::write(fixture.alpha_root.join("payload.bin"), &genuine)
            .expect("alpha content is writable");

        let transitions = fixture.beta_transitions();
        fixture.stage(&transitions);
        let digest = *blake3::hash(&genuine).as_bytes();
        let staged = staged_path(&fixture.beta.staging_root, &digest);
        let decoy = fixture.beta.staging_root.join("decoy");
        fs::write(&decoy, &genuine).expect("the decoy is writable");
        fs::remove_file(&staged).expect("the staged file is removable");
        std::os::unix::fs::symlink(&decoy, &staged).expect("the swap succeeds");

        let _ = fixture
            .beta
            .transition(transitions)
            .expect("the transition itself runs");
        let target = fixture.beta_root.join("payload.bin");
        if let Ok(metadata) = fs::symlink_metadata(&target) {
            // Not landing at all is safe; landing as anything but the
            // verified regular file is not.
            assert!(
                metadata.file_type().is_file(),
                "a symlink was published as a verified regular file"
            );
        }
    }

    /// A refused creation names the entry that is in the way.
    ///
    /// Found on a real configuration: a Linux destination held one PDF
    /// under two spellings — one with a combining acute, one precomposed
    /// — while the Mac held only the decomposed one. APFS files both under
    /// a single entry, so creating the second was refused, correctly. The
    /// message said only "refusing to create over existing content", which
    /// sent the reader looking for a file that appears not to be there.
    #[test]
    fn a_refused_creation_names_the_entry_it_cannot_be_told_apart_from() {
        let directory = tempfile::tempdir().expect("temporary directory");
        // The same name twice: decomposed, then precomposed.
        let decomposed = "Jorge Sua\u{301}rez resume.pdf";
        let precomposed = "Jorge Su\u{e1}rez resume.pdf";
        assert_ne!(decomposed, precomposed, "the two spellings differ in bytes");
        fs::write(directory.path().join(decomposed), b"same").expect("writes");

        assert_eq!(
            folded_twin(directory.path(), precomposed),
            Some((decomposed.to_owned(), "unicode collision")),
            "the entry already there is named, and so is the rule that folded them"
        );

        // Nothing to report for a name that is genuinely absent, or for
        // one spelled exactly as it is stored.
        assert_eq!(folded_twin(directory.path(), "unrelated.txt"), None);
        assert_eq!(folded_twin(directory.path(), decomposed), None);

        // Case folds the same way. The probed flags are not consulted at
        // all: the observer can be holding a default that claims names
        // never fold, and this decision rests on what the directory holds.
        fs::write(directory.path().join("Report.md"), b"x").expect("writes");
        assert_eq!(
            folded_twin(directory.path(), "REPORT.MD"),
            Some(("Report.md".to_owned(), "casing collision"))
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod watch_tests {
    use super::ChangeWatcher;
    use crate::scan::IgnoreSet;
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn recorded_within(watcher: &ChangeWatcher, wanted: &Path, deadline: Duration) -> bool {
        let end = Instant::now() + deadline;
        while Instant::now() < end {
            if watcher.recorded().iter().any(|path| path == wanted) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// The point of building the watch here: an ignored directory has no
    /// watch, so a write beneath it is never even seen — it costs no kernel
    /// watch and no entry in the change record.
    #[test]
    fn an_ignored_directory_is_not_watched() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join("kept")).unwrap();
        std::fs::create_dir_all(root.path().join("node_modules/pkg")).unwrap();
        let ignores = IgnoreSet::new(&["node_modules".to_string()]).expect("ignores");
        let watcher = ChangeWatcher::new(root.path(), ignores, true, || {}).expect("watch");

        std::fs::write(root.path().join("kept/a"), b"a").unwrap();
        std::fs::write(root.path().join("node_modules/pkg/b"), b"b").unwrap();
        assert!(recorded_within(
            &watcher,
            &root.path().join("kept/a"),
            Duration::from_secs(3)
        ));
        std::thread::sleep(Duration::from_millis(300));
        let ignored = root.path().join("node_modules");
        let recorded = watcher.recorded();
        assert!(
            recorded.iter().all(|path| !path.starts_with(&ignored)),
            "a write beneath an ignored directory was recorded: {recorded:?}"
        );
    }

    /// A directory that appears after the watch was built is watched
    /// before its own event is recorded, so what is then written beneath
    /// it is seen — unless it is ignored, in which case it is left alone
    /// exactly like one that was there from the start.
    #[test]
    fn a_directory_that_appears_later_is_watched_unless_ignored() {
        let root = tempfile::tempdir().expect("tempdir");
        let ignores = IgnoreSet::new(&["target".to_string()]).expect("ignores");
        let watcher = ChangeWatcher::new(root.path(), ignores, true, || {}).expect("watch");

        std::fs::create_dir(root.path().join("fresh")).unwrap();
        assert!(recorded_within(
            &watcher,
            &root.path().join("fresh"),
            Duration::from_secs(3)
        ));
        std::fs::write(root.path().join("fresh/f"), b"f").unwrap();
        assert!(
            recorded_within(
                &watcher,
                &root.path().join("fresh/f"),
                Duration::from_secs(3)
            ),
            "a file beneath a directory that appeared later was not seen"
        );

        std::fs::create_dir(root.path().join("target")).unwrap();
        assert!(recorded_within(
            &watcher,
            &root.path().join("target"),
            Duration::from_secs(3)
        ));
        std::fs::write(root.path().join("target/out"), b"o").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !watcher
                .recorded()
                .iter()
                .any(|path| path == &root.path().join("target/out")),
            "a write beneath an ignored directory that appeared later was recorded"
        );
    }
}

/// The apply path's requests and publishing: what a peer can name, and
/// what autobahn writes where.
#[cfg(test)]
mod apply_path_tests {
    use super::*;

    use std::sync::atomic::AtomicBool;
    use tempfile::{tempdir, TempDir};

    /// A root with a symbolic link inside it, `root/link`, pointing at a
    /// directory outside it that holds `secret`.
    struct Escape {
        _keep: TempDir,
        root: PathBuf,
        outside: PathBuf,
        endpoint: LocalEndpoint,
    }

    fn escape() -> Escape {
        let keep = tempdir().expect("temporary directory");
        let root = keep.path().join("root");
        let outside = keep.path().join("outside");
        fs::create_dir_all(&root).expect("root");
        fs::create_dir_all(&outside).expect("outside");
        fs::write(outside.join("secret"), b"secret").expect("secret");
        symlink(&outside, root.join("link")).expect("link");
        let endpoint = LocalEndpoint::new(
            root.clone(),
            keep.path().join("staging"),
            EndpointOptions::default(),
        )
        .expect("endpoint");
        Escape {
            _keep: keep,
            root,
            outside,
            endpoint,
        }
    }

    #[test]
    fn read_file_refuses_a_symlinked_parent() {
        let mut fixture = escape();
        let result = fixture.endpoint.read_file("link/secret");
        let error = result.expect_err("reading through a symlinked parent must be refused");
        assert!(
            format!("{error:#}").contains("not a directory"),
            "{error:#}"
        );
    }

    #[test]
    fn read_file_of_a_final_symlink_is_none() {
        let mut fixture = escape();
        symlink(
            fixture.outside.join("secret"),
            fixture.root.join("to-secret"),
        )
        .expect("link");
        assert_eq!(fixture.endpoint.read_file("to-secret").expect("read"), None);
        assert_eq!(fixture.endpoint.read_file("link").expect("read"), None);
    }

    #[test]
    fn read_file_of_a_fifo_is_none_without_hanging() {
        let Escape {
            _keep,
            root,
            mut endpoint,
            ..
        } = escape();
        let fifo = root.join("fifo");
        let path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(endpoint.read_file("fifo").map_err(|e| e.to_string()));
        });
        let result = receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("reading a FIFO must not hang");
        assert_eq!(result, Ok(None));
    }

    #[test]
    fn read_file_reads_a_regular_file() {
        let mut fixture = escape();
        fs::create_dir(fixture.root.join("sub")).expect("sub");
        fs::write(fixture.root.join("sub/a.txt"), b"hello").expect("a");
        assert_eq!(
            fixture.endpoint.read_file("sub/a.txt").expect("read"),
            Some(b"hello".to_vec())
        );
        assert_eq!(
            fixture.endpoint.read_file("sub/missing").expect("read"),
            None
        );
    }

    #[test]
    fn rename_out_through_a_symlinked_parent_is_refused() {
        let mut fixture = escape();
        fs::write(fixture.root.join("a.txt"), b"a").expect("a");
        fixture
            .endpoint
            .rename("a.txt", "link/a.txt")
            .expect_err("moving out through a symlinked parent must be refused");
        assert!(!fixture.outside.join("a.txt").exists());
        assert!(fixture.root.join("a.txt").exists());
        fixture
            .endpoint
            .rename("a.txt", "link/deeper/a.txt")
            .expect_err("creating parents through a symlink must be refused");
        assert!(!fixture.outside.join("deeper").exists());
    }

    #[test]
    fn rename_in_from_a_symlinked_parent_is_refused() {
        let mut fixture = escape();
        fixture
            .endpoint
            .rename("link/secret", "stolen")
            .expect_err("moving in from a symlinked parent must be refused");
        assert!(fixture.outside.join("secret").exists());
        assert!(!fixture.root.join("stolen").exists());
    }

    #[test]
    fn rename_creates_missing_real_parents() {
        let mut fixture = escape();
        fs::write(fixture.root.join("a.txt"), b"a").expect("a");
        fixture
            .endpoint
            .rename("a.txt", "new/dir/a.txt")
            .expect("the move should succeed");
        for directory in ["new", "new/dir"] {
            let metadata = fs::symlink_metadata(fixture.root.join(directory)).expect("parent");
            assert!(metadata.file_type().is_dir(), "{directory}");
            assert_eq!(
                metadata.mode() & 0o777,
                DEFAULT_DIRECTORY_MODE,
                "{directory}"
            );
        }
        assert_eq!(fs::read(fixture.root.join("new/dir/a.txt")).unwrap(), b"a");
    }

    /// A name taken between the absence check and the move keeps its
    /// occupant: the move is a no-replace rename, not a plain one.
    #[test]
    fn rename_does_not_overwrite_a_target_that_appears_after_the_check() {
        let mut fixture = escape();
        fs::write(fixture.root.join("a.txt"), b"moved").expect("a");
        let target = fixture.root.join("b.txt");
        let fired = Arc::new(AtomicBool::new(false));
        let hook_fired = Arc::clone(&fired);
        fixture.endpoint.between_announce_and_writes = Some(Box::new(move || {
            fs::write(&target, b"occupant").expect("occupant");
            hook_fired.store(true, Ordering::SeqCst);
        }));
        let error = fixture
            .endpoint
            .rename("a.txt", "b.txt")
            .expect_err("the occupant must not be replaced");
        assert!(fired.load(Ordering::SeqCst));
        assert!(format!("{error:#}").contains("already exists"), "{error:#}");
        assert_eq!(fs::read(fixture.root.join("b.txt")).unwrap(), b"occupant");
        assert_eq!(fs::read(fixture.root.join("a.txt")).unwrap(), b"moved");
    }

    fn file_node(name: &str) -> Node {
        Node {
            name: name.to_owned(),
            content: Content::File {
                digest: [9; 32],
                executable: false,
                metadata: FileMetadata::default(),
            },
        }
    }

    fn problem_paths(outcome: &TransitionOutcome) -> Vec<&str> {
        outcome
            .problems
            .iter()
            .map(|problem| problem.path.as_str())
            .collect()
    }

    /// What scanning hides or refuses to synchronize, a peer cannot
    /// create: neither at a transition's own path nor as a child of a
    /// directory it creates.
    #[test]
    fn a_transition_creating_a_reserved_name_is_refused() {
        let fixture = escape();
        let mut endpoint = fixture.endpoint;
        endpoint.scan().expect("scan");
        let changes = vec![
            Change {
                path: ".autobahn-tmp-staging-s-beta".into(),
                old: None,
                new: Some(Node::directory(".autobahn-tmp-staging-s-beta", Vec::new())),
            },
            Change {
                path: ".autobahn-tmp-x".into(),
                old: None,
                new: Some(file_node(".autobahn-tmp-x")),
            },
            Change {
                path: "d".into(),
                old: None,
                new: Some(Node::directory(
                    "d",
                    vec![file_node(".autobahn-tmp-recv-1-1")],
                )),
            },
        ];
        let outcome = endpoint.transition(changes).expect("transition");
        let problems = problem_paths(&outcome);
        for path in [
            ".autobahn-tmp-staging-s-beta",
            ".autobahn-tmp-x",
            "d/.autobahn-tmp-recv-1-1",
        ] {
            assert!(problems.contains(&path), "{path}: {:?}", outcome.problems);
            assert!(
                fs::symlink_metadata(fixture.root.join(path)).is_err(),
                "{path} was created"
            );
        }
        assert!(outcome
            .problems
            .iter()
            .all(|problem| problem.message.contains("reserved")));
    }

    fn endpoint_staging_in(root: &Path, staging: PathBuf) -> LocalEndpoint {
        LocalEndpoint::new(root.to_path_buf(), staging, EndpointOptions::default())
            .expect("endpoint")
    }

    /// An inside-root staging directory that is a symbolic link out of the
    /// root is refused, and nothing is staged at its target.
    #[test]
    fn a_symlinked_inside_root_staging_directory_is_refused() {
        use crate::endpoint::StagingMode;
        let fixture = escape();
        let staging = staging_root_for(
            StagingMode::InsideRoot,
            &fixture.root,
            PathBuf::new(),
            "s1",
            "beta",
        )
        .expect("staging root");
        let target = fixture.outside.join("planted");
        fs::create_dir(&target).expect("planted");
        symlink(&target, &staging).expect("link");
        let mut endpoint = endpoint_staging_in(&fixture.root, staging);
        let error = endpoint
            .stage_begin(vec![FileRequest {
                path: "a.txt".into(),
                digest: [1; 32],
            }])
            .expect_err("staging through a symbolic link must be refused");
        assert!(format!("{error:#}").contains("symbolic link"), "{error:#}");
        assert_eq!(fs::read_dir(&target).expect("target").count(), 0);
    }

    /// Every placement's staging directory is created owner-only.
    #[test]
    fn new_staging_directories_are_private_in_every_placement() {
        use crate::endpoint::StagingMode;
        let keep = tempdir().expect("temporary directory");
        let state = keep.path().join("state/staging/s1-beta");
        for mode in [
            StagingMode::State,
            StagingMode::BesideRoot,
            StagingMode::InsideRoot,
        ] {
            let root = keep.path().join(format!("{mode:?}")).join("root");
            fs::create_dir_all(&root).expect("root");
            let staging =
                staging_root_for(mode, &root, state.clone(), "s1", "beta").expect("staging root");
            endpoint_staging_in(&root, staging.clone())
                .stage_begin(Vec::new())
                .expect("staging begins");
            let metadata = fs::symlink_metadata(&staging).expect("staging exists");
            assert!(metadata.is_dir(), "{mode:?}");
            assert_eq!(metadata.mode() & 0o777, 0o700, "{mode:?}");
        }
        // The state area's own staging parent is created owner-only too.
        let parent = fs::metadata(keep.path().join("state/staging")).expect("parent");
        assert_eq!(parent.mode() & 0o777, 0o700);
    }

    /// A staging directory an older version left loose is tightened, not
    /// refused.
    #[test]
    fn a_loose_staging_directory_is_tightened() {
        let keep = tempdir().expect("temporary directory");
        let root = keep.path().join("root");
        let staging = keep.path().join("staging");
        fs::create_dir_all(&root).expect("root");
        fs::create_dir(&staging).expect("staging");
        fs::set_permissions(&staging, Permissions::from_mode(0o755)).expect("loosen");
        endpoint_staging_in(&root, staging.clone())
            .stage_begin(Vec::new())
            .expect("staging begins");
        let mode = fs::metadata(&staging).expect("staging").mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    /// Another user's staging directory is refused. Only root can make
    /// one, so the test runs only as root.
    #[test]
    fn a_staging_directory_owned_by_another_user_is_refused() {
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let keep = tempdir().expect("temporary directory");
        let root = keep.path().join("root");
        let staging = keep.path().join("staging");
        fs::create_dir_all(&root).expect("root");
        fs::create_dir(&staging).expect("staging");
        std::os::unix::fs::chown(&staging, Some(65534), Some(65534)).expect("chown");
        let error = endpoint_staging_in(&root, staging)
            .stage_begin(Vec::new())
            .expect_err("another user's staging directory must be refused");
        assert!(format!("{error:#}").contains("owned by"), "{error:#}");
    }

    /// Temporary names carry an unpredictable token, and still read as
    /// autobahn's own to the scanner.
    #[test]
    fn temporary_names_are_unpredictable_and_hidden_from_scans() {
        let first = temporary_name("apply");
        let second = temporary_name("apply");
        assert!(scan::autobahn_temporary(&first), "{first}");
        let token = |name: &str| name.rsplit('-').next().unwrap().to_owned();
        assert_eq!(token(&first).len(), 16);
        assert_ne!(token(&first), token(&second));
    }

    /// A copy-publish never writes through something planted at its
    /// temporary's name, and what it creates is owner-only.
    #[test]
    fn a_publish_copy_is_private_and_never_follows_a_planted_link() {
        let keep = tempdir().expect("temporary directory");
        let source = keep.path().join("source");
        fs::write(&source, b"content").expect("source");
        let digest = *blake3::hash(b"content").as_bytes();

        let victim = keep.path().join("victim");
        fs::write(&victim, b"untouched").expect("victim");
        let planted = keep.path().join("planted");
        symlink(&victim, &planted).expect("planted");
        let mut input = File::open(&source).expect("open");
        copy_into_private(&mut input, &source, &planted, &digest)
            .expect_err("a planted link must be refused");
        assert_eq!(fs::read(&victim).unwrap(), b"untouched");

        let temporary = keep.path().join("temporary");
        let mut input = File::open(&source).expect("open");
        assert!(copy_into_private(&mut input, &source, &temporary, &digest).expect("copy"));
        assert_eq!(
            fs::metadata(&temporary).expect("temporary").mode() & 0o777,
            0o600
        );
    }

    /// Deletes `path` on an endpoint just scanned, with the synchronizable
    /// part of what the scan saw there as the expectation, as a
    /// reconciled deletion carries.
    fn delete_scanned(endpoint: &mut LocalEndpoint, path: &str) -> TransitionOutcome {
        let snapshot = endpoint.scan().expect("scan");
        let mut node = snapshot.root.as_ref().expect("root");
        for component in path.split('/') {
            node = node.child(component).expect("scanned");
        }
        let old = node.synchronizable_subtree();
        endpoint
            .transition(vec![Change {
                path: path.into(),
                old,
                new: None,
            }])
            .expect("transition")
    }

    fn endpoint_with(root: &Path, options: EndpointOptions) -> LocalEndpoint {
        LocalEndpoint::new(root.to_path_buf(), root.with_extension("staging"), options)
            .expect("endpoint")
    }

    /// A file over the size limit exists only on this side: deleting its
    /// directory removes the rest, and leaves it, and the directory, with
    /// a problem saying why.
    #[test]
    fn a_deleted_directory_leaves_an_oversized_file_in_place() {
        let keep = tempdir().expect("temporary directory");
        let root = keep.path().join("root");
        fs::create_dir_all(root.join("data")).expect("data");
        fs::write(root.join("data/small.txt"), b"small").expect("small");
        fs::write(root.join("data/dump.sql"), vec![7u8; 4096]).expect("dump");
        let mut endpoint = endpoint_with(
            &root,
            EndpointOptions {
                max_file_size: Some(1024),
                ..EndpointOptions::default()
            },
        );
        let outcome = delete_scanned(&mut endpoint, "data");
        assert_eq!(fs::read(root.join("data/dump.sql")).unwrap().len(), 4096);
        assert!(!root.join("data/small.txt").exists());
        let problem = outcome
            .problems
            .iter()
            .find(|problem| problem.path == "data/dump.sql")
            .expect("the survivor is reported");
        assert!(problem.message.contains("excluded by size"), "{problem:?}");
        assert!(!problem.disagreement);
    }

    /// A FIFO is excluded for what it is, not by a pattern, so it stays.
    #[test]
    fn a_deleted_directory_leaves_a_fifo_in_place() {
        let keep = tempdir().expect("temporary directory");
        let root = keep.path().join("root");
        fs::create_dir_all(root.join("d")).expect("d");
        fs::write(root.join("d/a.txt"), b"a").expect("a");
        let fifo = root.join("d/fifo");
        let path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let mut endpoint = endpoint_with(&root, EndpointOptions::default());
        let outcome = delete_scanned(&mut endpoint, "d");
        assert!(fs::symlink_metadata(&fifo).is_ok(), "the FIFO was removed");
        assert!(!root.join("d/a.txt").exists());
        assert!(outcome
            .problems
            .iter()
            .any(|problem| problem.path == "d/fifo" && problem.message.contains("by type")));
    }

    /// What a pattern ignores still goes with its directory, at any depth.
    #[test]
    fn a_deleted_directory_still_takes_its_ignored_content() {
        let keep = tempdir().expect("temporary directory");
        let root = keep.path().join("root");
        fs::create_dir_all(root.join("project/.git/objects")).expect("git");
        fs::write(root.join("project/.git/objects/x"), b"x").expect("object");
        fs::write(root.join("project/build.log"), b"log").expect("log");
        fs::write(root.join("project/main.rs"), b"fn main() {}").expect("main");
        let mut endpoint = endpoint_with(
            &root,
            EndpointOptions {
                ignores: IgnoreSet::new(&[".git".to_string(), "*.log".to_string()])
                    .expect("ignores"),
                ..EndpointOptions::default()
            },
        );
        let outcome = delete_scanned(&mut endpoint, "project");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        assert!(!root.join("project").exists());
    }

    /// Two publishes of one digest: the first counts its use, and before
    /// it acts the second takes the last use and moves the staged file
    /// into place. The first still publishes, from the file it opened,
    /// rather than finding nothing and asking for the content again.
    #[test]
    fn an_earlier_publish_survives_the_last_use_moving_the_staged_file() {
        let keep = tempdir().expect("temporary directory");
        let root = keep.path().join("root");
        let staging = keep.path().join("staging");
        fs::create_dir_all(&root).expect("root");
        fs::create_dir_all(&staging).expect("staging");
        let digest = *blake3::hash(b"shared").as_bytes();
        let staged = staged_path(&staging, &digest);
        fs::write(&staged, b"shared").expect("staged");

        let uses: HashMap<Digest, AtomicUsize> = [(digest, AtomicUsize::new(2))].into();
        let helpers = AtomicUsize::new(0);
        let ignores = IgnoreSet::default();
        let moved_to = root.join("b");
        let hook = || {
            // The other publish, taking the last use: a move into place.
            fs::rename(&staged, &moved_to).expect("the last use moves the file");
        };
        let mut transitioner = Transitioner {
            root: &root,
            staging_root: &staging,
            scanned: None,
            behavior: FilesystemBehavior::default(),
            symlink_mode: SymlinkMode::default(),
            ignores: &ignores,
            file_mode: DEFAULT_FILE_MODE,
            directory_mode: DEFAULT_DIRECTORY_MODE,
            owner: None,
            group: None,
            staged_uses: &uses,
            helpers: &helpers,
            after_use_counted: Some(&hook),
            swept: &Mutex::new(HashSet::new()),
            problems: Vec::new(),
            missing_staged_files: false,
            missing_staged: Vec::new(),
        };
        let published =
            transitioner.publish_file("a", &root, &root.join("a"), &digest, false, false);
        assert!(
            transitioner.problems.is_empty(),
            "{:?}",
            transitioner.problems
        );
        assert!(!transitioner.missing_staged_files);
        assert!(published.is_some());
        assert_eq!(fs::read(root.join("a")).unwrap(), b"shared");
        assert_eq!(fs::read(root.join("b")).unwrap(), b"shared");
    }

    /// Writes an `.autobahn-tmp-apply-*` leftover into `directory`, by a
    /// process identifier no process has, modified `age` ago.
    fn leftover(directory: &Path, count: u32, age: std::time::Duration) -> PathBuf {
        let path = directory.join(format!("{TEMPORARY_PREFIX}-apply-{}-{count}", i32::MAX));
        let file = File::create(&path).expect("leftover");
        file.set_modified(std::time::SystemTime::now() - age)
            .expect("backdate");
        path
    }

    /// A crashed publish's temporary inside a deleted directory goes with
    /// it: no disagreement, which would force a full walk every cycle.
    #[test]
    fn a_deleted_directory_takes_a_leftover_temporary_with_it() {
        let keep = tempdir().expect("temporary directory");
        let root = keep.path().join("root");
        fs::create_dir_all(root.join("d")).expect("d");
        fs::write(root.join("d/a.txt"), b"a").expect("a");
        leftover(&root.join("d"), 1, std::time::Duration::ZERO);
        let mut endpoint = endpoint_with(&root, EndpointOptions::default());
        let outcome = delete_scanned(&mut endpoint, "d");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        assert!(!root.join("d").exists());
    }

    /// Publishing into a directory sweeps a stale leftover from it, and
    /// leaves one a copy may still be writing.
    #[test]
    fn publishing_into_a_directory_sweeps_its_stale_leftovers() {
        let keep = tempdir().expect("temporary directory");
        let root = keep.path().join("root");
        fs::create_dir_all(root.join("d")).expect("d");
        let stale = leftover(&root.join("d"), 1, std::time::Duration::from_secs(3600));
        let fresh = leftover(&root.join("d"), 2, std::time::Duration::ZERO);
        let mut endpoint = endpoint_with(&root, EndpointOptions::default());
        let digest = *blake3::hash(b"new").as_bytes();
        prepare_staging_root(&endpoint.staging_root, &root).expect("staging");
        fs::write(staged_path(&endpoint.staging_root, &digest), b"new").expect("staged");
        endpoint.scan().expect("scan");
        let outcome = endpoint
            .transition(vec![Change {
                path: "d/new".into(),
                old: None,
                new: Some(Node {
                    name: "new".into(),
                    content: Content::File {
                        digest,
                        executable: false,
                        metadata: FileMetadata::default(),
                    },
                }),
            }])
            .expect("transition");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        assert_eq!(fs::read(root.join("d/new")).unwrap(), b"new");
        assert!(!stale.exists(), "the stale leftover was kept");
        assert!(fresh.exists(), "a leftover still being written was removed");
    }

    #[test]
    fn a_leftover_of_a_running_process_is_kept_until_it_is_very_old() {
        let keep = tempdir().expect("temporary directory");
        let ours = |count: u32, age: std::time::Duration| {
            let path = keep.path().join(format!(
                "{TEMPORARY_PREFIX}-apply-{}-{count}",
                std::process::id()
            ));
            File::create(&path)
                .expect("leftover")
                .set_modified(std::time::SystemTime::now() - age)
                .expect("backdate");
        };
        ours(1, std::time::Duration::from_secs(3600));
        ours(
            2,
            crate::fsutil::TMP_MAX_AGE + std::time::Duration::from_secs(60),
        );
        let stale: Vec<String> = fs::read_dir(keep.path())
            .expect("list")
            .flatten()
            .filter(|entry| stale_publish_leftover(entry.file_name().to_str().unwrap(), entry))
            .map(|entry| entry.file_name().into_string().unwrap())
            .collect();
        assert_eq!(stale.len(), 1, "{stale:?}");
        assert!(stale[0].ends_with("-2"), "{stale:?}");
    }

    /// Makes `path`, as the endpoint just scanned it, executable.
    fn make_executable(endpoint: &mut LocalEndpoint, path: &str) -> TransitionOutcome {
        let snapshot = endpoint.scan().expect("scan");
        let old = snapshot
            .root
            .as_ref()
            .and_then(|root| root.child(path))
            .expect("scanned")
            .clone();
        let Content::File {
            digest, metadata, ..
        } = old.content.clone()
        else {
            panic!("{path} is not a file");
        };
        let new = Node {
            name: old.name.clone(),
            content: Content::File {
                digest,
                executable: true,
                metadata,
            },
        };
        endpoint
            .transition(vec![Change {
                path: path.into(),
                old: Some(old),
                new: Some(new),
            }])
            .expect("transition")
    }

    /// A file hardlinked into the root from outside it: changing its
    /// executable bit breaks the link, and the outside name keeps its
    /// mode.
    #[test]
    fn a_mode_change_never_reaches_a_hardlink_outside_the_root() {
        let keep = tempdir().expect("temporary directory");
        let root = keep.path().join("root");
        fs::create_dir_all(&root).expect("root");
        let outside = keep.path().join("outside");
        fs::write(&outside, b"shared inode").expect("outside");
        fs::set_permissions(&outside, Permissions::from_mode(0o640)).expect("mode");
        fs::hard_link(&outside, root.join("linked")).expect("hardlink");
        let mut endpoint = endpoint_with(&root, EndpointOptions::default());
        let outcome = make_executable(&mut endpoint, "linked");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);

        let outside_metadata = fs::metadata(&outside).expect("outside");
        assert_eq!(outside_metadata.mode() & 0o777, 0o640);
        let inside = fs::metadata(root.join("linked")).expect("inside");
        assert_ne!(
            inside.ino(),
            outside_metadata.ino(),
            "the link was not broken"
        );
        assert_eq!(
            inside.mode() & 0o777,
            creation_mode(DEFAULT_FILE_MODE, true)
        );
        assert_eq!(fs::read(root.join("linked")).unwrap(), b"shared inode");
        let achieved = outcome.results[0].as_ref().expect("achieved");
        let Content::File { metadata, .. } = &achieved.content else {
            panic!("not a file");
        };
        assert_eq!(metadata.inode, inside.ino());
    }

    /// A file with one link keeps its inode: the mode changes in place.
    #[test]
    fn a_mode_change_on_a_single_link_is_in_place() {
        let keep = tempdir().expect("temporary directory");
        let root = keep.path().join("root");
        fs::create_dir_all(&root).expect("root");
        fs::write(root.join("tool"), b"#!/bin/sh").expect("tool");
        let before = fs::metadata(root.join("tool")).expect("tool").ino();
        let mut endpoint = endpoint_with(&root, EndpointOptions::default());
        let outcome = make_executable(&mut endpoint, "tool");
        assert!(outcome.problems.is_empty(), "{:?}", outcome.problems);
        let after = fs::metadata(root.join("tool")).expect("tool");
        assert_eq!(after.ino(), before);
        assert_eq!(after.mode() & 0o777, creation_mode(DEFAULT_FILE_MODE, true));
    }
}

/// Supply and the staging receive: a peer may name only content this
/// side's own scan recorded, inside its root (T1-1, T1-5); staging
/// temporaries are private (LOCAL-03).
#[cfg(test)]
mod supply_receive_tests {
    use super::*;

    use std::os::unix::fs::symlink;
    use std::sync::mpsc;
    use std::time::Duration;
    use tempfile::{tempdir, TempDir};

    /// A scanned endpoint over `<keep>/root`, with staging beside it.
    fn scanned(options: EndpointOptions) -> (TempDir, PathBuf, LocalEndpoint) {
        let keep = tempdir().expect("temporary directory should be creatable");
        let root = keep.path().join("root");
        fs::create_dir_all(&root).expect("root should be creatable");
        let endpoint = LocalEndpoint::new(root.clone(), keep.path().join("staging"), options)
            .expect("endpoint should be creatable");
        (keep, root, endpoint)
    }

    fn digest_of(bytes: &[u8]) -> Digest {
        *blake3::hash(bytes).as_bytes()
    }

    fn need(path: &str, digest: Digest) -> StagingNeed {
        StagingNeed {
            request: FileRequest {
                path: path.into(),
                digest,
            },
            signature: Signature::default(),
        }
    }

    /// Supplies `needs` to exhaustion and returns every frame.
    fn supply_all(endpoint: &mut LocalEndpoint, needs: Vec<StagingNeed>) -> Vec<TransferFrame> {
        endpoint.supply_open(needs).expect("supply should open");
        let mut frames = Vec::new();
        loop {
            let batch = endpoint.supply_pull(64).expect("supply should pull");
            if batch.is_empty() {
                return frames;
            }
            frames.extend(batch);
        }
    }

    /// Asserts that a single-need stream carried no content: its begin,
    /// then an end of file with an error.
    fn assert_refused(frames: &[TransferFrame]) {
        assert!(
            matches!(
                frames,
                [
                    TransferFrame::Begin { .. },
                    TransferFrame::EndOfFile { error: Some(_) }
                ]
            ),
            "content left the root: {frames:?}"
        );
    }

    fn data_bytes(frames: &[TransferFrame]) -> Vec<u8> {
        frames
            .iter()
            .filter_map(|frame| match frame {
                TransferFrame::Op(crate::rsync::Op::Data(data)) => Some(data.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn an_absolute_supply_path_is_refused() {
        let (_keep, root, mut endpoint) = scanned(EndpointOptions::default());
        fs::write(root.join("a.txt"), b"inside").expect("file should be writable");
        endpoint.scan().expect("scan should succeed");
        let outside = tempdir().expect("temporary directory should be creatable");
        let secret = outside.path().join("secret.txt");
        fs::write(&secret, b"secret").expect("file should be writable");
        let frames = supply_all(
            &mut endpoint,
            vec![need(&secret.to_string_lossy(), digest_of(b"secret"))],
        );
        assert_refused(&frames);
    }

    #[test]
    fn a_dot_dot_supply_path_is_refused() {
        let (keep, root, mut endpoint) = scanned(EndpointOptions::default());
        fs::write(root.join("a.txt"), b"inside").expect("file should be writable");
        fs::write(keep.path().join("outside.txt"), b"secret").expect("file should be writable");
        endpoint.scan().expect("scan should succeed");
        let frames = supply_all(
            &mut endpoint,
            vec![need("../outside.txt", digest_of(b"secret"))],
        );
        assert_refused(&frames);
    }

    #[test]
    fn a_supply_path_through_a_symlinked_parent_is_refused() {
        let (keep, root, mut endpoint) = scanned(EndpointOptions::default());
        let outside = keep.path().join("outside");
        fs::create_dir_all(&outside).expect("directory should be creatable");
        fs::write(outside.join("secret.txt"), b"secret").expect("file should be writable");
        symlink(&outside, root.join("link")).expect("symlink should be creatable");
        endpoint.scan().expect("scan should succeed");
        let frames = supply_all(
            &mut endpoint,
            vec![need("link/secret.txt", digest_of(b"secret"))],
        );
        assert_refused(&frames);
    }

    #[test]
    fn content_other_than_the_requested_digest_is_refused() {
        let (_keep, root, mut endpoint) = scanned(EndpointOptions::default());
        fs::write(root.join("a.txt"), b"inside").expect("file should be writable");
        endpoint.scan().expect("scan should succeed");
        let frames = supply_all(&mut endpoint, vec![need("a.txt", [0u8; 32])]);
        assert_refused(&frames);
    }

    #[test]
    fn an_ignored_file_is_never_supplied() {
        let options = EndpointOptions {
            ignores: IgnoreSet::new(&[".env".to_string()]).expect("ignores"),
            ..EndpointOptions::default()
        };
        let (_keep, root, mut endpoint) = scanned(options);
        fs::write(root.join(".env"), b"TOKEN=secret").expect("file should be writable");
        endpoint.scan().expect("scan should succeed");
        let frames = supply_all(
            &mut endpoint,
            vec![need(".env", digest_of(b"TOKEN=secret"))],
        );
        assert_refused(&frames);
    }

    #[test]
    fn a_fifo_swapped_in_after_the_scan_is_refused_without_hanging() {
        let (_keep, root, mut endpoint) = scanned(EndpointOptions::default());
        let path = root.join("a.txt");
        fs::write(&path, b"content").expect("file should be writable");
        endpoint.scan().expect("scan should succeed");
        fs::remove_file(&path).expect("file should be removable");
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .expect("the path has no NUL");
        // SAFETY: `name` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);

        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let frames = supply_all(&mut endpoint, vec![need("a.txt", digest_of(b"content"))]);
            let _ = sender.send(frames);
        });
        let frames = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("supplying a FIFO must not hang");
        assert_refused(&frames);
    }

    #[test]
    fn a_file_grown_since_the_scan_never_supplies_more_than_was_scanned() {
        let (_keep, root, mut endpoint) = scanned(EndpointOptions::default());
        let path = root.join("a.txt");
        fs::write(&path, b"scanned").expect("file should be writable");
        endpoint.scan().expect("scan should succeed");
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("file should open");
        file.write_all(&vec![b'x'; 1 << 20])
            .expect("file should grow");
        let frames = supply_all(&mut endpoint, vec![need("a.txt", digest_of(b"scanned"))]);
        let data = data_bytes(&frames);
        assert!(
            data.len() <= b"scanned".len(),
            "{} bytes supplied",
            data.len()
        );
        assert!(matches!(
            frames.last(),
            Some(TransferFrame::EndOfFile { .. })
        ));
    }

    /// A staging request naming a path no scan could produce refuses the
    /// whole batch before anything is touched: no staging directory, no
    /// receive state, no signature.
    #[test]
    fn staging_requests_for_unsafe_paths_are_refused() {
        for path in [
            "/etc/hosts",
            "../outside.txt",
            "",
            ".autobahn-tmp-staging-x/y",
            "a/../../outside.txt",
        ] {
            let (keep, root, mut endpoint) = scanned(EndpointOptions::default());
            fs::write(root.join("a.txt"), b"inside").expect("file should be writable");
            fs::write(keep.path().join("outside.txt"), b"secret").expect("file should be writable");
            endpoint.scan().expect("scan should succeed");
            let requests = vec![
                FileRequest {
                    path: "a.txt".into(),
                    digest: digest_of(b"other"),
                },
                FileRequest {
                    path: path.into(),
                    digest: digest_of(b"secret"),
                },
            ];
            let error = endpoint
                .stage_begin(requests)
                .expect_err("an unsafe staging path must be refused");
            assert!(
                format!("{error:#}").contains("refusing"),
                "{path:?}: {error:#}"
            );
            assert!(endpoint.receive.is_none(), "{path:?} left receive state");
            assert!(
                !keep.path().join("staging").exists(),
                "{path:?} created the staging directory"
            );
        }
    }

    /// Even if the snapshot claimed a file behind a symlinked parent —
    /// a shape no real scan produces — the base signature is not computed
    /// through the link.
    #[test]
    fn a_base_behind_a_symlinked_parent_yields_an_empty_signature() {
        let (keep, root, mut endpoint) = scanned(EndpointOptions::default());
        let outside = keep.path().join("outside");
        fs::create_dir_all(&outside).expect("directory should be creatable");
        fs::write(outside.join("big.bin"), vec![7u8; 64 * 1024]).expect("file should be writable");
        symlink(&outside, root.join("link")).expect("symlink should be creatable");
        let mut snapshot = endpoint.scan().expect("scan should succeed");
        let metadata = file_metadata(
            &fs::metadata(outside.join("big.bin")).expect("file should be inspectable"),
        );
        snapshot.root = Some(Node {
            name: String::new(),
            content: Content::Directory(Arc::new(vec![Node {
                name: "link".into(),
                content: Content::Directory(Arc::new(vec![Node {
                    name: "big.bin".into(),
                    content: Content::File {
                        digest: digest_of(&[7u8; 64 * 1024]),
                        executable: false,
                        metadata,
                    },
                }])),
            }])),
        });
        endpoint.last_snapshot = Some(snapshot);
        let needs = endpoint
            .stage_begin(vec![FileRequest {
                path: "link/big.bin".into(),
                digest: digest_of(b"new content"),
            }])
            .expect("staging should begin");
        assert_eq!(needs.len(), 1);
        assert!(needs[0].signature.is_empty(), "a signature leaked");
    }

    /// The agent direction: a hostile controller asks a follower's agent
    /// for a file outside its root, and no content comes back.
    #[test]
    fn an_agent_supplies_nothing_outside_its_root() {
        use crate::protocol::{Initialize, Request, Response};
        use crate::transport::mux::AgentConnection;

        let keep = tempdir().expect("temporary directory should be creatable");
        let root = keep.path().join("root");
        let state = keep.path().join("state");
        fs::create_dir_all(&root).expect("root should be creatable");
        fs::create_dir_all(state.join("staging")).expect("staging should be creatable");
        fs::write(root.join("a.txt"), b"inside").expect("file should be writable");
        let secret = keep.path().join("secret.txt");
        fs::write(&secret, b"secret").expect("file should be writable");

        let (client, agent) = crate::transport::tests::connected_pair();
        let (agent_reader, agent_writer, _) = agent.into_parts();
        let agent_state = state.clone();
        std::thread::spawn(move || {
            let _ = crate::transport::serve_agent_in(agent_reader, agent_writer, &agent_state);
        });
        let connection = AgentConnection::connect(client).expect("unable to connect");
        let root_text = root.to_string_lossy().into_owned();
        let mut channel = connection
            .open(Initialize {
                session: crate::session::session_identifier(&root_text, "confinement"),
                root: root_text,
                ignores: Vec::new(),
                symlink_mode: SymlinkMode::Raw,
                file_mode: None,
                directory_mode: None,
                side: "alpha".into(),
                staging: Default::default(),
                max_file_size: None,
                max_entry_count: None,
                ignore_mounts: true,
                default_owner: None,
                default_group: None,
            })
            .expect("open");
        channel.exchange(Request::Scan).expect("the scan exchanges");
        let opened = channel
            .exchange(Request::SupplyOpen(vec![need(
                &secret.to_string_lossy(),
                digest_of(b"secret"),
            )]))
            .expect("the supply opens");
        assert!(matches!(opened, Response::SupplyOpened), "{opened:?}");
        let mut frames = Vec::new();
        loop {
            match channel
                .exchange(Request::SupplyPull(64))
                .expect("the supply pulls")
            {
                Response::SupplyPull(batch) if batch.is_empty() => break,
                Response::SupplyPull(batch) => frames.extend(batch),
                other => panic!("unexpected response: {other:?}"),
            }
        }
        assert_refused(&frames);
    }

    /// Two scanned endpoints: `alpha` supplies, `beta` receives.
    struct Pair {
        keep: TempDir,
        alpha: LocalEndpoint,
        beta: LocalEndpoint,
        alpha_root: PathBuf,
        beta_root: PathBuf,
        beta_staging: PathBuf,
        /// The changes the last [`Pair::begin`] staged for.
        changes: Vec<Change>,
    }

    fn pair() -> Pair {
        let keep = tempdir().expect("temporary directory should be creatable");
        let alpha_root = keep.path().join("alpha");
        let beta_root = keep.path().join("beta");
        let beta_staging = keep.path().join("staging-beta");
        fs::create_dir_all(&alpha_root).expect("root should be creatable");
        fs::create_dir_all(&beta_root).expect("root should be creatable");
        let alpha = LocalEndpoint::new(
            alpha_root.clone(),
            keep.path().join("staging-alpha"),
            EndpointOptions::default(),
        )
        .expect("endpoint should be creatable");
        let beta = LocalEndpoint::new(
            beta_root.clone(),
            beta_staging.clone(),
            EndpointOptions::default(),
        )
        .expect("endpoint should be creatable");
        Pair {
            keep,
            alpha,
            beta,
            alpha_root,
            beta_root,
            beta_staging,
            changes: Vec::new(),
        }
    }

    impl Pair {
        /// Scans both sides and begins staging beta's requests for every
        /// file alpha holds, opening alpha's supply of what beta needs.
        fn begin(&mut self) -> Vec<StagingNeed> {
            let alpha = self.alpha.scan().expect("scan should succeed");
            let beta = self.beta.scan().expect("scan should succeed");
            let changes = crate::tree::diff(beta.root.as_ref(), alpha.root.as_ref());
            let requests = crate::session::transition_dependencies(&changes);
            self.changes = changes;
            let needs = self
                .beta
                .stage_begin(requests)
                .expect("staging should begin");
            self.alpha
                .supply_open(needs.clone())
                .expect("supply should open");
            needs
        }

        /// Pulls and pushes until the supply is exhausted.
        fn drain(&mut self) {
            loop {
                let frames = self.alpha.supply_pull(4).expect("supply should pull");
                if frames.is_empty() {
                    return;
                }
                self.beta.stage_push(frames).expect("staging should accept");
            }
        }
    }

    fn receive_temporaries(staging: &Path) -> Vec<PathBuf> {
        fs::read_dir(staging)
            .expect("staging should be readable")
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(TEMPORARY_PREFIX))
            })
            .collect()
    }

    /// A transfer interrupted after its first operation leaves content
    /// only its owner can read.
    #[test]
    fn an_interrupted_receive_leaves_a_private_temporary() {
        let mut pair = pair();
        fs::write(pair.alpha_root.join("big.bin"), vec![3u8; 1 << 20])
            .expect("file should be writable");
        pair.begin();
        let frames = pair.alpha.supply_pull(2).expect("supply should pull");
        assert!(matches!(
            frames.as_slice(),
            [TransferFrame::Begin { .. }, TransferFrame::Op(_)]
        ));
        pair.beta.stage_push(frames).expect("staging should accept");
        let temporaries = receive_temporaries(&pair.beta_staging);
        assert_eq!(temporaries.len(), 1, "{temporaries:?}");
        let mode = fs::symlink_metadata(&temporaries[0])
            .expect("temporary should exist")
            .mode();
        assert_eq!(mode & 0o777, 0o600, "mode {mode:o}");
    }

    /// Symbolic links planted at the next temporary names are neither
    /// followed nor written through, for a received file or a local copy.
    #[test]
    fn a_planted_symlink_at_a_temporary_name_is_not_followed() {
        let plant = |staging: &Path, purpose: &str, target: &Path| {
            // Tests running alongside draw from the same counter, so the
            // plant covers the next few names, not just the next one: fewer
            // than `staging_temporary` passes over before giving up.
            let next = TEMPORARY_COUNTER.load(Ordering::Relaxed);
            for count in next..next + 4 {
                symlink(target, staging.join(temporary_name_at(purpose, count)))
                    .expect("symlink should be creatable");
            }
        };

        // Received content.
        let mut pair = pair();
        fs::write(pair.alpha_root.join("a.txt"), b"received").expect("file should be writable");
        let needs = pair.begin();
        let target = pair.keep.path().join("received-target");
        plant(&pair.beta_staging, "recv", &target);
        pair.drain();
        assert!(!target.exists(), "a receive wrote through a planted link");
        assert!(pair.beta.staged_path(&needs[0].request.digest).is_file());

        // A local copy.
        let mut pair = self::pair();
        fs::write(pair.alpha_root.join("copy.txt"), b"shared").expect("file should be writable");
        fs::write(pair.alpha_root.join("original.txt"), b"shared")
            .expect("file should be writable");
        fs::write(pair.beta_root.join("original.txt"), b"shared").expect("file should be writable");
        fs::create_dir_all(&pair.beta_staging).expect("staging should be creatable");
        let target = pair.keep.path().join("copy-target");
        plant(&pair.beta_staging, "copy", &target);
        let needs = pair.begin();
        assert!(needs.is_empty(), "{needs:?}");
        assert!(
            !target.exists(),
            "a local copy wrote through a planted link"
        );
        assert!(pair.beta.staged_path(&digest_of(b"shared")).is_file());
    }

    /// A staging failure's error, which crosses the wire to the peer,
    /// names no staging path.
    #[test]
    fn a_staging_failure_names_no_staging_path() {
        let mut pair = pair();
        fs::write(pair.alpha_root.join("a.txt"), b"content").expect("file should be writable");
        pair.begin();
        fs::remove_dir_all(&pair.beta_staging).expect("staging should be removable");
        let frames = pair.alpha.supply_pull(4).expect("supply should pull");
        let error = pair
            .beta
            .stage_push(frames)
            .expect_err("staging without a staging directory must fail");
        let text = format!("{error:#}");
        assert!(
            !text.contains(&*pair.beta_staging.to_string_lossy()),
            "{text}"
        );
        assert!(!text.contains(TEMPORARY_PREFIX), "{text}");
    }

    /// Pulls and pushes to exhaustion, counting the files that ended in an
    /// error.
    fn drain_counting_errors(pair: &mut Pair) -> usize {
        let mut errors = 0;
        loop {
            let frames = pair.alpha.supply_pull(4).expect("supply should pull");
            if frames.is_empty() {
                return errors;
            }
            errors += frames
                .iter()
                .filter(|frame| matches!(frame, TransferFrame::EndOfFile { error: Some(_) }))
                .count();
            pair.beta.stage_push(frames).expect("staging should accept");
        }
    }

    /// A file truncated after its first frame has gone out cannot fall
    /// back to another path: it ends in an error, the receiver keeps
    /// nothing, and the next cycle transfers what is there now.
    #[test]
    fn a_file_truncated_mid_supply_ends_in_an_error_and_converges_next_cycle() {
        let mut pair = pair();
        let path = pair.alpha_root.join("big.bin");
        fs::write(&path, vec![5u8; 1 << 20]).expect("file should be writable");
        let needs = pair.begin();
        let frames = pair.alpha.supply_pull(2).expect("supply should pull");
        assert!(matches!(
            frames.as_slice(),
            [TransferFrame::Begin { .. }, TransferFrame::Op(_)]
        ));
        pair.beta.stage_push(frames).expect("staging should accept");
        File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_len(1 << 19))
            .expect("file should be truncatable");
        assert_eq!(drain_counting_errors(&mut pair), 1);
        assert!(!pair.beta.staged_path(&needs[0].request.digest).exists());
        assert!(receive_temporaries(&pair.beta_staging).is_empty());

        // The next cycle.
        let needs = pair.begin();
        assert_eq!(drain_counting_errors(&mut pair), 0);
        assert_eq!(needs[0].request.digest, digest_of(&vec![5u8; 1 << 19]));
        assert!(pair.beta.staged_path(&needs[0].request.digest).is_file());
    }

    /// A file that grows after its first frame has gone out supplies
    /// exactly what was scanned.
    #[test]
    fn a_file_grown_mid_supply_supplies_what_was_scanned() {
        let mut pair = pair();
        let path = pair.alpha_root.join("big.bin");
        fs::write(&path, vec![5u8; 1 << 20]).expect("file should be writable");
        let needs = pair.begin();
        let frames = pair.alpha.supply_pull(2).expect("supply should pull");
        pair.beta.stage_push(frames).expect("staging should accept");
        File::options()
            .append(true)
            .open(&path)
            .and_then(|mut file| file.write_all(&[6u8; 1 << 20]))
            .expect("file should grow");
        assert_eq!(drain_counting_errors(&mut pair), 0);
        assert!(pair.beta.staged_path(&needs[0].request.digest).is_file());
    }

    /// A destination base that shrinks between staging and the push fails
    /// that file alone: the other files of the stream land and publish, and
    /// the next cycle transfers the one that failed.
    #[test]
    fn a_base_changed_mid_stream_fails_only_its_own_file() {
        let mut pair = pair();
        let names = ["one.bin", "two.bin", "three.bin"];
        for (seed, name) in names.iter().enumerate() {
            let base = pseudo_random(256 * 1024, seed as u64);
            fs::write(pair.beta_root.join(name), &base).expect("file should be writable");
            let mut changed = base;
            changed.extend_from_slice(b"appended on alpha");
            fs::write(pair.alpha_root.join(name), &changed).expect("file should be writable");
        }
        let needs = pair.begin();
        assert_eq!(needs.len(), 3);
        assert!(needs.iter().all(|need| !need.signature.is_empty()));
        // Beta's base for two.bin goes short after its signature was taken.
        File::options()
            .write(true)
            .open(pair.beta_root.join("two.bin"))
            .and_then(|file| file.set_len(1024))
            .expect("file should be truncatable");
        pair.drain();
        for need in &needs {
            let staged = pair.beta.staged_path(&need.request.digest).is_file();
            assert_eq!(
                staged,
                need.request.path != "two.bin",
                "{}",
                need.request.path
            );
        }
        assert!(receive_temporaries(&pair.beta_staging).is_empty());

        let changes = std::mem::take(&mut pair.changes);
        let outcome = pair
            .beta
            .transition(changes)
            .expect("transition should succeed");
        // two.bin is not published: its content is missing, and its base
        // changed since the scan besides.
        assert!(outcome.missing_staged_files || !outcome.problems.is_empty());
        assert_eq!(
            fs::metadata(pair.beta_root.join("two.bin"))
                .expect("file should exist")
                .len(),
            1024
        );
        for name in ["one.bin", "three.bin"] {
            assert_eq!(
                fs::read(pair.beta_root.join(name)).expect("file should be readable"),
                fs::read(pair.alpha_root.join(name)).expect("file should be readable"),
                "{name}"
            );
        }

        // The next cycle converges.
        let needs = pair.begin();
        assert_eq!(needs.len(), 1);
        pair.drain();
        let changes = std::mem::take(&mut pair.changes);
        let outcome = pair
            .beta
            .transition(changes)
            .expect("transition should succeed");
        assert!(!outcome.missing_staged_files, "{:?}", outcome.problems);
        for name in names {
            assert_eq!(
                fs::read(pair.beta_root.join(name)).expect("file should be readable"),
                fs::read(pair.alpha_root.join(name)).expect("file should be readable"),
                "{name}"
            );
        }
    }

    /// Deterministic content that no block of another seed matches.
    fn pseudo_random(length: usize, seed: u64) -> Vec<u8> {
        let mut state = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut data = Vec::with_capacity(length + 8);
        while data.len() < length {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            data.extend_from_slice(&state.to_le_bytes());
        }
        data.truncate(length);
        data
    }
}
