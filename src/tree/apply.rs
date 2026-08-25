//! Change application (ancestor updates).

use std::sync::Arc;

use super::{Change, Content, Node};

/// Applies a series of changes to an optional base hierarchy, returning the
/// resulting hierarchy. Only the `new` side of each change is used. The
/// result shares unmodified subtrees with the base and with the changes' new
/// hierarchies: copy-on-write happens naturally through [`Arc::make_mut`],
/// which clones a child vector only when it's shared — the entire class of
/// stale-aliasing bugs that copy-on-write bookkeeping invites in other
/// languages is structurally impossible here.
///
/// Changes must be ordered so that parents precede their children when both
/// are changed (which reconciliation guarantees). Application fails only if
/// a change's parent path can't be resolved to a directory.
pub fn apply(base: Option<&Node>, changes: &[Change]) -> Result<Option<Node>, String> {
    let mut root = base.cloned();
    for change in changes {
        if change.path.is_empty() {
            root = change.new.clone().map(|mut node| {
                node.name = String::new();
                node
            });
            continue;
        }

        // Resolve the parent directory chain, cloning shared child vectors
        // along the way.
        let root_node = root
            .as_mut()
            .ok_or_else(|| format!("unable to resolve path {:?} in empty root", change.path))?;
        let mut current = root_node;
        let mut components = change.path.split('/').peekable();
        while let Some(component) = components.next() {
            let is_leaf = components.peek().is_none();
            let children = match &mut current.content {
                Content::Directory(children) => Arc::make_mut(children),
                _ => {
                    return Err(format!(
                        "unable to resolve parent path for {:?}: not a directory",
                        change.path
                    ))
                }
            };
            let position = children.binary_search_by(|child| child.name.as_str().cmp(component));
            if is_leaf {
                match (&change.new, position) {
                    (Some(new), Ok(index)) => {
                        let mut node = new.clone();
                        node.name = component.to_owned();
                        children[index] = node;
                    }
                    (Some(new), Err(index)) => {
                        let mut node = new.clone();
                        node.name = component.to_owned();
                        children.insert(index, node);
                    }
                    (None, Ok(index)) => {
                        children.remove(index);
                    }
                    (None, Err(_)) => {}
                }
                break;
            }
            let index = position
                .map_err(|_| format!("unable to resolve parent path for {:?}", change.path))?;
            current = &mut children[index];
        }
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::super::tests::file;
    use super::*;
    use crate::tree::{diff, Node};

    #[test]
    fn apply_round_trips_diff() {
        let base = Node::directory(
            "",
            vec![
                Node::directory("d", vec![file("x", 1, false), file("y", 2, false)]),
                file("top", 3, false),
            ],
        );
        let target = Node::directory(
            "",
            vec![
                Node::directory("d", vec![file("x", 9, true), file("z", 5, false)]),
                file("added", 4, false),
            ],
        );
        let changes = diff(Some(&base), Some(&target));
        let applied = apply(Some(&base), &changes).unwrap().unwrap();
        assert!(applied.content_equal(&target, true));
    }

    #[test]
    fn apply_does_not_mutate_base() {
        let base = Node::directory("", vec![Node::directory("d", vec![file("x", 1, false)])]);
        let replacement = file("x", 7, false);
        let changes = vec![Change {
            path: "d/x".into(),
            old: None,
            new: Some(replacement),
        }];
        let applied = apply(Some(&base), &changes).unwrap().unwrap();
        // The base must be untouched (copy-on-write must have cloned the
        // shared child vectors rather than mutating them).
        match &base.child("d").unwrap().child("x").unwrap().content {
            Content::File { digest, .. } => assert_eq!(digest[0], 1),
            _ => panic!("expected file"),
        }
        match &applied.child("d").unwrap().child("x").unwrap().content {
            Content::File { digest, .. } => assert_eq!(digest[0], 7),
            _ => panic!("expected file"),
        }
    }

    #[test]
    fn apply_handles_root_replacement_and_deletion() {
        let base = Node::directory("", vec![file("a", 1, false)]);
        let deleted = apply(
            Some(&base),
            &[Change {
                path: String::new(),
                old: None,
                new: None,
            }],
        )
        .unwrap();
        assert!(deleted.is_none());
        let recreated = apply(
            None,
            &[Change {
                path: String::new(),
                old: None,
                new: Some(base.clone()),
            }],
        )
        .unwrap();
        assert!(recreated.unwrap().content_equal(&base, true));
    }

    #[test]
    fn apply_supports_creation_then_nested_modification() {
        let base = Node::directory("", vec![]);
        let created = Node::directory("d", vec![file("x", 1, false)]);
        let changes = vec![
            Change {
                path: "d".into(),
                old: None,
                new: Some(created),
            },
            Change {
                path: "d/x".into(),
                old: None,
                new: Some(file("x", 2, false)),
            },
        ];
        let applied = apply(Some(&base), &changes).unwrap().unwrap();
        match &applied.child("d").unwrap().child("x").unwrap().content {
            Content::File { digest, .. } => assert_eq!(digest[0], 2),
            _ => panic!("expected file"),
        }
    }
}
