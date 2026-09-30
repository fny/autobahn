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
//!   agent, and the star as the leader last pushed it. While the lease is
//!   fresh, or stale for less than this host's wait, sleep an interval and
//!   look again. The wait is the configured `failover_after` plus one
//!   lease lifetime for every beta ahead of this one in the configuration's
//!   order, so the first live beta acts first without any of them being
//!   asked. A blip never reaches the wait. A lease that names this host
//!   is led at once, at its term.
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

use super::ROLE_POLL;

/// Runs a peer until `stop`: following, then leading, then following.
pub fn run(directory: &Path, state_root: &Path, verbose: bool, stop: &AtomicBool) -> Result<()> {
    while !stop.load(Ordering::Relaxed) {
        let star = current_star(directory)?;
        let (followed, star) = follow(directory, star, stop)?;
        match followed {
            Followed::Stopped => return Ok(()),
            Followed::Resume { term } => {
                // The lease on this host names this peer: it led, and a
                // restart or a handoff to it brings it back. Admitted at
                // that term like any lease; the hosts that moved on since
                // refuse it, and this peer steps down again.
                let lease = Lease::new(&star.name, term, star.timing.ttl);
                if let peering::LeaseAnswer::Refused { current } =
                    peering::admit_lease(directory, &lease)?
                {
                    crate::note!(
                        "peering: {} took the lead at term {} first; following",
                        current.leader,
                        current.term
                    );
                    continue;
                }
                crate::note!(
                    "peering: the lease names {}; leading at term {term}",
                    star.name
                );
                lead(directory, state_root, star, term, verbose, stop)?;
            }
            Followed::TakeOver { term } => {
                // Admitted under the lease lock, as the agent admits any
                // lease: a rival that took this host at the same term or a
                // later one first wins, and this peer follows on.
                let lease = Lease::new(&star.name, term, star.timing.ttl);
                if let peering::LeaseAnswer::Refused { current } =
                    peering::admit_lease(directory, &lease)?
                {
                    crate::note!(
                        "peering: {} took the lead at term {} first; following",
                        current.leader,
                        current.term
                    );
                    continue;
                }
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

/// The star as the leader last pushed it to this host.
fn current_star(directory: &Path) -> Result<peering::FollowerStar> {
    let Some((configuration, name)) = peering::pushed_configuration(directory)? else {
        anyhow::bail!(
            "{} holds no pushed configuration; this host is not a peer",
            directory.display()
        );
    };
    peering::derive_star(&configuration, &name, directory)
}

/// What following ended with.
enum Followed {
    /// The stop flag.
    Stopped,
    /// The lease has been stale for this host's wait; lead at `term`.
    TakeOver { term: u64 },
    /// The lease names this host; lead at its term.
    Resume { term: u64 },
}

/// Watches the lease until it is time to act, or to stop. Returns the star
/// as it stood when following ended, which is what the takeover runs.
///
/// The leader keeps pushing its configuration while this host follows, so
/// the star is derived again from what is pushed every time round: a
/// takeover runs the configuration as it last stood, and waits as long as
/// that configuration says. A push that does not derive — half written, or
/// from a leader with a different idea of the star — leaves the last star
/// that did in place, and says so once.
fn follow(
    directory: &Path,
    mut star: peering::FollowerStar,
    stop: &AtomicBool,
) -> Result<(Followed, peering::FollowerStar)> {
    let mut refused: Option<String> = None;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok((Followed::Stopped, star));
        }
        match current_star(directory) {
            Ok(current) => {
                star = current;
                refused = None;
            }
            Err(error) => {
                let message = format!("{error:#}");
                if refused.as_ref() != Some(&message) {
                    crate::complain!(
                        "peering: keeping the star as it was; the pushed one does not derive: \
                         {message}"
                    );
                    refused = Some(message);
                }
            }
        }
        let wait = peering::takeover_wait(star.position, &star.timing);
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
        write_status(directory, &star, lease.as_ref(), now, &standing)?;
        match (&standing, &lease) {
            (_, Some(lease)) if lease.leader == star.name => {
                return Ok((Followed::Resume { term: lease.term }, star));
            }
            (Standing::Due, Some(lease)) => {
                return Ok((
                    Followed::TakeOver {
                        term: lease.term + 1,
                    },
                    star,
                ));
            }
            _ => {}
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
    let context =
        super::PeeringContext::for_leader(directory.to_path_buf(), name.clone(), term, ttl);
    let supervisor =
        super::Supervisor::new(star.plans, state_root.to_path_buf(), verbose).with_peering(context);
    let inner_stop = AtomicBool::new(false);
    // The attach socket: the alpha dials in here, and its connection
    // becomes the endpoint of the session that has the alpha's side.
    let socket = directory.join(peering::ATTACH_SOCKET);
    let listener = bind_attach_socket(directory, &socket)?;
    std::thread::scope(|scope| -> Result<()> {
        let watcher = scope.spawn(|| supervisor.run_watch(&inner_stop));
        let acceptor = &inner_stop;
        let supervisor_ref = &supervisor;
        scope.spawn(move || {
            serve_attachments(&listener, acceptor, GREETING_TIMEOUT, |name, connection| {
                crate::note!("peering: {name} attached");
                supervisor_ref.offer_attachment(&name, connection);
            })
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
                // renewed, and the check and the write are one step under
                // the lease lock. A handoff writes the next leader's lease
                // here while the sessions are still passing it on;
                // renewing over that would hand the lead back to nobody and
                // have this peer take it up again from itself.
                if let Err(error) =
                    peering::renew_own_lease(directory, &Lease::new(&name, term, ttl))
                {
                    crate::complain!("peering: unable to renew the lease locally: {error:#}");
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

/// How long a connection to the attach socket has to say who it is.
const GREETING_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest greeting read: a name, which is `alpha`.
const MAXIMUM_GREETING: u64 = 64;

/// Binds the attach socket as the control socket is bound: in a private
/// directory, the socket itself only the owner's, whatever the umask.
fn bind_attach_socket(directory: &Path, socket: &Path) -> Result<std::os::unix::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    crate::fsutil::private_dir(directory)
        .with_context(|| format!("unable to prepare {}", directory.display()))?;
    let _ = std::fs::remove_file(socket);
    let listener = std::os::unix::net::UnixListener::bind(socket)
        .with_context(|| format!("unable to listen at {}", socket.display()))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("unable to restrict {}", socket.display()))?;
    listener
        .set_nonblocking(true)
        .context("unable to configure the attach socket")?;
    Ok(listener)
}

/// Accepts attachments until `stop`, each on a thread of its own: a
/// connection that says nothing holds up only itself, until its greeting
/// times out, and never the alpha dialing in behind it. What greets
/// properly is handed to `attached` with its name.
fn serve_attachments(
    listener: &std::os::unix::net::UnixListener,
    stop: &AtomicBool,
    timeout: Duration,
    attached: impl Fn(String, crate::transport::Connection) + Sync,
) {
    std::thread::scope(|scope| {
        while !stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let attached = &attached;
                    scope.spawn(move || match greet(stream, timeout) {
                        Ok((name, connection)) => attached(name, connection),
                        Err(error) => {
                            crate::complain!("peering: an attachment was refused: {error:#}")
                        }
                    });
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
}

/// Reads an attachment's greeting — the peer's name on a line — and
/// returns the connection behind it. Held to what the control socket
/// holds its clients to: the same user as this process, a greeting within
/// `timeout` and [`MAXIMUM_GREETING`] bytes, and the only name that
/// attaches.
fn greet(
    stream: std::os::unix::net::UnixStream,
    timeout: Duration,
) -> Result<(String, crate::transport::Connection)> {
    use std::io::{BufRead, Read};
    stream
        .set_nonblocking(false)
        .context("unable to configure the attachment")?;
    if !super::control::peer_is_same_user(&stream)? {
        anyhow::bail!("the connection is from another user");
    }
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(timeout)))
        .context("unable to configure the attachment")?;
    let mut limited = std::io::BufReader::new(
        stream
            .try_clone()
            .context("unable to clone the attachment")?,
    )
    .take(MAXIMUM_GREETING);
    let mut name = String::new();
    limited
        .read_line(&mut name)
        .context("unable to read the greeting")?;
    if !name.ends_with('\n') {
        anyhow::bail!("no greeting within {MAXIMUM_GREETING} bytes");
    }
    let name = name.trim().to_owned();
    if name != peering::ALPHA {
        anyhow::bail!("{name:?} is not a peer that attaches");
    }
    // The connection is long-lived and idles between cycles: the greeting's
    // deadline is not the session's.
    stream
        .set_read_timeout(None)
        .and_then(|()| stream.set_write_timeout(None))
        .context("unable to configure the attachment")?;
    let reader = limited.into_inner();
    Ok((
        name,
        crate::transport::Connection::from_streams(Box::new(reader), Box::new(stream)),
    ))
}

/// The alpha's `watch` when its groups peer: lead until fenced, then
/// attach to whoever leads until the lease names the alpha again, and
/// lead again. The configured alpha is never dialed, so while a beta
/// leads the alpha makes itself an endpoint by dialing the beta.
///
/// One supervisor runs throughout. While it follows, its peering sessions
/// wait for the lead and its plain groups keep running: they are the
/// alpha's alone, whoever leads the star.
#[allow(clippy::too_many_arguments)]
pub fn run_alpha(
    config_path: &Path,
    directory: &Path,
    plans: &[crate::config::SessionPlan],
    alerts: &crate::alerts::AlertPlan,
    state_root: &Path,
    verbose: bool,
    stop: &AtomicBool,
    reloader: Option<&std::sync::Arc<super::reload::Reloader>>,
) -> Result<()> {
    let interval = plans
        .iter()
        .map(|plan| plan.interval)
        .min()
        .unwrap_or(Duration::from_secs(5));
    let timing = plans
        .iter()
        .find_map(|plan| plan.peering)
        .unwrap_or(crate::config::PeeringPlan {
            ttl: crate::config::DEFAULT_PEERING_TTL,
            failover_after: crate::config::DEFAULT_PEERING_FAILOVER_AFTER,
        });
    let context = super::PeeringContext::for_alpha(
        config_path.to_path_buf(),
        directory.to_path_buf(),
        timing.ttl,
    )?;
    if context.role() == Role::Off {
        anyhow::bail!("no plan is in a peering mode");
    }
    let supervisor = super::Supervisor::new(plans.to_vec(), state_root.to_path_buf(), verbose)
        .with_alerts(alerts.clone())
        .with_peering(context)
        .with_reload(reloader.cloned());
    let inner_stop = AtomicBool::new(false);
    std::thread::scope(|scope| -> Result<()> {
        let watcher = scope.spawn(|| supervisor.run_watch(&inner_stop));
        let policy = super::attach_policy(plans);
        let outcome = alpha_roles(
            directory,
            &supervisor,
            &policy,
            interval,
            timing,
            stop,
            || watcher.is_finished(),
        );
        inner_stop.store(true, Ordering::Relaxed);
        let watched = match watcher.join() {
            Ok(result) => result,
            Err(_) => anyhow::bail!("the supervisor panicked"),
        };
        outcome.and(watched)
    })
    // An edit loaded: the caller runs it, with this loop entered again
    // under the new plans.
}

/// The alpha's part while its supervisor runs: nothing while it leads;
/// while it follows, attach to the leader, and take the lead back when
/// the lease names the alpha again, or when the leader's lease has been
/// stale for the configured wait.
fn alpha_roles(
    directory: &Path,
    supervisor: &super::Supervisor,
    policy: &crate::transport::AttachPolicy,
    interval: Duration,
    timing: crate::config::PeeringPlan,
    stop: &AtomicBool,
    finished: impl Fn() -> bool,
) -> Result<()> {
    let mut announced: Option<Role> = None;
    while !stop.load(Ordering::Relaxed) && !finished() {
        let role = supervisor.role();
        if announced.as_ref() != Some(&role) {
            match &role {
                Role::Leader { term, .. } => {
                    crate::note!("peering: leading as the alpha at term {term}")
                }
                Role::Follower { leader, term } => {
                    crate::note!("peering: {leader} leads at term {term}; attaching")
                }
                Role::Off => {}
            }
            announced = Some(role.clone());
        }
        let Role::Follower { leader, .. } = role else {
            std::thread::sleep(ROLE_POLL);
            continue;
        };
        // Attach, and serve as an agent until the leader lets go — it
        // does when it hands the lead back, or dies.
        let destination = peering::destination_of(&leader).to_owned();
        let argv = peering::attach_argv(&destination);
        if let Err(error) = crate::transport::attach_as_agent(&argv, policy) {
            crate::complain!("peering: the attachment to {leader} ended: {error:#}");
        }
        // The lease says whether the lead came back. A stale lease from a
        // beta that died is taken over as a beta would take it — the
        // alpha is the head of the order, so it waits only the configured
        // time.
        let now = peering::now_seconds();
        match peering::read_lease(directory)? {
            Some(lease) if lease.leader == peering::ALPHA => {
                supervisor.lead_as_alpha(lease.term)?;
            }
            Some(lease)
                if lease.is_stale_at(now) && lease.stale_for_at(now) >= timing.failover_after =>
            {
                crate::note!(
                    "peering: the lease of {} went stale; leading again",
                    lease.leader
                );
                // Under the lease lock: a beta that took the lead at this
                // term meanwhile keeps it, and the alpha attaches to it on
                // the next pass.
                supervisor.lead_as_alpha(lease.term + 1)?;
            }
            _ => super::sleep_interruptible(interval, stop),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Pushes a star of one peering group with these betas, as a leader
    /// pushes its configuration.
    fn push(directory: &Path, betas: &str) {
        let configuration = format!(
            r#"
            [advanced.peering-dangerously-experimental]
            ttl = "2s"
            failover_after = "2s"

            [groups.g]
            mode = "peering-conflict-dangerously-experimental"
            interval = 1
            alpha = "/home/f/g"
            betas = [{betas}]
            "#
        );
        peering::write_pushed_file(directory, "config.toml", configuration.as_bytes()).unwrap();
        peering::write_pushed_file(directory, "name", b"box:/srv/g").unwrap();
    }

    /// The leader keeps pushing while a peer follows. The takeover runs
    /// the star as it was last pushed, not as it stood when following
    /// began: here, a beta that joined the group meanwhile is in it.
    #[test]
    fn a_configuration_pushed_while_following_is_the_one_used_at_takeover() {
        let keep = tempfile::tempdir().expect("a temporary directory");
        let directory = keep.path();
        push(directory, r#""box:/srv/g""#);
        let star = current_star(directory).expect("a star");
        assert!(star.plans.is_empty(), "{:?}", star.plans);

        push(directory, r#""box:/srv/g", "other:/srv/g""#);
        peering::write_lease(
            directory,
            &Lease {
                leader: peering::ALPHA.to_owned(),
                term: 3,
                renewed_at: peering::now_seconds() - 600,
                ttl_seconds: 2,
            },
        )
        .unwrap();
        let (followed, star) = follow(directory, star, &AtomicBool::new(false)).expect("follows");
        assert!(matches!(followed, Followed::TakeOver { term: 4 }));
        let betas: Vec<String> = star.plans.iter().map(|plan| plan.beta_spec()).collect();
        assert_eq!(betas, ["other:/srv/g"]);
    }

    /// A connection that says nothing holds up only itself: the alpha
    /// dialing in behind it attaches at once, and the silent one is closed
    /// when its greeting times out. The socket and its directory are the
    /// owner's alone.
    #[test]
    fn a_silent_connection_does_not_hold_up_the_alpha() {
        use std::io::{Read, Write};
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixStream;
        let keep = tempfile::tempdir().expect("a temporary directory");
        let directory = keep.path().join("peering");
        let socket = directory.join(peering::ATTACH_SOCKET);
        let listener = bind_attach_socket(&directory, &socket).expect("bound");
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(&directory), mode(&socket)), (0o700, 0o600));

        let stop = AtomicBool::new(false);
        let attached = std::sync::Mutex::new(Vec::new());
        let timeout = Duration::from_secs(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                serve_attachments(&listener, &stop, timeout, |name, _| {
                    attached.lock().unwrap().push(name)
                })
            });
            let mut silent = UnixStream::connect(&socket).expect("connects");
            std::thread::sleep(Duration::from_millis(100));
            let started = std::time::Instant::now();
            let mut alpha = UnixStream::connect(&socket).expect("connects");
            alpha.write_all(b"alpha\n").unwrap();
            while attached.lock().unwrap().is_empty() {
                assert!(
                    started.elapsed() < Duration::from_secs(1),
                    "the alpha was held up behind a silent connection"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(*attached.lock().unwrap(), ["alpha"]);
            silent
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            assert_eq!(silent.read(&mut [0u8; 1]).expect("closed, not failed"), 0);
            assert!(started.elapsed() >= timeout - Duration::from_millis(200));
            stop.store(true, Ordering::Relaxed);
        });
    }

    /// A greeting is a short line naming the alpha: an endless one and a
    /// name that does not attach are refused.
    #[test]
    fn a_greeting_is_short_and_names_the_alpha() {
        use std::io::Write;
        use std::os::unix::net::UnixStream;
        let greeting = |sent: &[u8]| {
            let (server, mut client) = UnixStream::pair().unwrap();
            client.write_all(sent).unwrap();
            greet(server, Duration::from_secs(2)).map(|(name, _)| name)
        };
        let error = greeting(&[b'a'; 200]).expect_err("endless");
        assert!(
            format!("{error:#}").contains("no greeting within"),
            "{error:#}"
        );
        let error = greeting(b"box2\n").expect_err("not the alpha");
        assert!(
            format!("{error:#}").contains("not a peer that attaches"),
            "{error:#}"
        );
        assert_eq!(
            greeting(b"alpha\nand the handshake").expect("the alpha"),
            "alpha"
        );
    }

    /// A connection from another user is refused before anything it sends
    /// is read, whatever the socket's permissions let through. Runs only as
    /// root, which can connect as another user.
    #[test]
    fn another_user_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::process::CommandExt;
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let keep = tempfile::tempdir().expect("a temporary directory");
        std::fs::set_permissions(keep.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        let socket = keep.path().join("attach.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666)).unwrap();
        let mut client = std::process::Command::new("python3")
            .args([
                "-c",
                "import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); \
                 s.sendall(b'alpha\\n'); s.recv(1)",
            ])
            .arg(&socket)
            .uid(65534)
            .gid(65534)
            .spawn()
            .expect("a client as nobody");
        let (stream, _) = listener.accept().expect("accepted");
        let error = greet(stream, Duration::from_secs(5))
            .map(|(name, _)| name)
            .expect_err("another user");
        assert!(format!("{error:#}").contains("another user"), "{error:#}");
        let _ = client.wait();
    }

    /// A lease on this host that names this peer brings it back leading at
    /// that term, fresh or not: it led and restarted, or the lead was
    /// handed to it. Following it would leave nobody leading until the
    /// lease went stale.
    #[test]
    fn a_lease_naming_this_peer_is_led_at_once() {
        let keep = tempfile::tempdir().expect("a temporary directory");
        let directory = keep.path();
        push(directory, r#""box:/srv/g", "other:/srv/g""#);
        peering::write_lease(
            directory,
            &Lease::new("box:/srv/g", 7, Duration::from_secs(2)),
        )
        .unwrap();
        let star = current_star(directory).expect("a star");
        let (followed, _) = follow(directory, star, &AtomicBool::new(false)).expect("follows");
        assert!(matches!(followed, Followed::Resume { term: 7 }));
    }
}
