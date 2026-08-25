//! Executability propagation.
//!
//! On a filesystem that doesn't preserve executability bits (the FAT
//! family, most prominently), the bits a scan reports are noise — often
//! every file reads as executable, or none does. Fed directly into
//! reconciliation, that noise would masquerade as content changes: phantom
//! modifications that churn permissions on the other side forever. The fix
//! (Mutagen's shape) is strip-then-graft: a non-preserving side contributes
//! no executability information of its own, so its snapshot's bits are
//! replaced wholesale — grafted from a trusted reference (the ancestor)
//! where the reference holds a file at the same path, and stripped to
//! non-executable everywhere else. Only real content differences remain for
//! reconciliation to see.
//!
//! The pass is structural and copy-on-write: subtrees without any adjusted
//! bit share their storage with the input snapshot, so it costs allocation
//! only in proportion to the bits it actually changes.

use std::sync::Arc;

use super::{Content, Node};

/// Propagates executability bits from `reference` onto `target`, returning
/// the adjusted hierarchy: files with a file counterpart in the reference
/// take the reference's bit, and files without one are stripped to
/// non-executable (this is what keeps a volume that reports every file
/// executable from marking freshly created content executable everywhere
/// else).
pub fn propagate_executability(reference: Option<&Node>, target: Option<&Node>) -> Option<Node> {
    let target = target?;
    let adjusted = match reference {
        Some(reference) => propagate_node(reference, target),
        None => strip_node(target),
    };
    Some(adjusted.unwrap_or_else(|| target.clone()))
}

/// Rebuilds a directory node with the specified child replacements
/// (returning `None`, meaning "use the input as-is", when there are none).
fn rebuild(target: &Node, children: &Arc<Vec<Node>>, changes: Vec<(usize, Node)>) -> Option<Node> {
    if changes.is_empty() {
        return None;
    }
    let mut rebuilt = children.as_ref().clone();
    for (index, node) in changes {
        rebuilt[index] = node;
    }
    Some(Node {
        name: target.name.clone(),
        content: Content::Directory(Arc::new(rebuilt)),
    })
}

/// Strips executability throughout a hierarchy, returning the adjusted node
/// if anything changed.
fn strip_node(target: &Node) -> Option<Node> {
    match &target.content {
        Content::File {
            digest,
            executable: true,
            metadata,
        } => Some(Node {
            name: target.name.clone(),
            content: Content::File {
                digest: *digest,
                executable: false,
                metadata: *metadata,
            },
        }),
        Content::Directory(children) => {
            let changes: Vec<(usize, Node)> = children
                .iter()
                .enumerate()
                .filter_map(|(index, child)| strip_node(child).map(|node| (index, node)))
                .collect();
            rebuild(target, children, changes)
        }
        _ => None,
    }
}

/// Grafts executability from `reference` onto `target` (stripping wherever
/// the reference holds no file counterpart), returning the adjusted node if
/// anything changed and `None` when `target` can be used as-is.
fn propagate_node(reference: &Node, target: &Node) -> Option<Node> {
    match (&reference.content, &target.content) {
        (
            Content::File {
                executable: reference_executable,
                ..
            },
            Content::File {
                digest,
                executable,
                metadata,
            },
        ) => {
            if executable == reference_executable {
                return None;
            }
            Some(Node {
                name: target.name.clone(),
                content: Content::File {
                    digest: *digest,
                    executable: *reference_executable,
                    metadata: *metadata,
                },
            })
        }
        (Content::Directory(reference_children), Content::Directory(target_children)) => {
            // Both child lists are name-sorted, so the reference counterpart
            // of each target child is found by a linear merge; children the
            // reference doesn't vouch for are stripped.
            let mut changes: Vec<(usize, Node)> = Vec::new();
            let mut reference_index = 0usize;
            for (target_index, target_child) in target_children.iter().enumerate() {
                while reference_index < reference_children.len()
                    && reference_children[reference_index].name < target_child.name
                {
                    reference_index += 1;
                }
                let counterpart = reference_children
                    .get(reference_index)
                    .filter(|child| child.name == target_child.name);
                let adjusted = match counterpart {
                    Some(reference_child) => propagate_node(reference_child, target_child),
                    None => strip_node(target_child),
                };
                if let Some(adjusted) = adjusted {
                    changes.push((target_index, adjusted));
                }
            }
            rebuild(target, target_children, changes)
        }
        // A type mismatch means the reference vouches for nothing here.
        _ => strip_node(target),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Digest, FileMetadata};

    fn file(name: &str, executable: bool) -> Node {
        Node {
            name: name.into(),
            content: Content::File {
                digest: [1u8; 32] as Digest,
                executable,
                metadata: FileMetadata::default(),
            },
        }
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
    fn bits_are_grafted_where_vouched_and_stripped_where_not() {
        let reference = Node::directory(
            "",
            vec![
                Node::directory("sub", vec![file("tool", true)]),
                file("plain", false),
            ],
        );
        // The target reports noise: everything executable, including a file
        // the reference has never seen.
        let target = Node::directory(
            "",
            vec![
                Node::directory("sub", vec![file("new", true), file("tool", false)]),
                file("plain", true),
            ],
        );
        let adjusted = propagate_executability(Some(&reference), Some(&target))
            .expect("the target should survive");
        // Grafted where the reference holds a file...
        assert!(executable_of(&adjusted, "sub/tool"));
        assert!(!executable_of(&adjusted, "plain"));
        // ...and stripped where it doesn't: a non-preserving volume can't
        // vouch for a new file's executability.
        assert!(!executable_of(&adjusted, "sub/new"));
    }

    #[test]
    fn a_missing_reference_strips_everything() {
        let target = Node::directory(
            "",
            vec![file("a", true), Node::directory("d", vec![file("b", true)])],
        );
        let adjusted =
            propagate_executability(None, Some(&target)).expect("the target should survive");
        assert!(!executable_of(&adjusted, "a"));
        assert!(!executable_of(&adjusted, "d/b"));
        assert!(propagate_executability(Some(&target), None).is_none());
    }

    #[test]
    fn unchanged_subtrees_share_storage_with_the_input() {
        let reference = Node::directory(
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
        let adjusted = propagate_executability(Some(&reference), Some(&target))
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
    fn a_fully_vouched_unchanged_target_is_returned_as_is() {
        let tree = Node::directory("", vec![file("a", true), file("b", false)]);
        let adjusted = propagate_executability(Some(&tree), Some(&tree.clone()))
            .expect("the target should survive");
        assert!(adjusted.content_equal(&tree, true));
    }

    #[test]
    fn type_mismatches_strip_rather_than_trust() {
        // The reference holds a directory where the target holds an
        // executable file: nothing vouches for that bit.
        let reference = Node::directory("", vec![Node::directory("entry", vec![])]);
        let target = Node::directory("", vec![file("entry", true)]);
        let adjusted = propagate_executability(Some(&reference), Some(&target))
            .expect("the target should survive");
        assert!(!executable_of(&adjusted, "entry"));
    }
}
