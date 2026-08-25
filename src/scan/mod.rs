//! Filesystem scanning.

pub mod ignore;

use std::path::Path;

use anyhow::Result;

use crate::tree::Snapshot;

pub use ignore::IgnoreSet;

/// Scans the filesystem hierarchy at `root`, producing a snapshot.
///
/// A `baseline` (typically the previous scan's snapshot) accelerates the
/// scan: files whose type, mtime, size, and inode match their baseline
/// counterparts reuse the recorded digest instead of being re-read, and
/// unchanged directories share their child storage with the baseline
/// (copy-on-write via `Arc`), so steady-state rescans allocate in proportion
/// to change. The baseline is never trusted structurally — every directory
/// is re-listed and every entry re-stat'd.
///
/// Ignored entries and unsupported filesystem types appear as untracked
/// content; unreadable entries appear as problematic content. A missing root
/// yields a snapshot with no content.
pub fn scan(root: &Path, baseline: Option<&Snapshot>, ignores: &IgnoreSet) -> Result<Snapshot> {
    todo!("implemented by the scan module")
}
