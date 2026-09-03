//! The supervisor: runs every session a configuration describes, each on its
//! own thread, with per-cycle connection healing.
//!
//! The supervisor deliberately has no notion of "reachable hosts": it simply
//! attempts each session's cycle, and a session whose destination is down
//! fails that cycle, backs off, and heals automatically the moment the host
//! answers again. This keeps the configuration authoritative at all times —
//! there is no start-time probe whose result can go stale.
//!
//! Each session records its state after every attempt as a small JSON status
//! file under `<state root>/status/<session id>.json`, written atomically.
//! The status files are the supervisor's only output channel besides its
//! (optional) log lines, and are what the `status` command reads — so status
//! can be inspected from any process, whether or not a supervisor is
//! currently running.

pub mod control;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{mode_name, EndpointTarget, SessionPlan};
use crate::endpoint::local::{EndpointOptions, LocalEndpoint};
use crate::endpoint::Endpoint;
use crate::scan::IgnoreSet;
use crate::session::{CycleReport, Session, SessionLock, SessionLockHeld};
use crate::transport::mux::AgentPool;
use crate::transport::Connection;

/// The maximum delay between attempts for a failing session.
const MAXIMUM_BACKOFF: Duration = Duration::from_secs(300);

/// The number of immediate follow-up cycles permitted when an endpoint
/// reports missing staged content.
///
/// This is a loop boundary, not a failure threshold. Reaching it returns
/// control to the worker, which checks stop, pause, reset and flush, writes
/// status, and then paces the next attempt on the watcher — so a busy tree
/// stays responsive instead of spinning inside one attempt. The follow-up
/// exists only to save a watcher round trip in the common case where the
/// content settles immediately.
pub const MAXIMUM_FOLLOW_UP_CYCLES: u32 = 5;

/// The granularity at which sleeping workers check for a stop request.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// The recorded state of one supervised session, as persisted to its status
/// file after every attempt.
/// One side of a conflict, as of the cycle that reported it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConflictSide {
    /// Whether anything exists at the path on this side.
    pub present: bool,
    /// The kind of entry: "file", "directory", "symlink", or "" when absent.
    pub kind: String,
    /// The file's size in bytes, for a file.
    pub size: u64,
    /// The file's modification time in seconds since the epoch, for a file.
    pub mtime_seconds: i64,
}

/// A conflict, with what each side held.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConflictDetail {
    /// The root-relative path.
    pub path: String,
    /// Alpha's side.
    pub alpha: ConflictSide,
    /// Beta's side.
    pub beta: ConflictSide,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionStatus {
    /// The group the session belongs to.
    pub group: String,
    /// The destination label (host or local path).
    pub host: String,
    /// The alpha root.
    pub alpha: String,
    /// The beta specification.
    pub beta: String,
    /// The synchronization mode name.
    pub mode: String,
    /// The session state: `synchronized`, `conflicts`, `blocked`,
    /// `paused`, `halted`, `unreachable`, or `errored`.
    pub state: String,
    /// The number of cycles completed since the supervisor started this
    /// session.
    pub cycles: u64,
    /// The transitions applied to alpha and beta by the most recent cycle.
    pub last_alpha_transitions: usize,
    /// The transitions applied to beta by the most recent cycle.
    pub last_beta_transitions: usize,
    /// The root paths of any conflicts reported by the most recent cycle.
    pub conflicts: Vec<String>,
    /// What each side held at each conflict, as of the cycle that reported
    /// it — so `conflicts` can describe the sides without connecting to
    /// them. Absent in records written before this field existed.
    #[serde(default)]
    pub conflict_details: Vec<ConflictDetail>,
    /// Paths the most recent cycle could not read or write, as `side path:
    /// message` strings. The cycle itself succeeded; these are what it
    /// could not carry.
    ///
    /// Written as `problems` before it had a name that said what it was;
    /// the alias keeps existing status files readable.
    #[serde(alias = "problems")]
    pub blocked: Vec<String>,
    /// The failure that ended the most recent attempt, if it failed.
    pub error: Option<String>,
    /// When this status was recorded, in seconds since the Unix epoch.
    pub updated_at: u64,
    /// The entry count alpha's last completed scan reported, which is what
    /// the next run's first scan is measured against for an estimate.
    /// Absent in records written before this field existed.
    #[serde(default)]
    pub alpha_entries: u64,
    /// The entry count beta's last completed scan reported.
    #[serde(default)]
    pub beta_entries: u64,
}

/// The outcome of one session's participation in a single-pass run.
#[derive(Debug)]
pub struct SessionOutcome {
    /// The session's display name (`group@host`).
    pub display: String,
    /// The result: a digest of the work performed, or the failure message.
    pub result: Result<CycleDigest, String>,
}

/// A digest of the cycles a session ran during one attempt (a cycle plus any
/// missing-staged-content follow-ups).
#[derive(Clone, Copy, Debug, Default)]
pub struct CycleDigest {
    /// The number of cycles run.
    pub cycles: u64,
    /// The total transitions applied to alpha.
    pub alpha_transitions: usize,
    /// The total transitions applied to beta.
    pub beta_transitions: usize,
    /// The number of conflicts reported by the final cycle.
    pub conflicts: usize,
    /// The number of problems reported by the final cycle.
    pub problems: usize,
}

/// A supervisor over a set of session plans.
pub struct Supervisor {
    /// The sessions to run.
    plans: Vec<SessionPlan>,
    /// The state root, holding per-session state and status files.
    state_root: PathBuf,
    /// Whether or not to log per-cycle activity to standard output.
    verbose: bool,
    /// The agent connection pool: sessions on the same host share one
    /// connection, each as its own channel.
    pool: AgentPool,
    /// What to run when sessions need attention. Nothing is observed or
    /// timed when nothing is configured to run.
    alerts: crate::alerts::AlertPlan,
}

impl Supervisor {
    /// Creates a supervisor over the provided plans, with state and status
    /// kept under the provided root.
    pub fn new(plans: Vec<SessionPlan>, state_root: PathBuf, verbose: bool) -> Supervisor {
        Supervisor {
            plans,
            state_root,
            verbose,
            pool: AgentPool::default(),
            alerts: crate::alerts::AlertPlan::default(),
        }
    }

    /// Adopts an alert plan, so that sessions needing attention are
    /// announced rather than merely recorded.
    pub fn with_alerts(mut self, alerts: crate::alerts::AlertPlan) -> Supervisor {
        self.alerts = alerts;
        self
    }

    /// Runs one attempt of every session in parallel and returns their
    /// outcomes (in plan order). Individual failures are captured in the
    /// outcomes rather than aborting the run, and a failure to *record* a
    /// successful attempt's status is itself a failure — automation reading
    /// the exit code must be able to trust that the status files reflect
    /// what happened.
    pub fn run_once(&self) -> Vec<SessionOutcome> {
        std::thread::scope(|scope| {
            let handles: Vec<_> = self
                .plans
                .iter()
                .map(|plan| {
                    scope.spawn(move || {
                        let mut worker =
                            Worker::new(plan, &self.state_root, &self.pool, self.verbose);
                        let result = worker.attempt();
                        let recorded = worker.conclude(&result);
                        let result = match (result, recorded) {
                            // A single pass must not report success while
                            // content is known to be missing: the watching
                            // supervisor can wait for a busy tree to settle,
                            // but a one-shot run has nothing left to wait
                            // with, and automation reads its exit status.
                            (Ok((_, report)), Ok(())) if report.missing_staged_files => {
                                Err("staged content was still missing when the pass ended; \
                                     source content is changing faster than it can be \
                                     transferred"
                                    .to_owned())
                            }
                            (Ok((digest, _)), Ok(())) => Ok(digest),
                            (Ok(_), Err(record_error)) => Err(format!(
                                "synchronized, but unable to record status: {record_error:#}"
                            )),
                            (Err(error), Ok(())) => Err(format!("{error:#}")),
                            (Err(error), Err(record_error)) => Err(format!(
                                "{error:#}; additionally, unable to record status: \
                                 {record_error:#}"
                            )),
                        };
                        SessionOutcome {
                            display: plan.display(),
                            result,
                        }
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("session worker panicked"))
                .collect()
        })
    }

    /// Runs every session continuously until `stop` becomes true: each
    /// session cycles on its own interval, backs off (exponentially, with
    /// per-session jitter so a recovering host isn't hit by every session at
    /// once) while its destination is failing, and reconnects (healing the
    /// session) on the first attempt after a failure.
    ///
    /// The stop flag is honored between attempts and during sleeps; it
    /// cannot interrupt a cycle already in flight, so returning waits for
    /// in-flight cycles to finish. SSH keepalives bound how long a dead
    /// network can hold one; for the CLI, process termination remains the
    /// hard stop.
    pub fn run_watch(&self, stop: &AtomicBool) -> Result<()> {
        // One supervisor per state root: a second one's workers would all
        // lose their session locks anyway, but it would still capture the
        // control socket — commands would land in a supervisor that owns
        // nothing. Refuse up front instead (and let the caller exit
        // non-zero — a refused supervisor is a failure, not a quiet no-op).
        let _supervisor_lock = SessionLock::acquire(self.state_root.join("supervisor"))
            .context("unable to supervise")?;

        // Every session gets a control-flag block; the registry shares them
        // with the control socket's server thread.
        let controls: Vec<Arc<control::WorkerControl>> = self
            .plans
            .iter()
            .map(|_| Arc::<control::WorkerControl>::default())
            .collect();
        // Every session also gets a progress record, seeded from what its
        // last run recorded so that the first scan after a restart can be
        // measured rather than merely timed.
        let progresses: Vec<Arc<crate::progress::Progress>> = self
            .plans
            .iter()
            .map(|plan| {
                let progress = Arc::<crate::progress::Progress>::default();
                if let Ok(Some(status)) = read_status(&self.state_root, &plan.identifier()) {
                    if status.alpha_entries > 0 {
                        progress.alpha.seed_expected(status.alpha_entries);
                    }
                    if status.beta_entries > 0 {
                        progress.beta.seed_expected(status.beta_entries);
                    }
                }
                progress
            })
            .collect();
        let registry = control::Registry {
            entries: self
                .plans
                .iter()
                .zip(&controls)
                .zip(&progresses)
                .map(|((plan, flags), progress)| control::Entry {
                    group: plan.group.clone(),
                    host: plan.host.clone(),
                    control: flags.clone(),
                    progress: progress.clone(),
                })
                .collect(),
        };
        let listener = match control::bind(&self.state_root) {
            Ok(listener) => Some(listener),
            Err(error) => {
                // Control is a convenience, not a prerequisite for syncing.
                eprintln!("control socket unavailable: {error:#}");
                None
            }
        };

        // The alerter watches what the workers publish. It lives outside
        // the thread scope because both it and the workers borrow this.
        let published: Vec<Arc<Mutex<Option<SessionStatus>>>> =
            self.plans.iter().map(|_| Arc::default()).collect();

        std::thread::scope(|scope| {
            if let Some(listener) = listener {
                let registry = &registry;
                scope.spawn(move || control::serve(listener, registry, stop));
            }
            // The log is a file nobody else prunes: launchd and systemd
            // both write to it forever and neither rotates it.
            scope.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match crate::service::rotate_log() {
                        Ok(true) => eprintln!("the service log reached its cap and was rotated"),
                        Ok(false) => {}
                        Err(error) => eprintln!("unable to rotate the service log: {error:#}"),
                    }
                    sleep_interruptible(LOG_CHECK_INTERVAL, stop);
                }
            });

            if self.alerts.is_configured() {
                let plans = &self.plans;
                let published = &published;
                let state_root = self.state_root.as_path();
                let alerts = self.alerts.clone();
                scope.spawn(move || watch_alerts(plans, published, state_root, alerts, stop));
            }

            for (index, plan) in self.plans.iter().enumerate() {
                let flags = controls[index].clone();
                let progress = progresses[index].clone();
                let published = published[index].clone();
                scope.spawn(move || {
                    // Stagger the first attempts so a large fan-out doesn't
                    // open every connection in the same instant (bounded, so
                    // small deployments and fast test intervals barely
                    // notice it).
                    let stagger = Duration::from_millis(100)
                        .saturating_mul(index as u32)
                        .min(Duration::from_secs(3))
                        .min(plan.interval);
                    sleep_interruptible(stagger, stop);

                    let mut worker = Worker::new(plan, &self.state_root, &self.pool, self.verbose);
                    worker.progress = progress;
                    worker.published = Some(published);
                    let identifier = plan.identifier();
                    let mut failures = 0u32;
                    while !stop.load(Ordering::Relaxed) {
                        if flags.paused.load(Ordering::Relaxed) {
                            worker.hold_paused(&flags, stop);
                            continue;
                        }
                        if flags.reset.swap(false, Ordering::Relaxed) {
                            worker.reset();
                        }
                        if flags.verify.swap(false, Ordering::Relaxed) {
                            worker.verify_pending = true;
                        }
                        let result = worker.attempt();
                        let failed = result.is_err();
                        if let Err(error) = worker.conclude(&result) {
                            eprintln!("[{}] unable to record status: {error:#}", plan.display());
                        }
                        if failed {
                            failures = failures.saturating_add(1);
                            let delay = backoff_delay(
                                plan.interval,
                                failures,
                                jitter_percent(&identifier, failures),
                            );
                            worker.progress.rest(crate::progress::Phase::Retrying);
                            sleep_flagged(delay, stop, &flags);
                        } else {
                            failures = 0;
                            worker.progress.rest(crate::progress::Phase::Waiting);
                            worker.await_activity(plan.interval, stop, &flags);
                        }
                    }
                });
            }
        });
        Ok(())
    }
}

/// The per-session worker state: the live session (present while the
/// connection is healthy) and the running cycle count.
struct Worker<'a> {
    /// The session's plan.
    plan: &'a SessionPlan,
    /// The state root.
    state_root: &'a Path,
    /// The shared agent connection pool.
    pool: &'a AgentPool,
    /// Whether or not to log activity.
    verbose: bool,
    /// The live session, if the last attempt (if any) succeeded.
    session: Option<Session>,
    /// The number of cycles completed since this worker started.
    cycles: u64,
    /// A verify request awaiting the next cycle (survives reconnection).
    verify_pending: bool,
    /// What this session is doing, published to the control socket. A
    /// worker that nobody is watching — a single pass, or a test — still
    /// has one; it is simply never read.
    progress: Arc<crate::progress::Progress>,
    /// The conflicts and blocked paths the last cycle reported, so a set
    /// that has not changed is not written out again.
    reported: Option<(Vec<String>, Vec<String>)>,
    /// Where the last recorded status is shared with the alerter. Absent
    /// when nothing is alerting, so a single pass costs nothing.
    published: Option<Arc<Mutex<Option<SessionStatus>>>>,
}

impl<'a> Worker<'a> {
    /// Creates a worker for a plan.
    fn new(
        plan: &'a SessionPlan,
        state_root: &'a Path,
        pool: &'a AgentPool,
        verbose: bool,
    ) -> Worker<'a> {
        Worker {
            plan,
            state_root,
            pool,
            verbose,
            session: None,
            cycles: 0,
            verify_pending: false,
            progress: Arc::default(),
            published: None,
            reported: None,
        }
    }

    /// Runs one attempt: connect if not connected, then run a cycle (plus
    /// bounded follow-ups while staged content is reported missing).
    ///
    /// A failed attempt's session is *not* dropped here but in
    /// [`conclude`](Worker::conclude), after the status has been recorded:
    /// dropping it releases the state lock, and releasing before recording
    /// would let a waiting successor acquire the session and publish a newer
    /// status that this worker's stale error write then overwrites.
    fn attempt(&mut self) -> Result<(CycleDigest, CycleReport)> {
        let result = (|| {
            if self.session.is_none() {
                // Connecting is its own phase because it is its own wait:
                // the first connection to a host installs the agent there,
                // and an unreachable one is where a session sits until it
                // times out.
                self.progress.enter(crate::progress::Phase::Connecting);
                let mut session = connect(self.plan, self.state_root, self.pool)?;
                session.set_progress(self.progress.clone());
                self.session = Some(session);
            }
            let session = self.session.as_mut().expect("the session was just created");
            if std::mem::take(&mut self.verify_pending) {
                session.request_verify();
            }
            run_cycles(session)
        })();
        if let Ok((digest, _)) = &result {
            self.cycles += digest.cycles;
        }
        result
    }

    /// Concludes an attempt: records its status (while any held lock is
    /// still ours), then, on failure, drops the session so the next attempt
    /// reconnects from scratch. That closes this session's channels; a
    /// pooled agent process outlives them and is reaped only once its last
    /// channel and handle are gone.
    fn conclude(&mut self, result: &Result<(CycleDigest, CycleReport)>) -> Result<()> {
        let recorded = self.record(result);
        if result.is_err() {
            self.session = None;
        }
        recorded
    }

    /// Waits for the next reason to cycle: a change signaled by either
    /// endpoint, a control wake (flush/pause/resume/reset), the interval
    /// heartbeat, or a stop request — whichever comes first. A change gets a
    /// short settle delay so a burst of writes lands in one cycle.
    fn await_activity(
        &mut self,
        interval: Duration,
        stop: &AtomicBool,
        flags: &control::WorkerControl,
    ) {
        // The ceiling on coalescing a burst, and the slice of quiet that
        // ends it early. An isolated write now costs QUIET, not SETTLE.
        const SETTLE: Duration = Duration::from_millis(100);
        const QUIET: Duration = Duration::from_millis(20);
        let deadline = std::time::Instant::now() + interval;
        while !stop.load(Ordering::Relaxed) {
            if flags.wake.swap(false, Ordering::Relaxed)
                || flags.paused.load(Ordering::Relaxed)
                || flags.reset.load(Ordering::Relaxed)
            {
                return;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return;
            }
            let slice = remaining.min(Duration::from_millis(500));
            match self.session.as_mut() {
                Some(session) => match session.await_change(slice) {
                    Ok(true) => {
                        session.settle(SETTLE.min(interval), QUIET);
                        return;
                    }
                    Ok(false) => {}
                    // The connection is failing; let the next attempt
                    // surface (and heal) it.
                    Err(_) => return,
                },
                None => std::thread::sleep(slice.min(STOP_POLL_INTERVAL)),
            }
        }
    }

    /// Holds the worker while its session is paused: the paused state is
    /// recorded once, and the hold ends on resume (or any other control
    /// wake) or stop.
    fn hold_paused(&mut self, flags: &control::WorkerControl, stop: &AtomicBool) {
        // Dropping the session releases its state lock and shuts down any
        // agent — a paused session holds no resources and doesn't block
        // other processes.
        self.session = None;
        self.progress.rest(crate::progress::Phase::Paused);
        self.record_state("paused");
        if self.verbose {
            println!("[{}] paused", self.plan.display());
        }
        while !stop.load(Ordering::Relaxed) && flags.paused.load(Ordering::Relaxed) {
            std::thread::sleep(STOP_POLL_INTERVAL);
        }
    }

    /// Performs a session reset: the ancestor is deleted under the session
    /// state lock, so the next cycle reconciles with no baseline and merges
    /// both sides additively.
    fn reset(&mut self) {
        // Release our own session (and its lock) first, then reacquire the
        // lock bare for the deletion: the ancestor must never be removed
        // while any session — ours or another process's — could be using
        // it. If another process holds the lock, the reset is refused
        // rather than raced.
        self.session = None;
        let state_directory = self
            .state_root
            .join("sessions")
            .join(self.plan.identifier());
        let _lock = match SessionLock::acquire(state_directory.clone()) {
            Ok(lock) => lock,
            Err(error) => {
                eprintln!("[{}] unable to reset: {error:#}", self.plan.display());
                return;
            }
        };
        if let Err(error) =
            crate::session::ancestor::AncestorStore::reset(&state_directory.join("ancestor"))
        {
            eprintln!(
                "[{}] unable to reset the ancestor: {error:#}",
                self.plan.display()
            );
            return;
        }
        if self.verbose {
            println!(
                "[{}] reset: the next cycle merges both sides additively",
                self.plan.display()
            );
        }
    }

    /// Records a bare state (such as `paused`) to the status file.
    fn record_state(&self, state: &str) {
        let status = SessionStatus {
            group: self.plan.group.clone(),
            host: self.plan.host.clone(),
            alpha: self.plan.alpha_spec.clone(),
            beta: self.plan.beta_spec(),
            mode: mode_name(self.plan.mode).to_owned(),
            state: state.to_owned(),
            cycles: self.cycles,
            last_alpha_transitions: 0,
            last_beta_transitions: 0,
            conflicts: Vec::new(),
            conflict_details: Vec::new(),
            blocked: Vec::new(),
            error: None,
            updated_at: epoch_seconds(),
            alpha_entries: self.progress.alpha.expected_total(),
            beta_entries: self.progress.beta.expected_total(),
        };
        self.publish(&status);
        if let Err(error) = write_status(self.state_root, &self.plan.identifier(), &status) {
            eprintln!(
                "[{}] unable to record status: {error:#}",
                self.plan.display()
            );
        }
    }

    /// Records an attempt's result to the session's status file (and, when
    /// verbose, to standard output), reporting a failure to persist the
    /// status so that callers can surface it.
    ///
    /// A lock conflict is the one failure that is *not* recorded: the
    /// session's shared state — its status file included — belongs to the
    /// lock holder, and writing "another process is synchronizing" over the
    /// holder's live status would replace the truth with a complaint about
    /// having lost the race to tell it.
    fn record(&mut self, result: &Result<(CycleDigest, CycleReport)>) -> Result<()> {
        let display = self.plan.display();
        if let Err(error) = result {
            if error.downcast_ref::<SessionLockHeld>().is_some() {
                if self.verbose {
                    eprintln!("[{display}] skipped: {error:#}");
                }
                return Ok(());
            }
        }
        let mut status = SessionStatus {
            group: self.plan.group.clone(),
            host: self.plan.host.clone(),
            alpha: self.plan.alpha_spec.clone(),
            beta: self.plan.beta_spec(),
            mode: mode_name(self.plan.mode).to_owned(),
            state: "synchronized".into(),
            cycles: self.cycles,
            last_alpha_transitions: 0,
            last_beta_transitions: 0,
            conflicts: Vec::new(),
            conflict_details: Vec::new(),
            blocked: Vec::new(),
            error: None,
            updated_at: epoch_seconds(),
            alpha_entries: self.progress.alpha.expected_total(),
            beta_entries: self.progress.beta.expected_total(),
        };
        match result {
            Ok((digest, report)) => {
                status.last_alpha_transitions = digest.alpha_transitions;
                status.last_beta_transitions = digest.beta_transitions;
                status.conflicts = report
                    .conflicts
                    .iter()
                    .map(|conflict| conflict.root.clone())
                    .collect();
                status.conflict_details = report.conflicts.iter().map(conflict_detail).collect();
                status.blocked = blocked_paths(report);
                // The headline word, for a reader who wants one. It is
                // lossy by construction — a session can be in conflict
                // *and* have blocked paths — so the counts themselves are
                // what `status` prints and what alerts are drawn from.
                status.state = if !status.conflicts.is_empty() {
                    "conflicts".into()
                } else if !status.blocked.is_empty() {
                    "blocked".into()
                } else {
                    "synchronized".into()
                };
                if self.verbose {
                    if digest.alpha_transitions > 0 || digest.beta_transitions > 0 {
                        println!(
                            "[{display}] synchronized: {} change(s) to alpha, {} change(s) to beta",
                            digest.alpha_transitions, digest.beta_transitions
                        );
                    }
                    // Only what changed. A session holding the same
                    // seven hundred conflicts writes them once, not on
                    // every cycle for as long as they last: that one
                    // statement was 97% of a 176 MB log, restating a list
                    // that had not moved in three days.
                    report_changes(
                        &display,
                        self.reported.as_ref(),
                        &status.conflicts,
                        &status.blocked,
                    );
                }
            }
            Err(error) => {
                // Decided from the error's *type*, once, here — not by
                // searching its prose at display time, where rewording a
                // message silently reclassified a session.
                status.state = if error.downcast_ref::<crate::session::SafetyHalt>().is_some() {
                    "halted"
                } else if error
                    .downcast_ref::<crate::endpoint::remote::Unreachable>()
                    .is_some()
                {
                    "unreachable"
                } else {
                    "errored"
                }
                .into();
                status.error = Some(format!("{error:#}"));
                if self.verbose {
                    eprintln!("[{display}] error: {error:#}");
                }
            }
        }
        self.reported = Some((status.conflicts.clone(), status.blocked.clone()));
        self.publish(&status);
        write_status(self.state_root, &self.plan.identifier(), &status)
    }

    /// Shares a status with the alerter.
    fn publish(&self, status: &SessionStatus) {
        if let Some(published) = &self.published {
            *published.lock().unwrap_or_else(|error| error.into_inner()) = Some(status.clone());
        }
    }
}

/// Runs one cycle plus bounded follow-ups while staged content is reported
/// missing. Exhausting the cap with content *still* missing is a failure,
/// not a quiet success: the destination is churning faster than content can
/// be transferred, and automation must not read that as "synchronized".
fn run_cycles(session: &mut Session) -> Result<(CycleDigest, CycleReport)> {
    let mut digest = CycleDigest::default();
    let mut previously_missing: std::collections::HashMap<String, crate::tree::Digest> =
        std::collections::HashMap::new();
    loop {
        let report = session.run_cycle()?;
        digest.cycles += 1;
        digest.alpha_transitions += report.alpha_transitions;
        digest.beta_transitions += report.beta_transitions;
        digest.conflicts = report.conflicts.len();
        digest.problems = blocked_paths(&report).len();
        if !report.missing_staged_files {
            return Ok((digest, report));
        }

        // The same path at the same digest, missing twice running, is not a
        // file being rewritten: a rewrite changes the digest, because the
        // follow-up rescanned and asked for the new content. Staging is
        // failing to produce this content at all — a wiped staging
        // directory, a cleaner, a supply defect — and that is worth an
        // error and the backoff that follows one.
        if report
            .missing_staged
            .iter()
            .any(|request| previously_missing.get(&request.path) == Some(&request.digest))
        {
            bail!(
                "staged content failed to appear across consecutive cycles; staging is \
                 failing rather than racing the source"
            );
        }

        // Otherwise the source is simply being written faster than it can be
        // transferred. That is a busy tree, not a broken session: return the
        // work that was done, with the flag still set. The caller decides
        // what it means — a one-shot run treats it as incomplete, while a
        // watching supervisor keeps its session, its watcher and its warm
        // state, and lets the next change schedule the next attempt.
        if digest.cycles > MAXIMUM_FOLLOW_UP_CYCLES as u64 {
            return Ok((digest, report));
        }
        previously_missing = report
            .missing_staged
            .iter()
            .map(|request| (request.path.clone(), request.digest))
            .collect();
    }
}

/// Builds a live session for a plan: alpha and beta endpoints (each local
/// or remote) and the persisted session state under the state root.
fn connect(plan: &SessionPlan, state_root: &Path, pool: &AgentPool) -> Result<Session> {
    let identifier = plan.identifier();
    let state_directory = state_root.join("sessions").join(&identifier);

    // The lock comes first: a conflicting session must be discovered before
    // any endpoint work happens, not after spawning SSH and handshaking
    // with a remote agent — endpoint construction is expensive, can block
    // on the network, and has remote side effects.
    let lock = SessionLock::acquire(state_directory.clone())?;
    // The pair lock is independent of the state root, so two supervisors
    // pointed at different state directories cannot own the same trees.
    let pair_lock =
        crate::session::EndpointPairLock::acquire(&plan.alpha_identity, &plan.beta_identity)?;
    let (alpha, beta) = open_endpoints(plan, state_root, pool)?;
    let mut session = Session::with_lock(alpha, beta, plan.mode, lock)?;
    session.hold(pair_lock);
    session.set_power_durability(plan.power_durability);
    Ok(session)
}

/// Opens a plan's two endpoints without taking its session lock.
///
/// The supervisor takes the lock first and then calls this; `resolve` and
/// `diff` call it alone, because they operate *beside* a running session —
/// reading and writing individual files through the same endpoints, in the
/// way any other program writing to the tree would — rather than owning
/// the pair. The identity check is the same: a root that no longer
/// resolves to the tree it was planned against is refused.
pub fn open_endpoints(
    plan: &SessionPlan,
    state_root: &Path,
    pool: &AgentPool,
) -> Result<(Box<dyn Endpoint + Send>, Box<dyn Endpoint + Send>)> {
    let identifier = plan.identifier();
    let state_directory = state_root.join("sessions").join(&identifier);

    // The identity was resolved when the plan was built; between then and
    // now a symlink along the path can have been retargeted, and connecting
    // would bind whatever tree the path reaches *today* to the ancestor of
    // the tree it reached *then* — provenance for the wrong root, which is
    // how a deliberate revert gets silently overwritten. Refuse instead.
    // The verified resolution is also the path the endpoint will use, so a
    // symlink retargeted after this check cannot redirect the endpoint: the
    // check and the use are one resolution, not two.
    let mut frozen: [Option<PathBuf>; 2] = [None, None];
    for (index, (target, planned, side)) in [
        (&plan.alpha, &plan.alpha_identity, "alpha"),
        (&plan.beta, &plan.beta_identity, "beta"),
    ]
    .into_iter()
    .enumerate()
    {
        if let EndpointTarget::Local(path) = target {
            let resolved = crate::paths::resolve_for_identity(path);
            if resolved.to_string_lossy() != planned.as_str() {
                anyhow::bail!(
                    "the {side} root {} no longer resolves to the tree it was planned \
                     against ({planned} became {resolved}); refusing to attach its \
                     session state to a different tree — restart to replan",
                    path.display(),
                    resolved = resolved.display()
                );
            }
            // A missing alpha stays an error: combined with a mirroring
            // mode, a mistyped source path would otherwise read as "the
            // source is empty" and empty the destination. A missing beta is
            // a legitimate state a transition resolves by creating it.
            if side == "alpha" && !resolved.exists() {
                anyhow::bail!("alpha root {} does not exist", resolved.display());
            }
            frozen[index] = Some(resolved);
        }
    }

    let endpoint = |target: &EndpointTarget, side: &str| -> Result<Box<dyn Endpoint + Send>> {
        match target {
            EndpointTarget::Local(path) => {
                // The frozen resolution from the identity check above —
                // never a second canonicalization of the original spelling,
                // which would reopen the window the check just closed.
                let root = frozen[if side == "alpha" { 0 } else { 1 }]
                    .clone()
                    .unwrap_or_else(|| path.clone());
                let staging = crate::endpoint::local::staging_root_for(
                    plan.staging,
                    &root,
                    state_directory.join(format!("staging-{side}")),
                    &identifier,
                    side,
                )?;
                Ok(Box::new(LocalEndpoint::new(
                    root,
                    staging,
                    EndpointOptions {
                        ignores: IgnoreSet::new(&plan.ignores)?,
                        symlink_mode: plan.symlink_mode,
                        file_mode: plan.file_mode,
                        directory_mode: plan.directory_mode,
                        max_file_size: plan.max_file_size,
                        max_entry_count: plan.max_entry_count,
                        default_owner: plan.default_owner.clone(),
                        default_group: plan.default_group.clone(),
                    },
                )?))
            }
            EndpointTarget::Remote {
                destination,
                path,
                agent_command,
            } => {
                let initialize = crate::protocol::Initialize {
                    root: path.clone(),
                    session: identifier.clone(),
                    ignores: plan.ignores.clone(),
                    symlink_mode: plan.symlink_mode,
                    file_mode: plan.file_mode,
                    directory_mode: plan.directory_mode,
                    side: side.to_owned(),
                    staging: plan.staging,
                    max_file_size: plan.max_file_size,
                    max_entry_count: plan.max_entry_count,
                    default_owner: plan.default_owner.clone(),
                    default_group: plan.default_group.clone(),
                };
                // Sessions sharing a spawn command share one pooled
                // connection, each as its own channel — one SSH process per
                // host, however many sessions (and sides) target it.
                match agent_command {
                    Some(argv) => Ok(Box::new(crate::endpoint::remote::connect_pooled(
                        pool, None, argv, initialize,
                    )?)),
                    None => {
                        let argv = Connection::ssh_argv(
                            destination,
                            Some(&crate::transport::install::versioned_remote_command()),
                        );
                        Ok(Box::new(crate::endpoint::remote::connect_pooled(
                            pool,
                            Some(destination),
                            &argv,
                            initialize,
                        )?))
                    }
                }
            }
        }
    };

    let alpha = endpoint(&plan.alpha, "alpha")?;
    let beta = endpoint(&plan.beta, "beta")?;
    Ok((alpha, beta))
}

/// Everything `status` knows, as one document — the seam any user
/// interface builds on. `status --json` prints it; `autobahn tray` reads
/// it directly. The `version` field moves when the shape does.
#[derive(Clone, Debug, Serialize)]
pub struct StatusReport {
    /// The schema version of this document.
    pub version: u32,
    /// Whether a supervisor is answering on the control socket.
    pub supervisor_running: bool,
    /// The login service: "not-installed", "stopped", "running", or
    /// "unknown".
    pub service: String,
    /// The configured groups, in configuration order.
    pub groups: Vec<GroupReport>,
}

/// One group's report.
#[derive(Clone, Debug, Serialize)]
pub struct GroupReport {
    pub name: String,
    /// The alpha root as written in the configuration.
    pub alpha: String,
    pub sessions: Vec<SessionReport>,
}

/// One session's report: its plan, and its recorded status if any.
#[derive(Clone, Debug, Serialize)]
pub struct SessionReport {
    /// The destination label status prints (a host, or a local path).
    pub host: String,
    /// The full destination specification.
    pub beta: String,
    pub mode: String,
    /// "never-run", "synchronized", "conflicts", "blocked", "unreachable",
    /// "halted", or "errored".
    pub state: String,
    pub cycles: u64,
    /// Seconds since the status was recorded, or null when never run.
    pub age_seconds: Option<u64>,
    pub conflicts: Vec<ConflictDetail>,
    /// Paths the cycle could not read or write.
    #[serde(rename = "blocked")]
    pub blocked: Vec<String>,
    pub error: Option<String>,
    /// What the session is doing right now, when a supervisor is running
    /// and reports it. Everything else in this record is what the last
    /// cycle left behind; this is the only live field.
    pub progress: Option<crate::progress::ProgressSnapshot>,
}

/// Writes what changed about a session's conflicts and blocked paths.
///
/// A cycle used to write out both lists in full, every time. That is
/// correct and unreadable: a session holding seven hundred conflicts wrote
/// seven hundred lines twice a minute for as long as they lasted, and one
/// statement grew to 97% of a 176 MB log restating a list that had not
/// moved in three days. What is worth writing down is the change.
///
/// The first report after a start has nothing to compare against, so it
/// says how many there are rather than naming them all.
fn report_changes(
    display: &str,
    previous: Option<&(Vec<String>, Vec<String>)>,
    conflicts: &[String],
    blocked: &[String],
) {
    /// How many new entries are named before the rest become a count.
    const NAMED: usize = 5;

    let Some((was_conflicts, was_blocked)) = previous else {
        // A fresh worker. Counts, so a restart does not reprint
        // everything a session has been holding all along.
        if !conflicts.is_empty() {
            eprintln!("[{display}] {} conflict(s)", conflicts.len());
        }
        if !blocked.is_empty() {
            eprintln!("[{display}] {} blocked path(s)", blocked.len());
        }
        return;
    };

    for (what, now, before) in [
        ("conflict", conflicts, was_conflicts),
        ("blocked", blocked, was_blocked),
    ] {
        let appeared: Vec<&String> = now.iter().filter(|entry| !before.contains(entry)).collect();
        let cleared = before.iter().filter(|entry| !now.contains(entry)).count();
        for entry in appeared.iter().take(NAMED) {
            eprintln!("[{display}] {what}: {entry}");
        }
        if appeared.len() > NAMED {
            eprintln!("[{display}] {what}: and {} more", appeared.len() - NAMED);
        }
        if cleared > 0 {
            eprintln!("[{display}] {what}: {cleared} cleared, {} left", now.len());
        }
    }
}

/// Reports every session whose ancestor this build cannot read.
///
/// Only each checkpoint's header is read, so this costs one short read per
/// session and runs before any cycle does. An ancestor is never discarded
/// silently, so a format this build does not know stops the session that
/// owns it — and a session that stops hours later, on a timer, is found by
/// nobody. Reported at startup, it is found while someone is watching.
pub fn unreadable_ancestors(plans: &[SessionPlan], state_root: &Path) -> Vec<(String, String)> {
    plans
        .iter()
        .filter_map(|plan| {
            let checkpoint = state_root
                .join("sessions")
                .join(plan.identifier())
                .join("ancestor");
            crate::session::ancestor::readable(&checkpoint)
                .err()
                .map(|error| (plan.display(), format!("{error:#}")))
        })
        .collect()
}

/// Builds the report for a set of plans.
pub fn status_report(plans: &[&SessionPlan], state_root: &Path) -> StatusReport {
    // One round trip serves both questions: a supervisor that answers is
    // running, and its answer is what every session is doing.
    let live = control::query_progress(state_root);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let mut groups: Vec<GroupReport> = Vec::new();
    for plan in plans {
        let status = read_status(state_root, &plan.identifier()).ok().flatten();
        let progress = live.as_ref().and_then(|sessions| {
            sessions
                .iter()
                .find(|session| session.group == plan.group && session.host == plan.host)
                .map(|session| session.progress.clone())
        });
        let session = match status {
            None => SessionReport {
                host: plan.host.clone(),
                beta: plan.beta_spec(),
                mode: crate::config::mode_name(plan.mode).to_owned(),
                state: "never-run".into(),
                cycles: 0,
                age_seconds: None,
                conflicts: Vec::new(),
                blocked: Vec::new(),
                error: None,
                progress: progress.clone(),
            },
            Some(status) => SessionReport {
                host: plan.host.clone(),
                beta: plan.beta_spec(),
                mode: crate::config::mode_name(plan.mode).to_owned(),
                state: classify_state(&status),
                cycles: status.cycles,
                age_seconds: Some(now.saturating_sub(status.updated_at)),
                conflicts: status.conflict_details.clone(),
                blocked: status.blocked.clone(),
                error: status.error.clone(),
                progress: progress.clone(),
            },
        };
        match groups.last_mut() {
            Some(group) if group.name == plan.group => group.sessions.push(session),
            _ => groups.push(GroupReport {
                name: plan.group.clone(),
                alpha: plan.alpha_spec.clone(),
                sessions: vec![session],
            }),
        }
    }
    StatusReport {
        version: 2,
        supervisor_running: live.is_some(),
        service: match crate::service::state() {
            Ok(crate::service::ServiceState::NotInstalled) => "not-installed",
            Ok(crate::service::ServiceState::Stopped) => "stopped",
            Ok(crate::service::ServiceState::Running) => "running",
            Err(_) => "unknown",
        }
        .to_owned(),
        groups,
    }
}

/// The state word for a recorded status, with the two error shapes that
/// deserve their own word — a host that cannot be reached, and a session
/// that halted for safety — told apart from the rest.
pub fn classify_state(status: &SessionStatus) -> String {
    // Records written now carry the classification already. Ones written
    // before they did say only "error", and are read the old way — by the
    // message — so that a status file surviving an upgrade still reads
    // correctly. New records never take this path.
    if status.state != "error" {
        return status.state.clone();
    }
    let error = status.error.as_deref().unwrap_or("");
    if error.contains("unable to synchronize with") {
        "unreachable".into()
    } else if error.contains("halted") {
        "halted".into()
    } else {
        "errored".into()
    }
}

/// Every condition a session is currently in that warrants telling
/// someone.
///
/// A set rather than a word, because they genuinely co-occur: a session can
/// hold hundreds of conflicts *and* a handful of paths it cannot write, and
/// the single state word has to pick one and hide the other.
pub fn alerts_for(status: &SessionStatus) -> Vec<crate::alerts::Alert> {
    use crate::alerts::Alert;
    let mut alerts = Vec::new();
    match classify_state(status).as_str() {
        "halted" => alerts.push(Alert::Halted),
        "unreachable" => alerts.push(Alert::Unreachable),
        "errored" => alerts.push(Alert::Errored),
        // A cycle that ran leaves its exceptions behind; a cycle that
        // failed leaves last time's, which are not news about now.
        _ => {
            if !status.conflicts.is_empty() {
                alerts.push(Alert::Conflicts);
            }
            if !status.blocked.is_empty() {
                alerts.push(Alert::Blocked);
            }
        }
    }
    alerts
}

/// How to describe a session's conditions in one line.
pub fn alert_summary(status: &SessionStatus) -> String {
    let mut parts = Vec::new();
    match classify_state(status).as_str() {
        state @ ("halted" | "unreachable" | "errored") => parts.push(state.to_owned()),
        _ => {
            if !status.conflicts.is_empty() {
                parts.push(format!("{} conflicts", status.conflicts.len()));
            }
            if !status.blocked.is_empty() {
                parts.push(format!("{} blocked", status.blocked.len()));
            }
        }
    }
    parts.join(", ")
}

/// Describes a conflict's sides from the changes reconciliation recorded
/// for it: the newest node each side's changes carry at the conflict's
/// root, or absence.
fn conflict_detail(conflict: &crate::tree::Conflict) -> ConflictDetail {
    use crate::tree::{Change, Content};
    let side = |changes: &[Change]| -> ConflictSide {
        // The change at the conflict root itself describes the side; a
        // conflict rooted at a directory carries changes beneath it, and
        // the root's own entry is what the reader wants to see.
        let node = changes
            .iter()
            .find(|change| change.path == conflict.root)
            .or_else(|| changes.first())
            .and_then(|change| change.new.as_ref());
        match node {
            None => ConflictSide::default(),
            Some(node) => match &node.content {
                Content::File { metadata, .. } => ConflictSide {
                    present: true,
                    kind: "file".into(),
                    size: metadata.size,
                    mtime_seconds: metadata.mtime_seconds,
                },
                Content::Directory(_) => ConflictSide {
                    present: true,
                    kind: "directory".into(),
                    ..Default::default()
                },
                Content::Symlink { .. } => ConflictSide {
                    present: true,
                    kind: "symlink".into(),
                    ..Default::default()
                },
                _ => ConflictSide {
                    present: true,
                    kind: "other".into(),
                    ..Default::default()
                },
            },
        }
    };
    ConflictDetail {
        path: conflict.root.clone(),
        alpha: side(&conflict.alpha_changes),
        beta: side(&conflict.beta_changes),
    }
}

/// Flattens a cycle report's problems into labeled lines.
fn blocked_paths(report: &CycleReport) -> Vec<String> {
    let mut lines = Vec::new();
    for (side, problems) in [
        ("alpha", &report.alpha_scan_problems),
        ("alpha", &report.alpha_transition_problems),
        ("beta", &report.beta_scan_problems),
        ("beta", &report.beta_transition_problems),
    ] {
        for problem in problems.iter() {
            lines.push(format!("{side} {}: {}", problem.path, problem.message));
        }
    }
    lines
}

/// Watches what the workers publish and runs the alert hooks.
///
/// On its own thread, and deliberately so: a hook is someone else's
/// program. It may be slow, it may hang, and it must never be on the path
/// of a synchronization cycle. The worst a wedged hook can do from here is
/// delay the *next* hook, which the dispatcher already declines to launch.
fn watch_alerts(
    plans: &[SessionPlan],
    published: &[Arc<Mutex<Option<SessionStatus>>>],
    state_root: &Path,
    plan: crate::alerts::AlertPlan,
    stop: &AtomicBool,
) {
    use crate::alerts::{Alerter, Dispatcher, Fire, SessionAlerts};

    let timeout = plan.timeout;
    let mut alerter = Alerter::new(plan);
    let dispatcher = Dispatcher::default();
    while !stop.load(Ordering::Relaxed) {
        let sessions: Vec<SessionAlerts> = plans
            .iter()
            .zip(published)
            .map(|(plan, published)| {
                let status = published
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone();
                // A session that has not run yet is not in trouble; it has
                // simply not started. Alerting on it would fire on every
                // supervisor start.
                let (alerts, summary) = match &status {
                    Some(status) => (alerts_for(status), alert_summary(status)),
                    None => (Vec::new(), String::new()),
                };
                SessionAlerts {
                    group: plan.group.clone(),
                    host: plan.host.clone(),
                    alerts,
                    summary,
                }
            })
            .collect();

        if let Some(fire) = alerter.observe(&sessions, std::time::Instant::now()) {
            let commands = alerter.commands(&fire);
            let (summary, count, states) = match &fire {
                Fire::Alert {
                    summary,
                    alerts,
                    sessions,
                    ..
                } => (
                    summary.clone(),
                    *sessions,
                    alerts
                        .iter()
                        .map(|alert| alert.name())
                        .collect::<Vec<_>>()
                        .join(","),
                ),
                Fire::Recovered => ("all clear".to_owned(), 0, String::new()),
            };
            let environment = vec![
                ("AUTOBAHN_SUMMARY".to_owned(), summary),
                ("AUTOBAHN_ALERT_COUNT".to_owned(), count.to_string()),
                ("AUTOBAHN_STATES".to_owned(), states),
                (
                    "AUTOBAHN_EVENT".to_owned(),
                    match fire {
                        Fire::Recovered => "recovered".to_owned(),
                        Fire::Alert { repeat: true, .. } => "repeat".to_owned(),
                        Fire::Alert { .. } => "alert".to_owned(),
                    },
                ),
            ];
            // The same document `status --json` prints, so a hook that
            // wants more than the summary reads the seam that already
            // exists rather than a second one invented for it.
            let selected: Vec<&SessionPlan> = plans.iter().collect();
            let document =
                serde_json::to_string(&status_report(&selected, state_root)).unwrap_or_default();
            dispatcher.dispatch(commands, environment, document, timeout);
        }

        sleep_interruptible(ALERT_POLL_INTERVAL, stop);
    }
}

/// How often the service log is measured against its cap. Rare, because
/// the check is a `stat` and the file only grows between cycles.
const LOG_CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// How often the alerter looks at what the workers have published. The
/// confirmation period is what governs timeliness; this only has to be
/// finer than that.
const ALERT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Computes the delay before a failing session's next attempt: its interval
/// doubled per consecutive failure, capped at [`MAXIMUM_BACKOFF`], plus a
/// per-session jitter fraction. The jitter is applied *after* the cap so
/// that sessions saturated at the cap stay spread out — without it, every
/// session that failed together (a rebooting host, a dropped network)
/// would retry together forever.
fn backoff_delay(interval: Duration, consecutive_failures: u32, jitter_percent: u64) -> Duration {
    let factor = 1u32 << consecutive_failures.saturating_sub(1).min(16);
    let base = interval.saturating_mul(factor).min(MAXIMUM_BACKOFF);
    let jitter = Duration::from_millis(base.as_millis() as u64 * jitter_percent / 100);
    base + jitter
}

/// Derives a jitter percentage (0–24) from the session identifier and the
/// retry round, spreading retry schedules without any runtime randomness.
/// Mixing the round in decorrelates sessions from one retry to the next, so
/// sessions that happen to share a delay bucket in one round don't stay
/// phase-aligned forever.
fn jitter_percent(identifier: &str, round: u32) -> u64 {
    identifier
        .bytes()
        .fold(round as u64, |accumulator, byte| {
            accumulator
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add(byte as u64)
        })
        .rotate_left(17)
        % 25
}

/// Sleeps for the specified duration, waking early on stop or on any
/// control wake (so a flush or pause lands promptly even during backoff).
fn sleep_flagged(duration: Duration, stop: &AtomicBool, flags: &control::WorkerControl) {
    let deadline = std::time::Instant::now() + duration;
    while !stop.load(Ordering::Relaxed) {
        if flags.wake.swap(false, Ordering::Relaxed) {
            return;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return;
        }
        std::thread::sleep(remaining.min(STOP_POLL_INTERVAL));
    }
}

/// Sleeps for the specified duration, waking early if `stop` becomes true.
fn sleep_interruptible(duration: Duration, stop: &AtomicBool) {
    let deadline = std::time::Instant::now() + duration;
    while !stop.load(Ordering::Relaxed) {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return;
        }
        std::thread::sleep(remaining.min(STOP_POLL_INTERVAL));
    }
}

/// Returns the current time in seconds since the Unix epoch.
fn epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Returns the directory holding status files under a state root.
fn status_directory(state_root: &Path) -> PathBuf {
    state_root.join("status")
}

/// The counter that uniquifies status temporary names within a process.
static STATUS_TEMPORARY_COUNTER: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Writes a session's status file atomically. The temporary name is unique
/// per writer (process and counter), so even two processes racing over the
/// same session — which the session lock makes rare but a crashed lock
/// holder's final write can still overlap — publish whole files in
/// last-writer-wins order rather than tearing each other's temporaries.
fn write_status(state_root: &Path, identifier: &str, status: &SessionStatus) -> Result<()> {
    let directory = status_directory(state_root);
    fs::create_dir_all(&directory).context("unable to create the status directory")?;
    let path = directory.join(format!("{identifier}.json"));
    let temporary = directory.join(format!(
        "{identifier}.json.{}-{}.tmp",
        std::process::id(),
        STATUS_TEMPORARY_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let data = serde_json::to_vec_pretty(status).context("unable to encode status")?;
    if let Err(error) = fs::write(&temporary, data) {
        let _ = fs::remove_file(&temporary);
        return Err(error).context("unable to write status");
    }
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(error).context("unable to publish status");
    }
    Ok(())
}

/// Reads the status recorded for a session, if any.
pub fn read_status(state_root: &Path, identifier: &str) -> Result<Option<SessionStatus>> {
    let path = status_directory(state_root).join(format!("{identifier}.json"));
    let data = match fs::read(&path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("unable to read status {}", path.display()))
        }
    };
    let status = serde_json::from_slice(&data)
        .with_context(|| format!("unable to decode status {}", path.display()))?;
    Ok(Some(status))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A list that has not changed is not written out again.
    ///
    /// One statement grew to 97% of a 176 MB log — a session restating its
    /// seven hundred conflicts twice a minute for three days. What is
    /// worth recording is the change.
    #[test]
    fn only_changes_are_written_to_the_log() {
        let lines = |previous: Option<(Vec<String>, Vec<String>)>,
                     conflicts: &[&str],
                     blocked: &[&str]| {
            // The reporter writes to standard error, so this exercises the
            // decision rather than the output: what it *would* say is
            // derived the same way it derives it.
            let conflicts: Vec<String> = conflicts.iter().map(|s| s.to_string()).collect();
            let blocked: Vec<String> = blocked.iter().map(|s| s.to_string()).collect();
            match &previous {
                None => conflicts.len() + blocked.len(),
                Some((was_conflicts, was_blocked)) => {
                    let new_conflicts = conflicts
                        .iter()
                        .filter(|c| !was_conflicts.contains(c))
                        .count();
                    let new_blocked = blocked.iter().filter(|b| !was_blocked.contains(b)).count();
                    let gone = was_conflicts
                        .iter()
                        .filter(|c| !conflicts.contains(c))
                        .count()
                        + was_blocked.iter().filter(|b| !blocked.contains(b)).count();
                    new_conflicts + new_blocked + gone
                }
            }
        };

        let held: Vec<String> = (0..700).map(|n| format!("path{n}")).collect();
        let held_refs: Vec<&str> = held.iter().map(String::as_str).collect();

        // The same seven hundred, cycle after cycle: nothing to say.
        let previous = Some((held.clone(), Vec::new()));
        assert_eq!(lines(previous.clone(), &held_refs, &[]), 0);

        // One more appears, and only that one is news.
        let mut grown = held_refs.clone();
        grown.push("path700");
        assert_eq!(lines(previous.clone(), &grown, &[]), 1);

        // One clears, and that is news too.
        assert_eq!(lines(previous.clone(), &held_refs[1..], &[]), 1);

        // With nothing to compare against, the count stands in for the
        // list — a restart must not reprint everything a session holds.
        assert_eq!(lines(None, &held_refs, &[]), 700);
    }

    #[test]
    fn backoff_doubles_per_failure_and_saturates() {
        let interval = Duration::from_secs(5);
        assert_eq!(backoff_delay(interval, 1, 0), Duration::from_secs(5));
        assert_eq!(backoff_delay(interval, 2, 0), Duration::from_secs(10));
        assert_eq!(backoff_delay(interval, 3, 0), Duration::from_secs(20));
        assert_eq!(backoff_delay(interval, 7, 0), MAXIMUM_BACKOFF);
        // Large failure counts don't overflow the shift.
        assert_eq!(backoff_delay(interval, 1000, 0), MAXIMUM_BACKOFF);
    }

    #[test]
    fn backoff_jitter_spreads_saturated_sessions() {
        // Jitter is bounded (under 25% above the cap) and survives
        // saturation, so sessions that failed together stay spread out.
        let saturated = backoff_delay(Duration::from_secs(5), 1000, 20);
        assert_eq!(saturated, MAXIMUM_BACKOFF + MAXIMUM_BACKOFF / 5);
        for identifier in ["a", "b", "0123456789abcdef", ""] {
            for round in [1, 2, 3, 100] {
                assert!(jitter_percent(identifier, round) < 25);
            }
        }
        // The percentage is a stable function of the identifier and round,
        // and varies with the round (so retry buckets don't stay aligned).
        assert_eq!(
            jitter_percent("session-x", 3),
            jitter_percent("session-x", 3)
        );
        let varied: std::collections::HashSet<u64> = (1..=25)
            .map(|round| jitter_percent("session-x", round))
            .collect();
        assert!(varied.len() > 1, "jitter should vary across rounds");
    }

    /// An endpoint whose scans always present the same content and whose
    /// transitions always report staged content missing — the shape of a
    /// destination churning faster than transfers can complete.
    struct ChurningEndpoint {
        /// The root this endpoint reports on every scan.
        root: crate::tree::Node,
    }

    impl crate::endpoint::Endpoint for ChurningEndpoint {
        fn scan(&mut self) -> Result<crate::tree::Snapshot> {
            Ok(crate::tree::Snapshot {
                root: Some(self.root.clone()),
                ..crate::tree::Snapshot::default()
            })
        }

        fn stage_begin(
            &mut self,
            _files: Vec<crate::endpoint::FileRequest>,
        ) -> Result<Vec<crate::endpoint::StagingNeed>> {
            // Everything is claimed to be staged already, so the cycle
            // proceeds straight to a transition that reports it missing.
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

        fn transition(
            &mut self,
            transitions: Vec<crate::tree::Change>,
        ) -> Result<crate::endpoint::TransitionOutcome> {
            let missing = transitions
                .iter()
                .filter_map(|change| match &change.new {
                    Some(crate::tree::Node {
                        content: crate::tree::Content::File { digest, .. },
                        ..
                    }) => Some(crate::endpoint::FileRequest {
                        path: change.path.clone(),
                        digest: *digest,
                    }),
                    _ => None,
                })
                .collect();
            Ok(crate::endpoint::TransitionOutcome {
                results: vec![None; transitions.len()],
                problems: Vec::new(),
                missing_staged_files: true,
                missing_staged: missing,
            })
        }
    }

    /// Staging that never produces the *same* content is a real failure,
    /// and must surface as one: the digest never changes, so nothing about
    /// this is a file being rewritten.
    #[test]
    fn content_that_never_appears_is_an_error_not_a_success() {
        use crate::tree::{Content, FileMetadata, Node, SyncMode};

        let alpha_root = Node::directory(
            "",
            vec![Node {
                name: "churning.txt".into(),
                content: Content::File {
                    digest: [7u8; 32],
                    executable: false,
                    metadata: FileMetadata::default(),
                },
            }],
        );
        let beta_root = Node::directory("", Vec::new());

        let state = tempfile::tempdir().expect("temporary directory should be creatable");
        let mut session = Session::new(
            Box::new(ChurningEndpoint { root: alpha_root }),
            Box::new(ChurningEndpoint { root: beta_root }),
            SyncMode::TwoWaySafe,
            state.path().join("session"),
        )
        .expect("the session should construct");

        let error =
            run_cycles(&mut session).expect_err("unappearing content must surface as an error");
        assert!(
            format!("{error:#}").contains("staging is failing"),
            "{error:#}"
        );
    }

    /// A tree being written faster than it transfers is busy, not broken.
    /// The cap still bounds the attempt, but it returns the work that was
    /// done — with the flag still set — so the worker keeps its session and
    /// paces the next attempt on its watcher.
    #[test]
    fn churning_content_returns_the_work_done_rather_than_an_error() {
        use crate::tree::{Content, FileMetadata, Node, SyncMode};

        /// Reports a *different* digest each cycle, as a rewritten file
        /// does: the follow-up rescans and asks for the new content.
        struct RotatingEndpoint {
            root: Node,
            round: std::cell::Cell<u8>,
        }
        impl crate::endpoint::Endpoint for RotatingEndpoint {
            fn scan(&mut self) -> Result<crate::tree::Snapshot> {
                let round = self.round.get().wrapping_add(1);
                self.round.set(round);
                let mut root = self.root.clone();
                if let Content::Directory(children) = &mut root.content {
                    for child in std::sync::Arc::make_mut(children) {
                        if let Content::File { digest, .. } = &mut child.content {
                            *digest = [round; 32];
                        }
                    }
                }
                Ok(crate::tree::Snapshot {
                    root: Some(root),
                    ..crate::tree::Snapshot::default()
                })
            }
            fn stage_begin(
                &mut self,
                _requests: Vec<crate::endpoint::FileRequest>,
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
            fn transition(
                &mut self,
                transitions: Vec<crate::tree::Change>,
            ) -> Result<crate::endpoint::TransitionOutcome> {
                let missing = transitions
                    .iter()
                    .filter_map(|change| match &change.new {
                        Some(Node {
                            content: Content::File { digest, .. },
                            ..
                        }) => Some(crate::endpoint::FileRequest {
                            path: change.path.clone(),
                            digest: *digest,
                        }),
                        _ => None,
                    })
                    .collect();
                Ok(crate::endpoint::TransitionOutcome {
                    results: vec![None; transitions.len()],
                    problems: Vec::new(),
                    missing_staged_files: true,
                    missing_staged: missing,
                })
            }
        }

        let file = |name: &str| Node {
            name: name.into(),
            content: Content::File {
                digest: [1u8; 32],
                executable: false,
                metadata: FileMetadata::default(),
            },
        };
        let state = tempfile::tempdir().expect("temporary directory should be creatable");
        let mut session = Session::new(
            Box::new(RotatingEndpoint {
                root: Node::directory("", vec![file("churning.txt")]),
                round: std::cell::Cell::new(0),
            }),
            Box::new(RotatingEndpoint {
                root: Node::directory("", Vec::new()),
                round: std::cell::Cell::new(100),
            }),
            SyncMode::TwoWaySafe,
            state.path().join("session"),
        )
        .expect("the session should construct");

        let (digest, report) =
            run_cycles(&mut session).expect("churn must not be reported as a failure");
        assert!(report.missing_staged_files, "the flag must survive the cap");
        assert_eq!(digest.cycles, MAXIMUM_FOLLOW_UP_CYCLES as u64 + 1);
    }

    #[test]
    fn status_files_round_trip() {
        let directory = tempfile::tempdir().expect("temporary directory should be creatable");
        let status = SessionStatus {
            group: "g".into(),
            host: "h".into(),
            alpha: "~/a".into(),
            beta: "h:~/a".into(),
            mode: "two-way-safe".into(),
            state: "conflicts".into(),
            cycles: 3,
            last_alpha_transitions: 1,
            last_beta_transitions: 2,
            conflicts: vec!["path/to/conflict".into()],
            conflict_details: Vec::new(),
            blocked: vec!["beta x: denied".into()],
            error: None,
            updated_at: 12345,
            alpha_entries: 1_000,
            beta_entries: 1_002,
        };
        write_status(directory.path(), "abc123", &status).expect("status should write");
        let loaded = read_status(directory.path(), "abc123")
            .expect("status should read")
            .expect("status should exist");
        assert_eq!(loaded.group, "g");
        assert_eq!(loaded.state, "conflicts");
        assert_eq!(loaded.cycles, 3);
        // The scan totals ride along so the next run's first scan can be
        // measured against them rather than merely timed.
        assert_eq!(loaded.alpha_entries, 1_000);
        assert_eq!(loaded.beta_entries, 1_002);
        assert_eq!(loaded.conflicts, vec!["path/to/conflict".to_owned()]);

        assert!(read_status(directory.path(), "missing")
            .expect("a missing status should read as None")
            .is_none());
    }

    #[test]
    fn interruptible_sleep_wakes_on_stop() {
        let stop = AtomicBool::new(false);
        let start = std::time::Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(50));
                stop.store(true, Ordering::Relaxed);
            });
            sleep_interruptible(Duration::from_secs(60), &stop);
        });
        assert!(start.elapsed() < Duration::from_secs(10));
    }
}
