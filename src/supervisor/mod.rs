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

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{mode_name, BetaTarget, SessionPlan};
use crate::endpoint::local::LocalEndpoint;
use crate::endpoint::remote::RemoteEndpoint;
use crate::endpoint::Endpoint;
use crate::scan::IgnoreSet;
use crate::session::{CycleReport, Session};
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
    /// The session state: `synchronized`, `conflicts`, `problems`, or
    /// `error`.
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
    /// outcomes rather than aborting the run.
    pub fn run_once(&self) -> Vec<SessionOutcome> {
        std::thread::scope(|scope| {
            let handles: Vec<_> = self
                .plans
                .iter()
                .map(|plan| {
                    scope.spawn(move || {
                        let mut worker = Worker::new(plan, &self.state_root, self.verbose);
                        let result = worker.attempt();
                        worker.record(&result);
                        SessionOutcome {
                            display: plan.display(),
                            result: result
                                .map(|(digest, _)| digest)
                                .map_err(|error| format!("{error:#}")),
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
    /// session cycles on its own interval, backs off exponentially while its
    /// destination is failing, and reconnects (healing the session) on the
    /// first attempt after a failure.
    pub fn run_watch(&self, stop: &AtomicBool) {
        std::thread::scope(|scope| {
            for plan in &self.plans {
                scope.spawn(move || {
                    let mut worker = Worker::new(plan, &self.state_root, self.verbose);
                    let mut failures = 0u32;
                    while !stop.load(Ordering::Relaxed) {
                        let result = worker.attempt();
                        let delay = match &result {
                            Ok(_) => {
                                failures = 0;
                                plan.interval
                            }
                            Err(_) => {
                                failures = failures.saturating_add(1);
                                backoff_delay(plan.interval, failures)
                            }
                        };
                        worker.record(&result);
                        sleep_interruptible(delay, stop);
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
    /// bounded follow-ups while staged content is reported missing). On
    /// failure the session is dropped, so the next attempt reconnects from
    /// scratch (which also shuts down and reaps any agent process).
    fn attempt(&mut self) -> Result<(CycleDigest, CycleReport)> {
        let result = (|| {
            if self.session.is_none() {
                self.session = Some(connect(self.plan, self.state_root)?);
            }
            let session = self.session.as_mut().expect("the session was just created");
            let mut digest = CycleDigest::default();
            loop {
                let report = session.run_cycle()?;
                digest.cycles += 1;
                digest.alpha_transitions += report.alpha_transitions;
                digest.beta_transitions += report.beta_transitions;
                digest.conflicts = report.conflicts.len();
                digest.problems = problem_lines(&report).len();
                if !report.missing_staged_files || digest.cycles > MAXIMUM_FOLLOW_UP_CYCLES as u64 {
                    return Ok((digest, report));
                }
            }
        })();
        if result.is_err() {
            self.session = None;
        } else {
            self.cycles += result
                .as_ref()
                .map(|(digest, _)| digest.cycles)
                .unwrap_or(0);
        }
        result
    }

    /// Records an attempt's result to the session's status file (and, when
    /// verbose, to standard output).
    fn record(&self, result: &Result<(CycleDigest, CycleReport)>) {
        let display = self.plan.display();
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
        if let Err(error) = write_status(self.state_root, &self.plan.identifier(), &status) {
            eprintln!("[{display}] unable to record status: {error:#}");
        }
    }
}

/// Builds a live session for a plan: a local alpha endpoint, a local or
/// remote beta endpoint, and the persisted session state under the state
/// root.
fn connect(plan: &SessionPlan, state_root: &Path) -> Result<Session> {
    let identifier = plan.identifier();
    let state_directory = state_root.join("sessions").join(&identifier);

    let alpha_root = plan
        .alpha
        .canonicalize()
        .with_context(|| format!("unable to resolve alpha root {}", plan.alpha.display()))?;
    let alpha: Box<dyn Endpoint + Send> = Box::new(LocalEndpoint::new(
        alpha_root,
        state_directory.join("staging-alpha"),
        IgnoreSet::new(&plan.ignores)?,
    )?);

    let beta: Box<dyn Endpoint + Send> = match &plan.beta {
        BetaTarget::Local(path) => Box::new(LocalEndpoint::new(
            path.clone(),
            state_directory.join("staging-beta"),
            IgnoreSet::new(&plan.ignores)?,
        )?),
        BetaTarget::Remote {
            destination,
            path,
            agent_command,
        } => {
            let argv = match agent_command {
                Some(argv) => argv.clone(),
                None => Connection::ssh_argv(destination, None),
            };
            let connection = Connection::spawn(&argv)?;
            Box::new(RemoteEndpoint::connect(
                connection,
                path.clone(),
                identifier.clone(),
                plan.ignores.clone(),
            )?)
        }
    };

    Session::new(alpha, beta, plan.mode, state_directory)
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
/// doubled per consecutive failure, capped at [`MAXIMUM_BACKOFF`].
fn backoff_delay(interval: Duration, consecutive_failures: u32) -> Duration {
    let factor = 1u32 << consecutive_failures.saturating_sub(1).min(16);
    interval.saturating_mul(factor).min(MAXIMUM_BACKOFF)
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

/// Writes a session's status file atomically.
fn write_status(state_root: &Path, identifier: &str, status: &SessionStatus) -> Result<()> {
    let directory = status_directory(state_root);
    fs::create_dir_all(&directory).context("unable to create the status directory")?;
    let path = directory.join(format!("{identifier}.json"));
    let temporary = directory.join(format!("{identifier}.json.tmp"));
    let data = serde_json::to_vec_pretty(status).context("unable to encode status")?;
    fs::write(&temporary, data).context("unable to write status")?;
    fs::rename(&temporary, &path).context("unable to publish status")?;
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
        assert_eq!(backoff_delay(interval, 1), Duration::from_secs(5));
        assert_eq!(backoff_delay(interval, 2), Duration::from_secs(10));
        assert_eq!(backoff_delay(interval, 3), Duration::from_secs(20));
        assert_eq!(backoff_delay(interval, 7), MAXIMUM_BACKOFF);
        // Large failure counts don't overflow the shift.
        assert_eq!(backoff_delay(interval, 1000), MAXIMUM_BACKOFF);
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
