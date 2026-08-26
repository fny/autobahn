//! Hierarchy diffing.

use super::{path_join, Change, Node};

/// Recursively diffs two optional hierarchies, appending changes that would
/// transform `base` into `target`.
fn diff_recursive(
    path: &str,
    base: Option<&Node>,
    target: Option<&Node>,
    changes: &mut Vec<Change>,
) {
    // If the content at this path isn't (shallowly) equal, then record a
    // complete replacement.
    let equal = match (base, target) {
        (None, None) => true,
        (Some(b), Some(t)) => b.content_equal(t, false),
        _ => false,
    };
    if !equal {
        changes.push(Change {
            path: path.to_owned(),
            old: base.cloned(),
            new: target.cloned(),
        });
        return;
    }

    // Identical storage cannot hold differing content: an unchanged scan
    // adopts its baseline's children, so whole subtrees compare here in
    // constant time instead of being walked to prove they agree.
    if super::roots_share_storage(base, target) {
        return;
    }

    // The content was equal at this path, so diff children with a linear
    // merge over the (name-sorted) child lists.
    let base_children = base.map(Node::children).unwrap_or(&[]);
    let target_children = target.map(Node::children).unwrap_or(&[]);
    let (mut i, mut j) = (0, 0);
    while i < base_children.len() || j < target_children.len() {
        let order = match (base_children.get(i), target_children.get(j)) {
            (Some(b), Some(t)) => b.name.cmp(&t.name),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => unreachable!(),
        };
        match order {
            std::cmp::Ordering::Less => {
                let child = &base_children[i];
                diff_recursive(&path_join(path, &child.name), Some(child), None, changes);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                let child = &target_children[j];
                diff_recursive(&path_join(path, &child.name), None, Some(child), changes);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                let child = &base_children[i];
                diff_recursive(
                    &path_join(path, &child.name),
                    Some(child),
                    Some(&target_children[j]),
                    changes,
                );
                i += 1;
                j += 1;
            }
        }
    }
}

/// Diffs two optional hierarchies rooted at the specified path, generating
/// changes that, if applied to `base`, would transform it into `target`.
pub fn diff_at(path: &str, base: Option<&Node>, target: Option<&Node>) -> Vec<Change> {
    let mut changes = Vec::new();
    diff_recursive(path, base, target, &mut changes);
    changes
}

/// Diffs two optional hierarchies rooted at the synchronization root.
pub fn diff(base: Option<&Node>, target: Option<&Node>) -> Vec<Change> {
    diff_at("", base, target)
}

#[cfg(test)]
mod tests {
    use super::super::tests::file;
    use super::*;
    use crate::tree::Node;

    #[test]
    fn diff_detects_changes() {
        let base = Node::directory("", vec![file("a", 1, false), file("b", 2, false)]);
        let target = Node::directory(
            "",
            vec![
                file("a", 1, false),
                file("b", 3, false),
                file("c", 4, false),
            ],
        );
        let changes = diff(Some(&base), Some(&target));
        let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, vec!["b", "c"]);
        assert!(changes[0].old.is_some() && changes[0].new.is_some());
        assert!(changes[1].old.is_none() && changes[1].new.is_some());
    }

    #[test]
    fn diff_of_identical_hierarchies_is_empty() {
        let base = Node::directory("", vec![file("a", 1, false)]);
        assert!(diff(Some(&base), Some(&base.clone())).is_empty());
    }
}
