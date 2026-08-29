//! The filesystem hierarchy model.
//!
//! A [`Node`] represents one filesystem entry; a hierarchy is a root node
//! whose directory contents are name-sorted child vectors shared between
//! snapshot generations via [`Arc`] (copy-on-write). Nodes are immutable by
//! convention once published — the borrow checker plus `Arc::make_mut`
//! enforce what convention alone had to carry in the Go implementation.
//!
//! Content equality ([`Node::content_equal`]) deliberately ignores names
//! (which are positional) and scan metadata (which describes observation
//! time, not content), exactly matching the reconciliation semantics of the
//! Mutagen engine this design derives from.

mod apply;
mod diff;
mod executability;
mod reconcile;

pub use apply::apply;
pub use diff::{diff, diff_at};
pub use executability::propagate_executability;
pub use reconcile::{reconcile, Reconciliation};

use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// The size of content digests (BLAKE3).
pub const DIGEST_SIZE: usize = 32;

/// A content digest.
pub type Digest = [u8; DIGEST_SIZE];

/// Filesystem metadata observed for a file at scan time. It drives digest
/// reuse on subsequent scans and modification detection during transitions.
/// It is carried on file nodes (and thus shared, persisted, and garbage
/// collected with the hierarchy) but excluded from content equality.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMetadata {
    /// The whole-second component of the modification time (Unix epoch).
    pub mtime_seconds: i64,
    /// The fractional component of the modification time in nanoseconds.
    pub mtime_nanos: u32,
    /// The file size in bytes.
    pub size: u64,
    /// The file's inode number (0 where unavailable).
    pub inode: u64,
    /// The raw filesystem mode bits.
    pub mode: u32,
}

/// The content of a filesystem entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Content {
    /// A directory with name-sorted, name-unique children.
    Directory(Arc<Vec<Node>>),
    /// A regular file.
    File {
        /// The BLAKE3 digest of the file's contents.
        digest: Digest,
        /// Whether or not the file is executable.
        executable: bool,
        /// Scan-time metadata (excluded from content equality).
        metadata: FileMetadata,
    },
    /// A symbolic link.
    Symlink {
        /// The link target.
        target: String,
    },
    /// Content that exists on disk but is intentionally excluded from
    /// synchronization (ignored entries and unsupported filesystem types).
    Untracked,
    /// Content that could not be scanned or classified.
    Problematic {
        /// The error message describing the problem.
        message: String,
    },
}

/// A filesystem entry: a name plus content.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    /// The entry's name within its parent (empty for hierarchy roots).
    pub name: String,
    /// The entry's content.
    pub content: Content,
}

/// The synchronization mode governing reconciliation directionality and
/// conflict handling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncMode {
    /// Bidirectional synchronization that surfaces conflicts without
    /// resolving them.
    TwoWaySafe,
    /// Bidirectional synchronization that resolves conflicts in alpha's
    /// favor.
    TwoWayResolved,
    /// Unidirectional (alpha to beta) synchronization that refuses to
    /// overwrite or reverse-propagate beta-side changes.
    OneWaySafe,
    /// Unidirectional (alpha to beta) synchronization that maintains beta as
    /// an exact mirror of alpha.
    OneWayReplica,
}

/// A content change: a transition from one content state to another at a
/// path.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Change {
    /// The root-relative path at which the change occurs (empty for the
    /// root itself).
    pub path: String,
    /// The old content at the path (`None` for creations).
    pub old: Option<Node>,
    /// The new content at the path (`None` for deletions).
    pub new: Option<Node>,
}

impl Change {
    /// Indicates whether or not this change deletes the synchronization root.
    pub fn is_root_deletion(&self) -> bool {
        self.path.is_empty() && self.old.is_some() && self.new.is_none()
    }
}

/// A conflict between changes made on alpha and beta.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Conflict {
    /// The root path of the conflict.
    pub root: String,
    /// The relevant changes on alpha.
    pub alpha_changes: Vec<Change>,
    /// The relevant changes on beta.
    pub beta_changes: Vec<Change>,
}

/// A non-fatal problem encountered at a particular path.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Problem {
    /// The root-relative path at which the problem occurred.
    pub path: String,
    /// The problem description.
    pub message: String,
}

/// Joins a parent path and a child name using the synchronization path
/// convention (root-relative, forward slashes, no leading slash).
pub fn path_join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        let mut joined = String::with_capacity(parent.len() + 1 + name.len());
        joined.push_str(parent);
        joined.push('/');
        joined.push_str(name);
        joined
    }
}

impl Content {
    /// Indicates whether or not this content kind participates in
    /// synchronization.
    pub fn synchronizable(&self) -> bool {
        matches!(
            self,
            Content::Directory(_) | Content::File { .. } | Content::Symlink { .. }
        )
    }
}

impl Node {
    /// Creates a directory node from a (possibly unsorted) child vector,
    /// sorting the children by name.
    pub fn directory(name: impl Into<String>, mut children: Vec<Node>) -> Node {
        children.sort_by(|a, b| a.name.cmp(&b.name));
        Node {
            name: name.into(),
            content: Content::Directory(Arc::new(children)),
        }
    }

    /// Returns the node's children, or an empty slice for non-directories.
    pub fn children(&self) -> &[Node] {
        match &self.content {
            Content::Directory(children) => children,
            _ => &[],
        }
    }

    /// Returns the child with the specified name, if any, via binary search
    /// (children are name-sorted).
    pub fn child(&self, name: &str) -> Option<&Node> {
        let children = self.children();
        children
            .binary_search_by(|child| child.name.as_str().cmp(name))
            .ok()
            .map(|index| &children[index])
    }

    /// Performs a content equivalence comparison with another node, ignoring
    /// names and scan metadata. If `deep` is true, directory contents are
    /// compared recursively (as a linear merge over the sorted children).
    pub fn content_equal(&self, other: &Node, deep: bool) -> bool {
        match (&self.content, &other.content) {
            (Content::Directory(a), Content::Directory(b)) => {
                if !deep {
                    return true;
                }
                if Arc::ptr_eq(a, b) {
                    return true;
                }
                a.len() == b.len()
                    && a.iter()
                        .zip(b.iter())
                        .all(|(x, y)| x.name == y.name && x.content_equal(y, true))
            }
            (
                Content::File {
                    digest: da,
                    executable: ea,
                    ..
                },
                Content::File {
                    digest: db,
                    executable: eb,
                    ..
                },
            ) => da == db && ea == eb,
            (Content::Symlink { target: ta }, Content::Symlink { target: tb }) => ta == tb,
            (Content::Untracked, Content::Untracked) => true,
            (Content::Problematic { message: ma }, Content::Problematic { message: mb }) => {
                ma == mb
            }
            _ => false,
        }
    }

    /// Counts the synchronizable entries in the hierarchy rooted at this
    /// node, excluding unsynchronizable subtrees.
    pub fn count(&self) -> u64 {
        if !self.content.synchronizable() {
            return 0;
        }
        1 + self.children().iter().map(Node::count).sum::<u64>()
    }

    /// Indicates whether or not the hierarchy rooted at this node contains
    /// (or is) unsynchronizable content.
    pub fn has_unsynchronizable_content(&self) -> bool {
        if !self.content.synchronizable() {
            return true;
        }
        self.children()
            .iter()
            .any(Node::has_unsynchronizable_content)
    }

    /// Returns the subtree of this hierarchy consisting of only
    /// synchronizable content: `None` if this node itself is
    /// unsynchronizable, the node itself (cheaply cloned, sharing children)
    /// if the hierarchy is fully synchronizable, and a filtered copy
    /// (sharing clean subtrees) otherwise.
    pub fn synchronizable_subtree(&self) -> Option<Node> {
        if !self.content.synchronizable() {
            return None;
        }
        if !self.has_unsynchronizable_content() {
            return Some(self.clone());
        }
        let filtered: Vec<Node> = self
            .children()
            .iter()
            .filter_map(Node::synchronizable_subtree)
            .collect();
        Some(Node {
            name: self.name.clone(),
            content: Content::Directory(Arc::new(filtered)),
        })
    }

    /// Collects problems from problematic nodes in the hierarchy, with paths
    /// computed relative to this node as the synchronization root.
    pub fn problems(&self) -> Vec<Problem> {
        // The walk carries the components of the current path rather than a
        // joined string, and materializes one only where a problem is
        // actually recorded. Problems are rare and hierarchies are large:
        // building a path at every node allocated one string per entry in
        // the tree in order to describe a handful of them.
        fn collect<'a>(node: &'a Node, components: &mut Vec<&'a str>, problems: &mut Vec<Problem>) {
            if let Content::Problematic { message } = &node.content {
                problems.push(Problem {
                    path: components.join("/"),
                    message: message.clone(),
                });
                return;
            }
            for child in node.children() {
                components.push(&child.name);
                collect(child, components, problems);
                components.pop();
            }
        }
        let mut problems = Vec::new();
        collect(self, &mut Vec::new(), &mut problems);
        problems
    }

    /// Validates the hierarchy's structural invariants: sorted, unique,
    /// non-empty, separator-free child names; content-appropriate fields; and
    /// (when `synchronizable_only` is set) an absence of unsynchronizable
    /// content.
    pub fn validate(&self, synchronizable_only: bool) -> Result<(), String> {
        match &self.content {
            Content::Directory(children) => {
                let mut previous: Option<&str> = None;
                for child in children.iter() {
                    if child.name.is_empty() {
                        return Err("empty child name".into());
                    }
                    if child.name == "." || child.name == ".." {
                        return Err("dot child name".into());
                    }
                    if child.name.contains('/') || child.name.contains('\0') {
                        return Err("child name contains path separator or NUL".into());
                    }
                    if let Some(previous) = previous {
                        if child.name.as_str() <= previous {
                            return Err("unsorted or duplicate child names".into());
                        }
                    }
                    previous = Some(child.name.as_str());
                    child.validate(synchronizable_only)?;
                }
                Ok(())
            }
            Content::File { .. } => Ok(()),
            Content::Symlink { target } => {
                if target.is_empty() {
                    Err("empty symbolic link target".into())
                } else {
                    Ok(())
                }
            }
            Content::Untracked => {
                if synchronizable_only {
                    Err("untracked content is not synchronizable".into())
                } else {
                    Ok(())
                }
            }
            Content::Problematic { message } => {
                if synchronizable_only {
                    Err("problematic content is not synchronizable".into())
                } else if message.is_empty() {
                    Err("empty problem message".into())
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Validates as [`validate`](Self::validate) does, but skips any subtree
    /// that shares storage with `previous` — which must itself already have
    /// been validated under the same `synchronizable_only`.
    ///
    /// A hierarchy built by [`apply`](crate::tree::apply) keeps the storage
    /// of every subtree the change did not touch, so validating a new
    /// ancestor against the old one revisits only the root-to-leaf path that
    /// actually changed. On a large tree that is the difference between
    /// walking half a million entries and walking a handful.
    ///
    /// The saving is sound only because sharing is proof of *identity*: the
    /// two subtrees are one immutable allocation, so one having passed
    /// validation is the other having passed it. Nothing weaker qualifies —
    /// in particular, sharing with a scanned or transferred hierarchy proves
    /// nothing here, because those are never validated under
    /// `synchronizable_only` and may legitimately hold untracked or
    /// problematic content.
    pub fn validate_against(
        &self,
        previous: Option<&Node>,
        synchronizable_only: bool,
    ) -> Result<(), String> {
        if nodes_share_storage(Some(self), previous) {
            return Ok(());
        }
        let Content::Directory(children) = &self.content else {
            // Only directories carry shareable storage, so everything else
            // is validated exactly as it would be anyway.
            return self.validate(synchronizable_only);
        };
        // This children vector differs from the previous one, so its own
        // invariants hold nothing over from before and are checked in full.
        // Only the children themselves may be skipped, and only individually.
        let previous_children = match previous.map(|node| &node.content) {
            Some(Content::Directory(children)) => Some(children),
            _ => None,
        };
        let mut cursor = 0usize;
        let mut last: Option<&str> = None;
        for child in children.iter() {
            if child.name.is_empty() {
                return Err("empty child name".into());
            }
            if child.name == "." || child.name == ".." {
                return Err("dot child name".into());
            }
            if child.name.contains('/') || child.name.contains('\0') {
                return Err("child name contains path separator or NUL".into());
            }
            if let Some(last) = last {
                if child.name.as_str() <= last {
                    return Err("unsorted or duplicate child names".into());
                }
            }
            last = Some(child.name.as_str());
            // Both vectors are name-sorted, so the counterpart is found by
            // advancing a single cursor rather than searching.
            let mut counterpart = None;
            if let Some(previous_children) = previous_children {
                while cursor < previous_children.len()
                    && previous_children[cursor].name.as_str() < child.name.as_str()
                {
                    cursor += 1;
                }
                if cursor < previous_children.len() && previous_children[cursor].name == child.name
                {
                    counterpart = Some(&previous_children[cursor]);
                }
            }
            child.validate_against(counterpart, synchronizable_only)?;
        }
        Ok(())
    }
}

/// Reports whether two optional nodes are backed by the *same storage* —
/// the pointer check that copy-on-write sharing makes meaningful.
///
/// An unchanged scan adopts its baseline's children wholesale, so an
/// unchanged subtree compares equal here in constant time however large it
/// is. Two absent nodes agree; anything that is not a pair of directories
/// does not, since only directories carry shared storage to compare.
///
/// Sharing is an artifact of how a hierarchy was *produced*, not of what it
/// contains: a hierarchy decoded from disk or from the wire shares nothing,
/// so equal content can and does answer `false` here. Callers may therefore
/// use this to prove agreement, never to prove difference.
pub fn nodes_share_storage(a: Option<&Node>, b: Option<&Node>) -> bool {
    match (a, b) {
        (
            Some(Node {
                content: Content::Directory(a),
                ..
            }),
            Some(Node {
                content: Content::Directory(b),
                ..
            }),
        ) => Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

/// A filesystem snapshot: an optional root hierarchy plus scan statistics
/// and behavioral information.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    /// The snapshot content (`None` if the synchronization root doesn't
    /// exist).
    pub root: Option<Node>,
    /// Whether or not the scanned filesystem preserves executability bits.
    pub preserves_executability: bool,
    /// The number of synchronizable directories scanned.
    pub directories: u64,
    /// The number of synchronizable files scanned.
    pub files: u64,
    /// The number of synchronizable symbolic links scanned.
    pub symlinks: u64,
    /// The total size of synchronizable file content.
    pub total_file_size: u64,
}

impl Snapshot {
    /// Performs a content equivalence comparison with another snapshot.
    pub fn content_equal(&self, other: &Snapshot) -> bool {
        match (&self.root, &other.root) {
            (None, None) => true,
            (Some(a), Some(b)) => {
                a.content_equal(b, true)
                    && self.preserves_executability == other.preserves_executability
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn file(name: &str, digest_byte: u8, executable: bool) -> Node {
        Node {
            name: name.into(),
            content: Content::File {
                digest: [digest_byte; DIGEST_SIZE],
                executable,
                metadata: FileMetadata::default(),
            },
        }
    }

    #[test]
    fn child_lookup_and_sorting() {
        let dir = Node::directory("", vec![file("b", 1, false), file("a", 2, false)]);
        assert_eq!(dir.children()[0].name, "a");
        assert!(dir.child("a").is_some());
        assert!(dir.child("b").is_some());
        assert!(dir.child("c").is_none());
    }

    #[test]
    fn content_equality_ignores_names_and_metadata() {
        let mut a = file("x", 1, false);
        let b = file("y", 1, false);
        assert!(a.content_equal(&b, true));
        if let Content::File { metadata, .. } = &mut a.content {
            metadata.size = 42;
        }
        assert!(a.content_equal(&b, true));
    }

    #[test]
    fn synchronizable_subtree_shares_clean_hierarchies() {
        let clean = Node::directory("", vec![file("a", 1, false)]);
        let filtered = clean.synchronizable_subtree().unwrap();
        match (&clean.content, &filtered.content) {
            (Content::Directory(a), Content::Directory(b)) => assert!(Arc::ptr_eq(a, b)),
            _ => panic!("expected directories"),
        }

        let dirty = Node::directory(
            "",
            vec![
                file("a", 1, false),
                Node {
                    name: "u".into(),
                    content: Content::Untracked,
                },
            ],
        );
        let filtered = dirty.synchronizable_subtree().unwrap();
        assert_eq!(filtered.children().len(), 1);
        assert!(dirty.children().len() == 2);
    }

    #[test]
    fn validation_rejects_malformed_hierarchies() {
        let dup = Node {
            name: String::new(),
            content: Content::Directory(Arc::new(vec![file("a", 1, false), file("a", 2, false)])),
        };
        assert!(dup.validate(false).is_err());
        let ok = Node::directory("", vec![file("a", 1, false), file("b", 2, false)]);
        assert!(ok.validate(true).is_ok());
    }
}
