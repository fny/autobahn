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
    /// The request failed.
    Error(String),
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
}

/// The registry mapping sessions to their control flags, shared between the
/// socket thread and the workers.
pub(crate) struct Registry {
    /// One entry per supervised session: group, host, and flags.
    pub entries: Vec<(String, String, Arc<WorkerControl>)>,
}

impl Registry {
    /// Applies a control request, returning the number of affected sessions.
    fn apply(&self, request: &ControlRequest) -> ControlResponse {
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
        };
        let mut sessions = 0;
        for (group, host, control) in &self.entries {
            if selector.matches(group, host) {
                action(control);
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
    std::env::temp_dir()
        .join(format!("autobahn-{}", unsafe { libc::getuid() }))
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

/// Handles one control connection: a single request/response exchange.
fn handle(stream: UnixStream, registry: &Registry) -> Result<()> {
    stream
        .set_nonblocking(false)
        .context("unable to configure the control connection")?;
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
            "unable to reach a running supervisor at {} (is `autobahn up` running?)",
            path.display()
        )
    })?;
    let mut reader = stream
        .try_clone()
        .context("unable to clone the control connection")?;
    let mut writer = stream;
    crate::transport::send_control_frame(&mut writer, request)?;
    crate::transport::receive_control_frame(&mut reader)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        Registry {
            entries: vec![
                ("work".into(), "host1".into(), Arc::default()),
                ("work".into(), "host2".into(), Arc::default()),
                ("other".into(), "host1".into(), Arc::default()),
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
        assert!(registry.entries[0].2.paused.load(Ordering::Relaxed));
        assert!(!registry.entries[2].2.paused.load(Ordering::Relaxed));
        // One session.
        let response = registry.apply(&ControlRequest::Reset(Selector {
            group: Some("other".into()),
            host: Some("host1".into()),
        }));
        assert!(matches!(response, ControlResponse::Applied { sessions: 1 }));
        assert!(registry.entries[2].2.reset.load(Ordering::Relaxed));
        // No match.
        let response = registry.apply(&ControlRequest::Flush(Selector {
            group: Some("absent".into()),
            host: None,
        }));
        assert!(matches!(response, ControlResponse::Error(_)));
    }
}
