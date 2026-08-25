//! The local (in-process) endpoint.

use std::path::PathBuf;

use anyhow::Result;

use super::{Endpoint, FileRequest, StagingNeed, TransferFrame, TransitionOutcome};
use crate::scan::IgnoreSet;
use crate::tree::{Change, Snapshot};

/// A local filesystem endpoint.
pub struct LocalEndpoint {
    _private: PhantomInner,
}

struct PhantomInner;

impl LocalEndpoint {
    /// Creates a local endpoint for the specified synchronization root,
    /// with staging state isolated under `staging_root` (which will be
    /// created if needed).
    pub fn new(root: PathBuf, staging_root: PathBuf, ignores: IgnoreSet) -> Result<LocalEndpoint> {
        todo!("implemented by the local endpoint module")
    }
}

impl Endpoint for LocalEndpoint {
    fn scan(&mut self) -> Result<Snapshot> {
        todo!("implemented by the local endpoint module")
    }
    fn stage_begin(&mut self, files: Vec<FileRequest>) -> Result<Vec<StagingNeed>> {
        todo!("implemented by the local endpoint module")
    }
    fn supply_open(&mut self, needs: Vec<StagingNeed>) -> Result<()> {
        todo!("implemented by the local endpoint module")
    }
    fn supply_pull(&mut self, max_frames: usize) -> Result<Vec<TransferFrame>> {
        todo!("implemented by the local endpoint module")
    }
    fn stage_push(&mut self, frames: Vec<TransferFrame>) -> Result<()> {
        todo!("implemented by the local endpoint module")
    }
    fn transition(&mut self, transitions: Vec<Change>) -> Result<TransitionOutcome> {
        todo!("implemented by the local endpoint module")
    }
}
