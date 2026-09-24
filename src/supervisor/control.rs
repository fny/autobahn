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
use std::sync::Arc;

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
    /// The session's group.
    pub group: String,
    /// The session's destination.
    pub host: String,
    /// What it is doing.
    pub progress: crate::progress::ProgressSnapshot,
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
    /// The session's group.
    pub group: String,
    /// The session's destination.
    pub host: String,
    /// The flags the control socket flips and the worker consumes.
    pub control: Arc<WorkerControl>,
    /// What the session is doing, updated by the worker as it works.
    pub progress: Arc<crate::progress::Progress>,
}

/// Peering: what the control socket calls to hand the lead on.
pub type YieldHandle = Arc<dyn Fn(&str) -> anyhow::Result<()> + Send + Sync>;

/// The registry mapping sessions to their control flags, shared between the
/// socket thread and the workers.
pub(crate) struct Registry {
    /// One entry per supervised session.
    pub entries: Vec<Entry>,
    /// Peering: how to hand the lead on, when this supervisor leads.
    pub yield_to: Option<YieldHandle>,
}

impl Registry {
    /// Applies a control request, returning the number of affected sessions.
    fn apply(&self, request: &ControlRequest) -> ControlResponse {
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
                    for entry in &self.entries {
                        entry.control.wake.store(true, Ordering::Relaxed);
                    }
                    ControlResponse::Applied {
                        sessions: self.entries.len(),
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
                Ok(inner) => self.apply(&inner),
                Err(error) => ControlResponse::Error(format!("undecodable request: {error}")),
            };
        }
        if let ControlRequest::Progress = request {
            return ControlResponse::Progress(
                self.entries
                    .iter()
                    .map(|entry| SessionProgress {
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
            | ControlRequest::Yield { .. }
            | ControlRequest::Versioned { .. } => {
                unreachable!("answered above")
            }
        };
        let mut sessions = 0;
        for entry in &self.entries {
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
    UnixStream::connect(socket_path(state_root)).is_ok()
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
                    eprintln!("control request failed: {error:#}");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(error) => {
                eprintln!("control socket failed: {error:#}");
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
    let path = socket_path(state_root);
    let stream = UnixStream::connect(&path).with_context(|| {
        format!(
            "unable to reach a running supervisor at {} (is `autobahn watch` running?)",
            path.display()
        )
    })?;
    let mut reader = stream
        .try_clone()
        .context("unable to clone the control connection")?;
    let mut writer = stream;
    let versioned = ControlRequest::Versioned {
        version: crate::protocol::version(),
        request: bincode::serialize(request).context("unable to encode the control request")?,
    };
    crate::transport::send_control_frame(&mut writer, &versioned)?;
    match crate::transport::receive_control_frame(&mut reader) {
        Ok(ControlResponse::Mismatch { supervisor }) => {
            Err(anyhow::anyhow!(mismatch_message(Some(&supervisor))))
        }
        Ok(response) => Ok(response),
        // Connected, and then nothing that decodes: a supervisor from
        // before builds were compared, which cannot read the envelope.
        Err(error) => Err(error.context(mismatch_message(None))),
    }
}

/// Asks the supervisor owning `state_root` what its sessions are doing,
/// and says which of the three answers came back.
pub fn probe(state_root: &Path) -> Probe {
    let stream = match UnixStream::connect(socket_path(state_root)) {
        Ok(stream) => stream,
        Err(_) => return Probe::Absent,
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
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
    if crate::transport::send_control_frame(&mut writer, &versioned).is_err() {
        return Probe::Mismatch(None);
    }
    match crate::transport::receive_control_frame(&mut reader) {
        Ok(ControlResponse::Progress(sessions)) => Probe::Answered(sessions),
        Ok(ControlResponse::Mismatch { supervisor }) => Probe::Mismatch(Some(supervisor)),
        _ => Probe::Mismatch(None),
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
            group: group.into(),
            host: host.into(),
            control: Arc::default(),
            progress: Arc::default(),
        }
    }

    fn registry() -> Registry {
        Registry {
            yield_to: None,
            entries: vec![
                entry("work", "host1"),
                entry("work", "host2"),
                entry("other", "host1"),
            ],
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
        assert!(registry.entries[0].control.paused.load(Ordering::Relaxed));
        assert!(!registry.entries[2].control.paused.load(Ordering::Relaxed));
        // One session.
        let response = registry.apply(&ControlRequest::Reset(Selector {
            group: Some("other".into()),
            host: Some("host1".into()),
        }));
        assert!(matches!(response, ControlResponse::Applied { sessions: 1 }));
        assert!(registry.entries[2].control.reset.load(Ordering::Relaxed));
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
        for entry in &registry.entries {
            entry.control.wake.store(false, Ordering::Relaxed);
        }
        registry.apply(&versioned("0.0.1+e1", &flush));
        assert!(registry
            .entries
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
    }

    #[test]
    fn probing_a_socket_nobody_listens_on_is_absent() {
        let root = tempfile::tempdir().expect("a temporary directory");
        assert!(matches!(probe(root.path()), Probe::Absent));
        assert!(!probe(root.path()).is_running());
    }
}
