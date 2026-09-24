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

use crate::config::{EndpointTarget, SessionPlan};

pub mod peer;
pub mod reload;
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
    /// Content on this side that cannot be synchronized, when there is any.
    ///
    /// This is frequently the *reason* for the conflict rather than a
    /// detail of it: reconciliation propagates one side over the other
    /// freely, but it refuses to overwrite a side holding content it never
    /// scanned, and reports a conflict instead. Without this the listing
    /// shows two ordinary-looking sides and no cause, which reads as a
    /// bug in the comparison.
    #[serde(default)]
    pub unsynchronizable: Option<Unsynchronizable>,
}

/// Content that synchronization cannot carry, summarized for a reader.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Unsynchronizable {
    /// How many such entries this side holds.
    pub entries: u64,
    /// One of them, root-relative — enough to go and look.
    pub example: String,
    /// Why that one cannot be synchronized.
    pub reason: String,
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
    /// Files and bytes moved over the session's life, carried across
    /// restarts. The shop's tally.
    #[serde(default)]
    pub moved_files: u64,
    #[serde(default)]
    pub moved_bytes: u64,
    /// Peering: the supervisor's role when this was recorded — `leader`,
    /// `follower`, or empty for a plain mode — and its term.
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub term: u64,
    /// How long the failure recorded here must stand before it alerts,
    /// when that differs from its state's usual patience: a halt that
    /// clears on its own, like a missing alpha folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert_after_seconds: Option<u64>,
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
    /// The configuration file, watched for edits while the sessions run.
    /// None when there is no file — `sync`, a peer, a test — or the file
    /// says not to.
    reloader: Option<Arc<reload::Reloader>>,
    /// Peering, when any plan is in a peering mode: the role this
    /// supervisor holds, shared by its workers.
    peering: Option<PeeringContext>,
}

/// Peering, from the supervisor's side: the role, and what the leader
/// pushes to its followers.
///
/// The role is one value for the whole supervisor. A lease is per host,
/// so a fence answered on any session means another controller leads,
/// and every session of this supervisor stops writing together.
pub struct PeeringContext {
    /// The configuration file the leader pushes, read at push time so a
    /// follower gets the file as it is on disk.
    config_path: PathBuf,
    /// This machine's own peering directory, where its role is remembered
    /// across restarts.
    directory: PathBuf,
    /// Where the ignore files the configuration names are: the alpha's
    /// own `ignores/`, or the pushed copies for a beta that leads.
    ignores_directory: PathBuf,
    /// The role and the handoff, shared with every worker and with the
    /// control socket.
    shared: Arc<PeeringShared>,
}

/// The part of the peering context that outlives a borrow: the control
/// socket's yield closure holds it, and so does every worker.
struct PeeringShared {
    /// This machine's own peering directory.
    directory: PathBuf,
    /// The role.
    role: Mutex<crate::peering::Role>,
    /// A handoff in progress: who leads next, at what term. Each worker
    /// hands its peer the new lease on its next attempt; when every
    /// session has, the role becomes follower.
    handoff: Mutex<Option<(String, u64)>>,
    /// How many sessions have handed the new lease on.
    handed: std::sync::atomic::AtomicUsize,
    /// How many sessions there are to hand it on.
    sessions: std::sync::atomic::AtomicUsize,
}

impl PeeringShared {
    fn new(directory: PathBuf, role: crate::peering::Role) -> PeeringShared {
        PeeringShared {
            directory,
            role: Mutex::new(role),
            handoff: Mutex::new(None),
            handed: std::sync::atomic::AtomicUsize::new(0),
            sessions: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn role(&self) -> crate::peering::Role {
        self.role
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Starts a handoff to `to` at the next term. The local lease says so
    /// at once, so a restart in the middle does not come back leading.
    fn yield_to(&self, to: &str, ttl: Duration) -> Result<()> {
        let role = self.role();
        let crate::peering::Role::Leader { term, .. } = role else {
            anyhow::bail!("not leading; nothing to yield");
        };
        let next = term + 1;
        let mut handoff = self
            .handoff
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some((current, at)) = &*handoff {
            if current == to && *at == next {
                return Ok(());
            }
        }
        crate::note!("peering: handing the lead to {to} at term {next}");
        crate::peering::write_lease(&self.directory, &crate::peering::Lease::new(to, next, ttl))?;
        *handoff = Some((to.to_owned(), next));
        self.handed.store(0, Ordering::SeqCst);
        Ok(())
    }

    /// The handoff in progress, if any.
    fn handoff(&self) -> Option<(String, u64)> {
        self.handoff
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// One session handed the lease on. When every session has, the
    /// supervisor follows.
    fn handed_one(&self) {
        let handed = self.handed.fetch_add(1, Ordering::SeqCst) + 1;
        if handed >= self.sessions.load(Ordering::SeqCst) {
            let handoff = self
                .handoff
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            if let Some((to, term)) = handoff {
                *self.role.lock().unwrap_or_else(|error| error.into_inner()) =
                    crate::peering::Role::Follower { leader: to, term };
            }
        }
    }
}

impl PeeringContext {
    /// The context for a supervisor that is the configured alpha: it
    /// leads at the term its own lease file remembers, or at a first
    /// term, unless that file says a beta led while it was away — then it
    /// follows, and stays a follower until a later phase hands the lead
    /// back.
    pub fn for_alpha(config_path: PathBuf, directory: PathBuf) -> Result<PeeringContext> {
        let role = match crate::peering::alpha_term(&directory)? {
            crate::peering::AlphaStart::Lead { term } => {
                let lease = crate::peering::Lease::new(
                    crate::peering::ALPHA,
                    term,
                    crate::config::DEFAULT_PEERING_TTL,
                );
                crate::peering::write_lease(&directory, &lease)?;
                crate::peering::Role::Leader {
                    leader: crate::peering::ALPHA.to_owned(),
                    term,
                }
            }
            crate::peering::AlphaStart::Follow { lease } => crate::peering::Role::Follower {
                leader: lease.leader,
                term: lease.term,
            },
        };
        Ok(PeeringContext {
            config_path,
            ignores_directory: crate::paths::default_state_root()?
                .join(crate::scan::ignorefile::DIRECTORY),
            shared: Arc::new(PeeringShared::new(directory.clone(), role)),
            directory,
        })
    }

    /// The context for a beta that took the lead: it leads as `leader`
    /// (its own spec) at `term`, runs the pushed configuration, and
    /// pushes the pushed ignore files on.
    pub fn for_leader(directory: PathBuf, leader: String, term: u64) -> PeeringContext {
        PeeringContext {
            config_path: directory.join("config.toml"),
            ignores_directory: directory.join(crate::scan::ignorefile::DIRECTORY),
            shared: Arc::new(PeeringShared::new(
                directory.clone(),
                crate::peering::Role::Leader { leader, term },
            )),
            directory,
        }
    }

    /// The role as it stands.
    pub fn role(&self) -> crate::peering::Role {
        self.shared.role()
    }

    /// This machine's peering directory.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Hands the lead to `to`: the local lease names it at the next term,
    /// every session presents that lease to its peer on its next attempt,
    /// and then the supervisor follows.
    pub fn yield_to(&self, to: &str) -> Result<()> {
        self.shared.yield_to(to, crate::config::DEFAULT_PEERING_TTL)
    }

    /// A handle the control socket can call to yield.
    fn yield_handle(&self) -> control::YieldHandle {
        let shared = self.shared.clone();
        Arc::new(move |to: &str| shared.yield_to(to, crate::config::DEFAULT_PEERING_TTL))
    }

    /// Steps down: another controller holds `current` on some host. The
    /// alpha's own lease file records it too, so a restart does not come
    /// back leading.
    fn step_down(&self, current: &crate::peering::Lease) {
        let mut role = self
            .shared
            .role
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let crate::peering::Role::Follower { term, .. } = &*role {
            if *term >= current.term {
                return;
            }
        }
        crate::complain!(
            "peering: {} leads at term {}; stepping down",
            current.leader,
            current.term
        );
        *role = crate::peering::Role::Follower {
            leader: current.leader.clone(),
            term: current.term,
        };
        if let Err(error) = crate::peering::write_lease(&self.directory, current) {
            crate::complain!("peering: unable to record the lease locally: {error:#}");
        }
    }

    /// The files a follower needs, as they are on disk right now: the
    /// configuration, and every ignore file the configuration can name.
    fn pushed_files(
        &self,
        beta_spec: &str,
        group: &str,
        identifier: &str,
    ) -> Result<Vec<(String, Vec<u8>)>> {
        let mut files = Vec::new();
        let config = std::fs::read(&self.config_path)
            .with_context(|| format!("unable to read {}", self.config_path.display()))?;
        files.push(("config.toml".to_owned(), config));
        files.push(("name".to_owned(), beta_spec.as_bytes().to_vec()));
        files.push((format!("sessions/{group}"), identifier.as_bytes().to_vec()));
        let ignores = &self.ignores_directory;
        if let Ok(entries) = std::fs::read_dir(ignores) {
            let mut names: Vec<_> = entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().is_file())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            for name in names {
                let bytes = std::fs::read(ignores.join(&name))
                    .with_context(|| format!("unable to read {}", ignores.join(&name).display()))?;
                files.push((format!("ignores/{name}"), bytes));
            }
        }
        Ok(files)
    }
}

/// The error a worker's attempt ends with while its supervisor follows:
/// nothing was connected, nothing was written.
#[derive(Debug, thiserror::Error)]
#[error("following {leader} at term {term}; this supervisor is not leading")]
pub struct Following {
    pub leader: String,
    pub term: u64,
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
            reloader: None,
            peering: None,
        }
    }

    /// Watches the configuration file the plans came from. `run_watch`
    /// then returns when an edit loads, with the new configuration in the
    /// reloader for the caller to run.
    pub fn with_reload(mut self, reloader: Option<Arc<reload::Reloader>>) -> Supervisor {
        self.reloader = reloader;
        self
    }

    /// Adopts an alert plan, so that sessions needing attention are
    /// announced rather than merely recorded.
    pub fn with_alerts(mut self, alerts: crate::alerts::AlertPlan) -> Supervisor {
        self.alerts = alerts;
        self
    }

    /// Adopts a peering context, so that sessions in a peering mode lead
    /// (or follow) rather than run as plain sessions.
    pub fn with_peering(mut self, peering: PeeringContext) -> Supervisor {
        peering
            .shared
            .sessions
            .store(self.plans.len(), Ordering::SeqCst);
        self.peering = Some(peering);
        self
    }

    /// Peering: offers a connection a peer opened to this supervisor, for
    /// the session whose endpoint is reached by attachment.
    pub fn offer_attachment(&self, name: &str, connection: crate::transport::Connection) {
        self.pool.offer_attachment(name, connection);
    }

    /// The peering role, for a caller that wants to show it.
    pub fn role(&self) -> crate::peering::Role {
        match &self.peering {
            Some(peering) => peering.role(),
            None => crate::peering::Role::Off,
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
                    crate::threads::spawn_deep_scoped(scope, move || {
                        let mut worker =
                            Worker::new(plan, &self.state_root, &self.pool, self.verbose);
                        worker.peering = self.peering.as_ref();
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
        // A loaded edit winds the workers down through a flag of its own,
        // mirrored from the caller's: the caller's `stop` still means
        // stop, and the caller learns which it was from the reloader.
        let halt = AtomicBool::new(false);
        let caller_stop = stop;
        let stop: &AtomicBool = match self.reloader {
            Some(_) => &halt,
            None => caller_stop,
        };
        // This supervisor started from a configuration that passed, so a
        // refusal an earlier one left behind is over.
        if self.reloader.is_some() {
            reload::clear_notice(&self.state_root);
        }

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
                    progress.seed_moved(status.moved_files, status.moved_bytes);
                }
                progress
            })
            .collect();
        let registry = control::Registry {
            yield_to: self.peering.as_ref().map(|peering| peering.yield_handle()),
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
                crate::complain!("control socket unavailable: {error:#}");
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
                        Ok(true) => {
                            crate::complain!("the service log reached its cap and was rotated")
                        }
                        Ok(false) => {}
                        Err(error) => {
                            crate::complain!("unable to rotate the service log: {error:#}")
                        }
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

            if let Some(reloader) = &self.reloader {
                let state_root = self.state_root.as_path();
                let alerts = &self.alerts;
                let halt = &halt;
                scope.spawn(move || {
                    // The watch answers to the caller's stop; the workers
                    // answer to `halt`, which it raises either way.
                    reloader.watch(state_root, alerts, caller_stop, halt);
                    halt.store(true, Ordering::Relaxed);
                });
            }

            for (index, plan) in self.plans.iter().enumerate() {
                let flags = controls[index].clone();
                let progress = progresses[index].clone();
                let published = published[index].clone();
                crate::threads::spawn_deep_scoped(scope, move || {
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
                    worker.peering = self.peering.as_ref();
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
                            crate::complain!(
                                "[{}] unable to record status: {error:#}",
                                plan.display()
                            );
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
    /// Peering, when the supervisor has it and this plan is in a peering
    /// mode.
    peering: Option<&'a PeeringContext>,
    /// A digest of the files last pushed to the beta, so they go again
    /// only when they change.
    pushed: Option<[u8; 32]>,
    /// The handoff this worker has already handed on, if any.
    handed: Option<(String, u64)>,
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
            peering: None,
            pushed: None,
            handed: None,
        }
    }

    /// Peering: which side of this plan the peer is.
    fn peer_side(&self) -> crate::peering::PeerSide {
        match &self.plan.alpha {
            EndpointTarget::Remote { destination, .. }
                if crate::peering::attached_name(destination).is_some() =>
            {
                crate::peering::PeerSide::Alpha
            }
            _ => crate::peering::PeerSide::Beta,
        }
    }

    /// Peering: a handoff in progress is handed on — the new lease
    /// presented to this session's peer — once per handoff, and this
    /// attempt then ends as a follower's would.
    fn hand_on(&mut self) -> Result<()> {
        let (Some(peering), Some(plan)) = (self.peering, self.plan.peering) else {
            return Ok(());
        };
        let Some((to, term)) = peering.shared.handoff() else {
            return Ok(());
        };
        if self.handed.as_ref() == Some(&(to.clone(), term)) {
            return Err(Following { leader: to, term }.into());
        }
        let side = self.peer_side();
        if let Some(session) = self.session.as_mut() {
            session.set_leadership(
                Some(crate::peering::Leadership {
                    leader: to.clone(),
                    term,
                    ttl: plan.ttl,
                }),
                side,
            );
            if let Err(error) = session.present_lease() {
                crate::complain!(
                    "[{}] unable to hand the lease on: {error:#}",
                    self.plan.display()
                );
            }
        }
        self.handed = Some((to.clone(), term));
        peering.shared.handed_one();
        Err(Following { leader: to, term }.into())
    }

    /// Peering: the supervisor's role as it applies to this plan — `Off`
    /// for a plan in a plain mode whatever the supervisor holds.
    fn role(&self) -> crate::peering::Role {
        match (self.peering, self.plan.peering) {
            (Some(peering), Some(_)) => peering.role(),
            _ => crate::peering::Role::Off,
        }
    }

    /// Peering, before an attempt: refuses to run while the supervisor
    /// follows, and otherwise hands the session the leadership to present.
    /// Returns the leadership, so the caller can push files after the
    /// session exists.
    fn leadership(&self) -> Result<Option<crate::peering::Leadership>> {
        let (Some(peering), Some(plan)) = (self.peering, self.plan.peering) else {
            return Ok(None);
        };
        match peering.role() {
            crate::peering::Role::Off => Ok(None),
            crate::peering::Role::Follower { leader, term } => {
                Err(Following { leader, term }.into())
            }
            crate::peering::Role::Leader { leader, term } => Ok(Some(crate::peering::Leadership {
                leader,
                term,
                ttl: plan.ttl,
            })),
        }
    }

    /// Peering, once the session is up: pushes the follower's files when
    /// they have changed since the last push.
    fn push_files(&mut self) -> Result<()> {
        let Some(peering) = self.peering else {
            return Ok(());
        };
        let files = peering.pushed_files(
            &self.plan.beta_spec(),
            &self.plan.group,
            &self.plan.identifier(),
        )?;
        let mut hasher = blake3::Hasher::new();
        for (name, bytes) in &files {
            hasher.update(name.as_bytes());
            hasher.update(&(bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        }
        let digest = *hasher.finalize().as_bytes();
        if self.pushed == Some(digest) {
            return Ok(());
        }
        let session = self.session.as_mut().expect("the session exists");
        session.push_peering_files(&files)?;
        self.pushed = Some(digest);
        Ok(())
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
        // A connection that died underneath the session is reconnected
        // once, right now, before anything is recorded. A laptop waking
        // from sleep finds every connection it held dead; the reconnect
        // usually succeeds at once, and then nothing was ever wrong. If
        // it fails, *that* failure is recorded — the honest one, typed by
        // how the connection could not be made.
        let mut retried = false;
        let result = loop {
            let outcome = self.attempt_once();
            if let Err(error) = &outcome {
                if !retried && crate::transport::mux::ConnectionFailed::is_in(error) {
                    crate::debug!(
                        "[{}] the connection failed ({error:#}); reconnecting once",
                        self.plan.display()
                    );
                    retried = true;
                    self.session = None;
                    continue;
                }
            }
            break outcome;
        };
        if let Ok((digest, _)) = &result {
            self.cycles += digest.cycles;
        }
        // Peering: the alpha is back and level. A beta leads only while
        // the alpha is away, so one settled cycle with the alpha attached
        // hands the lead back to it.
        if let (Ok((_, report)), Some(peering)) = (&result, self.peering) {
            if self.peer_side() == crate::peering::PeerSide::Alpha
                && report.settled()
                && matches!(peering.role(), crate::peering::Role::Leader { .. })
            {
                if let Err(error) = peering.yield_to(crate::peering::ALPHA) {
                    crate::complain!("[{}] unable to yield: {error:#}", self.plan.display());
                }
            }
        }
        result
    }

    /// One attempt, as [`attempt`](Worker::attempt) makes it: connect if
    /// not connected, then run a cycle.
    fn attempt_once(&mut self) -> Result<(CycleDigest, CycleReport)> {
        (|| {
            // Peering first: a handoff is handed on, and a follower
            // connects nothing.
            self.hand_on()?;
            let leadership = self.leadership()?;
            if self.session.is_none() {
                // Connecting is its own phase because it is its own wait:
                // the first connection to a host installs the agent there,
                // and an unreachable one is where a session sits until it
                // times out.
                self.progress.enter(crate::progress::Phase::Connecting);
                crate::debug!("[{}] connecting", self.plan.display());
                let connecting = std::time::Instant::now();
                let mut session = connect(
                    self.plan,
                    self.state_root,
                    self.pool,
                    self.peering.map(|peering| peering.directory()),
                )?;
                crate::debug!(
                    "[{}] connected in {:.2}s",
                    self.plan.display(),
                    connecting.elapsed().as_secs_f64()
                );
                session.set_progress(self.progress.clone());
                self.session = Some(session);
            }
            let leading = leadership.is_some();
            let side = self.peer_side();
            let session = self.session.as_mut().expect("the session was just created");
            session.set_leadership(leadership, side);
            if leading {
                // The lease before anything else: a host another leader
                // holds refuses it here, and this attempt ends without
                // having written a byte — not even the follower's files,
                // which would otherwise overwrite the real leader's. The
                // attached alpha gets no files: it has its own.
                session.present_lease()?;
                if side == crate::peering::PeerSide::Beta {
                    self.push_files()?;
                }
            }
            let session = self.session.as_mut().expect("the session was just created");
            if std::mem::take(&mut self.verify_pending) {
                session.request_verify();
            }
            let started = std::time::Instant::now();
            let outcome = run_cycles(session, &self.plan.display());
            let elapsed = started.elapsed();
            // A cycle that found nothing is not worth a line even here.
            // At a five-second interval an idle session would otherwise
            // write seventeen thousand lines a day saying so, and the log
            // rotates on size — debug would evict the very evidence it was
            // turned on to collect. Anything that did work, took long
            // enough to be interesting, or failed still gets its line.
            let worth_saying = match &outcome {
                Err(_) => true,
                Ok((digest, _)) => {
                    digest.alpha_transitions > 0
                        || digest.beta_transitions > 0
                        || digest.conflicts > 0
                        || digest.problems > 0
                        || digest.cycles > 1
                        || elapsed >= QUIET_CYCLE_CEILING
                }
            };
            if worth_saying {
                crate::debug!(
                    "[{}] cycle {} in {:.2}s{}",
                    self.plan.display(),
                    match &outcome {
                        Ok(_) => "finished",
                        Err(_) => "failed",
                    },
                    elapsed.as_secs_f64(),
                    match &outcome {
                        Ok((digest, _)) => format!(
                            ": {} inner cycle(s), {} to alpha, {} to beta, {} conflict(s), \
                             {} blocked",
                            digest.cycles,
                            digest.alpha_transitions,
                            digest.beta_transitions,
                            digest.conflicts,
                            digest.problems
                        ),
                        Err(error) => format!(": {error:#}"),
                    }
                );
            }
            outcome
        })()
    }

    /// Concludes an attempt: records its status (while any held lock is
    /// still ours), then, on failure, drops the session so the next attempt
    /// reconnects from scratch. That closes this session's channels; a
    /// pooled agent process outlives them and is reaped only once its last
    /// channel and handle are gone.
    fn conclude(&mut self, result: &Result<(CycleDigest, CycleReport)>) -> Result<()> {
        // Peering: a refused lease steps the whole supervisor down, before
        // the status is written, so the record already says "follower".
        if let (Err(error), Some(peering)) = (result, self.peering) {
            if let Some(fenced) = error.downcast_ref::<crate::peering::Fenced>() {
                peering.step_down(&fenced.current);
            }
        }
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
        // Every change pays one QUIET before its cycle starts, and a burst
        // keeps paying them, up to SETTLE, while it keeps landing. QUIET
        // is therefore a floor under every edit's latency: at 20 ms it was
        // nearly half of a single editor's p50 (47 ms; 25 ms at 5), and
        // with ten editors the tree is never quiet, so every edit paid it
        // (p50 48 → 22 ms). Shorter still buys little: with no window at
        // all, a 1,000-file burst that one cycle used to absorb takes
        // three, and half again as much cycle work, for the same wall time
        // to convergence; at 25/5 it takes two. Measured 2026-09-23.
        const SETTLE: Duration = Duration::from_millis(25);
        const QUIET: Duration = Duration::from_millis(5);
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
            crate::note!("[{}] paused", self.plan.display());
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
                crate::complain!("[{}] unable to reset: {error:#}", self.plan.display());
                return;
            }
        };
        if let Err(error) =
            crate::session::ancestor::AncestorStore::reset(&state_directory.join("ancestor"))
        {
            crate::complain!(
                "[{}] unable to reset the ancestor: {error:#}",
                self.plan.display()
            );
            return;
        }
        if self.verbose {
            crate::note!(
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
            mode: self.plan.mode_name().to_owned(),
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
            moved_files: self.progress.moved().0,
            moved_bytes: self.progress.moved().1,
            role: self.role().label().to_owned(),
            term: self.role().term(),
            alert_after_seconds: None,
        };
        self.publish(&status);
        if let Err(error) = write_status(self.state_root, &self.plan.identifier(), &status) {
            crate::complain!(
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
                    crate::complain!("[{display}] skipped: {error:#}");
                }
                return Ok(());
            }
        }
        let mut status = SessionStatus {
            group: self.plan.group.clone(),
            host: self.plan.host.clone(),
            alpha: self.plan.alpha_spec.clone(),
            beta: self.plan.beta_spec(),
            mode: self.plan.mode_name().to_owned(),
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
            moved_files: self.progress.moved().0,
            moved_bytes: self.progress.moved().1,
            role: self.role().label().to_owned(),
            term: self.role().term(),
            alert_after_seconds: None,
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
                        crate::note!(
                            "[{display}] synchronized: {} change(s) to alpha, {} change(s) to beta",
                            digest.alpha_transitions,
                            digest.beta_transitions
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
                let halt = error.downcast_ref::<crate::session::SafetyHalt>();
                status.alert_after_seconds = halt
                    .and_then(|halt| halt.alert_after())
                    .map(|after| after.as_secs());
                status.state = if halt.is_some() {
                    "halted"
                } else if error.downcast_ref::<Following>().is_some()
                    || error.downcast_ref::<crate::peering::Fenced>().is_some()
                {
                    // Not trouble: another controller leads, and this one
                    // is waiting its turn. The alerter does not know the
                    // word, so it never wakes anyone for it.
                    "following"
                } else if error
                    .downcast_ref::<crate::endpoint::remote::Unreachable>()
                    .is_some()
                    || crate::transport::mux::ConnectionFailed::is_in(error)
                {
                    // A connection that failed underneath the session is
                    // the host being away, however the transport worded
                    // it — and the alerter's patience for a host being
                    // away is the right patience for it.
                    "unreachable"
                } else {
                    "errored"
                }
                .into();
                status.error = Some(format!("{error:#}"));
                if self.verbose {
                    crate::complain!("[{display}] error: {error:#}");
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

/// How long an otherwise uneventful cycle must take before it is worth a
/// debug line. A cycle that changed nothing and finished promptly says
/// nothing; one that changed nothing and took seconds is the shape of a
/// problem worth seeing, and is rare enough to be cheap to record.
const QUIET_CYCLE_CEILING: Duration = Duration::from_secs(1);

/// Runs one cycle plus bounded follow-ups while staged content is reported
/// missing. Exhausting the cap with content *still* missing is a failure,
/// not a quiet success: the destination is churning faster than content can
/// be transferred, and automation must not read that as "synchronized".
fn run_cycles(session: &mut Session, display: &str) -> Result<(CycleDigest, CycleReport)> {
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
        for request in &report.missing_staged {
            crate::debug!(
                "[{}] staged content missing: {} ({}){}",
                display,
                request.path,
                request
                    .digest
                    .iter()
                    .take(4)
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>(),
                match previously_missing.get(&request.path) == Some(&request.digest) {
                    true => " — the same content as the previous cycle",
                    false => " — asking again",
                }
            );
        }
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
fn connect(
    plan: &SessionPlan,
    state_root: &Path,
    pool: &AgentPool,
    peering_directory: Option<&Path>,
) -> Result<Session> {
    let identifier = plan.identifier();
    let state_directory = state_root.join("sessions").join(&identifier);

    // The lock comes first: a conflicting session must be discovered before
    // any endpoint work happens, not after spawning SSH and handshaking
    // with a remote agent — endpoint construction is expensive, can block
    // on the network, and has remote side effects.
    let lock = SessionLock::acquire(state_directory.clone())?;
    // Peering: a copy of this session's ancestor that a leader pushed here
    // and that is newer than what this directory holds is adopted — under
    // the lock, before the store is opened. A beta that starts to lead
    // seeds its session this way; an alpha that gets the lead back takes
    // up what the beta recorded meanwhile.
    if let (Some(directory), Some(_)) = (peering_directory, plan.peering) {
        if crate::peering::adopt_newer_copy(state_root, directory, &identifier)? {
            crate::note!(
                "[{}] adopted the ancestor copy a leader pushed",
                plan.display()
            );
        }
    }
    // The pair lock is independent of the state root, so two supervisors
    // pointed at different state directories cannot own the same trees.
    let pair_lock =
        crate::session::EndpointPairLock::acquire(&plan.alpha_identity, &plan.beta_identity)?;
    let (alpha, beta) = open_endpoints(plan, state_root, pool)?;
    let mut session = Session::with_lock(alpha, beta, plan.mode, lock)?;
    session.hold(pair_lock);
    session.set_power_durability(plan.power_durability);
    session.set_ignore_mounts(plan.ignore_mounts);
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
                return Err(crate::session::SafetyHalt::AlphaRootMissing(
                    resolved.display().to_string(),
                )
                .into());
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
                        one_shot: false,
                        ignore_mounts: plan.ignore_mounts,
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
                    ignore_mounts: plan.ignore_mounts,
                };
                // Peering: an endpoint reached by attachment is a
                // connection the peer opened to this supervisor. None
                // waiting means the peer has not dialed in, which is the
                // ordinary unreachable case and backs off like one.
                if let Some(name) = crate::peering::attached_name(destination) {
                    let Some(connection) = pool.take_attachment(name) else {
                        return Err(crate::endpoint::remote::Unreachable {
                            destination: destination.clone(),
                        }
                        .into());
                    };
                    return Ok(Box::new(
                        crate::endpoint::remote::RemoteEndpoint::connect(connection, initialize)
                            .map_err(|error| {
                                crate::complain!(
                                    "the attached {name} could not be connected: {error:#}"
                                );
                                crate::endpoint::remote::Unreachable {
                                    destination: destination.clone(),
                                }
                            })?,
                    ));
                }
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
    /// An edit to the configuration the running supervisor refused: the
    /// sessions run on under the last good one until it is fixed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_notice: Option<reload::Notice>,
    /// A supervisor is running but is another build, so it cannot say
    /// what it is doing; what to tell someone about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor_mismatch: Option<String>,
}

/// One group's report.
#[derive(Clone, Debug, Serialize)]
pub struct GroupReport {
    /// Peering: the supervisor's role for this group as its sessions last
    /// recorded it — `leader`, `follower`, or empty — and the term.
    pub role: String,
    pub term: u64,
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
    /// The alerting conditions this session is in, decided by the same
    /// rule the supervisor's hook uses, so a second reader of the report
    /// — the tray — cannot disagree with it about what needs a person.
    /// Not part of the JSON document: `state`, `conflicts`, `blocked` and
    /// `error` already carry the facts, and this is their reading.
    #[serde(skip)]
    pub alerts: Vec<crate::alerts::Alert>,
    /// The session's own patience, when its failure asks for one.
    #[serde(skip)]
    pub alert_after: Option<std::time::Duration>,
    /// How to describe the session in one line when it is alerting.
    #[serde(skip)]
    pub alert_summary: String,
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
            crate::complain!("[{display}] {} conflict(s)", conflicts.len());
        }
        if !blocked.is_empty() {
            crate::complain!("[{display}] {} blocked path(s)", blocked.len());
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
            crate::complain!("[{display}] {what}: {entry}");
        }
        if appeared.len() > NAMED {
            crate::complain!("[{display}] {what}: and {} more", appeared.len() - NAMED);
        }
        if cleared > 0 {
            crate::complain!("[{display}] {what}: {cleared} cleared, {} left", now.len());
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
                .map(|error| {
                    (
                        plan.display(),
                        format!(
                            "{error:#}; the first cycle rebuilds it if both sides match, \
                         and halts otherwise (`autobahn doctor {}` shows which)",
                            plan.group
                        ),
                    )
                })
        })
        .collect()
}

/// Builds the report for a set of plans.
pub fn status_report(plans: &[&SessionPlan], state_root: &Path) -> StatusReport {
    // One round trip serves both questions: a supervisor that answers is
    // running, and its answer is what every session is doing. One of
    // another build is running too, and can say only that.
    let probe = control::probe(state_root);
    let running = probe.is_running();
    let mismatch = probe.mismatch_message();
    let live = probe.progress();
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
        let (role, term) = status
            .as_ref()
            .map(|status| (status.role.clone(), status.term))
            .unwrap_or_default();
        let session = match status {
            None => SessionReport {
                host: plan.host.clone(),
                beta: plan.beta_spec(),
                mode: plan.mode_name().to_owned(),
                state: "never-run".into(),
                cycles: 0,
                age_seconds: None,
                conflicts: Vec::new(),
                blocked: Vec::new(),
                error: None,
                progress: progress.clone(),
                // Not in trouble; it has simply not started.
                alerts: Vec::new(),
                alert_after: None,
                alert_summary: String::new(),
            },
            Some(status) => SessionReport {
                host: plan.host.clone(),
                beta: plan.beta_spec(),
                mode: plan.mode_name().to_owned(),
                state: classify_state(&status),
                cycles: status.cycles,
                age_seconds: Some(now.saturating_sub(status.updated_at)),
                conflicts: status.conflict_details.clone(),
                blocked: status.blocked.clone(),
                error: status.error.clone(),
                progress: progress.clone(),
                alerts: alerts_for(&status),
                alert_after: alert_after(&status),
                alert_summary: alert_summary(&status),
            },
        };
        match groups.last_mut() {
            Some(group) if group.name == plan.group => group.sessions.push(session),
            _ => groups.push(GroupReport {
                role: role.clone(),
                term,
                name: plan.group.clone(),
                alpha: plan.alpha_spec.clone(),
                sessions: vec![session],
            }),
        }
    }
    StatusReport {
        version: 4,
        supervisor_running: running,
        supervisor_mismatch: mismatch,
        // Only a running supervisor's refusal is news: the one that wrote
        // it is gone otherwise, and `start` checks the file itself.
        config_notice: running.then(|| reload::read_notice(state_root)).flatten(),
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

/// The session's own alert patience, when its recorded failure asks for
/// one longer than its state's.
pub fn alert_after(status: &SessionStatus) -> Option<Duration> {
    status.alert_after_seconds.map(Duration::from_secs)
}

/// How to describe a session's conditions in one line.
pub fn alert_summary(status: &SessionStatus) -> String {
    use crate::alerts::plural;
    let mut parts = Vec::new();
    match classify_state(status).as_str() {
        // Read after the host name: "boite refused the key".
        "unreachable" => {
            parts.push(unreachable_reason(status.error.as_deref().unwrap_or("")).to_owned())
        }
        // The word alone told nobody anything: "errored" is every failure
        // that is not a halt or an absent host. The message's last clause
        // is the diagnosis, and it fits on the line.
        state @ ("halted" | "errored") => match status.error.as_deref() {
            Some(error) if !error.is_empty() => {
                let clause = error.rsplit(": ").next().unwrap_or(error);
                parts.push(format!("{state}: {clause}"));
            }
            _ => parts.push(state.to_owned()),
        },
        _ => {
            if !status.conflicts.is_empty() {
                parts.push(plural(status.conflicts.len(), "conflict"));
            }
            if !status.blocked.is_empty() {
                parts.push(plural(status.blocked.len(), "blocked path"));
            }
        }
    }
    parts.join(", ")
}

/// Why a destination could not be reached, from ssh's own words — which
/// the error carries, now that a spawned agent's stderr is kept rather
/// than discarded. A sleeping laptop and a rejected key both used to read
/// "unreachable", and they ask for opposite things: wait, or go fix it.
pub fn unreachable_reason(error: &str) -> &'static str {
    if error.contains("Permission denied") {
        "refused the key"
    } else if error.contains("HOST IDENTIFICATION HAS CHANGED")
        || error.contains("Host key verification failed")
    {
        "changed its host key"
    } else if error.contains("Could not resolve hostname")
        || error.contains("Name or service not known")
        || error.contains("nodename nor servname provided")
    {
        "does not resolve"
    } else {
        "is unreachable"
    }
}

/// Describes a conflict's sides from the changes reconciliation recorded
/// for it: the newest node each side's changes carry at the conflict's
/// root, or absence.
fn conflict_detail(conflict: &crate::tree::Conflict) -> ConflictDetail {
    use crate::tree::{path_join, Change, Content, Node};

    // Everything on this side that synchronization cannot carry, found by
    // walking the changes rather than by looking at the conflict's root.
    // The blocking entry is usually *not* at the root — a directory is
    // refused because of one unreadable file somewhere beneath it — so the
    // root alone can never name the cause.
    fn unsynchronizable(changes: &[Change]) -> Option<Unsynchronizable> {
        fn walk(
            path: &str,
            node: &Node,
            found: &mut Vec<(String, String)>,
            faults: &mut Vec<(String, String)>,
        ) {
            match &node.content {
                Content::Directory(children) => {
                    for child in children.iter() {
                        walk(&path_join(path, &child.name), child, found, faults);
                    }
                }
                Content::Problematic { message } => {
                    let entry = (path.to_owned(), message.clone());
                    faults.push(entry.clone());
                    found.push(entry);
                }
                Content::Untracked => {
                    found.push((path.to_owned(), "excluded from synchronization".to_owned()))
                }
                _ => {}
            }
        }
        let mut found = Vec::new();
        let mut faults = Vec::new();
        for change in changes {
            if let Some(node) = &change.new {
                walk(&change.path, node, &mut found, &mut faults);
            }
        }
        // An unreadable entry is preferred as the example over an excluded
        // one. Both stop the propagation, but they ask for opposite things:
        // an exclusion is policy the reader chose and can leave alone, an
        // unreadable entry is a fault to go and fix. Children are
        // name-sorted, so without this rule an ignored file beginning with
        // "a" hides the broken one two directories down.
        let (example, reason) = faults.first().or_else(|| found.first())?.clone();
        Some(Unsynchronizable {
            entries: found.len() as u64,
            example,
            reason,
        })
    }

    let side = |changes: &[Change]| -> ConflictSide {
        // The change at the conflict root itself describes the side; a
        // conflict rooted at a directory carries changes beneath it, and
        // the root's own entry is what the reader wants to see.
        let node = changes
            .iter()
            .find(|change| change.path == conflict.root)
            .or_else(|| changes.first())
            .and_then(|change| change.new.as_ref());
        let blocking = unsynchronizable(changes);
        match node {
            None => ConflictSide {
                unsynchronizable: blocking,
                ..Default::default()
            },
            Some(node) => match &node.content {
                Content::File { metadata, .. } => ConflictSide {
                    present: true,
                    kind: "file".into(),
                    size: metadata.size,
                    mtime_seconds: metadata.mtime_seconds,
                    unsynchronizable: blocking,
                },
                Content::Directory(_) => ConflictSide {
                    present: true,
                    kind: "directory".into(),
                    unsynchronizable: blocking,
                    ..Default::default()
                },
                Content::Symlink { .. } => ConflictSide {
                    present: true,
                    kind: "symlink".into(),
                    unsynchronizable: blocking,
                    ..Default::default()
                },
                // Named for what it is rather than as "other". These are
                // the two ways content exists without being synchronized,
                // and they call for opposite responses: an ignore rule is
                // policy the reader chose, an unreadable entry is a fault
                // to go and fix.
                Content::Untracked => ConflictSide {
                    present: true,
                    kind: "excluded from synchronization".into(),
                    unsynchronizable: blocking,
                    ..Default::default()
                },
                Content::Problematic { .. } => ConflictSide {
                    present: true,
                    kind: "unreadable".into(),
                    unsynchronizable: blocking,
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
                let (alerts, summary, after) = match &status {
                    Some(status) => (
                        alerts_for(status),
                        alert_summary(status),
                        alert_after(status),
                    ),
                    None => (Vec::new(), String::new(), None),
                };
                SessionAlerts {
                    group: plan.group.clone(),
                    host: plan.host.clone(),
                    alerts,
                    summary,
                    after,
                }
            })
            .collect();

        if let Some(fire) = alerter.observe(&sessions, std::time::Instant::now()) {
            let commands = alerter.commands(&fire);
            let (summary, detail, count, states) = match &fire {
                Fire::Alert {
                    summary,
                    detail,
                    alerts,
                    sessions,
                    ..
                } => (
                    summary.clone(),
                    detail.clone(),
                    *sessions,
                    alerts
                        .iter()
                        .map(|alert| alert.name())
                        .collect::<Vec<_>>()
                        .join(","),
                ),
            };
            let environment = vec![
                ("AUTOBAHN_SUMMARY".to_owned(), summary),
                ("AUTOBAHN_DETAIL".to_owned(), detail),
                ("AUTOBAHN_ALERT_COUNT".to_owned(), count.to_string()),
                ("AUTOBAHN_STATES".to_owned(), states),
                // An absolute path, because a hook runs with the
                // service's working directory, not the reader's.
                (
                    "AUTOBAHN_ICON".to_owned(),
                    crate::icon::ensure(state_root)
                        .map(|path| path.display().to_string())
                        .unwrap_or_default(),
                ),
                (
                    "AUTOBAHN_EVENT".to_owned(),
                    match fire {
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
pub(crate) fn write_status(
    state_root: &Path,
    identifier: &str,
    status: &SessionStatus,
) -> Result<()> {
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
    fn a_rejected_key_is_not_a_sleeping_laptop() {
        assert_eq!(
            unreachable_reason(
                "unable to reach boite: claude@boite: Permission denied (publickey)."
            ),
            "refused the key"
        );
        assert_eq!(
            unreachable_reason("WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!"),
            "changed its host key"
        );
        assert_eq!(
            unreachable_reason("ssh: Could not resolve hostname fny.voltai.party"),
            "does not resolve"
        );
        assert_eq!(
            unreachable_reason("ssh: connect to host boite port 22: Operation timed out"),
            "is unreachable"
        );
    }

    /// The entry that blocks a conflict is almost never the entry the
    /// conflict is named after: a directory is refused because of one file
    /// somewhere beneath it. Looking only at the root therefore reports
    /// two ordinary sides and no cause at all.
    #[test]
    fn a_conflict_names_the_content_that_cannot_be_carried() {
        use crate::tree::{Change, Conflict, Content, Node};
        let unreadable = Node {
            name: "socket".into(),
            content: Content::Problematic {
                message: "unsupported entry type".into(),
            },
        };
        let excluded = Node {
            name: "big.bin".into(),
            content: Content::Untracked,
        };
        let tree = Node::directory(
            "happy",
            vec![Node::directory("packages", vec![unreadable]), excluded],
        );
        let detail = conflict_detail(&Conflict {
            root: "happy".into(),
            alpha_changes: Vec::new(),
            beta_changes: vec![Change {
                path: "happy".into(),
                old: None,
                new: Some(tree),
            }],
        });

        // The side is still a directory; what it *holds* is the diagnosis.
        assert_eq!(detail.beta.kind, "directory");
        let blocking = detail.beta.unsynchronizable.expect("a cause is recorded");
        assert_eq!(blocking.entries, 2);
        // `big.bin` sorts first and would be found first, but it is merely
        // excluded. The unreadable entry is the one worth naming.
        assert_eq!(blocking.example, "happy/packages/socket");
        assert_eq!(blocking.reason, "unsupported entry type");

        // A side with nothing of the kind says nothing, rather than
        // reporting an empty cause.
        assert!(detail.alpha.unsynchronizable.is_none());
    }

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

        let error = run_cycles(&mut session, "test")
            .expect_err("unappearing content must surface as an error");
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
            run_cycles(&mut session, "test").expect("churn must not be reported as a failure");
        assert!(report.missing_staged_files, "the flag must survive the cap");
        assert_eq!(digest.cycles, MAXIMUM_FOLLOW_UP_CYCLES as u64 + 1);
    }

    /// The notification for an errored session carries the error's last
    /// clause, not the bare word.
    #[test]
    fn an_errored_summary_says_what_the_error_was() {
        let mut status = SessionStatus {
            group: "g".into(),
            host: "boite".into(),
            alpha: "~/a".into(),
            beta: "boite:~/a".into(),
            mode: "two-way-conflict".into(),
            state: "errored".into(),
            cycles: 3,
            last_alpha_transitions: 0,
            last_beta_transitions: 0,
            conflicts: Vec::new(),
            conflict_details: Vec::new(),
            blocked: Vec::new(),
            error: Some(
                "beta scan failed: the agent connection has failed: connection closed".into(),
            ),
            updated_at: 12345,
            alpha_entries: 0,
            beta_entries: 0,
            moved_files: 0,
            moved_bytes: 0,
            role: String::new(),
            term: 0,
            alert_after_seconds: None,
        };
        assert_eq!(alert_summary(&status), "errored: connection closed");
        status.error = None;
        assert_eq!(alert_summary(&status), "errored");
        status.state = "halted".into();
        status.error = Some("halted: the synchronization root was deleted on one side".into());
        assert_eq!(
            alert_summary(&status),
            "halted: the synchronization root was deleted on one side"
        );
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
            moved_files: 0,
            moved_bytes: 0,
            role: String::new(),
            term: 0,
            alert_after_seconds: None,
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
