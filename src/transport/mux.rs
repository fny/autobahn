//! The controller side of agent connection multiplexing.
//!
//! One agent connection (one SSH process, one TCP connection, one
//! authentication) carries any number of session channels. This is what
//! keeps a host that serves many sessions — or a host behind a per-IP
//! connection rate limit — from being hammered by one connection per
//! session: the supervisor opens a single connection per host and each
//! session becomes a channel on it.
//!
//! The shape is deliberately simple. Each channel is strict
//! request/response (at most one outstanding request), so the router needs
//! no reordering: a single reader thread tags responses back to their
//! channels' queues, and senders interleave whole frames through a shared
//! writer. The agent serves each channel on its own thread, so one
//! channel's blocking change-wait never stalls another's scan.
//!
//! Lifecycle: the connection shuts down (and the agent process is reaped)
//! when its last channel closes. A connection whose transport fails marks
//! itself dead and unblocks every waiting channel with the failure; pooled
//! callers observe the death and build a fresh connection.

use std::collections::HashMap;
use std::io::Write;
use std::process::Child;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};

use crate::protocol::{Handshake, Initialize, MuxRequest, MuxResponse, Request, Response};

use super::Connection;

/// A multiplexed agent connection: a cloneable handle through which
/// channels are opened.
#[derive(Clone)]
pub struct AgentConnection {
    /// The shared connection state.
    shared: Arc<Shared>,
}

/// One session channel on an agent connection, speaking the ordinary
/// request/response protocol. Dropping the channel closes it (and shuts the
/// connection down if it was the last).
pub struct AgentChannel {
    /// The shared connection state.
    shared: Arc<Shared>,
    /// This channel's identifier.
    channel: u32,
    /// The stream of responses routed to this channel.
    receiver: mpsc::Receiver<Response>,
}

/// The state shared between channel handles and the reader thread.
struct Shared {
    /// The frame writer (senders interleave whole frames).
    writer: Mutex<Box<dyn Write + Send>>,
    /// The agent process, if this connection owns one.
    child: Mutex<Option<Child>>,
    /// The routing table and lifecycle flags.
    state: Mutex<Router>,
    /// The next channel identifier to assign.
    next_channel: AtomicU32,
}

/// The routing table and lifecycle flags.
struct Router {
    /// Response queues by channel.
    channels: HashMap<u32, mpsc::Sender<Response>>,
    /// The number of live [`AgentChannel`] handles.
    open: usize,
    /// The transport failure that killed the connection, if any.
    dead: Option<String>,
    /// Whether or not shutdown has begun (guards double reaping).
    shutdown: bool,
}

impl AgentConnection {
    /// Establishes a multiplexed connection: exchanges handshakes
    /// (enforcing version equality) and starts the response router.
    pub fn connect(connection: Connection) -> Result<AgentConnection> {
        let (mut reader, mut writer, child) = connection.into_parts();

        // Exchange handshakes. Ours goes out first (the agent does the
        // same), so neither side blocks waiting for the other to speak.
        super::send_frame(&mut writer, &super::local_handshake())
            .context("unable to send handshake")?;
        let peer: Handshake =
            super::receive_frame(&mut reader).context("unable to receive the agent's handshake")?;
        super::verify_handshake(&peer)?;

        let shared = Arc::new(Shared {
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            state: Mutex::new(Router {
                channels: HashMap::new(),
                open: 0,
                dead: None,
                shutdown: false,
            }),
            next_channel: AtomicU32::new(1),
        });

        // The router: the connection's only reader. It ends when the stream
        // does — cleanly after a shutdown, or with the failure it then
        // reports to every waiting channel.
        let router = shared.clone();
        std::thread::spawn(move || {
            let failure = loop {
                match super::receive_frame::<_, MuxResponse>(&mut reader) {
                    Ok(MuxResponse { channel, response }) => {
                        let state = router
                            .state
                            .lock()
                            .expect("the state lock is never poisoned");
                        if let Some(sender) = state.channels.get(&channel) {
                            // A failed send means the channel handle is
                            // being dropped; its close is already on the
                            // way.
                            let _ = sender.send(response);
                        }
                    }
                    Err(error) => break format!("{error:#}"),
                }
            };
            router.fail(failure);
        });

        Ok(AgentConnection { shared })
    }

    /// Opens a session channel, initializing its endpoint on the agent.
    pub fn open(&self, initialize: Initialize) -> Result<AgentChannel> {
        let channel = self.shared.next_channel.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        {
            let mut state = self
                .shared
                .state
                .lock()
                .expect("the state lock is never poisoned");
            if let Some(reason) = &state.dead {
                bail!("the agent connection has failed: {reason}");
            }
            if state.shutdown {
                bail!("the agent connection has shut down");
            }
            state.channels.insert(channel, sender);
            state.open += 1;
        }
        let opened = (|| -> Result<()> {
            self.shared
                .send(&MuxRequest::Open {
                    channel,
                    initialize,
                })
                .context("unable to send channel open")?;
            match receiver.recv() {
                Ok(Response::Initialized) => Ok(()),
                Ok(Response::Error(message)) => bail!("remote error: {message}"),
                Ok(_) => bail!("protocol error: unexpected answer to a channel open"),
                Err(_) => bail!(
                    "the agent connection failed during the channel open: {}",
                    self.shared.death_reason()
                ),
            }
        })();
        match opened {
            Ok(()) => Ok(AgentChannel {
                shared: self.shared.clone(),
                channel,
                receiver,
            }),
            Err(error) => {
                self.shared.release(channel);
                Err(error)
            }
        }
    }

    /// Indicates whether or not the connection can still open channels.
    pub fn usable(&self) -> bool {
        let state = self
            .shared
            .state
            .lock()
            .expect("the state lock is never poisoned");
        state.dead.is_none() && !state.shutdown
    }
}

impl AgentChannel {
    /// Performs one request/response exchange on this channel. The returned
    /// response may be [`Response::Error`] (a request-level failure on the
    /// far side); a transport failure is an error here.
    pub fn exchange(&mut self, request: Request) -> Result<Response> {
        self.shared
            .send(&MuxRequest::Request {
                channel: self.channel,
                request,
            })
            .context("unable to send request to the agent")?;
        self.receiver.recv().map_err(|_| {
            anyhow!(
                "the agent connection failed: {}",
                self.shared.death_reason()
            )
        })
    }
}

impl Drop for AgentChannel {
    fn drop(&mut self) {
        // Close the channel (best-effort: a dead connection has nothing to
        // tell) and let the shared state decide whether the whole
        // connection should shut down.
        let _ = self.shared.send(&MuxRequest::Close {
            channel: self.channel,
        });
        self.shared.release(self.channel);
    }
}

impl Shared {
    /// Sends one frame through the shared writer.
    fn send(&self, frame: &MuxRequest) -> Result<()> {
        let mut writer = self
            .writer
            .lock()
            .expect("the writer lock is never poisoned");
        super::send_frame(&mut *writer, frame)
    }

    /// Returns the recorded death reason (or a generic disconnection).
    fn death_reason(&self) -> String {
        self.state
            .lock()
            .expect("the state lock is never poisoned")
            .dead
            .clone()
            .unwrap_or_else(|| "the connection closed".to_owned())
    }

    /// Marks the connection dead, unblocking every waiting channel, and
    /// reaps the agent process.
    fn fail(&self, reason: String) {
        let already_down = {
            let mut state = self.state.lock().expect("the state lock is never poisoned");
            let already_down = state.shutdown || state.dead.is_some();
            state.dead.get_or_insert(reason);
            // Dropping the senders is what unblocks the receivers.
            state.channels.clear();
            already_down
        };
        if !already_down {
            self.reap(false);
        }
    }

    /// Releases one channel handle, shutting the connection down when it
    /// was the last.
    fn release(&self, channel: u32) {
        let shut_down = {
            let mut state = self.state.lock().expect("the state lock is never poisoned");
            state.channels.remove(&channel);
            state.open = state.open.saturating_sub(1);
            if state.open == 0 && !state.shutdown && state.dead.is_none() {
                state.shutdown = true;
                true
            } else {
                false
            }
        };
        if shut_down {
            let _ = self.send(&MuxRequest::Shutdown);
            self.reap(true);
        }
    }

    /// Reaps the agent process: a graceful reap waits for the exit that the
    /// shutdown frame causes; an ungraceful one kills first.
    fn reap(&self, graceful: bool) {
        let Some(mut child) = self
            .child
            .lock()
            .expect("the child lock is never poisoned")
            .take()
        else {
            return;
        };
        if !graceful {
            let _ = child.kill();
        }
        if child.wait().is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // Last-resort cleanup: normally the last channel release or a
        // transport failure has already reaped, but a connection dropped
        // with zero channels ever opened must not leak its process.
        if let Some(mut child) = self
            .child
            .lock()
            .expect("the child lock is never poisoned")
            .take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A pool of agent connections keyed by their spawn command, sharing one
/// connection per key and serializing (re)establishment per key — which is
/// also what makes first-contact agent installation happen once per host
/// rather than once per session.
#[derive(Default)]
pub struct AgentPool {
    /// The per-key slots.
    slots: Mutex<HashMap<Vec<String>, Arc<Mutex<Slot>>>>,
}

/// One pool slot: the live connection for a key, if any.
#[derive(Default)]
struct Slot {
    /// The connection, which may have died or shut down since it was
    /// stored.
    connection: Option<AgentConnection>,
}

impl AgentPool {
    /// Opens a channel on the pooled connection for `key`, building a fresh
    /// connection with `establish` when none exists or the existing one is
    /// no longer usable. Establishment (including any agent installation it
    /// performs) runs under the key's lock, so concurrent sessions for one
    /// host wait for a single bootstrap instead of racing their own.
    pub fn channel(
        &self,
        key: &[String],
        initialize: Initialize,
        establish: impl FnOnce() -> Result<AgentConnection>,
    ) -> Result<AgentChannel> {
        let slot = {
            let mut slots = self.slots.lock().expect("the pool lock is never poisoned");
            slots.entry(key.to_vec()).or_default().clone()
        };
        let mut slot = slot.lock().expect("the slot lock is never poisoned");
        if let Some(connection) = &slot.connection {
            if connection.usable() {
                // An open failure on a connection that *looked* usable means
                // it died underneath us; fall through and rebuild.
                if let Ok(channel) = connection.open(initialize.clone()) {
                    return Ok(channel);
                }
            }
        }
        let connection = establish()?;
        let channel = connection.open(initialize)?;
        slot.connection = Some(connection);
        Ok(channel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::SymlinkMode;
    use crate::transport::tests::connected_pair;

    /// Builds a test initialization for the specified root.
    fn initialize(root: &std::path::Path) -> Initialize {
        Initialize {
            root: root.to_string_lossy().into_owned(),
            session: format!("mux-test-{}", root.to_string_lossy().len()),
            ignores: Vec::new(),
            symlink_mode: SymlinkMode::Raw,
            file_mode: None,
            directory_mode: None,
        }
    }

    #[test]
    fn channels_multiplex_without_blocking_each_other() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let root_a = keep.path().join("a");
        let root_b = keep.path().join("b");
        std::fs::create_dir_all(&root_a).expect("root should be creatable");
        std::fs::create_dir_all(&root_b).expect("root should be creatable");
        std::fs::write(root_a.join("file.txt"), b"content").expect("file should be writable");

        // The agent serves both channels over one in-memory connection.
        let (client, agent) = connected_pair();
        let (agent_reader, agent_writer, _) = agent.into_parts();
        let server =
            std::thread::spawn(move || crate::transport::serve_agent(agent_reader, agent_writer));

        let connection = AgentConnection::connect(client).expect("unable to connect");
        let mut channel_a = connection.open(initialize(&root_a)).expect("open a");
        let mut channel_b = connection.open(initialize(&root_b)).expect("open b");

        // Channel B blocks in a long change wait; channel A's scan must
        // complete while B is still waiting — the proof that channels are
        // served concurrently.
        std::thread::scope(|scope| {
            let waiter = scope.spawn(move || {
                let started = std::time::Instant::now();
                let response = channel_b
                    .exchange(Request::AwaitChanges(2_000))
                    .expect("await should exchange");
                assert!(matches!(response, Response::AwaitChanges(false)));
                (started.elapsed(), channel_b)
            });
            // Give the wait a moment to actually start.
            std::thread::sleep(std::time::Duration::from_millis(100));
            let started = std::time::Instant::now();
            let response = channel_a
                .exchange(Request::Scan)
                .expect("scan should exchange");
            let scan_elapsed = started.elapsed();
            let Response::Scan(snapshot) = response else {
                panic!("expected a scan response");
            };
            assert_eq!(snapshot.files, 1);
            assert!(
                scan_elapsed < std::time::Duration::from_millis(1_000),
                "the scan waited on the other channel: {scan_elapsed:?}"
            );
            let (await_elapsed, channel_b) = waiter.join().expect("waiter");
            assert!(await_elapsed >= std::time::Duration::from_millis(1_500));
            drop(channel_b);
        });
        drop(channel_a);
        drop(connection);

        // With every channel closed, the agent shut down cleanly.
        server
            .join()
            .expect("the agent thread panicked")
            .expect("the agent should exit cleanly");
    }

    #[test]
    fn an_abrupt_disconnect_ends_the_agent() {
        // A controller that dies (rather than closing channels) reaches the
        // agent as a bare end-of-stream. The agent must exit — with open
        // channels and all — instead of deadlocking its dispatcher against
        // channel threads that would otherwise wait on their queues forever.
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let root = keep.path().join("root");
        std::fs::create_dir_all(&root).expect("root should be creatable");

        let (client, agent) = connected_pair();
        let (agent_reader, agent_writer, _) = agent.into_parts();
        let (finished_sender, finished) = mpsc::channel();
        std::thread::spawn(move || {
            let result = crate::transport::serve_agent(agent_reader, agent_writer);
            let _ = finished_sender.send(result);
        });

        // The protocol is spoken by hand so that the transport can be
        // severed with the channel still open (the real client would send a
        // close on the way down).
        let mut client = client;
        client
            .send(&crate::transport::local_handshake())
            .expect("handshake");
        let _: Handshake = client.receive().expect("handshake");
        client
            .send(&MuxRequest::Open {
                channel: 1,
                initialize: initialize(&root),
            })
            .expect("open");
        let _: MuxResponse = client.receive().expect("opened");
        drop(client);

        let result = finished
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the agent must exit after an abrupt disconnect");
        result.expect("a clean end-of-stream is a clean agent exit");
    }

    #[test]
    fn the_pool_shares_one_connection_per_key() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let root = keep.path().join("root");
        std::fs::create_dir_all(&root).expect("root should be creatable");

        let (client, agent) = connected_pair();
        let (agent_reader, agent_writer, _) = agent.into_parts();
        let _server =
            std::thread::spawn(move || crate::transport::serve_agent(agent_reader, agent_writer));

        let pool = AgentPool::default();
        let key = vec!["test-host".to_owned()];
        let mut connections_built = 0;
        let mut client = Some(client);
        let mut establish = || {
            connections_built += 1;
            AgentConnection::connect(client.take().expect("only one connection may be built"))
        };

        let channel_one = pool
            .channel(&key, initialize(&root), &mut establish)
            .expect("first channel");
        let channel_two = pool
            .channel(&key, initialize(&root), &mut establish)
            .expect("second channel");
        assert_eq!(connections_built, 1);
        drop(channel_one);
        drop(channel_two);
    }
}
