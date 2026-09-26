//! Byte-stream transports and framing.
//!
//! Frames are the unit of exchange for the agent protocol: a 32-bit
//! little-endian length prefix followed by that many bytes of payload, a
//! flag byte and a bincode-encoded body, LZ4-compressed when that helps. A
//! message too large for one frame is split across several and reassembled
//! on receipt, up to `MAXIMUM_MESSAGE_SIZE`. The receiver checks each
//! prefix against [`protocol::MAXIMUM_FRAME_SIZE`] before allocating, so a
//! corrupt or adversarial length can never induce a large allocation, and
//! the sender's chunks stay under it by construction. Every frame is
//! flushed as soon as it is written (the protocol is strictly
//! request/response, so a buffered frame would deadlock both sides).
//!
//! A [`Connection`] carries those frames over a byte stream — normally the
//! stdio of a child process (`ssh host autobahn agent`) — while
//! [`serve_agent`] implements the other end of that stream, dispatching
//! decoded requests to a [`LocalEndpoint`].
//!
//! [`LocalEndpoint`]: crate::endpoint::local::LocalEndpoint

pub mod install;
pub mod mux;

/// Sends one frame over an arbitrary writer (used by the control socket,
/// which shares the agent protocol's framing).
pub(crate) fn send_control_frame<W: Write, T: Serialize>(
    writer: &mut W,
    message: &T,
) -> anyhow::Result<()> {
    send_frame(writer, message)
}

/// Receives one frame from an arbitrary reader (the control-socket
/// counterpart of [`send_control_frame`]).
pub(crate) fn receive_control_frame<R: Read, T: serde::de::DeserializeOwned>(
    reader: &mut R,
) -> anyhow::Result<T> {
    receive_frame(reader)
}

use std::cell::RefCell;
use std::io::{ErrorKind, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::endpoint::local::{EndpointOptions, LocalEndpoint};
use crate::endpoint::Endpoint;
use crate::protocol::{self, Handshake, Initialize, Request, Response};
use crate::scan::IgnoreSet;
use crate::tree::{path_join, Change, Node, Snapshot};

/// The remote command used by [`Connection::ssh_argv`] when no override is
/// provided.
pub const DEFAULT_REMOTE_COMMAND: &str = "autobahn agent";

/// Returns the SSH executable to use: the `AUTOBAHN_SSH` environment
/// variable when set (a testing and customization hook, in the spirit of
/// Mutagen's `MUTAGEN_SSH_PATH`), and plain `ssh` from the search path
/// otherwise.
pub(crate) fn ssh_binary() -> String {
    std::env::var("AUTOBAHN_SSH").unwrap_or_else(|_| "ssh".to_owned())
}

/// The options applied to every SSH invocation. `BatchMode` disables
/// interactive prompting (prompts would compete with the protocol for
/// stdio), and the keepalives bound how long a dead network can hang a
/// synchronous cycle. SSH-level compression is deliberately *disabled*:
/// the protocol stream is already LZ4-compressed, and recompressing it
/// with zlib costs seconds of CPU on both ends of a large transfer for
/// almost no wire savings.
///
/// The rest hold whatever the user's `ssh_config` says, since options on
/// the command line win over it, and these connections stay open for days:
/// no terminal on a binary protocol stream (`-T`), no agent or X11
/// forwarded to the remote host for the supervisor's lifetime, no
/// configured port forwards to break a reconnect when their port is taken,
/// no local command, and a bound on connecting (the handshake and
/// installation steps have their own). Host-key checking stays the user's:
/// `BatchMode` already refuses an unknown host.
pub(crate) fn ssh_options() -> Vec<&'static str> {
    vec![
        "-T",
        "-o",
        "ForwardAgent=no",
        "-o",
        "ForwardX11=no",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "PermitLocalCommand=no",
        "-o",
        "ConnectTimeout=20",
        "-o",
        "BatchMode=yes",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=4",
        "-o",
        "Compression=no",
    ]
}

/// A spawned agent's standard error, held until the connection proves
/// itself and relayed line by line after that.
///
/// The speculative first connection to a host fails routinely — that is
/// how a missing agent is discovered — and its stderr is the remote
/// shell's "no such file", which must not print. But the connection that
/// *succeeds* is the one whose agent then runs for days, and everything it
/// says about itself went to the same discarded stream: a watch that could
/// not be established was retried every 30 seconds for a week with no
/// trace anywhere. So the lines are held until the handshake, then either
/// printed (with the host in front, since several agents share one log)
/// or attached to the failure, which is the ssh diagnosis the error was
/// missing.
pub struct StderrRelay {
    state: Arc<Mutex<RelayState>>,
}

struct RelayState {
    label: String,
    released: bool,
    held: Vec<String>,
}

/// How many lines are kept back before the handshake. A failure's
/// diagnosis is in the first few; a runaway is not worth the memory.
const HELD_STDERR_LINES: usize = 64;

/// The most of one line relayed as one piece. A longer line arrives as
/// several, so a stream without newlines costs no more than this.
const RELAY_LINE_BYTES: usize = 4096;

/// Reads a far side's standard error to its end, handing each line to
/// `each` as text safe to print: decoded lossily (one bad byte no longer
/// ends the relay and loses everything after it), with control characters
/// escaped, so nothing it says can drive the terminal or split a log
/// line, and in pieces of at most [`RELAY_LINE_BYTES`], each but the last
/// of a long line marked with `…`.
fn relay_lines(stderr: impl Read, mut each: impl FnMut(String)) {
    use std::io::BufRead;
    let mut reader = std::io::BufReader::new(stderr);
    let mut line = Vec::new();
    let mut emit = |line: &mut Vec<u8>, cut: bool| {
        if line.last() == Some(&b'\r') && !cut {
            line.pop();
        }
        let text = String::from_utf8_lossy(line);
        let text = crate::text::display_safe(&text);
        each(match cut {
            true => format!("{text}…"),
            false => text.into_owned(),
        });
        line.clear();
    };
    loop {
        let buffer = match reader.fill_buf() {
            Ok([]) => break,
            Ok(buffer) => buffer,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let room = RELAY_LINE_BYTES - line.len();
        if room == 0 {
            // A full piece: the line ends here, or goes on in another.
            let ends = buffer[0] == b'\n';
            if ends {
                reader.consume(1);
            }
            emit(&mut line, !ends);
            continue;
        }
        let window = &buffer[..buffer.len().min(room)];
        match window.iter().position(|&byte| byte == b'\n') {
            Some(end) => {
                line.extend_from_slice(&window[..end]);
                reader.consume(end + 1);
                emit(&mut line, false);
            }
            None => {
                let taken = window.len();
                line.extend_from_slice(window);
                reader.consume(taken);
            }
        }
    }
    if !line.is_empty() {
        emit(&mut line, false);
    }
}

impl StderrRelay {
    fn start(label: String, stderr: std::process::ChildStderr) -> StderrRelay {
        let state = Arc::new(Mutex::new(RelayState {
            label,
            released: false,
            held: Vec::new(),
        }));
        let relay = Arc::clone(&state);
        // Best effort: if the thread cannot start, the pipe fills and the
        // agent's writes to stderr block, which is a stall rather than a
        // fault — and starting a thread does not fail on a working host.
        let _ = std::thread::Builder::new()
            .name("autobahn-agent-stderr".into())
            .spawn(move || {
                relay_lines(stderr, |line| {
                    let mut state = relay.lock().unwrap_or_else(|e| e.into_inner());
                    if state.released {
                        eprintln!("[{}] {line}", state.label);
                    } else if state.held.len() < HELD_STDERR_LINES {
                        state.held.push(line);
                    }
                });
            });
        StderrRelay { state }
    }

    /// Prints what was held and everything after it, prefixed by the host.
    pub fn release(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.released = true;
        let label = state.label.clone();
        for line in state.held.drain(..) {
            eprintln!("[{label}] {line}");
        }
    }

    /// What the agent said before the connection was given up on.
    pub fn held(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .held
            .clone()
    }
}

/// A bidirectional byte-stream connection to an agent (typically a child
/// process's stdio: `ssh host autobahn agent` for remote roots, or a direct
/// `autobahn agent` child for testing — the identical code path minus SSH).
pub struct Connection {
    /// The stream carrying frames from the agent.
    reader: Box<dyn Read + Send>,
    /// The stream carrying frames to the agent.
    writer: Box<dyn Write + Send>,
    /// The agent process, if this connection owns one.
    child: Option<Child>,
    /// The child's standard error, when it is being relayed rather than
    /// inherited or discarded.
    stderr: Option<StderrRelay>,
}

impl Connection {
    /// Spawns a command (argv form) and connects to its stdio.
    ///
    /// The child's standard input and output become the frame streams, while
    /// its standard error is inherited so that failures on the far side (SSH
    /// authentication problems, a missing agent binary, panics) surface
    /// directly in the user's terminal instead of being swallowed. The child
    /// is retained so that [`close`](Connection::close) can reap it.
    pub fn spawn(argv: &[String]) -> Result<Connection> {
        Connection::spawn_inner(argv, Stdio::inherit())
    }

    /// Spawns as [`spawn`](Connection::spawn) does, but holds the child's
    /// standard error back until [`release_stderr`](Connection::release_stderr)
    /// — see [`StderrRelay`].
    ///
    /// For the *speculative* first connection to a host, whose failure is
    /// the ordinary way a missing agent is discovered: the remote shell's
    /// "no such file or directory" is expected, is followed by an install
    /// and a retry, and printing it makes routine bootstrapping look like a
    /// fault. Held rather than discarded, so that the connection that
    /// succeeds keeps its agent's voice, and one that fails for a real
    /// reason carries ssh's own words in its error.
    pub fn spawn_relayed(argv: &[String], label: &str) -> Result<Connection> {
        let mut connection = Connection::spawn_inner(argv, Stdio::piped())?;
        if let Some(stderr) = connection
            .child
            .as_mut()
            .and_then(|child| child.stderr.take())
        {
            connection.stderr = Some(StderrRelay::start(label.to_owned(), stderr));
        }
        Ok(connection)
    }

    /// Lets a held standard error through. A no-op for a connection whose
    /// stderr was inherited.
    pub fn release_stderr(&self) {
        if let Some(relay) = &self.stderr {
            relay.release();
        }
    }

    /// Takes the relay, for a caller that will consume the connection but
    /// wants to decide about its stderr afterwards.
    pub(crate) fn take_stderr_relay(&mut self) -> Option<StderrRelay> {
        self.stderr.take()
    }

    fn spawn_inner(argv: &[String], stderr: Stdio) -> Result<Connection> {
        let (command, arguments) = argv
            .split_first()
            .ok_or_else(|| anyhow!("unable to spawn agent: empty command"))?;
        let mut child = Command::new(command)
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .with_context(|| format!("unable to start {command}"))?;
        let writer = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("agent standard input unavailable"))?;
        let reader = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("agent standard output unavailable"))?;
        Ok(Connection {
            reader: Box::new(reader),
            writer: Box::new(writer),
            child: Some(child),
            stderr: None,
        })
    }

    /// Creates a connection over arbitrary streams, without an associated
    /// child process. This wraps transports that aren't a spawned subprocess
    /// (an already-established socket, or in-memory pipes in tests);
    /// [`close`](Connection::close) simply drops the streams.
    pub fn from_streams(reader: Box<dyn Read + Send>, writer: Box<dyn Write + Send>) -> Connection {
        Connection {
            reader,
            writer,
            child: None,
            stderr: None,
        }
    }

    /// Builds the argv for an SSH connection to `host` running the remote
    /// agent (`remote_command`, defaulting to `autobahn agent`).
    ///
    /// The agent is the same binary as the CLI. This only builds the
    /// command and installs nothing: with the default command the agent must
    /// already be resolvable on the remote login `PATH`. The connections
    /// autobahn makes for its sessions pass
    /// [`install::versioned_remote_command`] instead, and when that agent is
    /// missing they install it over SSH ([`install::ensure_agent`]) and
    /// retry. Its version must match the local version exactly; the
    /// handshake performed by [`RemoteEndpoint::connect`] enforces that and
    /// reports both versions on mismatch.
    ///
    /// `BatchMode=yes` disables interactive prompting: password and
    /// passphrase prompts would otherwise compete with the protocol for the
    /// child's stdio, so key-based (or agent-based) authentication is
    /// required. The keepalive options bound how long a dead network can
    /// hang a session mid-cycle: without them, a vanished peer could block a
    /// protocol read indefinitely (the synchronous workers have no other way
    /// to interrupt an in-flight cycle).
    ///
    /// [`RemoteEndpoint::connect`]: crate::endpoint::remote::RemoteEndpoint::connect
    pub fn ssh_argv(host: &str, remote_command: Option<&str>) -> Vec<String> {
        let mut argv = vec![ssh_binary()];
        argv.extend(ssh_options().into_iter().map(str::to_owned));
        // The option terminator keeps a hostile host specification (one
        // beginning with `-`) from being parsed as an SSH option such as
        // `ProxyCommand`, which would mean local command execution.
        argv.push("--".to_owned());
        argv.push(host.to_owned());
        argv.push(remote_command.unwrap_or(DEFAULT_REMOTE_COMMAND).to_owned());
        argv
    }

    /// Sends one length-prefixed, bincode-encoded frame.
    pub fn send<T: Serialize>(&mut self, message: &T) -> Result<()> {
        send_frame(&mut self.writer, message)
    }

    /// Receives one length-prefixed, bincode-encoded frame. A closed stream
    /// (at a frame boundary or partway through a frame) is reported as an
    /// error, not as a value.
    pub fn receive<T: DeserializeOwned>(&mut self) -> Result<T> {
        receive_frame(&mut self.reader)
    }

    /// Terminates the connection (and reaps the child process, if any).
    ///
    /// The streams are dropped first: closing the agent's standard input is
    /// what makes it observe end-of-stream and exit, and closing its standard
    /// output ensures it can't block writing to a pipe nobody is draining.
    /// The wait that follows is therefore expected to return promptly and is
    /// performed without a timeout.
    pub fn close(mut self) -> Result<()> {
        drop(std::mem::replace(
            &mut self.writer,
            Box::new(std::io::sink()),
        ));
        drop(std::mem::replace(
            &mut self.reader,
            Box::new(std::io::empty()),
        ));
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        let status = match child.wait() {
            Ok(status) => status,
            Err(error) => {
                // The process is unreapable in its current state, so avoid
                // leaving it behind holding the (now-closed) pipes.
                let _ = child.kill();
                return Err(error).context("unable to wait for the agent process");
            }
        };
        if !status.success() {
            bail!("the agent process exited with {status}");
        }
        Ok(())
    }

    /// Returns the process identifier of the owned child, if any.
    #[cfg(test)]
    pub(crate) fn child_id(&self) -> Option<u32> {
        self.child.as_ref().map(std::process::Child::id)
    }

    /// Decomposes the connection into its streams and child, transferring
    /// cleanup responsibility to the caller (the drop-time reaping is
    /// disarmed).
    pub(crate) fn into_parts(
        mut self,
    ) -> (Box<dyn Read + Send>, Box<dyn Write + Send>, Option<Child>) {
        let reader = std::mem::replace(&mut self.reader, Box::new(std::io::empty()));
        let writer = std::mem::replace(&mut self.writer, Box::new(std::io::sink()));
        let child = self.child.take();
        (reader, writer, child)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Last-resort cleanup for connections dropped without a graceful
        // [`close`](Connection::close) — most notably when a handshake or
        // initialization fails before a `RemoteEndpoint` (whose own drop
        // closes the connection) ever exists. Without this, every failed
        // connection attempt would leave a zombie (or a live orphan holding
        // dead pipes), and a watch-mode retry loop would accumulate them
        // indefinitely. The graceful path has already taken the child, so
        // this kills only processes nothing else is responsible for.
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Runs the agent side of the protocol over the provided streams until the
/// controller disconnects or requests shutdown: handshake, initialization,
/// then a request/response loop dispatching to a local endpoint.
///
/// Errors raised by the endpoint while servicing a request are answered with
/// [`Response::Error`] and the loop continues — request-level failures (an
/// unreadable file, a refused transition) are part of normal operation and
/// must not tear down the session. Only handshake, initialization, and
/// transport failures terminate the agent, and a clean end-of-stream (the
/// controller going away without a shutdown frame) is a successful
/// exit.
pub fn serve_agent<R: Read, W: Write + Send>(input: R, output: W) -> Result<()> {
    serve_agent_with(input, output, crate::paths::default_state_root())
}

/// [`serve_agent`] over an explicit state area, so that a test's agent
/// keeps its staging, scan caches and peering files in the test's own
/// directory rather than in the real `~/.autobahn`.
#[cfg(test)]
pub(crate) fn serve_agent_in<R: Read, W: Write + Send>(
    input: R,
    output: W,
    state_root: &std::path::Path,
) -> Result<()> {
    serve_agent_with(input, output, Ok(state_root.to_path_buf()))
}

/// The agent's side of the protocol, keeping its state under `state_root`.
/// A state root that cannot be determined fails each channel's open, not
/// the connection, as it did when it was read at each open.
fn serve_agent_with<R: Read, W: Write + Send>(
    input: R,
    output: W,
    state_root: Result<PathBuf>,
) -> Result<()> {
    if let Ok(root) = &state_root {
        crate::scan::exclude_state_root(root);
    }
    let state_root = &state_root;
    let mut input = input;
    let output = std::sync::Mutex::new(output);

    // Exchange handshakes. Ours goes out first so that a version mismatch is
    // diagnosable from either side.
    {
        let mut output = output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        send_frame(&mut *output, &local_handshake()).context("unable to send handshake")?;
    }
    let peer: Handshake = receive_frame(&mut input).context("unable to receive handshake")?;
    verify_handshake(&peer)?;

    // One connection carries any number of session channels, each served by
    // its own thread over its own endpoint — a channel blocked in a change
    // wait (or a slow transfer) never stalls its siblings. The dispatch
    // below is the only reader; responses interleave through the shared
    // writer, one whole frame at a time.
    //
    // Each channel's work counter is reported beside its responses, so the
    // controller can tell a request that is being worked on from one that
    // never will be answered.
    let counters: std::sync::Mutex<ChannelCounters> = Default::default();
    std::thread::scope(|scope| -> Result<()> {
        let mut channels: std::collections::HashMap<u32, std::sync::mpsc::Sender<Request>> =
            std::collections::HashMap::new();
        let (stop_reporting, stopped) = std::sync::mpsc::channel::<()>();
        {
            let (output, counters) = (&output, &counters);
            scope.spawn(move || report_progress(output, counters, stopped));
        }
        let result = (|| -> Result<()> {
            loop {
                let frame: protocol::MuxRequest = match read_frame(&mut input)? {
                    Some(frame) => {
                        bincode::deserialize(&frame).context("unable to decode frame")?
                    }
                    // A clean end-of-stream is the controller going away, which
                    // ends every channel (the scope joins their threads once
                    // their senders drop below).
                    None => return Ok(()),
                };
                match frame {
                    protocol::MuxRequest::Open {
                        channel,
                        initialize,
                    } => {
                        if channels.contains_key(&channel) {
                            serve_send(
                                &output,
                                channel,
                                Response::Error(
                                    "protocol error: the channel is already open".into(),
                                ),
                            )?;
                            continue;
                        }
                        // Endpoint creation happens on the channel's own
                        // thread (it touches the filesystem, and the
                        // dispatcher must never block on one channel's
                        // behalf); the thread answers the open itself, with
                        // Initialized or with the creation failure.
                        let (sender, receiver) = std::sync::mpsc::channel::<Request>();
                        channels.insert(channel, sender);
                        let counted = std::sync::Arc::new(crate::progress::SideProgress::default());
                        counters
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .insert(channel, counted.clone());
                        let output = &output;
                        crate::threads::spawn_deep_scoped(scope, move || {
                            serve_channel(
                                channel, initialize, state_root, receiver, output, counted,
                            )
                        });
                    }
                    protocol::MuxRequest::Request { channel, request } => {
                        match channels.get(&channel) {
                            // A send failure means the channel thread died; the
                            // stale entry drops so the error isn't repeated.
                            Some(sender) => {
                                if sender.send(request).is_err() {
                                    channels.remove(&channel);
                                    serve_send(
                                        &output,
                                        channel,
                                        Response::Error(
                                            "protocol error: the channel has failed".into(),
                                        ),
                                    )?;
                                }
                            }
                            None => {
                                serve_send(
                                    &output,
                                    channel,
                                    Response::Error(
                                        "protocol error: the channel is not open".into(),
                                    ),
                                )?;
                            }
                        }
                    }
                    protocol::MuxRequest::Close { channel } => {
                        // Dropping the sender ends the channel thread after any
                        // in-flight request completes.
                        channels.remove(&channel);
                        counters
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .remove(&channel);
                    }
                    protocol::MuxRequest::Shutdown => return Ok(()),
                }
            }
        })();
        // Dropping every sender is what lets the channel threads finish;
        // without this, the scope's implicit join would deadlock against
        // threads blocked on their (still-live) request queues whenever the
        // controller disappears abruptly.
        channels.clear();
        drop(stop_reporting);
        result
    })
}

/// Each open channel's work counter, by channel: what its scans, hashing,
/// transfers and transitions advance as they go.
type ChannelCounters =
    std::collections::HashMap<u32, std::sync::Arc<crate::progress::SideProgress>>;

/// How often an agent reports the channels whose work moved. Well inside
/// the controller's silence limit, so a working channel is never taken for
/// a stuck one; short in tests, so their real agents exercise the reports.
const PROGRESS_INTERVAL: std::time::Duration = if cfg!(test) {
    std::time::Duration::from_millis(100)
} else {
    std::time::Duration::from_secs(5)
};

/// Reports, every `PROGRESS_INTERVAL` until `stop` is dropped, each
/// channel whose work counter moved since the last report. A channel whose
/// work has stopped — wedged on a filesystem, or its thread gone — is
/// never reported, however healthy the rest of the agent is; that silence
/// is what the controller detects.
fn report_progress<W: Write>(
    output: &std::sync::Mutex<W>,
    counters: &std::sync::Mutex<ChannelCounters>,
    stop: std::sync::mpsc::Receiver<()>,
) {
    let mut reported = std::collections::HashMap::new();
    while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = stop.recv_timeout(PROGRESS_INTERVAL)
    {
        for (channel, counter) in moved_counters(counters, &mut reported) {
            let mut output = output
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let frame = protocol::MuxResponse::Progress { channel, counter };
            if send_frame(&mut *output, &frame).is_err() {
                return;
            }
        }
    }
}

/// The channels whose counter moved since `reported` last recorded it,
/// with their counters, recording the new values. A channel starts from
/// zero, so one that has done nothing yet is not reported.
fn moved_counters(
    counters: &std::sync::Mutex<ChannelCounters>,
    reported: &mut std::collections::HashMap<u32, u64>,
) -> Vec<(u32, u64)> {
    let counters = counters.lock().unwrap_or_else(|error| error.into_inner());
    reported.retain(|channel, _| counters.contains_key(channel));
    let mut moved: Vec<(u32, u64)> = counters
        .iter()
        .filter_map(|(&channel, progress)| {
            let counter = progress.activity();
            let before = reported.insert(channel, counter).unwrap_or(0);
            (before != counter).then_some((channel, counter))
        })
        .collect();
    moved.sort_unstable();
    moved
}

/// Sessions whose channel threads panic on their first request: the test
/// hook for a channel that dies without answering.
#[cfg(test)]
pub(crate) static PANICKING_SESSIONS: std::sync::Mutex<Vec<String>> =
    std::sync::Mutex::new(Vec::new());

/// How often a running scan reports its count, and how long it runs
/// before the first report: a scan that finishes sooner — every routine
/// cycle's — sends nothing extra.
const SCAN_REPORT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Runs a scan, reporting its count on the channel every
/// `SCAN_REPORT_INTERVAL` while it runs. The reporter is joined before
/// this returns, so no report can follow the scan's own answer, and a
/// report that fails to send is dropped: the answer is what matters.
fn reporting_scan<W: Write + Send, T>(
    output: &std::sync::Mutex<W>,
    channel: u32,
    counted: &crate::progress::SideProgress,
    scan: impl FnOnce() -> T,
) -> T {
    // The reporter waits on a channel nothing is ever sent on: a timeout is
    // its cue to report, and the sender's drop at the end of the scan wakes
    // it at once. A sleep in its place would hold every scan's answer until
    // the sleep ran out — measured as twenty milliseconds on the p99 of an
    // edit over ssh.
    let (finished, waiting) = std::sync::mpsc::channel::<()>();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                waiting.recv_timeout(SCAN_REPORT_INTERVAL)
            {
                let (entries, bytes) = counted.counts();
                let _ = serve_send(output, channel, Response::ScanProgress { entries, bytes });
            }
        });
        let result = scan();
        drop(finished);
        result
    })
}

/// Serves one channel: the endpoint is created here (answering the open),
/// then requests are served in order, each answered on the shared writer.
/// The thread ends when the dispatcher drops the channel's sender.
fn serve_channel<W: Write + Send>(
    channel: u32,
    initialize: Initialize,
    state_root: &Result<PathBuf>,
    requests: std::sync::mpsc::Receiver<Request>,
    output: &std::sync::Mutex<W>,
    counted: std::sync::Arc<crate::progress::SideProgress>,
) {
    // Endpoint creation failures answer on the channel (the controller
    // would otherwise see only silence) without affecting the connection's
    // other channels.
    // `counted` is what a scan has counted so far, for the reports a long
    // one sends, and the channel's work counter besides.
    let created = crate::root::check_agent(crate::root::Identity::current(), &initialize)
        .and_then(|()| create_endpoint(&initialize, state_root));
    let mut endpoint = match created {
        Ok(mut endpoint) => {
            if serve_send(output, channel, Response::Initialized).is_err() {
                return;
            }
            endpoint.set_scan_progress(counted.clone());
            endpoint
        }
        Err(error) => {
            // The same fallback as below: even a failed *error* send must
            // not leave the controller's open waiting forever while the
            // transport is healthy.
            let response = Response::Error(format!("{error:#}"));
            if let Err(send_error) = serve_send(output, channel, response) {
                let fallback =
                    Response::Error(format!("unable to send the response: {send_error:#}"));
                let _ = serve_send(output, channel, fallback);
            }
            return;
        }
    };
    // The snapshot this channel last transmitted. It is compared by
    // storage identity, not equality: an unchanged rescan adopts its
    // baseline's children, so the comparison is a pointer check. It must
    // track what was *sent* rather than the endpoint's latest snapshot,
    // because transitions fold their achieved results into the latter —
    // leaving the endpoint holding a tree the controller has never seen.
    let mut last_sent: Option<Snapshot> = None;
    // The encoding of `last_sent`, and its digest, when a scan produced it:
    // the next delta's baseline, kept rather than re-encoded. It changes
    // only with `last_sent` — a transition's fold or a failed send clears
    // it — so it always describes exactly that snapshot.
    let mut last_sent_encoding: Option<Encoding> = None;
    // Tree digests for changed scans, remembering what it hashed so a scan
    // costs the size of its change.
    let mut digester = crate::tree::TreeDigester::default();
    // The operations of a snapshot delta in flight, drained by ScanPull.
    let mut pending: std::collections::VecDeque<crate::rsync::Op> = Default::default();
    // Peering. The fence is the lease this channel was refused against:
    // while it is set, nothing this channel asks for may change the host.
    // It is per channel, not per connection, because each channel is one
    // controller's session and presents its own term. The ancestor copy
    // is opened on first use — most channels never see a peering request.
    let peering_directory = state_root
        .as_ref()
        .map(|root| root.join(crate::peering::DIRECTORY))
        .map_err(|error| anyhow!("{error:#}"));
    let mut fence: Option<crate::peering::Lease> = None;
    let mut copy: Option<crate::peering::AncestorCopy> = None;
    while let Ok(request) = requests.recv() {
        #[cfg(test)]
        if PANICKING_SESSIONS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains(&initialize.session)
        {
            panic!("the test hook panics this channel");
        }
        // What becomes of the record of what this channel has transmitted,
        // *if* this response reaches the controller. It is applied only
        // after a successful send: a response that fails to encode or
        // transmit (an oversized frame, say) leaves the controller with its
        // previous model, and recording the new one here would make the
        // next rescan report "unchanged" against a tree it never received.
        let mut anchor = Anchor::Keep;
        // The encoding of the snapshot an `Anchor::To` names, when a scan
        // just produced it.
        let mut anchor_encoding: Option<Encoding> = None;
        // The fence refuses every write. Reads still answer, so a fenced
        // controller can see the tree it is no longer allowed to change,
        // and its scans keep the session's model honest for when it is
        // allowed again.
        let writes = matches!(
            request,
            Request::Transition(_)
                | Request::StagePush(_)
                | Request::Rename(..)
                | Request::AncestorRecord { .. }
                | Request::AncestorCheckpoint { .. }
                | Request::PutPeeringFile { .. }
        );
        if let (true, Some(held)) = (writes, &fence) {
            let response = Response::Error(format!(
                "fenced: this host's lease is held by {} at term {}; a controller at a lower \
                 term may not write here",
                held.leader, held.term
            ));
            if serve_send(output, channel, response).is_err() {
                return;
            }
            continue;
        }
        let result = match request {
            Request::Lease(lease) => peering_directory
                .as_ref()
                .map_err(|e| anyhow!("{e:#}"))
                .and_then(|directory| {
                    let held = crate::peering::read_lease(directory)?;
                    match held {
                        Some(held) if !held.admits(&lease) => {
                            fence = Some(held.clone());
                            Ok(Response::Lease(crate::peering::LeaseAnswer::Refused {
                                current: held,
                            }))
                        }
                        _ => {
                            crate::peering::write_lease(directory, &lease)?;
                            fence = None;
                            Ok(Response::Lease(crate::peering::LeaseAnswer::Accepted))
                        }
                    }
                }),
            Request::AncestorRecord {
                generation,
                changes,
            } => open_copy(&peering_directory, &initialize.session, &mut copy)
                .and_then(|copy| copy.record(generation, &changes))
                .map(|generation| Response::Recorded { generation }),
            Request::AncestorCheckpoint {
                generation,
                ancestor,
            } => open_copy(&peering_directory, &initialize.session, &mut copy)
                .and_then(|copy| copy.checkpoint(generation, ancestor))
                .map(|generation| Response::Recorded { generation }),
            Request::PutPeeringFile { name, bytes } => peering_directory
                .as_ref()
                .map_err(|e| anyhow!("{e:#}"))
                .and_then(|directory| crate::peering::write_pushed_file(directory, &name, &bytes))
                .map(|()| Response::Written),
            Request::PeeringState => peering_directory
                .as_ref()
                .map_err(|e| anyhow!("{e:#}"))
                .and_then(|directory| {
                    let lease = crate::peering::read_lease(directory)?;
                    let generation = match &copy {
                        Some(copy) => Some(copy.generation()),
                        None if crate::peering::ancestor_copy_path(
                            directory,
                            &initialize.session,
                        )?
                        .exists() =>
                        {
                            Some(
                                open_copy(&peering_directory, &initialize.session, &mut copy)?
                                    .generation(),
                            )
                        }
                        None => None,
                    };
                    Ok(Response::PeeringState(crate::peering::State {
                        lease,
                        generation,
                    }))
                }),
            Request::Scan => reporting_scan(output, channel, &counted, || endpoint.scan())
                .and_then(|snapshot| {
                    // Root identity settles the whole snapshot: its statistics
                    // are derived from the hierarchy, leaving only the probed
                    // executability behavior to compare alongside it.
                    let unchanged = last_sent.as_ref().is_some_and(|sent| {
                        crate::tree::nodes_share_storage(sent.root.as_ref(), snapshot.root.as_ref())
                            && sent.preserves_executability == snapshot.preserves_executability
                    });
                    if unchanged {
                        return Ok(Response::ScanUnchanged {
                            generation: endpoint.generation().unwrap_or(0),
                        });
                    }
                    let (answer, encoding) = changed_scan(
                        &snapshot,
                        last_sent.as_ref(),
                        last_sent_encoding.as_ref(),
                        &mut digester,
                        &mut pending,
                        endpoint.generation().unwrap_or(0),
                    )?;
                    anchor = Anchor::To(Some(snapshot));
                    anchor_encoding = encoding;
                    Ok(answer)
                }),
            Request::ScanVerified => {
                { reporting_scan(output, channel, &counted, || endpoint.scan_verified()) }.and_then(
                    |snapshot| {
                        // Never elided: the entire point is a full re-read whose
                        // result the controller sees in full.
                        let (answer, encoding) = changed_scan(
                            &snapshot,
                            last_sent.as_ref(),
                            last_sent_encoding.as_ref(),
                            &mut digester,
                            &mut pending,
                            endpoint.generation().unwrap_or(0),
                        )?;
                        anchor = Anchor::To(Some(snapshot));
                        anchor_encoding = encoding;
                        Ok(answer)
                    },
                )
            }
            Request::ScanFull => match last_sent.as_ref() {
                // The controller could not reproduce the baseline the last
                // delta named. The snapshot it wants is the one this channel
                // just anchored; it goes again against nothing.
                Some(snapshot) => snapshot_delta(
                    snapshot,
                    None,
                    None,
                    &mut pending,
                    endpoint.generation().unwrap_or(0),
                )
                .map(|(header, _)| Response::ScanDelta(header)),
                None => Err(anyhow!("a full scan was requested before any scan")),
            },
            Request::ScanPull => Ok(Response::ScanOps(next_scan_batch(&mut pending))),
            Request::ReadFile(path) => endpoint.read_file(&path).map(Response::File),
            Request::Rename(from, to) => endpoint.rename(&from, &to).map(|()| Response::Written),
            Request::StageBegin(files) => endpoint.stage_begin(files).map(Response::StageBegin),
            Request::SupplyOpen(needs) => {
                endpoint.supply_open(needs).map(|()| Response::SupplyOpened)
            }
            Request::SupplyPull(max_frames) => {
                endpoint.supply_pull(max_frames).map(Response::SupplyPull)
            }
            Request::StagePush(frames) => {
                endpoint.stage_push(frames).map(|()| Response::StagePushed)
            }
            Request::Transition(transitions) => {
                let outcome = endpoint.transition(transitions);
                // The transition folded its results into the endpoint's
                // snapshot, and the controller folds its own model the same
                // way — so what the controller now believes is exactly this
                // tree. Re-anchoring here is what lets the *next* scan of an
                // otherwise untouched destination report itself unchanged,
                // which is the common case under one-directional editing.
                // Only re-anchor when something *was* sent: with nothing
                // transmitted yet the controller has no model to fold, so
                // claiming this tree as its own would let the next scan
                // answer "unchanged" for a hierarchy it never received.
                if outcome.is_ok() && last_sent.is_some() {
                    anchor = Anchor::To(anchor_after_transition(
                        last_sent.as_ref(),
                        endpoint.snapshot(),
                    ));
                }
                let generation = endpoint.generation().unwrap_or(0);
                outcome.map(|outcome| Response::Transition {
                    outcome,
                    generation,
                })
            }
            Request::AwaitChanges {
                milliseconds,
                since,
            } => endpoint
                .await_change_since(since, std::time::Duration::from_millis(milliseconds))
                .map(|(changed, watching)| Response::AwaitChanges { changed, watching }),
        };
        let response = result.unwrap_or_else(|error| Response::Error(format!("{error:#}")));
        // A response can be unsendable for its own reasons (most notably an
        // encoding larger than the frame cap) while the transport is
        // healthy; a small error frame keeps the controller from waiting
        // forever. If even that fails, the connection is gone and the
        // dispatcher is failing with it.
        let delivered = serve_send(output, channel, response);
        match (&delivered, anchor) {
            (Ok(()), Anchor::To(snapshot)) => {
                last_sent = snapshot;
                last_sent_encoding = anchor_encoding;
                // Digested now, while the controller works on the answer,
                // rather than when the next scan is waited on. After a
                // changed scan this finds everything already digested.
                if let Some(sent) = &last_sent {
                    digester.snapshot(sent);
                }
            }
            (Ok(()), Anchor::Keep) => {}
            // Forgetting everything costs one full resend and avoids having
            // to reason about which send failures leave the controller's
            // model intact and which do not. Claiming otherwise is the
            // expensive mistake: it would let a later scan report
            // "unchanged" against a tree that never arrived.
            (Err(_), _) => {
                last_sent = None;
                last_sent_encoding = None;
            }
        }
        if let Err(error) = delivered {
            let fallback = Response::Error(format!("unable to send the response: {error:#}"));
            if serve_send(output, channel, fallback).is_err() {
                return;
            }
        }
    }
}

/// Encodes a snapshot the way both ends of a channel must: the bare
/// hierarchy, so that the controller can re-encode the copy it holds and
/// obtain the exact bytes a delta was computed against.
pub fn encode_snapshot(snapshot: &Snapshot) -> Result<Vec<u8>> {
    bincode::serialize(snapshot).context("unable to encode the snapshot")
}

/// What this channel should record as sent, once a transition succeeds.
///
/// Both sides fold the same achieved results, but from different copies.
/// The controller folds the snapshot it last received and keeps that copy's
/// scan stamp. This side's endpoint has already moved its stamp on, at a
/// rescan that reported itself unchanged and so never reached the
/// controller. The trees agree and the stamp does not, and the baseline is
/// agreed by a digest over the whole encoding — so without carrying the
/// controller's stamp across, the next delta names a baseline it cannot
/// reproduce and the snapshot is resent in full.
///
/// The root is untouched, so the storage sharing that lets the next scan of
/// an untouched tree report itself unchanged still holds.
fn anchor_after_transition(sent: Option<&Snapshot>, folded: Option<&Snapshot>) -> Option<Snapshot> {
    let sent = sent?;
    let mut folded = folded?.clone();
    folded.scanned_at_seconds = sent.scanned_at_seconds;
    Some(folded)
}

/// Prepares a snapshot for transmission as a delta against `baseline` (the
/// snapshot this channel last sent, or `None` for a full stream), leaving
/// the operations queued for `ScanPull` and returning the header.
///
/// The largest change set sent as changes; a bigger one goes as a byte
/// delta, which bounds what one answer can carry (a new subtree is sent
/// whole) and suits a rewrite of much of the tree better anyway.
const SCAN_CHANGES_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// A snapshot's encoding and the digest of it.
pub(crate) type Encoding = (Vec<u8>, crate::tree::Digest);

/// Answers a changed scan: as the changes from the snapshot last sent when
/// there is one and the changes are small, and as a byte delta otherwise.
/// Returns the answer and, for a byte delta, the new snapshot's encoding,
/// which the next byte delta can use as its base; after changes it is made
/// only if a byte delta ever needs it.
fn changed_scan(
    snapshot: &Snapshot,
    last_sent: Option<&Snapshot>,
    last_sent_encoding: Option<&Encoding>,
    digester: &mut crate::tree::TreeDigester,
    pending: &mut std::collections::VecDeque<crate::rsync::Op>,
    generation: u64,
) -> Result<(Response, Option<Encoding>)> {
    if let Some(sent) = last_sent {
        let changes = exact_changes(sent.root.as_ref(), snapshot.root.as_ref());
        let small =
            bincode::serialized_size(&changes).is_ok_and(|size| size <= SCAN_CHANGES_MAX_BYTES);
        if small {
            // Both digests are tree digests, which hash only what changed
            // since the digester last saw these trees: no encoding at all
            // on this path. One is made only if a later scan needs a byte
            // delta against this snapshot.
            let answer = Response::ScanChanges(Box::new(protocol::ScanChanges {
                generation,
                baseline: digester.snapshot(sent),
                digest: digester.snapshot(snapshot),
                head: Snapshot {
                    root: None,
                    ..snapshot.clone()
                },
                changes,
            }));
            return Ok((answer, None));
        }
    }
    let (header, encoding) =
        snapshot_delta(snapshot, last_sent, last_sent_encoding, pending, generation)?;
    Ok((Response::ScanDelta(header), Some(encoding)))
}

/// The changes that turn `base` into `target` *exactly* — scan metadata
/// included, which `tree::diff` rightly ignores — so that applying them
/// reproduces `target`'s encoding byte for byte. Each carries only its new
/// content. Subtrees sharing storage are skipped without a walk, which is
/// what makes this cost the size of the change: a scan adopts what it did
/// not revisit.
pub(crate) fn exact_changes(base: Option<&Node>, target: Option<&Node>) -> Vec<Change> {
    fn same_leaf(a: &Node, b: &Node) -> bool {
        use crate::tree::Content;
        match (&a.content, &b.content) {
            (
                Content::File {
                    digest: d1,
                    executable: e1,
                    metadata: m1,
                },
                Content::File {
                    digest: d2,
                    executable: e2,
                    metadata: m2,
                },
            ) => d1 == d2 && e1 == e2 && m1 == m2,
            (Content::Symlink { target: t1 }, Content::Symlink { target: t2 }) => t1 == t2,
            (Content::Untracked, Content::Untracked) => true,
            (Content::Problematic { message: m1 }, Content::Problematic { message: m2 }) => {
                m1 == m2
            }
            _ => false,
        }
    }
    fn walk(path: &str, base: Option<&Node>, target: Option<&Node>, changes: &mut Vec<Change>) {
        use crate::tree::Content;
        let replace = |changes: &mut Vec<Change>| {
            changes.push(Change {
                path: path.to_owned(),
                old: None,
                new: target.cloned(),
            })
        };
        match (base, target) {
            (None, None) => {}
            (Some(b), Some(t)) => match (&b.content, &t.content) {
                (Content::Directory(left), Content::Directory(right)) => {
                    if std::sync::Arc::ptr_eq(left, right) {
                        return;
                    }
                    let (mut i, mut j) = (0, 0);
                    while i < left.len() || j < right.len() {
                        let order = match (left.get(i), right.get(j)) {
                            (Some(l), Some(r)) => l.name.cmp(&r.name),
                            (Some(_), None) => std::cmp::Ordering::Less,
                            (None, Some(_)) => std::cmp::Ordering::Greater,
                            (None, None) => unreachable!(),
                        };
                        match order {
                            std::cmp::Ordering::Less => {
                                let child = &left[i];
                                walk(&path_join(path, &child.name), Some(child), None, changes);
                                i += 1;
                            }
                            std::cmp::Ordering::Greater => {
                                let child = &right[j];
                                walk(&path_join(path, &child.name), None, Some(child), changes);
                                j += 1;
                            }
                            std::cmp::Ordering::Equal => {
                                let child = &left[i];
                                walk(
                                    &path_join(path, &child.name),
                                    Some(child),
                                    Some(&right[j]),
                                    changes,
                                );
                                i += 1;
                                j += 1;
                            }
                        }
                    }
                }
                (Content::Directory(_), _) | (_, Content::Directory(_)) => replace(changes),
                _ => {
                    if !same_leaf(b, t) {
                        replace(changes);
                    }
                }
            },
            _ => replace(changes),
        }
    }
    let mut changes = Vec::new();
    walk("", base, target, &mut changes);
    changes
}

/// The baseline's encoding is the one the scan that produced it made, when
/// the caller kept it (`baseline_encoding`), and is re-encoded otherwise —
/// after a transition's fold. The new snapshot's encoding comes back for
/// the caller to keep for the next delta. Keeping them holds one encoding
/// per channel (45 MB at 420k files) in exchange for not encoding and
/// hashing the baseline again on every changed scan, which was 65 ms of the
/// 224 ms the agent spent per edit at that size. The header carries the
/// digest of the *new* encoding, so if the controller's copy of the
/// baseline were ever to differ from this one, the reassembly would fail to
/// verify and be redone in full — determinism of the encoding is a
/// performance assumption, not a correctness one.
fn snapshot_delta(
    snapshot: &Snapshot,
    baseline: Option<&Snapshot>,
    baseline_encoding: Option<&Encoding>,
    pending: &mut std::collections::VecDeque<crate::rsync::Op>,
    generation: u64,
) -> Result<(protocol::ScanDelta, Encoding)> {
    let target = encode_snapshot(snapshot)?;
    let digest = *blake3::hash(&target).as_bytes();
    let (baseline_digest, signature) = match baseline {
        Some(baseline) => {
            let encoded;
            let (base, base_digest) = match baseline_encoding {
                Some((bytes, digest)) => (bytes.as_slice(), *digest),
                None => {
                    encoded = encode_snapshot(baseline)?;
                    let digest = *blake3::hash(&encoded).as_bytes();
                    (encoded.as_slice(), digest)
                }
            };
            let block_size = crate::rsync::optimal_block_size(base.len() as u64);
            let signature = crate::rsync::signature(std::io::Cursor::new(base), block_size)
                .context("unable to sign the baseline snapshot")?;
            (Some(base_digest), signature)
        }
        None => (None, crate::rsync::Signature::default()),
    };
    pending.clear();
    crate::rsync::deltify(std::io::Cursor::new(&target), &signature, &mut |op| {
        pending.push_back(op);
        Ok(())
    })
    .context("unable to compute the snapshot delta")?;
    let header = protocol::ScanDelta {
        generation,
        baseline: baseline_digest,
        digest,
        length: target.len() as u64,
        block_size: signature.block_size,
    };
    Ok((header, (target, digest)))
}

/// The content bound on one batch of snapshot delta operations. Batches
/// are bounded by content, not count: a data operation carries up to
/// 64 KiB, a block operation almost nothing, and the frame cap is on bytes.
const SCAN_BATCH_BYTES: usize = 4 * 1024 * 1024;

/// Takes the next batch of queued delta operations, empty when the stream
/// is exhausted.
fn next_scan_batch(
    pending: &mut std::collections::VecDeque<crate::rsync::Op>,
) -> Vec<crate::rsync::Op> {
    let mut batch = Vec::new();
    let mut bytes = 0usize;
    while let Some(op) = pending.front() {
        let size = match op {
            crate::rsync::Op::Data(data) => data.len(),
            crate::rsync::Op::Blocks { .. } => 16,
        };
        if !batch.is_empty() && bytes + size > SCAN_BATCH_BYTES {
            break;
        }
        bytes += size;
        batch.push(pending.pop_front().expect("front was Some"));
    }
    batch
}

/// Sends one channel-tagged response frame through the shared writer.
fn serve_send<W: Write>(
    output: &std::sync::Mutex<W>,
    channel: u32,
    response: Response,
) -> Result<()> {
    // Encoded before the lock is taken: see `encode_frame`.
    let bytes = encode_frame(&protocol::MuxResponse::Response { channel, response })
        .context("unable to send response")?;
    let mut output = output
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    output
        .write_all(&bytes)
        .and_then(|()| output.flush())
        .context("unable to send response")
}

/// What a delivered response implies about the snapshot a channel has
/// transmitted.
enum Anchor {
    /// The response says nothing about it; leave the record alone.
    Keep,
    /// The controller now holds this hierarchy (`None` for "nothing").
    To(Option<Snapshot>),
}

/// Creates a channel's endpoint from the controller's initialization
/// request. State-mode staging lives under the agent's state area —
/// `crate::paths::default_state_root()` in production, so that
/// `AUTOBAHN_HOME` moves it with everything else autobahn keeps — keyed by
/// session identifier and side so that concurrent sessions (and
/// interrupted cycles, and the two sides of one session) never share
/// staging space; the root-relative placements follow the controller's
/// staging mode.
fn create_endpoint(initialize: &Initialize, state_root: &Result<PathBuf>) -> Result<LocalEndpoint> {
    // The session and side come off the wire and name directories below;
    // nothing touches the filesystem until they are known to be genuine.
    initialize.validate()?;
    let state_root = state_root
        .as_ref()
        .map_err(|error| anyhow!("unable to determine the agent's state directory: {error:#}"))?;
    // Expand a home-relative root against this agent's home directory, so
    // that a configuration like `alpha = "~/project"` fanned out to several
    // hosts lands in each host's own home rather than a literal `~`.
    let root = crate::paths::expand_tilde(&initialize.root)?;
    let staging_area = state_root.join("staging");
    let state_staging = staging_area.join(format!("{}-{}", initialize.session, initialize.side));
    let staging_root = crate::endpoint::local::staging_root_for(
        initialize.staging,
        &root,
        state_staging,
        &initialize.session,
        &initialize.side,
    )?;
    let options = EndpointOptions {
        ignores: IgnoreSet::new(&initialize.ignores).context("unable to compile ignores")?,
        symlink_mode: initialize.symlink_mode,
        file_mode: initialize.file_mode,
        directory_mode: initialize.directory_mode,
        max_file_size: initialize.max_file_size,
        max_entry_count: initialize.max_entry_count,
        default_owner: initialize.default_owner.clone(),
        default_group: initialize.default_group.clone(),
        // A session that will wait for changes has its root watched; a
        // single pass says so, and is spared the registration walk.
        one_shot: initialize.one_shot,
        ignore_mounts: initialize.ignore_mounts,
    };
    LocalEndpoint::new(root, staging_root, options)
        .with_context(|| format!("unable to create an endpoint for {}", initialize.root))
}

/// The argv that runs `remote_command` on `destination` over SSH, with the
/// options every autobahn connection uses.
pub fn ssh_argv_for(destination: &str, remote_command: &str) -> Vec<String> {
    Connection::ssh_argv(destination, Some(remote_command))
}

/// Peering: runs `argv` — the alpha's way to a leader, `ssh <leader>
/// autobahn peering attach` by default — and serves as an agent over its
/// stdio until the far side closes. The alpha is never dialed; this is
/// how it makes itself an endpoint of a session a beta leads.
pub fn attach_as_agent(argv: &[String]) -> Result<()> {
    let (program, arguments) = argv
        .split_first()
        .ok_or_else(|| anyhow!("the attach command is empty"))?;
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("unable to run {}", argv.join(" ")))?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let served = serve_agent(stdout, stdin);
    let _ = child.wait();
    served
}

/// The channel's ancestor copy, opened on first use and kept for the
/// channel's lifetime.
fn open_copy<'a>(
    directory: &Result<PathBuf>,
    session: &str,
    slot: &'a mut Option<crate::peering::AncestorCopy>,
) -> Result<&'a mut crate::peering::AncestorCopy> {
    if slot.is_none() {
        let directory = directory.as_ref().map_err(|error| anyhow!("{error:#}"))?;
        *slot = Some(crate::peering::AncestorCopy::open(directory, session)?);
    }
    Ok(slot.as_mut().expect("just opened"))
}

/// Returns the handshake describing this build.
pub(crate) fn local_handshake() -> Handshake {
    Handshake {
        magic: protocol::MAGIC,
        version: protocol::version(),
    }
}

/// Verifies a peer's handshake, requiring the protocol magic and an exact
/// version match. Both ends run the same binary, so anything short of
/// equality is treated as an incompatibility rather than negotiated: the
/// wire format is free to change between versions.
pub(crate) fn verify_handshake(handshake: &Handshake) -> Result<()> {
    if handshake.magic != protocol::MAGIC {
        bail!(
            "invalid handshake magic (0x{:08x}, expected 0x{:08x}): the peer is not an autobahn agent",
            handshake.magic,
            protocol::MAGIC
        );
    }
    let version = protocol::version();
    if handshake.version != version {
        bail!(
            "version mismatch: the local version is {} but the remote version is {} \
             (the agent on the remote host must be updated to match)",
            version,
            handshake.version
        );
    }
    Ok(())
}

/// The frame-payload flag marking an uncompressed body.
///
/// The flag byte was introduced with compression; a build predating it
/// reads the flag as part of the handshake payload (and vice versa), so
/// version mismatches against such builds surface as decode errors rather
/// than the named-versions diagnostic. Builds from the flag onward share
/// the outer format, and their handshakes decode — and diagnose — across
/// versions.
const FRAME_UNCOMPRESSED: u8 = 0;

/// The frame-payload flag marking an LZ4-compressed body (followed by the
/// decompressed length).
const FRAME_COMPRESSED: u8 = 1;

/// The flag bit marking a frame as one piece of a larger message, with
/// more pieces to follow. A message whose encoding exceeds
/// [`FRAME_CHUNK_SIZE`] is sent as a sequence of frames carrying this bit,
/// ending with one that does not; the reader concatenates the bodies.
///
/// The per-frame cap therefore bounds a *frame* — its job, defending
/// against a corrupt or hostile length prefix — while [`MAXIMUM_MESSAGE_SIZE`]
/// bounds what a sequence may reassemble to. Before this, the frame cap
/// was also a ceiling on message size, which made it a ceiling on tree
/// size: a staging request or a transition over a large tree is one list,
/// and a large enough list could not be sent at all.
const FRAME_MORE: u8 = 2;

/// The encoded size above which a message is split into frames.
const FRAME_CHUNK_SIZE: usize = 16 * 1024 * 1024;

/// The largest message a sequence of frames may reassemble to.
pub(crate) const MAXIMUM_MESSAGE_SIZE: usize = 4 * 1024 * 1024 * 1024;

/// The encoded size below which compression isn't attempted: tiny frames
/// (bare requests, acknowledgements) can't compress meaningfully and would
/// only pay the header.
const COMPRESSION_THRESHOLD: usize = 256;

/// The bytes the compressed-frame header adds to a payload.
const COMPRESSED_HEADER_SIZE: usize = 5;

// Scratch buffers for frame assembly, reused across sends on this thread.
//
// `bincode::serialize` traverses a message once to size it and again to
// encode it, then hands back a fresh allocation; `serialize_into` a buffer
// that already has capacity does one traversal into memory that is already
// there. On a supply batch — megabytes of file content — the sizing pass is
// most of the encoding cost.
thread_local! {
    static FRAME_SCRATCH: RefCell<FrameScratch> = const {
        RefCell::new(FrameScratch { encoded: Vec::new(), compressed: Vec::new() })
    };
}

/// Scratch above this size is released rather than retained: one oversized
/// frame should not pin megabytes on a thread that goes back to sending
/// acknowledgements.
const SCRATCH_RETENTION_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Default)]
struct FrameScratch {
    encoded: Vec<u8>,
    compressed: Vec<u8>,
}

/// Encodes a message into the exact bytes `send_frame` would write, so a
/// caller sharing its writer can encode and compress *before* taking the
/// writer's lock and hold it only to write. Held across the encoding, the
/// lock made every small request to a host wait out the compression of
/// whatever 8 MiB transfer batch was ahead of it.
pub(crate) fn encode_frame<T: Serialize>(message: &T) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    send_frame(&mut bytes, message)?;
    Ok(bytes)
}

/// Encodes a message and writes it as length-prefixed frames (one, unless
/// it is larger than [`FRAME_CHUNK_SIZE`]), flushing so that the peer sees
/// it immediately.
fn send_frame<W: Write, T: Serialize>(writer: &mut W, message: &T) -> Result<()> {
    // The buffers are moved out for the duration rather than borrowed
    // across the write, so a writer that re-entered this function on the
    // same thread would find empty scratch rather than a panicking borrow.
    let mut scratch = FRAME_SCRATCH.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
    let result = assemble_and_write(writer, message, &mut scratch);
    if scratch.encoded.capacity() <= SCRATCH_RETENTION_LIMIT
        && scratch.compressed.capacity() <= SCRATCH_RETENTION_LIMIT
    {
        FRAME_SCRATCH.with(|cell| *cell.borrow_mut() = scratch);
    }
    result
}

fn assemble_and_write<W: Write, T: Serialize>(
    writer: &mut W,
    message: &T,
    scratch: &mut FrameScratch,
) -> Result<()> {
    scratch.encoded.clear();
    bincode::serialize_into(&mut scratch.encoded, message).context("unable to encode frame")?;
    if scratch.encoded.len() > MAXIMUM_MESSAGE_SIZE {
        bail!(
            "outgoing message of {} bytes exceeds the maximum message size of {} bytes",
            scratch.encoded.len(),
            MAXIMUM_MESSAGE_SIZE
        );
    }
    // A message larger than one chunk goes as several frames, each marked
    // "more follows" except the last. The scratch buffers are split
    // temporarily so a chunk can be compressed into `compressed` while the
    // encoding is read from `encoded`.
    let encoded = std::mem::take(&mut scratch.encoded);
    let mut result = Ok(());
    let chunks = if encoded.is_empty() {
        1
    } else {
        encoded.len().div_ceil(FRAME_CHUNK_SIZE)
    };
    for (index, chunk) in encoded.chunks(FRAME_CHUNK_SIZE.max(1)).enumerate() {
        let more = index + 1 < chunks;
        result = write_chunk(writer, chunk, more, scratch);
        if result.is_err() {
            break;
        }
    }
    if encoded.is_empty() {
        result = write_chunk(writer, &[], false, scratch);
    }
    scratch.encoded = encoded;
    result
}

/// Writes one frame carrying `encoded` (a whole message, or a chunk of
/// one when `more` is set).
fn write_chunk<W: Write>(
    writer: &mut W,
    encoded: &[u8],
    more: bool,
    scratch: &mut FrameScratch,
) -> Result<()> {
    let more_bit = if more { FRAME_MORE } else { 0 };

    // Compress when it actually helps: the payload carries a flag byte
    // declaring which form it took, so the reader never guesses (and a
    // frame that doesn't shrink — already-compressed file content, mostly —
    // travels verbatim).
    //
    // The body is written straight from whichever buffer holds it. Copying
    // it into an assembled payload first, as this once did, meant a third
    // full-size pass over every frame purely to prepend five bytes.
    let mut header = [0u8; 9];
    // One place assembles the header, so the three ways a frame can be
    // framed cannot drift apart. Each returns the body to write beside it.
    let uncompressed = |header: &mut [u8; 9]| -> usize {
        header[..4].copy_from_slice(&((encoded.len() + 1) as u32).to_le_bytes());
        header[4] = FRAME_UNCOMPRESSED | more_bit;
        5
    };
    let (header_len, body): (usize, &[u8]) = if encoded.len() >= COMPRESSION_THRESHOLD {
        // Compress straight into the reused buffer. The buffer is only ever
        // grown, so the zero-fill that sizing it requires is paid on the
        // first large frame and not on the thousands that follow.
        let capacity = lz4_flex::block::get_maximum_output_size(encoded.len());
        if scratch.compressed.len() < capacity {
            scratch.compressed.resize(capacity, 0);
        }
        let size = lz4_flex::block::compress_into(encoded, &mut scratch.compressed[..capacity])
            .context("unable to compress frame")?;
        if size + COMPRESSED_HEADER_SIZE < encoded.len() {
            header[..4].copy_from_slice(&((size + COMPRESSED_HEADER_SIZE) as u32).to_le_bytes());
            header[4] = FRAME_COMPRESSED | more_bit;
            header[5..9].copy_from_slice(&(encoded.len() as u32).to_le_bytes());
            (9, &scratch.compressed[..size])
        } else {
            (uncompressed(&mut header), encoded)
        }
    } else {
        (uncompressed(&mut header), encoded)
    };

    writer
        .write_all(&header[..header_len])
        .context("unable to write frame length")?;
    writer
        .write_all(body)
        .context("unable to write frame payload")?;
    writer.flush().context("unable to flush frame")?;
    Ok(())
}

/// Reads one frame and decodes it, treating end-of-stream as an error.
fn receive_frame<R: Read, T: DeserializeOwned>(reader: &mut R) -> Result<T> {
    let payload = read_frame(reader)?.ok_or_else(|| anyhow!("connection closed"))?;
    bincode::deserialize(&payload).context("unable to decode frame")
}

/// Reads one frame's payload, returning `None` for a clean end-of-stream at a
/// frame boundary (which callers awaiting an answer treat as an error, but
/// which is the agent's normal exit condition). The decompressed size is
/// validated against the frame cap *before* any allocation, so a hostile
/// header can't induce one.
fn read_frame<R: Read>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let Some((mut message, mut more)) = read_chunk(reader)? else {
        return Ok(None);
    };
    while more {
        // End-of-stream inside a sequence is a truncated message, not a
        // clean close.
        let Some((chunk, further)) = read_chunk(reader)? else {
            bail!("connection closed in the middle of a message");
        };
        if message.len() + chunk.len() > MAXIMUM_MESSAGE_SIZE {
            bail!(
                "incoming message exceeds the maximum message size of {} bytes",
                MAXIMUM_MESSAGE_SIZE
            );
        }
        message.extend_from_slice(&chunk);
        more = further;
    }
    Ok(Some(message))
}

/// Reads one frame, returning its body and whether more frames of the same
/// message follow.
fn read_chunk<R: Read>(reader: &mut R) -> Result<Option<(Vec<u8>, bool)>> {
    let mut prefix = [0u8; 4];
    match reader.read_exact(&mut prefix) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error).context("unable to read frame length"),
    }
    let length = u32::from_le_bytes(prefix);
    if length as usize > protocol::MAXIMUM_FRAME_SIZE as usize + COMPRESSED_HEADER_SIZE {
        bail!(
            "incoming frame of {} bytes exceeds the maximum frame size of {} bytes",
            length,
            protocol::MAXIMUM_FRAME_SIZE
        );
    }
    let mut payload = vec![0u8; length as usize];
    match reader.read_exact(&mut payload) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::UnexpectedEof => {
            bail!("connection closed in the middle of a frame")
        }
        Err(error) => return Err(error).context("unable to read frame payload"),
    }

    let Some((&flag, _)) = payload.split_first() else {
        bail!("empty frame");
    };
    let more = flag & FRAME_MORE != 0;
    let flag = flag & !FRAME_MORE;
    match payload.split_first().map(|(_, body)| (flag, body)) {
        Some((FRAME_UNCOMPRESSED, body)) => {
            // The outer length admits the compressed header's overhead; an
            // uncompressed body must still respect the frame cap itself.
            if body.len() > protocol::MAXIMUM_FRAME_SIZE as usize {
                bail!(
                    "incoming frame of {} bytes exceeds the maximum frame size of {} bytes",
                    body.len(),
                    protocol::MAXIMUM_FRAME_SIZE
                );
            }
            Ok(Some((body.to_vec(), more)))
        }
        Some((FRAME_COMPRESSED, rest)) => {
            if rest.len() < 4 {
                bail!("compressed frame is missing its length header");
            }
            let (header, body) = rest.split_at(4);
            let raw_length =
                u32::from_le_bytes(header.try_into().expect("the header is four bytes"));
            if raw_length > protocol::MAXIMUM_FRAME_SIZE {
                bail!(
                    "compressed frame declares {} bytes, exceeding the maximum frame size of \
                     {} bytes",
                    raw_length,
                    protocol::MAXIMUM_FRAME_SIZE
                );
            }
            let decompressed = lz4_flex::block::decompress(body, raw_length as usize)
                .context("unable to decompress frame")?;
            Ok(Some((decompressed, more)))
        }
        Some((flag, _)) => bail!("invalid frame flag {flag}"),
        None => bail!("empty frame"),
    }
}

#[cfg(test)]
mod adversarial {
    //! The frame decoder reads from a peer across a trust boundary. These
    //! feed it input no honest peer would send — random bytes, crafted
    //! headers, truncation at every offset — and demand that it always
    //! returns an error rather than panicking, hanging, or allocating on
    //! the strength of a number the peer chose.
    use super::*;

    /// A tiny deterministic generator, so a failure reproduces from its seed.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn bytes(&mut self, count: usize) -> Vec<u8> {
            (0..count).map(|_| (self.next() & 0xff) as u8).collect()
        }
    }

    /// Random bytes must never panic the decoder.
    #[test]
    fn random_input_is_rejected_without_panicking() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..20_000 {
            let length = (rng.next() % 64) as usize;
            let bytes = rng.bytes(length);
            let _ = read_frame(&mut bytes.as_slice());
        }
    }

    /// A frame truncated at any offset must be an error, never a panic and
    /// never a silent partial read.
    #[test]
    fn truncation_at_every_offset_is_rejected() {
        let body = b"\x00some plausible payload";
        let mut framed = (body.len() as u32).to_le_bytes().to_vec();
        framed.extend_from_slice(body);
        for cut in 0..framed.len() {
            let truncated = &framed[..cut];
            match read_frame(&mut &truncated[..]) {
                Ok(None) => {} // clean end of stream
                Ok(Some(_)) if cut == framed.len() => {}
                Ok(Some(_)) => panic!("a truncated frame decoded at offset {cut}"),
                Err(_) => {} // reported, which is the contract
            }
        }
    }

    /// A declared length beyond the cap must be refused on the strength of
    /// the number alone, before the decoder sizes a buffer from it.
    ///
    /// Note the allowance: the outer length legitimately covers the
    /// compression header, so a value a few bytes past the nominal cap is
    /// accepted for reading and then fails as a short frame. Only a value
    /// past cap-plus-header is refused by the cap itself.
    #[test]
    fn an_oversized_length_prefix_is_refused_not_allocated() {
        let over_the_cap = protocol::MAXIMUM_FRAME_SIZE + COMPRESSED_HEADER_SIZE as u32 + 1;
        for declared in [over_the_cap, protocol::MAXIMUM_FRAME_SIZE * 2, u32::MAX] {
            let prefix = declared.to_le_bytes();
            let error = read_frame(&mut &prefix[..]).expect_err("must be refused");
            assert!(
                format!("{error:#}").contains("maximum frame size"),
                "declared {declared}: {error:#}"
            );
        }
        // Just inside the allowance is still an error, by way of the short
        // read rather than the cap — the point is that it never succeeds.
        let prefix = (protocol::MAXIMUM_FRAME_SIZE + 1).to_le_bytes();
        read_frame(&mut &prefix[..]).expect_err("a short frame must be refused");
    }

    /// A compressed frame that claims to expand to far more than it carries
    /// is the classic decompression bomb. The declared size is capped, and
    /// a body that does not actually produce it must fail rather than
    /// yielding a partially filled buffer.
    #[test]
    fn a_decompression_bomb_is_refused() {
        let mut rng = Rng(0xDEAD_BEEF_CAFE_F00D);
        for declared in [protocol::MAXIMUM_FRAME_SIZE, 1 << 20, 4096] {
            let mut payload = vec![FRAME_COMPRESSED];
            payload.extend_from_slice(&declared.to_le_bytes());
            payload.extend_from_slice(&rng.bytes(16));
            let mut framed = (payload.len() as u32).to_le_bytes().to_vec();
            framed.extend_from_slice(&payload);
            if let Ok(Some(body)) = read_frame(&mut framed.as_slice()) {
                assert!(
                    body.len() <= declared as usize,
                    "decoded {} bytes against a declared {declared}",
                    body.len()
                );
            }
        }
        // And one that declares more than the cap must be refused outright.
        let mut payload = vec![FRAME_COMPRESSED];
        payload.extend_from_slice(&(protocol::MAXIMUM_FRAME_SIZE + 1).to_le_bytes());
        payload.extend_from_slice(&[0u8; 8]);
        let mut framed = (payload.len() as u32).to_le_bytes().to_vec();
        framed.extend_from_slice(&payload);
        read_frame(&mut framed.as_slice()).expect_err("an over-cap expansion must be refused");
    }

    /// An unknown flag byte is a protocol violation, not something to guess at.
    #[test]
    fn an_unknown_frame_flag_is_refused() {
        for flag in [2u8, 7, 200, 255] {
            let payload = vec![flag, 1, 2, 3];
            let mut framed = (payload.len() as u32).to_le_bytes().to_vec();
            framed.extend_from_slice(&payload);
            read_frame(&mut framed.as_slice()).expect_err("unknown flags must be refused");
        }
        // An empty frame carries no flag at all.
        let framed = 0u32.to_le_bytes().to_vec();
        read_frame(&mut framed.as_slice()).expect_err("an empty frame must be refused");
    }

    /// Whatever survives framing still has to decode as a message. Random
    /// bodies must be rejected by the message decoder, not panic it.
    #[test]
    fn random_bodies_do_not_panic_the_message_decoder() {
        let mut rng = Rng(0x0123_4567_89AB_CDEF);
        for _ in 0..20_000 {
            let length = (rng.next() % 128) as usize;
            let body = rng.bytes(length);
            let _ = bincode::deserialize::<protocol::MuxRequest>(&body);
            let _ = bincode::deserialize::<protocol::MuxResponse>(&body);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use std::sync::mpsc::{channel, Receiver, Sender};

    use crate::endpoint::FileRequest;

    /// A hostile controller's traversal session is refused before
    /// anything touches the filesystem: `..` once named the state area
    /// itself to a `remove_dir_all`, taking the configuration, every
    /// ancestor and the agent bundle with it.
    #[test]
    fn a_traversal_session_is_refused_with_no_filesystem_effect() {
        let keep = tempfile::tempdir().expect("temporary directory");
        let state = keep.path().join(".autobahn");
        let sentinel = state.join("sentinel");
        std::fs::create_dir_all(state.join("staging")).expect("the state area");
        std::fs::write(&sentinel, b"kept").expect("the sentinel");
        let root = keep.path().join("root");
        std::fs::create_dir_all(&root).expect("the root");

        for (session, side) in [
            ("..", "beta"),
            ("../..", "beta"),
            ("/tmp/x", "beta"),
            ("", "beta"),
            (
                crate::session::session_identifier("a", "b").as_str(),
                "../..",
            ),
        ] {
            let initialize = Initialize {
                root: root.to_string_lossy().into_owned(),
                session: session.into(),
                ignores: Vec::new(),
                symlink_mode: crate::scan::SymlinkMode::Raw,
                file_mode: None,
                directory_mode: None,
                side: side.into(),
                staging: Default::default(),
                max_file_size: None,
                max_entry_count: None,
                ignore_mounts: true,
                default_owner: None,
                default_group: None,
                one_shot: false,
            };
            let error = create_endpoint(&initialize, &Ok(state.clone()))
                .err()
                .expect("the endpoint must be refused");
            assert!(
                format!("{error:#}").contains("refusing"),
                "{session:?}/{side:?}: {error:#}"
            );
            assert!(
                sentinel.is_file(),
                "{session:?}/{side:?} removed the state area"
            );
        }
        // Nothing was created for any of them either.
        let staged: Vec<_> = std::fs::read_dir(state.join("staging"))
            .expect("staging is readable")
            .collect();
        assert!(staged.is_empty(), "{staged:?}");
    }

    /// A genuine session and side are served, under the agent's state area.
    #[test]
    fn a_genuine_session_is_accepted() {
        let keep = tempfile::tempdir().expect("temporary directory");
        let root = keep.path().join("root");
        std::fs::create_dir_all(&root).expect("the root");
        let initialize = Initialize {
            root: root.to_string_lossy().into_owned(),
            session: crate::session::session_identifier("a", "b"),
            ignores: Vec::new(),
            symlink_mode: crate::scan::SymlinkMode::Raw,
            file_mode: None,
            directory_mode: None,
            side: "beta".into(),
            staging: Default::default(),
            max_file_size: None,
            max_entry_count: None,
            ignore_mounts: true,
            default_owner: None,
            default_group: None,
            one_shot: false,
        };
        create_endpoint(&initialize, &Ok(keep.path().join("state")))
            .expect("a genuine initialization is served");
    }

    /// What this channel recorded as sent must encode exactly as the
    /// controller's own model does. Every delta names its baseline by a
    /// digest over that encoding, so one differing field costs a full
    /// resend — and content equality, which is what the transition tests
    /// assert, cannot see a difference like that.
    #[test]
    fn a_transition_anchors_to_what_the_controller_will_hold() {
        use crate::tree::{Content, FileMetadata, Node};

        let file = |name: &str, digest_byte: u8| Node {
            name: name.to_owned(),
            content: Content::File {
                digest: [digest_byte; 32],
                executable: false,
                metadata: FileMetadata::default(),
            },
        };
        // What the controller last received, stamped when that scan ran.
        let sent = Snapshot {
            root: Some(Node::directory("", vec![file("a", 1)])),
            files: 1,
            directories: 1,
            symlinks: 0,
            total_file_size: 0,
            scanned_at_seconds: 100,
            preserves_executability: true,
            mount_points: Vec::new(),
        };
        // A transition adds a file. Both sides fold the same achieved
        // result, the controller from the snapshot above.
        let transitions = vec![crate::tree::Change {
            path: "b".to_owned(),
            old: None,
            new: Some(file("b", 2)),
        }];
        let outcome = crate::endpoint::TransitionOutcome {
            results: vec![Some(file("b", 2))],
            problems: Vec::new(),
            missing_staged_files: false,
            missing_staged: Vec::new(),
        };
        let controller = crate::endpoint::fold_transition(&sent, &transitions, &outcome)
            .expect("the controller folds its own copy");

        // This side's endpoint folded the same way, but a rescan that
        // reported itself unchanged has since moved its scan stamp on.
        let mut endpoint = controller.clone();
        endpoint.scanned_at_seconds = 200;
        assert_ne!(
            encode_snapshot(&endpoint).expect("encodes"),
            encode_snapshot(&controller).expect("encodes"),
            "the fixture must reproduce the disagreement being guarded against"
        );

        let anchored = anchor_after_transition(Some(&sent), Some(&endpoint))
            .expect("an anchor, once something has been sent");
        assert_eq!(
            encode_snapshot(&anchored).expect("encodes"),
            encode_snapshot(&controller).expect("encodes"),
            "the record of what was sent must encode as the controller's model does"
        );
    }
    use crate::tree::DIGEST_SIZE;

    /// The reading half of an in-memory pipe. Chunks arrive over a channel
    /// and are handed out a slice at a time; a closed channel is
    /// end-of-stream.
    pub(crate) struct PipeReader {
        /// The source of chunks.
        receiver: Receiver<Vec<u8>>,
        /// The chunk currently being consumed.
        buffer: Vec<u8>,
        /// The consumed prefix length of `buffer`.
        offset: usize,
    }

    impl Read for PipeReader {
        fn read(&mut self, target: &mut [u8]) -> std::io::Result<usize> {
            if target.is_empty() {
                return Ok(0);
            }
            while self.offset == self.buffer.len() {
                match self.receiver.recv() {
                    Ok(chunk) => {
                        self.buffer = chunk;
                        self.offset = 0;
                    }
                    Err(_) => return Ok(0),
                }
            }
            let count = std::cmp::min(target.len(), self.buffer.len() - self.offset);
            target[..count].copy_from_slice(&self.buffer[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        }
    }

    /// The writing half of an in-memory pipe. Each write becomes one chunk,
    /// and flushing is a no-op (the channel is never buffered locally).
    pub(crate) struct PipeWriter {
        /// The destination for chunks.
        sender: Sender<Vec<u8>>,
    }

    impl Write for PipeWriter {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.sender
                .send(data.to_vec())
                .map_err(|_| std::io::Error::new(ErrorKind::BrokenPipe, "pipe reader dropped"))?;
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Creates one in-memory pipe.
    pub(crate) fn pipe() -> (PipeReader, PipeWriter) {
        let (sender, receiver) = channel();
        (
            PipeReader {
                receiver,
                buffer: Vec::new(),
                offset: 0,
            },
            PipeWriter { sender },
        )
    }

    /// Creates a connected pair of connections over in-memory pipes, so that
    /// a frame sent on either connection is received on the other.
    pub(crate) fn connected_pair() -> (Connection, Connection) {
        let (first_reader, first_writer) = pipe();
        let (second_reader, second_writer) = pipe();
        (
            Connection::from_streams(Box::new(first_reader), Box::new(second_writer)),
            Connection::from_streams(Box::new(second_reader), Box::new(first_writer)),
        )
    }

    /// The bytes on the wire are pinned, not just the round trip.
    ///
    /// Frame assembly writes the header and the body as two writes out of
    /// reused scratch rather than copying both into one buffer, so a
    /// mistake there would shift the layout while both ends still agreed
    /// with each other — invisible to a round-trip test, fatal against a
    /// peer built from other code. This asserts the exact prefix instead.
    /// A stale agent built from the same package version but an older
    /// compatibility epoch must fail the handshake: safety-relevant
    /// behavior can change without a wire-format change, and "the agent
    /// runs and decodes frames" must not be read as "the agent has the
    /// fixes". The epoch rides in the version string, so the installed
    /// agent's filename changes with it too — the installer never invokes
    /// a stale same-semver binary.
    #[test]
    fn a_stale_epoch_fails_the_handshake() {
        let current = local_handshake();
        verify_handshake(&current).expect("the current version verifies");

        let stale = Handshake {
            magic: protocol::MAGIC,
            version: format!(
                "{}+e{}",
                env!("CARGO_PKG_VERSION"),
                protocol::COMPATIBILITY_EPOCH + 1
            ),
        };
        let error = verify_handshake(&stale).expect_err("a different epoch must fail");
        assert!(
            format!("{error:#}").contains("version mismatch"),
            "{error:#}"
        );

        // And the epoch reaches the installed agent's path.
        assert!(
            crate::transport::install::versioned_remote_command()
                .contains(&format!("+e{}", protocol::COMPATIBILITY_EPOCH)),
            "the install path must carry the epoch"
        );
    }

    #[test]
    fn frame_layout_is_a_length_then_a_flag_then_the_body() {
        // Small frames travel uncompressed: [len(4)][flag=0][bincode].
        let mut wire = Vec::new();
        send_frame(&mut wire, &7u8).expect("unable to send");
        assert_eq!(wire[4], FRAME_UNCOMPRESSED);
        let length = u32::from_le_bytes(wire[..4].try_into().expect("length")) as usize;
        assert_eq!(length, wire.len() - 4, "length must cover flag and body");
        assert_eq!(&wire[5..], &bincode::serialize(&7u8).expect("encode")[..]);

        // Compressible frames above the threshold carry the decompressed
        // length after the flag: [len(4)][flag=1][original(4)][lz4].
        let repetitive = vec![0xABu8; 64 * 1024];
        let mut wire = Vec::new();
        send_frame(&mut wire, &repetitive).expect("unable to send");
        assert_eq!(wire[4], FRAME_COMPRESSED);
        let length = u32::from_le_bytes(wire[..4].try_into().expect("length")) as usize;
        assert_eq!(length, wire.len() - 4);
        let original = u32::from_le_bytes(wire[5..9].try_into().expect("original")) as usize;
        assert_eq!(
            original,
            bincode::serialize(&repetitive).expect("encode").len()
        );
        assert!(wire.len() < repetitive.len(), "compression should have won");

        // And the scratch buffers must not leak between sends: a large
        // frame followed by a small one must produce exactly the small
        // frame, not the tail of its predecessor.
        let mut wire = Vec::new();
        send_frame(&mut wire, &repetitive).expect("unable to send");
        let after_large = wire.len();
        send_frame(&mut wire, &7u8).expect("unable to send");
        let mut expected = Vec::new();
        send_frame(&mut expected, &7u8).expect("unable to send");
        assert_eq!(&wire[after_large..], &expected[..]);
    }

    #[test]
    fn frame_round_trip() {
        let (mut first, mut second) = connected_pair();

        first
            .send(&Request::StageBegin(vec![FileRequest {
                path: "directory/file".into(),
                digest: [7u8; DIGEST_SIZE],
            }]))
            .expect("unable to send");
        match second.receive::<Request>().expect("unable to receive") {
            Request::StageBegin(files) => {
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].path, "directory/file");
                assert_eq!(files[0].digest, [7u8; DIGEST_SIZE]);
            }
            other => panic!("unexpected request: {other:?}"),
        }

        // Frames flow in both directions and back-to-back frames stay
        // aligned.
        second
            .send(&Request::SupplyPull(64))
            .expect("unable to send");
        second.send(&Request::Scan).expect("unable to send");
        assert!(matches!(
            first.receive::<Request>().expect("unable to receive"),
            Request::SupplyPull(64)
        ));
        assert!(matches!(
            first.receive::<Request>().expect("unable to receive"),
            Request::Scan
        ));
    }

    #[test]
    fn frames_compress_transparently() {
        let (mut first, mut second) = connected_pair();

        // A large, compressible payload round-trips (and, at 300KB of
        // repetition, would fail fast if the compressed path were broken).
        let large = "highly repetitive content ".repeat(12_000);
        first.send(&large).expect("unable to send");
        let received: String = second.receive().expect("unable to receive");
        assert_eq!(received, large);

        // A payload below the compression threshold (stored verbatim)
        // round-trips as well.
        first.send(&Request::Scan).expect("unable to send");
        assert!(matches!(
            second.receive::<Request>().expect("unable to receive"),
            Request::Scan
        ));
    }

    #[test]
    fn hostile_decompressed_sizes_are_rejected_before_allocation() {
        let (reader, mut writer) = pipe();
        let (_sink_reader, sink_writer) = pipe();
        let mut connection = Connection::from_streams(Box::new(reader), Box::new(sink_writer));

        // A tiny frame declaring an enormous decompressed size: the reader
        // must reject the declaration, not trust it with an allocation.
        let mut frame = Vec::new();
        frame.push(FRAME_COMPRESSED);
        frame.extend_from_slice(&(protocol::MAXIMUM_FRAME_SIZE + 1).to_le_bytes());
        frame.extend_from_slice(b"junk");
        writer
            .write_all(&(frame.len() as u32).to_le_bytes())
            .expect("unable to write");
        writer.write_all(&frame).expect("unable to write");
        let error = connection
            .receive::<Request>()
            .expect_err("expected a rejection");
        assert!(
            format!("{error:#}").contains("maximum frame size"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn uncompressed_bodies_cannot_ride_the_compression_headroom() {
        // The outer length admits the compressed header's five bytes; an
        // uncompressed body must not be able to use that headroom to exceed
        // the frame cap itself.
        let (reader, mut writer) = pipe();
        let (_sink_reader, sink_writer) = pipe();
        let mut connection = Connection::from_streams(Box::new(reader), Box::new(sink_writer));

        let body_length = protocol::MAXIMUM_FRAME_SIZE as usize + 1;
        let mut frame = Vec::with_capacity(body_length + 1);
        frame.push(FRAME_UNCOMPRESSED);
        frame.resize(body_length + 1, 0);
        writer
            .write_all(&(frame.len() as u32).to_le_bytes())
            .expect("unable to write");
        writer.write_all(&frame).expect("unable to write");
        let error = connection
            .receive::<Request>()
            .expect_err("expected a rejection");
        assert!(
            format!("{error:#}").contains("maximum frame size"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn a_message_larger_than_a_frame_is_split_and_reassembled() {
        // A message past the frame cap used to be refused outright, which
        // made the cap a ceiling on tree size: a staging request or a
        // transition over a large tree is one list. It now crosses as a
        // sequence of frames, each under the cap, that the reader
        // concatenates.
        let (reader, mut writer) = pipe();
        let (_sink_reader, sink_writer) = pipe();
        let mut receiver = Connection::from_streams(Box::new(reader), Box::new(sink_writer));

        // Varied content, so compression cannot hide the size.
        let large: Vec<u8> = (0..(protocol::MAXIMUM_FRAME_SIZE as usize + 12_345))
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        let expected = large.clone();
        let sender = std::thread::spawn(move || {
            send_frame(&mut writer, &large).expect("a large message sends");
        });
        let received: Vec<u8> = receiver.receive().expect("a large message is received");
        sender.join().expect("sender");
        assert_eq!(received.len(), expected.len());
        assert!(received == expected, "the reassembled message differs");
    }

    #[test]
    fn every_frame_of_a_split_message_respects_the_cap() {
        // Capture the raw bytes and walk the frames: none may exceed the
        // per-frame cap, all but the last carry the more-follows bit, and
        // the last does not.
        let large: Vec<u8> = (0..(3 * FRAME_CHUNK_SIZE + 7))
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        let mut wire = Vec::new();
        send_frame(&mut wire, &large).expect("sends");
        let mut offset = 0;
        let mut frames = Vec::new();
        while offset < wire.len() {
            let length = u32::from_le_bytes(wire[offset..offset + 4].try_into().unwrap()) as usize;
            assert!(length <= protocol::MAXIMUM_FRAME_SIZE as usize + COMPRESSED_HEADER_SIZE);
            let flag = wire[offset + 4];
            frames.push(flag & FRAME_MORE != 0);
            offset += 4 + length;
        }
        assert!(
            frames.len() >= 4,
            "expected several frames, got {}",
            frames.len()
        );
        assert!(frames[..frames.len() - 1].iter().all(|more| *more));
        assert!(!frames[frames.len() - 1]);
    }

    #[test]
    fn a_message_truncated_between_frames_is_an_error_not_a_message() {
        let large: Vec<u8> = vec![7u8; 2 * FRAME_CHUNK_SIZE + 1];
        let mut wire = Vec::new();
        send_frame(&mut wire, &large).expect("sends");
        // Cut after the first frame.
        let first_length = u32::from_le_bytes(wire[..4].try_into().unwrap()) as usize;
        let cut = wire[..4 + first_length].to_vec();
        let mut reader = std::io::Cursor::new(cut);
        let error = read_frame(&mut reader).expect_err("a truncated sequence must fail");
        assert!(
            format!("{error:#}").contains("middle of a message"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn oversized_frames_are_rejected_on_receive() {
        let (reader, mut writer) = pipe();
        let (_sink_reader, sink_writer) = pipe();
        let mut connection = Connection::from_streams(Box::new(reader), Box::new(sink_writer));

        // Only the length prefix is written: an honest implementation must
        // reject it without waiting for (or allocating) the body. The limit
        // admits the compression header on top of the frame cap, so the
        // first rejectable length sits just beyond both.
        writer
            .write_all(
                &(protocol::MAXIMUM_FRAME_SIZE + COMPRESSED_HEADER_SIZE as u32 + 1).to_le_bytes(),
            )
            .expect("unable to write");
        let error = connection
            .receive::<Request>()
            .expect_err("expected a rejection");
        assert!(
            format!("{error:#}").contains("maximum frame size"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn closed_connections_report_closure() {
        let (mut first, second) = connected_pair();
        drop(second);
        let error = first
            .receive::<Request>()
            .expect_err("expected a closure error");
        assert!(
            format!("{error:#}").contains("connection closed"),
            "unexpected error: {error:#}"
        );

        // A closure partway through a frame is reported the same way.
        let (reader, mut writer) = pipe();
        let (_sink_reader, sink_writer) = pipe();
        let mut connection = Connection::from_streams(Box::new(reader), Box::new(sink_writer));
        writer
            .write_all(&16u32.to_le_bytes())
            .expect("unable to write");
        drop(writer);
        let error = connection
            .receive::<Request>()
            .expect_err("expected a closure error");
        assert!(
            format!("{error:#}").contains("connection closed"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn handshakes_exchange_and_verify() {
        let (mut first, mut second) = connected_pair();
        let peer = std::thread::spawn(move || -> Result<Handshake> {
            second.send(&local_handshake())?;
            let peer: Handshake = second.receive()?;
            verify_handshake(&peer)?;
            Ok(peer)
        });

        first.send(&local_handshake()).expect("unable to send");
        let received: Handshake = first.receive().expect("unable to receive");
        verify_handshake(&received).expect("unable to verify");
        assert_eq!(received.magic, protocol::MAGIC);
        assert_eq!(received.version, protocol::version());

        let peer = peer
            .join()
            .expect("handshake thread panicked")
            .expect("handshake failed");
        assert_eq!(peer.version, protocol::version());
    }

    #[test]
    fn handshake_verification_rejects_mismatches() {
        let error = verify_handshake(&Handshake {
            magic: protocol::MAGIC,
            version: "0.0.0-ancient".into(),
        })
        .expect_err("expected a version rejection");
        let message = format!("{error:#}");
        assert!(
            message.contains("0.0.0-ancient"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains(&protocol::version()),
            "unexpected error: {message}"
        );

        let error = verify_handshake(&Handshake {
            magic: protocol::MAGIC ^ 0xffff,
            version: protocol::version(),
        })
        .expect_err("expected a magic rejection");
        assert!(
            format!("{error:#}").contains("magic"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn spawned_connections_frame_and_reap() {
        // `cat` echoes the framed bytes back verbatim, exercising spawning,
        // framing over real pipes, and reaping on close (the child exits when
        // its standard input closes).
        let mut connection = Connection::spawn(&["cat".to_owned()]).expect("unable to spawn");
        connection
            .send(&Request::SupplyPull(3))
            .expect("unable to send");
        assert!(matches!(
            connection.receive::<Request>().expect("unable to receive"),
            Request::SupplyPull(3)
        ));
        connection.close().expect("unable to close");
    }

    #[test]
    fn relayed_stderr_is_escaped_capped_and_survives_bad_bytes() {
        let mut input = Vec::new();
        input.extend_from_slice(b"plain\r\n");
        input.extend_from_slice(b"\x1b]52;c;cGF5bG9hZA==\x07\x1b[2J\rsettled\n");
        input.extend_from_slice(b"bad \xff byte\n");
        input.extend_from_slice(&vec![b'a'; 3 * RELAY_LINE_BYTES + 5]);
        input.extend_from_slice(b"\n");
        input.extend_from_slice(&vec![b'b'; RELAY_LINE_BYTES]);
        input.extend_from_slice(b"\nafter\nno newline at the end");
        let mut lines = Vec::new();
        relay_lines(std::io::Cursor::new(input), |line| lines.push(line));

        assert_eq!(lines[0], "plain");
        assert_eq!(lines[1], "\\x1b]52;c;cGF5bG9hZA==\\x07\\x1b[2J\\rsettled");
        assert_eq!(lines[2], "bad \u{fffd} byte");
        // The long line, in capped pieces, all but the last marked.
        let long = &lines[3..7];
        assert!(long[..3]
            .iter()
            .all(|piece| piece.len() == RELAY_LINE_BYTES + "…".len() && piece.ends_with('…')));
        assert_eq!(long[3], "aaaaa");
        // Nothing is lost after the bad byte or the long line.
        // A line of exactly one piece is not marked.
        assert_eq!(lines[7], "b".repeat(RELAY_LINE_BYTES));
        assert_eq!(&lines[8..], ["after", "no newline at the end"]);
        for line in &lines {
            assert!(!line.chars().any(char::is_control), "{line:?}");
        }
    }

    #[test]
    fn ssh_argv_defaults_to_the_agent_command() {
        let argv = Connection::ssh_argv("host", None);
        assert!(argv[0].ends_with("ssh"), "{argv:?}");
        assert!(argv.contains(&"BatchMode=yes".to_owned()));
        assert!(argv.contains(&"ServerAliveInterval=15".to_owned()));
        // The option terminator precedes the host, so a hostile host can't
        // read as an SSH option.
        assert_eq!(&argv[argv.len() - 3..], ["--", "host", "autobahn agent"]);
        // The options that keep a long-lived connection safe and working,
        // whatever ssh_config says, all ahead of the terminator.
        let terminator = argv.iter().position(|word| word == "--").unwrap();
        let options = &argv[..terminator];
        assert!(options.contains(&"-T".to_owned()), "{argv:?}");
        for option in [
            "ForwardAgent=no",
            "ForwardX11=no",
            "ClearAllForwardings=yes",
            "PermitLocalCommand=no",
            "ConnectTimeout=20",
        ] {
            let at = options
                .iter()
                .position(|word| word == option)
                .unwrap_or_else(|| panic!("{option} missing from {argv:?}"));
            assert_eq!(options[at - 1], "-o", "{argv:?}");
        }
        // Host-key checking stays the user's: BatchMode already refuses an
        // unknown host, and accept-new would trust one silently.
        assert!(
            !argv
                .iter()
                .any(|word| word.starts_with("StrictHostKeyChecking")),
            "{argv:?}"
        );

        let argv = Connection::ssh_argv("user@host", Some("/opt/bin/autobahn agent"));
        assert_eq!(
            &argv[argv.len() - 2..],
            ["user@host", "/opt/bin/autobahn agent"]
        );
    }

    #[test]
    fn dropping_an_unclosed_connection_reaps_the_child() {
        // `cat` blocks on its standard input, standing in for an agent (or
        // ssh) process whose handshake never completed. Dropping the
        // connection without close() must terminate and reap it — this is
        // the path taken whenever a connection attempt fails.
        let connection = Connection::spawn(&["cat".to_owned()]).expect("cat should spawn");
        let pid = connection.child_id().expect("the child id should be known") as i32;
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            0,
            "the child should be alive"
        );
        drop(connection);
        // The drop killed and reaped synchronously: the pid no longer
        // refers to a process (or zombie) of ours.
        let alive = unsafe { libc::kill(pid, 0) };
        assert_eq!(alive, -1, "the child should be gone after the drop");
    }

    /// A snapshot too large for one frame crosses the wire as a delta
    /// stream whose every batch fits — the ceiling this transport used to
    /// put on tree size no longer exists.
    #[test]
    fn a_snapshot_larger_than_a_frame_streams_as_a_delta() {
        use crate::tree::{Content, FileMetadata, Node};
        // Long names inflate the encoding without a large tree: a thousand
        // files with 70 KiB names encode past the 64 MiB frame cap.
        let name_length = 70 * 1024;
        let file = |index: usize, digest_byte: u8| Node {
            name: format!("{index:06}{}", "n".repeat(name_length)),
            content: Content::File {
                digest: [digest_byte; 32],
                executable: false,
                metadata: FileMetadata::default(),
            },
        };
        let snapshot_with = |changed: u8| -> Snapshot {
            let children: Vec<Node> = (0..1000)
                .map(|index| file(index, if index == 500 { changed } else { 1 }))
                .collect();
            Snapshot {
                root: Some(Node::directory("", children)),
                files: 1000,
                directories: 1,
                symlinks: 0,
                total_file_size: 0,
                scanned_at_seconds: 0,
                preserves_executability: true,
                mount_points: Vec::new(),
            }
        };
        let first = snapshot_with(1);
        let encoded = encode_snapshot(&first).expect("encodes");
        assert!(
            encoded.len() > protocol::MAXIMUM_FRAME_SIZE as usize,
            "the fixture must exceed the frame cap ({} bytes)",
            encoded.len()
        );

        // Full stream: against nothing.
        let mut pending = std::collections::VecDeque::new();
        let (header, first_encoding) =
            snapshot_delta(&first, None, None, &mut pending, 0).expect("delta");
        assert!(header.baseline.is_none());
        let mut output = Vec::new();
        let mut base = std::io::Cursor::new(Vec::new());
        let signature = crate::rsync::Signature::default();
        let mut batches = 0;
        loop {
            let batch = next_scan_batch(&mut pending);
            if batch.is_empty() {
                break;
            }
            batches += 1;
            let encoded_batch = bincode::serialize(&Response::ScanOps(batch.clone())).unwrap();
            assert!(
                encoded_batch.len() < protocol::MAXIMUM_FRAME_SIZE as usize / 4,
                "a batch must fit a frame with room to spare"
            );
            for op in &batch {
                crate::rsync::patch(&mut base, &signature, op, &mut output).expect("patch");
            }
        }
        assert!(batches > 1, "the stream should have needed several batches");
        assert_eq!(output, encoded);
        assert_eq!(*blake3::hash(&output).as_bytes(), header.digest);

        // Delta: one file changed against the first as baseline. The
        // stream must reproduce the second snapshot, and carry far less
        // data than the encoding — that is the point of the delta.
        let second = snapshot_with(2);
        // Against the first's encoding as kept from its own delta, and as
        // re-encoded: the same delta either way, which is what lets the
        // agent keep it.
        let (header, _) =
            snapshot_delta(&second, Some(&first), None, &mut pending, 0).expect("delta");
        let reencoded: Vec<crate::rsync::Op> = pending.iter().cloned().collect();
        let (kept, _) = snapshot_delta(
            &second,
            Some(&first),
            Some(&first_encoding),
            &mut pending,
            0,
        )
        .expect("delta");
        assert_eq!(format!("{kept:?}"), format!("{header:?}"));
        assert_eq!(format!("{reencoded:?}"), format!("{:?}", pending));
        assert_eq!(header.baseline, Some(*blake3::hash(&encoded).as_bytes()));
        let signature =
            crate::rsync::signature(std::io::Cursor::new(&encoded), header.block_size).unwrap();
        let mut base = std::io::Cursor::new(encoded.clone());
        let mut output = Vec::new();
        let mut data_bytes = 0usize;
        loop {
            let batch = next_scan_batch(&mut pending);
            if batch.is_empty() {
                break;
            }
            for op in &batch {
                if let crate::rsync::Op::Data(data) = op {
                    data_bytes += data.len();
                }
                crate::rsync::patch(&mut base, &signature, op, &mut output).expect("patch");
            }
        }
        assert_eq!(*blake3::hash(&output).as_bytes(), header.digest);
        let decoded: Snapshot = bincode::deserialize(&output).expect("decodes");
        match &decoded.root.unwrap().children()[500].content {
            Content::File { digest, .. } => assert_eq!(*digest, [2u8; 32]),
            other => panic!("expected a file, got {other:?}"),
        }
        assert!(
            data_bytes < encoded.len() / 100,
            "one changed file should cost a sliver of data, not {data_bytes} bytes"
        );
    }

    /// An agent reports a channel only when its work counter moved: never
    /// one that has done nothing, never the same count twice, and never a
    /// channel that has closed.
    #[test]
    fn only_channels_whose_work_moved_are_reported() {
        let busy = std::sync::Arc::new(crate::progress::SideProgress::default());
        let idle = std::sync::Arc::new(crate::progress::SideProgress::default());
        let counters = std::sync::Mutex::new(ChannelCounters::from([
            (1, busy.clone()),
            (2, idle.clone()),
        ]));
        let mut reported = std::collections::HashMap::new();
        assert!(moved_counters(&counters, &mut reported).is_empty());

        busy.pulse();
        let moved = moved_counters(&counters, &mut reported);
        assert_eq!(moved, vec![(1, busy.activity())]);
        assert!(moved_counters(&counters, &mut reported).is_empty());

        // Every kind of counted work moves it.
        for work in [
            &|progress: &crate::progress::SideProgress| progress.advance(1, 0),
            &|progress: &crate::progress::SideProgress| progress.advance(0, 4096),
            &|progress: &crate::progress::SideProgress| progress.change_applied(),
        ] as [&dyn Fn(&crate::progress::SideProgress); 3]
        {
            work(&idle);
            assert_eq!(moved_counters(&counters, &mut reported).len(), 1);
        }

        counters.lock().unwrap().remove(&1);
        busy.pulse();
        assert!(moved_counters(&counters, &mut reported).is_empty());
        assert!(!reported.contains_key(&1));
    }

    /// Through a real agent: a scan that hashes a file counts as work on
    /// its channel, and the agent reports it.
    #[test]
    fn an_agent_reports_the_work_of_a_running_request() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let root = keep.path().join("root");
        std::fs::create_dir_all(&root).expect("root should be creatable");
        std::fs::write(root.join("file"), vec![1u8; 1 << 20]).expect("file should be writable");
        let (mut client, agent) = connected_pair();
        let (agent_reader, agent_writer, _) = agent.into_parts();
        let state = keep.path().join("state");
        let served = std::thread::spawn(move || serve_agent_in(agent_reader, agent_writer, &state));
        client.send(&local_handshake()).expect("handshake");
        let _: Handshake = client.receive().expect("handshake");
        let root_text = root.to_string_lossy().into_owned();
        client
            .send(&protocol::MuxRequest::Open {
                channel: 3,
                initialize: Initialize {
                    session: crate::session::session_identifier(&root_text, "progress-test"),
                    root: root_text,
                    ignores: Vec::new(),
                    symlink_mode: crate::scan::SymlinkMode::Raw,
                    file_mode: None,
                    directory_mode: None,
                    side: "beta".into(),
                    staging: Default::default(),
                    max_file_size: None,
                    max_entry_count: None,
                    ignore_mounts: true,
                    default_owner: None,
                    default_group: None,
                    one_shot: false,
                },
            })
            .expect("open");
        client
            .send(&protocol::MuxRequest::Request {
                channel: 3,
                request: Request::Scan,
            })
            .expect("scan");
        // The open's answer, the scan's, and then — within a report
        // interval or two — the channel's progress.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut progress = None;
        while progress.is_none() && std::time::Instant::now() < deadline {
            match client.receive::<protocol::MuxResponse>().expect("a frame") {
                protocol::MuxResponse::Progress { channel, counter } => {
                    progress = Some((channel, counter))
                }
                protocol::MuxResponse::Response { .. } => {}
            }
        }
        let (channel, counter) = progress.expect("the scan's work is reported");
        assert_eq!(channel, 3);
        assert!(counter > 0);
        client
            .send(&protocol::MuxRequest::Shutdown)
            .expect("shutdown");
        served.join().expect("the agent thread").expect("the agent");
    }

    #[test]
    fn a_long_scan_reports_its_count_and_a_short_one_says_nothing_extra() {
        let decode = |bytes: &[u8]| -> Vec<Response> {
            let mut cursor = std::io::Cursor::new(bytes.to_vec());
            let mut responses = Vec::new();
            while (cursor.position() as usize) < bytes.len() {
                let frame: protocol::MuxResponse = receive_frame(&mut cursor).expect("a frame");
                let protocol::MuxResponse::Response { channel, response } = frame else {
                    panic!("only responses: {frame:?}");
                };
                assert_eq!(channel, 7);
                responses.push(response);
            }
            responses
        };
        let counted = crate::progress::SideProgress::default();

        let output = std::sync::Mutex::new(Vec::<u8>::new());
        let answer = reporting_scan(&output, 7, &counted, || 42);
        assert_eq!(answer, 42);
        assert!(
            output.lock().unwrap().is_empty(),
            "a quick scan sends nothing extra"
        );

        let output = std::sync::Mutex::new(Vec::<u8>::new());
        reporting_scan(&output, 7, &counted, || {
            for _ in 0..12 {
                counted.advance(100, 1_000);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });
        let reports = decode(&output.lock().unwrap());
        assert!(!reports.is_empty(), "{reports:?}");
        let mut last = 0;
        for report in reports {
            let Response::ScanProgress { entries, bytes } = report else {
                panic!("only progress: {report:?}");
            };
            assert!(entries >= last && entries > 0 && bytes == entries * 10);
            last = entries;
        }
    }

    #[test]
    fn exact_changes_reproduce_the_target_encoding() {
        use crate::tree::{apply, Content, FileMetadata, Node};
        use std::sync::Arc;
        let file = |name: &str, byte: u8, mtime: i64| Node {
            name: name.into(),
            content: Content::File {
                digest: [byte; 32],
                executable: false,
                metadata: FileMetadata {
                    mtime_seconds: mtime,
                    size: u64::from(byte),
                    ..FileMetadata::default()
                },
            },
        };
        let shared = Node::directory(
            "shared",
            (0..50).map(|i| file(&format!("s{i:02}"), 1, 5)).collect(),
        );
        let base = Node::directory(
            "",
            vec![
                Node::directory(
                    "d",
                    vec![file("a", 1, 10), file("b", 2, 10), file("c", 3, 10)],
                ),
                file("becomes-dir", 4, 10),
                Node::directory("becomes-file", vec![file("x", 5, 10)]),
                shared.clone(),
                Node {
                    name: "link".into(),
                    content: Content::Symlink {
                        target: "d/a".into(),
                    },
                },
            ],
        );
        let target = Node::directory(
            "",
            vec![
                // a: metadata only (a touch); b: content; c: removed; e: added.
                Node::directory(
                    "d",
                    vec![file("a", 1, 11), file("b", 9, 10), file("e", 6, 10)],
                ),
                Node::directory("becomes-dir", vec![file("y", 7, 10)]),
                file("becomes-file", 8, 10),
                shared,
                Node {
                    name: "link".into(),
                    content: Content::Symlink {
                        target: "d/e".into(),
                    },
                },
                Node {
                    name: "skipped".into(),
                    content: Content::Untracked,
                },
            ],
        );
        let changes = exact_changes(Some(&base), Some(&target));
        // The shared subtree is not walked, and nothing unchanged is sent.
        assert!(
            changes
                .iter()
                .all(|change| !change.path.starts_with("shared")),
            "{changes:?}"
        );
        assert!(changes.iter().all(|change| change.old.is_none()));
        let paths: Vec<&str> = changes.iter().map(|change| change.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "becomes-dir",
                "becomes-file",
                "d/a",
                "d/b",
                "d/c",
                "d/e",
                "link",
                "skipped"
            ]
        );
        let built = apply(Some(&base), &changes).unwrap();
        let snapshot = |root: Option<Node>| Snapshot {
            root,
            files: 9,
            ..Snapshot::default()
        };
        assert_eq!(
            encode_snapshot(&snapshot(built)).unwrap(),
            encode_snapshot(&snapshot(Some(target.clone()))).unwrap(),
            "applying the changes must reproduce the encoding exactly"
        );
        // Identical storage: no changes at all.
        assert!(exact_changes(Some(&target), Some(&target)).is_empty());
        // From nothing, and to nothing: the root itself.
        assert_eq!(exact_changes(None, Some(&target)).len(), 1);
        assert_eq!(exact_changes(Some(&target), None).len(), 1);
        let _ = Arc::<()>::default();
    }
}
