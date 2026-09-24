//! Filesystem scanning.

pub mod ignore;
pub mod ignorefile;
pub mod probes;

use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::io::{ErrorKind, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use crate::tree::{path_join, Content, Digest, FileMetadata, Node, Snapshot};

pub use ignore::IgnoreSet;
pub use probes::{probe, recompose, FilesystemBehavior};

/// The size of the fixed buffer used to stream file contents through the
/// digester.
const DIGEST_BUFFER_SIZE: usize = 64 * 1024;

/// How far past its `lstat` size a file may grow while it is digested
/// before the read is abandoned as racing a writer. Room for an appended
/// log line or two, which is what a live file usually sees in the time one
/// read takes; a file growing faster than that is recorded as changed and
/// read again by the next scan.
const DIGEST_GROWTH_ALLOWANCE: u64 = 1024 * 1024;

/// The problem recorded for a file that was not the same file, or not the
/// same size within [`DIGEST_GROWTH_ALLOWANCE`], by the time it was read.
const CHANGED_DURING_SCAN: &str = "changed during scan; it will be read again";

/// How many entries a scan counts before publishing them. Large enough
/// that the atomic add disappears against the filesystem work, small
/// enough that a reader watching a long scan sees a number that moves.
const PROGRESS_BLOCK: u64 = 512;

/// The most threads one scan spreads over, itself included.
///
/// A full walk is stat-bound on a warm cache and digest-bound on a cold
/// one, and both parallelize by directory: every directory that must be
/// walked is an independent unit whose result slots back into its
/// parent by position, so the hierarchy that comes out is the same
/// hierarchy, in the same order, with the same storage sharing. The cap
/// keeps a wide host from being taken over by one scan; the budget is
/// further cut to the cores actually present.
const SCAN_THREADS_MAX: usize = 8;

#[cfg(test)]
thread_local! {
    /// A test's choice of helper budget for scans on this thread, so the
    /// serial and parallel paths can both be exercised on any machine —
    /// a one-core CI runner otherwise never takes the parallel one.
    static HELPERS: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// A test's action on a path, run by the scan at a chosen moment.
#[cfg(test)]
type PathHook = Box<dyn FnMut(&Path)>;

#[cfg(test)]
thread_local! {
    /// A test's action between a file's `lstat` and its open, on this
    /// thread, to swap something else in under the name.
    static BEFORE_OPEN: std::cell::RefCell<Option<PathHook>> =
        const { std::cell::RefCell::new(None) };
}

/// The deepest a scan descends: a directory this many levels below the
/// root is recorded as a problem rather than walked. Real trees cannot get
/// near it — `PATH_MAX` stops them at about 2,000 levels — but a FUSE
/// filesystem can fake an arbitrarily deep one without any path growing
/// past the limit, and every level costs stack. See [`crate::threads`].
const MAX_SCAN_DEPTH: usize = 4_096;

#[cfg(test)]
thread_local! {
    /// A test's choice of depth cap for scans started on this thread.
    static MAX_DEPTH: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// The depth cap for a scan starting now.
fn max_scan_depth() -> usize {
    #[cfg(test)]
    if let Some(depth) = MAX_DEPTH.with(|depth| depth.get()) {
        return depth;
    }
    MAX_SCAN_DEPTH
}

/// How many threads a scan may add beside the one it runs on.
fn scan_helpers() -> usize {
    #[cfg(test)]
    if let Some(helpers) = HELPERS.with(|helpers| helpers.get()) {
        return helpers;
    }
    std::thread::available_parallelism()
        .map(|cores| cores.get())
        .unwrap_or(1)
        .min(SCAN_THREADS_MAX)
        .saturating_sub(1)
}

/// The file type mask within a raw mode value (`S_IFMT`).
const MODE_TYPE_MASK: u32 = 0o170000;

/// The executability bits within a raw mode value.
const MODE_EXECUTABLE_MASK: u32 = 0o111;

/// The name prefix used by transition staging temporaries, which are
/// invisible to scans.
pub(crate) const TEMPORARY_PREFIX: &str = ".autobahn-tmp";

/// Whether a name is one of autobahn's own temporary names, as opposed to a
/// user's file that merely begins with the reserved prefix.
///
/// The scan used to skip *everything* starting with the prefix, which made
/// a legitimate file named `.autobahn-tmp-notes` permanently invisible: it
/// was never synchronized, never reported, and never conflicted — the two
/// roots could diverge forever while every cycle reported success. Only
/// names matching the grammars autobahn actually generates are its to hide:
///
/// - `.autobahn-tmp-staging-<session>-<side>` — a staging directory placed
///   beside or inside the root;
/// - `.autobahn-tmp-<purpose>-<pid>-<count>[-<token>]` — transition and
///   probe temporaries, always carrying a numeric process id and counter.
///
/// Anything else in the reserved space is surfaced as a scan problem: not
/// silently skipped (the divergence above), and not synchronized either
/// (another process's in-flight temporary must never be transferred).
pub(crate) fn autobahn_temporary(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(TEMPORARY_PREFIX) else {
        return false;
    };
    let Some(rest) = rest.strip_prefix('-') else {
        return false;
    };
    if let Some(staging) = rest.strip_prefix("staging-") {
        return !staging.is_empty();
    }
    let mut parts = rest.splitn(2, '-');
    let purpose = parts.next().unwrap_or("");
    let Some(tail) = parts.next() else {
        return false;
    };
    if purpose.is_empty() || !purpose.bytes().all(|byte| byte.is_ascii_lowercase()) {
        return false;
    }
    let mut fields = tail.split('-');
    let numeric = |field: Option<&str>| {
        field.is_some_and(|field| {
            !field.is_empty() && field.bytes().all(|byte| byte.is_ascii_digit())
        })
    };
    numeric(fields.next()) && numeric(fields.next())
}

/// The suffix appended to the lossy rendering of a non-UTF-8 entry name.
const NON_UTF8_SUFFIX: &str = " (non-UTF-8)";

/// A set of paths whose on-disk state may have changed since the baseline
/// scan, as a trie over path components.
///
/// An *incremental* scan consults this set instead of walking the whole
/// hierarchy: a directory with no marked descendant is adopted from the
/// baseline whole (one `Arc` clone, no `readdir`, no `stat`), so the cost
/// of a scan falls from the size of the tree to the size of what actually
/// changed. Correctness rests on the marks being complete — the caller is
/// responsible for falling back to a full scan whenever they might not be
/// (a watcher that dropped events, a freshly established watch, or simply
/// often enough to bound the damage from a missed notification).
#[derive(Debug, Default)]
pub struct DirtyPaths {
    /// The hierarchy root's node.
    root: DirtyNode,
}

/// One entry in a [`DirtyPaths`] trie.
#[derive(Debug, Default)]
struct DirtyNode {
    /// Whether this directory's entries must be listed again — set on the
    /// *parent* of every marked path, since creation and removal are only
    /// observable by listing, and on the marked path itself.
    relist: bool,
    /// Whether this entry was itself marked. A marked directory may be a
    /// different directory from the baseline's — renamed into place,
    /// removed and made again — and the rename that brought it is the only
    /// event there was: nothing beneath it was reported. So its whole
    /// subtree is read, as a full scan would, and none of it is adopted
    /// unread. (Relisting it alone catches a swap one level deep; a swap
    /// whose names coincide further down would keep the old contents.)
    marked: bool,
    /// Marked entries beneath this one, by name.
    children: std::collections::HashMap<String, DirtyNode>,
}

impl DirtyPaths {
    /// Marks a root-relative path (`""` for the root itself) as changed:
    /// the entry is rescanned — a directory with everything beneath it —
    /// and its parent is listed again so that its creation or removal is
    /// observed.
    pub fn mark(&mut self, path: &str) {
        let mut components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        let name = components.pop();
        let mut node = &mut self.root;
        for component in components {
            node = node.children.entry(component.to_owned()).or_default();
        }
        node.relist = true;
        if let Some(name) = name {
            node = node.children.entry(name.to_owned()).or_default();
            node.relist = true;
        }
        node.marked = true;
    }

    /// Indicates whether nothing at all is marked.
    pub fn is_empty(&self) -> bool {
        !self.root.relist && self.root.children.is_empty()
    }
}

/// State roots registered by this process beyond the default one. See
/// [`exclude_state_root`].
static STATE_ROOTS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Registers a directory holding autobahn's own state — configuration,
/// hooks, journals, locks, installed agents — so that no scan in this
/// process ever descends into it.
///
/// This is the backstop behind planning's refusal of a root that contains
/// the state root. A directory registered here, and always the default
/// state root (`$AUTOBAHN_HOME`, or `~/.autobahn`, which is also an agent's
/// own), scans as untracked wherever it appears inside a root, whatever
/// the ignores say: so it is never synchronized, and never deleted.
/// Synchronizing it would let a peer rewrite this side's configuration or
/// alert hook, and would have a session sync its own journal underneath
/// itself. Matched by device and inode, so an alias or a symbolic link on
/// the way to it makes no difference.
pub fn exclude_state_root(state_root: &Path) {
    let mut roots = STATE_ROOTS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if !roots.iter().any(|root| root == state_root) {
        roots.push(state_root.to_path_buf());
    }
}

/// The device and inode of every state root that exists now.
fn state_root_identities() -> Vec<(u64, u64)> {
    let registered = STATE_ROOTS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    crate::paths::default_state_root()
        .ok()
        .into_iter()
        .chain(registered)
        .filter_map(|root| fs::metadata(root).ok())
        .filter(|metadata| metadata.is_dir())
        .map(|metadata| (metadata.dev(), metadata.ino()))
        .collect()
}

/// The treatment of symbolic links during scanning and transitioning.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, Hash,
)]
pub enum SymlinkMode {
    /// Symbolic links are invisible: never scanned, never propagated.
    Ignore,
    /// Symbolic links are synchronized only when their targets are portable:
    /// relative, colon-free, and confined to the synchronization root. A
    /// link that fails validation is recorded as problematic content.
    Portable,
    /// Symbolic links are synchronized verbatim, targets untouched and
    /// unvalidated (POSIX raw).
    #[default]
    Raw,
}

/// Validates a symbolic link target under [`SymlinkMode::Portable`]: it must
/// be relative, free of colons (which denote drive letters or stream names
/// on the platforms portability targets), and must resolve within the
/// synchronization root from the link's location (`path`, root-relative).
pub fn validate_portable_target(path: &str, target: &str) -> Result<(), String> {
    if target.starts_with('/') {
        return Err("target is absolute".into());
    }
    if target.contains(':') {
        return Err("target contains a colon".into());
    }
    // The link's containing directory sits this many levels below the root.
    let mut depth = path.split('/').count().saturating_sub(1);
    for component in target.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if depth == 0 {
                    return Err("target escapes the synchronization root".into());
                }
                depth -= 1;
            }
            _ => depth += 1,
        }
    }
    Ok(())
}

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
#[allow(clippy::too_many_arguments)] // a scan is configured, not builder-shaped
pub fn scan(
    root: &Path,
    baseline: Option<&Snapshot>,
    ignores: &IgnoreSet,
    behavior: &FilesystemBehavior,
    symlink_mode: SymlinkMode,
    max_file_size: Option<u64>,
    dirty: Option<&DirtyPaths>,
    rehash: bool,
    progress: Option<&crate::progress::SideProgress>,
    ignore_mounts: bool,
) -> Result<Snapshot> {
    // Probe the root without following symbolic links. A missing root isn't
    // an error — it's a legitimate (and common) synchronization state.
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(Snapshot {
                root: None,
                preserves_executability: behavior.preserves_executability,
                ..Snapshot::default()
            });
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("unable to probe synchronization root {}", root.display())
            });
        }
    };
    if !metadata.is_dir() {
        bail!("synchronization root {} is not a directory", root.display());
    }

    // Captured before the walk begins: every digest this scan records was
    // computed no earlier than this, which is what the racy-timestamp rule
    // in `reusable_digest` compares file modification times against.
    let scanned_at_seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0);

    // An incremental scan is only meaningful against a baseline: without
    // one there is nothing to adopt, and everything must be read anyway.
    let baseline_root = baseline.and_then(|s| s.root.as_ref());
    let dirty = dirty.filter(|_| baseline_root.is_some());
    // A scan with no marks to work from reads the whole tree, which is
    // both the slow case and the only one whose running count can be
    // measured against a whole-tree total.
    if let Some(progress) = progress {
        progress.begin(dirty.is_none());
    }
    // One budget of helper threads for the whole walk, taken and returned
    // per directory as subtrees start and finish, so the walk spreads as
    // wide as the tree allows and no wider than the machine does.
    let helpers = AtomicUsize::new(scan_helpers());
    let state_roots = state_root_identities();
    let mut scanner = Scanner::new(
        ignores,
        behavior,
        symlink_mode,
        max_file_size,
        dirty.is_some(),
        baseline.map(|snapshot| snapshot.scanned_at_seconds),
        rehash,
        progress,
        &helpers,
        &state_roots,
    );
    scanner.device = metadata.dev();
    scanner.ignore_mounts = ignore_mounts;
    let content = scanner.scan_directory(root, "", baseline_root, dirty.map(|d| &d.root));
    scanner.publish();
    let root_node = Node {
        name: String::new(),
        content,
    };
    // An incremental scan adopts what it did not revisit — whole subtrees,
    // and single unchanged entries of a directory it did relist — so a
    // mount point it did not probe again is not found again. It is carried
    // from the baseline when its own node was adopted: still left alone
    // (untracked), or, when mounts are followed, the very subtree the
    // baseline held. One probed again and found not to be a mount any more
    // is neither.
    // The racy-timestamp rule judges a digest by when it was recorded. An
    // incremental scan hands on digests it adopted from the baseline, which
    // earlier scans recorded, so its snapshot claims the baseline's start
    // rather than its own: claiming its own let the next scan trust a digest
    // taken while its file was still recently written. Digests this scan
    // did compute are judged more strictly than they need be, which costs
    // at most one re-read of files written since the baseline's scan.
    let recorded_at = match (dirty, baseline) {
        (Some(_), Some(baseline)) => scanned_at_seconds.min(baseline.scanned_at_seconds),
        _ => scanned_at_seconds,
    };
    let mut mount_points = std::mem::take(&mut scanner.mount_points);
    if let Some(baseline) = baseline {
        for point in &baseline.mount_points {
            if mount_points.contains(point) {
                continue;
            }
            let now = crate::tree::node_at(Some(&root_node), point);
            let untracked = matches!(now, Some(node) if matches!(node.content, Content::Untracked));
            let shared = crate::tree::nodes_share_storage(
                now,
                crate::tree::node_at(baseline.root.as_ref(), point),
            );
            if now.is_some() && (untracked || shared) {
                mount_points.push(point.clone());
            }
        }
    }
    mount_points.sort();
    mount_points.dedup();
    let mut snapshot = Snapshot {
        root: Some(root_node),
        mount_points,
        preserves_executability: behavior.preserves_executability,
        directories: scanner.directories,
        files: scanner.files,
        symlinks: scanner.symlinks,
        total_file_size: scanner.total_file_size,
        scanned_at_seconds: recorded_at,
    };
    // An incremental scan only counts what it visited, so the statistics
    // are recomputed from the assembled hierarchy (a pointer walk, with no
    // filesystem access, over a tree that is mostly shared storage).
    if dirty.is_some() {
        recount(&mut snapshot);
    }
    // Every scan's statistics describe the whole tree — an incremental one
    // recounts the assembled hierarchy above — so every scan leaves behind
    // a total for the next full scan to be measured against.
    if let Some(progress) = progress {
        progress.end(Some(
            snapshot.directories + snapshot.files + snapshot.symlinks,
        ));
    }
    Ok(snapshot)
}

/// Recomputes a snapshot's statistics from its hierarchy: every
/// synchronizable directory (the root included), file, and symbolic link.
pub fn recount(snapshot: &mut Snapshot) {
    fn count(node: &Node, tallies: &mut (u64, u64, u64, u64)) {
        match &node.content {
            Content::Directory(children) => {
                tallies.0 += 1;
                for child in children.iter() {
                    count(child, tallies);
                }
            }
            Content::File { metadata, .. } => {
                tallies.1 += 1;
                tallies.3 += metadata.size;
            }
            Content::Symlink { .. } => tallies.2 += 1,
            _ => {}
        }
    }
    let mut tallies = (0, 0, 0, 0);
    if let Some(root) = &snapshot.root {
        count(root, &mut tallies);
    }
    (
        snapshot.directories,
        snapshot.files,
        snapshot.symlinks,
        snapshot.total_file_size,
    ) = tallies;
}

/// The mutable state of a single scan operation: the ignore set being
/// applied, a reusable digest buffer, and the running statistics.
struct Scanner<'a> {
    /// Whether the walk is currently inside a directory the patterns
    /// ignore, entered only because a negation names something below it.
    /// Inside such a region an entry is ignored by virtue of where it is,
    /// unless a negation re-includes it — the patterns cannot be asked
    /// "is this ignored", because nothing matches the contents of a
    /// bare-named directory; their exclusion was the pruning, and the
    /// pruning is what this region replaces. The region ends at the first
    /// re-included directory, so what is brought back is brought back
    /// whole.
    within_ignored: bool,
    /// The ignore set consulted for every entry.
    ignores: &'a IgnoreSet,
    /// The behavior of the filesystem being scanned.
    behavior: &'a FilesystemBehavior,
    /// The treatment of symbolic links.
    symlink_mode: SymlinkMode,
    /// The per-file size limit (`None` for unlimited).
    max_file_size: Option<u64>,
    /// Whether this scan may adopt unmarked baseline content rather than
    /// reading it (set when the caller supplied a set of changed paths).
    incremental: bool,
    /// When the baseline's scan started, for the racy-timestamp rule.
    baseline_scanned_at: Option<i64>,
    /// Whether this scan re-reads every file regardless of metadata — the
    /// verify verb's mode, which makes content changed without its
    /// metadata moving visible.
    rehash: bool,
    /// The digest streaming buffer, allocated once per scan.
    buffer: Vec<u8>,
    /// The number of synchronizable directories scanned.
    directories: u64,
    /// The number of synchronizable files scanned.
    files: u64,
    /// The number of synchronizable symbolic links scanned.
    symlinks: u64,
    /// The total size of synchronizable file content.
    total_file_size: u64,
    /// Where to publish the running counts, when someone is watching.
    progress: Option<&'a crate::progress::SideProgress>,
    /// Entries and bytes counted but not yet published. See
    /// [`counted`](Scanner::counted).
    pending_entries: u64,
    pending_bytes: u64,
    /// Helper threads not currently walking a subtree, shared by every
    /// scanner of one scan. See [`scan_helpers`].
    helpers: &'a AtomicUsize,
    /// The device of the directory being scanned: an entry directory on
    /// another is a mount point.
    device: u64,
    /// Whether a mount point is left alone rather than walked.
    ignore_mounts: bool,
    /// The mount points this scanner visited, root-relative.
    mount_points: Vec<String>,
    /// The device and inode of every state root, never scanned. See
    /// [`exclude_state_root`].
    state_roots: &'a [(u64, u64)],
    /// How many levels below the root the directory being scanned is.
    depth: usize,
    /// The depth past which a directory is not walked. See
    /// [`MAX_SCAN_DEPTH`].
    max_depth: usize,
}

/// What probing a listed entry established.
enum Probed {
    /// Listed a moment ago, gone now.
    Vanished,
    /// Content that needs no further look.
    Settled(Content),
    /// A regular file, with the metadata the probe fetched.
    File(Metadata),
    /// A symbolic link.
    Symlink,
    /// A directory to walk, whether it opens (or continues) an ignored
    /// region, and the device it is on.
    Directory { region: bool, device: u64 },
}

/// One entry of a listing between the two passes of a directory scan.
enum Pending<'n> {
    /// Its node is known.
    Done(Node),
    /// A directory still to be walked, with everything the walk needs.
    Walk {
        name: String,
        entry_path: PathBuf,
        child_path: String,
        baseline: Option<&'n Node>,
        dirty: Option<&'n DirtyNode>,
        region: bool,
        device: u64,
    },
}

impl<'a> Scanner<'a> {
    /// Creates a scanner applying the specified ignore set.
    #[allow(clippy::too_many_arguments)] // a scan is configured, not builder-shaped
    fn new(
        ignores: &'a IgnoreSet,
        behavior: &'a FilesystemBehavior,
        symlink_mode: SymlinkMode,
        max_file_size: Option<u64>,
        incremental: bool,
        baseline_scanned_at: Option<i64>,
        rehash: bool,
        progress: Option<&'a crate::progress::SideProgress>,
        helpers: &'a AtomicUsize,
        state_roots: &'a [(u64, u64)],
    ) -> Scanner<'a> {
        Scanner {
            within_ignored: false,
            ignores,
            behavior,
            symlink_mode,
            max_file_size,
            incremental,
            baseline_scanned_at,
            rehash,
            buffer: vec![0u8; DIGEST_BUFFER_SIZE],
            directories: 0,
            files: 0,
            symlinks: 0,
            total_file_size: 0,
            progress,
            pending_entries: 0,
            pending_bytes: 0,
            helpers,
            device: 0,
            ignore_mounts: false,
            mount_points: Vec::new(),
            state_roots,
            depth: 0,
            max_depth: max_scan_depth(),
        }
    }

    /// A scanner for a subtree, to run on another thread: the same
    /// configuration, its own counters and digest buffer, and the ignored
    /// region the subtree's root is in.
    fn fork(&self, within_ignored: bool) -> Scanner<'a> {
        let mut forked = Scanner::new(
            self.ignores,
            self.behavior,
            self.symlink_mode,
            self.max_file_size,
            self.incremental,
            self.baseline_scanned_at,
            self.rehash,
            self.progress,
            self.helpers,
            self.state_roots,
        );
        forked.within_ignored = within_ignored;
        forked.device = self.device;
        forked.ignore_mounts = self.ignore_mounts;
        forked.max_depth = self.max_depth;
        forked
    }

    /// Folds a finished subtree scanner's counts into this one.
    fn absorb(&mut self, mut forked: Scanner<'a>) {
        forked.publish();
        self.directories += forked.directories;
        self.files += forked.files;
        self.symlinks += forked.symlinks;
        self.total_file_size += forked.total_file_size;
        self.mount_points.append(&mut forked.mount_points);
    }

    /// Claims a helper thread for a subtree, if one is free.
    fn take_helper(&self) -> bool {
        self.helpers
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |free| {
                free.checked_sub(1)
            })
            .is_ok()
    }

    /// Counts one visited entry, and any bytes read for it.
    ///
    /// The counts are accumulated in plain fields and published a block at
    /// a time. An atomic add per entry sounds free and is not: this runs
    /// once per filesystem entry, for a number nobody reads more than once
    /// a second.
    #[inline]
    fn counted(&mut self, bytes: u64) {
        self.pending_entries += 1;
        self.pending_bytes += bytes;
        if self.pending_entries >= PROGRESS_BLOCK {
            self.publish();
        }
    }

    /// Publishes the accumulated counts.
    fn publish(&mut self) {
        let (entries, bytes) = (
            std::mem::take(&mut self.pending_entries),
            std::mem::take(&mut self.pending_bytes),
        );
        if let Some(progress) = self.progress {
            progress.advance(entries, bytes);
        }
    }

    /// Scans the directory at `disk_path`, whose root-relative path is
    /// `path`, using `baseline` (the node observed at the same position by a
    /// previous scan, if any) for digest reuse and structural sharing.
    ///
    /// `dirty` carries the incremental scan's marks for this position:
    /// `None` means nothing beneath this directory changed, so the
    /// baseline's subtree is adopted whole without touching the filesystem.
    fn scan_directory<'n>(
        &mut self,
        disk_path: &Path,
        path: &str,
        baseline: Option<&'n Node>,
        dirty: Option<&'n DirtyNode>,
    ) -> Content {
        if self.depth > self.max_depth {
            return problematic(format!(
                "too deep: more than {} levels below the root",
                self.max_depth
            ));
        }

        // Nothing marked beneath this directory: adopt the baseline whole.
        // This is what makes an incremental scan cost the size of the
        // change rather than the size of the tree.
        if let (Some(baseline), true) = (baseline, self.incremental) {
            if dirty.is_none() {
                if let Content::Directory(_) = &baseline.content {
                    return baseline.content.clone();
                }
            }
        }

        // A directory marked itself may not be the baseline's directory at
        // all, so it is read whole, as a full scan reads it: every entry
        // listed and probed, digests still reused where the metadata
        // allows, unchanged storage still shared.
        if self.incremental && dirty.is_some_and(|node| node.marked) {
            self.incremental = false;
            let content = self.scan_directory(disk_path, path, baseline, None);
            self.incremental = true;
            return content;
        }

        // With the entry list itself unchanged, the baseline's children can
        // be walked directly: only the marked ones are re-examined, and the
        // rest are adopted as they stand. (A decomposing volume is excluded:
        // its on-disk names are NFD while the hierarchy carries NFC, so a
        // disk path cannot be reconstructed from a recorded name.)
        let relist = dirty.map(|node| node.relist).unwrap_or(true)
            || self.behavior.decomposes_unicode
            || !matches!(
                baseline.map(|node| &node.content),
                Some(Content::Directory(_))
            );
        if !relist {
            let baseline = baseline.expect("a non-relisted directory has a baseline");
            let dirty = dirty.expect("a non-relisted directory is marked");
            self.directories += 1;
            self.counted(0);
            let mut children = Vec::with_capacity(baseline.children().len());
            for baseline_child in baseline.children() {
                let Some(child_dirty) = dirty.children.get(&baseline_child.name) else {
                    children.push(baseline_child.clone());
                    continue;
                };
                let child_path = path_join(path, &baseline_child.name);
                let entry_path = disk_path.join(&baseline_child.name);
                if let Some(node) = self.scan_entry(
                    baseline_child.name.clone(),
                    &entry_path,
                    &child_path,
                    Some(baseline_child),
                    Some(child_dirty),
                ) {
                    children.push(node);
                }
            }
            if let Content::Directory(baseline_children) = &baseline.content {
                if adoptable(&children, baseline_children) {
                    return Content::Directory(baseline_children.clone());
                }
            }
            return Content::Directory(Arc::new(children));
        }

        // The full listing is materialized up front so that it can be
        // sorted: the hierarchy model requires name-sorted children, and
        // sorted children are what make baseline lookups and reconciliation
        // linear merges.
        let entries = match read_directory(disk_path) {
            Ok(entries) => entries,
            Err(error) => return problematic(format!("unable to read directory: {error:#}")),
        };
        self.directories += 1;
        self.counted(0);

        // Pass one lists, adopts and probes. A directory that has to be
        // walked is held back rather than walked on the spot, so that once
        // their number is known the walks can be spread over threads.
        let mut children: Vec<Pending<'n>> = Vec::with_capacity(entries.len());
        for (raw_name, entry_path) in entries {
            let lossy_name = raw_name.to_string_lossy();

            // Staging temporaries belong to in-flight transitions, not to
            // the synchronized hierarchy. A user's name that merely enters
            // the reserved space is a problem, not a secret.
            if lossy_name.starts_with(TEMPORARY_PREFIX) {
                if autobahn_temporary(&lossy_name) {
                    continue;
                }
                children.push(Pending::Done(Node {
                    name: lossy_name.into_owned(),
                    content: Content::Problematic {
                        message: format!(
                            "the name prefix '{TEMPORARY_PREFIX}' is reserved for \
                             autobahn temporaries; rename the entry to synchronize it"
                        ),
                    },
                }));
                continue;
            }

            // Names that aren't valid UTF-8 can't be represented in (or
            // transmitted with) the hierarchy model, so they're recorded
            // under a lossy, explicitly marked name.
            let non_utf8 = raw_name.to_str().is_none();
            let name = if non_utf8 {
                format!("{lossy_name}{NON_UTF8_SUFFIX}")
            } else if self.behavior.decomposes_unicode {
                // A decomposing volume stores names NFD; the hierarchy
                // model carries NFC, so recompose on the way in (the
                // creation path writes NFC and lets the volume decompose).
                probes::recompose(&lossy_name)
            } else {
                lossy_name.into_owned()
            };
            let child_path = path_join(path, &name);

            // The baseline is tracked in parallel with the walk, so the
            // counterpart of an entry is one binary search into the current
            // directory's baseline children rather than a walk from the
            // hierarchy root.
            let baseline_child = baseline.and_then(|node| node.child(&name));
            let child_dirty = dirty.and_then(|node| node.children.get(&name));

            // An unmarked entry with usable baseline content is adopted
            // without so much as a stat: the listing established that it
            // still exists, and the marks establish that it hasn't changed.
            // (Problematic content is always retried — its problem may have
            // resolved without any event to announce it.)
            if self.incremental && child_dirty.is_none() && !non_utf8 {
                if let Some(baseline_child) = baseline_child {
                    if !matches!(baseline_child.content, Content::Problematic { .. }) {
                        children.push(Pending::Done(baseline_child.clone()));
                        continue;
                    }
                }
            }

            if non_utf8 {
                // Classification still needs the entry's type for the
                // ignore set, so probe before recording the problem.
                let ignored = fs::symlink_metadata(&entry_path)
                    .map(|metadata| self.entry_ignored(&child_path, metadata.is_dir()))
                    .unwrap_or(false);
                children.push(Pending::Done(Node {
                    name,
                    content: if ignored {
                        Content::Untracked
                    } else {
                        problematic("non-UTF-8 filename")
                    },
                }));
                continue;
            }

            match self.probe_entry(&entry_path, &child_path) {
                Probed::Vanished => {}
                Probed::Settled(content) => children.push(Pending::Done(Node { name, content })),
                Probed::File(metadata) => {
                    let content = self.scan_file(&entry_path, &metadata, baseline_child);
                    children.push(Pending::Done(Node { name, content }));
                }
                Probed::Symlink => {
                    let content = self.scan_symlink(&entry_path, &child_path);
                    children.push(Pending::Done(Node { name, content }));
                }
                Probed::Directory { region, device } => children.push(Pending::Walk {
                    name,
                    entry_path,
                    child_path,
                    baseline: baseline_child,
                    dirty: child_dirty,
                    region,
                    device,
                }),
            }
        }

        // Pass two walks. With two or more subtrees to walk there is
        // something to overlap, and each is handed to a helper thread while
        // one is free, or walked here while none is. Results land by
        // position, so the order of the hierarchy is the listing's, exactly
        // as it would be from a walk on one thread.
        let walks = children
            .iter()
            .filter(|pending| matches!(pending, Pending::Walk { .. }))
            .count();
        let spread = walks >= 2 && self.helpers.load(Ordering::Relaxed) > 0;
        let mut children: Vec<Node> = if !spread {
            children
                .into_iter()
                .map(|pending| match pending {
                    Pending::Done(node) => node,
                    Pending::Walk {
                        name,
                        entry_path,
                        child_path,
                        baseline,
                        dirty,
                        region,
                        device,
                    } => Node {
                        name,
                        content: self.walk(
                            &entry_path,
                            &child_path,
                            baseline,
                            dirty,
                            region,
                            device,
                        ),
                    },
                })
                .collect()
        } else {
            let mut nodes: Vec<Node> = Vec::with_capacity(children.len());
            std::thread::scope(|scope| {
                let mut handles = Vec::new();
                for pending in children {
                    match pending {
                        Pending::Done(node) => nodes.push(node),
                        Pending::Walk {
                            name,
                            entry_path,
                            child_path,
                            baseline,
                            dirty,
                            region,
                            device,
                        } => {
                            if self.take_helper() {
                                let mut forked = self.fork(region);
                                forked.device = device;
                                forked.depth = self.depth + 1;
                                handles.push((
                                    nodes.len(),
                                    crate::threads::spawn_deep_scoped(scope, move || {
                                        let content = forked.scan_directory(
                                            &entry_path,
                                            &child_path,
                                            baseline,
                                            dirty,
                                        );
                                        // The helper is free for the next
                                        // subtree as soon as this one is
                                        // walked, not once it is joined.
                                        forked.helpers.fetch_add(1, Ordering::AcqRel);
                                        (Node { name, content }, forked)
                                    }),
                                ));
                                nodes.push(Node {
                                    name: String::new(),
                                    content: Content::Untracked,
                                });
                            } else {
                                let content = self.walk(
                                    &entry_path,
                                    &child_path,
                                    baseline,
                                    dirty,
                                    region,
                                    device,
                                );
                                nodes.push(Node { name, content });
                            }
                        }
                    }
                }
                for (index, handle) in handles {
                    let (node, forked) = handle.join().expect("scan thread panicked");
                    nodes[index] = node;
                    self.absorb(forked);
                }
            });
            nodes
        };

        // Entries were processed in on-disk name order, which is also the
        // recorded name order except where lossy renaming intervened, so a
        // (stable) re-sort settles those cases. Two distinct on-disk names
        // can also collapse onto one recorded name, in which case the last
        // entry wins.
        children.sort_by(|a, b| a.name.cmp(&b.name));
        let mut unique: Vec<Node> = Vec::with_capacity(children.len());
        for child in children {
            if unique.last().is_some_and(|last| last.name == child.name) {
                unique.pop();
            }
            unique.push(child);
        }

        // If nothing in this directory changed, adopt the baseline's child
        // storage instead of publishing a fresh allocation. Adoption is
        // bottom-up: a subdirectory that adopted its own baseline storage
        // compares pointer-equal here, so an unchanged subtree collapses to
        // a single Arc clone at its top.
        if let Some(Content::Directory(baseline_children)) = baseline.map(|node| &node.content) {
            if adoptable(&unique, baseline_children) {
                return Content::Directory(baseline_children.clone());
            }
        }
        Content::Directory(Arc::new(unique))
    }

    /// Scans one directory entry, returning its node — or `None` when the
    /// entry has vanished since it was listed (or was never there: an
    /// incremental walk of baseline children can reach a removed entry
    /// whose parent listing hasn't been repeated).
    /// Whether an entry is ignored, given where the walk is. Outside an
    /// ignored region the patterns decide as usual. Inside one, the
    /// question inverts: everything is ignored except what a negation
    /// explicitly re-includes.
    fn entry_ignored(&self, child_path: &str, is_directory: bool) -> bool {
        self.ignores
            .ignored_within(child_path, is_directory, self.within_ignored)
    }

    fn scan_entry(
        &mut self,
        name: String,
        entry_path: &Path,
        child_path: &str,
        baseline: Option<&Node>,
        dirty: Option<&DirtyNode>,
    ) -> Option<Node> {
        let content = match self.probe_entry(entry_path, child_path) {
            Probed::Vanished => return None,
            Probed::Settled(content) => content,
            Probed::File(metadata) => self.scan_file(entry_path, &metadata, baseline),
            Probed::Symlink => self.scan_symlink(entry_path, child_path),
            Probed::Directory { region, device } => {
                self.walk(entry_path, child_path, baseline, dirty, region, device)
            }
        };
        Some(Node { name, content })
    }

    /// Probes one directory entry: its type, and whether the ignore set
    /// settles it. What comes back is either settled content, or the kind
    /// of scan the entry still needs.
    fn probe_entry(&mut self, entry_path: &Path, child_path: &str) -> Probed {
        // The entry's type is needed both to dispatch the scan and to
        // resolve directory-only ignore patterns, so it's fetched (without
        // following symbolic links) before anything else.
        let metadata = match fs::symlink_metadata(entry_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Probed::Vanished,
            Err(error) => {
                // Without a type there's nothing to classify, not even for
                // the purposes of the ignore set.
                return Probed::Settled(problematic(format!("unable to probe entry: {error}")));
            }
        };
        let file_type = metadata.file_type();

        // Ignores are consulted before any descent, which is what keeps
        // ignored subtrees from costing anything at all. The exception is
        // a directory that a negation names something inside: pruning
        // there would mean nothing below is ever tested, so the negation
        // could never be consulted. Such a directory is walked instead,
        // as an ignored region, and its contents are decided one by one.
        let is_directory = file_type.is_dir();
        // Autobahn's own state is never scanned, and no negation brings it
        // back.
        if is_directory && self.state_roots.contains(&(metadata.dev(), metadata.ino())) {
            return Probed::Settled(Content::Untracked);
        }
        let Some(ignored) = self
            .ignores
            .traversal(child_path, is_directory, self.within_ignored)
        else {
            return Probed::Settled(Content::Untracked);
        };

        if is_directory {
            // A directory on another device than the one holding it is a
            // mount point: something mounted into the tree, not part of it.
            // Recorded either way, so the session can exclude it on both
            // sides and notice when it goes; with `ignore_mounts` it is
            // left alone like an ignored entry, which is what `rsync -x`,
            // `tar --one-file-system` and `du -x` all do.
            let device = metadata.dev();
            if device != self.device {
                self.mount_points.push(child_path.to_owned());
                if self.ignore_mounts {
                    return Probed::Settled(Content::Untracked);
                }
            }
            // An ignored directory opens a region; a re-included one ends
            // it. Without the second half the negation would bring the
            // directory back but not what is in it, which is not what
            // anyone means by re-including.
            Probed::Directory {
                region: ignored,
                device,
            }
        } else if file_type.is_file() {
            Probed::File(metadata)
        } else if file_type.is_symlink() {
            Probed::Symlink
        } else {
            // Sockets, FIFOs, and device nodes have no portable
            // representation and aren't synchronized.
            Probed::Settled(Content::Untracked)
        }
    }

    /// Walks a subdirectory as part of the ignored region `region` says
    /// it is in, restoring this scanner's own region afterwards.
    fn walk(
        &mut self,
        entry_path: &Path,
        child_path: &str,
        baseline: Option<&Node>,
        dirty: Option<&DirtyNode>,
        region: bool,
        device: u64,
    ) -> Content {
        let (outer, outer_device) = (self.within_ignored, self.device);
        self.within_ignored = region;
        self.device = device;
        self.depth += 1;
        let content = self.scan_directory(entry_path, child_path, baseline, dirty);
        self.depth -= 1;
        self.within_ignored = outer;
        self.device = outer_device;
        content
    }

    /// Scans the file at `disk_path`, whose (already fetched) metadata is
    /// `metadata`, reusing the baseline digest where the metadata proves the
    /// content can't have changed.
    fn scan_file(
        &mut self,
        disk_path: &Path,
        metadata: &Metadata,
        baseline: Option<&Node>,
    ) -> Content {
        let recorded = file_metadata(metadata);

        // A file over the size limit is deliberately excluded from
        // synchronization: it scans as *untracked* content — present, and
        // never mistakable for a deletion — and is never opened or
        // digested. Crossing the limit in either direction just flips this
        // classification on the next scan.
        if let Some(limit) = self.max_file_size {
            if recorded.size > limit {
                return Content::Untracked;
            }
        }

        // The digest is only recomputed when the metadata that would have
        // accompanied it has changed. This is the difference between a scan
        // that reads the whole hierarchy and one that reads only what moved.
        let reusable = if self.rehash {
            None
        } else {
            reusable_digest(baseline, &recorded, self.baseline_scanned_at)
        };
        // Bytes actually read for this file, which is what the scan's
        // running byte count means: a file whose digest was reused cost
        // nothing to read.
        let mut bytes_read = 0;
        let digest = match reusable {
            Some(digest) => digest,
            None => {
                let (digest, read) = match self.digest_file(disk_path, metadata) {
                    Ok(Some(result)) => result,
                    // Rescanned next time, like any problem, rather than
                    // failing this scan.
                    Ok(None) => return problematic(CHANGED_DURING_SCAN),
                    Err(error) => return problematic(format!("unable to read file: {error:#}")),
                };
                // A file that changed size between the stat and the read is
                // being written. The digest describes bytes the recorded
                // metadata does not — and the *original* stat is kept
                // deliberately: the writer's finishing stat will differ
                // from it, forcing a re-read on the next scan. Adopting a
                // fresh stat here recorded the writer's final metadata next
                // to a digest of a prefix, and if the writer finished
                // inside that window the pair validated itself forever —
                // the wrong digest survived every future scan, full scans
                // included, and was persisted into the cache.
                bytes_read = read;
                // Under a verifying scan, a recomputed digest that differs
                // while the metadata matches the baseline exactly is the
                // precise class the metadata gate cannot see — content
                // rewritten with its timestamps restored. Each instance is
                // evidence and is reported loudly.
                if self.rehash {
                    if let Some(Content::File {
                        digest: recorded_digest,
                        metadata: recorded_metadata,
                        ..
                    }) = baseline.map(|node| &node.content)
                    {
                        let metadata_matches = recorded_metadata.mtime_seconds
                            == recorded.mtime_seconds
                            && recorded_metadata.mtime_nanos == recorded.mtime_nanos
                            && recorded_metadata.size == recorded.size
                            && recorded_metadata.inode == recorded.inode;
                        if metadata_matches && *recorded_digest != digest {
                            eprintln!(
                                "verify: {} changed content without its metadata \
                                 moving — invisible to ordinary scans until now",
                                disk_path.display()
                            );
                        }
                    }
                }
                digest
            }
        };

        self.files += 1;
        self.counted(bytes_read);
        self.total_file_size += recorded.size;
        Content::File {
            digest,
            executable: recorded.mode & MODE_EXECUTABLE_MASK != 0,
            metadata: recorded,
        }
    }

    /// Streams the file at `disk_path` through BLAKE3, returning its digest
    /// and the number of bytes read — or `None` when what is there now is
    /// not the file `metadata` (its `lstat`) described, or is being written
    /// faster than it can be read.
    ///
    /// The name is opened without following a symbolic link and without
    /// blocking, and the handle must be the same regular file the `lstat`
    /// saw. So a name swapped in between can neither hang the scan (a
    /// FIFO), feed it forever (a link to `/dev/zero`), nor have a file
    /// outside the root digested and later supplied (a link out).
    fn digest_file(
        &mut self,
        disk_path: &Path,
        metadata: &Metadata,
    ) -> Result<Option<(Digest, u64)>> {
        #[cfg(test)]
        BEFORE_OPEN.with(|hook| {
            if let Some(hook) = hook.borrow_mut().as_mut() {
                hook(disk_path);
            }
        });
        let mut file = match fs::File::options()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(disk_path)
        {
            Ok(file) => file,
            // A symbolic link (`ELOOP`), a socket (`ENXIO`), or nothing at
            // all where a regular file was a moment ago.
            Err(error)
                if error.kind() == ErrorKind::NotFound
                    || matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENXIO)) =>
            {
                return Ok(None);
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("unable to open {}", disk_path.display()))
            }
        };
        let opened = file
            .metadata()
            .with_context(|| format!("unable to probe {}", disk_path.display()))?;
        if !opened.is_file() || opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            return Ok(None);
        }
        let limit = metadata.size().saturating_add(DIGEST_GROWTH_ALLOWANCE);
        let mut hasher = blake3::Hasher::new();
        let mut read = 0u64;
        loop {
            let count = file
                .read(&mut self.buffer)
                .with_context(|| format!("unable to read {}", disk_path.display()))?;
            if count == 0 {
                break;
            }
            hasher.update(&self.buffer[..count]);
            read += count as u64;
            if read > limit {
                return Ok(None);
            }
        }
        Ok(Some((*hasher.finalize().as_bytes(), read)))
    }

    /// Scans the symbolic link at `disk_path` (root-relative path `path`).
    fn scan_symlink(&mut self, disk_path: &Path, path: &str) -> Content {
        // Ignored symbolic links are invisible, exactly like unsupported
        // filesystem types.
        if self.symlink_mode == SymlinkMode::Ignore {
            return Content::Untracked;
        }
        let target = match fs::read_link(disk_path) {
            Ok(target) => target,
            Err(error) => return problematic(format!("unable to read symbolic link: {error}")),
        };
        // Targets are opaque strings: they're recorded exactly as stored,
        // with no normalization, resolution, or rewriting. Portable mode
        // additionally *validates* (but never modifies) them.
        let Some(target) = target.to_str() else {
            return problematic("non-UTF-8 symbolic link target");
        };
        if target.is_empty() {
            return problematic("empty symbolic link target");
        }
        if self.symlink_mode == SymlinkMode::Portable {
            if let Err(message) = validate_portable_target(path, target) {
                return problematic(format!("symbolic link target is not portable: {message}"));
            }
        }
        self.symlinks += 1;
        self.counted(0);
        Content::Symlink {
            target: target.to_owned(),
        }
    }
}

/// Lists a directory, returning its entries' names and paths sorted by name
/// in byte order.
fn read_directory(path: &Path) -> Result<Vec<(OsString, PathBuf)>> {
    let listing =
        fs::read_dir(path).with_context(|| format!("unable to list {}", path.display()))?;
    let mut entries = Vec::new();
    for entry in listing {
        let entry = entry.with_context(|| format!("unable to list {}", path.display()))?;
        entries.push((entry.file_name(), entry.path()));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

/// Extracts the scan-time metadata recorded on file nodes.
fn file_metadata(metadata: &Metadata) -> FileMetadata {
    FileMetadata {
        mtime_seconds: metadata.mtime(),
        mtime_nanos: metadata.mtime_nsec() as u32,
        size: metadata.size(),
        inode: metadata.ino(),
        mode: metadata.mode(),
    }
}

/// The margin, in seconds, by which a file's modification time must
/// predate the recording scan's start before its digest is trusted without
/// a re-read. Two writes inside one mtime granule carry identical
/// timestamps, and granules reach a full second on common network and
/// legacy filesystems (two on FAT); a file modified this close to the scan
/// that digested it may have been rewritten after the read without any
/// metadata moving. The cost of the margin is one re-read, next scan, of
/// exactly the files modified just before this one — the files most worth
/// re-reading.
const RACY_MTIME_MARGIN_SECONDS: i64 = 2;

/// Returns the baseline node's digest if its metadata proves that the file's
/// content matches what was observed at scan time.
fn reusable_digest(
    baseline: Option<&Node>,
    fresh: &FileMetadata,
    baseline_scanned_at: Option<i64>,
) -> Option<Digest> {
    let Some(Content::File {
        digest, metadata, ..
    }) = baseline.map(|node| &node.content)
    else {
        return None;
    };
    // The racy-timestamp rule (git's, transplanted): a digest recorded for
    // a file whose mtime was not comfortably older than the scan that read
    // it cannot prove anything, because a same-granule rewrite after the
    // read is metadata-invisible. `None` (a baseline without a recorded
    // start) and zero (a pre-upgrade cache) both refuse everything, which
    // downgrades once to a full re-read.
    let start = baseline_scanned_at.unwrap_or(0);
    if metadata.mtime_seconds >= start.saturating_sub(RACY_MTIME_MARGIN_SECONDS) {
        return None;
    }
    // Modification time, size, and inode together detect every content
    // change that doesn't deliberately forge them, and the type bits guard
    // against a path having become a different kind of file entirely.
    let unchanged = metadata.mtime_seconds == fresh.mtime_seconds
        && metadata.mtime_nanos == fresh.mtime_nanos
        && metadata.size == fresh.size
        && metadata.inode == fresh.inode
        && metadata.mode & MODE_TYPE_MASK == fresh.mode & MODE_TYPE_MASK;
    unchanged.then_some(*digest)
}

/// Indicates whether or not freshly scanned children are equivalent to their
/// baseline counterparts, and thus whether the baseline's child storage can
/// be adopted in place of the fresh allocation.
///
/// Equivalence is deliberately stricter than content equality: directories
/// must be pointer-equal (having themselves adopted their baseline storage)
/// and files must carry identical scan metadata, so that adoption never
/// discards a fresher observation.
fn adoptable(fresh: &[Node], baseline: &[Node]) -> bool {
    fresh.len() == baseline.len()
        && fresh.iter().zip(baseline.iter()).all(|(new, old)| {
            new.name == old.name
                && match (&new.content, &old.content) {
                    (Content::Directory(a), Content::Directory(b)) => Arc::ptr_eq(a, b),
                    (Content::Directory(_), _) | (_, Content::Directory(_)) => false,
                    (Content::File { metadata: a, .. }, Content::File { metadata: b, .. }) => {
                        a == b && new.content_equal(old, false)
                    }
                    _ => new.content_equal(old, false),
                }
        })
}

/// Creates problematic content with the specified message.
fn problematic(message: impl Into<String>) -> Content {
    Content::Problematic {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use tempfile::{tempdir, TempDir};

    use crate::tree::DIGEST_SIZE;

    fn ignores(patterns: &[&str]) -> IgnoreSet {
        let patterns: Vec<String> = patterns.iter().map(|p| (*p).to_owned()).collect();
        IgnoreSet::new(&patterns).expect("patterns should compile")
    }

    fn write(root: &Path, path: &str, contents: &str) {
        let full = root.join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("parent should be creatable");
        }
        fs::write(&full, contents).expect("file should be writable");
    }

    fn digest_of(contents: &str) -> Digest {
        *blake3::hash(contents.as_bytes()).as_bytes()
    }

    fn child<'a>(node: &'a Node, path: &str) -> &'a Node {
        let mut current = node;
        for component in path.split('/') {
            current = current
                .child(component)
                .unwrap_or_else(|| panic!("{path} should exist"));
        }
        current
    }

    fn file_content(node: &Node, path: &str) -> (Digest, bool) {
        match &child(node, path).content {
            Content::File {
                digest, executable, ..
            } => (*digest, *executable),
            other => panic!("{path} should be a file, found {other:?}"),
        }
    }

    fn children_arc(node: &Node) -> Arc<Vec<Node>> {
        match &node.content {
            Content::Directory(children) => children.clone(),
            other => panic!("expected a directory, found {other:?}"),
        }
    }

    /// Builds a hierarchy exercising every content kind the scanner
    /// produces, returning the (retained) temporary directory.
    fn fixture() -> TempDir {
        let directory = tempdir().expect("temporary directory should be creatable");
        let root = directory.path();
        write(root, "alpha.txt", "alpha");
        write(root, "beta.txt", "beta contents");
        write(root, "nested/inner.txt", "inner");
        write(root, "nested/deeper/leaf.txt", "leaf");
        write(root, "tool.sh", "#!/bin/sh\n");
        fs::set_permissions(root.join("tool.sh"), fs::Permissions::from_mode(0o755))
            .expect("permissions should be settable");
        symlink("alpha.txt", root.join("link")).expect("symlink should be creatable");
        write(root, "excluded/secret.txt", "secret");
        write(root, ".autobahn-tmp-staging-s1-beta", "staging");
        directory
    }

    #[test]
    fn reserved_prefix_hides_only_real_temporaries() {
        // Reproduced before the fix: a user file named `.autobahn-tmp-notes`
        // was silently invisible to every scan — never synchronized, never
        // reported — while the roots diverged.
        assert!(autobahn_temporary(".autobahn-tmp-staging-s1-beta"));
        assert!(autobahn_temporary(".autobahn-tmp-recv-1234-7"));
        assert!(autobahn_temporary(".autobahn-tmp-probe-1234-7-token"));
        assert!(!autobahn_temporary(".autobahn-tmp-notes"));
        assert!(!autobahn_temporary(".autobahn-tmp"));
        assert!(!autobahn_temporary(".autobahn-tmp-"));
        assert!(!autobahn_temporary(".autobahn-tmp-staging-"));
        assert!(!autobahn_temporary(".autobahn-tmp-recv-x-7"));
        assert!(!autobahn_temporary(".autobahn-tmpother"));

        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path();
        write(root, "normal.txt", "hello");
        write(root, ".autobahn-tmp-notes", "user content");
        write(root, ".autobahn-tmp-recv-1-1", "in-flight");
        let snapshot = scan_fixture(root, None);
        let names: Vec<&str> = snapshot
            .root
            .as_ref()
            .expect("root")
            .children()
            .iter()
            .map(|child| child.name.as_str())
            .collect();
        assert!(names.contains(&"normal.txt"));
        assert!(
            names.contains(&".autobahn-tmp-notes"),
            "a user's reserved-prefix name must be visible: {names:?}"
        );
        assert!(
            !names.contains(&".autobahn-tmp-recv-1-1"),
            "a real temporary must stay hidden: {names:?}"
        );
        let notes = snapshot
            .root
            .as_ref()
            .expect("root")
            .child(".autobahn-tmp-notes")
            .expect("present");
        assert!(
            matches!(notes.content, Content::Problematic { .. }),
            "surfaced as a problem, not synchronized"
        );
    }

    /// The racy-timestamp rule: a rewrite that lands in the same mtime
    /// granule as the write a scan digested is metadata-invisible, and the
    /// only defense is refusing to trust digests recorded for files
    /// modified around the scan that read them. Reproduced (as review
    /// finding F2.2) before the rule existed: the second write was never
    /// observed by any scan, full scans included, and a later transition
    /// validated against the poisoned record and overwrote it.
    #[test]
    fn a_same_granule_rewrite_is_reread_not_trusted() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path();
        let moment = std::time::SystemTime::now();
        let set_mtime = |path: &str| {
            fs::File::options()
                .write(true)
                .open(root.join(path))
                .expect("file should open")
                .set_modified(moment)
                .expect("mtime should be settable");
        };

        write(root, "racy.txt", "version-one");
        set_mtime("racy.txt");
        let baseline = scan_fixture(root, None);

        // Same length, same forced mtime: the metadata cannot tell the
        // versions apart. Only the racy rule forces the re-read.
        write(root, "racy.txt", "version-TWO");
        set_mtime("racy.txt");
        let rescan = scan_fixture(root, Some(&baseline));
        assert_eq!(
            file_content(rescan.root.as_ref().expect("root"), "racy.txt").0,
            digest_of("version-TWO"),
            "a same-granule rewrite went unobserved"
        );
    }

    /// An incremental scan hands on digests it adopted, which earlier
    /// scans recorded; the racy-timestamp rule must go on judging them by
    /// when they were recorded. Reproduced before the fix: the incremental
    /// snapshot claimed its own start, a later full scan trusted a digest
    /// recorded while the file's mtime was still recent, and a same-second
    /// rewrite was never seen.
    #[test]
    fn an_adopted_digest_keeps_the_recent_write_rule_of_its_scan() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path();
        let now_seconds = || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_secs()
        };
        let moment = std::time::SystemTime::now() - std::time::Duration::from_secs(1);
        let set_mtime = |path: &str| {
            fs::File::options()
                .write(true)
                .open(root.join(path))
                .expect("file should open")
                .set_modified(moment)
                .expect("mtime should be settable");
        };
        write(root, "racy.txt", "version-one");
        set_mtime("racy.txt");
        let full = scan_fixture(root, None);

        // Same length, same mtime: metadata cannot tell the versions apart.
        write(root, "racy.txt", "version-TWO");
        set_mtime("racy.txt");

        // Late enough that the file's mtime is no longer recent by the
        // clock, so only the recording scan's time protects it.
        let moment_seconds = moment
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        while now_seconds() < moment_seconds + 2 + RACY_MTIME_MARGIN_SECONDS as u64 {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let mut dirty = DirtyPaths::default();
        dirty.mark("elsewhere.txt");
        let incremental = scan(
            root,
            Some(&full),
            &ignores(&["excluded/"]),
            &FilesystemBehavior::default(),
            SymlinkMode::default(),
            None,
            Some(&dirty),
            false,
            None,
            true,
        )
        .expect("incremental scan should succeed");
        let rescan = scan_fixture(root, Some(&incremental));
        assert_eq!(
            file_content(rescan.root.as_ref().expect("root"), "racy.txt").0,
            digest_of("version-TWO"),
            "the full scan trusted a digest recorded while its file was recent"
        );
    }

    /// Backdates a file's modification time so the racy-timestamp rule
    /// does not (correctly) refuse to reuse its digest: these tests are
    /// about metadata *matching*, and a freshly written fixture is exactly
    /// the recently-modified case the rule re-reads on principle.
    fn age(root: &Path, path: &str) {
        let file = fs::File::options()
            .write(true)
            .open(root.join(path))
            .expect("file should open");
        file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(60))
            .expect("mtime should be settable");
    }

    fn scan_fixture(root: &Path, baseline: Option<&Snapshot>) -> Snapshot {
        scan(
            root,
            baseline,
            &ignores(&["excluded/"]),
            &FilesystemBehavior::default(),
            SymlinkMode::default(),
            None,
            None,
            false,
            None,
            true,
        )
        .expect("scan should succeed")
    }

    /// Scans `root` twice from the same baseline — once reading everything,
    /// once consulting only `marks` — and asserts the two agree exactly.
    /// This is the incremental scan's whole contract.
    fn assert_incremental_matches_full(root: &Path, baseline: &Snapshot, marks: &[&str]) {
        let mut dirty = DirtyPaths::default();
        for mark in marks {
            dirty.mark(mark);
        }
        let ignores = ignores(&["excluded/"]);
        let full = scan(
            root,
            Some(baseline),
            &ignores,
            &FilesystemBehavior::default(),
            SymlinkMode::default(),
            None,
            None,
            false,
            None,
            true,
        )
        .expect("full scan should succeed");
        let incremental = scan(
            root,
            Some(baseline),
            &ignores,
            &FilesystemBehavior::default(),
            SymlinkMode::default(),
            None,
            Some(&dirty),
            false,
            None,
            true,
        )
        .expect("incremental scan should succeed");
        assert!(
            full.content_equal(&incremental),
            "incremental scan disagreed with a full scan\nfull: {:#?}\nincremental: {:#?}",
            full.root,
            incremental.root
        );
        assert_eq!(full.files, incremental.files, "file counts differ");
        assert_eq!(
            full.directories, incremental.directories,
            "directory counts differ"
        );
        assert_eq!(full.symlinks, incremental.symlinks, "symlink counts differ");
        assert_eq!(
            full.total_file_size, incremental.total_file_size,
            "sizes differ"
        );
    }

    #[test]
    fn incremental_scans_agree_with_full_scans() {
        let directory = tempdir().expect("temporary directory should be creatable");
        let root = directory.path();
        write(root, "top.txt", "top");
        write(root, "a/one.txt", "one");
        write(root, "a/b/two.txt", "two");
        write(root, "a/b/three.txt", "three");
        write(root, "c/four.txt", "four");
        let baseline = scan_fixture(root, None);

        // A modification deep in the tree.
        write(root, "a/b/two.txt", "two, revised");
        assert_incremental_matches_full(root, &baseline, &["a/b/two.txt"]);

        // A creation (its parent must be listed again to see it).
        write(root, "a/b/new.txt", "new");
        assert_incremental_matches_full(root, &baseline, &["a/b/two.txt", "a/b/new.txt"]);

        // A removal.
        fs::remove_file(root.join("a/b/three.txt")).expect("file should be removable");
        assert_incremental_matches_full(
            root,
            &baseline,
            &["a/b/two.txt", "a/b/new.txt", "a/b/three.txt"],
        );

        // A whole new subtree.
        write(root, "d/deep/deeper/five.txt", "five");
        assert_incremental_matches_full(
            root,
            &baseline,
            &["a/b/two.txt", "a/b/new.txt", "a/b/three.txt", "d"],
        );

        // A directory removed with content beneath it.
        fs::remove_dir_all(root.join("c")).expect("directory should be removable");
        assert_incremental_matches_full(
            root,
            &baseline,
            &["a/b/two.txt", "a/b/new.txt", "a/b/three.txt", "d", "c"],
        );

        // A file replaced by a directory of the same name.
        fs::remove_file(root.join("top.txt")).expect("file should be removable");
        write(root, "top.txt/inside.txt", "inside");
        assert_incremental_matches_full(
            root,
            &baseline,
            &[
                "a/b/two.txt",
                "a/b/new.txt",
                "a/b/three.txt",
                "d",
                "c",
                "top.txt",
            ],
        );
    }

    /// A directory replaced by another under the same name: only the names
    /// the watcher reported are marked, never the contents that arrived
    /// with the rename. Reproduced before the fix: the swapped-in `live`
    /// kept the old `live`'s files until the next full walk.
    #[test]
    fn a_replaced_directory_is_read_again_not_adopted() {
        // An atomic deploy swap, with the same names at every level.
        let directory = tempdir().expect("temporary directory should be creatable");
        let root = directory.path();
        write(root, "live/f1", "old content");
        write(root, "live/sub/f2", "old deeper");
        write(root, "staging/f1", "new content");
        write(root, "staging/sub/f2", "new deeper");
        let baseline = scan_fixture(root, None);
        fs::rename(root.join("live"), root.join("old")).expect("rename");
        fs::rename(root.join("staging"), root.join("live")).expect("rename");
        assert_incremental_matches_full(root, &baseline, &["live", "old", "staging"]);

        // A directory removed, made again, and populated. The population's
        // own events mark the new names; a name the old directory also held
        // is marked by its creation, so only the directory's own mark is
        // asked to cover what vanished.
        let directory = tempdir().expect("temporary directory should be creatable");
        let root = directory.path();
        write(root, "x/keep", "old");
        write(root, "x/gone", "old");
        write(root, "x/sub/deep", "old");
        let baseline = scan_fixture(root, None);
        fs::remove_dir_all(root.join("x")).expect("remove");
        fs::create_dir(root.join("x")).expect("mkdir");
        write(root, "x/keep", "new");
        write(root, "x/sub/deep", "new");
        assert_incremental_matches_full(root, &baseline, &["x", "x/keep", "x/sub"]);

        // A directory renamed over an empty one.
        let directory = tempdir().expect("temporary directory should be creatable");
        let root = directory.path();
        write(root, "full/f1", "content");
        write(root, "full/sub/f2", "content");
        fs::create_dir(root.join("empty")).expect("mkdir");
        let baseline = scan_fixture(root, None);
        fs::rename(root.join("full"), root.join("empty")).expect("rename");
        assert_incremental_matches_full(root, &baseline, &["full", "empty"]);
    }

    /// Swaps at every depth of a small tree, one after another, each
    /// checked against a full scan. The rename is the whole change: no
    /// event names anything inside the directories that moved.
    #[test]
    fn directory_swaps_at_any_depth_agree_with_full_scans() {
        let directory = tempdir().expect("temporary directory should be creatable");
        let root = directory.path();
        let levels = ["", "a", "a/b", "a/b/c"];
        for (round, level) in levels.iter().enumerate() {
            let at = |name: &str| path_join(level, name);
            write(root, &at("one/f"), &format!("one {round}"));
            write(root, &at("one/deeper/g"), &format!("one deeper {round}"));
            write(root, &at("two/f"), &format!("two {round}!"));
            write(root, &at("two/deeper/g"), &format!("two deeper {round}!"));
            let baseline = scan_fixture(root, None);
            fs::rename(root.join(at("one")), root.join(at("spare"))).expect("rename");
            fs::rename(root.join(at("two")), root.join(at("one"))).expect("rename");
            fs::rename(root.join(at("spare")), root.join(at("two"))).expect("rename");
            assert_incremental_matches_full(
                root,
                &baseline,
                &[&at("one"), &at("two"), &at("spare")],
            );
        }
    }

    /// Scans `root` with `action` run between each file's `lstat` and its
    /// open, on a thread of its own so that a scan that hangs is reported
    /// rather than hanging the suite.
    fn scan_with_swap(root: &Path, action: impl FnMut(&Path) + Send + 'static) -> Option<Snapshot> {
        let root = root.to_path_buf();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            HELPERS.with(|helpers| helpers.set(Some(0)));
            BEFORE_OPEN.with(|hook| *hook.borrow_mut() = Some(Box::new(action)));
            let _ = sender.send(scan_fixture(&root, None));
        });
        receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .ok()
    }

    /// Reproduced before the fix: a FIFO swapped in after the `lstat`
    /// blocked the open forever, and the session never finished a scan.
    #[test]
    fn a_file_swapped_for_a_fifo_does_not_hang_the_scan() {
        let directory = tempdir().expect("temporary directory should be creatable");
        let root = directory.path().join("root");
        write(&root, "file.txt", "contents");
        let fifo = root.join("file.txt");
        let snapshot = scan_with_swap(&root, move |path| {
            if path == fifo {
                fs::remove_file(path).expect("remove");
                let name =
                    std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("path");
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0, "mkfifo");
            }
        });
        let Some(snapshot) = snapshot else {
            // Unblock the stuck open so the thread can end.
            let _ = fs::OpenOptions::new()
                .write(true)
                .open(root.join("file.txt"));
            panic!("the scan hung on a FIFO");
        };
        let root_node = snapshot.root.as_ref().expect("root");
        match &child(root_node, "file.txt").content {
            Content::Problematic { message } => assert!(
                message.contains("changed during scan"),
                "unexpected message: {message}"
            ),
            other => panic!("expected a changed-during-scan entry, found {other:?}"),
        }
    }

    /// Reproduced before the fix: the open followed a symbolic link
    /// swapped in after the `lstat`, and a file outside the root was
    /// digested — and later supplied to the peer.
    #[test]
    fn a_file_swapped_for_a_symlink_outside_is_not_digested() {
        let directory = tempdir().expect("temporary directory should be creatable");
        let root = directory.path().join("root");
        write(&root, "file.txt", "contents");
        write(directory.path(), "outside.txt", "secret");
        let outside = directory.path().join("outside.txt");
        let target = root.join("file.txt");
        let snapshot = scan_with_swap(&root, move |path| {
            if path == target {
                fs::remove_file(path).expect("remove");
                symlink(&outside, path).expect("symlink");
            }
        })
        .expect("the scan should finish");
        let root_node = snapshot.root.as_ref().expect("root");
        match &child(root_node, "file.txt").content {
            Content::File { digest, .. } => assert_ne!(
                *digest,
                digest_of("secret"),
                "a file outside the root was digested"
            ),
            Content::Problematic { message } => assert!(
                message.contains("changed during scan"),
                "unexpected message: {message}"
            ),
            other => panic!("unexpected content {other:?}"),
        }
    }

    /// A file growing well past its `lstat` size while it is read is being
    /// written: the scan stops reading and records it as changed, rather
    /// than digesting whatever the writer has produced so far — or, for a
    /// file that never stops growing, reading forever.
    #[test]
    fn a_file_that_grows_during_the_read_is_changed_during_scan() {
        let directory = tempdir().expect("temporary directory should be creatable");
        let root = directory.path().join("root");
        write(&root, "file.txt", "small");
        let target = root.join("file.txt");
        let snapshot = scan_with_swap(&root, move |path| {
            if path == target {
                use std::io::Write;
                let mut file = fs::OpenOptions::new()
                    .append(true)
                    .open(path)
                    .expect("open");
                file.write_all(&vec![b'x'; 4 * 1024 * 1024])
                    .expect("append");
            }
        })
        .expect("the scan should finish");
        let root_node = snapshot.root.as_ref().expect("root");
        match &child(root_node, "file.txt").content {
            Content::Problematic { message } => assert!(
                message.contains("changed during scan"),
                "unexpected message: {message}"
            ),
            other => panic!("expected a changed-during-scan entry, found {other:?}"),
        }
    }

    /// The backstop behind planning's refusal: a state root inside a
    /// synchronization root is never scanned, whatever the ignores say,
    /// so it is never synchronized and never deleted.
    #[test]
    fn a_state_root_inside_the_root_is_untracked() {
        let directory = tempdir().expect("temporary directory should be creatable");
        let root = directory.path().join("root");
        write(&root, "file.txt", "synchronized");
        write(&root, "nested/state/config.toml", "agent_command = 'evil'");
        write(&root, "nested/state/sessions/journal", "journal");
        exclude_state_root(&root.join("nested/state"));

        // A negation naming something inside does not bring it back.
        let snapshot = scan(
            &root,
            None,
            &ignores(&["nested/state", "!nested/state/config.toml"]),
            &FilesystemBehavior::default(),
            SymlinkMode::default(),
            None,
            None,
            false,
            None,
            true,
        )
        .expect("scan should succeed");
        let root_node = snapshot.root.as_ref().expect("root");
        assert!(matches!(
            child(root_node, "file.txt").content,
            Content::File { .. }
        ));
        assert!(
            matches!(child(root_node, "nested/state").content, Content::Untracked),
            "the state root was scanned: {:?}",
            child(root_node, "nested/state").content
        );

        // Nor does an alias: identity is the directory, not its spelling.
        let other = directory.path().join("other");
        write(&other, "file.txt", "x");
        write(&other, "state/config.toml", "x");
        let alias = directory.path().join("alias");
        symlink(other.join("state"), &alias).expect("symlink");
        exclude_state_root(&alias);
        let snapshot = scan_fixture(&other, None);
        assert!(matches!(
            child(snapshot.root.as_ref().expect("root"), "state").content,
            Content::Untracked
        ));
    }

    /// The backstop against a tree deeper than any stack should be asked
    /// to walk — a FUSE filesystem can fake one without any path growing
    /// past its limit: past the cap, a directory is a problem, not a
    /// descent. The cap is lowered for the test; the real one sits past
    /// what `PATH_MAX` lets a real tree reach.
    #[test]
    fn a_directory_past_the_depth_cap_is_problematic() {
        let directory = tempdir().expect("temporary directory should be creatable");
        let root = directory.path();
        write(root, "a/b/c/d/e/f.txt", "deep");
        write(root, "a/shallow.txt", "shallow");
        MAX_DEPTH.with(|depth| depth.set(Some(3)));
        for helpers in [0, 4] {
            HELPERS.with(|cell| cell.set(Some(helpers)));
            let snapshot = scan_fixture(root, None);
            let root_node = snapshot.root.as_ref().expect("root");
            assert!(matches!(
                child(root_node, "a/shallow.txt").content,
                Content::File { .. }
            ));
            assert!(matches!(
                child(root_node, "a/b/c").content,
                Content::Directory(_)
            ));
            match &child(root_node, "a/b/c/d").content {
                Content::Problematic { message } => {
                    assert!(
                        message.contains("too deep"),
                        "unexpected message: {message}"
                    )
                }
                other => panic!("expected a problem past the cap, found {other:?}"),
            }
        }
        MAX_DEPTH.with(|depth| depth.set(None));
        HELPERS.with(|cell| cell.set(None));
    }

    #[test]
    fn an_unmarked_tree_is_adopted_whole() {
        let directory = tempdir().expect("temporary directory should be creatable");
        let root = directory.path();
        write(root, "a/one.txt", "one");
        write(root, "a/b/two.txt", "two");
        let baseline = scan_fixture(root, None);

        // With nothing marked, an incremental scan reproduces the baseline
        // without reading the filesystem at all — so even a change made
        // behind its back is invisible (which is exactly why the caller
        // must fall back to full scans when its marks may be incomplete).
        write(root, "a/b/two.txt", "changed behind the scan's back");
        let mut dirty = DirtyPaths::default();
        dirty.mark("unrelated/elsewhere.txt");
        let incremental = scan(
            root,
            Some(&baseline),
            &ignores(&[]),
            &FilesystemBehavior::default(),
            SymlinkMode::default(),
            None,
            Some(&dirty),
            false,
            None,
            true,
        )
        .expect("incremental scan should succeed");
        assert!(incremental.content_equal(&baseline));
        // The adopted subtree is the baseline's own storage, not a copy.
        let (Some(before), Some(after)) = (&baseline.root, &incremental.root) else {
            panic!("both scans should have roots");
        };
        let (Content::Directory(before), Content::Directory(after)) =
            (&before.content, &after.content)
        else {
            panic!("both roots should be directories");
        };
        assert!(Arc::ptr_eq(before, after));
    }

    #[test]
    fn symlink_modes_govern_scanning() {
        let directory = tempdir().expect("temporary directory should be creatable");
        let root = directory.path();
        write(root, "file.txt", "content");
        symlink("file.txt", root.join("relative")).expect("symlink should be creatable");
        symlink("/etc/passwd", root.join("absolute")).expect("symlink should be creatable");
        symlink("../escape", root.join("escaping")).expect("symlink should be creatable");

        let scan_with = |mode: SymlinkMode| {
            scan(
                root,
                None,
                &ignores(&[]),
                &FilesystemBehavior::default(),
                mode,
                None,
                None,
                false,
                None,
                true,
            )
            .expect("scan should succeed")
        };

        // Raw records everything verbatim.
        let snapshot = scan_with(SymlinkMode::Raw);
        assert_eq!(snapshot.symlinks, 3);

        // Ignore makes symbolic links invisible (untracked).
        let snapshot = scan_with(SymlinkMode::Ignore);
        assert_eq!(snapshot.symlinks, 0);
        let root_node = snapshot.root.as_ref().expect("root should exist");
        assert!(matches!(
            root_node.child("relative").expect("recorded").content,
            Content::Untracked
        ));

        // Portable records only validated targets; the rest are problems.
        let snapshot = scan_with(SymlinkMode::Portable);
        assert_eq!(snapshot.symlinks, 1);
        let root_node = snapshot.root.as_ref().expect("root should exist");
        assert!(matches!(
            root_node.child("relative").expect("recorded").content,
            Content::Symlink { .. }
        ));
        for name in ["absolute", "escaping"] {
            assert!(
                matches!(
                    root_node.child(name).expect("recorded").content,
                    Content::Problematic { .. }
                ),
                "{name} should be problematic"
            );
        }
    }

    #[test]
    fn portable_target_validation() {
        assert!(validate_portable_target("link", "sibling.txt").is_ok());
        assert!(validate_portable_target("dir/link", "../top.txt").is_ok());
        assert!(validate_portable_target("dir/link", "sub/./inner").is_ok());
        assert!(validate_portable_target("link", "/absolute").is_err());
        assert!(validate_portable_target("link", "../escapes").is_err());
        assert!(validate_portable_target("dir/link", "../../escapes").is_err());
        assert!(validate_portable_target("link", "c:drive").is_err());
    }

    #[test]
    fn decomposing_volumes_yield_recomposed_names() {
        // The test filesystem stores bytes verbatim, so an NFD name written
        // here simulates exactly what a decomposing volume would report.
        let directory = tempdir().expect("temporary directory should be creatable");
        write(directory.path(), "cafe\u{0301}.txt", "decomposed on disk");
        let behavior = FilesystemBehavior {
            decomposes_unicode: true,
            ..FilesystemBehavior::default()
        };
        let snapshot = scan(
            directory.path(),
            None,
            &ignores(&[]),
            &behavior,
            SymlinkMode::default(),
            None,
            None,
            false,
            None,
            true,
        )
        .expect("scan should succeed");
        let root = snapshot.root.expect("root should exist");
        assert!(root.child("caf\u{00E9}.txt").is_some(), "{root:?}");
        assert!(root.child("cafe\u{0301}.txt").is_none());

        // Without the flag, names pass through byte-exact.
        let snapshot = scan(
            directory.path(),
            None,
            &ignores(&[]),
            &FilesystemBehavior::default(),
            SymlinkMode::default(),
            None,
            None,
            false,
            None,
            true,
        )
        .expect("scan should succeed");
        let root = snapshot.root.expect("root should exist");
        assert!(root.child("cafe\u{0301}.txt").is_some());
    }

    #[test]
    fn missing_root_yields_empty_snapshot() {
        let directory = tempdir().expect("temporary directory should be creatable");
        let snapshot = scan(
            &directory.path().join("absent"),
            None,
            &ignores(&[]),
            &FilesystemBehavior::default(),
            SymlinkMode::default(),
            None,
            None,
            false,
            None,
            true,
        )
        .expect("a missing root is not an error");
        assert!(snapshot.root.is_none());
        assert!(snapshot.preserves_executability);
        assert_eq!(snapshot.directories, 0);
        assert_eq!(snapshot.files, 0);
        assert_eq!(snapshot.symlinks, 0);
        assert_eq!(snapshot.total_file_size, 0);
    }

    #[test]
    fn non_directory_root_is_rejected() {
        let directory = tempdir().expect("temporary directory should be creatable");
        write(directory.path(), "file.txt", "contents");
        assert!(scan(
            &directory.path().join("file.txt"),
            None,
            &ignores(&[]),
            &FilesystemBehavior::default(),
            SymlinkMode::default(),
            None,
            None,
            false,
            None,
            true,
        )
        .is_err());
    }

    #[test]
    fn scans_a_mixed_hierarchy() {
        let directory = fixture();
        let snapshot = scan_fixture(directory.path(), None);
        let root = snapshot.root.as_ref().expect("root should exist");
        assert_eq!(root.name, "");
        assert!(root.validate(false).is_ok());

        // Children are name-sorted, and the staging temporary is invisible.
        let names: Vec<&str> = root.children().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "alpha.txt",
                "beta.txt",
                "excluded",
                "link",
                "nested",
                "tool.sh"
            ]
        );

        // Files carry correct digests and executability.
        assert_eq!(file_content(root, "alpha.txt"), (digest_of("alpha"), false));
        assert_eq!(
            file_content(root, "nested/deeper/leaf.txt"),
            (digest_of("leaf"), false)
        );
        assert_eq!(
            file_content(root, "tool.sh"),
            (digest_of("#!/bin/sh\n"), true)
        );

        // Symbolic link targets are stored verbatim.
        match &child(root, "link").content {
            Content::Symlink { target } => assert_eq!(target, "alpha.txt"),
            other => panic!("expected a symlink, found {other:?}"),
        }

        // Ignored directories are untracked and never descended.
        let excluded = child(root, "excluded");
        assert!(matches!(excluded.content, Content::Untracked));
        assert!(excluded.children().is_empty());

        // File metadata is recorded for digest reuse.
        match &child(root, "alpha.txt").content {
            Content::File { metadata, .. } => {
                assert_eq!(metadata.size, 5);
                assert_ne!(metadata.inode, 0);
                assert_eq!(metadata.mode & MODE_TYPE_MASK, 0o100000);
            }
            other => panic!("expected a file, found {other:?}"),
        }
    }

    #[test]
    fn statistics_count_synchronizable_content() {
        let directory = fixture();
        let snapshot = scan_fixture(directory.path(), None);
        // The root, "nested", and "nested/deeper" — the ignored directory is
        // untracked and therefore uncounted.
        assert_eq!(snapshot.directories, 3);
        assert_eq!(snapshot.files, 5);
        assert_eq!(snapshot.symlinks, 1);
        assert_eq!(
            snapshot.total_file_size,
            ("alpha".len()
                + "beta contents".len()
                + "inner".len()
                + "leaf".len()
                + "#!/bin/sh\n".len()) as u64
        );
        assert!(snapshot.preserves_executability);
    }

    #[test]
    fn unchanged_rescan_adopts_the_entire_hierarchy() {
        let directory = fixture();
        let baseline = scan_fixture(directory.path(), None);
        let rescan = scan_fixture(directory.path(), Some(&baseline));

        let baseline_root = baseline.root.as_ref().expect("root should exist");
        let rescan_root = rescan.root.as_ref().expect("root should exist");
        assert!(Arc::ptr_eq(
            &children_arc(baseline_root),
            &children_arc(rescan_root)
        ));
        assert!(baseline.content_equal(&rescan));

        // Statistics must match a from-scratch scan even though nothing was
        // reallocated.
        let fresh = scan_fixture(directory.path(), None);
        assert_eq!(rescan.directories, fresh.directories);
        assert_eq!(rescan.files, fresh.files);
        assert_eq!(rescan.symlinks, fresh.symlinks);
        assert_eq!(rescan.total_file_size, fresh.total_file_size);
    }

    #[test]
    fn rescan_rehashes_changes_and_adopts_untouched_siblings() {
        let directory = fixture();
        let root_path = directory.path();
        let baseline = scan_fixture(root_path, None);
        let baseline_root = baseline.root.as_ref().expect("root should exist");
        let baseline_nested = children_arc(child(baseline_root, "nested"));

        // Rewrite one root-level file with content of a different length, so
        // that the change is visible regardless of mtime granularity.
        write(root_path, "alpha.txt", "alpha, revised");
        let rescan = scan_fixture(root_path, Some(&baseline));
        let rescan_root = rescan.root.as_ref().expect("root should exist");

        // The rewritten file is re-digested...
        assert_eq!(
            file_content(rescan_root, "alpha.txt").0,
            digest_of("alpha, revised")
        );
        // ...the changed directory's storage is fresh...
        assert!(!Arc::ptr_eq(
            &children_arc(baseline_root),
            &children_arc(rescan_root)
        ));
        // ...but the untouched sibling subtree is adopted wholesale.
        assert!(Arc::ptr_eq(
            &baseline_nested,
            &children_arc(child(rescan_root, "nested"))
        ));
        assert_eq!(
            rescan.total_file_size,
            baseline.total_file_size + ("alpha, revised".len() - "alpha".len()) as u64
        );
    }

    #[test]
    fn matching_metadata_reuses_the_baseline_digest() {
        let directory = fixture();
        let root_path = directory.path();
        age(root_path, "alpha.txt");
        let mut baseline = scan_fixture(root_path, None);

        // Poison a baseline digest without touching the file. A scan that
        // re-read the file would compute the true digest; only a scan that
        // reused the recorded one can reproduce the poison.
        let poison = [0xAB; DIGEST_SIZE];
        let root = baseline.root.as_mut().expect("root should exist");
        if let Content::Directory(children) = &mut root.content {
            for child in Arc::make_mut(children) {
                if child.name == "alpha.txt" {
                    if let Content::File { digest, .. } = &mut child.content {
                        *digest = poison;
                    }
                }
            }
        }

        let rescan = scan_fixture(root_path, Some(&baseline));
        let rescan_root = rescan.root.as_ref().expect("root should exist");
        assert_eq!(file_content(rescan_root, "alpha.txt").0, poison);
        // A file whose size changed is re-read despite the baseline entry.
        write(root_path, "beta.txt", "beta contents, extended");
        let third = scan_fixture(root_path, Some(&rescan));
        let third_root = third.root.as_ref().expect("root should exist");
        assert_eq!(
            file_content(third_root, "beta.txt").0,
            digest_of("beta contents, extended")
        );
    }

    #[test]
    fn unicode_names_are_scanned_and_sorted() {
        let directory = tempdir().expect("temporary directory should be creatable");
        let root_path = directory.path();
        write(root_path, "Ünicode.txt", "u");
        write(root_path, "日本語/файл.txt", "b");
        write(root_path, "a.txt", "a");
        let snapshot = scan(
            root_path,
            None,
            &ignores(&[]),
            &FilesystemBehavior::default(),
            SymlinkMode::default(),
            None,
            None,
            false,
            None,
            true,
        )
        .expect("scan should succeed");
        let root = snapshot.root.as_ref().expect("root should exist");

        // Sorting is by UTF-8 byte order, so ASCII precedes the rest.
        let names: Vec<&str> = root.children().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "Ünicode.txt", "日本語"]);
        assert_eq!(file_content(root, "Ünicode.txt").0, digest_of("u"));
        assert_eq!(file_content(root, "日本語/файл.txt").0, digest_of("b"));
        assert!(root.validate(true).is_ok());
    }

    #[test]
    fn non_utf8_names_are_marked_and_deduplicated() {
        use std::os::unix::ffi::OsStrExt;

        let directory = tempdir().expect("temporary directory should be creatable");
        let root_path = directory.path();
        // Two distinct invalid byte sequences that render to the same lossy
        // name, plus an ordinary file for company.
        for raw in [b"\xff\xfe".as_slice(), b"\xfe\xff".as_slice()] {
            let name = std::ffi::OsStr::from_bytes(raw);
            if let Err(error) = fs::write(root_path.join(name), "bytes") {
                // APFS refuses invalid UTF-8 names outright, so the
                // condition this test guards against cannot exist there.
                eprintln!("skipping: this filesystem refuses invalid UTF-8 names ({error})");
                return;
            }
        }
        write(root_path, "plain.txt", "plain");

        let snapshot = scan(
            root_path,
            None,
            &ignores(&[]),
            &FilesystemBehavior::default(),
            SymlinkMode::default(),
            None,
            None,
            false,
            None,
            true,
        )
        .expect("scan should succeed");
        let root = snapshot.root.as_ref().expect("root should exist");
        // The colliding entries collapse into a single marked node, and the
        // hierarchy remains sorted and unique.
        assert_eq!(root.children().len(), 2);
        assert!(root.validate(false).is_ok());
        let marked = root
            .children()
            .iter()
            .find(|child| child.name.ends_with(NON_UTF8_SUFFIX))
            .expect("the non-UTF-8 entry should be recorded");
        match &marked.content {
            Content::Problematic { message } => assert_eq!(message, "non-UTF-8 filename"),
            other => panic!("expected problematic content, found {other:?}"),
        }
        // Problematic content isn't synchronizable, so it isn't counted.
        assert_eq!(snapshot.files, 1);
    }

    /// The plain case has to stay free: an ignored directory with nothing
    /// re-included beneath it is never opened, and appears as a single
    /// untracked entry.
    #[test]
    fn an_ignored_directory_with_no_re_inclusion_is_never_descended() {
        let directory = tempdir().expect("temporary directory should be creatable");
        let root_path = directory.path();
        write(root_path, "vendor/junk.txt", "junk");
        write(root_path, "src/main.rs", "fn main() {}");
        let set = ignores(&["vendor"]);
        let snapshot = scan(
            root_path,
            None,
            &set,
            &FilesystemBehavior::default(),
            SymlinkMode::default(),
            None,
            None,
            false,
            None,
            true,
        )
        .expect("scan should succeed");
        let root = snapshot.root.as_ref().expect("root should exist");

        let vendor = child(root, "vendor");
        assert!(matches!(vendor.content, Content::Untracked));
        assert!(vendor.child("junk.txt").is_none(), "never opened");
        assert_eq!(snapshot.files, 1);
        assert_eq!(snapshot.directories, 2);
    }

    /// A negation naming something inside an ignored directory makes the
    /// walk enter it after all. Everything inside stays ignored except
    /// what the negation names — so `vendor` with `!vendor/keep.txt` now
    /// means what `vendor/*` with `!vendor/keep.txt` always meant.
    #[test]
    fn a_negation_beneath_an_ignored_directory_is_honoured() {
        let directory = tempdir().expect("temporary directory should be creatable");
        let root_path = directory.path();
        write(root_path, "vendor/keep.txt", "keep");
        write(root_path, "vendor/junk.txt", "junk");
        write(root_path, "src/main.rs", "fn main() {}");
        let set = ignores(&["vendor", "!vendor/keep.txt"]);
        let snapshot = scan(
            root_path,
            None,
            &set,
            &FilesystemBehavior::default(),
            SymlinkMode::default(),
            None,
            None,
            false,
            None,
            true,
        )
        .expect("scan should succeed");
        let root = snapshot.root.as_ref().expect("root should exist");

        let vendor = child(root, "vendor");
        assert!(
            matches!(vendor.content, Content::Directory(_)),
            "walked, because a negation names something inside"
        );
        let keep = vendor.child("keep.txt").expect("re-included");
        assert!(matches!(keep.content, Content::File { .. }));
        let junk = vendor.child("junk.txt").expect("present but untracked");
        assert!(
            matches!(junk.content, Content::Untracked),
            "inside the region, everything not re-included stays ignored"
        );
        assert_eq!(snapshot.files, 2);
        assert_eq!(snapshot.directories, 3);
    }

    /// The region ends at a re-included directory: what is brought back is
    /// brought back whole, contents and all, not as an empty shell.
    #[test]
    fn a_re_included_directory_comes_back_with_its_contents() {
        let directory = tempdir().expect("temporary directory should be creatable");
        let root_path = directory.path();
        write(root_path, "vendor/keep/deep/file.txt", "deep");
        write(root_path, "vendor/junk.txt", "junk");
        let set = ignores(&["vendor", "!vendor/keep"]);
        let snapshot = scan(
            root_path,
            None,
            &set,
            &FilesystemBehavior::default(),
            SymlinkMode::default(),
            None,
            None,
            false,
            None,
            true,
        )
        .expect("scan should succeed");
        let root = snapshot.root.as_ref().expect("root should exist");

        let keep = child(child(root, "vendor"), "keep");
        assert!(matches!(keep.content, Content::Directory(_)));
        let file = child(child(keep, "deep"), "file.txt");
        assert!(
            matches!(file.content, Content::File { .. }),
            "nothing inside a re-included directory is still in the region"
        );
        assert!(matches!(
            child(root, "vendor")
                .child("junk.txt")
                .expect("present")
                .content,
            Content::Untracked
        ));
    }

    #[test]
    fn unreadable_directories_become_problematic() {
        let directory = tempdir().expect("temporary directory should be creatable");
        let root_path = directory.path();
        write(root_path, "locked/inner.txt", "inner");
        let locked = root_path.join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))
            .expect("permissions should be settable");
        let snapshot = scan(
            root_path,
            None,
            &ignores(&[]),
            &FilesystemBehavior::default(),
            SymlinkMode::default(),
            None,
            None,
            false,
            None,
            true,
        )
        .expect("scan should succeed");
        let root = snapshot.root.as_ref().expect("root should exist");
        let problems = root.problems();
        // Running as root defeats the permission bits, in which case the
        // directory scans normally; otherwise it must be problematic.
        if !problems.is_empty() {
            assert_eq!(problems[0].path, "locked");
            assert_eq!(snapshot.directories, 1);
        }
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755))
            .expect("permissions should be restorable");
    }

    /// `/dev` holds mounts of its own on Linux (`pts`, `shm`, `mqueue`):
    /// real mount points, with no need for privilege to make one.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_directory_on_another_device_is_recorded_and_left_alone() {
        let mounted: Vec<String> = std::fs::read_to_string("/proc/self/mountinfo")
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.split_whitespace().nth(4))
            .filter_map(|point| point.strip_prefix("/dev/"))
            .filter(|rest| !rest.is_empty() && !rest.contains('/'))
            .map(str::to_owned)
            .collect();
        if mounted.is_empty() {
            return;
        }
        let scan_dev = |ignore_mounts: bool| {
            scan(
                Path::new("/dev"),
                None,
                &IgnoreSet::new(&[]).unwrap(),
                &FilesystemBehavior::default(),
                SymlinkMode::default(),
                None,
                None,
                false,
                None,
                ignore_mounts,
            )
            .expect("/dev scans")
        };
        let left_alone = scan_dev(true);
        for point in &mounted {
            assert!(
                left_alone.mount_points.contains(point),
                "{point} in {:?}",
                left_alone.mount_points
            );
            let node = left_alone
                .root
                .as_ref()
                .unwrap()
                .child(point)
                .expect("listed");
            assert!(
                matches!(node.content, Content::Untracked),
                "{point} left alone"
            );
        }
        // An incremental scan that relists `/dev` adopts the mount points
        // it does not probe again, and must still report them: a watch
        // session scans this way on nearly every cycle.
        let mut dirty = DirtyPaths::default();
        dirty.mark("null");
        let again = scan(
            Path::new("/dev"),
            Some(&left_alone),
            &IgnoreSet::new(&[]).unwrap(),
            &FilesystemBehavior::default(),
            SymlinkMode::default(),
            None,
            Some(&dirty),
            false,
            None,
            true,
        )
        .expect("/dev rescans");
        for point in &mounted {
            assert!(
                again.mount_points.contains(point),
                "{point} carried: {:?}",
                again.mount_points
            );
        }

        // Followed, they are still recorded, and walked.
        let followed = scan_dev(false);
        for point in &mounted {
            assert!(followed.mount_points.contains(point), "{point}");
            let node = followed
                .root
                .as_ref()
                .unwrap()
                .child(point)
                .expect("listed");
            assert!(
                matches!(node.content, Content::Directory(_)),
                "{point} walked"
            );
        }
    }

    /// A tree wide and deep enough that the parallel walk actually spreads.
    fn wide_tree(root: &Path) {
        for a in 0..6 {
            for b in 0..5 {
                for c in 0..4 {
                    write(root, &format!("d{a}/e{b}/f{c}.txt"), &format!("{a}{b}{c}"));
                }
                std::os::unix::fs::symlink("f0.txt", root.join(format!("d{a}/e{b}/link"))).unwrap();
            }
            write(root, &format!("d{a}/top.txt"), "top");
        }
        write(root, "excluded/skip.txt", "skip");
        std::fs::create_dir_all(root.join("empty/deeper")).unwrap();
    }

    /// Asserts two hierarchies are the same in every recorded respect —
    /// names, order, content, metadata — and share storage with `baseline`
    /// at exactly the same places.
    fn assert_identical(serial: &Node, parallel: &Node, baseline: Option<&Node>, path: &str) {
        assert_eq!(serial.name, parallel.name, "at {path}");
        assert!(serial.content_equal(parallel, false), "at {path}");
        if let (Content::File { metadata: a, .. }, Content::File { metadata: b, .. }) =
            (&serial.content, &parallel.content)
        {
            assert_eq!(a, b, "metadata at {path}");
        }
        assert_eq!(
            crate::tree::nodes_share_storage(Some(serial), baseline),
            crate::tree::nodes_share_storage(Some(parallel), baseline),
            "storage sharing at {path}"
        );
        let (left, right) = (serial.children(), parallel.children());
        assert_eq!(left.len(), right.len(), "children at {path}");
        for (a, b) in left.iter().zip(right) {
            let child = format!("{path}/{}", a.name);
            assert_identical(a, b, baseline.and_then(|node| node.child(&a.name)), &child);
        }
    }

    #[test]
    fn a_parallel_scan_builds_exactly_the_serial_one() {
        let root = fixture();
        wide_tree(root.path());
        let with_helpers =
            |helpers: usize, baseline: Option<&Snapshot>, dirty: Option<&DirtyPaths>| {
                HELPERS.with(|cell| cell.set(Some(helpers)));
                let snapshot = scan(
                    root.path(),
                    baseline,
                    &ignores(&["excluded/"]),
                    &FilesystemBehavior::default(),
                    SymlinkMode::default(),
                    None,
                    dirty,
                    false,
                    None,
                    true,
                )
                .expect("scan");
                HELPERS.with(|cell| cell.set(None));
                snapshot
            };

        // Full scans, from nothing.
        let serial = with_helpers(0, None, None);
        let parallel = with_helpers(7, None, None);
        assert_identical(
            serial.root.as_ref().unwrap(),
            parallel.root.as_ref().unwrap(),
            None,
            "",
        );
        assert_eq!(
            (
                serial.directories,
                serial.files,
                serial.symlinks,
                serial.total_file_size
            ),
            (
                parallel.directories,
                parallel.files,
                parallel.symlinks,
                parallel.total_file_size
            )
        );

        // Incremental scans against one baseline, after edits in a few
        // subtrees: the same content, and the same subtrees adopted.
        age(root.path(), "d0/e0/f0.txt");
        write(root.path(), "d1/e2/f3.txt", "changed");
        write(root.path(), "d4/new.txt", "new");
        std::fs::remove_file(root.path().join("d5/e4/f1.txt")).unwrap();
        let mut dirty = DirtyPaths::default();
        for path in ["d1/e2/f3.txt", "d4/new.txt", "d5/e4/f1.txt"] {
            dirty.mark(path);
        }
        let baseline = serial;
        let serial = with_helpers(0, Some(&baseline), Some(&dirty));
        let parallel = with_helpers(7, Some(&baseline), Some(&dirty));
        assert_identical(
            serial.root.as_ref().unwrap(),
            parallel.root.as_ref().unwrap(),
            baseline.root.as_ref(),
            "",
        );
        assert_eq!(
            (
                serial.directories,
                serial.files,
                serial.symlinks,
                serial.total_file_size
            ),
            (
                parallel.directories,
                parallel.files,
                parallel.symlinks,
                parallel.total_file_size
            )
        );
    }
}
