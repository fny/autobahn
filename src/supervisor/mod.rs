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
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{mode_name, BetaTarget, SessionPlan};
use crate::endpoint::local::{EndpointOptions, LocalEndpoint};
use crate::endpoint::remote::RemoteEndpoint;
use crate::endpoint::Endpoint;
use crate::scan::IgnoreSet;
use crate::session::{CycleReport, Session, SessionLock, SessionLockHeld};
use crate::transport::Connection;

/// The maximum delay between attempts for a failing session.
const MAXIMUM_BACKOFF: Duration = Duration::from_secs(300);

/// The number of immediate follow-up cycles permitted when an endpoint
/// reports missing staged content, bounding the retry loop that a
/// continuously churning file could otherwise sustain.
const MAXIMUM_FOLLOW_UP_CYCLES: u32 = 5;

/// The granularity at which sleeping workers check for a stop request.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// The recorded state of one supervised session, as persisted to its status
/// file after every attempt.
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
    /// The session state: `synchronized`, `conflicts`, `problems`,
    /// `paused`, or `error`.
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
    /// Any problems reported by the most recent cycle, as `side path:
    /// message` strings.
    pub problems: Vec<String>,
    /// The failure that ended the most recent attempt, if it failed.
    pub error: Option<String>,
    /// When this status was recorded, in seconds since the Unix epoch.
    pub updated_at: u64,
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
}

impl Supervisor {
    /// Creates a supervisor over the provided plans, with state and status
    /// kept under the provided root.
    pub fn new(plans: Vec<SessionPlan>, state_root: PathBuf, verbose: bool) -> Supervisor {
        Supervisor {
            plans,
            state_root,
            verbose,
        }
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
                        let mut worker = Worker::new(plan, &self.state_root, self.verbose);
                        let result = worker.attempt();
                        let recorded = worker.conclude(&result);
                        let result = match (result, recorded) {
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
    pub fn run_watch(&self, stop: &AtomicBool) {
        // One supervisor per state root: a second one's workers would all
        // lose their session locks anyway, but it would still capture the
        // control socket — commands would land in a supervisor that owns
        // nothing. Refuse up front instead.
        let _supervisor_lock = match SessionLock::acquire(self.state_root.join("supervisor")) {
            Ok(lock) => lock,
            Err(error) => {
                eprintln!("unable to supervise: {error:#}");
                return;
            }
        };

        // Every session gets a control-flag block; the registry shares them
        // with the control socket's server thread.
        let controls: Vec<Arc<control::WorkerControl>> = self
            .plans
            .iter()
            .map(|_| Arc::<control::WorkerControl>::default())
            .collect();
        let registry = control::Registry {
            entries: self
                .plans
                .iter()
                .zip(&controls)
                .map(|(plan, flags)| (plan.group.clone(), plan.host.clone(), flags.clone()))
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

        std::thread::scope(|scope| {
            if let Some(listener) = listener {
                let registry = &registry;
                scope.spawn(move || control::serve(listener, registry, stop));
            }
            for (index, plan) in self.plans.iter().enumerate() {
                let flags = controls[index].clone();
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

                    let mut worker = Worker::new(plan, &self.state_root, self.verbose);
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
                            sleep_flagged(delay, stop, &flags);
                        } else {
                            failures = 0;
                            worker.await_activity(plan.interval, stop, &flags);
                        }
                    }
                });
            }
        });
    }
}

/// The per-session worker state: the live session (present while the
/// connection is healthy) and the running cycle count.
struct Worker<'a> {
    /// The session's plan.
    plan: &'a SessionPlan,
    /// The state root.
    state_root: &'a Path,
    /// Whether or not to log activity.
    verbose: bool,
    /// The live session, if the last attempt (if any) succeeded.
    session: Option<Session>,
    /// The number of cycles completed since this worker started.
    cycles: u64,
}

impl<'a> Worker<'a> {
    /// Creates a worker for a plan.
    fn new(plan: &'a SessionPlan, state_root: &'a Path, verbose: bool) -> Worker<'a> {
        Worker {
            plan,
            state_root,
            verbose,
            session: None,
            cycles: 0,
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
                self.session = Some(connect(self.plan, self.state_root)?);
            }
            run_cycles(self.session.as_mut().expect("the session was just created"))
        })();
        if let Ok((digest, _)) = &result {
            self.cycles += digest.cycles;
        }
        result
    }

    /// Concludes an attempt: records its status (while any held lock is
    /// still ours), then, on failure, drops the session so the next attempt
    /// reconnects from scratch (which also shuts down and reaps any agent
    /// process).
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
        const SETTLE: Duration = Duration::from_millis(100);
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
                        std::thread::sleep(SETTLE.min(interval));
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
        if let Err(error) = std::fs::remove_file(state_directory.join("ancestor")) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "[{}] unable to reset the ancestor: {error}",
                    self.plan.display()
                );
                return;
            }
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
            problems: Vec::new(),
            error: None,
            updated_at: epoch_seconds(),
        };
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
    fn record(&self, result: &Result<(CycleDigest, CycleReport)>) -> Result<()> {
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
            problems: Vec::new(),
            error: None,
            updated_at: epoch_seconds(),
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
                status.problems = problem_lines(report);
                status.state = if !status.conflicts.is_empty() {
                    "conflicts".into()
                } else if !status.problems.is_empty() {
                    "problems".into()
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
                    for root in &status.conflicts {
                        eprintln!("[{display}] conflict at {root:?} (left unresolved)");
                    }
                    for problem in &status.problems {
                        eprintln!("[{display}] problem: {problem}");
                    }
                }
            }
            Err(error) => {
                status.state = "error".into();
                status.error = Some(format!("{error:#}"));
                if self.verbose {
                    eprintln!("[{display}] error: {error:#}");
                }
            }
        }
        write_status(self.state_root, &self.plan.identifier(), &status)
    }
}

/// Runs one cycle plus bounded follow-ups while staged content is reported
/// missing. Exhausting the cap with content *still* missing is a failure,
/// not a quiet success: the destination is churning faster than content can
/// be transferred, and automation must not read that as "synchronized".
fn run_cycles(session: &mut Session) -> Result<(CycleDigest, CycleReport)> {
    let mut digest = CycleDigest::default();
    loop {
        let report = session.run_cycle()?;
        digest.cycles += 1;
        digest.alpha_transitions += report.alpha_transitions;
        digest.beta_transitions += report.beta_transitions;
        digest.conflicts = report.conflicts.len();
        digest.problems = problem_lines(&report).len();
        if !report.missing_staged_files {
            return Ok((digest, report));
        }
        if digest.cycles > MAXIMUM_FOLLOW_UP_CYCLES as u64 {
            bail!(
                "staged content was still missing after {} cycles; source content is \
                 changing faster than it can be transferred",
                digest.cycles
            );
        }
    }
}

/// Builds a live session for a plan: a local alpha endpoint, a local or
/// remote beta endpoint, and the persisted session state under the state
/// root.
fn connect(plan: &SessionPlan, state_root: &Path) -> Result<Session> {
    let identifier = plan.identifier();
    let state_directory = state_root.join("sessions").join(&identifier);

    // The lock comes first: a conflicting session must be discovered before
    // any endpoint work happens, not after spawning SSH and handshaking
    // with a remote agent — endpoint construction is expensive, can block
    // on the network, and has remote side effects.
    let lock = SessionLock::acquire(state_directory.clone())?;

    let options = || -> Result<EndpointOptions> {
        Ok(EndpointOptions {
            ignores: IgnoreSet::new(&plan.ignores)?,
            symlink_mode: plan.symlink_mode,
            file_mode: plan.file_mode,
            directory_mode: plan.directory_mode,
        })
    };
    let alpha_root = plan
        .alpha
        .canonicalize()
        .with_context(|| format!("unable to resolve alpha root {}", plan.alpha.display()))?;
    let alpha: Box<dyn Endpoint + Send> = Box::new(LocalEndpoint::new(
        alpha_root,
        state_directory.join("staging-alpha"),
        options()?,
    )?);

    let beta: Box<dyn Endpoint + Send> = match &plan.beta {
        BetaTarget::Local(path) => Box::new(LocalEndpoint::new(
            path.clone(),
            state_directory.join("staging-beta"),
            options()?,
        )?),
        BetaTarget::Remote {
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
            };
            match agent_command {
                Some(argv) => {
                    let connection = Connection::spawn(argv)?;
                    Box::new(RemoteEndpoint::connect(connection, initialize)?)
                }
                None => Box::new(crate::endpoint::remote::connect_ssh(
                    destination,
                    initialize,
                )?),
            }
        }
    };

    Session::with_lock(alpha, beta, plan.mode, lock)
}

/// Flattens a cycle report's problems into labeled lines.
fn problem_lines(report: &CycleReport) -> Vec<String> {
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
            Ok(crate::endpoint::TransitionOutcome {
                results: vec![None; transitions.len()],
                problems: Vec::new(),
                missing_staged_files: true,
            })
        }
    }

    #[test]
    fn exhausting_the_follow_up_cap_is_an_error_not_a_success() {
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

        let error = run_cycles(&mut session).expect_err("the cap must surface as an error");
        assert!(format!("{error:#}").contains("still missing"), "{error:#}");
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
            problems: vec!["beta x: denied".into()],
            error: None,
            updated_at: 12345,
        };
        write_status(directory.path(), "abc123", &status).expect("status should write");
        let loaded = read_status(directory.path(), "abc123")
            .expect("status should read")
            .expect("status should exist");
        assert_eq!(loaded.group, "g");
        assert_eq!(loaded.state, "conflicts");
        assert_eq!(loaded.cycles, 3);
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
