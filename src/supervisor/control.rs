//! The supervisor control socket.
//!
//! A running supervisor listens on a Unix socket in its state root
//! (`control.sock`), through which the CLI adjusts live sessions without
//! restarting anything: `flush` wakes a session for an immediate cycle,
//! `pause`/`resume` suspend and restore cycling, and `reset` discards a
//! session's ancestor so its next cycle merges both sides additively.
//!
//! Control is deliberately shallow: requests only flip per-worker flags
//! (wake, paused, reset) that the workers themselves act on at their next
//! opportunity. The socket thread never touches session state directly, so
//! there is nothing for it to race.

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A request to a running supervisor.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ControlRequest {
    /// Wake the selected sessions for an immediate cycle.
    Flush(Selector),
    /// Suspend cycling for the selected sessions.
    Pause(Selector),
    /// Resume cycling for the selected sessions.
    Resume(Selector),
    /// Discard the selected sessions' ancestors (their next cycles merge
    /// both sides additively, resurrecting deletions) and cycle.
    Reset(Selector),
    /// Re-read every file's content on the selected sessions' next cycle,
    /// making content changed without its metadata moving visible.
    Verify(Selector),
    /// Report what every supervised session is doing right now. The one
    /// request that reads rather than writes.
    Progress,
    /// Peering: hand the lead to the named peer — `alpha`, or a beta's
    /// spec — at the next term, and step down.
    Yield {
        /// Who leads next.
        to: String,
    },
    /// Any of the above, sent with the sender's build. The request is
    /// carried as bytes and decoded only once the versions agree, so a
    /// change to any request's shape can never be misread across builds:
    /// a mismatch is answered with `Mismatch`, which both builds decode.
    ///
    /// Variant order is the wire: this and `Mismatch` stay last, and later
    /// variants go after them.
    Versioned {
        /// The sender's `protocol::version()`.
        version: String,
        /// The request, encoded.
        request: Vec<u8>,
    },
    /// Report which sessions are running, the configuration they were
    /// planned from, and any refused edit — so a caller shows what runs
    /// rather than what the file on disk says now.
    Sessions,
}

/// Selects sessions by group and destination.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Selector {
    /// The group to select (all groups when absent).
    pub group: Option<String>,
    /// The destination host (or local beta path) within the group (all
    /// destinations when absent).
    pub host: Option<String>,
}

impl Selector {
    /// Indicates whether or not a session matches this selector.
    fn matches(&self, group: &str, host: &str) -> bool {
        self.group.as_deref().is_none_or(|wanted| wanted == group)
            && self.host.as_deref().is_none_or(|wanted| wanted == host)
    }
}

/// The supervisor's answer to a control request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ControlResponse {
    /// The request was applied to this many sessions.
    Applied {
        /// The number of sessions affected.
        sessions: usize,
    },
    /// The live progress of every supervised session, in supervision
    /// order.
    Progress(Vec<SessionProgress>),
    /// The request failed.
    Error(String),
    /// The request came from another build. Nothing was applied: the
    /// running supervisor and this command must be the same build, and a
    /// restart makes them so.
    Mismatch {
        /// The running supervisor's `protocol::version()`.
        supervisor: String,
    },
    /// What the supervisor is running.
    Sessions(Inventory),
}

/// What a running supervisor is running.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Inventory {
    /// Every session running, in supervision order.
    pub sessions: Vec<SessionSummary>,
    /// The configuration the sessions were planned from, as it loaded —
    /// absent for a supervisor that did not start from a file of its own,
    /// such as a peer's.
    pub configuration: Option<String>,
    /// The edit the supervisor refused, while it stands.
    pub notice: Option<super::reload::Notice>,
    /// Whether the supervisor has failed to write a log line.
    pub logging_failed: bool,
}

/// One running session, as the inventory lists it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSummary {
    /// The session's key: its state identifier.
    pub identifier: String,
    /// `group@host`.
    pub display: String,
    /// The synchronization mode's name.
    pub mode: String,
    /// The state its last attempt recorded, empty before the first.
    pub state: String,
}

/// What asking the running supervisor came to.
#[derive(Clone, Debug)]
pub enum Probe {
    /// It answered, with every session's progress.
    Answered(Vec<SessionProgress>),
    /// One is running but is another build, named when it said so. A
    /// supervisor from before builds were compared cannot say, and does
    /// not understand the question.
    Mismatch(Option<String>),
    /// One is running, or its socket is still being listened on, but it
    /// did not answer within the client timeout: wedged.
    Unresponsive,
    /// Nothing is listening.
    Absent,
}

impl Probe {
    /// Whether a supervisor is running, answering or not.
    pub fn is_running(&self) -> bool {
        !matches!(self, Probe::Absent)
    }

    /// The progress, when the supervisor answered.
    pub fn progress(self) -> Option<Vec<SessionProgress>> {
        match self {
            Probe::Answered(sessions) => Some(sessions),
            _ => None,
        }
    }

    /// What to tell someone about a supervisor of another build.
    pub fn mismatch_message(&self) -> Option<String> {
        match self {
            Probe::Mismatch(supervisor) => Some(mismatch_message(supervisor.as_deref())),
            _ => None,
        }
    }
}

/// Says that the running supervisor is another build, and what to do.
pub fn mismatch_message(supervisor: Option<&str>) -> String {
    let this = crate::protocol::version();
    match supervisor {
        Some(supervisor) => format!(
            "the running supervisor is {supervisor} and this is {this}; \
             `autobahn restart` to run this build"
        ),
        None => format!(
            "a supervisor is running but does not understand this build ({this}) — \
             most likely it is older; `autobahn restart` to run this build"
        ),
    }
}

/// One supervised session's live progress.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionProgress {
    /// The session's identifier: what tells two sessions apart when they
    /// share a group and a destination host.
    pub session: String,
    /// The session's group.
    pub group: String,
    /// The session's destination.
    pub host: String,
    /// What it is doing.
    pub progress: crate::progress::ProgressSnapshot,
}

/// The live progress of the session `identifier` names, from a
/// supervisor's answer. By identifier, not by group and host, which two
/// betas on one host share.
pub fn progress_of<'a>(
    sessions: &'a [SessionProgress],
    identifier: &str,
) -> Option<&'a crate::progress::ProgressSnapshot> {
    sessions
        .iter()
        .find(|session| session.session == identifier)
        .map(|session| &session.progress)
}

/// The control flags of one supervised session, flipped by the control
/// socket and consumed by the session's worker.
#[derive(Debug, Default)]
pub(crate) struct WorkerControl {
    /// Wake the worker for an immediate cycle.
    pub wake: AtomicBool,
    /// Suspend cycling.
    pub paused: AtomicBool,
    /// Discard the ancestor before the next cycle.
    pub reset: AtomicBool,
    /// Re-read every file's content on the next cycle.
    pub verify: AtomicBool,
}

/// One registry entry: a session, its control flags, and its live
/// progress.
pub(crate) struct Entry {
    /// The session's identifier.
    pub session: String,
    /// The session's name for people to read.
    pub display: String,
    /// The session's mode, by name.
    pub mode: String,
    /// The session's group.
    pub group: String,
    /// The session's destination.
    pub host: String,
    /// The flags the control socket flips and the worker consumes.
    pub control: Arc<WorkerControl>,
    /// What the session is doing, updated by the worker as it works.
    pub progress: Arc<crate::progress::Progress>,
    /// The status the session last recorded.
    pub published: Arc<std::sync::Mutex<Option<super::SessionStatus>>>,
}

/// Peering: what the control socket calls to hand the lead on.
pub type YieldHandle = Arc<dyn Fn(&str) -> anyhow::Result<()> + Send + Sync>;

/// The registry mapping sessions to their control flags, shared between the
/// socket thread and the workers.
pub(crate) struct Registry {
    /// One entry per supervised session, replaced as an edit to the
    /// configuration changes which sessions run.
    pub entries: RwLock<Vec<Entry>>,
    /// Peering: how to hand the lead on, when this supervisor leads.
    pub yield_to: Option<YieldHandle>,
    /// The configuration the sessions were planned from, replaced with
    /// the entries.
    pub configuration: RwLock<Option<String>>,
    /// The state root, where a refused edit's notice is kept.
    pub state_root: PathBuf,
}

impl Registry {
    /// Applies a control request, returning the number of affected sessions.
    fn apply(&self, request: &ControlRequest) -> ControlResponse {
        let entries = self
            .entries
            .read()
            .unwrap_or_else(|error| error.into_inner());
        // Progress reads rather than writes, and selects nothing: the
        // caller wants the whole picture and matches it up itself.
        if let ControlRequest::Yield { to } = request {
            let Some(yield_to) = &self.yield_to else {
                return ControlResponse::Error(
                    "this supervisor is not leading a peering group".into(),
                );
            };
            return match yield_to(to) {
                Ok(()) => {
                    // Every worker hands its peer the new lease on its
                    // next attempt; woken, that is now.
                    for entry in entries.iter() {
                        entry.control.wake.store(true, Ordering::Relaxed);
                    }
                    ControlResponse::Applied {
                        sessions: entries.len(),
                    }
                }
                Err(error) => ControlResponse::Error(format!("{error:#}")),
            };
        }
        if let ControlRequest::Versioned { version, request } = request {
            if *version != crate::protocol::version() {
                return ControlResponse::Mismatch {
                    supervisor: crate::protocol::version(),
                };
            }
            return match bincode::deserialize::<ControlRequest>(request) {
                // Nested once, never twice: the inner request is a verb.
                Ok(ControlRequest::Versioned { .. }) => {
                    ControlResponse::Error("a versioned request inside another".into())
                }
                Ok(inner) => {
                    drop(entries);
                    self.apply(&inner)
                }
                Err(error) => ControlResponse::Error(format!("undecodable request: {error}")),
            };
        }
        if let ControlRequest::Sessions = request {
            return ControlResponse::Sessions(Inventory {
                sessions: entries
                    .iter()
                    .map(|entry| SessionSummary {
                        identifier: entry.session.clone(),
                        display: entry.display.clone(),
                        mode: entry.mode.clone(),
                        state: entry
                            .published
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .as_ref()
                            .map(super::classify_state)
                            .unwrap_or_default(),
                    })
                    .collect(),
                configuration: self
                    .configuration
                    .read()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone(),
                notice: super::reload::read_notice(&self.state_root),
                logging_failed: crate::logging::failed(),
            });
        }
        if let ControlRequest::Progress = request {
            return ControlResponse::Progress(
                entries
                    .iter()
                    .map(|entry| SessionProgress {
                        session: entry.session.clone(),
                        group: entry.group.clone(),
                        host: entry.host.clone(),
                        progress: entry.progress.snapshot(),
                    })
                    .collect(),
            );
        }
        let (selector, action): (&Selector, fn(&WorkerControl)) = match request {
            ControlRequest::Flush(selector) => (selector, |control| {
                control.wake.store(true, Ordering::Relaxed);
            }),
            ControlRequest::Pause(selector) => (selector, |control| {
                control.paused.store(true, Ordering::Relaxed);
                control.wake.store(true, Ordering::Relaxed);
            }),
            ControlRequest::Resume(selector) => (selector, |control| {
                control.paused.store(false, Ordering::Relaxed);
                control.wake.store(true, Ordering::Relaxed);
            }),
            ControlRequest::Reset(selector) => (selector, |control| {
                control.reset.store(true, Ordering::Relaxed);
                control.wake.store(true, Ordering::Relaxed);
            }),
            ControlRequest::Verify(selector) => (selector, |control| {
                control.verify.store(true, Ordering::Relaxed);
                control.wake.store(true, Ordering::Relaxed);
            }),
            ControlRequest::Progress
            | ControlRequest::Sessions
            | ControlRequest::Yield { .. }
            | ControlRequest::Versioned { .. } => {
                unreachable!("answered above")
            }
        };
        let mut sessions = 0;
        for entry in entries.iter() {
            if selector.matches(&entry.group, &entry.host) {
                action(&entry.control);
                sessions += 1;
            }
        }
        if sessions == 0 {
            ControlResponse::Error("no supervised session matches the selector".into())
        } else {
            ControlResponse::Applied { sessions }
        }
    }
}

/// Whether a supervisor is listening for control requests on this state
/// root.
///
/// Connecting is the only honest test. The socket *file* outlives the
/// process that made it — a supervisor killed with SIGKILL leaves one
/// behind — so its presence proves nothing, while a refused connection
/// proves nobody is listening.
pub fn supervisor_is_running(state_root: &Path) -> bool {
    // A connection that cannot even be queued within the timeout is a
    // supervisor too wedged to accept, not an absent one.
    match connect_client(&socket_path(state_root), CLIENT_TIMEOUT) {
        Ok(_) => true,
        Err(error) => error.kind() == std::io::ErrorKind::TimedOut,
    }
}

/// How long a client call to the control socket may take, each of the
/// connect, the request's write, and the answer's read. A wedged
/// supervisor then reads as not responding, instead of freezing `status`,
/// the shop, or the tray's event loop.
pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

/// Connects to a control socket within `timeout`, with read and write
/// timeouts of the same length on the stream.
///
/// A blocking connect to a Unix socket whose supervisor has stopped
/// accepting waits as soon as the listen queue is full, and has no
/// timeout of its own, so the connect is made non-blocking and retried
/// until the deadline. A full queue is reported as `TimedOut`.
fn connect_client(path: &Path, timeout: Duration) -> std::io::Result<UnixStream> {
    use std::io::{Error, ErrorKind};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::{AsRawFd, FromRawFd};

    let bytes = path.as_os_str().as_bytes();
    // SAFETY: an all-zero `sockaddr_un` is a valid (empty) address.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= address.sun_path.len() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "control socket path too long",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    // SAFETY: plain socket creation; the descriptor is owned by the stream
    // at once, which closes it on every path out.
    let descriptor = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if descriptor < 0 {
        return Err(Error::last_os_error());
    }
    let stream = unsafe { UnixStream::from_raw_fd(descriptor) };
    stream.set_nonblocking(true)?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        // SAFETY: the address is initialized and its length is its size.
        let connected = unsafe {
            libc::connect(
                stream.as_raw_fd(),
                &address as *const libc::sockaddr_un as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
            )
        };
        if connected == 0 {
            break;
        }
        let error = Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            // The listen queue is full: the supervisor is not accepting.
            Some(libc::EAGAIN) => {
                let now = std::time::Instant::now();
                if now >= deadline {
                    return Err(Error::new(
                        ErrorKind::TimedOut,
                        "the supervisor is not accepting connections",
                    ));
                }
                std::thread::sleep((deadline - now).min(Duration::from_millis(20)));
            }
            // In progress: wait for it to finish, within the deadline.
            Some(libc::EINPROGRESS) => {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                let mut poll = libc::pollfd {
                    fd: stream.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                // SAFETY: one valid pollfd.
                let ready =
                    unsafe { libc::poll(&mut poll, 1, remaining.as_millis() as libc::c_int) };
                if ready == 0 {
                    return Err(Error::new(
                        ErrorKind::TimedOut,
                        "the supervisor is not accepting connections",
                    ));
                }
                if ready < 0 {
                    return Err(Error::last_os_error());
                }
                if let Some(error) = stream.take_error()? {
                    return Err(error);
                }
                break;
            }
            _ => return Err(error),
        }
    }
    stream.set_nonblocking(false)?;
    // A zero timeout means "none" to the socket; the smallest real one
    // keeps a zero deadline a deadline.
    let timeout = Some(timeout.max(Duration::from_millis(1)));
    stream.set_read_timeout(timeout)?;
    stream.set_write_timeout(timeout)?;
    Ok(stream)
}

/// Whether a client call failed because the supervisor did not answer in
/// time, rather than for any other reason.
fn timed_out(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            )
        })
    })
}

/// Returns the control socket path for a state root.
///
/// Unix socket paths are limited to roughly 108 bytes (`sockaddr_un`), so a
/// deeply nested state root can't hold its own socket. In that case the
/// socket falls back to a short per-user directory, named by the *resolved*
/// state root's digest — both the supervisor and the CLI derive the same
/// path from the same state root, wherever it lives.
pub fn socket_path(state_root: &Path) -> PathBuf {
    const MAXIMUM_SOCKET_PATH: usize = 100;
    let direct = state_root.join("control.sock");
    if direct.as_os_str().len() <= MAXIMUM_SOCKET_PATH {
        return direct;
    }
    let identity = crate::paths::resolve_for_identity(state_root);
    let digest = blake3::hash(identity.as_os_str().as_encoded_bytes());
    let mut name = String::with_capacity(16);
    for byte in &digest.as_bytes()[..8] {
        use std::fmt::Write;
        let _ = write!(name, "{byte:02x}");
    }
    let directory = format!("autobahn-{}", unsafe { libc::getuid() });
    let fallback = std::env::temp_dir()
        .join(&directory)
        .join(format!("{name}.sock"));
    if fallback.as_os_str().len() <= MAXIMUM_SOCKET_PATH {
        return fallback;
    }
    // An over-length TMPDIR would defeat the fallback too; /tmp is short by
    // construction.
    PathBuf::from("/tmp")
        .join(directory)
        .join(format!("{name}.sock"))
}

/// Binds the control socket, replacing any stale socket file left by a
/// previous supervisor (the state lock already guarantees no *live* one
/// shares this state root's sessions).
pub(crate) fn bind(state_root: &Path) -> Result<UnixListener> {
    std::fs::create_dir_all(state_root)
        .with_context(|| format!("unable to create the state root {}", state_root.display()))?;
    let path = socket_path(state_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("unable to create {}", parent.display()))?;
        // The fallback directory lives in the shared temporary directory;
        // keep it private to the user (best-effort — it may already exist
        // with these permissions).
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("unable to bind control socket {}", path.display()))?;
    // Restrict the socket itself as a second layer under the peer
    // credential check performed per connection.
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    listener
        .set_nonblocking(true)
        .context("unable to configure the control socket")?;
    Ok(listener)
}

/// Serves control requests until `stop` becomes true.
pub(crate) fn serve(listener: UnixListener, registry: &Registry, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(error) = handle(stream, registry) {
                    crate::complain!("control request failed: {error:#}");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(error) => {
                crate::complain!("control socket failed: {error:#}");
                return;
            }
        }
    }
}

/// Verifies that the connecting peer is the same user as this process:
/// control requests are state-destructive (`reset` in particular), so
/// authorization must not rest on directory permissions alone.
fn peer_is_same_user(stream: &UnixStream) -> Result<bool> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    #[cfg(target_os = "linux")]
    {
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut credentials as *mut _ as *mut libc::c_void,
                &mut length,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error()).context("unable to read peer credentials");
        }
        Ok(credentials.uid == unsafe { libc::getuid() })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let (mut uid, mut gid): (libc::uid_t, libc::gid_t) = (0, 0);
        let result = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
        if result != 0 {
            return Err(std::io::Error::last_os_error()).context("unable to read peer credentials");
        }
        Ok(uid == unsafe { libc::getuid() })
    }
}

/// Handles one control connection: a single request/response exchange, with
/// timeouts so a stalled client can never wedge the control service.
fn handle(stream: UnixStream, registry: &Registry) -> Result<()> {
    // A peer that connected and left without a word — `status` probing
    // whether anyone is listening does exactly this, twice a second under
    // `watch` — is not a failed request; it made none. macOS reports the
    // vanished peer as EINVAL from the very first setsockopt, so a failure
    // to configure the connection is read as that, and answered with
    // silence rather than a complaint on every probe.
    let configured = stream.set_nonblocking(false).and_then(|()| {
        let timeout = Some(std::time::Duration::from_secs(2));
        stream.set_read_timeout(timeout)?;
        stream.set_write_timeout(timeout)
    });
    if configured.is_err() {
        return Ok(());
    }
    if !peer_is_same_user(&stream)? {
        anyhow::bail!("rejecting a control request from another user");
    }
    let mut reader = stream
        .try_clone()
        .context("unable to clone the control connection")?;
    let mut writer = stream;
    let request: ControlRequest = crate::transport::receive_control_frame(&mut reader)?;
    let response = registry.apply(&request);
    crate::transport::send_control_frame(&mut writer, &response)
}

/// Sends one control request to the supervisor owning `state_root`,
/// returning its response.
pub fn send(state_root: &Path, request: &ControlRequest) -> Result<ControlResponse> {
    // Handing the lead on talks to a peer before it answers; every other
    // request only flips a flag.
    let timeout = match request {
        ControlRequest::Yield { .. } => crate::transport::mux::SETUP_TIMEOUT,
        _ => CLIENT_TIMEOUT,
    };
    send_within(state_root, request, timeout)
}

/// [`send`], with the client timeout given.
fn send_within(
    state_root: &Path,
    request: &ControlRequest,
    timeout: Duration,
) -> Result<ControlResponse> {
    let path = socket_path(state_root);
    let unresponsive = || {
        format!(
            "the supervisor at {} is not responding (waited {} s)",
            path.display(),
            timeout.as_secs_f64()
        )
    };
    let stream = match connect_client(&path, timeout) {
        Ok(stream) => stream,
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            return Err(anyhow::Error::new(error).context(unresponsive()));
        }
        Err(error) => {
            return Err(anyhow::Error::new(error).context(format!(
                "unable to reach a running supervisor at {} (is `autobahn watch` running?)",
                path.display()
            )));
        }
    };
    let mut reader = stream
        .try_clone()
        .context("unable to clone the control connection")?;
    let mut writer = stream;
    let versioned = ControlRequest::Versioned {
        version: crate::protocol::version(),
        request: bincode::serialize(request).context("unable to encode the control request")?,
    };
    if let Err(error) = crate::transport::send_control_frame(&mut writer, &versioned) {
        return Err(match timed_out(&error) {
            true => error.context(unresponsive()),
            false => error,
        });
    }
    match crate::transport::receive_control_frame(&mut reader) {
        Ok(ControlResponse::Mismatch { supervisor }) => {
            Err(anyhow::anyhow!(mismatch_message(Some(&supervisor))))
        }
        Ok(response) => Ok(response),
        Err(error) if timed_out(&error) => Err(error.context(unresponsive())),
        // Connected, and then nothing that decodes: a supervisor from
        // before builds were compared, which cannot read the envelope.
        Err(error) => Err(error.context(mismatch_message(None))),
    }
}

/// Asks the supervisor owning `state_root` what its sessions are doing,
/// and says which of the answers came back.
pub fn probe(state_root: &Path) -> Probe {
    probe_within(state_root, CLIENT_TIMEOUT)
}

/// [`probe`], with the client timeout given.
fn probe_within(state_root: &Path, timeout: Duration) -> Probe {
    let stream = match connect_client(&socket_path(state_root), timeout) {
        Ok(stream) => stream,
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => return Probe::Unresponsive,
        Err(_) => return Probe::Absent,
    };
    let Ok(mut reader) = stream.try_clone() else {
        return Probe::Absent;
    };
    let mut writer = stream;
    let Ok(request) = bincode::serialize(&ControlRequest::Progress) else {
        return Probe::Absent;
    };
    let versioned = ControlRequest::Versioned {
        version: crate::protocol::version(),
        request,
    };
    if let Err(error) = crate::transport::send_control_frame(&mut writer, &versioned) {
        return match timed_out(&error) {
            true => Probe::Unresponsive,
            false => Probe::Mismatch(None),
        };
    }
    match crate::transport::receive_control_frame(&mut reader) {
        Ok(ControlResponse::Progress(sessions)) => Probe::Answered(sessions),
        Ok(ControlResponse::Mismatch { supervisor }) => Probe::Mismatch(Some(supervisor)),
        Err(error) if timed_out(&error) => Probe::Unresponsive,
        _ => Probe::Mismatch(None),
    }
}

/// Asks the supervisor owning `state_root` what it is running. None when
/// no supervisor answers — none is running, or one of another build is.
pub fn inventory(state_root: &Path) -> Option<Inventory> {
    let stream = UnixStream::connect(socket_path(state_root)).ok()?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(2)));
    let mut reader = stream.try_clone().ok()?;
    let mut writer = stream;
    let versioned = ControlRequest::Versioned {
        version: crate::protocol::version(),
        request: bincode::serialize(&ControlRequest::Sessions).ok()?,
    };
    crate::transport::send_control_frame(&mut writer, &versioned).ok()?;
    match crate::transport::receive_control_frame(&mut reader) {
        Ok(ControlResponse::Sessions(inventory)) => Some(inventory),
        _ => None,
    }
}

/// Asks the supervisor owning `state_root` what its sessions are doing.
///
/// Absent when no supervisor is running — which is the honest answer, since
/// live progress is something only a running supervisor has. A supervisor
/// from a different build may not understand the request; that is reported
/// the same way, because the caller's remedy is identical.
pub fn query_progress(state_root: &Path) -> Option<Vec<SessionProgress>> {
    probe(state_root).progress()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(group: &str, host: &str) -> Entry {
        Entry {
            session: format!("{group}-{host}"),
            display: format!("{group}@{host}"),
            mode: "two-way-safe".into(),
            published: Arc::default(),
            group: group.into(),
            host: host.into(),
            control: Arc::default(),
            progress: Arc::default(),
        }
    }

    fn registry() -> Registry {
        Registry {
            yield_to: None,
            configuration: RwLock::new(Some("[groups.work]".into())),
            state_root: PathBuf::from("/nonexistent/state"),
            entries: RwLock::new(vec![
                entry("work", "host1"),
                entry("work", "host2"),
                entry("other", "host1"),
            ]),
        }
    }

    #[test]
    fn socket_paths_respect_the_unix_socket_length_limit() {
        // A short state root holds its own socket.
        let short = socket_path(Path::new("/tmp/autobahn-state"));
        assert_eq!(short, PathBuf::from("/tmp/autobahn-state/control.sock"));

        // A deep one falls back to a short per-user path — deterministically,
        // so the CLI and the supervisor agree on it.
        let deep = PathBuf::from(format!("/tmp/{}/state", "long-component/".repeat(12)));
        let fallback = socket_path(&deep);
        assert!(fallback.as_os_str().len() <= 108, "{fallback:?}");
        assert_eq!(fallback, socket_path(&deep));
        assert_ne!(fallback, socket_path(Path::new("/other/equally/deep/root")));
    }

    #[test]
    fn selectors_scope_requests() {
        let registry = registry();
        // Everything.
        let response = registry.apply(&ControlRequest::Flush(Selector::default()));
        assert!(matches!(response, ControlResponse::Applied { sessions: 3 }));
        // One group.
        let response = registry.apply(&ControlRequest::Pause(Selector {
            group: Some("work".into()),
            host: None,
        }));
        assert!(matches!(response, ControlResponse::Applied { sessions: 2 }));
        assert!(registry.entries.read().unwrap()[0]
            .control
            .paused
            .load(Ordering::Relaxed));
        assert!(!registry.entries.read().unwrap()[2]
            .control
            .paused
            .load(Ordering::Relaxed));
        // One session.
        let response = registry.apply(&ControlRequest::Reset(Selector {
            group: Some("other".into()),
            host: Some("host1".into()),
        }));
        assert!(matches!(response, ControlResponse::Applied { sessions: 1 }));
        assert!(registry.entries.read().unwrap()[2]
            .control
            .reset
            .load(Ordering::Relaxed));
        // No match.
        let response = registry.apply(&ControlRequest::Flush(Selector {
            group: Some("absent".into()),
            host: None,
        }));
        assert!(matches!(response, ControlResponse::Error(_)));
    }

    fn versioned(version: &str, request: &ControlRequest) -> ControlRequest {
        ControlRequest::Versioned {
            version: version.into(),
            request: bincode::serialize(request).expect("encodes"),
        }
    }

    #[test]
    fn a_request_from_this_build_is_applied_and_one_from_another_is_not() {
        let registry = registry();
        let flush = ControlRequest::Flush(Selector::default());
        match registry.apply(&versioned(&crate::protocol::version(), &flush)) {
            ControlResponse::Applied { sessions } => assert_eq!(sessions, 3),
            other => panic!("expected Applied, got {other:?}"),
        }
        match registry.apply(&versioned("0.0.1+e1", &flush)) {
            ControlResponse::Mismatch { supervisor } => {
                assert_eq!(supervisor, crate::protocol::version())
            }
            other => panic!("expected Mismatch, got {other:?}"),
        }
        // Nothing was flipped by the refused one.
        for entry in registry.entries.read().unwrap().iter() {
            entry.control.wake.store(false, Ordering::Relaxed);
        }
        registry.apply(&versioned("0.0.1+e1", &flush));
        assert!(registry
            .entries
            .read()
            .unwrap()
            .iter()
            .all(|entry| !entry.control.wake.load(Ordering::Relaxed)));
    }

    #[test]
    fn the_envelope_and_the_mismatch_keep_their_place_on_the_wire() {
        // Variant order is the wire: a later build adding a request must
        // add it after these, or an older supervisor misreads the envelope
        // as something else and a newer one cannot say "restart".
        let envelope = bincode::serialize(&versioned("x", &ControlRequest::Progress)).unwrap();
        assert_eq!(envelope[..4], 7u32.to_le_bytes());
        let mismatch = bincode::serialize(&ControlResponse::Mismatch {
            supervisor: "x".into(),
        })
        .unwrap();
        assert_eq!(mismatch[..4], 3u32.to_le_bytes());
        // Later requests come after them.
        let sessions = bincode::serialize(&ControlRequest::Sessions).unwrap();
        assert_eq!(sessions[..4], 8u32.to_le_bytes());
        let inventory = bincode::serialize(&ControlResponse::Sessions(Inventory {
            sessions: Vec::new(),
            configuration: None,
            notice: None,
            logging_failed: false,
        }))
        .unwrap();
        assert_eq!(inventory[..4], 4u32.to_le_bytes());
    }

    #[test]
    fn the_inventory_lists_the_sessions_and_what_they_were_planned_from() {
        let registry = registry();
        *registry.entries.read().unwrap()[1]
            .published
            .lock()
            .unwrap() = Some(crate::supervisor::SessionStatus {
            state: "errored".into(),
            ..Default::default()
        });
        let ControlResponse::Sessions(inventory) = registry.apply(&versioned(
            &crate::protocol::version(),
            &ControlRequest::Sessions,
        )) else {
            panic!("expected an inventory");
        };
        let shown: Vec<(&str, &str, &str)> = inventory
            .sessions
            .iter()
            .map(|session| {
                (
                    session.identifier.as_str(),
                    session.display.as_str(),
                    session.state.as_str(),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                ("work-host1", "work@host1", ""),
                ("work-host2", "work@host2", "errored"),
                ("other-host1", "other@host1", ""),
            ]
        );
        assert_eq!(inventory.configuration.as_deref(), Some("[groups.work]"));
        assert_eq!(inventory.notice, None);
    }

    /// A control socket whose supervisor is wedged: bound and listening,
    /// never accepting. With `backlog_full`, its queue of pending
    /// connections is full too, so a blocking connect would never return.
    fn wedged_socket(state_root: &Path, backlog_full: bool) -> UnixListener {
        let listener = UnixListener::bind(socket_path(state_root)).expect("binds");
        if backlog_full {
            use std::os::unix::io::AsRawFd;
            // Listening again sets the backlog; the smallest fills fast.
            assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
            let mut pending = Vec::new();
            let full =
                (0..64).any(
                    |_| match connect_client(&socket_path(state_root), Duration::ZERO) {
                        Ok(stream) => {
                            pending.push(stream);
                            false
                        }
                        Err(_) => true,
                    },
                );
            assert!(full, "the backlog never filled");
            std::mem::forget(pending);
        }
        listener
    }

    #[test]
    fn a_wedged_supervisor_is_unresponsive_within_the_client_timeout() {
        for backlog_full in [false, true] {
            let root = tempfile::tempdir().expect("a temporary directory");
            let _listener = wedged_socket(root.path(), backlog_full);
            let started = std::time::Instant::now();
            let probe = probe_within(root.path(), Duration::from_millis(300));
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "backlog full {backlog_full}: took {:?}",
                started.elapsed()
            );
            assert!(
                matches!(probe, Probe::Unresponsive),
                "backlog full {backlog_full}: {probe:?}"
            );
            assert!(probe.is_running());

            let started = std::time::Instant::now();
            let error = send_within(
                root.path(),
                &ControlRequest::Flush(Selector::default()),
                Duration::from_millis(300),
            )
            .expect_err("a wedged supervisor cannot answer");
            assert!(started.elapsed() < Duration::from_secs(3));
            assert!(format!("{error:#}").contains("not responding"), "{error:#}");
        }
    }

    /// The tray and `status` build their report through the probe, so a
    /// wedged supervisor costs them the client timeout, not their event
    /// loop.
    #[test]
    fn a_status_report_against_a_wedged_supervisor_returns() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let _listener = wedged_socket(root.path(), true);
        let started = std::time::Instant::now();
        let report = crate::supervisor::status_report(&[], root.path());
        assert!(started.elapsed() < CLIENT_TIMEOUT + Duration::from_secs(5));
        assert!(report.supervisor_running);
    }

    #[test]
    fn probing_a_socket_nobody_listens_on_is_absent() {
        let root = tempfile::tempdir().expect("a temporary directory");
        assert!(matches!(probe(root.path()), Probe::Absent));
        assert!(!probe(root.path()).is_running());
    }
}
