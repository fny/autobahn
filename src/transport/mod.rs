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
/// stdio), the keepalives bound how long a dead network can hang a
/// synchronous cycle, and `Compression` recovers most of a dedicated
/// compression layer's benefit on the raw stream for free.
pub(crate) fn ssh_options() -> Vec<&'static str> {
    vec![
        "-o",
        "BatchMode=yes",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=4",
        "-o",
        "Compression=yes",
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
        let (command, arguments) = argv
            .split_first()
            .ok_or_else(|| anyhow!("unable to spawn agent: empty command"))?;
        let mut child = Command::new(command)
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
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
            let _ = serve_send(output, channel, Response::Error(format!("{error:#}")));
            return;
        }
    };
    while let Ok(request) = requests.recv() {
        let result = match request {
            Request::Scan => endpoint.scan().map(Response::Scan),
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
                endpoint.transition(transitions).map(Response::Transition)
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
        if let Err(error) = serve_send(output, channel, response) {
            let fallback = Response::Error(format!("unable to send the response: {error:#}"));
            if serve_send(output, channel, fallback).is_err() {
                return;
            }
        }
    }
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

/// Creates the agent's local endpoint from the controller's initialization
/// request. Staging state lives under the agent user's home directory, keyed
/// by session identifier so that concurrent sessions (and interrupted
/// cycles) never share staging space.
fn create_endpoint(initialize: &Initialize) -> Result<LocalEndpoint> {
    let home = std::env::var("HOME")
        .context("unable to determine the agent's home directory (HOME is not set)")?;
    let staging_root = PathBuf::from(home)
        .join(".autobahn")
        .join("staging")
        .join(&initialize.session);
    let options = EndpointOptions {
        ignores: IgnoreSet::new(&initialize.ignores).context("unable to compile ignores")?,
        symlink_mode: initialize.symlink_mode,
        file_mode: initialize.file_mode,
        directory_mode: initialize.directory_mode,
    };
    // Expand a home-relative root against this agent's home directory, so
    // that a configuration like `alpha = "~/project"` fanned out to several
    // hosts lands in each host's own home rather than a literal `~`.
    let root = crate::paths::expand_tilde(&initialize.root)?;
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

/// The encoded size below which compression isn't attempted: tiny frames
/// (bare requests, acknowledgements) can't compress meaningfully and would
/// only pay the header.
const COMPRESSION_THRESHOLD: usize = 256;

/// The bytes the compressed-frame header adds to a payload.
const COMPRESSED_HEADER_SIZE: usize = 5;

fn send_frame<W: Write, T: Serialize>(writer: &mut W, message: &T) -> Result<()> {
    let encoded = bincode::serialize(message).context("unable to encode frame")?;
    if encoded.len() > protocol::MAXIMUM_FRAME_SIZE as usize {
        bail!(
            "outgoing frame of {} bytes exceeds the maximum frame size of {} bytes",
            encoded.len(),
            protocol::MAXIMUM_FRAME_SIZE
        );
    }

    // Compress when it actually helps: the payload carries a flag byte
    // declaring which form it took, so the reader never guesses (and a
    // frame that doesn't shrink — already-compressed file content, mostly —
    // travels verbatim).
    let mut payload = Vec::with_capacity(encoded.len() + COMPRESSED_HEADER_SIZE);
    if encoded.len() >= COMPRESSION_THRESHOLD {
        let compressed = lz4_flex::block::compress(&encoded);
        if compressed.len() + COMPRESSED_HEADER_SIZE < encoded.len() {
            payload.push(FRAME_COMPRESSED);
            payload.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
            payload.extend_from_slice(&compressed);
        }
    }
    if payload.is_empty() {
        payload.push(FRAME_UNCOMPRESSED);
        payload.extend_from_slice(&encoded);
    }

    let length = payload.len() as u32;
    writer
        .write_all(&length.to_le_bytes())
        .context("unable to write frame length")?;
    writer
        .write_all(&payload)
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

    match payload.split_first() {
        Some((&FRAME_UNCOMPRESSED, body)) => {
            // The outer length admits the compressed header's overhead; an
            // uncompressed body must still respect the frame cap itself.
            if body.len() > protocol::MAXIMUM_FRAME_SIZE as usize {
                bail!(
                    "incoming frame of {} bytes exceeds the maximum frame size of {} bytes",
                    body.len(),
                    protocol::MAXIMUM_FRAME_SIZE
                );
            }
            Ok(Some(body.to_vec()))
        }
        Some((&FRAME_COMPRESSED, rest)) => {
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
            Ok(Some(decompressed))
        }
        Some((flag, _)) => bail!("invalid frame flag {flag}"),
        None => bail!("empty frame"),
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
    fn oversized_frames_are_rejected_on_send() {
        let (mut first, mut second) = connected_pair();

        // A payload exactly at the cap still carries a bincode length prefix,
        // putting the encoded frame over it. (A string keeps the encoding a
        // single bulk copy rather than a per-byte sequence.)
        let oversized = "a".repeat(protocol::MAXIMUM_FRAME_SIZE as usize);
        let error = first.send(&oversized).expect_err("expected a rejection");
        assert!(
            format!("{error:#}").contains("maximum frame size"),
            "unexpected error: {error:#}"
        );

        // Nothing was written, so the connection remains usable.
        first.send(&Request::Scan).expect("unable to send");
        assert!(matches!(
            second.receive::<Request>().expect("unable to receive"),
            Request::Scan
        ));
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
}
