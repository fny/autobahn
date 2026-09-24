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
//!
//! That reasoning needs a watch covering the whole tree: without one, an
//! external write moves no generation. So a root that is not watched, or
//! is watched only in part, reuses nothing, and every scan walks — though
//! a walk begun after a caller asked serves that caller too, so callers
//! arriving together still share one.

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
    /// Whether mount points inside the root are left alone.
    pub ignore_mounts: bool,
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
    /// Sessions sleeping on this root through their own signal (see
    /// [`RootObserver::subscribe`]), raised whenever the generation
    /// advances. Held weakly: a session that has gone leaves nothing to
    /// wake, and is dropped from the list at the next raise.
    sleepers: Mutex<Vec<Weak<crate::endpoint::WakeSignal>>>,
}

impl EventSignal {
    /// Advances the generation, returning the one this advance produced.
    fn advance(&self) -> u64 {
        let mut generation = self.generation.lock().unwrap_or_else(|e| e.into_inner());
        *generation += 1;
        let produced = *generation;
        self.wake.notify_all();
        drop(generation);
        let mut sleepers = self.sleepers.lock().unwrap_or_else(|e| e.into_inner());
        sleepers.retain(|sleeper| match sleeper.upgrade() {
            Some(sleeper) => {
                sleeper.raise();
                true
            }
            None => false,
        });
        produced
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
    /// The published scan. Served while its generation still stands and a
    /// healthy watcher stands behind that generation, or to a caller that
    /// arrived before its walk began.
    published: Option<Published>,
    /// How many walks have begun, so a caller can tell a walk that began
    /// after it asked from one already under way.
    walks_begun: u64,
    /// The tree a scan starts from, which is what makes it incremental.
    /// Advanced by a scan, and by a transition folding what it achieved.
    baseline: Option<Snapshot>,
    /// The generation the baseline reflects, so an offer based on older
    /// observations cannot roll the baseline back past a change whose
    /// dirty marks a scan already consumed.
    baseline_generation: u64,
    /// When the last *full* walk completed.
    last_full_scan: Option<Instant>,
    /// Set while a scan is running, so concurrent callers wait for it
    /// rather than each walking the tree.
    scanning: bool,
}

/// A published scan.
struct Published {
    /// The generation the scan was taken at.
    generation: u64,
    snapshot: Snapshot,
    /// Which walk produced it, counted by [`State::walks_begun`].
    walk: u64,
}

/// Holds a scan as running for as long as it lives. Dropping it — on
/// success, on an error, or while a panic unwinds out of the walk — lets
/// the next caller in, so nobody waits out the timeout on a scan that is
/// no longer running.
struct Scanning<'a>(&'a RootObserver);

impl Drop for Scanning<'_> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.scanning = false;
        drop(state);
        self.0.scanned.notify_all();
    }
}

/// One root, observed once on behalf of every session that synchronizes it.
pub struct RootObserver {
    key: ObserverKey,
    /// The compiled ignore set the key renders.
    ignores: IgnoreSet,
    signal: Arc<EventSignal>,
    /// Whether any endpoint over this root will ever wait for a change.
    /// Until one says so, no watcher is registered: a one-shot never
    /// waits, and registering walks the whole tree.
    watch_wanted: std::sync::atomic::AtomicBool,
    state: Mutex<State>,
    /// Woken when a scan completes, so callers waiting on one can proceed.
    scanned: Condvar,
    /// Where the scan cache lives. One file per observed root, rather than
    /// one per session.
    cache_path: PathBuf,
    /// The background writer for that cache.
    writer: crate::persist::StateWriter,
    /// A test seam invoked between the walk and its publication, so the
    /// generation gate — a snapshot must never be served as current across
    /// a change it did not observe — can be exercised at the one moment it
    /// exists to protect.
    #[cfg(test)]
    pub(crate) after_walk: Mutex<Option<Box<dyn Fn() + Send>>>,
    /// A test seam that keeps the watcher from ever starting, modelling
    /// the polling fallback: the worst case for the generation protocol,
    /// because no kernel event ever arrives to stamp a stale publication.
    #[cfg(test)]
    pub(crate) suppress_watching: std::sync::atomic::AtomicBool,
    /// A test seam that fails the next walk once, after it has taken its
    /// dirty marks: the moment a failure could lose them.
    #[cfg(test)]
    pub(crate) fail_walk: std::sync::atomic::AtomicBool,
}

impl RootObserver {
    /// The generation a subscriber should record after acting on a scan.
    pub fn generation(&self) -> u64 {
        self.signal.current()
    }

    /// Whether the root is being watched, as opposed to polled: only then
    /// does a generation that did not move mean nothing changed. A watch
    /// that has stopped covering the tree — part of it could not be
    /// watched, or the root was replaced — is not watching.
    pub fn is_watching(&self) -> bool {
        self.ensure_watching();
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::watched(&state)
    }

    /// Whether a watcher stands behind the generation, whole: the one
    /// condition under which a generation that did not move proves the
    /// tree did not either.
    fn watched(state: &State) -> bool {
        state
            .watcher
            .as_ref()
            .is_some_and(|watcher| watcher.fault().is_none())
    }

    /// Waits until the generation moves past `seen`, or the timeout expires.
    /// Returns whether it moved.
    pub fn await_change(&self, seen: u64, timeout: Duration) -> bool {
        self.await_change_seen(seen, timeout).is_some()
    }

    /// Like [`await_change`](RootObserver::await_change), but says which
    /// generation the wait observed, for a waiter that has no scan of its
    /// own to measure the next wait from.
    pub fn await_change_seen(&self, seen: u64, timeout: Duration) -> Option<u64> {
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
                return None;
            }
            let (next, timed_out) = self
                .signal
                .wake
                .wait_timeout(generation, remaining)
                .unwrap_or_else(|e| e.into_inner());
            generation = next;
            if timed_out.timed_out() && *generation <= seen {
                return None;
            }
        }
        Some(*generation)
    }

    /// Registers a signal to raise whenever the generation advances, and
    /// makes sure the root is being watched. Registering twice is
    /// harmless; the list is pruned of gone signals as it is raised.
    pub fn subscribe(&self, signal: &Arc<crate::endpoint::WakeSignal>) {
        self.ensure_watching();
        let mut sleepers = self
            .signal
            .sleepers
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !sleepers
            .iter()
            .any(|sleeper| sleeper.ptr_eq(&Arc::downgrade(signal)))
        {
            sleepers.push(Arc::downgrade(signal));
        }
    }

    /// Establishes the watcher if it is absent and not in a backoff period.
    ///
    /// A failed attempt is not retried immediately: registering a recursive
    /// watch walks the whole root, and the usual cause of failure is a host
    /// already at its watch limit.
    /// Says that an endpoint over this root will wait for changes, so
    /// the root is to be watched from now on.
    pub fn want_watching(&self) {
        self.watch_wanted
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn ensure_watching(&self) {
        if !self.watch_wanted.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        #[cfg(test)]
        if self
            .suppress_watching
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(watcher) = state.watcher.as_ref() {
            use crate::endpoint::local::WatchFault;
            match watcher.fault() {
                None => return,
                // A watch on a directory that is no longer the root sees
                // nothing that is synchronized. It is rebuilt at once: a
                // replaced root needs a full walk anyway, and nothing
                // suggests the host is at its watch limit.
                Some(WatchFault::Replaced) => {
                    eprintln!(
                        "[{}] the root was replaced; watching it afresh",
                        self.key.root.display()
                    );
                    state.watcher = None;
                    state.watch_retry_after = None;
                }
                // Part of the tree is unwatched, so the watch proves
                // nothing about it. Polling takes over, and the watch is
                // rebuilt whole after the usual backoff: the usual cause is
                // the watch limit, which an immediate retry only meets
                // again. Said once; the rebuild says when it succeeds.
                Some(WatchFault::Incomplete(reason)) => {
                    eprintln!(
                        "[{}] part of the tree could not be watched ({reason}); falling back \
                         to interval polling, rebuilding the watch every {}s",
                        self.key.root.display(),
                        WATCH_RETRY_INTERVAL.as_secs()
                    );
                    state.watcher = None;
                    state.last_full_scan = None;
                    state.watch_retry_after = Some(Instant::now() + WATCH_RETRY_INTERVAL);
                    return;
                }
            }
        }
        if let Some(retry_after) = state.watch_retry_after {
            if Instant::now() < retry_after {
                return;
            }
        }
        let signal = Arc::clone(&self.signal);
        match crate::endpoint::local::ChangeWatcher::new(
            &self.key.root,
            self.ignores.clone(),
            self.key.ignore_mounts,
            move || {
                signal.advance();
            },
        ) {
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
                // A root that is missing — moved aside, say, with its
                // replacement still being copied — is not a watch limit,
                // so it is tried again at the next opportunity rather than
                // after the backoff.
                let retry_in = match std::fs::symlink_metadata(&self.key.root) {
                    Ok(_) => WATCH_RETRY_INTERVAL,
                    Err(_) => Duration::ZERO,
                };
                state.watch_retry_after = Some(Instant::now() + retry_in);
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
    pub fn scan(
        &self,
        max_entry_count: Option<u64>,
        progress: Option<&crate::progress::SideProgress>,
    ) -> Result<(Snapshot, u64)> {
        self.scan_inner(max_entry_count, false, progress)
    }

    /// Scans with digest reuse disabled: every file is re-read, so content
    /// changed without its metadata moving becomes visible — and, being
    /// published like any other scan, the verified snapshot becomes every
    /// sharing session's baseline.
    pub fn scan_rehash(
        &self,
        max_entry_count: Option<u64>,
        progress: Option<&crate::progress::SideProgress>,
    ) -> Result<(Snapshot, u64)> {
        self.scan_inner(max_entry_count, true, progress)
    }

    fn scan_inner(
        &self,
        max_entry_count: Option<u64>,
        rehash: bool,
        progress: Option<&crate::progress::SideProgress>,
    ) -> Result<(Snapshot, u64)> {
        self.ensure_watching();
        // The walks already begun when this caller arrived. A walk begun
        // after it is as fresh as one of its own.
        let mut arrived = None;

        loop {
            let (baseline, behavior, want_full, walk, running) = {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let arrived = *arrived.get_or_insert(state.walks_begun);

                // Serve the published scan while its generation still
                // stands: nothing has happened to the tree since it was
                // taken, so a fresh walk could only reproduce it. That
                // holds only while a whole watch stands behind the
                // generation; without one an external write never moves
                // it, so every scan walks, which is the polling a root
                // without a watch is promised. A walk that began after this
                // caller arrived serves it either way, so callers waiting
                // on one walk share it.
                // A verifying scan never serves the cache: the re-read is
                // the entire point.
                if !rehash {
                    if let Some(published) = &state.published {
                        let current = published.generation == self.signal.current()
                            && !self.full_scan_due(&state)
                            && Self::watched(&state);
                        if current || published.walk > arrived {
                            self.within_limit(&published.snapshot, max_entry_count)?;
                            return Ok((published.snapshot.clone(), published.generation));
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
                state.walks_begun += 1;
                let want_full = rehash || self.full_scan_due(&state);
                (
                    state.baseline.clone(),
                    state.behavior.unwrap_or_default(),
                    want_full,
                    state.walks_begun,
                    Scanning(self),
                )
            };

            // Outside the lock: a large tree takes seconds to walk, and a
            // watcher callback must never wait behind one.
            let result = self.walk(baseline.as_ref(), &behavior, want_full, rehash, progress);
            #[cfg(test)]
            if let Some(hook) = self
                .after_walk
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
            {
                hook();
            }

            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            // An incremental walk has taken the watcher's dirty marks by
            // now. If it cannot publish, the next scan is full: that reads
            // everything the marks named, where putting them back could
            // only race the events recorded since.
            let (snapshot, taken_at, was_full) = match result {
                Ok(walked) => walked,
                Err(error) => {
                    state.last_full_scan = None;
                    return Err(error);
                }
            };
            if was_full {
                state.last_full_scan = Some(Instant::now());
            }

            // A refused scan updates nothing, and loses no marks either.
            if let Err(error) = self.within_limit(&snapshot, max_entry_count) {
                if !was_full {
                    state.last_full_scan = None;
                }
                return Err(error);
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
            state.baseline_generation = taken_at;
            state.published = Some(Published {
                generation: taken_at,
                snapshot: snapshot.clone(),
                walk,
            });
            // The state lock is released before the scan is: `running`
            // takes it again to let the next caller in.
            drop(state);
            drop(running);
            return Ok((snapshot, taken_at));
        }
    }

    /// Refuses a snapshot with more entries than the caller's limit.
    ///
    /// The entry limit guards against synchronizing the wrong tree
    /// entirely, so exceeding it fails rather than making partial progress
    /// on a probable mistake. Checked per caller and on every return,
    /// cached or walked, since sessions sharing one root may configure
    /// different limits.
    fn within_limit(&self, snapshot: &Snapshot, max_entry_count: Option<u64>) -> Result<()> {
        if let Some(limit) = max_entry_count {
            let entries = snapshot.directories + snapshot.files + snapshot.symlinks;
            if entries > limit {
                bail!(
                    "the scan of {} found {entries} entries, exceeding the configured limit of \
                     {limit}",
                    self.key.root.display()
                );
            }
        }
        Ok(())
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
        progress: Option<&crate::progress::SideProgress>,
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
            // Only a whole watch's marks can stand for what changed.
            let watched = Self::watched(&state);
            match (state.watcher.as_mut(), baseline) {
                (Some(watcher), Some(_)) if watched => watcher.take_dirty(&self.key.root, behavior),
                _ => None,
            }
        };

        #[cfg(test)]
        if self
            .fail_walk
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            bail!("an injected walk failure");
        }

        let snapshot = scan::scan(
            &self.key.root,
            baseline,
            &self.ignores,
            behavior,
            self.key.symlink_mode,
            self.key.max_file_size,
            dirty.as_ref(),
            rehash,
            progress,
            self.key.ignore_mounts,
        )
        .with_context(|| format!("unable to scan {}", self.key.root.display()))?;
        Ok((snapshot, taken_at, dirty.is_none()))
    }

    /// Declares that the given root-relative paths are about to be written.
    ///
    /// Called *before* the writes, not after: the watcher's own events for
    /// them may arrive late, and a scan published in that gap would
    /// describe a tree that no longer exists. The paths are recorded in the
    /// watcher's pending set before the generation advances — the same
    /// order the watcher's own callback uses — so any walk old enough to
    /// miss them in its dirty set is also old enough for its publication to
    /// be refused as current.
    ///
    /// Returns the generation this invalidation produced, so a caller can
    /// tell whether anyone else advanced the generation around it.
    pub fn invalidate<'a>(&self, paths: impl IntoIterator<Item = &'a str>) -> u64 {
        {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(watcher) = state.watcher.as_ref() {
                // Joined under the observer's canonical root, so the
                // consuming strip_prefix is exact.
                watcher.mark_pending(paths.into_iter().map(|path| self.key.root.join(path)));
            }
        }
        self.signal.advance()
    }

    /// Offers what a transition achieved as the next scan's baseline.
    ///
    /// The fold describes what is actually on disk — refusals included — so
    /// it keeps the next scan's digest reuse intact. It advances the
    /// *baseline* but not the published generation: the next scan still
    /// runs, it simply starts from a tree that already knows about the
    /// write instead of re-digesting everything just published.
    ///
    /// `based_on` is the generation of the lease the fold was built from.
    /// An offer based on an older generation than the standing baseline is
    /// refused: adopting it would roll the baseline back past a change
    /// whose dirty marks a scan has already consumed, and the next scan
    /// would adopt the rolled-back record for paths nothing tells it to
    /// re-read. The offerer's own writes stay safe under refusal — the
    /// transition announces its paths again after its last write, so even
    /// a scan that consumed the pre-write marks is outdated by the time
    /// the offer could be refused.
    pub fn offer_baseline(&self, folded: Snapshot, based_on: u64) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if based_on < state.baseline_generation {
            return;
        }
        state.baseline = Some(folded);
        state.baseline_generation = based_on;
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
        watch_wanted: std::sync::atomic::AtomicBool::new(false),
        signal: Arc::new(EventSignal {
            generation: Mutex::new(0),
            wake: Condvar::new(),
            sleepers: Mutex::new(Vec::new()),
        }),
        state: Mutex::new(State {
            behavior: None,
            watcher: None,
            watch_retry_after: None,
            published: None,
            walks_begun: 0,
            baseline: None,
            baseline_generation: 0,
            last_full_scan: None,
            scanning: false,
        }),
        scanned: Condvar::new(),
        cache_path,
        writer: crate::persist::StateWriter::new(),
        #[cfg(test)]
        after_walk: Mutex::new(None),
        #[cfg(test)]
        suppress_watching: std::sync::atomic::AtomicBool::new(false),
        #[cfg(test)]
        fail_walk: std::sync::atomic::AtomicBool::new(false),
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

    // ── the generation protocol, under enumerated interleavings ──────
    //
    // The observer's one hard promise: a snapshot is never served as
    // current across a change it did not observe. Example tests are weak
    // evidence for a protocol like this, so the promise is checked under
    // directed interleavings — including the mid-scan one, which is the
    // exact moment the generation gate exists for — and a randomized
    // sweep of operation sequences.

    fn harness_observer(root: &std::path::Path) -> Arc<RootObserver> {
        let observer = observer_with(root, &[]);
        // Watched, as a continuous session's root is: only then are scans
        // incremental, and dirty marks — what a stale baseline can hide a
        // change behind — come into play at all.
        observer.want_watching();
        assert!(observer.is_watching(), "the harness root is watched");
        observer
    }

    /// An observer over `root` with the given ignore patterns, not yet
    /// asked to watch.
    fn observer_with(root: &std::path::Path, patterns: &[&str]) -> Arc<RootObserver> {
        let cache = root.parent().expect("parent").join(format!(
            "cache-{}-{}",
            std::process::id(),
            root.file_name().and_then(|n| n.to_str()).unwrap_or("root")
        ));
        let ignores = IgnoreSet::new(
            &patterns
                .iter()
                .map(|pattern| pattern.to_string())
                .collect::<Vec<_>>(),
        )
        .expect("ignores");
        observer_for(
            ObserverKey {
                root: canonical_root(root),
                ignores: ignores.key(),
                symlink_mode: crate::scan::SymlinkMode::default(),
                max_file_size: None,
                ignore_mounts: true,
            },
            ignores,
            cache,
        )
    }

    /// The node at a root-relative path, if the snapshot records one.
    fn node_at<'s>(snapshot: &'s Snapshot, path: &str) -> Option<&'s crate::tree::Node> {
        let mut current = snapshot.root.as_ref();
        for component in path.split('/') {
            current = current.and_then(|node| node.child(component));
        }
        current
    }

    fn digest_at(snapshot: &Snapshot, path: &str) -> Option<crate::tree::Digest> {
        match &node_at(snapshot, path)?.content {
            crate::tree::Content::File { digest, .. } => Some(*digest),
            _ => None,
        }
    }

    /// Waits for `condition`, polling, for up to ten seconds.
    fn eventually(mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// Scans until the watcher is quiet, so every kernel event in flight
    /// has been consumed. Returns the last scan.
    fn settle(observer: &RootObserver) -> Snapshot {
        let (mut snapshot, mut generation) = observer.scan(None, None).expect("scans");
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(100));
            if observer.generation() == generation {
                return snapshot;
            }
            (snapshot, generation) = observer.scan(None, None).expect("scans");
        }
        panic!("the root never went quiet");
    }

    /// Writes `contents` at `path`, and waits for the watcher to report
    /// it before returning, so the next scan consumes every mark it left.
    fn write_observed(observer: &RootObserver, path: &std::path::Path, contents: &str) {
        let before = observer.generation();
        std::fs::write(path, contents).expect("writes");
        assert!(
            observer.await_change(before, Duration::from_secs(10)),
            "the write to {} was never observed",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // ── what a scan serves, and when ─────────────────────────────────

    /// Finding M-24: without a watcher, nothing advances the generation
    /// for an external write, so a cached snapshot would be served until
    /// the full walk. Every scan walks instead — both when the watcher is
    /// unavailable and when nobody asked for one (a one-shot run).
    #[test]
    fn an_unwatched_root_scans_an_unannounced_write() {
        for suppressed in [true, false] {
            let keep = tempfile::tempdir().expect("tempdir");
            let root = keep.path().join("root");
            std::fs::create_dir(&root).expect("root");
            std::fs::write(root.join("old.txt"), b"old").expect("writes");
            let observer = observer_with(&root, &[]);
            if suppressed {
                observer
                    .suppress_watching
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                observer.want_watching();
            }
            assert!(!observer.is_watching());

            let (first, _) = observer.scan(None, None).expect("scans");
            assert!(node_at(&first, "new.txt").is_none());
            std::fs::write(root.join("new.txt"), b"new").expect("writes");
            let (second, _) = observer.scan(None, None).expect("scans");
            assert!(
                node_at(&second, "new.txt").is_some(),
                "an unwatched root served a stale snapshot (suppressed: {suppressed})"
            );
        }
    }

    /// Finding M-23: the observer is shared across sessions with
    /// different entry limits, so a snapshot a generous session warmed
    /// must still be refused to a strict one — in either order.
    #[test]
    fn the_entry_limit_holds_for_a_cached_scan_in_either_order() {
        for strict_first in [true, false] {
            let keep = tempfile::tempdir().expect("tempdir");
            let root = keep.path().join("root");
            std::fs::create_dir(&root).expect("root");
            for name in ["a", "b", "c"] {
                std::fs::write(root.join(name), name).expect("writes");
            }
            let observer = harness_observer(&root);
            if strict_first {
                assert!(observer.scan(Some(1), None).is_err());
                assert!(observer.scan(None, None).is_ok());
            } else {
                settle(&observer);
            }
            let error = observer
                .scan(Some(1), None)
                .expect_err("a cached scan must honour the caller's limit");
            assert!(error.to_string().contains("limit"), "{error:#}");
            assert!(observer.scan(Some(100), None).is_ok());
        }
    }

    /// Finding M-28: a walk that fails has already taken the dirty marks
    /// it was given. The next scan must still see the change they named.
    #[test]
    fn a_failed_walk_keeps_the_changes_it_consumed() {
        let keep = tempfile::tempdir().expect("tempdir");
        let root = keep.path().join("root");
        std::fs::create_dir(&root).expect("root");
        std::fs::write(root.join("file.txt"), b"before").expect("writes");
        let observer = harness_observer(&root);
        settle(&observer);

        write_observed(&observer, &root.join("file.txt"), "after!");
        observer.invalidate(["file.txt"]);
        observer
            .fail_walk
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(observer.scan(None, None).is_err(), "the injected failure");

        let (fresh, _) = observer.scan(None, None).expect("scans");
        assert_eq!(
            digest_at(&fresh, "file.txt"),
            Some(*blake3::hash(b"after!").as_bytes()),
            "the change a failed walk consumed was lost"
        );
    }

    /// Finding M-28, the entry-limit half: a refused scan updates
    /// nothing, and loses nothing either.
    #[test]
    fn a_refused_scan_keeps_the_changes_it_consumed() {
        let keep = tempfile::tempdir().expect("tempdir");
        let root = keep.path().join("root");
        std::fs::create_dir(&root).expect("root");
        std::fs::write(root.join("file.txt"), b"before").expect("writes");
        let observer = harness_observer(&root);
        settle(&observer);

        write_observed(&observer, &root.join("file.txt"), "after!");
        observer.invalidate(["file.txt"]);
        assert!(observer.scan(Some(1), None).is_err(), "the limit refuses");

        let (fresh, _) = observer.scan(None, None).expect("scans");
        assert_eq!(
            digest_at(&fresh, "file.txt"),
            Some(*blake3::hash(b"after!").as_bytes()),
            "the change a refused scan consumed was lost"
        );
    }

    /// Finding L-27: a walk that panics must not leave the scan marked as
    /// running, or every later caller waits out the 60-second timeout.
    #[test]
    fn a_panicking_walk_does_not_hold_the_next_scan() {
        let keep = tempfile::tempdir().expect("tempdir");
        let root = keep.path().join("root");
        std::fs::create_dir(&root).expect("root");
        let observer = harness_observer(&root);
        *observer.after_walk.lock().unwrap() = Some(Box::new(|| panic!("an injected panic")));
        let panicking = Arc::clone(&observer);
        assert!(std::thread::spawn(move || panicking.scan(None, None))
            .join()
            .is_err());
        // The panic unwound through the seam's own lock.
        *observer
            .after_walk
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;

        let (sender, receiver) = std::sync::mpsc::channel();
        let second = Arc::clone(&observer);
        std::thread::spawn(move || {
            let _ = sender.send(second.scan(None, None).is_ok());
        });
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(10)),
            Ok(true),
            "a scan after a panicking walk waited for it"
        );
    }

    // ── what the watch covers ───────────────────────────────────────

    /// Finding M-25: the scanner walks into an ignored directory that
    /// holds a re-inclusion, so the watch must too, or an edit to the
    /// re-included file waits for the full walk.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_edit_under_a_re_inclusion_reaches_an_incremental_scan() {
        let keep = tempfile::tempdir().expect("tempdir");
        let root = keep.path().join("root");
        std::fs::create_dir_all(root.join("vendor/deeper")).expect("root");
        std::fs::write(root.join("vendor/keep.txt"), b"before").expect("writes");
        std::fs::write(root.join("vendor/deeper/keep.txt"), b"before").expect("writes");
        std::fs::write(root.join("vendor/other.txt"), b"ignored").expect("writes");
        let observer = observer_with(&root, &["vendor", "!vendor/keep.txt"]);
        observer.want_watching();
        assert!(observer.is_watching());
        let first = settle(&observer);
        assert!(digest_at(&first, "vendor/keep.txt").is_some());

        write_observed(&observer, &root.join("vendor/keep.txt"), "after!");
        let (fresh, _) = observer.scan(None, None).expect("scans");
        assert_eq!(
            digest_at(&fresh, "vendor/keep.txt"),
            Some(*blake3::hash(b"after!").as_bytes())
        );
    }

    /// Finding M-26: a directory that appears after the watch was built
    /// and cannot be watched leaves the observer partly watched. It must
    /// then say it is not watching, and scans must walk.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_watch_that_cannot_extend_falls_back_to_polling() {
        let keep = tempfile::tempdir().expect("tempdir");
        let root = keep.path().join("root");
        std::fs::create_dir(&root).expect("root");
        let observer = harness_observer(&root);
        settle(&observer);
        observer
            .state
            .lock()
            .unwrap()
            .watcher
            .as_ref()
            .expect("watching")
            .fail_extension
            .store(true, std::sync::atomic::Ordering::SeqCst);

        std::fs::create_dir(root.join("fresh")).expect("creates");
        assert!(
            eventually(|| !observer.is_watching()),
            "a watch that failed to extend still reports itself whole"
        );
        std::fs::write(root.join("fresh/unwatched.txt"), b"new").expect("writes");
        let (fresh, _) = observer.scan(None, None).expect("scans");
        assert!(node_at(&fresh, "fresh/unwatched.txt").is_some());
    }

    /// Finding M-27: a root moved aside and replaced by a copy left the
    /// watch on the old directory. The replacement must be noticed, and
    /// a later edit must arrive without waiting for the full walk.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_replaced_root_is_watched_afresh() {
        let keep = tempfile::tempdir().expect("tempdir");
        let root = keep.path().join("root");
        std::fs::create_dir(&root).expect("root");
        std::fs::write(root.join("file.txt"), b"before").expect("writes");
        let observer = harness_observer(&root);
        settle(&observer);

        // The copy lands only after the move has been noticed, as a
        // `cp -a` of any size does: a new root already in place when the
        // move's event arrives is re-watched by accident.
        std::fs::rename(&root, keep.path().join("root.old")).expect("moves");
        assert!(
            eventually(|| !observer.is_watching()),
            "a watch on a root moved aside still reports itself whole"
        );
        std::thread::sleep(Duration::from_millis(200));
        std::fs::create_dir(&root).expect("root");
        std::fs::write(root.join("file.txt"), b"before").expect("writes");
        let replaced = settle(&observer);
        assert!(digest_at(&replaced, "file.txt").is_some());

        write_observed(&observer, &root.join("file.txt"), "after!");
        let (fresh, _) = observer.scan(None, None).expect("scans");
        assert_eq!(
            digest_at(&fresh, "file.txt"),
            Some(*blake3::hash(b"after!").as_bytes())
        );
    }

    fn digest_of(snapshot: &Snapshot, name: &str) -> crate::tree::Digest {
        match &snapshot
            .root
            .as_ref()
            .and_then(|root| root.child(name))
            .unwrap_or_else(|| panic!("{name} missing"))
            .content
        {
            crate::tree::Content::File { digest, .. } => *digest,
            _ => panic!("{name} is not a file"),
        }
    }

    /// A write that lands *during* a scan — after the walk, before
    /// publication — must leave the published snapshot unserved: its
    /// generation predates the change, and the next scan must re-read.
    #[test]
    fn a_mid_scan_change_is_never_served_as_current() {
        let keep = tempfile::tempdir().expect("tempdir");
        let root = keep.path().join("root");
        std::fs::create_dir(&root).expect("root");
        std::fs::write(root.join("file.txt"), b"before").expect("writes");
        let observer = harness_observer(&root);

        let (first, _) = observer.scan(None, None).expect("scans");
        let before = digest_of(&first, "file.txt");

        // Arm the seam: the write and its invalidation land between the
        // walk and the publish.
        let seam_root = root.clone();
        let seam_observer = Arc::clone(&observer);
        *observer.after_walk.lock().unwrap() = Some(Box::new(move || {
            std::fs::write(seam_root.join("file.txt"), b"after!").expect("writes");
            seam_observer.invalidate(["file.txt"]);
        }));
        observer.invalidate(std::iter::empty::<&str>()); // force the next scan to walk
        let (stale, stale_generation) = observer.scan(None, None).expect("scans");
        *observer.after_walk.lock().unwrap() = None;

        // The walk predates the seam's write, so its snapshot is stale —
        // and its generation says so.
        assert_eq!(digest_of(&stale, "file.txt"), before);
        assert!(
            stale_generation < observer.generation(),
            "the stale walk must not claim the current generation"
        );
        // The gate: the next scan must NOT serve the stale snapshot. (Its
        // generation is not asserted equal to the observer's current one:
        // a live watcher — FSEvents especially — may deliver more dust
        // between the scan and the comparison, and quietness of the
        // scheduler is not the property under test.)
        let (fresh, fresh_generation) = observer.scan(None, None).expect("scans");
        assert_ne!(
            digest_of(&fresh, "file.txt"),
            before,
            "the mid-scan change was never observed"
        );
        assert!(fresh_generation > stale_generation);
    }

    /// A stale fold offered as the baseline is a hint, never an oracle: a
    /// change invalidated after the offer must still be seen.
    #[test]
    fn an_offered_baseline_cannot_hide_an_invalidated_change() {
        let keep = tempfile::tempdir().expect("tempdir");
        let root = keep.path().join("root");
        std::fs::create_dir(&root).expect("root");
        std::fs::write(root.join("file.txt"), b"before").expect("writes");
        let observer = harness_observer(&root);
        let (first, _) = observer.scan(None, None).expect("scans");

        // A transition-shaped sequence, interleaved badly on purpose: the
        // offer arrives, then the disk changes under it.
        observer.offer_baseline(first.clone(), 0);
        std::fs::write(root.join("file.txt"), b"after!").expect("writes");
        observer.invalidate(["file.txt"]);

        let (fresh, _) = observer.scan(None, None).expect("scans");
        assert_ne!(
            digest_of(&fresh, "file.txt"),
            digest_of(&first, "file.txt"),
            "an offered baseline hid an invalidated change"
        );
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 48, ..Default::default()
        })]

        /// Random interleavings of the operations two sessions actually
        /// perform. After every operation, the reference model knows the
        /// disk's true content; whenever any session scans, the snapshot
        /// must agree with the model — regardless of what offers,
        /// distrusts, cache hits, or stale baselines came before.
        ///
        /// A transition offers its fold at the generation of the lease it
        /// was built from, and that lease can be older than the baseline
        /// another session's scan has since established (finding H-3): the
        /// offering session writes one file while the other file's change
        /// was seen, and consumed, by a scan the lease predates.
        #[test]
        fn every_interleaving_scans_the_truth(
            operations in proptest::collection::vec((0u8..9, 0usize..64), 1..14)
        ) {
            let keep = tempfile::tempdir().expect("tempdir");
            let root = keep.path().join("root");
            std::fs::create_dir(&root).expect("root");
            std::fs::write(root.join("file.txt"), b"v-000").expect("writes");
            std::fs::write(root.join("own.txt"), b"o-000").expect("writes");
            let observer = harness_observer(&root);
            let mut truth = 0u32;
            let mut own = 0u32;
            let mut folds: Vec<(Snapshot, u64)> = Vec::new();
            let check = |snapshot: &Snapshot, truth: u32, own: u32, step: usize, what: &str|
                -> Result<(), proptest::test_runner::TestCaseError> {
                let expected = *blake3::hash(format!("v-{truth:03}").as_bytes()).as_bytes();
                proptest::prop_assert_eq!(
                    digest_of(snapshot, "file.txt"),
                    expected,
                    "step {}: {} disagreed with the disk",
                    step,
                    what
                );
                let expected = *blake3::hash(format!("o-{own:03}").as_bytes()).as_bytes();
                proptest::prop_assert_eq!(
                    digest_of(snapshot, "own.txt"),
                    expected,
                    "step {}: {} disagreed with the disk about own.txt",
                    step,
                    what
                );
                Ok(())
            };

            for (step, (operation, pick)) in operations.into_iter().enumerate() {
                match operation {
                    // A write, correctly announced — the transition path.
                    // The kernel's own events are let in before the
                    // announcement, so the next scan consumes every mark
                    // the write leaves: a late event would otherwise
                    // rescue a baseline that forgot the write, and hide
                    // exactly the roll-back this sweep looks for.
                    0..=1 => {
                        truth += 1;
                        let before = observer.generation();
                        std::fs::write(
                            root.join("file.txt"),
                            format!("v-{truth:03}"),
                        )
                        .expect("writes");
                        observer.await_change(before, Duration::from_secs(2));
                        std::thread::sleep(Duration::from_millis(20));
                        observer.invalidate(["file.txt"]);
                    }
                    // A scan by either of two sessions.
                    2..=4 => {
                        let (snapshot, generation) = observer.scan(None, None).expect("scans");
                        check(&snapshot, truth, own, step, "a scan")?;
                        folds.push((snapshot, generation));
                    }
                    // A stale fold offered as the next baseline.
                    5 => {
                        if let Some((fold, generation)) = folds.first().cloned() {
                            observer.offer_baseline(fold, generation);
                        }
                    }
                    // A transition problem: every session distrusts.
                    6 => observer.distrust_baseline(),
                    // A transition from a lease of any age: announce, write
                    // own.txt, announce again, and offer the lease — whose
                    // record of own.txt the announcements mark for
                    // re-reading — at the lease's own generation.
                    7 => {
                        if !folds.is_empty() {
                            let (lease, generation) = folds[pick % folds.len()].clone();
                            observer.invalidate(["own.txt"]);
                            own += 1;
                            std::fs::write(root.join("own.txt"), format!("o-{own:03}"))
                                .expect("writes");
                            observer.invalidate(["own.txt"]);
                            observer.offer_baseline(lease, generation);
                        }
                    }
                    // A verified scan must agree with the disk too.
                    _ => {
                        let (snapshot, _) = observer.scan_rehash(None, None).expect("scans");
                        check(&snapshot, truth, own, step, "a verified scan")?;
                    }
                }
            }
        }
    }
}
