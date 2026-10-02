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
pub mod blocked;
/// Photographing the window, for the screenshots that check it.
#[cfg(feature = "dash")]
pub(crate) mod camera;
pub mod config;
/// The window over the fleet: a dashboard, which is what the gauges
/// of a road vehicle have been called since they stopped mud being
/// dashed up by the horses.
#[cfg(feature = "dash")]
pub mod dash;
/// The application's own presence: the dock icon, and whether there is
/// a window, a menu bar item, or both.
#[cfg(feature = "dash")]
pub mod dock;
pub mod endpoint;
pub mod fsutil;
pub mod gate;
pub mod host;
pub mod icon;
pub mod invocation;
pub mod logging;
/// What the menu bar shows, for the tray and for the dash.
#[cfg(any(feature = "tray", feature = "dash"))]
pub(crate) mod menubar;
/// Notifications the system posts, for whoever can post them.
#[cfg(any(feature = "tray", feature = "dash"))]
pub(crate) mod native_alert;
pub mod ownership;
pub mod paths;
pub mod peering;
pub mod peerkeys;
pub mod persist;
pub mod power;
pub mod progress;
pub mod protocol;
pub mod root;
pub mod rsync;
pub mod scan;
pub mod service;
pub mod session;
pub mod supervisor;
/// What a window over the fleet is, apart from its drawing.
#[cfg(feature = "dash")]
pub(crate) mod surface;
pub mod text;
pub mod threads;
pub mod transport;
#[cfg(feature = "tray")]
pub mod tray;
pub mod tree;
pub mod update;
/// What the surfaces say, in one place.
#[cfg(any(feature = "tray", feature = "dash"))]
pub(crate) mod words;
