//! Executability propagation.
//!
//! On a filesystem that doesn't preserve executability bits (the FAT
//! family, most prominently), the bits a scan reports are noise — often
//! every file reads as executable, or none does. Fed directly into
//! reconciliation, that noise would masquerade as content changes: phantom
//! modifications that churn permissions on the other side forever. The fix
//! is to replace the affected snapshot's bits wholesale from trusted
//! references before reconciliation, so only real content differences
//! remain.
//!
//! For each file on the non-preserving side, the bit comes from the first
//! reference that can vouch for it:
//!
//! 1. **The peer** (the session's other side, when *it* preserves bits),
//!    if it holds a file with the same digest at the same path — identical
//!    bytes carry the peer's current intent, including a brand-new
//!    executable that has no ancestor yet. (This reference goes beyond
//!    Mutagen, which consults only the ancestor and reports a false
//!    conflict in exactly that case.)
//! 2. **The ancestor**, if it holds a file at the same path — regardless of
//!    digest, since an edit made on the non-preserving side changes bytes
//!    but cannot change the (unstorable) bit, which the ancestor still
//!    remembers.
//! 3. Otherwise the bit is stripped to non-executable: nothing vouches for
//!    it, and a volume that can't store bits can't assert them.
//!
//! The pass is structural and copy-on-write: subtrees without any adjusted
//! bit share their storage with the input snapshot, so it costs allocation
//! only in proportion to the bits it actually changes.

use std::sync::Arc;

use super::{Content, Digest, Node};

/// Propagates executability bits onto `target` from the ancestor and (when
/// it preserves bits) the peer, per the module rules.
pub fn propagate_executability(
    ancestor: Option<&Node>,
    peer: Option<&Node>,
    target: Option<&Node>,
) -> Option<Node> {
    let target = target?;
    Some(propagate_node(ancestor, peer, target).unwrap_or_else(|| target.clone()))
}

/// Determines the vouched-for bit for a file with the specified digest.
fn desired_bit(ancestor: Option<&Node>, peer: Option<&Node>, digest: &Digest) -> bool {
    if let Some(Node {
        content:
            Content::File {
                digest: peer_digest,
                executable,
                ..
            },
        ..
    }) = peer
    {
        if peer_digest == digest {
            return *executable;
        }
    }
    if let Some(Node {
        content: Content::File { executable, .. },
        ..
    }) = ancestor
    {
        return *executable;
    }
    false
}

/// Finds the child with the specified name in a (name-sorted) reference
/// directory, advancing the merge cursor.
fn counterpart<'a>(
    reference: Option<&'a Node>,
    cursor: &mut usize,
    name: &str,
) -> Option<&'a Node> {
    let Some(Node {
        content: Content::Directory(children),
        ..
    }) = reference
    else {
        return None;
    };
    while *cursor < children.len() && children[*cursor].name.as_str() < name {
        *cursor += 1;
    }
    children.get(*cursor).filter(|child| child.name == name)
}

/// Adjusts one node per the module rules, returning the adjusted node if
/// anything changed and `None` when `target` can be used as-is.
fn propagate_node(ancestor: Option<&Node>, peer: Option<&Node>, target: &Node) -> Option<Node> {
    match &target.content {
        Content::File {
            digest,
            executable,
            metadata,
        } => {
            let desired = desired_bit(ancestor, peer, digest);
            if *executable == desired {
                return None;
            }
            Some(Node {
                name: target.name.clone(),
                content: Content::File {
                    digest: *digest,
                    executable: desired,
                    metadata: *metadata,
                },
            })
        }
        Content::Directory(target_children) => {
            let mut ancestor_cursor = 0usize;
            let mut peer_cursor = 0usize;
            let mut changes: Vec<(usize, Node)> = Vec::new();
            for (target_index, target_child) in target_children.iter().enumerate() {
                let ancestor_child =
                    counterpart(ancestor, &mut ancestor_cursor, &target_child.name);
                let peer_child = counterpart(peer, &mut peer_cursor, &target_child.name);
                if let Some(adjusted) = propagate_node(ancestor_child, peer_child, target_child) {
                    changes.push((target_index, adjusted));
                }
            }
            if changes.is_empty() {
                return None;
            }
            let mut rebuilt = target_children.as_ref().clone();
            for (index, node) in changes {
                rebuilt[index] = node;
            }
            Some(Node {
                name: target.name.clone(),
                content: Content::Directory(Arc::new(rebuilt)),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::FileMetadata;

    fn file_with(name: &str, digest_byte: u8, executable: bool) -> Node {
        Node {
            name: name.into(),
            content: Content::File {
                digest: [digest_byte; 32] as Digest,
                executable,
                metadata: FileMetadata::default(),
            },
        }
    }

    fn file(name: &str, executable: bool) -> Node {
        file_with(name, 1, executable)
    }

    fn executable_of(node: &Node, path: &str) -> bool {
        let mut current = node;
        for component in path.split('/') {
            current = current.child(component).expect("path should exist");
        }
        match &current.content {
            Content::File { executable, .. } => *executable,
            _ => panic!("expected a file"),
        }
    }

    #[test]
    fn ancestor_bits_are_grafted_and_unvouched_bits_stripped() {
        let ancestor = Node::directory(
            "",
            vec![
                Node::directory("sub", vec![file("tool", true)]),
                file("plain", false),
            ],
        );
        // The target reports noise: everything executable, including a file
        // no reference has ever seen.
        let target = Node::directory(
            "",
            vec![
                Node::directory("sub", vec![file("new", true), file("tool", false)]),
                file("plain", true),
            ],
        );
        let adjusted = propagate_executability(Some(&ancestor), None, Some(&target))
            .expect("the target should survive");
        assert!(executable_of(&adjusted, "sub/tool"));
        assert!(!executable_of(&adjusted, "plain"));
        // Nothing vouches for the new file's bit.
        assert!(!executable_of(&adjusted, "sub/new"));
    }

    #[test]
    fn ancestor_bits_survive_content_edits() {
        // An edit on the non-preserving side changes bytes, not the
        // (unstorable) bit — the ancestor's bit is grafted regardless of the
        // digest difference, so the edit doesn't silently strip
        // executability from the other side.
        let ancestor = Node::directory("", vec![file_with("script", 1, true)]);
        let target = Node::directory("", vec![file_with("script", 2, false)]);
        let adjusted = propagate_executability(Some(&ancestor), None, Some(&target))
            .expect("the target should survive");
        assert!(executable_of(&adjusted, "script"));
    }

    #[test]
    fn a_matching_peer_outvotes_the_ancestor() {
        // Both sides hold the same new bytes; the preserving peer says
        // executable while the (stale) ancestor says not. The peer's digest
        // match carries current intent.
        let ancestor = Node::directory("", vec![file_with("tool", 1, false)]);
        let peer = Node::directory("", vec![file_with("tool", 2, true)]);
        let target = Node::directory("", vec![file_with("tool", 2, false)]);
        let adjusted = propagate_executability(Some(&ancestor), Some(&peer), Some(&target))
            .expect("the target should survive");
        assert!(executable_of(&adjusted, "tool"));

        // A peer with *different* bytes vouches for nothing; the ancestor
        // fallback applies.
        let stale_peer = Node::directory("", vec![file_with("tool", 3, true)]);
        let adjusted = propagate_executability(Some(&ancestor), Some(&stale_peer), Some(&target))
            .expect("the target should survive");
        assert!(!executable_of(&adjusted, "tool"));
    }

    #[test]
    fn a_missing_ancestor_strips_unless_the_peer_matches() {
        let peer = Node::directory("", vec![file_with("a", 1, true)]);
        let target = Node::directory("", vec![file_with("a", 1, true), file_with("b", 2, true)]);
        let adjusted = propagate_executability(None, Some(&peer), Some(&target))
            .expect("the target should survive");
        assert!(executable_of(&adjusted, "a"));
        assert!(!executable_of(&adjusted, "b"));
        assert!(propagate_executability(Some(&target), None, None).is_none());
    }

    #[test]
    fn unchanged_subtrees_share_storage_with_the_input() {
        let ancestor = Node::directory(
            "",
            vec![
                Node::directory("changed", vec![file("a", true)]),
                Node::directory("same", vec![file("b", false)]),
            ],
        );
        let target = Node::directory(
            "",
            vec![
                Node::directory("changed", vec![file("a", false)]),
                Node::directory("same", vec![file("b", false)]),
            ],
        );
        let adjusted = propagate_executability(Some(&ancestor), None, Some(&target))
            .expect("the target should survive");
        assert!(executable_of(&adjusted, "changed/a"));
        // The untouched subtree is the same allocation, not a copy.
        let (Content::Directory(before), Content::Directory(after)) = (
            &target.child("same").unwrap().content,
            &adjusted.child("same").unwrap().content,
        ) else {
            panic!("expected directories");
        };
        assert!(Arc::ptr_eq(before, after));
    }

    #[test]
    fn type_mismatches_strip_rather_than_trust() {
        // The ancestor holds a directory where the target holds an
        // executable file: nothing vouches for that bit.
        let ancestor = Node::directory("", vec![Node::directory("entry", vec![])]);
        let target = Node::directory("", vec![file("entry", true)]);
        let adjusted = propagate_executability(Some(&ancestor), None, Some(&target))
            .expect("the target should survive");
        assert!(!executable_of(&adjusted, "entry"));
    }
}
