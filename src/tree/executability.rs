//! Executability propagation.
//!
//! On a filesystem that doesn't preserve executability bits (the FAT
//! family, most prominently), the bits a scan reports are noise — often
//! every file reads as executable, or none does. Fed directly into
//! reconciliation, that noise would masquerade as content changes: phantom
//! modifications that churn permissions on the other side forever. The fix
//! (Mutagen's `PropagateExecutability`) is to graft the executability bits
//! from a trusted reference — the ancestor — onto the affected side's
//! snapshot before reconciliation, so only real content differences remain.
//!
//! Grafting is structural and copy-on-write: subtrees without any grafted
//! bit share their storage with the input snapshot, so the pass costs
//! allocation only in proportion to the bits it actually changes.

use std::sync::Arc;

use super::{Content, Node};

/// Propagates executability bits from `reference` onto `target`, returning
/// the adjusted hierarchy. Bits are grafted wherever both hierarchies hold a
/// file at the same path; everything else is left untouched.
pub fn propagate_executability(reference: Option<&Node>, target: Option<&Node>) -> Option<Node> {
    match (reference, target) {
        (Some(reference), Some(target)) => {
            Some(propagate_node(reference, target).unwrap_or_else(|| target.clone()))
        }
        (_, target) => target.cloned(),
    }
}

/// Grafts executability from `reference` onto `target`, returning the
/// adjusted node if anything changed and `None` when `target` can be used
/// as-is.
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
            // of each target child is found by a linear merge.
            let mut changes: Vec<(usize, Node)> = Vec::new();
            let mut reference_index = 0usize;
            for (target_index, target_child) in target_children.iter().enumerate() {
                while reference_index < reference_children.len()
                    && reference_children[reference_index].name < target_child.name
                {
                    reference_index += 1;
                }
                let Some(reference_child) = reference_children.get(reference_index) else {
                    break;
                };
                if reference_child.name != target_child.name {
                    continue;
                }
                if let Some(changed) = propagate_node(reference_child, target_child) {
                    changes.push((target_index, changed));
                }
            }
            if changes.is_empty() {
                return None;
            }
            let mut children = target_children.as_ref().clone();
            for (index, node) in changes {
                children[index] = node;
            }
            Some(Node {
                name: target.name.clone(),
                content: Content::Directory(Arc::new(children)),
            })
        }
        _ => None,
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
    fn bits_are_grafted_where_both_sides_hold_files() {
        let reference = Node::directory(
            "",
            vec![
                Node::directory("sub", vec![file("tool", true)]),
                file("plain", false),
            ],
        );
        // The target reports noise: everything executable.
        let target = Node::directory(
            "",
            vec![
                Node::directory("sub", vec![file("tool", false), file("new", true)]),
                file("plain", true),
            ],
        );
        let adjusted = propagate_executability(Some(&reference), Some(&target))
            .expect("the target should survive");
        // Grafted where both sides hold files...
        assert!(executable_of(&adjusted, "sub/tool"));
        assert!(!executable_of(&adjusted, "plain"));
        // ...and untouched where the reference has no counterpart.
        assert!(executable_of(&adjusted, "sub/new"));
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
    fn a_fully_unchanged_target_is_returned_as_is() {
        let tree = Node::directory("", vec![file("a", true), file("b", false)]);
        let adjusted = propagate_executability(Some(&tree), Some(&tree.clone()))
            .expect("the target should survive");
        assert!(adjusted.content_equal(&tree, true));
    }

    #[test]
    fn type_mismatches_and_missing_references_are_left_alone() {
        let reference = Node::directory("", vec![Node::directory("entry", vec![])]);
        let target = Node::directory("", vec![file("entry", true)]);
        let adjusted = propagate_executability(Some(&reference), Some(&target))
            .expect("the target should survive");
        assert!(executable_of(&adjusted, "entry"));

        // No reference at all: the target passes through.
        let passed = propagate_executability(None, Some(&target)).expect("target");
        assert!(passed.content_equal(&target, true));
        assert!(propagate_executability(Some(&reference), None).is_none());
    }
}
