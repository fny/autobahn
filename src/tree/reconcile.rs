//! Three-way reconciliation.
//!
//! This is a faithful port of Mutagen's reconciliation semantics (which this
//! project derives from): a recursive three-way merge between an ancestor
//! (the last synchronized state, containing only synchronizable content) and
//! the current alpha and beta states, producing ancestor updates, alpha
//! transitions, beta transitions, and conflicts. See the extensive reasoning
//! in Mutagen's `reconcile.go` for the derivation of each rule; comments
//! here summarize rather than re-derive.

use super::{diff_at, path_join, Change, Conflict, Content, Node, SyncMode};

/// The outcome of reconciliation.
#[derive(Debug, Default)]
pub struct Reconciliation {
    /// Changes to apply to the ancestor (beyond those implied by successful
    /// transitions).
    pub ancestor_changes: Vec<Change>,
    /// Transitions to perform on alpha.
    pub alpha_transitions: Vec<Change>,
    /// Transitions to perform on beta.
    pub beta_transitions: Vec<Change>,
    /// Conflicts between alpha and beta.
    pub conflicts: Vec<Conflict>,
}

/// The recursive reconciler.
struct Reconciler<'m> {
    mode: SyncMode,
    result: Reconciliation,
    /// The previous reconciliation of the same session, when there is one
    /// to skip against: see [`reconcile_since`].
    memo: Option<&'m ReconcileMemo>,
}

/// What one reconciliation needs to remember so the next can skip what did
/// not change: its three inputs, and every path where it produced anything.
///
/// Reconciling a subtree depends on nothing but the subtree's path, its
/// three inputs and the mode — the reconciler keeps no state across
/// subtrees — so three inputs storage-identical to last time, where last
/// time produced nothing, produce nothing again. A scan adopts what it did
/// not revisit, a snapshot applied from changes shares what did not
/// change, and the ancestor is updated copy-on-write, so from one cycle to
/// the next nearly every subtree is exactly that. Walking them all again
/// was about a third of the controller's time per changed cycle at 420k
/// files.
#[derive(Clone, Debug, Default)]
pub struct ReconcileMemo {
    ancestor: Option<Node>,
    alpha: Option<Node>,
    beta: Option<Node>,
    /// Every path the reconciliation produced a change, transition or
    /// conflict at, sorted.
    produced: Vec<String>,
}

impl ReconcileMemo {
    /// Remembers a reconciliation of these inputs, for the next.
    pub fn of(
        ancestor: Option<&Node>,
        alpha: Option<&Node>,
        beta: Option<&Node>,
        result: &Reconciliation,
    ) -> ReconcileMemo {
        let mut produced: Vec<String> = result
            .ancestor_changes
            .iter()
            .chain(&result.alpha_transitions)
            .chain(&result.beta_transitions)
            .map(|change| change.path.clone())
            .chain(
                result
                    .conflicts
                    .iter()
                    .map(|conflict| conflict.root.clone()),
            )
            .collect();
        produced.sort();
        produced.dedup();
        ReconcileMemo {
            ancestor: ancestor.cloned(),
            alpha: alpha.cloned(),
            beta: beta.cloned(),
            produced,
        }
    }

    /// Whether anything was produced exactly at `path`.
    fn produced_at(&self, path: &str) -> bool {
        self.produced
            .binary_search_by(|p| p.as_str().cmp(path))
            .is_ok()
    }

    /// Whether anything was produced at `path` or beneath it.
    fn produced_within(&self, path: &str) -> bool {
        if path.is_empty() {
            return !self.produced.is_empty();
        }
        // Everything starting with `path` sorts together, from where `path`
        // itself would; among them are siblings like `path-x`, which is why
        // the separator is checked.
        let start = self.produced.partition_point(|p| p.as_str() < path);
        self.produced[start..]
            .iter()
            .take_while(|p| p.starts_with(path))
            .any(|p| p.len() == path.len() || p.as_bytes()[path.len()] == b'/')
    }
}

/// The inputs a subtree was reconciled with last time, when they can be
/// trusted to be exactly that.
type Previous<'m> = Option<(Option<&'m Node>, Option<&'m Node>, Option<&'m Node>)>;

/// Whether two optional nodes are the same storage: both absent, or both
/// directories sharing their children.
fn identical(now: Option<&Node>, then: Option<&Node>) -> bool {
    match (now, then) {
        (None, None) => true,
        (Some(now), Some(then)) => super::nodes_share_storage(Some(now), Some(then)),
        _ => false,
    }
}

/// Extracts the non-deletion changes (creations and modifications) from a
/// change list.
fn non_deletion_changes(changes: &[Change]) -> Vec<Change> {
    changes
        .iter()
        .filter(|c| c.new.is_some())
        .cloned()
        .collect()
}

/// The part of a side's unsynchronizable content that must block the change
/// about to be propagated onto it at `path`, where the ancestor recorded
/// `ancestor`.
///
/// What blocks depends on the change, not only on the content. Excluded
/// content must never be *overwritten* — "do not synchronize this" cannot
/// mean "replace it with the peer's copy" — but an entry synchronization
/// never carried need not block a *deletion*. Treating it as an obstacle
/// in both directions made an ordinary action impossible: a project
/// directory almost always holds a `.git` or a `node_modules`, so deleting
/// one turned into a conflict that no resolution could settle.
fn blocking(
    path: &str,
    ancestor: Option<&Node>,
    incoming: Option<&Node>,
    unsynchronizable: Vec<Change>,
) -> Vec<Change> {
    fn unreadable(node: &Node) -> bool {
        match &node.content {
            Content::Problematic { .. } => true,
            Content::Directory(children) => children.iter().any(unreadable),
            _ => false,
        }
    }
    // Content arriving would be written *over* whatever is here, and an
    // excluded entry is exactly the thing that must not be overwritten:
    // "do not synchronize this" cannot mean "replace it with the peer's
    // copy". Everything unsynchronizable blocks a write.
    if incoming.is_some() {
        return unsynchronizable;
    }
    // A deletion is different. It is the directory around an excluded
    // entry that goes, and the endpoint takes a pattern-ignored entry with
    // it, as `docs/ignores.md` describes; an entry excluded only by size,
    // type or symlink mode it refuses to remove, and leaves standing with
    // a problem reported. Blocking the deletion for either would make an
    // ordinary action impossible, since a project directory almost always
    // holds a `.git` or a `node_modules` and could then never be deleted
    // through synchronization at all.
    //
    // Two kinds of content still block even a deletion, because what
    // stands behind them is a change nobody has weighed. Unreadable
    // content: nobody has seen what is there, so removing the directory
    // around it is not a decision anyone made. And an entry excluded where
    // the ancestor recorded content: the file was synchronized, and since
    // then it grew past the size limit, became a FIFO, or started to match
    // an ignore — an edit reconciliation cannot see. Letting a deletion of
    // its directory through would destroy it; it is a conflict instead.
    let recorded = |change: &Change| {
        let relative = if path.is_empty() {
            Some(change.path.as_str())
        } else if change.path == path {
            Some("")
        } else {
            change
                .path
                .strip_prefix(path)
                .and_then(|rest| rest.strip_prefix('/'))
        };
        relative.is_some_and(|relative| super::node_at(ancestor, relative).is_some())
    };
    unsynchronizable
        .into_iter()
        .filter(|change| {
            change.new.as_ref().is_some_and(unreadable)
                || change.old.as_ref().is_some_and(unreadable)
                || (matches!(
                    change.new.as_ref().map(|n| &n.content),
                    Some(Content::Untracked)
                ) && recorded(change))
        })
        .collect()
}

/// The synchronizable part of a side's content at a path: what a
/// transition on that side carries as `new`, and what it must name as
/// `old`. The endpoint checks every entry it finds against `old` before
/// it acts, and it never removes an untracked entry it was told to
/// expect, so an `old` taken from the raw scan, ignored entries and all,
/// is refused every cycle and the transition comes back forever (M-33).
fn synchronized(side: Option<&Node>) -> Option<Node> {
    side.and_then(Node::synchronizable_subtree)
}

/// Indicates whether or not optional content is nil-or-untracked.
fn nil_or_untracked(node: Option<&Node>) -> bool {
    match node {
        None => true,
        Some(node) => matches!(node.content, Content::Untracked),
    }
}

/// Indicates whether or not optional content is problematic.
fn problematic(node: Option<&Node>) -> bool {
    matches!(node, Some(node) if matches!(node.content, Content::Problematic { .. }))
}

/// Performs a shallow content equality comparison between optional nodes.
fn shallow_equal(a: Option<&Node>, b: Option<&Node>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.content_equal(b, false),
        _ => false,
    }
}

/// The size at which the paranoid mode stops trusting a one-sided
/// disappearance. A vanished mount, a wiped checkout, or a tool's cleanup
/// usually took a substantial tree with it; emptying or removing a small
/// directory is ordinary housekeeping and propagates in every mode.
pub const PARANOID_MINIMUM: usize = 8;

/// Whether the ancestor records a directory large enough for the paranoid
/// mode to guard. Counted only once the cheap shape test has fired, which
/// keeps the guard off the per-cycle cost of every ordinary reconciliation:
/// a whole-tree pass measured at twenty milliseconds per cycle on a
/// sixty-thousand-entry tree.
fn large_in_ancestor(ancestor: Option<&Node>) -> bool {
    fn entries_below(node: &Node) -> usize {
        node.children()
            .iter()
            .map(|child| 1 + entries_below(child))
            .sum()
    }
    ancestor.is_some_and(|node| {
        matches!(node.content, Content::Directory(_)) && entries_below(node) >= PARANOID_MINIMUM
    })
}

impl<'m> Reconciler<'m> {
    fn reconcile(
        &mut self,
        path: &str,
        ancestor: Option<&Node>,
        alpha: Option<&Node>,
        beta: Option<&Node>,
        previous: Previous<'m>,
    ) {
        // The same three inputs as last time, where last time produced
        // nothing: nothing again. See [`ReconcileMemo`].
        if let (Some((then_ancestor, then_alpha, then_beta)), Some(memo)) = (previous, self.memo) {
            if identical(ancestor, then_ancestor)
                && identical(alpha, then_alpha)
                && identical(beta, then_beta)
                && !memo.produced_within(path)
            {
                return;
            }
        }

        // If either side is purely problematic at this path, then there's
        // nothing safe to do here: the problem is already surfaced as a scan
        // problem.
        if problematic(alpha) || problematic(beta) {
            return;
        }

        // Both sides genuinely absent: the content is gone everywhere, and
        // the ancestor entry goes with it. Both sides *untracked* is a
        // different situation entirely — the content still exists, policy
        // has merely excluded it — and the ancestor entry is preserved.
        // Clearing it discarded provenance across a policy change: a file
        // ignored on both sides for a while, deliberately deleted on one
        // side during that window, was resurrected from the other side
        // when the ignore was lifted, because without the ancestor the
        // survivor read as a brand-new creation.
        if alpha.is_none() && beta.is_none() {
            if ancestor.is_some() {
                self.result.ancestor_changes.push(Change {
                    path: path.to_owned(),
                    old: None,
                    new: None,
                });
            }
            return;
        }
        if nil_or_untracked(alpha) && nil_or_untracked(beta) {
            // At least one side is untracked (both-none returned above).
            // Nothing to synchronize while policy excludes it; whatever
            // the ancestor holds stays, so re-inclusion resumes as an
            // ordinary three-way reconciliation with real provenance.
            return;
        }

        // A single untracked side over an existing ancestor is content that
        // synchronization deliberately leaves alone (a file that crossed a
        // size limit, an entry that stopped being synchronizable). It
        // neither offers changes nor can receive them, and it must never
        // read as a deletion — so both sides and the ancestor are
        // preserved, and content crossing back into tracked scope later
        // resumes as an ordinary three-way update against that ancestor.
        // (Without an ancestor, the existing disagreement handling already
        // surfaces such content as a conflict rather than propagating.)
        let untracked = |node: Option<&Node>| matches!(node, Some(node) if matches!(node.content, Content::Untracked));
        if (untracked(alpha) || untracked(beta)) && ancestor.is_some() {
            return;
        }

        // If alpha and beta agree (shallowly) at this path, then recurse.
        if shallow_equal(alpha, beta) {
            // The paranoid mode's emptied-directory guard: both sides hold
            // a directory here, exactly one of them is empty, and the
            // ancestor says it was substantial. Plain three-way merging
            // would read that as one side deleting every entry and carry
            // the deletions across; the shape is just as often a mount
            // that went away and left its mountpoint behind, or a tool
            // that swept a directory clean (`git gc` packing loose refs),
            // so the paranoid mode reports it as a conflict at the
            // directory and lets a person name the winner. The full side's
            // entries are not walked, so nothing beneath moves until then.
            //
            // Every other mode propagates it. This was once a halt in all
            // of them, and it stopped whole sessions for exactly the tool
            // cleanups above.
            if self.mode == SyncMode::TwoWayParanoid && !path.is_empty() {
                // Empty means nothing synchronizable: a directory emptied
                // down to one ignored entry is the same shape.
                let empty = |node: Option<&Node>| {
                    matches!(node, Some(node)
                        if matches!(node.content, Content::Directory(_))
                            && !node.holds_synchronizable())
                };
                if empty(alpha) != empty(beta) && large_in_ancestor(ancestor) {
                    let change = |side: Option<&Node>| Change {
                        path: path.to_owned(),
                        old: ancestor.cloned(),
                        new: synchronized(side),
                    };
                    self.result.conflicts.push(Conflict {
                        root: path.to_owned(),
                        alpha_changes: vec![change(alpha)],
                        beta_changes: vec![change(beta)],
                    });
                    return;
                }
            }
            // If the ancestor disagrees, then record an ancestor update at
            // this path (enabling "both modified same" reconciliation) and
            // don't let the old ancestor contents drive recursion.
            let mut ancestor_children: &[Node] = ancestor.map(Node::children).unwrap_or(&[]);
            if !shallow_equal(ancestor, alpha) {
                let slim = alpha.map(|node| Node {
                    name: node.name.clone(),
                    content: match &node.content {
                        Content::Directory(_) => Content::Directory(Default::default()),
                        other => other.clone(),
                    },
                });
                self.result.ancestor_changes.push(Change {
                    path: path.to_owned(),
                    old: None,
                    new: slim,
                });
                ancestor_children = &[];
            }

            // Recurse over the union of child names with a three-way linear
            // merge (all child lists are name-sorted).
            let alpha_children = alpha.map(Node::children).unwrap_or(&[]);
            let beta_children = beta.map(Node::children).unwrap_or(&[]);
            let (mut i, mut j, mut k) = (0, 0, 0);
            while i < ancestor_children.len() || j < alpha_children.len() || k < beta_children.len()
            {
                // Determine the smallest name among the remaining children.
                let mut name: Option<&str> = None;
                for candidate in [
                    ancestor_children.get(i).map(|n| n.name.as_str()),
                    alpha_children.get(j).map(|n| n.name.as_str()),
                    beta_children.get(k).map(|n| n.name.as_str()),
                ]
                .into_iter()
                .flatten()
                {
                    name = Some(match name {
                        Some(current) if current <= candidate => current,
                        _ => candidate,
                    });
                }
                let name = name.expect("non-empty merge frontier");

                // Extract the children carrying that name.
                let ancestor_child = match ancestor_children.get(i) {
                    Some(child) if child.name == name => {
                        i += 1;
                        Some(child)
                    }
                    _ => None,
                };
                let alpha_child = match alpha_children.get(j) {
                    Some(child) if child.name == name => {
                        j += 1;
                        Some(child)
                    }
                    _ => None,
                };
                let beta_child = match beta_children.get(k) {
                    Some(child) if child.name == name => {
                        k += 1;
                        Some(child)
                    }
                    _ => None,
                };

                let child_path = path_join(path, name);
                // Last time's inputs for the child are the children of last
                // time's here only if last time recursed here with its real
                // ancestor: all three were directories (anything else
                // returns or dispatches without recursing, or clears the
                // ancestor's children) and it produced nothing at exactly
                // this path (a conflict here, or an ancestor update, means
                // it did not recurse as now). Otherwise nothing below is
                // skipped.
                let child_previous = previous.and_then(|(a, x, y)| {
                    let directory = |node: Option<&'m Node>| {
                        matches!(node, Some(node) if matches!(node.content, Content::Directory(_)))
                    };
                    let recursed = directory(a)
                        && directory(x)
                        && directory(y)
                        && !self.memo.is_some_and(|memo| memo.produced_at(path));
                    recursed.then(|| {
                        (
                            a.and_then(|node| node.child(name)),
                            x.and_then(|node| node.child(name)),
                            y.and_then(|node| node.child(name)),
                        )
                    })
                });
                self.reconcile(
                    &child_path,
                    ancestor_child,
                    alpha_child,
                    beta_child,
                    child_previous,
                );
            }
            return;
        }

        // Alpha and beta disagree at this path; dispatch by mode.
        match self.mode {
            SyncMode::TwoWaySafe
            | SyncMode::TwoWayParanoid
            | SyncMode::TwoWayResolved
            | SyncMode::TwoWayStrict => {
                self.handle_disagreement_bidirectional(path, ancestor, alpha, beta)
            }
            SyncMode::OneWaySafe => {
                self.handle_disagreement_one_way_safe(path, ancestor, alpha, beta)
            }
            SyncMode::OneWayReplica => {
                self.handle_disagreement_one_way_replica(path, ancestor, alpha, beta)
            }
        }
    }

    fn handle_disagreement_bidirectional(
        &mut self,
        path: &str,
        ancestor: Option<&Node>,
        alpha: Option<&Node>,
        beta: Option<&Node>,
    ) {
        // Extract the synchronizable portion of each side.
        let alpha_sync = synchronized(alpha);
        let beta_sync = synchronized(beta);

        // Classic three-way merge: if one side is unmodified, propagate the
        // other side's synchronizable content (unless the unmodified side
        // carries unsynchronizable content, which indicates a conflict).
        let alpha_diff = diff_at(path, ancestor, alpha_sync.as_ref());
        let beta_diff = diff_at(path, ancestor, beta_sync.as_ref());

        // The paranoid mode's other rule: a large directory that is gone
        // on one side while the other still holds exactly what the
        // ancestor recorded is restored, not deleted. Partly for its own
        // sake — the mode exists for people who would rather re-delete
        // than lose a tree to a vanished disk — and partly because it is
        // what makes the emptied-directory conflict above resolvable in
        // the full side's favour: `resolve` retires the empty directory,
        // which leaves precisely this shape, and without this rule the
        // untouched side would then follow the deletion it was meant to
        // win against. A deletion made against a *changed* other side is
        // not this shape, so the emptying side can still win: retiring the
        // full copy leaves two pure deletions, and the fuller one carries.
        if self.mode == SyncMode::TwoWayParanoid
            && !path.is_empty()
            && large_in_ancestor(ancestor)
            && alpha.is_none() != beta.is_none()
        {
            let alpha_gone = alpha.is_none();
            let (kept, kept_diff) = if alpha_gone {
                (beta_sync.clone(), &beta_diff)
            } else {
                (alpha_sync.clone(), &alpha_diff)
            };
            if kept_diff.is_empty() {
                let restore = Change {
                    path: path.to_owned(),
                    old: None,
                    new: kept,
                };
                if alpha_gone {
                    self.result.alpha_transitions.push(restore);
                } else {
                    self.result.beta_transitions.push(restore);
                }
                return;
            }
        }

        if beta_diff.is_empty() {
            let beta_unsynchronizable = blocking(
                path,
                ancestor,
                alpha_sync.as_ref(),
                diff_at(path, beta_sync.as_ref(), beta),
            );
            if !beta_unsynchronizable.is_empty() {
                self.result.conflicts.push(Conflict {
                    root: path.to_owned(),
                    alpha_changes: alpha_diff,
                    beta_changes: beta_unsynchronizable,
                });
            } else {
                self.result.beta_transitions.push(Change {
                    path: path.to_owned(),
                    old: ancestor.cloned(),
                    new: alpha_sync,
                });
            }
            return;
        } else if alpha_diff.is_empty() {
            let alpha_unsynchronizable = blocking(
                path,
                ancestor,
                beta_sync.as_ref(),
                diff_at(path, alpha_sync.as_ref(), alpha),
            );
            if !alpha_unsynchronizable.is_empty() {
                self.result.conflicts.push(Conflict {
                    root: path.to_owned(),
                    alpha_changes: alpha_unsynchronizable,
                    beta_changes: beta_diff,
                });
            } else {
                self.result.alpha_transitions.push(Change {
                    path: path.to_owned(),
                    old: ancestor.cloned(),
                    new: beta_sync,
                });
            }
            return;
        }

        // Both sides are modified. Pure-deletion changes can be safely
        // overwritten, so classify each side.
        let alpha_non_deletion = non_deletion_changes(&alpha_diff);
        let beta_non_deletion = non_deletion_changes(&beta_diff);

        // Both sides purely deletions: propagate the full deletion to the
        // side with the partial deletion.
        if alpha_non_deletion.is_empty() && beta_non_deletion.is_empty() {
            if alpha_sync.is_none() {
                let beta_unsynchronizable = blocking(
                    path,
                    ancestor,
                    None,
                    diff_at(path, beta_sync.as_ref(), beta),
                );
                if !beta_unsynchronizable.is_empty() {
                    self.result.conflicts.push(Conflict {
                        root: path.to_owned(),
                        alpha_changes: alpha_diff,
                        beta_changes: beta_unsynchronizable,
                    });
                } else {
                    self.result.beta_transitions.push(Change {
                        path: path.to_owned(),
                        old: beta_sync,
                        new: None,
                    });
                }
            } else {
                let alpha_unsynchronizable = blocking(
                    path,
                    ancestor,
                    None,
                    diff_at(path, alpha_sync.as_ref(), alpha),
                );
                if !alpha_unsynchronizable.is_empty() {
                    self.result.conflicts.push(Conflict {
                        root: path.to_owned(),
                        alpha_changes: alpha_unsynchronizable,
                        beta_changes: beta_diff,
                    });
                } else {
                    self.result.alpha_transitions.push(Change {
                        path: path.to_owned(),
                        old: alpha_sync,
                        new: None,
                    });
                }
            }
            return;
        }

        // Exactly one side purely deletions: propagate the other side's
        // content over it (this is also what enables manual conflict
        // resolution by deleting the losing side).
        if beta_non_deletion.is_empty() {
            let beta_unsynchronizable = blocking(
                path,
                ancestor,
                alpha_sync.as_ref(),
                diff_at(path, beta_sync.as_ref(), beta),
            );
            if !beta_unsynchronizable.is_empty() {
                self.result.conflicts.push(Conflict {
                    root: path.to_owned(),
                    alpha_changes: alpha_non_deletion,
                    beta_changes: beta_unsynchronizable,
                });
            } else {
                self.result.beta_transitions.push(Change {
                    path: path.to_owned(),
                    old: beta_sync,
                    new: alpha_sync,
                });
            }
            return;
        } else if alpha_non_deletion.is_empty() {
            // Alpha only deleted here, and beta edited or added. The edit
            // wins: a deletion carries nothing to weigh against it, and
            // letting it win would destroy the only copy. In the strict
            // mode alpha's deletion is final, and beta is made to match.
            if self.mode == SyncMode::TwoWayStrict {
                let beta_unsynchronizable = blocking(
                    path,
                    ancestor,
                    alpha_sync.as_ref(),
                    diff_at(path, beta_sync.as_ref(), beta),
                );
                if !beta_unsynchronizable.is_empty() {
                    self.result.conflicts.push(Conflict {
                        root: path.to_owned(),
                        alpha_changes: alpha_diff,
                        beta_changes: beta_unsynchronizable,
                    });
                } else {
                    self.result.beta_transitions.push(Change {
                        path: path.to_owned(),
                        old: beta_sync,
                        new: alpha_sync,
                    });
                }
                return;
            }
            let alpha_unsynchronizable = blocking(
                path,
                ancestor,
                beta_sync.as_ref(),
                diff_at(path, alpha_sync.as_ref(), alpha),
            );
            if !alpha_unsynchronizable.is_empty() {
                self.result.conflicts.push(Conflict {
                    root: path.to_owned(),
                    alpha_changes: alpha_unsynchronizable,
                    beta_changes: beta_non_deletion,
                });
            } else {
                self.result.alpha_transitions.push(Change {
                    path: path.to_owned(),
                    old: alpha_sync,
                    new: beta_sync,
                });
            }
            return;
        }

        // Both sides have non-deletion changes: conflict, or forced
        // resolution in alpha's favor in resolved mode.
        if matches!(self.mode, SyncMode::TwoWaySafe | SyncMode::TwoWayParanoid) {
            self.result.conflicts.push(Conflict {
                root: path.to_owned(),
                alpha_changes: alpha_non_deletion,
                beta_changes: beta_non_deletion,
            });
        } else {
            let beta_unsynchronizable = blocking(
                path,
                ancestor,
                alpha_sync.as_ref(),
                diff_at(path, beta_sync.as_ref(), beta),
            );
            if !beta_unsynchronizable.is_empty() {
                self.result.conflicts.push(Conflict {
                    root: path.to_owned(),
                    alpha_changes: alpha_non_deletion,
                    beta_changes: beta_unsynchronizable,
                });
            } else {
                self.result.beta_transitions.push(Change {
                    path: path.to_owned(),
                    old: beta_sync,
                    new: alpha_sync,
                });
            }
        }
    }

    fn handle_disagreement_one_way_safe(
        &mut self,
        path: &str,
        ancestor: Option<&Node>,
        alpha: Option<&Node>,
        beta: Option<&Node>,
    ) {
        // If beta's synchronizable portion is unmodified or purely deleted,
        // overwrite it with alpha's content (unless beta carries
        // unsynchronizable content, which indicates a conflict, reported
        // with a synthetic alpha change).
        let beta_sync = synchronized(beta);
        let beta_non_deletion = non_deletion_changes(&diff_at(path, ancestor, beta_sync.as_ref()));
        if beta_non_deletion.is_empty() {
            let beta_unsynchronizable = blocking(
                path,
                ancestor,
                alpha,
                diff_at(path, beta_sync.as_ref(), beta),
            );
            if !beta_unsynchronizable.is_empty() {
                self.result.conflicts.push(Conflict {
                    root: path.to_owned(),
                    alpha_changes: vec![Change {
                        path: path.to_owned(),
                        old: ancestor.cloned(),
                        new: alpha.cloned(),
                    }],
                    beta_changes: beta_unsynchronizable,
                });
            } else {
                self.result.beta_transitions.push(Change {
                    path: path.to_owned(),
                    old: beta_sync,
                    new: synchronized(alpha),
                });
            }
            return;
        }

        // Beta has non-deletion changes. If alpha is nil or untracked, and
        // it's not the case that both the ancestor and beta are directories,
        // then nil out the ancestor and leave beta's content in place (the
        // core of one-way-safe semantics: beta-side creations and
        // modifications survive).
        let ancestor_is_directory =
            matches!(ancestor, Some(node) if matches!(node.content, Content::Directory(_)));
        let beta_is_directory =
            matches!(beta, Some(node) if matches!(node.content, Content::Directory(_)));
        let untrack_beta_content =
            nil_or_untracked(alpha) && !(ancestor_is_directory && beta_is_directory);
        if untrack_beta_content {
            if ancestor.is_some() {
                self.result.ancestor_changes.push(Change {
                    path: path.to_owned(),
                    old: None,
                    new: None,
                });
            }
            return;
        }

        // Otherwise indicate a conflict (with a synthetic alpha change).
        self.result.conflicts.push(Conflict {
            root: path.to_owned(),
            alpha_changes: vec![Change {
                path: path.to_owned(),
                old: ancestor.cloned(),
                new: alpha.cloned(),
            }],
            beta_changes: beta_non_deletion,
        });
    }

    fn handle_disagreement_one_way_replica(
        &mut self,
        path: &str,
        ancestor: Option<&Node>,
        alpha: Option<&Node>,
        beta: Option<&Node>,
    ) {
        // Alpha carrying untracked content cannot be mirrored — and must
        // not read as "nothing", which would delete beta's copy of content
        // that synchronization merely excludes (an oversized file, say).
        // It surfaces as a conflict, matching the treatment of beta-side
        // content that mirroring can't remove.
        let alpha_untracked =
            matches!(alpha, Some(node) if matches!(node.content, Content::Untracked));
        if alpha_untracked {
            self.result.conflicts.push(Conflict {
                root: path.to_owned(),
                alpha_changes: vec![Change {
                    path: path.to_owned(),
                    old: ancestor.cloned(),
                    new: alpha.cloned(),
                }],
                beta_changes: vec![Change {
                    path: path.to_owned(),
                    old: ancestor.cloned(),
                    new: beta.cloned(),
                }],
            });
            return;
        }

        // Exact mirroring: overwrite beta with alpha's synchronizable
        // content, unless beta carries unsynchronizable content (which can't
        // be removed), in which case indicate a conflict.
        let beta_sync = synchronized(beta);
        let beta_unsynchronizable = blocking(
            path,
            ancestor,
            alpha,
            diff_at(path, beta_sync.as_ref(), beta),
        );
        if !beta_unsynchronizable.is_empty() {
            self.result.conflicts.push(Conflict {
                root: path.to_owned(),
                alpha_changes: vec![Change {
                    path: path.to_owned(),
                    old: ancestor.cloned(),
                    new: alpha.cloned(),
                }],
                beta_changes: beta_unsynchronizable,
            });
        } else {
            self.result.beta_transitions.push(Change {
                path: path.to_owned(),
                old: beta_sync,
                new: synchronized(alpha),
            });
        }
    }
}

/// Performs three-way reconciliation between the ancestor, alpha, and beta
/// hierarchies under the specified synchronization mode. The ancestor must
/// contain only synchronizable content.
pub fn reconcile(
    ancestor: Option<&Node>,
    alpha: Option<&Node>,
    beta: Option<&Node>,
    mode: SyncMode,
) -> Reconciliation {
    reconcile_since(ancestor, alpha, beta, mode, None)
}

/// [`reconcile`], skipping every subtree whose three inputs are the very
/// ones `memo` reconciled to nothing — the same result, in the same order,
/// for the size of what changed. `memo` must come from the same session's
/// previous reconciliation, in the same mode.
pub fn reconcile_since(
    ancestor: Option<&Node>,
    alpha: Option<&Node>,
    beta: Option<&Node>,
    mode: SyncMode,
    memo: Option<&ReconcileMemo>,
) -> Reconciliation {
    let mut reconciler = Reconciler {
        mode,
        result: Reconciliation::default(),
        memo,
    };
    let previous = memo.map(|memo| {
        (
            memo.ancestor.as_ref(),
            memo.alpha.as_ref(),
            memo.beta.as_ref(),
        )
    });
    reconciler.reconcile("", ancestor, alpha, beta, previous);
    reconciler.result
}

#[cfg(test)]
mod tests {
    use super::super::tests::file;
    use super::*;
    use crate::tree::{apply, Node};

    fn dir(name: &str, children: Vec<Node>) -> Node {
        Node::directory(name, children)
    }

    /// Provenance must survive mutual exclusion. Reproduced before the
    /// fix: a file ignored on both sides had its ancestor entry cleared,
    /// so a deletion made during the exclusion read as "beta holds a new
    /// creation" when the ignore was lifted, and the deliberately deleted
    /// content came back.
    #[test]
    fn mutual_exclusion_preserves_the_ancestor() {
        let ancestor = Node::directory("", vec![file("secret", 1, false)]);
        let both_untracked = Node::directory(
            "",
            vec![Node {
                name: "secret".into(),
                content: Content::Untracked,
            }],
        );
        let result = reconcile(
            Some(&ancestor),
            Some(&both_untracked),
            Some(&both_untracked),
            SyncMode::TwoWaySafe,
        );
        assert!(
            result.ancestor_changes.is_empty(),
            "exclusion must not clear provenance: {:?}",
            result.ancestor_changes
        );
        assert!(result.alpha_transitions.is_empty() && result.beta_transitions.is_empty());

        // Genuinely gone on both sides is different: the entry goes.
        let empty = Node::directory("", vec![]);
        let result = reconcile(
            Some(&ancestor),
            Some(&empty),
            Some(&empty),
            SyncMode::TwoWaySafe,
        );
        assert_eq!(result.ancestor_changes.len(), 1);
        assert!(result.ancestor_changes[0].new.is_none());

        // And the payoff: after the exclusion lifts, the preserved
        // ancestor lets the deletion made while excluded propagate.
        let alpha_deleted = Node::directory("", vec![]);
        let beta_kept = Node::directory("", vec![file("secret", 1, false)]);
        let result = reconcile(
            Some(&ancestor),
            Some(&alpha_deleted),
            Some(&beta_kept),
            SyncMode::TwoWaySafe,
        );
        assert_eq!(result.beta_transitions.len(), 1, "{result:?}");
        assert!(
            result.beta_transitions[0].new.is_none(),
            "the deletion must propagate to beta, not resurrect"
        );
    }

    /// A large directory at `data/` holding `n` files, from which the tests
    /// below build the shapes the paranoid mode cares about: emptied on one
    /// side, gone on one side.
    fn large(n: u8) -> Node {
        dir(
            "data",
            (1..=n).map(|i| file(&format!("f{i}"), i, false)).collect(),
        )
    }

    /// The paranoid mode reports a large directory emptied on one side as a
    /// conflict at the directory; every other mode carries the emptying
    /// across as the deletions it literally is.
    #[test]
    fn paranoid_treats_an_emptied_large_directory_as_a_conflict() {
        let ancestor = dir("", vec![file("readme", 9, false), large(9)]);
        let emptied = dir("", vec![file("readme", 9, false), dir("data", vec![])]);

        let result = reconcile(
            Some(&ancestor),
            Some(&emptied),
            Some(&ancestor),
            SyncMode::TwoWayParanoid,
        );
        assert_eq!(result.conflicts.len(), 1, "{result:?}");
        let conflict = &result.conflicts[0];
        assert_eq!(conflict.root, "data");
        // Both sides are described at the directory, so the report can
        // show a directory on each side rather than an absence.
        let node = |changes: &[Change]| changes[0].new.clone().expect("present");
        assert!(node(&conflict.alpha_changes).children().is_empty());
        assert_eq!(node(&conflict.beta_changes).children().len(), 9);
        assert!(
            result.alpha_transitions.is_empty() && result.beta_transitions.is_empty(),
            "nothing beneath moves while the conflict stands: {result:?}"
        );

        // The other side being the empty one names it the same way.
        let result = reconcile(
            Some(&ancestor),
            Some(&ancestor),
            Some(&emptied),
            SyncMode::TwoWayParanoid,
        );
        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(result.conflicts[0].root, "data");

        // Everywhere else it is nine deletions, and they propagate. This
        // was a session halt in every mode once, and it stopped whole
        // sessions for `git gc` packing a directory of loose refs.
        for mode in [
            SyncMode::TwoWaySafe,
            SyncMode::TwoWayResolved,
            SyncMode::OneWaySafe,
            SyncMode::OneWayReplica,
        ] {
            let result = reconcile(Some(&ancestor), Some(&emptied), Some(&ancestor), mode);
            assert!(result.conflicts.is_empty(), "{mode:?}: {result:?}");
            assert_eq!(result.beta_transitions.len(), 9, "{mode:?}: {result:?}");
            assert!(result.beta_transitions.iter().all(|c| c.new.is_none()));
        }
    }

    /// A directory emptied down to one ignored entry is emptied: what is
    /// left does not synchronize, so it is the same shape as a truly empty
    /// directory and the paranoid mode reports it the same way.
    #[test]
    fn paranoid_treats_a_directory_emptied_down_to_an_ignored_entry_as_emptied() {
        let ancestor = dir("", vec![file("readme", 9, false), large(9)]);
        let emptied = dir(
            "",
            vec![
                file("readme", 9, false),
                dir(
                    "data",
                    vec![Node {
                        name: ".DS_Store".into(),
                        content: Content::Untracked,
                    }],
                ),
            ],
        );
        for (alpha, beta) in [(&emptied, &ancestor), (&ancestor, &emptied)] {
            let result = reconcile(
                Some(&ancestor),
                Some(alpha),
                Some(beta),
                SyncMode::TwoWayParanoid,
            );
            assert_eq!(result.conflicts.len(), 1, "{result:?}");
            assert_eq!(result.conflicts[0].root, "data");
            assert!(
                result.alpha_transitions.is_empty() && result.beta_transitions.is_empty(),
                "nothing beneath moves while the conflict stands: {result:?}"
            );
        }
    }

    /// Below the threshold, emptying is housekeeping in every mode.
    #[test]
    fn paranoid_lets_a_small_directory_be_emptied() {
        let ancestor = dir("", vec![file("readme", 9, false), large(3)]);
        let emptied = dir("", vec![file("readme", 9, false), dir("data", vec![])]);
        let result = reconcile(
            Some(&ancestor),
            Some(&emptied),
            Some(&ancestor),
            SyncMode::TwoWayParanoid,
        );
        assert!(result.conflicts.is_empty(), "{result:?}");
        assert_eq!(result.beta_transitions.len(), 3);
    }

    /// A large directory gone on one side while the other holds exactly
    /// what the ancestor recorded is restored in the paranoid mode, and
    /// deleted in every other. This is also the second half of resolving
    /// the emptied-directory conflict in the full side's favour: `resolve`
    /// retires the empty directory, and the next cycle sees this shape.
    #[test]
    fn paranoid_restores_a_large_directory_gone_from_one_side() {
        let ancestor = dir("", vec![file("readme", 9, false), large(9)]);
        let gone = dir("", vec![file("readme", 9, false)]);

        let result = reconcile(
            Some(&ancestor),
            Some(&gone),
            Some(&ancestor),
            SyncMode::TwoWayParanoid,
        );
        assert!(result.conflicts.is_empty(), "{result:?}");
        assert!(result.beta_transitions.is_empty(), "{result:?}");
        assert_eq!(result.alpha_transitions.len(), 1, "{result:?}");
        let restore = &result.alpha_transitions[0];
        assert_eq!(restore.path, "data");
        assert!(restore.old.is_none());
        assert_eq!(restore.new.as_ref().expect("restored").children().len(), 9);

        // Symmetric.
        let result = reconcile(
            Some(&ancestor),
            Some(&ancestor),
            Some(&gone),
            SyncMode::TwoWayParanoid,
        );
        assert_eq!(result.beta_transitions.len(), 1);
        assert!(result.beta_transitions[0].new.is_some());

        // Elsewhere the deletion is honoured.
        let result = reconcile(
            Some(&ancestor),
            Some(&gone),
            Some(&ancestor),
            SyncMode::TwoWaySafe,
        );
        assert_eq!(result.beta_transitions.len(), 1);
        assert!(result.beta_transitions[0].new.is_none());

        // A small directory is deleted in the paranoid mode too.
        let ancestor = dir("", vec![file("readme", 9, false), large(3)]);
        let result = reconcile(
            Some(&ancestor),
            Some(&gone),
            Some(&ancestor),
            SyncMode::TwoWayParanoid,
        );
        assert!(result.alpha_transitions.is_empty());
        assert_eq!(result.beta_transitions.len(), 1);
        assert!(result.beta_transitions[0].new.is_none());
    }

    /// The other resolution: the emptying side wins. `resolve` retires the
    /// full copy, leaving two pure deletions — the whole directory on one
    /// side, its entries on the other — and the fuller one carries, so
    /// the directory ends up gone everywhere rather than restored.
    #[test]
    fn paranoid_lets_the_emptying_side_win_once_the_full_copy_is_retired() {
        let ancestor = dir("", vec![file("readme", 9, false), large(9)]);
        let gone = dir("", vec![file("readme", 9, false)]);
        let emptied = dir("", vec![file("readme", 9, false), dir("data", vec![])]);
        let result = reconcile(
            Some(&ancestor),
            Some(&gone),
            Some(&emptied),
            SyncMode::TwoWayParanoid,
        );
        assert!(result.conflicts.is_empty(), "{result:?}");
        assert!(result.alpha_transitions.is_empty(), "{result:?}");
        assert_eq!(result.beta_transitions.len(), 1, "{result:?}");
        assert_eq!(result.beta_transitions[0].path, "data");
        assert!(result.beta_transitions[0].new.is_none());
    }

    /// The restore rule needs the kept side untouched. A deletion against
    /// an edited directory is the ordinary edit-beats-delete case, in the
    /// paranoid mode as in the others.
    #[test]
    fn paranoid_restore_rule_yields_to_an_edited_other_side() {
        let ancestor = dir("", vec![large(9)]);
        let gone = dir("", vec![]);
        let mut edited_children: Vec<Node> =
            (1..=9).map(|i| file(&format!("f{i}"), i, false)).collect();
        edited_children[0] = file("f1", 42, false);
        let edited = dir("", vec![dir("data", edited_children)]);
        let result = reconcile(
            Some(&ancestor),
            Some(&gone),
            Some(&edited),
            SyncMode::TwoWayParanoid,
        );
        assert!(result.conflicts.is_empty(), "{result:?}");
        assert_eq!(result.alpha_transitions.len(), 1, "{result:?}");
        assert_eq!(result.alpha_transitions[0].path, "data");
        assert_eq!(
            result.alpha_transitions[0]
                .new
                .as_ref()
                .expect("edit wins")
                .children()
                .len(),
            9
        );
    }

    #[test]
    fn content_leaving_tracked_scope_never_reads_as_deletion() {
        // A synchronized file crosses a size limit (or otherwise stops
        // being synchronizable) on one side: it scans as untracked there
        // while the peer and the ancestor still carry the file. Nothing may
        // propagate — in any mode — and the ancestor must survive, so the
        // file resumes as an ordinary update if it re-enters tracked scope.
        let ancestor = dir("", vec![file("big.bin", 1, false)]);
        let with_untracked = dir(
            "",
            vec![Node {
                name: "big.bin".into(),
                content: Content::Untracked,
            }],
        );
        for mode in [
            SyncMode::TwoWaySafe,
            SyncMode::TwoWayParanoid,
            SyncMode::TwoWayResolved,
            SyncMode::OneWaySafe,
            SyncMode::OneWayReplica,
        ] {
            let result = reconcile(
                Some(&ancestor),
                Some(&with_untracked),
                Some(&ancestor),
                mode,
            );
            assert!(result.alpha_transitions.is_empty(), "{mode:?}");
            assert!(result.beta_transitions.is_empty(), "{mode:?}");
            assert!(result.ancestor_changes.is_empty(), "{mode:?}");
            // The reverse orientation (beta untracked) must hold too.
            let result = reconcile(
                Some(&ancestor),
                Some(&ancestor),
                Some(&with_untracked),
                mode,
            );
            assert!(result.alpha_transitions.is_empty(), "{mode:?}");
            assert!(result.beta_transitions.is_empty(), "{mode:?}");
            assert!(result.ancestor_changes.is_empty(), "{mode:?}");

            // The ancestor-less case (the ancestor was cleared while both
            // sides were untracked, then one re-entered tracked scope) must
            // not delete either: at most it surfaces a conflict.
            let result = reconcile(None, Some(&with_untracked), Some(&ancestor), mode);
            assert!(result.alpha_transitions.is_empty(), "{mode:?}");
            assert!(result.beta_transitions.is_empty(), "{mode:?}");
        }
    }

    #[test]
    fn identical_states_produce_no_operations() {
        let state = dir("", vec![file("a", 1, false)]);
        let result = reconcile(
            Some(&state),
            Some(&state),
            Some(&state),
            SyncMode::TwoWaySafe,
        );
        assert!(result.ancestor_changes.is_empty());
        assert!(result.alpha_transitions.is_empty());
        assert!(result.beta_transitions.is_empty());
        assert!(result.conflicts.is_empty());
    }

    #[test]
    fn alpha_modification_propagates_to_beta() {
        let ancestor = dir("", vec![file("a", 1, false)]);
        let alpha = dir("", vec![file("a", 2, false)]);
        let result = reconcile(
            Some(&ancestor),
            Some(&alpha),
            Some(&ancestor),
            SyncMode::TwoWaySafe,
        );
        assert_eq!(result.beta_transitions.len(), 1);
        assert!(result.alpha_transitions.is_empty());
        assert!(result.conflicts.is_empty());
        assert_eq!(result.beta_transitions[0].path, "a");
    }

    #[test]
    fn beta_modification_propagates_to_alpha_bidirectionally_only() {
        let ancestor = dir("", vec![file("a", 1, false)]);
        let beta = dir("", vec![file("a", 2, false)]);
        let bidirectional = reconcile(
            Some(&ancestor),
            Some(&ancestor),
            Some(&beta),
            SyncMode::TwoWaySafe,
        );
        assert_eq!(bidirectional.alpha_transitions.len(), 1);
        let replica = reconcile(
            Some(&ancestor),
            Some(&ancestor),
            Some(&beta),
            SyncMode::OneWayReplica,
        );
        assert!(replica.alpha_transitions.is_empty());
        assert_eq!(replica.beta_transitions.len(), 1);
        // Replica overwrites the beta modification with alpha content.
        assert!(replica.beta_transitions[0]
            .new
            .as_ref()
            .unwrap()
            .content_equal(ancestor.child("a").unwrap(), true));
    }

    #[test]
    fn one_way_safe_preserves_beta_creations() {
        let ancestor = dir("", vec![file("a", 1, false)]);
        let beta = dir("", vec![file("a", 1, false), file("b", 2, false)]);
        let result = reconcile(
            Some(&ancestor),
            Some(&ancestor),
            Some(&beta),
            SyncMode::OneWaySafe,
        );
        assert!(result.beta_transitions.is_empty());
        assert!(result.conflicts.is_empty());
        // The beta creation is untracked by niling the (absent) ancestor:
        // no ancestor change needed since the ancestor lacks "b".
        assert!(result.ancestor_changes.is_empty());
    }

    #[test]
    fn concurrent_divergent_edits_conflict_in_safe_mode_and_resolve_in_resolved_mode() {
        let ancestor = dir("", vec![file("a", 1, false)]);
        let alpha = dir("", vec![file("a", 2, false)]);
        let beta = dir("", vec![file("a", 3, false)]);
        let safe = reconcile(
            Some(&ancestor),
            Some(&alpha),
            Some(&beta),
            SyncMode::TwoWaySafe,
        );
        assert_eq!(safe.conflicts.len(), 1);
        assert!(safe.alpha_transitions.is_empty() && safe.beta_transitions.is_empty());
        let resolved = reconcile(
            Some(&ancestor),
            Some(&alpha),
            Some(&beta),
            SyncMode::TwoWayResolved,
        );
        assert!(resolved.conflicts.is_empty());
        assert_eq!(resolved.beta_transitions.len(), 1);
        assert!(resolved.beta_transitions[0]
            .new
            .as_ref()
            .unwrap()
            .content_equal(alpha.child("a").unwrap(), true));
    }

    #[test]
    fn strict_lets_an_alpha_deletion_beat_a_beta_edit() {
        let ancestor = dir("", vec![file("a", 1, false), file("b", 1, false)]);
        let alpha = dir("", vec![file("b", 1, false)]);
        let beta = dir("", vec![file("a", 2, false), file("b", 1, false)]);
        // Every other two-way mode brings the edit back to alpha.
        for mode in [SyncMode::TwoWaySafe, SyncMode::TwoWayResolved] {
            let result = reconcile(Some(&ancestor), Some(&alpha), Some(&beta), mode);
            assert!(result.conflicts.is_empty(), "{mode:?}");
            assert_eq!(result.alpha_transitions.len(), 1, "{mode:?}");
            assert!(result.beta_transitions.is_empty(), "{mode:?}");
        }
        // Strict removes it from beta.
        let result = reconcile(
            Some(&ancestor),
            Some(&alpha),
            Some(&beta),
            SyncMode::TwoWayStrict,
        );
        assert!(result.conflicts.is_empty());
        assert!(result.alpha_transitions.is_empty());
        assert_eq!(result.beta_transitions.len(), 1);
        assert_eq!(result.beta_transitions[0].path, "a");
        assert!(result.beta_transitions[0].new.is_none());
        // And still carries a beta addition to alpha, as any two-way mode.
        let beta = dir("", vec![file("b", 1, false), file("c", 5, false)]);
        let result = reconcile(
            Some(&ancestor),
            Some(&alpha),
            Some(&beta),
            SyncMode::TwoWayStrict,
        );
        assert_eq!(result.alpha_transitions.len(), 1);
        assert_eq!(result.alpha_transitions[0].path, "c");
    }

    /// The row of `docs/modes.md`'s table for "alpha deletes a file beta
    /// edited", one assertion per cell. The one-way modes never write to
    /// alpha, so in `one-way-conflict` beta's edit stays on beta and
    /// synchronization forgets the file, as if beta had created it; it is
    /// not reported, since there is nothing of alpha's to overwrite it.
    #[test]
    fn alpha_deleting_a_file_beta_edited_matches_the_modes_table() {
        let ancestor = dir("", vec![file("a", 1, false), file("b", 1, false)]);
        let alpha = dir("", vec![file("b", 1, false)]);
        let beta = dir("", vec![file("a", 2, false), file("b", 1, false)]);
        let run = |mode| reconcile(Some(&ancestor), Some(&alpha), Some(&beta), mode);
        let restores_to_alpha = |result: &Reconciliation| {
            result.conflicts.is_empty()
                && result.beta_transitions.is_empty()
                && result.alpha_transitions.len() == 1
                && result.alpha_transitions[0].path == "a"
                && result.alpha_transitions[0]
                    .new
                    .as_ref()
                    .is_some_and(|n| n.content_equal(beta.child("a").unwrap(), true))
        };
        let deletes_on_beta = |result: &Reconciliation| {
            result.conflicts.is_empty()
                && result.alpha_transitions.is_empty()
                && result.beta_transitions.len() == 1
                && result.beta_transitions[0].path == "a"
                && result.beta_transitions[0].new.is_none()
        };

        // two-way-conflict: beta's edit comes back to alpha.
        let result = run(SyncMode::TwoWaySafe);
        assert!(restores_to_alpha(&result), "{result:?}");
        // two-way-alpha: beta's edit comes back to alpha.
        let result = run(SyncMode::TwoWayResolved);
        assert!(restores_to_alpha(&result), "{result:?}");
        // two-way-alpha-strict: deleted on beta too.
        let result = run(SyncMode::TwoWayStrict);
        assert!(deletes_on_beta(&result), "{result:?}");
        // one-way-conflict: beta's edit stays on beta, unreported, and the
        // ancestor forgets the file.
        let result = run(SyncMode::OneWaySafe);
        assert!(result.conflicts.is_empty(), "{result:?}");
        assert!(result.alpha_transitions.is_empty(), "{result:?}");
        assert!(result.beta_transitions.is_empty(), "{result:?}");
        assert_eq!(result.ancestor_changes.len(), 1, "{result:?}");
        assert_eq!(result.ancestor_changes[0].path, "a");
        assert!(result.ancestor_changes[0].new.is_none());
        // one-way-alpha: deleted on beta too.
        let result = run(SyncMode::OneWayReplica);
        assert!(deletes_on_beta(&result), "{result:?}");
    }

    #[test]
    fn deletion_versus_modification_repropagates_content() {
        // Alpha deleted a file; beta modified it: the modification wins on
        // both sides (deletion is the losing side of the conflict).
        let ancestor = dir("", vec![file("a", 1, false)]);
        let alpha = dir("", vec![]);
        let beta = dir("", vec![file("a", 2, false)]);
        let result = reconcile(
            Some(&ancestor),
            Some(&alpha),
            Some(&beta),
            SyncMode::TwoWaySafe,
        );
        assert_eq!(result.alpha_transitions.len(), 1);
        assert!(result.alpha_transitions[0].new.is_some());
        assert!(result.conflicts.is_empty());
    }

    #[test]
    fn both_modified_same_updates_ancestor_only() {
        let ancestor = dir("", vec![file("a", 1, false)]);
        let both = dir("", vec![file("a", 2, false)]);
        let result = reconcile(
            Some(&ancestor),
            Some(&both),
            Some(&both),
            SyncMode::TwoWaySafe,
        );
        assert!(result.alpha_transitions.is_empty());
        assert!(result.beta_transitions.is_empty());
        assert!(result.conflicts.is_empty());
        assert!(!result.ancestor_changes.is_empty());
        // Applying the ancestor changes must produce the agreed state.
        let updated = apply(Some(&ancestor), &result.ancestor_changes)
            .unwrap()
            .unwrap();
        assert!(updated.content_equal(&both, true));
    }

    /// An entry the ancestor recorded that one side now excludes — a file
    /// grown past `max_file_size`, turned into a FIFO, a symlink under the
    /// `ignore` mode — is a change reconciliation cannot see, and it
    /// blocks the deletion of the directory around it. Reproduced before
    /// the fix in `two-way-conflict`: alpha grew `d/a` past the limit, beta
    /// deleted `d`, and the "both purely deletions" branch deleted alpha's
    /// edited file.
    #[test]
    fn an_entry_excluded_since_the_ancestor_blocks_the_deletion_around_it() {
        let ancestor = dir(
            "",
            vec![dir("d", vec![file("a", 1, false), file("b", 1, false)])],
        );
        let excluded = dir(
            "",
            vec![dir(
                "d",
                vec![
                    Node {
                        name: "a".into(),
                        content: Content::Untracked,
                    },
                    file("b", 1, false),
                ],
            )],
        );
        let deleted = dir("", vec![]);
        let removes_d =
            |changes: &[Change]| changes.iter().any(|c| c.path == "d" && c.new.is_none());

        // Beta excluded `a`, alpha deleted `d`: a conflict in every mode.
        for mode in MODES {
            let result = reconcile(Some(&ancestor), Some(&deleted), Some(&excluded), mode);
            assert_eq!(result.conflicts.len(), 1, "{mode:?}: {result:?}");
            assert_eq!(result.conflicts[0].root, "d", "{mode:?}");
            assert!(!removes_d(&result.beta_transitions), "{mode:?}: {result:?}");
        }
        // Alpha excluded `a`, beta deleted `d`: a conflict in the two-way
        // modes, and alpha is never touched.
        for mode in MODES {
            let result = reconcile(Some(&ancestor), Some(&excluded), Some(&deleted), mode);
            assert!(
                !removes_d(&result.alpha_transitions),
                "{mode:?}: {result:?}"
            );
            if !matches!(mode, SyncMode::OneWaySafe | SyncMode::OneWayReplica) {
                assert_eq!(result.conflicts.len(), 1, "{mode:?}: {result:?}");
            }
        }

        // An ignored entry the ancestor never held does not block: a `.git`
        // goes with the project directory around it, in every mode.
        let with_git = dir(
            "",
            vec![dir(
                "d",
                vec![
                    Node {
                        name: ".git".into(),
                        content: Content::Untracked,
                    },
                    file("a", 1, false),
                    file("b", 1, false),
                ],
            )],
        );
        for mode in MODES {
            let result = reconcile(Some(&ancestor), Some(&deleted), Some(&with_git), mode);
            assert!(result.conflicts.is_empty(), "{mode:?}: {result:?}");
            assert!(removes_d(&result.beta_transitions), "{mode:?}: {result:?}");
        }
    }

    #[test]
    fn untracked_beta_content_blocks_replica_with_conflict() {
        let ancestor = dir("", vec![]);
        let alpha = dir("", vec![file("a", 1, false)]);
        let beta = dir(
            "",
            vec![Node {
                name: "a".into(),
                content: Content::Untracked,
            }],
        );
        let result = reconcile(
            Some(&ancestor),
            Some(&alpha),
            Some(&beta),
            SyncMode::OneWayReplica,
        );
        assert_eq!(result.conflicts.len(), 1);
        assert!(result.beta_transitions.is_empty());
    }

    // ── property-based reconciliation ────────────────────────────────
    //
    // Reconcile is a hand-port of the subtlest logic in the system, and a
    // defect in it is silent data loss with no crash required. These
    // properties hold over generated triples rather than remembered cases.

    /// Reads the instruction bytes a generated tree is built from, one at a
    /// time; past the end every instruction is zero.
    struct Instructions<'a> {
        bytes: &'a [u8],
        next: usize,
    }

    impl Instructions<'_> {
        fn take(&mut self) -> u8 {
            let byte = self.bytes.get(self.next).copied().unwrap_or(0);
            self.next += 1;
            byte
        }
    }

    /// The child names at every level, and how deep a generated tree goes.
    const NAMES: [&str; 3] = ["a", "b", "c"];
    const DEPTH: usize = 3;

    const MODES: [SyncMode; 6] = [
        SyncMode::TwoWaySafe,
        SyncMode::TwoWayParanoid,
        SyncMode::TwoWayResolved,
        SyncMode::TwoWayStrict,
        SyncMode::OneWaySafe,
        SyncMode::OneWayReplica,
    ];

    fn untracked_node(name: &str) -> Node {
        Node {
            name: name.into(),
            content: Content::Untracked,
        }
    }

    /// A fresh node: absent, a file (three possible contents), a symlink,
    /// a directory of fresh children, an empty directory, or — when
    /// permitted — untracked.
    fn fresh(name: &str, depth: usize, input: &mut Instructions, untracked: bool) -> Option<Node> {
        let byte = input.take();
        let content = match byte % 8 {
            0 => return None,
            3 => Content::Symlink {
                target: "elsewhere".into(),
            },
            4 | 5 if depth < DEPTH => Content::Directory(std::sync::Arc::new(
                NAMES
                    .iter()
                    .filter_map(|name| fresh(name, depth + 1, input, untracked))
                    .collect(),
            )),
            6 if untracked => Content::Untracked,
            7 => Content::Directory(Default::default()),
            _ => Content::File {
                digest: [byte / 8 % 3 + 1; crate::tree::DIGEST_SIZE],
                executable: false,
                metadata: crate::tree::FileMetadata::default(),
            },
        };
        Some(Node {
            name: name.into(),
            content,
        })
    }

    /// A side's copy of `base` after local activity: mostly kept, with
    /// entries deleted, created, replaced, emptied, or — when permitted —
    /// turned untracked, at every depth, the root included. An emptied
    /// directory may keep one ignored entry, which is how a bare mount
    /// point or a wiped checkout looks.
    fn mutated(
        base: Option<&Node>,
        name: &str,
        depth: usize,
        input: &mut Instructions,
        untracked: bool,
    ) -> Option<Node> {
        let byte = input.take();
        match (byte % 16, base) {
            (0, _) => None,
            (1, _) => fresh(name, depth, input, untracked),
            (2, Some(_)) if untracked => Some(untracked_node(name)),
            (3, Some(node)) if matches!(node.content, Content::Directory(_)) => {
                let left = if untracked && byte / 16 % 2 == 1 {
                    vec![untracked_node(".DS_Store")]
                } else {
                    Vec::new()
                };
                Some(Node::directory(name, left))
            }
            (_, Some(node)) if matches!(node.content, Content::Directory(_)) => {
                Some(Node::directory(
                    name,
                    NAMES
                        .iter()
                        .filter_map(|child| {
                            mutated(node.child(child), child, depth + 1, input, untracked)
                        })
                        .collect(),
                ))
            }
            (_, other) => other.cloned(),
        }
    }

    /// An ancestor, and alpha and beta each derived from it by their own
    /// local activity. The ancestor holds only synchronizable content, as
    /// a real one does; the sides hold untracked entries when permitted.
    fn generated(
        ancestor: &[u8],
        alpha: &[u8],
        beta: &[u8],
        untracked: bool,
    ) -> (Option<Node>, Option<Node>, Option<Node>) {
        let mut input = Instructions {
            bytes: ancestor,
            next: 0,
        };
        let ancestor = if input.take().is_multiple_of(16) {
            None
        } else {
            Some(Node::directory(
                "",
                NAMES
                    .iter()
                    .filter_map(|name| fresh(name, 1, &mut input, false))
                    .collect(),
            ))
        };
        let side = |bytes: &[u8]| {
            let mut input = Instructions { bytes, next: 0 };
            mutated(ancestor.as_ref(), "", 0, &mut input, untracked)
        };
        let (alpha, beta) = (side(alpha), side(beta));
        (ancestor, alpha, beta)
    }

    fn trees_equal(left: Option<&Node>, right: Option<&Node>) -> bool {
        match (left, right) {
            (None, None) => true,
            (Some(left), Some(right)) => left.content_equal(right, true),
            _ => false,
        }
    }

    fn no_unsynchronizable(node: &Node) -> bool {
        match &node.content {
            Content::Untracked | Content::Problematic { .. } => false,
            Content::Directory(children) => children.iter().all(no_unsynchronizable),
            _ => true,
        }
    }

    /// Every entry of a hierarchy with its root-relative path.
    fn entries<'a>(path: String, node: &'a Node, out: &mut Vec<(String, &'a Node)>) {
        for child in node.children() {
            entries(path_join(&path, &child.name), child, out);
        }
        out.push((path, node));
    }

    /// Whether a conflict reported at `root` stands over `path`.
    fn covers(root: &str, path: &str) -> bool {
        root.is_empty()
            || path == root
            || path
                .strip_prefix(root)
                .is_some_and(|rest| rest.starts_with('/'))
    }

    /// Whether `root` holds untracked content at `path` or above it: such
    /// content neither offers changes nor receives them.
    fn shielded(root: Option<&Node>, path: &str) -> bool {
        let untracked = |prefix: &str| {
            matches!(
                crate::tree::node_at(root, prefix).map(|n| &n.content),
                Some(Content::Untracked)
            )
        };
        let mut prefix = String::new();
        let mut shielded = untracked(&prefix);
        for part in path.split('/').filter(|p| !p.is_empty()) {
            prefix = path_join(&prefix, part);
            shielded |= untracked(&prefix);
        }
        shielded
    }

    /// The ways reconciliation can lose what one side did since the
    /// ancestor, named by path; empty when nothing is lost.
    ///
    /// Whatever a side holds that the ancestor does not vouch for — an
    /// edit, a creation, an entry turned untracked — survives on that side
    /// or stands under a conflict. The exceptions are the modes' own
    /// policy: alpha overwrites beta's synchronizable content in the
    /// alpha-wins modes, and an ignored entry the ancestor never held goes
    /// with a directory deleted around it (`docs/ignores.md`; which of
    /// those the endpoint may really remove is its own decision).
    ///
    /// And every synchronizable change a side made reaches the other side
    /// or stands under a conflict, unless the other side holds untracked
    /// content there (which neither offers nor receives changes) or the
    /// change is beta's in a one-way mode, where beta's changes stay put.
    fn silent_losses(
        mode: SyncMode,
        ancestor: Option<&Node>,
        (alpha, beta): (Option<&Node>, Option<&Node>),
        (alpha_after, beta_after): (Option<&Node>, Option<&Node>),
        conflicts: &[Conflict],
    ) -> Vec<String> {
        let one_way = matches!(mode, SyncMode::OneWaySafe | SyncMode::OneWayReplica);
        let alpha_wins = matches!(
            mode,
            SyncMode::TwoWayResolved | SyncMode::TwoWayStrict | SyncMode::OneWayReplica
        );
        let conflicted = |path: &str| conflicts.iter().any(|c| covers(&c.root, path));
        let mut losses = Vec::new();
        for (is_beta, before, after, other_before, other_after) in [
            (false, alpha, alpha_after, beta, beta_after),
            (true, beta, beta_after, alpha, alpha_after),
        ] {
            let side = if is_beta { "beta" } else { "alpha" };
            let mut held = Vec::new();
            if let Some(root) = before {
                entries(String::new(), root, &mut held);
            }
            for (path, node) in held {
                let recorded = crate::tree::node_at(ancestor, &path);
                if recorded.is_some_and(|a| a.content_equal(node, false)) {
                    continue;
                }
                let now = crate::tree::node_at(after, &path);
                if now.is_some_and(|n| n.content_equal(node, false)) || conflicted(&path) {
                    continue;
                }
                let untracked = matches!(node.content, Content::Untracked);
                if is_beta && alpha_wins && !untracked {
                    continue;
                }
                if untracked && recorded.is_none() && now.is_none() {
                    continue;
                }
                losses.push(format!("{side} lost '{path}'"));
            }

            if is_beta && one_way {
                continue;
            }
            let own = before.and_then(Node::synchronizable_subtree);
            for change in crate::tree::diff(ancestor, own.as_ref()) {
                let Some(new) = &change.new else { continue };
                let mut changed = Vec::new();
                entries(change.path.clone(), new, &mut changed);
                for (path, _) in changed {
                    if conflicted(&path) || shielded(other_before, &path) {
                        continue;
                    }
                    fn synchronizable<'n>(root: Option<&'n Node>, path: &str) -> Option<&'n Node> {
                        crate::tree::node_at(root, path).filter(|n| n.content.synchronizable())
                    }
                    if !shallow_equal(
                        synchronizable(after, &path),
                        synchronizable(other_after, &path),
                    ) {
                        losses.push(format!("{side}'s change at '{path}' went nowhere"));
                    }
                }
            }
        }
        losses
    }

    /// A hierarchy in one line, for failure messages: `d/{a=1 l@ u? e/{}}`
    /// is a directory holding a file, a symlink, an untracked entry and an
    /// empty directory.
    fn show(node: Option<&Node>) -> String {
        let Some(node) = node else {
            return "-".into();
        };
        let name = &node.name;
        match &node.content {
            Content::Directory(children) => {
                let inner: Vec<String> = children.iter().map(|c| show(Some(c))).collect();
                format!("{name}/{{{}}}", inner.join(" "))
            }
            Content::File { digest, .. } => format!("{name}={}", digest[0]),
            Content::Symlink { .. } => format!("{name}@"),
            Content::Untracked => format!("{name}?"),
            Content::Problematic { .. } => format!("{name}!"),
        }
    }

    fn show_changes(changes: &[Change]) -> String {
        let shown: Vec<String> = changes
            .iter()
            .map(|c| {
                format!(
                    "'{}': {} -> {}",
                    c.path,
                    show(c.old.as_ref()),
                    show(c.new.as_ref())
                )
            })
            .collect();
        shown.join(", ")
    }

    /// Instruction bytes for one generated side.
    fn instructions() -> impl proptest::strategy::Strategy<Value = Vec<u8>> {
        proptest::collection::vec(proptest::num::u8::ANY, 48)
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 1024, ..Default::default()
        })]

        /// Absent conflicts, applying the emitted transitions to each side
        /// leaves the two sides content-equal — synchronization actually
        /// synchronizes — and applying the ancestor changes on top of the
        /// old ancestor never fails.
        #[test]
        fn conflict_free_reconciliation_converges(
            ancestor_spec in instructions(),
            alpha_spec in instructions(),
            beta_spec in instructions(),
        ) {
            let (ancestor, alpha, beta) =
                generated(&ancestor_spec, &alpha_spec, &beta_spec, false);
            let result = reconcile(
                ancestor.as_ref(),
                alpha.as_ref(),
                beta.as_ref(),
                SyncMode::TwoWaySafe,
            );
            if result.conflicts.is_empty() {
                let alpha_after = apply(alpha.as_ref(), &result.alpha_transitions)
                    .expect("alpha transitions apply");
                let beta_after = apply(beta.as_ref(), &result.beta_transitions)
                    .expect("beta transitions apply");
                proptest::prop_assert!(
                    trees_equal(alpha_after.as_ref(), beta_after.as_ref()),
                    "conflict-free reconciliation did not converge:\n\
                     alpha {alpha_after:?}\nbeta {beta_after:?}"
                );
            }
            apply(ancestor.as_ref(), &result.ancestor_changes)
                .expect("ancestor changes apply");
        }

        /// Three-way agreement is inert: when ancestor, alpha, and beta all
        /// hold the same content, reconciliation has nothing to say.
        #[test]
        fn agreement_emits_nothing(spec in instructions(), mode_index in 0usize..6) {
            let (tree, _, _) = generated(&spec, &[], &[], false);
            let result = reconcile(tree.as_ref(), tree.as_ref(), tree.as_ref(), MODES[mode_index]);
            proptest::prop_assert!(result.alpha_transitions.is_empty());
            proptest::prop_assert!(result.beta_transitions.is_empty());
            proptest::prop_assert!(result.ancestor_changes.is_empty());
            proptest::prop_assert!(result.conflicts.is_empty());
        }

        /// Unsynchronizable content never travels: no emitted transition
        /// carries untracked or problematic nodes in its `new` side, in any
        /// mode, whatever the inputs hold.
        #[test]
        fn unsynchronizable_content_never_travels(
            ancestor_spec in instructions(),
            alpha_spec in instructions(),
            beta_spec in instructions(),
            mode_index in 0usize..6,
        ) {
            let mode = MODES[mode_index];
            let (ancestor, alpha, beta) =
                generated(&ancestor_spec, &alpha_spec, &beta_spec, true);
            let result = reconcile(ancestor.as_ref(), alpha.as_ref(), beta.as_ref(), mode);
            for change in result
                .alpha_transitions
                .iter()
                .chain(result.beta_transitions.iter())
                .chain(result.ancestor_changes.iter())
            {
                if let Some(new) = &change.new {
                    proptest::prop_assert!(
                        no_unsynchronizable(new),
                        "unsynchronizable content emitted at '{}'",
                        change.path
                    );
                }
            }
        }

        /// No silent loss: every change a side made since the ancestor
        /// either propagates or surfaces as a conflict, and nothing a side
        /// holds that the ancestor does not vouch for is destroyed without
        /// one, in every mode, with untracked entries at every depth. See
        /// `silent_losses` for the exact rule and the modes' exceptions.
        #[test]
        fn no_change_is_lost_silently(
            ancestor_spec in instructions(),
            alpha_spec in instructions(),
            beta_spec in instructions(),
            mode_index in 0usize..6,
        ) {
            let mode = MODES[mode_index];
            let (ancestor, alpha, beta) =
                generated(&ancestor_spec, &alpha_spec, &beta_spec, true);
            let result = reconcile(ancestor.as_ref(), alpha.as_ref(), beta.as_ref(), mode);
            let alpha_after = apply(alpha.as_ref(), &result.alpha_transitions)
                .expect("alpha transitions apply");
            let beta_after = apply(beta.as_ref(), &result.beta_transitions)
                .expect("beta transitions apply");
            let losses = silent_losses(
                mode,
                ancestor.as_ref(),
                (alpha.as_ref(), beta.as_ref()),
                (alpha_after.as_ref(), beta_after.as_ref()),
                &result.conflicts,
            );
            proptest::prop_assert!(
                losses.is_empty(),
                "{mode:?}: {losses:?}\nancestor {}\nalpha {}\nbeta {}\n\
                 alpha transitions {}\nbeta transitions {}\nconflicts at {:?}",
                show(ancestor.as_ref()),
                show(alpha.as_ref()),
                show(beta.as_ref()),
                show_changes(&result.alpha_transitions),
                show_changes(&result.beta_transitions),
                result.conflicts.iter().map(|c| c.root.as_str()).collect::<Vec<_>>(),
            );
        }

        /// A transition expects exactly what its side's synchronizable
        /// content holds at its path, which is what the endpoint validates
        /// before it acts. An expectation that names an untracked entry is
        /// refused there every time, and the same transition comes back
        /// every cycle (M-33).
        #[test]
        fn a_transition_expects_what_its_side_synchronizes(
            ancestor_spec in instructions(),
            alpha_spec in instructions(),
            beta_spec in instructions(),
            mode_index in 0usize..6,
        ) {
            let mode = MODES[mode_index];
            let (ancestor, alpha, beta) =
                generated(&ancestor_spec, &alpha_spec, &beta_spec, true);
            let result = reconcile(ancestor.as_ref(), alpha.as_ref(), beta.as_ref(), mode);
            for (side, root, transitions) in [
                ("alpha", alpha.as_ref(), &result.alpha_transitions),
                ("beta", beta.as_ref(), &result.beta_transitions),
            ] {
                for change in transitions {
                    let held = crate::tree::node_at(root, &change.path)
                        .and_then(Node::synchronizable_subtree);
                    proptest::prop_assert!(
                        trees_equal(change.old.as_ref(), held.as_ref()),
                        "{mode:?}: {side} transition at '{}' expects {} but the side holds {}",
                        change.path,
                        show(change.old.as_ref()),
                        show(held.as_ref())
                    );
                }
            }
        }

        /// The one-way modes never write to alpha, whatever they see.
        #[test]
        fn one_way_modes_never_touch_alpha(
            ancestor_spec in instructions(),
            alpha_spec in instructions(),
            beta_spec in instructions(),
            replica in proptest::bool::ANY,
        ) {
            let mode = if replica {
                SyncMode::OneWayReplica
            } else {
                SyncMode::OneWaySafe
            };
            let (ancestor, alpha, beta) =
                generated(&ancestor_spec, &alpha_spec, &beta_spec, true);
            let result = reconcile(ancestor.as_ref(), alpha.as_ref(), beta.as_ref(), mode);
            proptest::prop_assert!(
                result.alpha_transitions.is_empty(),
                "a one-way mode emitted alpha transitions: {:?}",
                result.alpha_transitions
            );
        }
    }

    /// `reconcile_since` is `reconcile`, faster: across hundreds of rounds of
    /// copy-on-write changes to all three trees, in every mode, the result
    /// against the previous round's memo is the fresh result exactly —
    /// content and order.
    #[test]
    fn reconcile_since_is_reconcile() {
        let mut seed = 0x1234_5678_9abc_def1u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let leaf = |name: &str, kind: u64, byte: u8| -> Node {
            match kind % 10 {
                0 => Node {
                    name: name.into(),
                    content: Content::Untracked,
                },
                1 => Node {
                    name: name.into(),
                    content: Content::Problematic {
                        message: "unreadable".into(),
                    },
                },
                2 => Node {
                    name: name.into(),
                    content: Content::Symlink {
                        target: format!("t{byte}"),
                    },
                },
                3 => dir(name, vec![]),
                _ => file(name, byte, kind.is_multiple_of(7)),
            }
        };
        let modes = [
            SyncMode::TwoWaySafe,
            SyncMode::TwoWayResolved,
            SyncMode::TwoWayParanoid,
            SyncMode::TwoWayStrict,
            SyncMode::OneWaySafe,
            SyncMode::OneWayReplica,
        ];
        for (trial, mode) in modes.iter().cycle().take(18).enumerate() {
            // A shared starting tree: most of it agrees on all three sides,
            // as a synchronized pair does, with storage shared among them.
            let base = dir(
                "",
                (0..6)
                    .map(|d| {
                        dir(
                            &format!("d{d}"),
                            (0..10)
                                .map(|e| {
                                    dir(
                                        &format!("e{e}"),
                                        (0..9)
                                            .map(|f| file(&format!("f{f}"), (f + e) as u8, false))
                                            .collect(),
                                    )
                                })
                                .collect(),
                        )
                    })
                    .collect(),
            );
            let (mut ancestor, mut alpha, mut beta) =
                (Some(base.clone()), Some(base.clone()), Some(base));
            let mut memo: Option<ReconcileMemo> = None;
            for round in 0..120 {
                // A few copy-on-write changes to one side or another, or to
                // the ancestor (as a cycle's record of what it did would).
                for _ in 0..(1 + next() % 4) {
                    let depth = 1 + next() % 3;
                    let mut path = format!("d{}", next() % 7);
                    if depth > 1 {
                        path.push_str(&format!("/e{}", next() % 11));
                    }
                    if depth > 2 {
                        path.push_str(&format!("/f{}", next() % 10));
                    }
                    let name = path.rsplit('/').next().unwrap().to_owned();
                    let new = match next() % 5 {
                        0 => None,
                        _ => Some(leaf(&name, next(), (next() % 250) as u8)),
                    };
                    let change = Change {
                        path,
                        old: None,
                        new,
                    };
                    let target = match next() % 3 {
                        0 => &mut ancestor,
                        1 => &mut alpha,
                        _ => &mut beta,
                    };
                    if let Ok(changed) = apply(target.as_ref(), std::slice::from_ref(&change)) {
                        *target = changed;
                    }
                }
                let fresh = reconcile(ancestor.as_ref(), alpha.as_ref(), beta.as_ref(), *mode);
                let skipping = reconcile_since(
                    ancestor.as_ref(),
                    alpha.as_ref(),
                    beta.as_ref(),
                    *mode,
                    memo.as_ref(),
                );
                assert_eq!(
                    format!("{skipping:?}"),
                    format!("{fresh:?}"),
                    "trial {trial} ({mode:?}), round {round}"
                );
                memo = Some(ReconcileMemo::of(
                    ancestor.as_ref(),
                    alpha.as_ref(),
                    beta.as_ref(),
                    &fresh,
                ));
                // Sometimes the cycle lands: its record reaches the
                // ancestor, as a session's would, copy-on-write.
                if next() % 2 == 0 {
                    if let Ok(recorded) = apply(ancestor.as_ref(), &fresh.ancestor_changes) {
                        ancestor = recorded;
                    }
                }
            }
        }
    }
}
