//! Filesystem scanning.

pub mod ignore;
pub mod probes;

use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::io::{ErrorKind, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use crate::tree::{path_join, Content, Digest, FileMetadata, Node, Snapshot};

pub use ignore::IgnoreSet;
pub use probes::{probe, recompose, FilesystemBehavior};

/// The size of the fixed buffer used to stream file contents through the
/// digester.
const DIGEST_BUFFER_SIZE: usize = 64 * 1024;

/// The file type mask within a raw mode value (`S_IFMT`).
const MODE_TYPE_MASK: u32 = 0o170000;

/// The executability bits within a raw mode value.
const MODE_EXECUTABLE_MASK: u32 = 0o111;

/// The name prefix used by transition staging temporaries, which are
/// invisible to scans.
const TEMPORARY_PREFIX: &str = ".autobahn-tmp";

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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
pub fn scan(
    root: &Path,
    baseline: Option<&Snapshot>,
    ignores: &IgnoreSet,
    behavior: &FilesystemBehavior,
    symlink_mode: SymlinkMode,
    max_file_size: Option<u64>,
    dirty: Option<&DirtyPaths>,
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

    // An incremental scan is only meaningful against a baseline: without
    // one there is nothing to adopt, and everything must be read anyway.
    let baseline_root = baseline.and_then(|s| s.root.as_ref());
    let dirty = dirty.filter(|_| baseline_root.is_some());
    let mut scanner = Scanner::new(
        ignores,
        behavior,
        symlink_mode,
        max_file_size,
        dirty.is_some(),
    );
    let content = scanner.scan_directory(root, "", baseline_root, dirty.map(|d| &d.root));
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
    };
    // An incremental scan only counts what it visited, so the statistics
    // are recomputed from the assembled hierarchy (a pointer walk, with no
    // filesystem access, over a tree that is mostly shared storage).
    if dirty.is_some() {
        recount(&mut snapshot);
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
}

impl<'a> Scanner<'a> {
    /// Creates a scanner applying the specified ignore set.
    fn new(
        ignores: &'a IgnoreSet,
        behavior: &'a FilesystemBehavior,
        symlink_mode: SymlinkMode,
        max_file_size: Option<u64>,
        incremental: bool,
    ) -> Scanner<'a> {
        Scanner {
            ignores,
            behavior,
            symlink_mode,
            max_file_size,
            incremental,
            buffer: vec![0u8; DIGEST_BUFFER_SIZE],
            directories: 0,
            files: 0,
            symlinks: 0,
            total_file_size: 0,
        }
    }

    /// Scans the directory at `disk_path`, whose root-relative path is
    /// `path`, using `baseline` (the node observed at the same position by a
    /// previous scan, if any) for digest reuse and structural sharing.
    ///
    /// `dirty` carries the incremental scan's marks for this position:
    /// `None` means nothing beneath this directory changed, so the
    /// baseline's subtree is adopted whole without touching the filesystem.
    fn scan_directory(
        &mut self,
        disk_path: &Path,
        path: &str,
        baseline: Option<&Node>,
        dirty: Option<&DirtyNode>,
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

        let mut children = Vec::with_capacity(entries.len());
        for (raw_name, entry_path) in entries {
            let lossy_name = raw_name.to_string_lossy();

            // Staging temporaries belong to in-flight transitions, not to
            // the synchronized hierarchy.
            if lossy_name.starts_with(TEMPORARY_PREFIX) {
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
                        children.push(baseline_child.clone());
                        continue;
                    }
                }
            }

            if non_utf8 {
                // Classification still needs the entry's type for the
                // ignore set, so probe before recording the problem.
                let ignored = fs::symlink_metadata(&entry_path)
                    .map(|metadata| self.ignores.ignored(&child_path, metadata.is_dir()))
                    .unwrap_or(false);
                children.push(Node {
                    name,
                    content: if ignored {
                        Content::Untracked
                    } else {
                        problematic("non-UTF-8 filename")
                    },
                });
                continue;
            }

            if let Some(node) =
                self.scan_entry(name, &entry_path, &child_path, baseline_child, child_dirty)
            {
                children.push(node);
            }
        }

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
    fn scan_entry(
        &mut self,
        name: String,
        entry_path: &Path,
        child_path: &str,
        baseline: Option<&Node>,
        dirty: Option<&DirtyNode>,
    ) -> Option<Node> {
        // The entry's type is needed both to dispatch the scan and to
        // resolve directory-only ignore patterns, so it's fetched (without
        // following symbolic links) before anything else.
        let metadata = match fs::symlink_metadata(entry_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return None,
            Err(error) => {
                // Without a type there's nothing to classify, not even for
                // the purposes of the ignore set.
                return Some(Node {
                    name,
                    content: problematic(format!("unable to probe entry: {error}")),
                });
            }
        };
        let file_type = metadata.file_type();

        // Ignores are consulted before any descent, which is what keeps
        // ignored subtrees from costing anything at all.
        if self.ignores.ignored(child_path, file_type.is_dir()) {
            return Some(Node {
                name,
                content: Content::Untracked,
            });
        }

        let content = if file_type.is_dir() {
            self.scan_directory(entry_path, child_path, baseline, dirty)
        } else if file_type.is_file() {
            self.scan_file(entry_path, &metadata, baseline)
        } else if file_type.is_symlink() {
            self.scan_symlink(entry_path, child_path)
        } else {
            // Sockets, FIFOs, and device nodes have no portable
            // representation and aren't synchronized.
            Content::Untracked
        };
        Some(Node { name, content })
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
        let mut recorded = file_metadata(metadata);

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
        let digest = match reusable_digest(baseline, &recorded) {
            Some(digest) => digest,
            None => {
                let (digest, read) = match self.digest_file(disk_path) {
                    Ok(result) => result,
                    Err(error) => return problematic(format!("unable to read file: {error:#}")),
                };
                if read != recorded.size {
                    // The file changed size between the stat and the read,
                    // so the digest describes content the recorded metadata
                    // doesn't. Re-stat and record what's there now: the
                    // digest is still a faithful record of some version of
                    // the file, and any further change moves the mtime and
                    // forces a re-read on the next scan.
                    match fs::symlink_metadata(disk_path) {
                        Ok(fresh) => recorded = file_metadata(&fresh),
                        Err(error) => {
                            return problematic(format!("unable to re-probe file: {error}"));
                        }
                    }
                }
                digest
            }
        };

        self.files += 1;
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

/// Returns the baseline node's digest if its metadata proves that the file's
/// content matches what was observed at scan time.
fn reusable_digest(baseline: Option<&Node>, fresh: &FileMetadata) -> Option<Digest> {
    let Some(Content::File {
        digest, metadata, ..
    }) = baseline.map(|node| &node.content)
    else {
        return None;
    };
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
        write(root, ".autobahn-tmp-staging", "staging");
        directory
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
            fs::write(root_path.join(name), "bytes").expect("file should be writable");
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

    #[test]
    fn ignored_directories_are_never_descended_despite_negations() {
        let directory = tempdir().expect("temporary directory should be creatable");
        let root_path = directory.path();
        write(root_path, "vendor/keep.txt", "keep");
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
        )
        .expect("scan should succeed");
        let root = snapshot.root.as_ref().expect("root should exist");

        let vendor = child(root, "vendor");
        assert!(matches!(vendor.content, Content::Untracked));
        assert!(vendor.child("keep.txt").is_none());
        assert_eq!(snapshot.files, 1);
        assert_eq!(snapshot.directories, 2);
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
