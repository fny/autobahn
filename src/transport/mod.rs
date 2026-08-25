//! Byte-stream transports and framing.

use std::io::{Read, Write};
use std::process::Child;

use anyhow::Result;
use serde::de::DeserializeOwned;
use serde::Serialize;

/// A bidirectional byte-stream connection to an agent (typically a child
/// process's stdio: `ssh host autobahn agent` for remote roots, or a direct
/// `autobahn agent` child for testing — the identical code path minus SSH).
pub struct Connection {
    _private: PhantomInner,
}

struct PhantomInner;

impl Connection {
    /// Spawns a command (argv form) and connects to its stdio.
    pub fn spawn(argv: &[String]) -> Result<Connection> {
        todo!("implemented by the transport module")
    }

    /// Builds the argv for an SSH connection to `host` running the remote
    /// agent (`remote_command`, defaulting to `autobahn agent`).
    pub fn ssh_argv(host: &str, remote_command: Option<&str>) -> Vec<String> {
        todo!("implemented by the transport module")
    }

    /// Sends one length-prefixed, bincode-encoded frame.
    pub fn send<T: Serialize>(&mut self, message: &T) -> Result<()> {
        todo!("implemented by the transport module")
    }

    /// Receives one length-prefixed, bincode-encoded frame.
    pub fn receive<T: DeserializeOwned>(&mut self) -> Result<T> {
        todo!("implemented by the transport module")
    }

    /// Terminates the connection (and reaps the child process, if any).
    pub fn close(self) -> Result<()> {
        todo!("implemented by the transport module")
    }
}

/// Runs the agent side of the protocol over the provided streams until the
/// controller disconnects or requests shutdown: handshake, initialization,
/// then a request/response loop dispatching to a local endpoint.
pub fn serve_agent<R: Read, W: Write>(input: R, output: W) -> Result<()> {
    let _ = (has_input(input), has_output(output));
    todo!("implemented by the transport module")
}

fn has_input<R: Read>(_r: R) {}
fn has_output<W: Write>(_w: W) {}
