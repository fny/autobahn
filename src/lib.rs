//! Autobahn: fast, safe, SSH-focused bidirectional file synchronization.
//!
//! Autobahn synchronizes a local directory with another directory, either
//! local or reachable over SSH, using three-way reconciliation against a
//! persisted ancestor. Its design distills the lessons of a deep
//! memory/performance overhaul of Mutagen's synchronization engine:
//!
//! - Filesystem hierarchies are modeled as an enum-based node tree with
//!   name-sorted children and inline digests, so traversals are linear merges
//!   and per-entry allocation is minimal.
//! - Scan-time file metadata lives on the nodes themselves (there is no
//!   separate path-keyed cache), so digest-reuse decisions ride the same
//!   lookups as structural reuse, and persisted scan state warms both.
//! - Directory children are shared between snapshot generations via
//!   copy-on-write (`Arc`), so steady-state rescans and ancestor updates
//!   allocate in proportion to change, not tree size.
//! - File contents transfer as rsync-style deltas, streamed and applied
//!   incrementally; nothing buffers whole files or whole deltas in memory.
//! - Remote endpoints run the same binary in agent mode over an SSH (or any
//!   subprocess) byte stream, speaking a framed, version-checked protocol.

#![warn(clippy::empty_line_after_doc_comments)]
#![warn(clippy::doc_lazy_continuation)]

pub mod alerts;
pub mod config;
pub mod endpoint;
pub mod fsutil;
pub mod icon;
pub mod invocation;
pub mod logging;
pub mod ownership;
pub mod paths;
pub mod peering;
pub mod persist;
pub mod progress;
pub mod protocol;
pub mod root;
pub mod rsync;
pub mod scan;
pub mod service;
pub mod session;
pub mod supervisor;
pub mod text;
pub mod threads;
pub mod transport;
#[cfg(feature = "tray")]
pub mod tray;
pub mod tree;
pub mod update;
