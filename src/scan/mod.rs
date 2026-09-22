//! Filesystem scanning.

pub mod ignore;
pub mod ignorefile;
pub mod probes;

use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::io::{ErrorKind, Read};
use std::os::unix::fs::MetadataExt;
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

/// How many threads a scan may add beside the one it runs on.
fn scan_helpers() -> usize {
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
const TEMPORARY_PREFIX: &str = ".autobahn-tmp";

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
fn autobahn_temporary(name: &str) -> bool {
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
    /// observable by listing.
    relist: bool,
    /// Marked entries beneath this one, by name.
    children: std::collections::HashMap<String, DirtyNode>,
}

impl DirtyPaths {
    /// Marks a root-relative path (`""` for the root itself) as changed:
    /// the entry is rescanned, and its parent is listed again so that its
    /// creation or removal is observed.
    pub fn mark(&mut self, path: &str) {
        let mut components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        let name = components.pop();
        let mut node = &mut self.root;
        for component in components {
            node = node.children.entry(component.to_owned()).or_default();
        }
        node.relist = true;
        if let Some(name) = name {
            node.children.entry(name.to_owned()).or_default();
        }
    }

    /// Indicates whether nothing at all is marked.
    pub fn is_empty(&self) -> bool {
        !self.root.relist && self.root.children.is_empty()
    }
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
    );
    let content = scanner.scan_directory(root, "", baseline_root, dirty.map(|d| &d.root));
    scanner.publish();
    let mut snapshot = Snapshot {
        root: Some(Node {
            name: String::new(),
            content,
        }),
        preserves_executability: behavior.preserves_executability,
        directories: scanner.directories,
        files: scanner.files,
        symlinks: scanner.symlinks,
        total_file_size: scanner.total_file_size,
        scanned_at_seconds,
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
    /// A directory to walk, and whether it opens (or continues) an
    /// ignored region.
    Directory { region: bool },
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
        );
        forked.within_ignored = within_ignored;
        forked
    }

    /// Folds a finished subtree scanner's counts into this one.
    fn absorb(&mut self, mut forked: Scanner<'a>) {
        forked.publish();
        self.directories += forked.directories;
        self.files += forked.files;
        self.symlinks += forked.symlinks;
        self.total_file_size += forked.total_file_size;
    }

    /// Claims a helper thread for a subtree, if one is free.
    fn take_helper(&self) -> bool {
        self.helpers
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |free| free.checked_sub(1))
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
                Probed::Directory { region } => children.push(Pending::Walk {
                    name,
                    entry_path,
                    child_path,
                    baseline: baseline_child,
                    dirty: child_dirty,
                    region,
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
                    } => Node {
                        name,
                        content: self.walk(&entry_path, &child_path, baseline, dirty, region),
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
                        } => {
                            if self.take_helper() {
                                let mut forked = self.fork(region);
                                handles.push((
                                    nodes.len(),
                                    scope.spawn(move || {
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
                                let content =
                                    self.walk(&entry_path, &child_path, baseline, dirty, region);
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
        match self.within_ignored {
            true => !self.ignores.re_included(child_path, is_directory),
            false => self.ignores.ignored(child_path, is_directory),
        }
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
            Probed::Directory { region } => {
                self.walk(entry_path, child_path, baseline, dirty, region)
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
        let ignored = self.entry_ignored(child_path, is_directory);
        if ignored && !(is_directory && self.ignores.holds_a_re_inclusion(child_path)) {
            return Probed::Settled(Content::Untracked);
        }

        if is_directory {
            // An ignored directory opens a region; a re-included one ends
            // it. Without the second half the negation would bring the
            // directory back but not what is in it, which is not what
            // anyone means by re-including.
            Probed::Directory { region: ignored }
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
    ) -> Content {
        let outer = self.within_ignored;
        self.within_ignored = region;
        let content = self.scan_directory(entry_path, child_path, baseline, dirty);
        self.within_ignored = outer;
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
                let (digest, read) = match self.digest_file(disk_path) {
                    Ok(result) => result,
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
    /// and the number of bytes read.
    fn digest_file(&mut self, disk_path: &Path) -> Result<(Digest, u64)> {
        let mut file = fs::File::open(disk_path)
            .with_context(|| format!("unable to open {}", disk_path.display()))?;
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
        }
        Ok((*hasher.finalize().as_bytes(), read))
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
}
