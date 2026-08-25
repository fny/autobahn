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

use std::io::{ErrorKind, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::endpoint::local::LocalEndpoint;
use crate::endpoint::Endpoint;
use crate::protocol::{self, Handshake, Initialize, Request, Response};
use crate::scan::IgnoreSet;

/// The remote command used by [`Connection::ssh_argv`] when no override is
/// provided.
pub const DEFAULT_REMOTE_COMMAND: &str = "autobahn agent";

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
    /// required.
    ///
    /// [`RemoteEndpoint::connect`]: crate::endpoint::remote::RemoteEndpoint::connect
    pub fn ssh_argv(host: &str, remote_command: Option<&str>) -> Vec<String> {
        vec![
            "ssh".to_owned(),
            "-o".to_owned(),
            "BatchMode=yes".to_owned(),
            host.to_owned(),
            remote_command.unwrap_or(DEFAULT_REMOTE_COMMAND).to_owned(),
        ]
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
    pub fn close(self) -> Result<()> {
        let Connection {
            reader,
            writer,
            child,
        } = self;
        drop(writer);
        drop(reader);
        let Some(mut child) = child else {
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
/// controller going away without a [`Request::Shutdown`]) is a successful
/// exit.
pub fn serve_agent<R: Read, W: Write>(input: R, output: W) -> Result<()> {
    let mut input = input;
    let mut output = output;

    // Exchange handshakes. Ours goes out first so that a version mismatch is
    // diagnosable from either side.
    send_frame(&mut output, &local_handshake()).context("unable to send handshake")?;
    let peer: Handshake = receive_frame(&mut input).context("unable to receive handshake")?;
    verify_handshake(&peer)?;

    // Initialize the endpoint. A failure here is reported to the controller
    // (which would otherwise see only an opaque disconnect) before exiting.
    let initialize: Initialize =
        receive_frame(&mut input).context("unable to receive initialization")?;
    let mut endpoint = match create_endpoint(&initialize) {
        Ok(endpoint) => {
            send_frame(&mut output, &Response::Initialized)
                .context("unable to send initialization response")?;
            endpoint
        }
        Err(error) => {
            send_frame(&mut output, &Response::Error(format!("{error:#}")))
                .context("unable to send initialization failure")?;
            return Ok(());
        }
    };

    // Service requests until shutdown or disconnection.
    loop {
        let request: Request = match read_frame(&mut input)? {
            Some(frame) => bincode::deserialize(&frame).context("unable to decode request")?,
            None => return Ok(()),
        };
        let result = match request {
            Request::Shutdown => return Ok(()),
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
        };
        let response = result.unwrap_or_else(|error| Response::Error(format!("{error:#}")));
        send_frame(&mut output, &response).context("unable to send response")?;
    }
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
    let ignores = IgnoreSet::new(&initialize.ignores).context("unable to compile ignores")?;
    // Expand a home-relative root against this agent's home directory, so
    // that a configuration like `alpha = "~/project"` fanned out to several
    // hosts lands in each host's own home rather than a literal `~`.
    let root = crate::paths::expand_tilde(&initialize.root)?;
    LocalEndpoint::new(root, staging_root, ignores)
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
fn send_frame<W: Write, T: Serialize>(writer: &mut W, message: &T) -> Result<()> {
    let payload = bincode::serialize(message).context("unable to encode frame")?;
    if payload.len() > protocol::MAXIMUM_FRAME_SIZE as usize {
        bail!(
            "outgoing frame of {} bytes exceeds the maximum frame size of {} bytes",
            payload.len(),
            protocol::MAXIMUM_FRAME_SIZE
        );
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
/// which is the agent's normal exit condition).
fn read_frame<R: Read>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut prefix = [0u8; 4];
    match reader.read_exact(&mut prefix) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error).context("unable to read frame length"),
    }
    let length = u32::from_le_bytes(prefix);
    if length > protocol::MAXIMUM_FRAME_SIZE {
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
    Ok(Some(payload))
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
        second.send(&Request::Shutdown).expect("unable to send");
        assert!(matches!(
            first.receive::<Request>().expect("unable to receive"),
            Request::SupplyPull(64)
        ));
        assert!(matches!(
            first.receive::<Request>().expect("unable to receive"),
            Request::Shutdown
        ));
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
        first.send(&Request::Shutdown).expect("unable to send");
        assert!(matches!(
            second.receive::<Request>().expect("unable to receive"),
            Request::Shutdown
        ));
    }

    #[test]
    fn oversized_frames_are_rejected_on_receive() {
        let (reader, mut writer) = pipe();
        let (_sink_reader, sink_writer) = pipe();
        let mut connection = Connection::from_streams(Box::new(reader), Box::new(sink_writer));

        // Only the length prefix is written: an honest implementation must
        // reject it without waiting for (or allocating) the body.
        writer
            .write_all(&(protocol::MAXIMUM_FRAME_SIZE + 1).to_le_bytes())
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
        assert_eq!(
            Connection::ssh_argv("host", None),
            vec!["ssh", "-o", "BatchMode=yes", "host", "autobahn agent"]
        );
        assert_eq!(
            Connection::ssh_argv("user@host", Some("/opt/bin/autobahn agent")),
            vec![
                "ssh",
                "-o",
                "BatchMode=yes",
                "user@host",
                "/opt/bin/autobahn agent"
            ]
        );
    }
}
