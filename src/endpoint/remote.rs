//! The remote endpoint: a client proxy speaking the agent protocol over a
//! transport byte stream.

use anyhow::Result;

use super::{Endpoint, FileRequest, StagingNeed, TransferFrame, TransitionOutcome};
use crate::transport::Connection;
use crate::tree::{Change, Snapshot};

/// A remote endpoint backed by an agent process.
pub struct RemoteEndpoint {
    _private: PhantomInner,
}

struct PhantomInner;

impl RemoteEndpoint {
    /// Establishes a remote endpoint over the provided connection:
    /// exchanges handshakes (enforcing version equality) and initializes
    /// the agent with the root, session identifier, and ignore patterns.
    pub fn connect(
        connection: Connection,
        root: String,
        session: String,
        ignores: Vec<String>,
    ) -> Result<RemoteEndpoint> {
        todo!("implemented by the remote endpoint module")
    }
}

impl Endpoint for RemoteEndpoint {
    fn scan(&mut self) -> Result<Snapshot> {
        todo!("implemented by the remote endpoint module")
    }
    fn stage_begin(&mut self, files: Vec<FileRequest>) -> Result<Vec<StagingNeed>> {
        todo!("implemented by the remote endpoint module")
    }
    fn supply_open(&mut self, needs: Vec<StagingNeed>) -> Result<()> {
        todo!("implemented by the remote endpoint module")
    }
    fn supply_pull(&mut self, max_frames: usize) -> Result<Vec<TransferFrame>> {
        todo!("implemented by the remote endpoint module")
    }
    fn stage_push(&mut self, frames: Vec<TransferFrame>) -> Result<()> {
        todo!("implemented by the remote endpoint module")
    }
    fn transition(&mut self, transitions: Vec<Change>) -> Result<TransitionOutcome> {
        todo!("implemented by the remote endpoint module")
    }
}
