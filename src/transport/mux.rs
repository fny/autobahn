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
//! request/response (at most one outstanding request, which the router
//! enforces: an unsolicited or duplicate response is a protocol violation
//! that fails the connection rather than desynchronizing a channel), so
//! routing needs no reordering: a single reader thread tags responses back
//! to their channels' queues, and senders interleave whole frames through a
//! shared writer. The agent serves each channel on its own thread, so one
//! channel's blocking change-wait never stalls another's scan.
//!
//! Lifecycle: the connection is kept alive by its handles
//! ([`AgentConnection`] clones — a pool typically holds one) and its open
//! channels, and shuts down (closing the writer so the agent sees
//! end-of-stream, then reaping the process with a bounded wait) when the
//! last of both is gone. A transport failure marks the connection dead,
//! unblocks every waiting channel with the failure, and reaps; pooled
//! callers observe the death and build a fresh connection.

use std::collections::HashMap;
use std::io::Write;
use std::process::Child;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};

use crate::protocol::{Handshake, Initialize, MuxRequest, MuxResponse, Request, Response};

use super::Connection;

/// How long a graceful shutdown waits for the agent to exit before killing
/// it. The writer is closed first, so a healthy agent exits on end-of-stream
/// almost immediately; the timeout only bounds a wedged one.
const REAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How long each step of setting a connection up may take: the agent's
/// handshake, and each channel open. A login stuck in a slow rc file, or an
/// agent that is alive but wedged, otherwise holds every session to that
/// host forever without an error, so nothing retries and nothing alerts.
pub const SETUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// How long a session waits for another session's connection to the same
/// host to be established. Establishment is itself bounded (each ssh step
/// and each setup step has its own deadline), so this only bounds the sum
/// of an installation's steps.
const POOL_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// A multiplexed agent connection handle. Handles (and open channels) keep
/// the connection alive; channels are opened through any handle.
pub struct AgentConnection {
    /// The shared connection state.
    shared: Arc<Shared>,
}

/// One session channel on an agent connection, speaking the ordinary
/// request/response protocol. Dropping the channel closes it; the
/// connection shuts down once no channels and no handles remain.
pub struct AgentChannel {
    /// The shared connection state.
    shared: Arc<Shared>,
    /// This channel's identifier.
    channel: u32,
    /// The stream of responses routed to this channel.
    receiver: mpsc::Receiver<Response>,
    /// Whether or not [`close`](AgentChannel::close) already released the
    /// channel (so the drop must not release it again).
    closed: bool,
}

/// The state shared between handles, channels, and the router thread.
struct Shared {
    /// The frame writer (senders interleave whole frames). Replaced with a
    /// sink at shutdown, which is what closes the agent's standard input.
    writer: Mutex<Box<dyn Write + Send>>,
    /// The agent process, if this connection owns one.
    child: Mutex<Option<Child>>,
    /// The routing table and lifecycle flags.
    state: Mutex<Router>,
    /// The next channel identifier to assign.
    next_channel: AtomicU32,
    /// The agent's relayed standard error, kept for the connection's life.
    _stderr: Option<super::StderrRelay>,
    /// How long a channel open may wait for its answer.
    setup_timeout: std::time::Duration,
}

/// One channel's routing slot.
struct Slot {
    /// The response queue.
    sender: mpsc::Sender<Response>,
    /// The number of outstanding requests (a response arriving with none
    /// is a protocol violation). Ordinary exchanges keep this at most one;
    /// windowed staging pushes keep several acknowledgements in flight.
    /// The counter bounds, rather than eliminates, duplicate damage: a
    /// duplicate landing while a request is outstanding is delivered as an
    /// answer — which surfaces as a typed protocol error at the caller —
    /// and the genuine answer that follows then fails the connection.
    /// Nothing desynchronizes silently.
    outstanding: u32,
}

/// The routing table and lifecycle flags.
struct Router {
    /// The routing slots by channel.
    channels: HashMap<u32, Slot>,
    /// The number of live [`AgentChannel`] values.
    open: usize,
    /// The number of live [`AgentConnection`] handles.
    handles: usize,
    /// The transport failure that killed the connection, if any.
    dead: Option<String>,
    /// Whether or not shutdown has begun (guards double reaping).
    shutdown: bool,
}

impl AgentConnection {
    /// Establishes a multiplexed connection: exchanges handshakes
    /// (enforcing version equality) and starts the response router. A
    /// handshake failure reaps the spawned process before reporting.
    pub fn connect(connection: Connection) -> Result<AgentConnection> {
        AgentConnection::connect_within(connection, SETUP_TIMEOUT)
    }

    /// Establishes a multiplexed connection as [`connect`](Self::connect)
    /// does, with `setup_timeout` bounding the handshake and each channel
    /// open. A missed deadline fails the connection as
    /// [`ConnectionFailed`], so its sessions back off and reconnect.
    pub fn connect_within(
        mut connection: Connection,
        setup_timeout: std::time::Duration,
    ) -> Result<AgentConnection> {
        // Held until the handshake proves the far side is an agent. After
        // that it is the agent's own voice, and everything it says about
        // itself — a watch it could not establish above all — is worth
        // hearing. Before that it is ssh's, and belongs in the error.
        let stderr = connection.take_stderr_relay();
        let (reader, mut writer, child) = connection.into_parts();

        // Exchange handshakes. Ours goes out first (the agent does the
        // same), so neither side blocks waiting for the other to speak. On
        // failure the child must be reaped here — `into_parts` transferred
        // that responsibility to us. The agent's handshake is read on its
        // own thread, which hands the reader back, so that the wait for it
        // has a deadline: a read cannot be interrupted, but killing the
        // child ends it.
        let (arrived, arrival) = mpsc::channel();
        let handshake = std::thread::Builder::new()
            .name("autobahn-handshake".into())
            .spawn(move || {
                let mut reader = reader;
                let peer = super::receive_frame::<_, Handshake>(&mut reader);
                let _ = arrived.send((reader, peer));
            })
            .context("unable to start the handshake reader")
            .and_then(|_| {
                super::send_frame(&mut writer, &super::local_handshake())
                    .context("unable to send handshake")?;
                let (reader, peer) = arrival.recv_timeout(setup_timeout).map_err(|_| {
                    ConnectionFailed::new(&format!(
                        "the agent did not answer the handshake within {}",
                        describe_timeout(setup_timeout)
                    ))
                })?;
                let peer = peer.context("unable to receive the agent's handshake")?;
                super::verify_handshake(&peer)?;
                Ok(reader)
            });
        let mut reader = match handshake {
            Ok(reader) => reader,
            Err(error) => {
                if let Some(mut child) = child {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                // Read after the reap, so what ssh wrote on its way out has
                // arrived. Best effort: the relay thread may still be a line
                // behind, and a diagnosis short one line beats none.
                let said = stderr
                    .as_ref()
                    .map(|relay| relay.held().join("\n"))
                    .unwrap_or_default();
                return Err(match said.is_empty() {
                    true => error,
                    false => error.context(format!("the far side said: {said}")),
                });
            }
        };
        if let Some(relay) = &stderr {
            relay.release();
        }

        let shared = Arc::new(Shared {
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            _stderr: stderr,
            state: Mutex::new(Router {
                channels: HashMap::new(),
                open: 0,
                handles: 1,
                dead: None,
                shutdown: false,
            }),
            next_channel: AtomicU32::new(1),
            setup_timeout,
        });

        // The router: the connection's only reader. It ends when the stream
        // does — cleanly after a shutdown, or with the failure it then
        // reports to every waiting channel.
        let router = shared.clone();
        crate::threads::spawn_deep(move || {
            let failure = loop {
                match super::receive_frame::<_, MuxResponse>(&mut reader) {
                    Ok(MuxResponse { channel, response }) => {
                        let mut state = router
                            .state
                            .lock()
                            .expect("the state lock is never poisoned");
                        match state.channels.get_mut(&channel) {
                            Some(slot) if slot.outstanding > 0 => {
                                // A scan's progress report comes ahead of
                                // its answer and does not answer it: the
                                // request stays owed until the answer does.
                                // Counted as an answer, the real one that
                                // follows read as unsolicited and failed the
                                // connection — every remote scan longer
                                // than the report interval.
                                if !matches!(response, Response::ScanProgress { .. }) {
                                    slot.outstanding -= 1;
                                }
                                // A failed send means the channel handle is
                                // being dropped; its close is on the way.
                                let _ = slot.sender.send(response);
                            }
                            Some(_) => {
                                // A response nobody asked for would be
                                // consumed as the answer to the *next*
                                // request, silently desynchronizing the
                                // channel; failing the connection is the
                                // safe interpretation.
                                break format!(
                                    "protocol error: unsolicited response on channel {channel}"
                                );
                            }
                            // A response for an unknown channel is the
                            // benign race of an answer crossing a close on
                            // the wire.
                            None => {}
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
    /// A failure with [`usable`](AgentConnection::usable) still true is
    /// channel-local (the agent refused this endpoint), not a connection
    /// failure.
    pub fn open(&self, initialize: Initialize) -> Result<AgentChannel> {
        let channel = self.shared.next_channel.fetch_add(1, Ordering::Relaxed);
        // Identifiers are never reused within a connection; exhausting them
        // (four billion opens) fails the open rather than wrapping into a
        // collision.
        if channel == u32::MAX {
            bail!("the connection's channel identifiers are exhausted");
        }
        let (sender, receiver) = mpsc::channel();
        {
            let mut state = self
                .shared
                .state
                .lock()
                .expect("the state lock is never poisoned");
            if let Some(reason) = &state.dead {
                return Err(ConnectionFailed::new(reason).into());
            }
            if state.shutdown {
                bail!("the agent connection has shut down");
            }
            state.channels.insert(
                channel,
                Slot {
                    sender,
                    outstanding: 1,
                },
            );
            state.open += 1;
        }
        let opened = (|| -> Result<()> {
            self.shared
                .send(&MuxRequest::Open {
                    channel,
                    initialize,
                })
                .context("unable to send channel open")?;
            match receiver.recv_timeout(self.shared.setup_timeout) {
                Ok(Response::Initialized) => Ok(()),
                Ok(Response::Error(message)) => bail!("remote error: {message}"),
                Ok(_) => bail!("protocol error: unexpected answer to a channel open"),
                // Silence fails the whole connection, not just this open:
                // an agent that cannot open a channel cannot serve the
                // sessions already on it either, and they must reconnect.
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.shared.fail(format!(
                        "the agent did not answer a channel open within {}",
                        describe_timeout(self.shared.setup_timeout)
                    ));
                    Err(ConnectionFailed::new(&self.shared.death_reason()).into())
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    Err(ConnectionFailed::new(&self.shared.death_reason()).into())
                }
            }
        })();
        match opened {
            Ok(()) => Ok(AgentChannel {
                shared: self.shared.clone(),
                channel,
                receiver,
                closed: false,
            }),
            Err(error) => {
                let _ = self.shared.send(&MuxRequest::Close { channel });
                let _ = self.shared.release_channel(channel);
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

impl Clone for AgentConnection {
    fn clone(&self) -> AgentConnection {
        self.shared
            .state
            .lock()
            .expect("the state lock is never poisoned")
            .handles += 1;
        AgentConnection {
            shared: self.shared.clone(),
        }
    }
}

impl Drop for AgentConnection {
    fn drop(&mut self) {
        let _ = self.shared.release_handle();
    }
}

impl AgentChannel {
    /// Performs one request/response exchange on this channel. The returned
    /// response may be [`Response::Error`] (a request-level failure on the
    /// far side); a transport failure is an error here.
    pub fn exchange(&mut self, request: Request) -> Result<Response> {
        self.send_only(request)?;
        self.receive_response()
    }

    /// Sends a request without awaiting its response, which remains owed on
    /// the channel and must be drained with
    /// [`receive_response`](AgentChannel::receive_response) (in order).
    /// This is what lets bulk staging keep a window of pushes in flight
    /// instead of paying one round trip per batch.
    pub fn send_only(&mut self, request: Request) -> Result<()> {
        {
            let mut state = self
                .shared
                .state
                .lock()
                .expect("the state lock is never poisoned");
            if let Some(reason) = &state.dead {
                return Err(ConnectionFailed::new(reason).into());
            }
            let slot = state
                .channels
                .get_mut(&self.channel)
                .ok_or_else(|| anyhow!("the channel has been closed"))?;
            slot.outstanding += 1;
        }
        if let Err(error) = self.shared.send(&MuxRequest::Request {
            channel: self.channel,
            request,
        }) {
            // The request never went out; it isn't outstanding (leaving the
            // count raised would let a stray later response masquerade as
            // an answer).
            if let Ok(mut state) = self.shared.state.lock() {
                if let Some(slot) = state.channels.get_mut(&self.channel) {
                    slot.outstanding = slot.outstanding.saturating_sub(1);
                }
            }
            // A write that fails is a connection that has failed, whatever
            // the operating system called it.
            return Err(ConnectionFailed::new(&format!("{error:#}")).into());
        }
        Ok(())
    }

    /// Receives the next owed response on this channel.
    pub fn receive_response(&mut self) -> Result<Response> {
        self.receiver
            .recv()
            .map_err(|_| ConnectionFailed::new(&self.shared.death_reason()).into())
    }

    /// Closes the channel, reporting any shutdown failure (dropping does
    /// the same silently).
    pub fn close(mut self) -> Result<()> {
        self.closed = true;
        let closed = self.shared.send(&MuxRequest::Close {
            channel: self.channel,
        });
        let released = self.shared.release_channel(self.channel);
        closed.and(released)
    }
}

impl Drop for AgentChannel {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        // Close the channel (best-effort: a dead connection has nothing to
        // tell) and let the shared state decide whether the whole
        // connection should shut down.
        let _ = self.shared.send(&MuxRequest::Close {
            channel: self.channel,
        });
        let _ = self.shared.release_channel(self.channel);
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
            let already_down = state.dead.is_some();
            state.dead.get_or_insert(reason);
            // Dropping the senders is what unblocks the receivers.
            state.channels.clear();
            already_down
        };
        if !already_down {
            // Kill unconditionally: even when a graceful shutdown is in
            // flight, a router failure means the stream died under it, and
            // its bounded wait must not depend on the agent's cooperation.
            let _ = self.reap(false);
        }
    }

    /// Releases one channel, shutting the connection down when nothing
    /// keeps it alive any longer.
    fn release_channel(&self, channel: u32) -> Result<()> {
        let shut_down = {
            let mut state = self.state.lock().expect("the state lock is never poisoned");
            state.channels.remove(&channel);
            state.open = state.open.saturating_sub(1);
            Self::begin_shutdown(&mut state)
        };
        if shut_down {
            self.shutdown()
        } else {
            Ok(())
        }
    }

    /// Releases one connection handle, shutting the connection down when
    /// nothing keeps it alive any longer.
    fn release_handle(&self) -> Result<()> {
        let shut_down = {
            let mut state = self.state.lock().expect("the state lock is never poisoned");
            state.handles = state.handles.saturating_sub(1);
            Self::begin_shutdown(&mut state)
        };
        if shut_down {
            self.shutdown()
        } else {
            Ok(())
        }
    }

    /// Decides (under the state lock) whether this release triggers
    /// shutdown.
    fn begin_shutdown(state: &mut Router) -> bool {
        if state.open == 0 && state.handles == 0 && !state.shutdown && state.dead.is_none() {
            state.shutdown = true;
            true
        } else {
            false
        }
    }

    /// Performs the graceful shutdown: ask the agent to exit, close its
    /// standard input (the guarantee that it exits even if it ignores the
    /// request), and reap with a bounded wait.
    fn shutdown(&self) -> Result<()> {
        let requested = self.send(&MuxRequest::Shutdown);
        {
            // Dropping the real writer closes the agent's stdin; the agent
            // exits on end-of-stream regardless of the shutdown frame's
            // fate.
            let mut writer = self
                .writer
                .lock()
                .expect("the writer lock is never poisoned");
            *writer = Box::new(std::io::sink());
        }
        let reaped = self.reap(true);
        requested.and(reaped)
    }

    /// Reaps the agent process. A graceful reap waits (bounded) for the
    /// exit that the closed stream causes, killing on timeout; an
    /// ungraceful one kills first.
    fn reap(&self, graceful: bool) -> Result<()> {
        let Some(mut child) = self
            .child
            .lock()
            .expect("the child lock is never poisoned")
            .take()
        else {
            return Ok(());
        };
        if graceful {
            let deadline = std::time::Instant::now() + REAP_TIMEOUT;
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        if status.success() {
                            return Ok(());
                        }
                        bail!("the agent process exited with {status}");
                    }
                    Ok(None) => {
                        if std::time::Instant::now() >= deadline {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                    Err(_) => break,
                }
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        if graceful {
            bail!("the agent process had to be killed after ignoring shutdown");
        }
        Ok(())
    }
}

/// A pool of agent connections keyed by their spawn command, sharing one
/// connection per key and serializing (re)establishment per key — which is
/// also what makes first-contact agent installation happen once per host
/// rather than once per session. The pool's stored handles keep idle
/// connections alive for reuse; dropping the pool releases them.
#[derive(Default)]
pub struct AgentPool {
    /// The per-key slots.
    slots: Mutex<HashMap<Vec<String>, Arc<PoolSlot>>>,
    /// How long a session waits on another's establishment, when not
    /// [`POOL_WAIT_TIMEOUT`].
    wait_timeout: Option<std::time::Duration>,
    /// Peering: connections that dialed *in*, by the peer's name, waiting
    /// for the session that will use them. The configured alpha attaches
    /// to a beta that leads this way, since the alpha is never dialed.
    attachments: Mutex<HashMap<String, super::Connection>>,
}

impl AgentPool {
    /// Peering: offers a connection a peer opened to this supervisor. A
    /// later offer for the same name replaces an earlier one that was
    /// never taken — the peer reconnected.
    pub fn offer_attachment(&self, name: &str, connection: super::Connection) {
        self.attachments
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(name.to_owned(), connection);
    }

    /// Keeps only the slots whose key `keep` accepts. A slot removed here
    /// drops the pool's handle on its connection, which closes once the
    /// last session channel on it is gone too.
    pub fn retain(&self, keep: impl Fn(&[String]) -> bool) {
        self.slots
            .lock()
            .expect("the pool lock is never poisoned")
            .retain(|key, _| keep(key));
    }

    /// Peering: takes the connection a peer opened, if one is waiting.
    pub fn take_attachment(&self, name: &str) -> Option<super::Connection> {
        self.attachments
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(name)
    }
}

/// The agent connection has failed underneath a session: the far side
/// closed it, or the transport broke. Typed so that the supervisor can
/// tell "the host went away" from "something went wrong": a laptop
/// waking from sleep finds every connection it held in this state, and
/// the right answer is to reconnect, not to report.
#[derive(Debug, thiserror::Error)]
#[error("the agent connection has failed: {reason}")]
pub struct ConnectionFailed {
    /// What ended it, in the transport's words.
    pub reason: String,
}

impl ConnectionFailed {
    fn new(reason: &str) -> ConnectionFailed {
        ConnectionFailed {
            reason: reason.to_owned(),
        }
    }

    /// Whether an error, anywhere in its chain, is a failed connection.
    pub fn is_in(error: &anyhow::Error) -> bool {
        error
            .chain()
            .any(|cause| cause.downcast_ref::<ConnectionFailed>().is_some())
    }
}

/// One pool slot: the live connection for a key, if any, or the mark of
/// a session establishing one.
///
/// The slot's lock is held only to read or change that state, never
/// across the network waits of establishing a connection or opening a
/// channel: a session that finds the slot connecting waits on the
/// condition, with a deadline, rather than on a lock the connecting
/// session holds for as long as its login takes.
#[derive(Default)]
struct PoolSlot {
    /// The slot's state.
    state: Mutex<SlotState>,
    /// Signalled whenever the state leaves [`SlotState::Connecting`].
    changed: std::sync::Condvar,
}

/// The state of one pool slot.
#[derive(Default)]
enum SlotState {
    /// No connection, and nobody establishing one.
    #[default]
    Empty,
    /// A session is establishing the connection.
    Connecting,
    /// The connection, which may have died since it was stored.
    Ready(AgentConnection),
}

/// A session's claim on a slot it marked as connecting. Dropped without
/// being published — an establishment that failed, or panicked — it
/// empties the slot, so the next session establishes afresh.
struct SlotClaim<'a> {
    /// The claimed slot.
    slot: &'a PoolSlot,
    /// Whether a connection was published into the slot.
    published: bool,
}

impl SlotClaim<'_> {
    /// Stores the established connection and wakes the waiting sessions.
    fn publish(mut self, connection: AgentConnection) {
        self.set(SlotState::Ready(connection));
        self.published = true;
    }

    fn set(&self, state: SlotState) {
        *self
            .slot
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = state;
        self.slot.changed.notify_all();
    }
}

impl Drop for SlotClaim<'_> {
    fn drop(&mut self) {
        if !self.published {
            self.set(SlotState::Empty);
        }
    }
}

impl AgentPool {
    /// Opens a channel on the pooled connection for `key`, building a fresh
    /// connection with `establish` when none exists or the existing one has
    /// failed. A channel-local refusal (the agent rejecting this endpoint
    /// on an otherwise healthy connection) is returned as-is — it would
    /// refuse identically on a fresh connection, and rebuilding would
    /// strand the sessions using the current one.
    ///
    /// One session at a time establishes a key's connection; the others
    /// wait for it, up to a deadline, without a lock held across its
    /// network waits.
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
        let wait = self.wait_timeout.unwrap_or(POOL_WAIT_TIMEOUT);
        let deadline = std::time::Instant::now() + wait;
        let claim = loop {
            let mut state = slot.state.lock().unwrap_or_else(|error| error.into_inner());
            match &*state {
                SlotState::Ready(connection) if connection.usable() => {
                    let connection = connection.clone();
                    drop(state);
                    match connection.open(initialize.clone()) {
                        Ok(channel) => return Ok(channel),
                        // Still usable: the refusal is channel-local.
                        Err(error) if connection.usable() => return Err(error),
                        // The connection died underneath the open; the
                        // next pass rebuilds it.
                        Err(_) => continue,
                    }
                }
                SlotState::Connecting => {
                    let now = std::time::Instant::now();
                    if now >= deadline {
                        bail!(
                            "another session's connection to this host is still connecting \
                             after {}",
                            describe_timeout(wait)
                        );
                    }
                    let _ = slot
                        .changed
                        .wait_timeout(state, deadline - now)
                        .unwrap_or_else(|error| error.into_inner());
                }
                // Nothing, or a connection that has died: this session
                // establishes the replacement, outside the lock.
                SlotState::Empty | SlotState::Ready(_) => {
                    *state = SlotState::Connecting;
                    break SlotClaim {
                        slot: &slot,
                        published: false,
                    };
                }
            }
        };
        let connection = establish()?;
        let channel = connection.open(initialize)?;
        claim.publish(connection);
        Ok(channel)
    }

    /// A pool whose sessions wait `wait` for another's establishment.
    #[cfg(test)]
    pub(crate) fn with_wait(wait: std::time::Duration) -> AgentPool {
        AgentPool {
            wait_timeout: Some(wait),
            ..AgentPool::default()
        }
    }
}

/// A deadline as it reads in a death reason: "60 s", or "300 ms".
pub(crate) fn describe_timeout(timeout: std::time::Duration) -> String {
    match timeout.as_secs() {
        0 => format!("{} ms", timeout.as_millis()),
        seconds => format!("{seconds} s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::SymlinkMode;
    use crate::transport::tests::connected_pair;

    /// Builds a test initialization for the specified root, under a
    /// session identifier of the root's own: tests running in parallel
    /// never share a session's staging or scan cache.
    fn initialize(root: &std::path::Path) -> Initialize {
        let root = root.to_string_lossy().into_owned();
        Initialize {
            session: crate::session::session_identifier(&root, "mux-test"),
            root,
            ignores: Vec::new(),
            symlink_mode: SymlinkMode::Raw,
            file_mode: None,
            directory_mode: None,
            side: "beta".into(),
            staging: Default::default(),
            max_file_size: None,
            max_entry_count: None,
            ignore_mounts: true,
            default_owner: None,
            default_group: None,
        }
    }

    /// Starts a real agent over in-memory pipes, keeping its state under
    /// `state` (the test's own directory, never the real `~/.autobahn`),
    /// and returns the client end and a receiver that yields the agent's
    /// exit result.
    fn spawned_agent(state: &std::path::Path) -> (Connection, mpsc::Receiver<Result<()>>) {
        let (client, agent) = connected_pair();
        let (agent_reader, agent_writer, _) = agent.into_parts();
        let (finished_sender, finished) = mpsc::channel();
        let state = state.to_path_buf();
        std::thread::spawn(move || {
            let result = crate::transport::serve_agent_in(agent_reader, agent_writer, &state);
            let _ = finished_sender.send(result);
        });
        (client, finished)
    }

    /// Waits for an agent's exit and asserts it was clean.
    fn assert_clean_exit(finished: &mpsc::Receiver<Result<()>>) {
        finished
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the agent must exit")
            .expect("the agent must exit cleanly");
    }

    /// An agent keeps a channel's staging and scan cache in the state area
    /// it was given, under the channel's own session and side — the state
    /// area is what moves a test's agent out of the real `~/.autobahn`.
    #[test]
    fn a_channel_keeps_its_state_in_the_agents_state_area() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let root = keep.path().join("root");
        let state = keep.path().join("state");
        std::fs::create_dir_all(&root).expect("root should be creatable");
        std::fs::write(root.join("file.txt"), b"content").expect("file should be writable");
        // The cache writer does not create directories; an agent's staging
        // area already exists by the time a real session scans.
        std::fs::create_dir_all(state.join("staging")).expect("staging should be creatable");

        let (client, finished) = spawned_agent(&state);
        let connection = AgentConnection::connect(client).expect("unable to connect");
        let initialize = initialize(&root);
        let session = initialize.session.clone();
        let mut channel = connection.open(initialize).expect("open");
        channel.exchange(Request::Scan).expect("the scan exchanges");
        // The cache is written in the background while the channel's
        // endpoint lives, so it is waited for before the channel closes.
        let cache = state
            .join("staging")
            .join(format!("{session}-beta.scancache"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !cache.is_file() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(cache.is_file(), "no scan cache at {}", cache.display());
        drop(channel);
        drop(connection);
        assert_clean_exit(&finished);
    }

    /// An agent whose root holds its own state area — a remote root of
    /// `~`, say — never scans that area, so it is never synchronized.
    #[test]
    fn an_agent_never_scans_its_own_state_area() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let root = keep.path().join("home");
        let state = root.join(".autobahn");
        std::fs::create_dir_all(state.join("staging")).expect("staging should be creatable");
        std::fs::write(state.join("config.toml"), b"secret").expect("config should be writable");
        std::fs::write(root.join("file.txt"), b"content").expect("file should be writable");

        let (client, finished) = spawned_agent(&state);
        let connection = AgentConnection::connect(client).expect("unable to connect");
        let mut channel = connection.open(initialize(&root)).expect("open");
        let Response::ScanDelta(header) = channel.exchange(Request::Scan).expect("scan") else {
            panic!("expected a scan delta response");
        };
        assert!(header.baseline.is_none(), "a first scan has no baseline");
        // Against no baseline, the delta is the whole encoding as data.
        let (mut base, signature) = (std::io::Cursor::new(Vec::new()), Default::default());
        let mut encoded = Vec::new();
        loop {
            match channel.exchange(Request::ScanPull).expect("pull") {
                Response::ScanOps(ops) if ops.is_empty() => break,
                Response::ScanOps(ops) => {
                    for op in &ops {
                        crate::rsync::patch(&mut base, &signature, op, &mut encoded)
                            .expect("patch");
                    }
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        let snapshot: crate::tree::Snapshot = bincode::deserialize(&encoded).expect("decode");
        let root_node = snapshot.root.as_ref().expect("root");
        assert!(root_node.child("file.txt").is_some());
        assert!(
            matches!(
                root_node.child(".autobahn").map(|node| &node.content),
                Some(crate::tree::Content::Untracked)
            ),
            "the agent scanned its own state area"
        );
        drop(channel);
        drop(connection);
        assert_clean_exit(&finished);
    }

    /// Through the wire: a channel opened with a traversal session fails
    /// its open, leaves the agent's state area alone, and leaves the
    /// connection serving.
    #[test]
    fn a_traversal_session_fails_its_open_and_touches_nothing() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let root = keep.path().join("root");
        let state = keep.path().join("state");
        let sentinel = state.join("sentinel");
        std::fs::create_dir_all(&root).expect("root should be creatable");
        std::fs::create_dir_all(state.join("staging")).expect("staging should be creatable");
        std::fs::write(&sentinel, b"kept").expect("the sentinel should be writable");

        let (client, finished) = spawned_agent(&state);
        let connection = AgentConnection::connect(client).expect("unable to connect");
        let mut hostile = initialize(&root);
        hostile.session = "..".into();
        let error = connection
            .open(hostile)
            .err()
            .expect("the open must be refused");
        assert!(
            format!("{error:#}").contains("refusing session identifier"),
            "unexpected error: {error:#}"
        );
        assert!(sentinel.is_file(), "the state area was removed");

        // The refusal is the channel's alone.
        let mut channel = connection.open(initialize(&root)).expect("open");
        channel.exchange(Request::Scan).expect("the scan exchanges");
        drop(channel);
        drop(connection);
        assert_clean_exit(&finished);
    }

    #[test]
    fn channels_multiplex_without_blocking_each_other() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let root_a = keep.path().join("a");
        let root_b = keep.path().join("b");
        std::fs::create_dir_all(&root_a).expect("root should be creatable");
        std::fs::create_dir_all(&root_b).expect("root should be creatable");
        std::fs::write(root_a.join("file.txt"), b"content").expect("file should be writable");

        let (client, finished) = spawned_agent(&keep.path().join("state"));
        let connection = AgentConnection::connect(client).expect("unable to connect");
        let mut channel_a = connection.open(initialize(&root_a)).expect("open a");
        let mut channel_b = connection.open(initialize(&root_b)).expect("open b");

        // Channel B's root has just been created, and a watcher can report
        // that creation as change (FSEvents replays startup dust). The
        // channel is settled first — scan, then short waits until one
        // passes quietly — so the long wait below measures blocking rather
        // than the watcher's opinion of its own startup.
        channel_b
            .exchange(Request::Scan)
            .expect("the settling scan exchanges");
        for _ in 0..20 {
            let response = channel_b
                .exchange(Request::AwaitChanges {
                    milliseconds: 100,
                    since: None,
                })
                .expect("the settling wait exchanges");
            if matches!(response, Response::AwaitChanges { changed: false, .. }) {
                break;
            }
            channel_b
                .exchange(Request::Scan)
                .expect("the settling scan exchanges");
        }

        // Channel B blocks in a long change wait; channel A's scan must
        // complete while B is still waiting — the proof that channels are
        // served concurrently.
        std::thread::scope(|scope| {
            let waiter = scope.spawn(move || {
                let started = std::time::Instant::now();
                let response = channel_b
                    .exchange(Request::AwaitChanges {
                        milliseconds: 2_000,
                        since: None,
                    })
                    .expect("await should exchange");
                assert!(matches!(
                    response,
                    Response::AwaitChanges { changed: false, .. }
                ));
                (started.elapsed(), channel_b)
            });
            // Give the wait a moment to actually start.
            std::thread::sleep(std::time::Duration::from_millis(100));
            let started = std::time::Instant::now();
            let response = channel_a
                .exchange(Request::Scan)
                .expect("scan should exchange");
            let scan_elapsed = started.elapsed();
            // A scan arrives as a delta header now; the test only needs to
            // know that the exchange completed on its own channel, and the
            // header's declared length is evidence the snapshot was built.
            let Response::ScanDelta(header) = response else {
                panic!("expected a scan delta response, got {response:?}");
            };
            assert!(header.length > 0);
            // Drain the stream so the channel is clean for shutdown.
            loop {
                match channel_a.exchange(Request::ScanPull).expect("pull") {
                    Response::ScanOps(ops) if ops.is_empty() => break,
                    Response::ScanOps(_) => {}
                    other => panic!("unexpected {other:?}"),
                }
            }
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

        // With every channel and handle released, the agent shut down
        // cleanly.
        assert_clean_exit(&finished);
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

        let (client, finished) = spawned_agent(&keep.path().join("state"));

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

        assert_clean_exit(&finished);
    }

    /// A connection that dies underneath a channel answers every later
    /// request with a typed failure, so a supervisor can tell "the host
    /// went away" from "something went wrong" and reconnect rather than
    /// report.
    #[test]
    fn a_connection_that_dies_underneath_a_channel_fails_typed() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let root = keep.path().join("root");
        std::fs::create_dir_all(&root).expect("root should be creatable");
        let (client, finished) = spawned_agent(&keep.path().join("state"));
        let connection = AgentConnection::connect(client).expect("unable to connect");
        let mut channel = connection.open(initialize(&root)).expect("open");
        // The agent goes away: its end of the pipes is dropped when its
        // serving thread ends, which a shutdown frame brings about.
        connection.shared.shutdown().expect("shutdown");
        assert_clean_exit(&finished);
        let error = channel
            .exchange(Request::Scan)
            .expect_err("a request over a dead connection fails");
        assert!(
            ConnectionFailed::is_in(&error),
            "the failure is typed, not worded: {error:#}"
        );
        assert!(
            !connection.usable(),
            "and the pool would not reuse the connection"
        );
    }

    #[test]
    fn a_handleless_connection_shuts_down_without_ever_opening_a_channel() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let (client, finished) = spawned_agent(keep.path());
        let connection = AgentConnection::connect(client).expect("unable to connect");
        // No channel is ever opened; dropping the last handle must still
        // shut the agent down (and end the router) rather than leaking
        // both.
        drop(connection);
        assert_clean_exit(&finished);
    }

    #[test]
    fn a_failed_handshake_reaps_the_spawned_process() {
        // `true` exits immediately without speaking the protocol, so the
        // handshake fails; the spawned process must be reaped rather than
        // left as a zombie.
        let connection = Connection::spawn(&["true".to_owned()]).expect("the process should spawn");
        let pid = connection.child_id().expect("the child id should be known") as i32;
        let error = AgentConnection::connect(connection)
            .err()
            .expect("the handshake must fail");
        assert!(
            format!("{error:#}").contains("handshake"),
            "unexpected error: {error:#}"
        );
        // Reaped: the pid no longer refers to a process (or zombie) of ours.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }

    /// A far side that takes whatever it is sent and never says a word,
    /// kept alive (so its silence is silence, not a closed stream) until
    /// the returned sender is dropped.
    fn silent_peer(connection: Connection) -> mpsc::Sender<()> {
        let (keep, hold) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            let _connection = connection;
            let _ = hold.recv();
        });
        keep
    }

    #[test]
    fn an_agent_that_never_answers_the_handshake_fails_within_the_deadline() {
        let (client, agent) = connected_pair();
        let _keep = silent_peer(agent);
        let started = std::time::Instant::now();
        let error = AgentConnection::connect_within(client, std::time::Duration::from_millis(300))
            .err()
            .expect("the handshake must time out");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(ConnectionFailed::is_in(&error), "{error:#}");
        let message = format!("{error:#}");
        assert!(
            message.contains("did not answer the handshake"),
            "{message}"
        );
    }

    #[test]
    fn an_agent_that_never_answers_a_channel_open_fails_the_connection() {
        let (scripted, client) = connected_pair();
        let (keep, hold) = mpsc::channel::<()>();
        std::thread::spawn(move || -> Result<()> {
            let mut connection = scripted;
            let _: Handshake = connection.receive()?;
            connection.send(&crate::transport::local_handshake())?;
            // Takes the open, and says nothing.
            let _: MuxRequest = connection.receive()?;
            let _ = hold.recv();
            Ok(())
        });
        let connection =
            AgentConnection::connect_within(client, std::time::Duration::from_millis(300))
                .expect("the handshake completes");
        let started = std::time::Instant::now();
        let error = connection
            .open(initialize(std::path::Path::new("/unused")))
            .err()
            .expect("the open must time out");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(ConnectionFailed::is_in(&error), "{error:#}");
        assert!(
            format!("{error:#}").contains("did not answer a channel open"),
            "{error:#}"
        );
        // The whole connection is failed, so every session on it reconnects.
        assert!(!connection.usable());
        drop(keep);
    }

    #[test]
    fn a_session_waits_on_a_connecting_slot_with_a_deadline_not_on_a_lock() {
        let pool = Arc::new(AgentPool::with_wait(std::time::Duration::from_millis(300)));
        let key = vec!["slow-host".to_owned()];

        // The first session's establishment hangs (a login stuck in a slow
        // rc file) until released.
        let (release, hung) = mpsc::channel::<()>();
        let (entered, establishing) = mpsc::channel::<()>();
        let first = {
            let pool = pool.clone();
            let key = key.clone();
            std::thread::spawn(move || {
                pool.channel(&key, initialize(std::path::Path::new("/unused")), || {
                    let _ = entered.send(());
                    let _ = hung.recv();
                    bail!("the login never finished")
                })
                .err()
                .expect("the first session fails")
            })
        };
        establishing
            .recv()
            .expect("the first session is establishing");

        // A second session to the same host is not stuck behind it: it
        // gives up within the wait, and never establishes a second time
        // while the first is connecting.
        let started = std::time::Instant::now();
        let error = pool
            .channel(&key, initialize(std::path::Path::new("/unused")), || {
                panic!("a second establishment while the first is connecting")
            })
            .err()
            .expect("the second session gives up");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "waited {:?}",
            started.elapsed()
        );
        assert!(
            format!("{error:#}").contains("still connecting"),
            "{error:#}"
        );

        drop(release);
        let error = first.join().expect("the first session's thread");
        assert!(format!("{error:#}").contains("never finished"), "{error:#}");

        // With the slot free again, the next session establishes afresh.
        let (client, agent) = connected_pair();
        let _keep = silent_peer(agent);
        let error = pool
            .channel(&key, initialize(std::path::Path::new("/unused")), || {
                AgentConnection::connect_within(client, std::time::Duration::from_millis(200))
            })
            .err()
            .expect("a silent agent fails the handshake");
        assert!(ConnectionFailed::is_in(&error), "{error:#}");
    }

    #[test]
    fn a_scan_reports_progress_before_its_answer_on_a_healthy_channel() {
        let (scripted, agent_side) = connected_pair();
        let script = std::thread::spawn(move || -> Result<()> {
            let mut connection = scripted;
            let _: Handshake = connection.receive()?;
            connection.send(&crate::transport::local_handshake())?;
            let MuxRequest::Open { channel, .. } = connection.receive()? else {
                anyhow::bail!("expected an open");
            };
            connection.send(&MuxResponse {
                channel,
                response: Response::Initialized,
            })?;
            // Two scans, each answered after progress reports; then a push
            // answered plainly, to show the channel is still in step.
            for _ in 0..2 {
                let _: MuxRequest = connection.receive()?;
                for entries in [1_000, 2_000, 3_000] {
                    connection.send(&MuxResponse {
                        channel,
                        response: Response::ScanProgress { entries, bytes: 0 },
                    })?;
                }
                connection.send(&MuxResponse {
                    channel,
                    response: Response::ScanUnchanged { generation: 7 },
                })?;
            }
            let _: MuxRequest = connection.receive()?;
            connection.send(&MuxResponse {
                channel,
                response: Response::StagePushed,
            })?;
            let _: Result<MuxRequest> = connection.receive();
            Ok(())
        });

        let connection = AgentConnection::connect(agent_side).expect("unable to connect");
        let mut channel = connection
            .open(initialize(std::path::Path::new("/unused")))
            .expect("open");
        for _ in 0..2 {
            let mut response = channel.exchange(Request::Scan).expect("the scan exchange");
            let mut reports = 0;
            while let Response::ScanProgress { .. } = response {
                reports += 1;
                response = channel.receive_response().expect("the answer follows");
            }
            assert_eq!(reports, 3);
            assert!(matches!(
                response,
                Response::ScanUnchanged { generation: 7 }
            ));
        }
        let response = channel
            .exchange(Request::StagePush(Vec::new()))
            .expect("the channel is still in step");
        assert!(matches!(response, Response::StagePushed));
        drop(channel);
        drop(connection);
        let _ = script.join();
    }

    #[test]
    fn unsolicited_responses_fail_the_connection() {
        let (scripted, agent_side) = connected_pair();
        let script = std::thread::spawn(move || -> Result<()> {
            let mut connection = scripted;
            let _: Handshake = connection.receive()?;
            connection.send(&crate::transport::local_handshake())?;
            let MuxRequest::Open { channel, .. } = connection.receive()? else {
                anyhow::bail!("expected an open");
            };
            connection.send(&MuxResponse {
                channel,
                response: Response::Initialized,
            })?;
            // One request arrives; answer it twice. The duplicate must fail
            // the connection rather than poisoning the channel's next
            // exchange.
            let _: MuxRequest = connection.receive()?;
            connection.send(&MuxResponse {
                channel,
                response: Response::StagePushed,
            })?;
            connection.send(&MuxResponse {
                channel,
                response: Response::StagePushed,
            })?;
            // Hold the connection open until the client observes the
            // failure.
            let _: Result<MuxRequest> = connection.receive();
            Ok(())
        });

        let connection = AgentConnection::connect(agent_side).expect("unable to connect");
        let mut channel = connection
            .open(initialize(std::path::Path::new("/unused")))
            .expect("open");
        let response = channel
            .exchange(Request::StagePush(Vec::new()))
            .expect("the first exchange succeeds");
        assert!(matches!(response, Response::StagePushed));

        // The duplicate arrives asynchronously; the next exchange must
        // surface the protocol failure.
        let mut failed = false;
        for _ in 0..100 {
            match channel.exchange(Request::StagePush(Vec::new())) {
                Ok(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
                Err(error) => {
                    // The requirement is that the protocol violation fails
                    // the *connection*, not that this exchange is the one
                    // to name it: the reader thread may tear the
                    // connection down before this call reaches it, in
                    // which case the caller legitimately sees the closure
                    // instead of the diagnosis.
                    let message = format!("{error:#}");
                    assert!(
                        message.contains("unsolicited") || message.contains("connection"),
                        "unexpected error: {message}"
                    );
                    failed = true;
                    break;
                }
            }
        }
        assert!(failed, "the duplicate response was never detected");
        drop(channel);
        drop(connection);
        let _ = script.join();
    }

    #[test]
    fn the_pool_shares_one_connection_and_keeps_channel_refusals_local() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let root = keep.path().join("root");
        std::fs::create_dir_all(&root).expect("root should be creatable");

        let (client, finished) = spawned_agent(&keep.path().join("state"));

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

        // An endpoint the agent refuses (an invalid ignore pattern) is a
        // channel-local error: the connection stays shared and no rebuild
        // is attempted.
        let mut refused = initialize(&root);
        refused.ignores = vec!["[unclosed".to_owned()];
        let error = pool
            .channel(&key, refused, &mut establish)
            .err()
            .expect("the endpoint must be refused");
        assert!(
            format!("{error:#}").contains("remote error"),
            "unexpected error: {error:#}"
        );
        assert_eq!(connections_built, 1);

        drop(channel_one);
        drop(channel_two);
        drop(pool);
        assert_clean_exit(&finished);
    }

    /// The agent reads and writes single files on request — atomically
    /// for readers, and never outside its root.
    #[test]
    fn an_agent_reads_and_moves_single_entries_within_its_root() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let root = keep.path().join("root");
        std::fs::create_dir_all(root.join("sub")).expect("root should be creatable");
        std::fs::write(root.join("sub/a.txt"), b"before").expect("writes");
        let outside = keep.path().join("outside.txt");
        std::fs::write(&outside, b"secret").expect("writes");

        let (client, finished) = spawned_agent(&keep.path().join("state"));
        let connection = AgentConnection::connect(client).expect("unable to connect");
        let mut channel = connection.open(initialize(&root)).expect("open");

        match channel
            .exchange(Request::ReadFile("sub/a.txt".into()))
            .expect("read")
        {
            Response::File(Some(bytes)) => assert_eq!(bytes, b"before"),
            other => panic!("unexpected {other:?}"),
        }
        match channel
            .exchange(Request::ReadFile("missing.txt".into()))
            .expect("read")
        {
            Response::File(None) => {}
            other => panic!("unexpected {other:?}"),
        }
        match channel
            .exchange(Request::Rename("sub/a.txt".into(), "sub/b.txt".into()))
            .expect("rename")
        {
            Response::Written => {}
            other => panic!("unexpected {other:?}"),
        }
        assert!(!root.join("sub/a.txt").exists());
        assert_eq!(std::fs::read(root.join("sub/b.txt")).unwrap(), b"before");
        // A name that is already taken is refused, not overwritten: the
        // caller is preserving something, so a wrong guess must not
        // destroy the occupant.
        std::fs::write(root.join("sub/c.txt"), b"occupied").expect("writes");
        match channel
            .exchange(Request::Rename("sub/b.txt".into(), "sub/c.txt".into()))
            .expect("exchange")
        {
            Response::Error(message) => assert!(message.contains("already exists"), "{message}"),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(std::fs::read(root.join("sub/c.txt")).unwrap(), b"occupied");

        // Escaping the root is refused, both ways.
        for path in ["../outside.txt", "/etc/passwd"] {
            match channel
                .exchange(Request::ReadFile(path.into()))
                .expect("exchange")
            {
                Response::Error(message) => assert!(message.contains("root-relative"), "{message}"),
                other => panic!("{path}: unexpected {other:?}"),
            }
        }
        match channel
            .exchange(Request::Rename("sub/b.txt".into(), "../outside.txt".into()))
            .expect("exchange")
        {
            Response::Error(_) => {}
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(std::fs::read(&outside).unwrap(), b"secret");

        // So is reaching out through a symbolic link inside the root.
        std::os::unix::fs::symlink(keep.path(), root.join("link")).expect("link");
        match channel
            .exchange(Request::ReadFile("link/outside.txt".into()))
            .expect("exchange")
        {
            Response::Error(message) => assert!(message.contains("not a directory"), "{message}"),
            other => panic!("unexpected {other:?}"),
        }

        drop(channel);
        drop(connection);
        assert_clean_exit(&finished);
    }
}
