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
    // The attach socket: the alpha dials in here, and its connection
    // becomes the endpoint of the session that has the alpha's side.
    let socket = directory.join(peering::ATTACH_SOCKET);
    let _ = std::fs::remove_file(&socket);
    let listener = std::os::unix::net::UnixListener::bind(&socket)
        .with_context(|| format!("unable to listen at {}", socket.display()))?;
    listener
        .set_nonblocking(true)
        .context("unable to configure the attach socket")?;
    std::thread::scope(|scope| -> Result<()> {
        let watcher = scope.spawn(|| supervisor.run_watch(&inner_stop));
        let acceptor = &inner_stop;
        let supervisor_ref = &supervisor;
        scope.spawn(move || {
            while !acceptor.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if let Err(error) = accept_attachment(stream, supervisor_ref) {
                            crate::complain!("peering: an attachment was refused: {error:#}");
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(ROLE_POLL);
                    }
                    Err(error) => {
                        crate::complain!("peering: the attach socket failed: {error:#}");
                        return;
                    }
                }
            }
        });
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
                // Only a lease that still names this peer at this term is
                // renewed. A handoff writes the next leader's lease here
                // while the sessions are still passing it on; renewing
                // over that would hand the lead back to nobody and have
                // this peer take it up again from itself.
                let still_mine = peering::read_lease(directory)
                    .ok()
                    .flatten()
                    .is_some_and(|lease| lease.leader == name && lease.term == term);
                if still_mine {
                    if let Err(error) =
                        peering::write_lease(directory, &Lease::new(&name, term, ttl))
                    {
                        crate::complain!("peering: unable to renew the lease locally: {error:#}");
                    }
                }
                renewed = std::time::Instant::now();
            }
            std::thread::sleep(ROLE_POLL);
        }
        inner_stop.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_file(&socket);
        match watcher.join() {
            Ok(result) => result,
            Err(_) => anyhow::bail!("the supervisor panicked"),
        }
    })
}

/// Reads an attachment's greeting — the peer's name on a line — and
/// offers the connection to the supervisor under that name.
fn accept_attachment(
    stream: std::os::unix::net::UnixStream,
    supervisor: &super::Supervisor,
) -> Result<()> {
    stream
        .set_nonblocking(false)
        .context("unable to configure the attachment")?;
    let mut reader = std::io::BufReader::new(
        stream
            .try_clone()
            .context("unable to clone the attachment")?,
    );
    let mut name = String::new();
    std::io::BufRead::read_line(&mut reader, &mut name).context("unable to read the greeting")?;
    let name = name.trim().to_owned();
    if name != peering::ALPHA {
        anyhow::bail!("{name:?} is not a peer that attaches");
    }
    crate::note!("peering: {name} attached");
    let connection = crate::transport::Connection::from_streams(Box::new(reader), Box::new(stream));
    supervisor.offer_attachment(&name, connection);
    Ok(())
}

/// The alpha's `watch` when its groups peer: lead until fenced, then
/// attach to whoever leads until the lease names the alpha again, and
/// lead again. The configured alpha is never dialed, so while a beta
/// leads the alpha makes itself an endpoint by dialing the beta.
pub fn run_alpha(
    config_path: &Path,
    directory: &Path,
    plans: &[crate::config::SessionPlan],
    alerts: &crate::alerts::AlertPlan,
    state_root: &Path,
    verbose: bool,
    stop: &AtomicBool,
) -> Result<()> {
    let interval = plans
        .iter()
        .map(|plan| plan.interval)
        .min()
        .unwrap_or(Duration::from_secs(5));
    let failover_after = plans
        .iter()
        .find_map(|plan| plan.peering)
        .map(|plan| plan.failover_after)
        .unwrap_or(crate::config::DEFAULT_PEERING_FAILOVER_AFTER);
    while !stop.load(Ordering::Relaxed) {
        let context =
            super::PeeringContext::for_alpha(config_path.to_path_buf(), directory.to_path_buf())?;
        match context.role() {
            Role::Leader { term, .. } => {
                crate::note!("peering: leading as the alpha at term {term}");
                let supervisor =
                    super::Supervisor::new(plans.to_vec(), state_root.to_path_buf(), verbose)
                        .with_alerts(alerts.clone())
                        .with_peering(context);
                let inner_stop = AtomicBool::new(false);
                std::thread::scope(|scope| -> Result<()> {
                    let watcher = scope.spawn(|| supervisor.run_watch(&inner_stop));
                    loop {
                        if stop.load(Ordering::Relaxed) || watcher.is_finished() {
                            break;
                        }
                        if let Role::Follower { leader, term } = supervisor.role() {
                            crate::note!("peering: {leader} leads at term {term}; attaching");
                            break;
                        }
                        std::thread::sleep(ROLE_POLL);
                    }
                    inner_stop.store(true, Ordering::Relaxed);
                    match watcher.join() {
                        Ok(result) => result,
                        Err(_) => anyhow::bail!("the supervisor panicked"),
                    }
                })?;
            }
            Role::Follower { leader, .. } => {
                // Attach, and serve as an agent until the leader lets go
                // — it does when it hands the lead back, or dies.
                let destination = peering::destination_of(&leader).to_owned();
                let argv = peering::attach_argv(&destination);
                crate::note!("peering: attaching to {leader}");
                if let Err(error) = crate::transport::attach_as_agent(&argv) {
                    crate::complain!("peering: the attachment to {leader} ended: {error:#}");
                }
                // The lease says whether the lead came back. A stale lease
                // from a beta that died is taken over as a beta would take
                // it — the alpha is the head of the order, so it waits only
                // the configured time.
                let now = peering::now_seconds();
                match peering::read_lease(directory)? {
                    Some(lease) if lease.leader == peering::ALPHA => {}
                    Some(lease)
                        if lease.is_stale_at(now) && lease.stale_for_at(now) >= failover_after =>
                    {
                        crate::note!(
                            "peering: the lease of {} went stale; leading again",
                            lease.leader
                        );
                        let ttl = Duration::from_secs(lease.ttl_seconds);
                        let next = peering::Lease::new(peering::ALPHA, lease.term + 1, ttl);
                        peering::write_lease(directory, &next)?;
                    }
                    _ => super::sleep_interruptible(interval, stop),
                }
            }
            Role::Off => anyhow::bail!("no plan is in a peering mode"),
        }
    }
    Ok(())
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
