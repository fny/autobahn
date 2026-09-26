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
    /// Settle conflicts: each named session applies its part — its
    /// ancestor forgets the paths, and the losing copies on its sides are
    /// retired — between two of its cycles, under its own lock, and then
    /// cycles. `resolve` sends this while a supervisor owns the sessions,
    /// since only the owner may write an ancestor.
    Resolve {
        /// What the answers to [`ControlRequest::Resolved`] are asked by.
        id: u64,
        /// One part per session.
        parts: Vec<ResolutionPart>,
    },
    /// How the resolution `id` names has gone so far.
    Resolved {
        /// The resolution's identifier.
        id: u64,
    },
}

/// One session's part in a resolution.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResolutionPart {
    /// The session.
    pub session: SessionKey,
    /// What it applies.
    pub settlement: crate::session::Settlement,
    /// Whether this part waits for every other part of its resolution.
    ///
    /// The part that retires alpha's copy, when a destination's version
    /// is kept, is applied last: until every other destination's ancestor
    /// has forgotten the path and its losing copy is gone, alpha's gap
    /// would read there as a deletion against an edited copy — and an edit
    /// beats a deletion, so the losing version would come back to alpha
    /// and win. It is not applied at all if another part failed, and it
    /// leaves alone a path another part's copy was refused at.
    pub last: bool,
}

/// Where one part of a resolution stands.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum PartState {
    /// Not yet applied: the session is mid-cycle, not connected, paused,
    /// or waiting for the other parts.
    Pending,
    /// Applied, with what it came to.
    Applied(crate::session::SettlementOutcome),
    /// Not applied, or applied only in part, and why.
    Failed(String),
}

/// A resolution, as the supervisor tracks it: every part's state, shared
/// by the workers applying them and the socket answering for them.
#[derive(Debug)]
pub(crate) struct Resolution {
    /// Its identifier.
    pub id: u64,
    /// Every part's session and state, in the order sent.
    pub parts: std::sync::Mutex<Vec<(SessionKey, PartState)>>,
    /// The workers applying parts, woken whenever one reports, so a part
    /// waiting on the others is applied as soon as it may be.
    pub workers: Vec<Arc<WorkerControl>>,
}

impl Resolution {
    /// Records the state `session`'s part reached, and wakes every worker
    /// with a part in it.
    pub fn report(&self, session: &SessionKey, state: PartState) {
        let mut parts = self.parts.lock().unwrap_or_else(|error| error.into_inner());
        if let Some((_, slot)) = parts.iter_mut().find(|(key, _)| key == session) {
            *slot = state;
        }
        drop(parts);
        for worker in &self.workers {
            worker.wake.store(true, Ordering::Relaxed);
        }
    }

    /// Every other part's state, when `session`'s part may go: `None`
    /// while another is pending.
    pub fn others(&self, session: &SessionKey) -> Option<Vec<PartState>> {
        let parts = self.parts.lock().unwrap_or_else(|error| error.into_inner());
        let others: Vec<PartState> = parts
            .iter()
            .filter(|(key, _)| key != session)
            .map(|(_, state)| state.clone())
            .collect();
        (!others
            .iter()
            .any(|state| matches!(state, PartState::Pending)))
        .then_some(others)
    }
}

/// One part waiting in a worker's control flags.
#[derive(Debug)]
pub(crate) struct PendingPart {
    /// The resolution it belongs to.
    pub resolution: Arc<Resolution>,
    /// The part.
    pub part: ResolutionPart,
}

/// What tells one session from every other: its state identifier.
///
/// A session's group and host are not enough — two betas of one group on
/// one host share both — and its display label is for people to read. So
/// everything that keeps track of sessions (control requests, progress,
/// alert state, the status inventory) keys them by this.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionKey(String);

impl SessionKey {
    /// The key of the session a plan describes.
    pub fn of(plan: &crate::config::SessionPlan) -> SessionKey {
        SessionKey(plan.identifier())
    }

    /// The key of the session with this state identifier.
    pub fn new(identifier: impl Into<String>) -> SessionKey {
        SessionKey(identifier.into())
    }

    /// The state identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A plan's destination as people read it: its host, or `host:path` when
/// another beta of its group is on the same host — its display label
/// without the group.
pub fn destination_of(plan: &crate::config::SessionPlan) -> String {
    let display = plan.display();
    display
        .strip_prefix(&format!("{}@", plan.group))
        .map(str::to_owned)
        .unwrap_or_else(|| plan.host.clone())
}

/// Selects sessions by group and destination, or one session by its key.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Selector {
    /// The group to select (all groups when absent).
    pub group: Option<String>,
    /// The destination within the group, as `status` names it: a host,
    /// which selects every beta on it, or a beta's specification (a local
    /// beta's path, or `host:path`), which selects that one (all
    /// destinations when absent).
    pub host: Option<String>,
    /// The one session to select, by key (any when absent).
    pub session: Option<SessionKey>,
}

impl Selector {
    /// Selects the one session `key` names.
    pub fn session(key: SessionKey) -> Selector {
        Selector {
            session: Some(key),
            ..Selector::default()
        }
    }

    /// Indicates whether or not a session matches this selector.
    fn matches(&self, entry: &Entry) -> bool {
        self.group
            .as_deref()
            .is_none_or(|wanted| wanted == entry.group)
            && self
                .host
                .as_deref()
                .is_none_or(|wanted| wanted == entry.host || wanted == entry.beta)
            && self
                .session
                .as_ref()
                .is_none_or(|wanted| *wanted == entry.session)
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
    /// Where each part of a resolution stands, in the order sent.
    Resolution(Vec<(SessionKey, PartState)>),
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
    pub identifier: SessionKey,
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

    /// Whether a supervisor is running but did not answer in time.
    pub fn is_unresponsive(&self) -> bool {
        matches!(self, Probe::Unresponsive)
    }

    /// What to tell someone about a supervisor of another build.
    pub fn mismatch_message(&self) -> Option<String> {
        match self {
            Probe::Mismatch(supervisor) => Some(mismatch_message(supervisor.as_deref())),
            _ => None,
        }
    }
}

/// What `status`, the shop and the tray say about a supervisor that is
/// running but answered nothing within the client timeout.
pub const UNRESPONSIVE: &str = "supervisor not responding";

/// Says what a supervisor that does not answer means, and what to do.
pub fn unresponsive_message() -> String {
    format!(
        "a supervisor is running but answered nothing within {} s; \
         `autobahn restart` if it stays so",
        CLIENT_TIMEOUT.as_secs()
    )
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
    /// The session's key: what tells two sessions apart when they share a
    /// group and a destination host.
    pub session: SessionKey,
    /// The session's group.
    pub group: String,
    /// The session's destination.
    pub host: String,
    /// What it is doing.
    pub progress: crate::progress::ProgressSnapshot,
}

/// The live progress of the session `key` names, from a supervisor's
/// answer. By key, not by group and host, which two betas on one host
/// share.
pub fn progress_of<'a>(
    sessions: &'a [SessionProgress],
    key: &SessionKey,
) -> Option<&'a crate::progress::ProgressSnapshot> {
    sessions
        .iter()
        .find(|session| session.session == *key)
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
    /// Parts of resolutions to apply before the next cycle.
    pub resolutions: std::sync::Mutex<Vec<PendingPart>>,
}

/// One registry entry: a session, its control flags, and its live
/// progress.
pub(crate) struct Entry {
    /// The session's key.
    pub session: SessionKey,
    /// The session's name for people to read.
    pub display: String,
    /// The session's mode, by name.
    pub mode: String,
    /// The session's group.
    pub group: String,
    /// The session's destination host.
    pub host: String,
    /// The session's beta, as its specification reads.
    pub beta: String,
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
    /// The resolutions sent lately, for `Resolved` to answer from.
    pub resolutions: std::sync::Mutex<Vec<Arc<Resolution>>>,
}

/// How many resolutions the registry remembers. `resolve` asks after its
/// own within moments; this only bounds what a long-running supervisor
/// keeps.
const REMEMBERED_RESOLUTIONS: usize = 64;

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
        if let ControlRequest::Resolve { id, parts } = request {
            // Every part's session must be running here, or none is
            // queued: half a resolution is worse than none.
            let mut workers = Vec::new();
            for part in parts {
                match entries.iter().find(|entry| entry.session == part.session) {
                    Some(entry) => workers.push(entry.control.clone()),
                    None => {
                        return ControlResponse::Error(format!(
                            "session {} is not supervised here",
                            part.session
                        ))
                    }
                }
            }
            let resolution = Arc::new(Resolution {
                id: *id,
                parts: std::sync::Mutex::new(
                    parts
                        .iter()
                        .map(|part| (part.session.clone(), PartState::Pending))
                        .collect(),
                ),
                workers: workers.clone(),
            });
            for (part, worker) in parts.iter().zip(&workers) {
                worker
                    .resolutions
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(PendingPart {
                        resolution: resolution.clone(),
                        part: part.clone(),
                    });
                worker.wake.store(true, Ordering::Relaxed);
            }
            let mut remembered = self
                .resolutions
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            remembered.push(resolution);
            let excess = remembered.len().saturating_sub(REMEMBERED_RESOLUTIONS);
            remembered.drain(..excess);
            return ControlResponse::Applied {
                sessions: parts.len(),
            };
        }
        if let ControlRequest::Resolved { id } = request {
            let remembered = self
                .resolutions
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            return match remembered.iter().find(|resolution| resolution.id == *id) {
                Some(resolution) => ControlResponse::Resolution(
                    resolution
                        .parts
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .clone(),
                ),
                None => ControlResponse::Error(format!("no resolution {id} is known here")),
            };
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
            | ControlRequest::Resolve { .. }
            | ControlRequest::Resolved { .. }
            | ControlRequest::Yield { .. }
            | ControlRequest::Versioned { .. } => {
                unreachable!("answered above")
            }
        };
        let mut sessions = 0;
        for entry in entries.iter() {
            if selector.matches(entry) {
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
/// proves nobody is listening. One served by another user is not this
/// user's supervisor, whatever it answers.
pub fn supervisor_is_running(state_root: &Path) -> bool {
    // A connection that cannot even be queued within the timeout is a
    // supervisor too wedged to accept, not an absent one.
    match connect_client(&socket_path(state_root), CLIENT_TIMEOUT) {
        Ok(stream) => matches!(peer_is_same_user(&stream), Ok(true)),
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
/// path from the same state root, wherever it lives. The directory is
/// `$XDG_RUNTIME_DIR/autobahn` on Linux when the system provides one (it
/// is already the user's own, and `0700`), and `autobahn-<uid>` in the
/// temporary directory otherwise, which `bind` makes private or refuses.
pub fn socket_path(state_root: &Path) -> PathBuf {
    socket_path_with(
        state_root,
        runtime_directory().as_deref(),
        &std::env::temp_dir(),
    )
}

/// `$XDG_RUNTIME_DIR`, on Linux, when it is set to an absolute path.
fn runtime_directory() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// [`socket_path`], given the runtime and temporary directories.
fn socket_path_with(state_root: &Path, runtime: Option<&Path>, temporary: &Path) -> PathBuf {
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
    let name = format!("{name}.sock");
    if let Some(runtime) = runtime {
        let fallback = runtime.join("autobahn").join(&name);
        if fallback.as_os_str().len() <= MAXIMUM_SOCKET_PATH {
            return fallback;
        }
    }
    let directory = format!("autobahn-{}", unsafe { libc::getuid() });
    let fallback = temporary.join(&directory).join(&name);
    if fallback.as_os_str().len() <= MAXIMUM_SOCKET_PATH {
        return fallback;
    }
    // An over-length TMPDIR would defeat the fallback too; /tmp is short by
    // construction.
    PathBuf::from("/tmp").join(directory).join(name)
}

/// Binds the control socket, replacing any stale socket file left by a
/// previous supervisor (the state lock already guarantees no *live* one
/// shares this state root's sessions).
pub(crate) fn bind(state_root: &Path) -> Result<UnixListener> {
    bind_at(state_root, &socket_path(state_root))
}

/// [`bind`], at a given socket path.
///
/// The state root, and a fallback directory the socket is in instead, are
/// made private with [`crate::fsutil::private_dir`] before anything is
/// bound or removed there: a fallback directory in the shared temporary
/// directory that another user made first — who could then replace the
/// socket and answer for this user's supervisor — is refused with an error
/// naming it, and a loose one of this user's is tightened.
fn bind_at(state_root: &Path, path: &Path) -> Result<UnixListener> {
    if let Some(parent) = state_root.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("unable to create {}", parent.display()))?;
        }
    }
    crate::fsutil::private_dir(state_root)
        .with_context(|| format!("unable to prepare the state root {}", state_root.display()))?;
    if let Some(parent) = path.parent() {
        if parent != state_root {
            crate::fsutil::private_dir(parent).with_context(|| {
                format!("unable to use {} for the control socket", parent.display())
            })?;
        }
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)
        .with_context(|| format!("unable to bind control socket {}", path.display()))?;
    // Restrict the socket itself as a second layer under the peer
    // credential check performed per connection.
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    listener
        .set_nonblocking(true)
        .context("unable to configure the control socket")?;
    Ok(listener)
}

/// Refuses a control socket served by another user: whoever serves it
/// answers `status`, `flush` and the rest, so a socket someone else put
/// there must not be spoken to.
fn refuse_another_user(path: &Path, stream: &UnixStream) -> Result<()> {
    if !peer_is_same_user(stream)? {
        anyhow::bail!(
            "the control socket {} is served by another user; refusing to send it a request",
            path.display()
        );
    }
    Ok(())
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
        // Refused, which on some kernels is what a full accept queue does
        // rather than making the client wait. The supervisor's lock says
        // which it was: still held means it is there and wedged.
        Err(error) if supervisor_lock_held(state_root) => {
            return Err(anyhow::Error::new(error).context(unresponsive()));
        }
        Err(error) => {
            return Err(anyhow::Error::new(error).context(format!(
                "unable to reach a running supervisor at {} (is `autobahn watch` running?)",
                path.display()
            )));
        }
    };
    refuse_another_user(&path, &stream)?;
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

/// Whether a supervisor holds this state root's lock — the one it takes
/// for as long as it runs. Asked only when a connection was refused, to
/// tell a wedged supervisor from an absent one. Acquiring it here would
/// prove it free, so the lock taken for the test is released immediately.
fn supervisor_lock_held(state_root: &Path) -> bool {
    let directory = state_root.join("supervisor");
    if !directory.join("lock").exists() {
        return false;
    }
    match crate::session::SessionLock::acquire(directory) {
        // Free: nothing is running here. (Dropped at once, which releases.)
        Ok(_lock) => false,
        Err(error) => error.downcast_ref::<crate::session::SessionLockHeld>().is_some(),
    }
}

/// [`probe`], with the client timeout given.
fn probe_within(state_root: &Path, timeout: Duration) -> Probe {
    let stream = match connect_client(&socket_path(state_root), timeout) {
        Ok(stream) => stream,
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => return Probe::Unresponsive,
        // A refusal is not proof of absence. Kernels differ on what a full
        // accept queue does to a connecting client: Linux makes it wait,
        // which times out above, while macOS refuses it at once — so a
        // wedged supervisor would read as no supervisor at all. The
        // supervisor's own lock settles it: still held means it is there
        // and not answering.
        Err(_) => match supervisor_lock_held(state_root) {
            true => return Probe::Unresponsive,
            false => return Probe::Absent,
        },
    };
    // One served by another user is not this user's supervisor.
    if !matches!(peer_is_same_user(&stream), Ok(true)) {
        return Probe::Absent;
    }
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
            session: SessionKey::new(format!("{group}-{host}")),
            display: format!("{group}@{host}"),
            mode: "two-way-safe".into(),
            published: Arc::default(),
            group: group.into(),
            host: host.into(),
            beta: format!("{host}:/tree"),
            control: Arc::default(),
            progress: Arc::default(),
        }
    }

    fn registry() -> Registry {
        Registry {
            yield_to: None,
            configuration: RwLock::new(Some("[groups.work]".into())),
            state_root: PathBuf::from("/nonexistent/state"),
            resolutions: Default::default(),
            entries: RwLock::new(vec![
                entry("work", "host1"),
                entry("work", "host2"),
                entry("other", "host1"),
            ]),
        }
    }

    /// A resolution names sessions by key. One the supervisor does not run
    /// refuses the whole request, queueing nothing; the rest are queued,
    /// each worker woken, and `Resolved` answers for them until they
    /// report. A part that goes last may go only once the others have.
    #[test]
    fn a_resolution_is_queued_whole_or_not_at_all() {
        let registry = registry();
        let part = |session: &str, last: bool| ResolutionPart {
            session: SessionKey::new(session),
            settlement: crate::session::Settlement::default(),
            last,
        };
        let response = registry.apply(&ControlRequest::Resolve {
            id: 1,
            parts: vec![part("work-host1", false), part("work-absent", true)],
        });
        assert!(
            matches!(response, ControlResponse::Error(_)),
            "{response:?}"
        );
        let entries = registry.entries.read().unwrap();
        assert!(entries
            .iter()
            .all(|entry| entry.control.resolutions.lock().unwrap().is_empty()));
        drop(entries);
        assert!(matches!(
            registry.apply(&ControlRequest::Resolved { id: 1 }),
            ControlResponse::Error(_)
        ));

        let response = registry.apply(&ControlRequest::Resolve {
            id: 2,
            parts: vec![part("work-host1", false), part("work-host2", true)],
        });
        assert!(
            matches!(response, ControlResponse::Applied { sessions: 2 }),
            "{response:?}"
        );
        let entries = registry.entries.read().unwrap();
        let queued = |index: usize| {
            let control = &entries[index].control;
            assert!(control.wake.load(Ordering::Relaxed), "the worker is woken");
            control.resolutions.lock().unwrap()[0].resolution.clone()
        };
        let resolution = queued(0);
        assert!(
            Arc::ptr_eq(&resolution, &queued(1)),
            "one resolution, shared"
        );
        let last = SessionKey::new("work-host2");
        assert!(resolution.others(&last).is_none(), "the last part waits");
        resolution.report(
            &SessionKey::new("work-host1"),
            PartState::Applied(Default::default()),
        );
        assert!(resolution.others(&last).is_some(), "and then may go");
        match registry.apply(&ControlRequest::Resolved { id: 2 }) {
            ControlResponse::Resolution(states) => {
                assert!(matches!(states[0].1, PartState::Applied(_)));
                assert!(matches!(states[1].1, PartState::Pending));
            }
            other => panic!("{other:?}"),
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

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::symlink_metadata(path)
            .expect("the path should exist")
            .permissions()
            .mode()
            & 0o7777
    }

    fn is_root() -> bool {
        // SAFETY: `geteuid` has no preconditions.
        unsafe { libc::geteuid() == 0 }
    }

    /// A state root too deep to hold its own socket, under `base`.
    fn deep_state_root(base: &Path) -> PathBuf {
        base.join("long-component/".repeat(8)).join("state")
    }

    #[test]
    fn with_a_runtime_directory_the_socket_is_placed_there() {
        let keep = tempfile::tempdir().expect("temporary directory");
        let runtime = keep.path().join("run");
        std::fs::create_dir(&runtime).unwrap();
        let temporary = keep.path().join("tmp");
        std::fs::create_dir(&temporary).unwrap();
        let state_root = deep_state_root(keep.path());
        let path = socket_path_with(&state_root, Some(&runtime), &temporary);
        assert_eq!(path.parent().unwrap(), runtime.join("autobahn"));
        assert_eq!(
            path,
            socket_path_with(&state_root, Some(&runtime), &temporary)
        );
        // Without one, the temporary directory's per-user one.
        let without = socket_path_with(&state_root, None, &temporary);
        assert!(without.starts_with(&temporary), "{without:?}");

        let _listener = bind_at(&state_root, &path).expect("binds");
        assert_eq!(mode(&runtime.join("autobahn")), 0o700);
        assert_eq!(mode(&state_root), 0o700);
        // This user's own supervisor is spoken to.
        let stream = connect_client(&path, CLIENT_TIMEOUT).expect("connects");
        refuse_another_user(&path, &stream).expect("the same user's socket is accepted");
    }

    #[test]
    fn a_loose_fallback_directory_is_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let keep = tempfile::tempdir().expect("temporary directory");
        let state_root = deep_state_root(keep.path());
        let path = socket_path_with(&state_root, None, keep.path());
        let directory = path.parent().unwrap();
        std::fs::create_dir(directory).unwrap();
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o777)).unwrap();
        let _listener = bind_at(&state_root, &path).expect("binds");
        assert_eq!(mode(directory), 0o700);
    }

    #[test]
    fn a_fallback_directory_that_is_a_link_is_refused() {
        let keep = tempfile::tempdir().expect("temporary directory");
        let state_root = deep_state_root(keep.path());
        let path = socket_path_with(&state_root, None, keep.path());
        let elsewhere = keep.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, path.parent().unwrap()).unwrap();
        let error = format!("{:#}", bind_at(&state_root, &path).unwrap_err());
        assert!(error.contains("symbolic link"), "{error}");
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    #[test]
    fn a_fallback_directory_owned_by_another_user_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        if !is_root() {
            eprintln!("skipped: giving a directory to another uid needs root");
            return;
        }
        let keep = tempfile::tempdir().expect("temporary directory");
        let state_root = deep_state_root(keep.path());
        let path = socket_path_with(&state_root, None, keep.path());
        let directory = path.parent().unwrap();
        std::fs::create_dir(directory).unwrap();
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o777)).unwrap();
        std::os::unix::fs::chown(directory, Some(65534), Some(65534)).unwrap();
        let error = format!("{:#}", bind_at(&state_root, &path).unwrap_err());
        assert!(error.contains(&directory.display().to_string()), "{error}");
        assert!(error.contains("owned by uid 65534"), "{error}");
        assert!(!path.exists());
        assert_eq!(mode(directory), 0o777);
    }

    #[test]
    fn a_client_refuses_a_server_run_by_another_user() {
        use std::io::BufRead;
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::process::CommandExt;
        if !is_root() {
            eprintln!("skipped: serving as another uid needs root");
            return;
        }
        let keep = tempfile::tempdir().expect("temporary directory");
        std::fs::set_permissions(keep.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        let path = keep.path().join("control.sock");
        let mut server = std::process::Command::new("python3")
            .arg("-c")
            .arg(
                "import socket, sys\n\
                 s = socket.socket(socket.AF_UNIX)\n\
                 s.bind(sys.argv[1])\n\
                 s.listen(1)\n\
                 print('ready', flush=True)\n\
                 c, _ = s.accept()\n\
                 c.recv(1)\n",
            )
            .arg(&path)
            .uid(65534)
            .gid(65534)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("python3 should run");
        let mut ready = String::new();
        std::io::BufReader::new(server.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready.trim(), "ready");
        let error = format!(
            "{:#}",
            send(keep.path(), &ControlRequest::Flush(Selector::default())).unwrap_err()
        );
        assert!(error.contains("another user"), "{error}");
        let _ = server.kill();
        let _ = server.wait();
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
            session: None,
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
            session: None,
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
            session: None,
        }));
        assert!(matches!(response, ControlResponse::Error(_)));
    }

    /// Two betas of one group on one host share the group and the host;
    /// a selector tells them apart by the beta's specification, as status
    /// names it, or by the session's key.
    #[test]
    fn two_betas_on_one_host_are_selected_apart() {
        let beta = |path: &str| Entry {
            session: SessionKey::new(format!("work-{path}")),
            beta: format!("host:{path}"),
            ..entry("work", "host")
        };
        let registry = Registry {
            entries: RwLock::new(vec![beta("/tree"), beta("/tree/nested")]),
            ..registry()
        };
        let paused = |index: usize| {
            registry.entries.read().unwrap()[index]
                .control
                .paused
                .load(Ordering::Relaxed)
        };

        // The host names both.
        let response = registry.apply(&ControlRequest::Flush(Selector {
            group: Some("work".into()),
            host: Some("host".into()),
            session: None,
        }));
        assert!(matches!(response, ControlResponse::Applied { sessions: 2 }));

        // The beta's specification names one.
        let response = registry.apply(&ControlRequest::Pause(Selector {
            group: Some("work".into()),
            host: Some("host:/tree/nested".into()),
            session: None,
        }));
        assert!(matches!(response, ControlResponse::Applied { sessions: 1 }));
        assert!(!paused(0));
        assert!(paused(1));

        // So does the key.
        let response = registry.apply(&ControlRequest::Pause(Selector::session(SessionKey::new(
            "work-/tree",
        ))));
        assert!(matches!(response, ControlResponse::Applied { sessions: 1 }));
        assert!(paused(0));
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

    /// How many connections the fixture will queue before it decides the
    /// platform will not refuse one. Above every backlog seen so far
    /// (Linux 1, macOS 128) and far below the open-file limit on both, so
    /// a refusal here is the queue filling and not descriptors running
    /// out.
    const QUEUE_PROBE_LIMIT: usize = 200;

    /// A control socket whose supervisor is wedged: bound and listening,
    /// never accepting. With `backlog_full`, its queue of pending
    /// connections is full too, so a blocking connect would never return.
    fn wedged_socket(
        state_root: &Path,
        backlog_full: bool,
    ) -> (UnixListener, crate::session::SessionLock) {
        // A real supervisor holds this for as long as it runs, and the
        // probe reads it to tell "wedged" from "gone".
        let held = crate::session::SessionLock::acquire(state_root.join("supervisor"))
            .expect("the supervisor lock is free in a fresh state root");
        let listener = UnixListener::bind(socket_path(state_root)).expect("binds");
        if backlog_full {
            use std::os::unix::io::AsRawFd;
            // Listening again sets the backlog. A kernel reads the number
            // as a hint, not an instruction: Linux queues one connection
            // for `0` and refuses the second, while macOS keeps its own
            // minimum and queues 128 (measured). So the fixture fills the
            // queue by connecting until one is refused, rather than
            // assuming how deep it is.
            assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
            let mut pending = Vec::new();
            let mut full = false;
            for _ in 0..QUEUE_PROBE_LIMIT {
                match connect_client(&socket_path(state_root), Duration::ZERO) {
                    Ok(stream) => pending.push(stream),
                    Err(_) => {
                        full = true;
                        break;
                    }
                }
            }
            assert!(
                full,
                "the backlog never filled: {} connections were queued and none refused",
                pending.len()
            );
            std::mem::forget(pending);
        }
        (listener, held)
    }

    #[test]
    fn a_wedged_supervisor_is_unresponsive_within_the_client_timeout() {
        for backlog_full in [false, true] {
            let root = tempfile::tempdir().expect("a temporary directory");
            let _wedged = wedged_socket(root.path(), backlog_full);
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
        let _wedged = wedged_socket(root.path(), true);
        let started = std::time::Instant::now();
        let report = crate::supervisor::status_report(&[], root.path());
        assert!(started.elapsed() < CLIENT_TIMEOUT + Duration::from_secs(5));
        assert!(report.supervisor_running);
        // And says so, rather than nothing or "another build".
        assert!(report.supervisor_unresponsive);
        assert!(report.supervisor_mismatch.is_none());
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["supervisor_unresponsive"], true);
    }

    #[test]
    fn probing_a_socket_nobody_listens_on_is_absent() {
        let root = tempfile::tempdir().expect("a temporary directory");
        assert!(matches!(probe(root.path()), Probe::Absent));
        assert!(!probe(root.path()).is_running());
    }
}
