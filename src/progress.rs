//! Live progress: what a session is doing *right now*.
//!
//! Recorded status (`SessionStatus`) answers "how did the last cycle end".
//! That is the wrong question while a cycle is running, and misleadingly so
//! for the cycle that matters most: a first scan of a large tree takes
//! minutes, and for all of them the recorded status is whatever preceded
//! it — commonly an error from before the problem was fixed, sitting under
//! a timestamp that only grows. The reader concludes the session is stuck.
//!
//! So a session also publishes what it is doing. The supervisor holds one
//! [`Progress`] per session, its worker and endpoints update it as the
//! cycle moves, and the control socket serves snapshots of it. Nothing is
//! written to disk: this is the live half of status, and a supervisor that
//! is not running has none of it to report — which is itself the truth.
//!
//! Updates are relaxed atomic adds on the scan's hot path. They must stay
//! that cheap: the scanner touches these counters once per entry.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// What a session is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// Between cycles: watching for changes, or sleeping out the interval.
    Waiting,
    /// Opening the connection to the destination (and, the first time,
    /// installing the agent there).
    Connecting,
    /// Reading both trees.
    Scanning,
    /// Comparing the two trees against the ancestor.
    Reconciling,
    /// Transferring content to the side that needs it.
    Staging,
    /// Applying changes to the filesystems.
    Applying,
    /// Recording the new ancestor.
    Saving,
    /// Suspended by `pause`.
    Paused,
    /// Waiting out the backoff after a failed attempt.
    Retrying,
}

impl Phase {
    /// The word status prints for this phase.
    pub fn label(self) -> &'static str {
        match self {
            Phase::Waiting => "waiting",
            Phase::Connecting => "connecting",
            Phase::Scanning => "scanning",
            Phase::Reconciling => "reconciling",
            Phase::Staging => "transferring",
            Phase::Applying => "applying",
            Phase::Saving => "saving",
            Phase::Paused => "paused",
            Phase::Retrying => "retrying",
        }
    }

    /// Whether this phase is a session actively working, as opposed to
    /// waiting between cycles. Only working phases displace the recorded
    /// status: a session sitting in `Waiting` is described by how its last
    /// cycle ended, which is exactly right.
    pub fn is_working(self) -> bool {
        !matches!(self, Phase::Waiting)
    }

    fn as_u8(self) -> u8 {
        match self {
            Phase::Waiting => 0,
            Phase::Connecting => 1,
            Phase::Scanning => 2,
            Phase::Reconciling => 3,
            Phase::Staging => 4,
            Phase::Applying => 5,
            Phase::Saving => 6,
            Phase::Paused => 7,
            Phase::Retrying => 8,
        }
    }

    fn from_u8(value: u8) -> Phase {
        match value {
            1 => Phase::Connecting,
            2 => Phase::Scanning,
            3 => Phase::Reconciling,
            4 => Phase::Staging,
            5 => Phase::Applying,
            6 => Phase::Saving,
            7 => Phase::Paused,
            8 => Phase::Retrying,
            _ => Phase::Waiting,
        }
    }
}

/// One side's scan progress, shared with whichever endpoint scans it.
#[derive(Debug, Default)]
pub struct SideProgress {
    /// Entries visited by the scan in progress.
    entries: AtomicU64,
    /// Bytes read (hashed) by the scan in progress.
    bytes: AtomicU64,
    /// Whether a scan is running on this side.
    active: AtomicBool,
    /// Whether the running scan reads the whole tree rather than adopting
    /// unchanged subtrees from a baseline. Only a full scan's counters can
    /// be measured against a whole-tree total, so only a full scan can be
    /// given an estimate.
    full: AtomicBool,
    /// When the running scan started, in milliseconds since the epoch.
    since: AtomicU64,
    /// The entry count this side's last completed full scan reported, which
    /// is what a fresh full scan is measured against. Zero when unknown.
    expected: AtomicU64,
    /// Changes this side has applied in the cycle's current transition.
    /// Only one side transitions at a time, so the cycle's count is the sum
    /// of the two.
    applied: AtomicU64,
}

impl SideProgress {
    /// Marks the start of a scan, discarding the previous one's counters.
    pub fn begin(&self, full: bool) {
        self.entries.store(0, Ordering::Relaxed);
        self.bytes.store(0, Ordering::Relaxed);
        self.full.store(full, Ordering::Relaxed);
        self.since.store(now_millis(), Ordering::Relaxed);
        self.active.store(true, Ordering::Relaxed);
    }

    /// Counts a block of visited entries and the bytes read for them.
    ///
    /// Deliberately a *block*: this number is read at most once a second,
    /// and an atomic add per entry buys nothing that an add per thousand
    /// does not. The scanner accumulates in a plain field and calls this
    /// when the block fills.
    #[inline]
    pub fn advance(&self, entries: u64, bytes: u64) {
        if entries > 0 {
            self.entries.fetch_add(entries, Ordering::Relaxed);
        }
        if bytes > 0 {
            self.bytes.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    /// Counts one applied change, as the transition applies it.
    #[inline]
    pub fn change_applied(&self) {
        self.applied.fetch_add(1, Ordering::Relaxed);
    }

    /// Marks the end of a scan. `entries` is the completed scan's own total,
    /// which becomes the baseline the next full scan is measured against —
    /// but only from a full scan, since an incremental one never visited
    /// the whole tree.
    pub fn end(&self, entries: Option<u64>) {
        self.active.store(false, Ordering::Relaxed);
        if let Some(entries) = entries {
            self.expected.store(entries, Ordering::Relaxed);
        }
    }

    /// Seeds the baseline from a previous run's recorded status, so the
    /// first scan after a restart is measured rather than merely timed.
    pub fn seed_expected(&self, entries: u64) {
        self.expected.store(entries, Ordering::Relaxed);
    }

    /// The entry count the last completed scan reported, for recording
    /// alongside the session's status so the next run starts with it. Zero
    /// when no scan has completed.
    pub fn expected_total(&self) -> u64 {
        self.expected.load(Ordering::Relaxed)
    }

    fn snapshot(&self, peer_expected: u64) -> SideSnapshot {
        let active = self.active.load(Ordering::Relaxed);
        let full = self.full.load(Ordering::Relaxed);
        let entries = self.entries.load(Ordering::Relaxed);
        let own = self.expected.load(Ordering::Relaxed);
        // A side with no history of its own borrows its peer's total. The
        // two roots are meant to hold the same tree, which makes the peer a
        // far better guess than nothing — and every estimate is presented
        // as one.
        let expected = match (own, peer_expected) {
            (0, 0) => None,
            (0, peer) => Some(peer),
            (own, _) => Some(own),
        }
        // While a scan is running, a total it has already passed is a total
        // that was wrong — most often a peer's, borrowed from a tree that
        // turned out to be nothing like this one. Saying nothing beats
        // "43,421 of ~1". A finished scan's count *equals* its total, which
        // is not the same thing and must not be erased.
        .filter(|expected| !active || *expected > entries);
        let elapsed = elapsed_since(self.since.load(Ordering::Relaxed));
        SideSnapshot {
            active,
            entries,
            bytes: self.bytes.load(Ordering::Relaxed),
            expected,
            seconds: elapsed.as_secs(),
            remaining_seconds: match (active && full, expected) {
                (true, Some(expected)) => {
                    estimate_remaining(entries, expected, elapsed).map(|left| left.as_secs())
                }
                _ => None,
            },
        }
    }
}

/// Everything a session is doing, as one live record.
#[derive(Debug)]
pub struct Progress {
    phase: AtomicU8,
    /// When the current phase began, in milliseconds since the epoch.
    phase_since: AtomicU64,
    /// The alpha side's scan progress.
    pub alpha: Arc<SideProgress>,
    /// The beta side's scan progress.
    pub beta: Arc<SideProgress>,
    /// Files transferred, and the total this cycle will transfer.
    staged: AtomicU64,
    staged_total: AtomicU64,
    /// Bytes transferred, and the total this cycle will transfer.
    staged_bytes: AtomicU64,
    staged_bytes_total: AtomicU64,
    /// Changes applied, and the total this cycle will apply.
    applied: AtomicU64,
    applied_total: AtomicU64,
}

impl Default for Progress {
    fn default() -> Progress {
        Progress {
            phase: AtomicU8::new(Phase::Waiting.as_u8()),
            phase_since: AtomicU64::new(now_millis()),
            alpha: Arc::default(),
            beta: Arc::default(),
            staged: AtomicU64::new(0),
            staged_total: AtomicU64::new(0),
            staged_bytes: AtomicU64::new(0),
            staged_bytes_total: AtomicU64::new(0),
            applied: AtomicU64::new(0),
            applied_total: AtomicU64::new(0),
        }
    }
}

impl Progress {
    /// Enters a phase, restarting its clock. Entering the phase already
    /// current leaves the clock alone, so a repeated announcement doesn't
    /// reset how long the phase has been running.
    pub fn enter(&self, phase: Phase) {
        let previous = self.phase.swap(phase.as_u8(), Ordering::Relaxed);
        if previous != phase.as_u8() {
            self.phase_since.store(now_millis(), Ordering::Relaxed);
        }
    }

    /// The current phase.
    pub fn phase(&self) -> Phase {
        Phase::from_u8(self.phase.load(Ordering::Relaxed))
    }

    /// Announces the size of the transfer a cycle is about to perform.
    pub fn begin_staging(&self, files: u64, bytes: u64) {
        self.staged.store(0, Ordering::Relaxed);
        self.staged_bytes.store(0, Ordering::Relaxed);
        self.staged_total.store(files, Ordering::Relaxed);
        self.staged_bytes_total.store(bytes, Ordering::Relaxed);
        self.enter(Phase::Staging);
    }

    /// Counts a transferred file and its bytes.
    #[inline]
    pub fn staged(&self, files: u64, bytes: u64) {
        self.staged.fetch_add(files, Ordering::Relaxed);
        self.staged_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Announces the number of changes a cycle is about to apply.
    pub fn begin_applying(&self, changes: u64) {
        self.applied.store(0, Ordering::Relaxed);
        self.alpha.applied.store(0, Ordering::Relaxed);
        self.beta.applied.store(0, Ordering::Relaxed);
        self.applied_total.store(changes, Ordering::Relaxed);
        self.enter(Phase::Applying);
    }

    /// Records that a transition finished, having applied this many
    /// changes.
    ///
    /// An endpoint that counts its changes as it applies them has already
    /// reported them; one that cannot — a remote endpoint, applying inside
    /// a single request on the far side — has reported nothing, and this
    /// is where its work becomes visible. Setting rather than adding is
    /// what makes it right for both.
    pub fn applied_reached(&self, changes: u64) {
        self.applied.store(changes, Ordering::Relaxed);
    }

    /// The changes applied so far: what the transitioning endpoint has
    /// counted, or what the finished transition reported.
    fn applied_now(&self) -> u64 {
        let counted =
            self.alpha.applied.load(Ordering::Relaxed) + self.beta.applied.load(Ordering::Relaxed);
        counted.max(self.applied.load(Ordering::Relaxed))
    }

    /// Clears the per-cycle counters and returns to waiting.
    pub fn rest(&self, phase: Phase) {
        self.alpha.end(None);
        self.beta.end(None);
        self.staged_total.store(0, Ordering::Relaxed);
        self.staged_bytes_total.store(0, Ordering::Relaxed);
        self.applied_total.store(0, Ordering::Relaxed);
        self.enter(phase);
    }

    /// Takes a snapshot for the control socket.
    pub fn snapshot(&self) -> ProgressSnapshot {
        let phase = self.phase();
        let elapsed = elapsed_since(self.phase_since.load(Ordering::Relaxed));
        let alpha = self
            .alpha
            .snapshot(self.beta.expected.load(Ordering::Relaxed));
        let beta = self
            .beta
            .snapshot(self.alpha.expected.load(Ordering::Relaxed));
        let staged = self.staged.load(Ordering::Relaxed);
        let staged_total = self.staged_total.load(Ordering::Relaxed);
        let applied = self.applied_now();
        let applied_total = self.applied_total.load(Ordering::Relaxed);
        // The phase's own estimate. Scanning runs both sides at once, so it
        // finishes with the slower of them, and a side with no estimate
        // makes the phase's unknowable rather than shorter.
        let remaining_seconds = match phase {
            Phase::Scanning => match (
                alpha.active.then_some(alpha.remaining_seconds),
                beta.active.then_some(beta.remaining_seconds),
            ) {
                (Some(Some(a)), Some(Some(b))) => Some(a.max(b)),
                (Some(estimate), None) | (None, Some(estimate)) => estimate,
                _ => None,
            },
            Phase::Staging => estimate_remaining(staged, staged_total, elapsed)
                .map(|remaining| remaining.as_secs()),
            Phase::Applying => estimate_remaining(applied, applied_total, elapsed)
                .map(|remaining| remaining.as_secs()),
            _ => None,
        };
        ProgressSnapshot {
            phase,
            seconds: elapsed.as_secs(),
            alpha,
            beta,
            staged,
            staged_total,
            staged_bytes: self.staged_bytes.load(Ordering::Relaxed),
            staged_bytes_total: self.staged_bytes_total.load(Ordering::Relaxed),
            applied,
            applied_total,
            remaining_seconds,
        }
    }
}

/// A session's live progress, as served over the control socket.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProgressSnapshot {
    /// What the session is doing.
    pub phase: Phase,
    /// How long it has been doing it.
    pub seconds: u64,
    /// The alpha side's scan.
    pub alpha: SideSnapshot,
    /// The beta side's scan.
    pub beta: SideSnapshot,
    /// Files transferred so far, of the total this cycle will transfer.
    pub staged: u64,
    pub staged_total: u64,
    /// Bytes transferred so far, of the total this cycle will transfer.
    pub staged_bytes: u64,
    pub staged_bytes_total: u64,
    /// Changes applied so far, of the total this cycle will apply.
    pub applied: u64,
    pub applied_total: u64,
    /// Estimated seconds left in this phase, when there is a total to
    /// measure against and enough has happened to extrapolate honestly.
    pub remaining_seconds: Option<u64>,
}

/// One side's scan, as served over the control socket.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SideSnapshot {
    /// Whether this side is scanning.
    pub active: bool,
    /// Entries visited so far.
    pub entries: u64,
    /// Bytes read so far.
    pub bytes: u64,
    /// The entry count this scan is expected to reach, from this side's
    /// last full scan or (failing that) its peer's. Absent on a tree
    /// neither side has ever finished scanning.
    pub expected: Option<u64>,
    /// How long this side has been scanning.
    pub seconds: u64,
    /// Estimated seconds left on this side.
    pub remaining_seconds: Option<u64>,
}

/// Extrapolates the time left from the work done so far.
///
/// Deliberately conservative about saying anything at all. An estimate
/// drawn from the first instant of a long phase is noise — the first
/// hundred files of a million-file tree say nothing about the million — and
/// a number that swings between wildly different answers is worse than no
/// number, because a reader believes it. So an estimate needs a phase that
/// has been running long enough to have a rate, enough work done to have
/// measured one, and a total that the work has not already overshot (which
/// means the total was wrong, and extrapolating from a wrong total would
/// compound it).
pub fn estimate_remaining(done: u64, expected: u64, elapsed: Duration) -> Option<Duration> {
    /// Below this, the rate is dominated by startup rather than by the work.
    const MINIMUM_ELAPSED: Duration = Duration::from_secs(3);
    /// Below this, too little has happened to divide by.
    const MINIMUM_DONE: u64 = 64;
    if elapsed < MINIMUM_ELAPSED || done < MINIMUM_DONE || expected <= done {
        return None;
    }
    let rate = done as f64 / elapsed.as_secs_f64();
    if rate <= 0.0 {
        return None;
    }
    let remaining = (expected - done) as f64 / rate;
    if !remaining.is_finite() || remaining < 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(remaining))
}

/// The current time in milliseconds since the Unix epoch.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// How long ago an epoch-millisecond timestamp was. A timestamp in the
/// future (a clock that moved) reads as no time at all rather than as a
/// wrapped-around eternity.
fn elapsed_since(millis: u64) -> Duration {
    Duration::from_millis(now_millis().saturating_sub(millis))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_estimate_needs_a_rate_worth_extrapolating() {
        // Too early in the phase to have a rate.
        assert_eq!(
            estimate_remaining(1_000, 10_000, Duration::from_millis(500)),
            None
        );
        // Too little done to have measured one.
        assert_eq!(
            estimate_remaining(10, 10_000, Duration::from_secs(30)),
            None
        );
        // No total to measure against, or one already overshot — which
        // means the total was wrong, not that the work is nearly done.
        assert_eq!(estimate_remaining(1_000, 0, Duration::from_secs(30)), None);
        assert_eq!(
            estimate_remaining(10_000, 9_000, Duration::from_secs(30)),
            None
        );
    }

    #[test]
    fn an_estimate_extrapolates_the_rate_observed_so_far() {
        // A quarter done in ten seconds: thirty seconds left.
        let remaining = estimate_remaining(2_500, 10_000, Duration::from_secs(10))
            .expect("a measured rate estimates");
        assert_eq!(remaining.as_secs(), 30);
        // Half done in the same ten: ten seconds left.
        let remaining = estimate_remaining(5_000, 10_000, Duration::from_secs(10))
            .expect("a measured rate estimates");
        assert_eq!(remaining.as_secs(), 10);
    }

    #[test]
    fn a_phase_lasts_from_when_it_was_entered_not_from_when_it_was_repeated() {
        let progress = Progress::default();
        progress.enter(Phase::Scanning);
        progress
            .phase_since
            .store(now_millis().saturating_sub(5_000), Ordering::Relaxed);
        // Re-announcing the same phase must not restart its clock: the
        // worker announces on every pass through the cycle.
        progress.enter(Phase::Scanning);
        assert!(progress.snapshot().seconds >= 5);
        // A different phase does restart it.
        progress.enter(Phase::Staging);
        assert!(progress.snapshot().seconds < 5);
    }

    #[test]
    fn a_side_without_a_history_borrows_its_peers_total() {
        let progress = Progress::default();
        // Beta has finished a full scan before; alpha never has.
        progress.beta.end(Some(1_000));
        progress.alpha.begin(true);
        let snapshot = progress.snapshot();
        assert_eq!(snapshot.alpha.expected, Some(1_000));
        // Once alpha has its own history, that is what it uses.
        progress.alpha.end(Some(4_000));
        progress.alpha.begin(true);
        assert_eq!(progress.snapshot().alpha.expected, Some(4_000));
    }

    #[test]
    fn an_incremental_scan_is_never_given_an_estimate() {
        // An incremental scan visits only what changed, so its counter
        // cannot be measured against a whole-tree total — extrapolating
        // one would promise minutes for work that takes a second.
        let progress = Progress::default();
        progress.alpha.end(Some(1_000_000));
        progress.alpha.begin(false);
        progress
            .alpha
            .since
            .store(now_millis().saturating_sub(10_000), Ordering::Relaxed);
        progress.alpha.advance(1_000, 0);
        let snapshot = progress.snapshot();
        assert_eq!(snapshot.alpha.remaining_seconds, None);
        // The same counters from a full scan do estimate.
        progress.alpha.begin(true);
        progress
            .alpha
            .since
            .store(now_millis().saturating_sub(10_000), Ordering::Relaxed);
        progress.alpha.advance(1_000, 0);
        assert!(progress.snapshot().alpha.remaining_seconds.is_some());
    }

    #[test]
    fn a_total_a_running_scan_has_passed_is_not_reported() {
        let progress = Progress::default();
        // Beta has only ever scanned an empty tree; alpha, scanning a real
        // one, would otherwise be shown as "30,000 of ~1 entries".
        progress.beta.end(Some(1));
        progress.alpha.begin(true);
        progress.alpha.advance(30_000, 0);
        assert_eq!(progress.snapshot().alpha.expected, None);

        // A scan that has *finished* reaches its total exactly, and that
        // total is what the next scan is measured against.
        progress.alpha.end(Some(30_000));
        let snapshot = progress.snapshot();
        assert_eq!(snapshot.alpha.expected, Some(30_000));
        assert!(!snapshot.alpha.active);
    }

    #[test]
    fn scanning_finishes_with_the_slower_side() {
        let progress = Progress::default();
        progress.enter(Phase::Scanning);
        for side in [&progress.alpha, &progress.beta] {
            side.end(Some(10_000));
            side.begin(true);
            side.since
                .store(now_millis().saturating_sub(10_000), Ordering::Relaxed);
        }
        // Alpha is half done (10s left); beta a quarter (30s left).
        progress.alpha.advance(5_000, 0);
        progress.beta.advance(2_500, 0);
        let snapshot = progress.snapshot();
        assert_eq!(snapshot.alpha.remaining_seconds, Some(10));
        assert_eq!(snapshot.beta.remaining_seconds, Some(30));
        assert_eq!(snapshot.remaining_seconds, Some(30));

        // A side that cannot be estimated makes the phase unknowable
        // rather than shorter: reporting alpha's ten seconds while beta
        // has an unknown number left would be a promise, not an estimate.
        progress.beta.begin(false);
        assert_eq!(progress.snapshot().remaining_seconds, None);
    }
}
