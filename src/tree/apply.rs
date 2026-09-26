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
    let mut index = 0;
    while index < changes.len() {
        let change = &changes[index];
        if change.path.is_empty() {
            root = change.new.clone().map(|mut node| {
                node.name = String::new();
                node
            });
            index += 1;
            continue;
        }

        // The changes that follow with the same parent go in together.
        // Applied one at a time, each removal or insertion shifts every
        // entry after it, so a directory of n entries losing k of them cost
        // O(k·n): emptying a flat directory of 100,000 took 24 seconds, in
        // every fold and every journal replay that carried it.
        // Reconciliation emits a directory's changes together, so one run
        // usually covers them all; changes interleaved with other
        // directories' simply make shorter runs.
        let parent = parent_of(&change.path);
        let mut end = index + 1;
        while end < changes.len()
            && !changes[end].path.is_empty()
            && parent_of(&changes[end].path) == parent
        {
            end += 1;
        }
        let run = &changes[index..end];

        let root_node = root
            .as_mut()
            .ok_or_else(|| format!("unable to resolve path {:?} in empty root", change.path))?;
        let children = directory_at(root_node, parent, &change.path)?;
        if run.len() == 1 {
            apply_one(children, leaf_of(&change.path), &change.new);
        } else {
            apply_run(children, run);
        }
        index = end;
    }
    Ok(root)
}

/// The parent path of a non-empty root-relative path (`""` for the root).
fn parent_of(path: &str) -> &str {
    path.rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or("")
}

/// The last component of a non-empty root-relative path.
fn leaf_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(_, leaf)| leaf).unwrap_or(path)
}

/// Resolves the directory at `parent`, cloning shared child vectors along
/// the way, and returns its children. `path` is the change being applied,
/// for the error.
fn directory_at<'a>(
    root: &'a mut Node,
    parent: &str,
    path: &str,
) -> Result<&'a mut Vec<Node>, String> {
    let mut current = root;
    if !parent.is_empty() {
        for component in parent.split('/') {
            let children = match &mut current.content {
                Content::Directory(children) => Arc::make_mut(children),
                _ => {
                    return Err(format!(
                        "unable to resolve parent path for {path:?}: not a directory"
                    ))
                }
            };
            let index = children
                .binary_search_by(|child| child.name.as_str().cmp(component))
                .map_err(|_| format!("unable to resolve parent path for {path:?}"))?;
            current = &mut children[index];
        }
    }
    match &mut current.content {
        Content::Directory(children) => Ok(Arc::make_mut(children)),
        _ => Err(format!(
            "unable to resolve parent path for {path:?}: not a directory"
        )),
    }
}

/// Applies one change's new content at `name` among sorted `children`.
fn apply_one(children: &mut Vec<Node>, name: &str, new: &Option<Node>) {
    let position = children.binary_search_by(|child| child.name.as_str().cmp(name));
    match (new, position) {
        (Some(new), Ok(index)) => {
            let mut node = new.clone();
            node.name = name.to_owned();
            children[index] = node;
        }
        (Some(new), Err(index)) => {
            let mut node = new.clone();
            node.name = name.to_owned();
            children.insert(index, node);
        }
        (None, Ok(index)) => {
            children.remove(index);
        }
        (None, Err(_)) => {}
    }
}

/// Applies a run of changes to one directory in a single merge: the same
/// result as applying them in order, in one pass over the children.
fn apply_run(children: &mut Vec<Node>, run: &[Change]) {
    // Sorted by name; for a name changed twice, the later change is the
    // one that stands, as it would be applied in order.
    let mut edits: Vec<(&str, usize)> = run
        .iter()
        .enumerate()
        .map(|(order, change)| (leaf_of(&change.path), order))
        .collect();
    edits.sort_by(|a, b| a.0.cmp(b.0).then(a.1.cmp(&b.1)));
    edits.dedup_by(|later, earlier| {
        if later.0 == earlier.0 {
            *earlier = *later;
            true
        } else {
            false
        }
    });

    let existing = std::mem::take(children);
    let mut merged = Vec::with_capacity(existing.len() + edits.len());
    let mut edits = edits.into_iter().peekable();
    let place = |merged: &mut Vec<Node>, name: &str, new: &Option<Node>| {
        if let Some(new) = new {
            let mut node = new.clone();
            node.name = name.to_owned();
            merged.push(node);
        }
    };
    for child in existing {
        while let Some(&(name, order)) = edits.peek() {
            if name >= child.name.as_str() {
                break;
            }
            place(&mut merged, name, &run[order].new);
            edits.next();
        }
        match edits.peek() {
            Some(&(name, order)) if name == child.name.as_str() => {
                place(&mut merged, name, &run[order].new);
                edits.next();
            }
            _ => merged.push(child),
        }
    }
    for (name, order) in edits {
        place(&mut merged, name, &run[order].new);
    }
    *children = merged;
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

    /// The algorithm this module used to be, one change at a time: the
    /// oracle the merge is held to.
    fn apply_in_order(base: Option<&Node>, changes: &[Change]) -> Result<Option<Node>, String> {
        let mut root = base.cloned();
        for change in changes {
            if change.path.is_empty() {
                root = change.new.clone().map(|mut node| {
                    node.name = String::new();
                    node
                });
                continue;
            }
            let root_node = root.as_mut().ok_or("empty root")?;
            let children = directory_at(root_node, parent_of(&change.path), &change.path)?;
            apply_one(children, leaf_of(&change.path), &change.new);
        }
        Ok(root)
    }

    fn same(a: Option<&Node>, b: Option<&Node>) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => {
                a.name == b.name
                    && a.content_equal(b, false)
                    && a.children().len() == b.children().len()
                    && a.children()
                        .iter()
                        .zip(b.children())
                        .all(|(x, y)| same(Some(x), Some(y)))
            }
            _ => false,
        }
    }

    #[test]
    fn a_run_of_changes_to_one_directory_matches_applying_them_in_order() {
        // Deterministic pseudo-random runs: creations, replacements and
        // removals, repeated names, names before, among and after the
        // existing children, and a nested directory's changes interleaved.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..200 {
            let existing: Vec<Node> = (0..(next() % 40))
                .map(|i| file(&format!("f{:03}", i * 3), (i % 250) as u8, false))
                .collect();
            let base = Node::directory(
                "",
                vec![
                    Node::directory("d", existing),
                    Node::directory("e", vec![file("x", 1, false)]),
                ],
            );
            let mut changes = Vec::new();
            for _ in 0..(1 + next() % 60) {
                let parent = if next() % 7 == 0 { "e" } else { "d" };
                let name = format!("f{:03}", next() % 130);
                let new = match next() % 3 {
                    0 => None,
                    _ => Some(file(&name, (next() % 250) as u8, false)),
                };
                changes.push(Change {
                    path: format!("{parent}/{name}"),
                    old: None,
                    new,
                });
            }
            let merged = apply(Some(&base), &changes).unwrap();
            let ordered = apply_in_order(Some(&base), &changes).unwrap();
            assert!(same(merged.as_ref(), ordered.as_ref()), "round {round}");
            // Still sorted, still name-unique.
            for directory in ["d", "e"] {
                let names: Vec<&str> = merged
                    .as_ref()
                    .unwrap()
                    .child(directory)
                    .unwrap()
                    .children()
                    .iter()
                    .map(|child| child.name.as_str())
                    .collect();
                assert!(
                    names.windows(2).all(|w| w[0] < w[1]),
                    "round {round}: {names:?}"
                );
            }
        }
    }

    #[test]
    fn emptying_a_flat_directory_of_100k_is_one_pass() {
        let children: Vec<Node> = (0..100_000)
            .map(|i| file(&format!("f{i:06}"), 1, false))
            .collect();
        let base = Node::directory("", vec![Node::directory("flat", children.clone())]);
        let changes: Vec<Change> = children
            .iter()
            .map(|child| Change {
                path: format!("flat/{}", child.name),
                old: Some(child.clone()),
                new: None,
            })
            .collect();
        let started = std::time::Instant::now();
        let result = apply(Some(&base), &changes).unwrap().unwrap();
        assert!(result.child("flat").unwrap().children().is_empty());
        // 24 s one at a time; a single merge is milliseconds. The bound is
        // loose so a slow CI runner cannot fail it.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }
}
