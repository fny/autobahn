//! A peer's `watch`: follow the lease, and take the lead when it is time.
//!
//! A host that a leader has pushed a name to runs this instead of a plain
//! supervisor. It has no configuration of its own; it runs the leader's,
//! turned around so that it is the alpha and every other beta stays a
//! beta. The configured alpha is not in its star — the alpha is never
//! dialed — so while a beta leads, the alpha's session waits for the
//! alpha to dial in (a later phase).
//!
//! The loop is a two-state machine:
//!
//! - **Following.** Read the lease the leader keeps renewing through the
//!   agent. While it is fresh, or stale for less than this host's wait,
//!   sleep an interval and look again. The wait is the configured
//!   `failover_after` plus one lease lifetime for every beta ahead of this
//!   one in the configuration's order, so the first live beta acts first
//!   without any of them being asked. A blip never reaches the wait.
//! - **Leading.** Write a lease at the next term, run a supervisor over the
//!   turned-around star, and watch its role. The supervisor's sessions
//!   present the lease to every other beta on their first cycle; a host
//!   already taken by a newer term refuses it, and the supervisor steps
//!   down, which ends this state and starts the first one again.
//!
//! Two candidates can act at once only when the stagger fails them —
//! clocks a lifetime apart, say. Both then present the same term to the
//! same hosts, each host keeps the first and refuses the second, and the
//! refused one steps down. The fence, not the stagger, is the guarantee.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::peering::{self, Lease, Role};

/// How often a leading peer checks whether it is still leading.
const ROLE_POLL: Duration = Duration::from_millis(250);

/// Runs a peer until `stop`: following, then leading, then following.
pub fn run(directory: &Path, state_root: &Path, verbose: bool, stop: &AtomicBool) -> Result<()> {
    while !stop.load(Ordering::Relaxed) {
        // Re-read every time round: the leader pushes the configuration
        // whenever it changes, and the name never changes but is cheap.
        let Some((configuration, name)) = peering::pushed_configuration(directory)? else {
            anyhow::bail!(
                "{} holds no pushed configuration; this host is not a peer",
                directory.display()
            );
        };
        let star = peering::derive_star(&configuration, &name, directory)?;
        match follow(directory, &star, stop)? {
            Followed::Stopped => return Ok(()),
            Followed::TakeOver { term } => {
                let lease = Lease::new(&star.name, term, star.timing.ttl);
                peering::write_lease(directory, &lease)?;
                crate::note!(
                    "peering: the lease went stale; taking the lead as {} at term {term}",
                    star.name
                );
                lead(directory, state_root, star, term, verbose, stop)?;
            }
        }
    }
    Ok(())
}

/// What following ended with.
enum Followed {
    /// The stop flag.
    Stopped,
    /// The lease has been stale for this host's wait; lead at `term`.
    TakeOver { term: u64 },
}

/// Watches the lease until it is time to act, or to stop.
fn follow(directory: &Path, star: &peering::FollowerStar, stop: &AtomicBool) -> Result<Followed> {
    let wait = peering::takeover_wait(star.position, &star.timing);
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(Followed::Stopped);
        }
        let lease = peering::read_lease(directory)?;
        let now = peering::now_seconds();
        let standing = match &lease {
            // No lease was ever written: the leader has not been here
            // yet, and there is nothing to take over from.
            None => Standing::Fresh,
            Some(lease) if !lease.is_stale_at(now) => Standing::Fresh,
            Some(lease) if lease.stale_for_at(now) < wait => Standing::Stale,
            Some(_) => Standing::Due,
        };
        write_status(directory, star, lease.as_ref(), now, &standing)?;
        if let (Standing::Due, Some(lease)) = (&standing, &lease) {
            return Ok(Followed::TakeOver {
                term: lease.term + 1,
            });
        }
        super::sleep_interruptible(star.interval, stop);
    }
}

/// Where a follower stands with respect to the lease.
enum Standing {
    Fresh,
    Stale,
    Due,
}

/// Leads until the supervisor steps down, or `stop`.
fn lead(
    directory: &Path,
    state_root: &Path,
    star: peering::FollowerStar,
    term: u64,
    verbose: bool,
    stop: &AtomicBool,
) -> Result<()> {
    let name = star.name.clone();
    let ttl = star.timing.ttl;
    let context = super::PeeringContext::for_leader(directory.to_path_buf(), name.clone(), term);
    let supervisor =
        super::Supervisor::new(star.plans, state_root.to_path_buf(), verbose).with_peering(context);
    let inner_stop = AtomicBool::new(false);
    std::thread::scope(|scope| -> Result<()> {
        let watcher = scope.spawn(|| supervisor.run_watch(&inner_stop));
        // The supervisor runs until it is told to stop; this thread tells
        // it to, when it has stepped down or the process is stopping.
        // Meanwhile it renews this host's own lease: nobody dials a
        // leader, so nobody else keeps that file fresh, and a restart
        // would otherwise read it as stale and take the lead from itself
        // at a new term.
        let mut renewed = std::time::Instant::now();
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            if watcher.is_finished() {
                break;
            }
            if let Role::Follower { leader, term } = supervisor.role() {
                crate::note!("peering: {leader} leads at term {term}; following again");
                break;
            }
            if renewed.elapsed() >= ttl / 2 {
                if let Err(error) = peering::write_lease(directory, &Lease::new(&name, term, ttl)) {
                    crate::complain!("peering: unable to renew the lease locally: {error:#}");
                }
                renewed = std::time::Instant::now();
            }
            std::thread::sleep(ROLE_POLL);
        }
        inner_stop.store(true, Ordering::Relaxed);
        match watcher.join() {
            Ok(result) => result,
            Err(_) => anyhow::bail!("the supervisor panicked"),
        }
    })
}

/// What `status` reads on a peer while it follows: a small file beside
/// the lease, rewritten every interval.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct FollowerStatus {
    /// This host's name in the star.
    pub name: String,
    /// The position in the order of succession.
    pub position: usize,
    /// The lease as last read, if any.
    pub lease: Option<Lease>,
    /// `fresh`, `stale`, or `due`.
    pub standing: String,
    /// How long the lease has been stale, in seconds.
    pub stale_seconds: u64,
    /// How long this host waits past a stale lease before it leads.
    pub wait_seconds: u64,
    /// When this was written.
    pub updated_at: u64,
}

/// The follower status file's name.
pub const STATUS_FILE: &str = "follower.json";

fn write_status(
    directory: &Path,
    star: &peering::FollowerStar,
    lease: Option<&Lease>,
    now: u64,
    standing: &Standing,
) -> Result<()> {
    let status = FollowerStatus {
        name: star.name.clone(),
        position: star.position,
        lease: lease.cloned(),
        standing: match standing {
            Standing::Fresh => "fresh",
            Standing::Stale => "stale",
            Standing::Due => "due",
        }
        .to_owned(),
        stale_seconds: lease
            .map(|lease| lease.stale_for_at(now).as_secs())
            .unwrap_or(0),
        wait_seconds: peering::takeover_wait(star.position, &star.timing).as_secs(),
        updated_at: now,
    };
    let bytes = serde_json::to_vec_pretty(&status).context("unable to encode the status")?;
    let path = directory.join(STATUS_FILE);
    let temporary = directory.join(format!(".{STATUS_FILE}.{}.tmp", std::process::id()));
    std::fs::write(&temporary, bytes)
        .with_context(|| format!("unable to write {}", temporary.display()))?;
    std::fs::rename(&temporary, &path)
        .with_context(|| format!("unable to move {} into place", path.display()))?;
    Ok(())
}

/// Reads the follower status a peer last wrote, if it is a peer.
pub fn read_status(directory: &Path) -> Result<Option<FollowerStatus>> {
    let path = directory.join(STATUS_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("unable to read {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("unable to read {}", path.display())),
    }
}

/// Whether this machine is a peer: a leader has pushed it a name.
pub fn is_peer(directory: &Path) -> bool {
    directory.join("name").is_file()
}

/// The peering directory, for callers that only have the default.
pub fn directory() -> Result<PathBuf> {
    peering::directory()
}
