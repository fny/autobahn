//! One observation of a root, shared by every session that synchronizes it.
//!
//! A configuration group with N destinations becomes N sessions, and each
//! builds its own endpoint over the *same* source directory. Measured at ten
//! destinations over a 20,000-file tree, that cost ten times the inotify
//! watches, ten scan caches, and eight times the memory — all of it linear
//! in the destination count, and none of it absorbed by the page cache,
//! because none of it is disk reads.
//!
//! The watch count is a ceiling rather than an inefficiency: 400 directories
//! at ten destinations is 8,020 watches, and a large tree at the same width
//! exhausts `fs.inotify.max_user_watches` outright, after which every
//! session falls back to interval polling.
//!
//! So a root is observed once. The observer owns the watcher, the scan, the
//! baseline that makes a scan incremental, and the persisted cache. What it
//! must never own is **provenance**: the ancestor stays with its session,
//! because it records the history of one alpha/beta pair and legitimately
//! differs between destinations. Sharing observations is safe; sharing
//! provenance is the silent-overwrite failure this codebase is built to
//! avoid.
//!
//! # What a subscriber keeps
//!
//! Each endpoint holds a **lease**: the exact snapshot its own last scan
//! returned. Transitions validate against that lease, not against whatever
//! the observer has published since, so the transitioner's contract — "the
//! retained snapshot *is* the scan these transitions were reconciled from" —
//! survives sharing unchanged.
//!
//! # How reuse is decided
//!
//! Every filesystem event advances a generation counter. A scan is published
//! with the generation it was taken at, and served to a later caller only
//! while that generation still stands. There is no staleness window to tune:
//! a quiet tree serves one snapshot indefinitely, and the first event forces
//! exactly one fresh scan that every waiting session then shares.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::scan::{self, FilesystemBehavior, IgnoreSet, SymlinkMode};
use crate::tree::Snapshot;

/// How long to wait before retrying a watch that could not be established.
///
/// Registering a recursive watch walks every directory in the root, and the
/// usual reason it fails is a host already at its watch limit — which
/// retrying only aggravates.
const WATCH_RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// The longest a scan may be served without a full walk behind it, bounding
/// how long a missed filesystem event can persist.
const FULL_SCAN_INTERVAL: Duration = Duration::from_secs(120);

/// What makes two endpoints able to share one observation.
///
/// Only the properties that change *what a scan sees* belong here. File
/// modes, ownership, staging placement and synchronization mode differ
/// freely between sessions over one root without affecting the tree that a
/// scan produces.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ObserverKey {
    /// The root, canonicalized as far as it exists, so aliases share.
    pub root: PathBuf,
    /// A stable rendering of the ignore set, so two endpoints share an
    /// observation only when they would see the same tree.
    pub ignores: String,
    /// How symbolic links are treated.
    pub symlink_mode: SymlinkMode,
    /// The per-file size limit, above which content is untracked.
    pub max_file_size: Option<u64>,
}

/// The signal a filesystem event raises.
///
/// Deliberately its own lock, held only for the instant it takes to bump a
/// counter: a scan of a large tree takes seconds, and a watcher callback
/// must never wait behind one.
struct EventSignal {
    /// Advanced by every event and by every deliberate invalidation.
    generation: Mutex<u64>,
    /// Woken whenever the generation advances.
    wake: Condvar,
}

impl EventSignal {
    fn advance(&self) {
        let mut generation = self.generation.lock().unwrap_or_else(|e| e.into_inner());
        *generation += 1;
        self.wake.notify_all();
    }

    fn current(&self) -> u64 {
        *self.generation.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The observer's mutable state, held only outside a scan.
struct State {
    /// Probed once per volume, then reused.
    behavior: Option<FilesystemBehavior>,
    /// The single watcher for this root.
    watcher: Option<crate::endpoint::local::ChangeWatcher>,
    /// When to next attempt a watch that failed.
    watch_retry_after: Option<Instant>,
    /// The published scan and the generation it was taken at. Served while
    /// that generation still stands.
    published: Option<(u64, Snapshot)>,
    /// The tree a scan starts from, which is what makes it incremental.
    /// Advanced by a scan, and by a transition folding what it achieved.
    baseline: Option<Snapshot>,
    /// When the last *full* walk completed.
    last_full_scan: Option<Instant>,
    /// Set while a scan is running, so concurrent callers wait for it
    /// rather than each walking the tree.
    scanning: bool,
}

/// One root, observed once on behalf of every session that synchronizes it.
pub struct RootObserver {
    key: ObserverKey,
    /// The compiled ignore set the key renders.
    ignores: IgnoreSet,
    signal: Arc<EventSignal>,
    state: Mutex<State>,
    /// Woken when a scan completes, so callers waiting on one can proceed.
    scanned: Condvar,
    /// Where the scan cache lives. One file per observed root, rather than
    /// one per session.
    cache_path: PathBuf,
    /// The background writer for that cache.
    writer: crate::persist::StateWriter,
}

impl RootObserver {
    /// The generation a subscriber should record after acting on a scan.
    pub fn generation(&self) -> u64 {
        self.signal.current()
    }

    /// Waits until the generation moves past `seen`, or the timeout expires.
    /// Returns whether it moved.
    pub fn await_change(&self, seen: u64, timeout: Duration) -> bool {
        self.ensure_watching();
        let deadline = Instant::now() + timeout;
        let mut generation = self
            .signal
            .generation
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while *generation <= seen {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, timed_out) = self
                .signal
                .wake
                .wait_timeout(generation, remaining)
                .unwrap_or_else(|e| e.into_inner());
            generation = next;
            if timed_out.timed_out() && *generation <= seen {
                return false;
            }
        }
        true
    }

    /// Establishes the watcher if it is absent and not in a backoff period.
    ///
    /// A failed attempt is not retried immediately: registering a recursive
    /// watch walks the whole root, and the usual cause of failure is a host
    /// already at its watch limit.
    fn ensure_watching(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.watcher.is_some() {
            return;
        }
        if let Some(retry_after) = state.watch_retry_after {
            if Instant::now() < retry_after {
                return;
            }
        }
        let signal = Arc::clone(&self.signal);
        match crate::endpoint::local::ChangeWatcher::new(&self.key.root, move || signal.advance()) {
            Ok(watcher) => {
                if state.watch_retry_after.take().is_some() {
                    eprintln!("[{}] watching resumed", self.key.root.display());
                }
                state.watcher = Some(watcher);
                // A watch just established has no record of what happened
                // before it existed, so the next scan must be full.
                state.last_full_scan = None;
            }
            Err(error) => {
                if state.watch_retry_after.is_none() {
                    eprintln!(
                        "[{}] unable to watch for changes ({error}); falling back to interval \
                         polling, retrying every {}s",
                        self.key.root.display(),
                        WATCH_RETRY_INTERVAL.as_secs()
                    );
                }
                state.watch_retry_after = Some(Instant::now() + WATCH_RETRY_INTERVAL);
            }
        }
    }

    /// The current view of the root, scanned if the published one is stale.
    ///
    /// Callers arriving while a scan is already running wait for it rather
    /// than starting their own, so N sessions cost one walk.
    /// Returns the snapshot and the generation it reflects. A subscriber
    /// records that generation, not the current one: an event arriving
    /// during the walk leaves the result immediately stale, which is the
    /// safe direction — at worst one extra cycle, never a missed change.
    pub fn scan(&self, max_entry_count: Option<u64>) -> Result<(Snapshot, u64)> {
        self.scan_inner(max_entry_count, false)
    }

    /// Scans with digest reuse disabled: every file is re-read, so content
    /// changed without its metadata moving becomes visible — and, being
    /// published like any other scan, the verified snapshot becomes every
    /// sharing session's baseline.
    pub fn scan_rehash(&self, max_entry_count: Option<u64>) -> Result<(Snapshot, u64)> {
        self.scan_inner(max_entry_count, true)
    }

    fn scan_inner(&self, max_entry_count: Option<u64>, rehash: bool) -> Result<(Snapshot, u64)> {
        self.ensure_watching();

        loop {
            let (baseline, behavior, want_full) = {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());

                // Serve the published scan while its generation still
                // stands: nothing has happened to the tree since it was
                // taken, so a fresh walk could only reproduce it.
                // A verifying scan never serves the cache: the re-read is
                // the entire point.
                if !rehash {
                    if let Some((taken_at, snapshot)) = &state.published {
                        if *taken_at == self.signal.current() && !self.full_scan_due(&state) {
                            return Ok((snapshot.clone(), *taken_at));
                        }
                    }
                }

                if state.scanning {
                    // Someone else is walking the tree; wait for their
                    // result rather than duplicating the work.
                    let _unused = self
                        .scanned
                        .wait_timeout(state, Duration::from_secs(60))
                        .unwrap_or_else(|e| e.into_inner());
                    continue;
                }

                if state.behavior.is_none() && std::fs::symlink_metadata(&self.key.root).is_ok() {
                    state.behavior = Some(scan::probe(&self.key.root));
                }
                state.scanning = true;
                let want_full = rehash || self.full_scan_due(&state);
                (
                    state.baseline.clone(),
                    state.behavior.unwrap_or_default(),
                    want_full,
                )
            };

            // Outside the lock: a large tree takes seconds to walk, and a
            // watcher callback must never wait behind one.
            let result = self.walk(baseline.as_ref(), &behavior, want_full, rehash);

            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.scanning = false;
            self.scanned.notify_all();
            let (snapshot, taken_at, was_full) = result?;
            if was_full {
                state.last_full_scan = Some(Instant::now());
            }

            // The entry limit guards against synchronizing the wrong tree
            // entirely, so exceeding it fails rather than making partial
            // progress on a probable mistake. Checked per caller, since
            // sessions may configure different limits over one root.
            if let Some(limit) = max_entry_count {
                let entries = snapshot.directories + snapshot.files + snapshot.symlinks;
                if entries > limit {
                    bail!(
                        "the scan of {} found {entries} entries, exceeding the configured \
                         limit of {limit}",
                        self.key.root.display()
                    );
                }
            }

            // Persist only when the hierarchy actually changed: an unchanged
            // scan shares storage with its baseline, so the comparison is a
            // pointer check rather than a walk.
            if !crate::tree::nodes_share_storage(
                baseline
                    .as_ref()
                    .and_then(|snapshot| snapshot.root.as_ref()),
                snapshot.root.as_ref(),
            ) {
                self.store_cache(&snapshot);
            }

            state.baseline = Some(snapshot.clone());
            state.published = Some((taken_at, snapshot.clone()));
            return Ok((snapshot, taken_at));
        }
    }

    fn full_scan_due(&self, state: &State) -> bool {
        match state.last_full_scan {
            Some(last) => last.elapsed() >= FULL_SCAN_INTERVAL,
            None => true,
        }
    }

    /// Walks the tree, returning the snapshot, the generation it reflects,
    /// and whether the walk was full.
    fn walk(
        &self,
        baseline: Option<&Snapshot>,
        behavior: &FilesystemBehavior,
        want_full: bool,
        rehash: bool,
    ) -> Result<(Snapshot, u64, bool)> {
        // The generation is read *before* the walk: an event arriving during
        // it leaves the published snapshot immediately stale, which is the
        // safe direction. Reading afterwards would claim the scan saw a
        // change it may have walked straight past.
        let taken_at = self.signal.current();

        let dirty = if want_full {
            None
        } else {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let watcher = state.watcher.as_mut();
            match (watcher, baseline) {
                (Some(watcher), Some(_)) => watcher.take_dirty(&self.key.root, behavior),
                _ => None,
            }
        };

        let snapshot = scan::scan(
            &self.key.root,
            baseline,
            &self.ignores,
            behavior,
            self.key.symlink_mode,
            self.key.max_file_size,
            dirty.as_ref(),
            rehash,
        )
        .with_context(|| format!("unable to scan {}", self.key.root.display()))?;
        Ok((snapshot, taken_at, dirty.is_none()))
    }

    /// Declares that this root is about to be written to.
    ///
    /// Called *before* the write, not after: the watcher's own event for it
    /// may arrive late, and a scan published in that gap would describe a
    /// tree that no longer exists.
    pub fn invalidate(&self) {
        self.signal.advance();
    }

    /// Offers what a transition achieved as the next scan's baseline.
    ///
    /// The fold describes what is actually on disk — refusals included — so
    /// it keeps the next scan's digest reuse intact. It advances the
    /// *baseline* but not the published generation: the next scan still
    /// runs, it simply starts from a tree that already knows about the
    /// write instead of re-digesting everything just published.
    pub fn offer_baseline(&self, folded: Snapshot) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.baseline = Some(folded);
        state.published = None;
    }

    /// Declares the observation unreliable: the filesystem disagreed with
    /// what a scan recorded, so the baseline is proven wrong somewhere and
    /// the next scan must read rather than adopt.
    pub fn distrust_baseline(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.last_full_scan = None;
        state.published = None;
        drop(state);
        self.signal.advance();
    }

    /// How much unconsumed change the watcher holds, for burst detection.
    pub fn activity(&self) -> Option<crate::endpoint::ChangeActivity> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.watcher.as_ref().map(|watcher| watcher.activity())
    }

    /// Where this root's scan cache lives.
    pub fn cache_path(&self) -> &Path {
        &self.cache_path
    }

    /// Overrides the probed filesystem behavior, so a test can exercise the
    /// decomposing and case-insensitive paths without such a volume.
    #[cfg(test)]
    pub(crate) fn force_behavior(&self, behavior: FilesystemBehavior) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .behavior = Some(behavior);
    }

    /// The probed behavior of the root's filesystem.
    pub fn behavior(&self) -> FilesystemBehavior {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .behavior
            .unwrap_or_default()
    }

    /// Blocks until pending cache writes complete.
    pub fn flush_state(&self) {
        self.writer.flush();
    }

    fn store_cache(&self, snapshot: &Snapshot) {
        let snapshot = snapshot.clone();
        self.writer.store(self.cache_path.clone(), move || {
            bincode::serialize(&snapshot).ok()
        });
    }

    fn load_cache(&self) -> Option<Snapshot> {
        let data = std::fs::read(&self.cache_path).ok()?;
        let snapshot: Snapshot = bincode::deserialize(&data).ok()?;
        snapshot.root.as_ref()?.validate(false).ok()?;
        Some(snapshot)
    }
}

/// The observers currently in use, one per distinct root and scan policy.
///
/// Weak references, so an observer lives exactly as long as the endpoints
/// using it: when the last session over a root goes away — paused, reset, or
/// backed off after a failure — its watches go with it, as they did when
/// every session owned its own.
static OBSERVERS: Mutex<Option<HashMap<ObserverKey, Weak<RootObserver>>>> = Mutex::new(None);

/// The observer for a root, created if no live one matches.
pub fn observer_for(
    key: ObserverKey,
    ignores: IgnoreSet,
    cache_path: PathBuf,
) -> Arc<RootObserver> {
    let mut registry = OBSERVERS.lock().unwrap_or_else(|e| e.into_inner());
    let registry = registry.get_or_insert_with(HashMap::new);
    if let Some(existing) = registry.get(&key).and_then(Weak::upgrade) {
        return existing;
    }
    let observer = Arc::new(RootObserver {
        key: key.clone(),
        ignores,
        signal: Arc::new(EventSignal {
            generation: Mutex::new(0),
            wake: Condvar::new(),
        }),
        state: Mutex::new(State {
            behavior: None,
            watcher: None,
            watch_retry_after: None,
            published: None,
            baseline: None,
            last_full_scan: None,
            scanning: false,
        }),
        scanned: Condvar::new(),
        cache_path,
        writer: crate::persist::StateWriter::new(),
    });
    // A cold start seeds the baseline from the persisted cache, so the first
    // scan of a process re-digests only what changed since the last one.
    if let Some(cached) = observer.load_cache() {
        observer
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .baseline = Some(cached);
    }
    registry.insert(key, Arc::downgrade(&observer));
    registry.retain(|_, weak| weak.strong_count() > 0);
    Arc::clone(&observer)
}

/// Canonicalizes a root as far as it exists, so two spellings of one
/// directory share an observer rather than watching it twice.
pub fn canonical_root(root: &Path) -> PathBuf {
    // A root that does not exist yet still has to key consistently, so the
    // deepest existing ancestor is resolved and the rest re-appended. Two
    // spellings of one directory — a symbolic link, a trailing `/.` — then
    // agree, and share one observation instead of watching it twice.
    let mut candidate = root.to_path_buf();
    let mut suffix = Vec::new();
    loop {
        if let Ok(resolved) = candidate.canonicalize() {
            let mut result = resolved;
            for component in suffix.iter().rev() {
                result.push(component);
            }
            return result;
        }
        match candidate.file_name() {
            Some(name) => {
                suffix.push(name.to_owned());
                if !candidate.pop() {
                    return root.to_path_buf();
                }
            }
            None => return root.to_path_buf(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two spellings of one directory must key the same, or the root is
    /// watched twice and the sharing does nothing.
    #[test]
    fn aliases_of_one_root_canonicalize_together() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let real = directory.path().join("real");
        std::fs::create_dir(&real).expect("create");
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        assert_eq!(canonical_root(&real), canonical_root(&link));
        assert_eq!(canonical_root(&real), canonical_root(&real.join(".")));
    }

    /// A root that does not exist yet keys by its deepest existing
    /// ancestor plus the rest of the path — including more than one
    /// missing component, which an earlier version of this got wrong.
    #[test]
    fn a_missing_root_keys_by_its_deepest_existing_ancestor() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let base = directory.path().canonicalize().expect("canonicalize");

        let one = canonical_root(&base.join("missing"));
        assert_eq!(one, base.join("missing"));

        // Two missing components: the earlier version returned after the
        // first, losing the rest of the path.
        let two = canonical_root(&base.join("missing").join("deeper"));
        assert_eq!(two, base.join("missing").join("deeper"));

        // And it must still agree with the same path reached through an
        // alias of its existing ancestor.
        let link = directory.path().join("alias");
        std::os::unix::fs::symlink(&base, &link).expect("symlink");
        assert_eq!(canonical_root(&link.join("missing").join("deeper")), two);
    }
}
