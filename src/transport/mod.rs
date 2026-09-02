//! Byte-stream transports and framing.
//!
//! Frames are the unit of exchange for the agent protocol: a 32-bit
//! little-endian length prefix followed by that many bytes of bincode-encoded
//! payload. The prefix is checked against [`protocol::MAXIMUM_FRAME_SIZE`] in
//! both directions, so a corrupt or adversarial length can never induce a
//! large allocation, and every frame is flushed as soon as it is written (the
//! protocol is strictly request/response, so a buffered frame would deadlock
//! both sides).
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

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::endpoint::local::{EndpointOptions, LocalEndpoint};
use crate::endpoint::Endpoint;
use crate::protocol::{self, Handshake, Initialize, Request, Response};
use crate::scan::IgnoreSet;
use crate::tree::Snapshot;

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
pub(crate) fn ssh_options() -> Vec<&'static str> {
    vec![
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

    /// Spawns as [`spawn`](Connection::spawn) does, but discards the child's
    /// standard error.
    ///
    /// For the *speculative* first connection to a host, whose failure is
    /// the ordinary way a missing agent is discovered: the remote shell's
    /// "no such file or directory" is expected, is followed by an install
    /// and a retry, and printing it makes routine bootstrapping look like a
    /// fault. A failure that is not routine still surfaces, through the
    /// installer's own diagnostics.
    pub fn spawn_quiet(argv: &[String]) -> Result<Connection> {
        Connection::spawn_inner(argv, Stdio::null())
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
        }
    }

    /// Builds the argv for an SSH connection to `host` running the remote
    /// agent (`remote_command`, defaulting to `autobahn agent`).
    ///
    /// The agent is the same binary as the CLI, and it must already be
    /// installed on the remote host and resolvable on the login `PATH` —
    /// autobahn never copies or bootstraps it. Its version must match the
    /// local version exactly; the handshake performed by
    /// [`RemoteEndpoint::connect`] enforces that and reports both versions on
    /// mismatch.
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
    let mut input = input;
    let output = std::sync::Mutex::new(output);

    // Exchange handshakes. Ours goes out first so that a version mismatch is
    // diagnosable from either side.
    {
        let mut output = output.lock().expect("the output lock is never poisoned");
        send_frame(&mut *output, &local_handshake()).context("unable to send handshake")?;
    }
    let peer: Handshake = receive_frame(&mut input).context("unable to receive handshake")?;
    verify_handshake(&peer)?;

    // One connection carries any number of session channels, each served by
    // its own thread over its own endpoint — a channel blocked in a change
    // wait (or a slow transfer) never stalls its siblings. The dispatch
    // below is the only reader; responses interleave through the shared
    // writer, one whole frame at a time.
    std::thread::scope(|scope| -> Result<()> {
        let mut channels: std::collections::HashMap<u32, std::sync::mpsc::Sender<Request>> =
            std::collections::HashMap::new();
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
                        let output = &output;
                        scope.spawn(move || serve_channel(channel, initialize, receiver, output));
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
        result
    })
}

/// Serves one channel: the endpoint is created here (answering the open),
/// then requests are served in order, each answered on the shared writer.
/// The thread ends when the dispatcher drops the channel's sender.
fn serve_channel<W: Write>(
    channel: u32,
    initialize: Initialize,
    requests: std::sync::mpsc::Receiver<Request>,
    output: &std::sync::Mutex<W>,
) {
    // Endpoint creation failures answer on the channel (the controller
    // would otherwise see only silence) without affecting the connection's
    // other channels.
    let mut endpoint = match create_endpoint(&initialize) {
        Ok(endpoint) => {
            if serve_send(output, channel, Response::Initialized).is_err() {
                return;
            }
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
    // The operations of a snapshot delta in flight, drained by ScanPull.
    let mut pending: std::collections::VecDeque<crate::rsync::Op> = Default::default();
    while let Ok(request) = requests.recv() {
        // What becomes of the record of what this channel has transmitted,
        // *if* this response reaches the controller. It is applied only
        // after a successful send: a response that fails to encode or
        // transmit (an oversized frame, say) leaves the controller with its
        // previous model, and recording the new one here would make the
        // next rescan report "unchanged" against a tree it never received.
        let mut anchor = Anchor::Keep;
        let result = match request {
            Request::Scan => endpoint.scan().and_then(|snapshot| {
                // Root identity settles the whole snapshot: its statistics
                // are derived from the hierarchy, leaving only the probed
                // executability behavior to compare alongside it.
                let unchanged = last_sent.as_ref().is_some_and(|sent| {
                    crate::tree::nodes_share_storage(sent.root.as_ref(), snapshot.root.as_ref())
                        && sent.preserves_executability == snapshot.preserves_executability
                });
                if unchanged {
                    return Ok(Response::ScanUnchanged);
                }
                let header = snapshot_delta(&snapshot, last_sent.as_ref(), &mut pending)?;
                anchor = Anchor::To(Some(snapshot));
                Ok(Response::ScanDelta(header))
            }),
            Request::ScanVerified => endpoint.scan_verified().and_then(|snapshot| {
                // Never elided: the entire point is a full re-read whose
                // result the controller sees in full.
                let header = snapshot_delta(&snapshot, last_sent.as_ref(), &mut pending)?;
                anchor = Anchor::To(Some(snapshot));
                Ok(Response::ScanDelta(header))
            }),
            Request::ScanFull => match last_sent.as_ref() {
                // The controller could not reproduce the baseline the last
                // delta named. The snapshot it wants is the one this channel
                // just anchored; it goes again against nothing.
                Some(snapshot) => {
                    snapshot_delta(snapshot, None, &mut pending).map(Response::ScanDelta)
                }
                None => Err(anyhow!("a full scan was requested before any scan")),
            },
            Request::ScanPull => Ok(Response::ScanOps(next_scan_batch(&mut pending))),
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
                    anchor = Anchor::To(endpoint.snapshot().cloned());
                }
                outcome.map(Response::Transition)
            }
            Request::AwaitChanges(milliseconds) => endpoint
                .await_change(std::time::Duration::from_millis(milliseconds))
                .map(Response::AwaitChanges),
        };
        let response = result.unwrap_or_else(|error| Response::Error(format!("{error:#}")));
        // A response can be unsendable for its own reasons (most notably an
        // encoding larger than the frame cap) while the transport is
        // healthy; a small error frame keeps the controller from waiting
        // forever. If even that fails, the connection is gone and the
        // dispatcher is failing with it.
        let delivered = serve_send(output, channel, response);
        match (&delivered, anchor) {
            (Ok(()), Anchor::To(snapshot)) => last_sent = snapshot,
            (Ok(()), Anchor::Keep) => {}
            // Forgetting everything costs one full resend and avoids having
            // to reason about which send failures leave the controller's
            // model intact and which do not. Claiming otherwise is the
            // expensive mistake: it would let a later scan report
            // "unchanged" against a tree that never arrived.
            (Err(_), _) => last_sent = None,
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

/// Prepares a snapshot for transmission as a delta against `baseline` (the
/// snapshot this channel last sent, or `None` for a full stream), leaving
/// the operations queued for `ScanPull` and returning the header.
///
/// The baseline is never held as bytes between scans: it is re-encoded
/// here when needed, which costs one serialization on a changed scan and
/// no memory in between. The header carries the digest of the *new*
/// encoding, so if the controller's re-encoding of its copy of the
/// baseline were ever to differ from this one, the reassembly would fail
/// to verify and be redone in full — determinism of the encoding is a
/// performance assumption, not a correctness one.
fn snapshot_delta(
    snapshot: &Snapshot,
    baseline: Option<&Snapshot>,
    pending: &mut std::collections::VecDeque<crate::rsync::Op>,
) -> Result<protocol::ScanDelta> {
    let target = encode_snapshot(snapshot)?;
    let digest = *blake3::hash(&target).as_bytes();
    let (baseline_digest, signature) = match baseline {
        Some(baseline) => {
            let base = encode_snapshot(baseline)?;
            let block_size = crate::rsync::optimal_block_size(base.len() as u64);
            let signature = crate::rsync::signature(std::io::Cursor::new(&base), block_size)
                .context("unable to sign the baseline snapshot")?;
            (Some(*blake3::hash(&base).as_bytes()), signature)
        }
        None => (None, crate::rsync::Signature::default()),
    };
    pending.clear();
    crate::rsync::deltify(std::io::Cursor::new(&target), &signature, &mut |op| {
        pending.push_back(op);
        Ok(())
    })
    .context("unable to compute the snapshot delta")?;
    Ok(protocol::ScanDelta {
        baseline: baseline_digest,
        digest,
        length: target.len() as u64,
        block_size: signature.block_size,
    })
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
    let mut output = output.lock().expect("the output lock is never poisoned");
    send_frame(&mut *output, &protocol::MuxResponse { channel, response })
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

/// Creates the agent's local endpoint from the controller's initialization
/// request. State-mode staging lives under the agent user's home directory,
/// keyed by session identifier and side so that concurrent sessions (and
/// interrupted cycles, and the two sides of one session) never share
/// staging space; the root-relative placements follow the controller's
/// staging mode.
fn create_endpoint(initialize: &Initialize) -> Result<LocalEndpoint> {
    let home = std::env::var("HOME")
        .context("unable to determine the agent's home directory (HOME is not set)")?;
    // Expand a home-relative root against this agent's home directory, so
    // that a configuration like `alpha = "~/project"` fanned out to several
    // hosts lands in each host's own home rather than a literal `~`.
    let root = crate::paths::expand_tilde(&initialize.root)?;
    let staging_area = PathBuf::from(home).join(".autobahn").join("staging");
    // Earlier versions keyed staging by session alone; such a directory can
    // only belong to this same session under an older agent, so it is
    // retired (best-effort) rather than left to hold stale content forever.
    let _ = std::fs::remove_dir_all(staging_area.join(&initialize.session));
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
    };
    LocalEndpoint::new(root, staging_root, options)
        .with_context(|| format!("unable to create an endpoint for {}", initialize.root))
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

/// Encodes a message and writes it as one length-prefixed frame, flushing so
/// that the peer sees it immediately.
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
const MAXIMUM_MESSAGE_SIZE: usize = 4 * 1024 * 1024 * 1024;

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
    fn ssh_argv_defaults_to_the_agent_command() {
        let argv = Connection::ssh_argv("host", None);
        assert!(argv[0].ends_with("ssh"), "{argv:?}");
        assert!(argv.contains(&"BatchMode=yes".to_owned()));
        assert!(argv.contains(&"ServerAliveInterval=15".to_owned()));
        // The option terminator precedes the host, so a hostile host can't
        // read as an SSH option.
        assert_eq!(&argv[argv.len() - 3..], ["--", "host", "autobahn agent"]);

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
        let header = snapshot_delta(&first, None, &mut pending).expect("delta");
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
        let header = snapshot_delta(&second, Some(&first), &mut pending).expect("delta");
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
}
