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
struct Reconciler {
    mode: SyncMode,
    result: Reconciliation,
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

impl Reconciler {
    fn reconcile(
        &mut self,
        path: &str,
        ancestor: Option<&Node>,
        alpha: Option<&Node>,
        beta: Option<&Node>,
    ) {
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
                self.reconcile(&child_path, ancestor_child, alpha_child, beta_child);
            }
            return;
        }

        // Alpha and beta disagree at this path; dispatch by mode.
        match self.mode {
            SyncMode::TwoWaySafe | SyncMode::TwoWayResolved => {
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
        let alpha_sync = alpha.and_then(Node::synchronizable_subtree);
        let beta_sync = beta.and_then(Node::synchronizable_subtree);

        // Classic three-way merge: if one side is unmodified, propagate the
        // other side's synchronizable content (unless the unmodified side
        // carries unsynchronizable content, which indicates a conflict).
        let alpha_diff = diff_at(path, ancestor, alpha_sync.as_ref());
        let beta_diff = diff_at(path, ancestor, beta_sync.as_ref());
        if beta_diff.is_empty() {
            let beta_unsynchronizable = diff_at(path, beta_sync.as_ref(), beta);
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
            let alpha_unsynchronizable = diff_at(path, alpha_sync.as_ref(), alpha);
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
                let beta_unsynchronizable = diff_at(path, beta_sync.as_ref(), beta);
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
                let alpha_unsynchronizable = diff_at(path, alpha_sync.as_ref(), alpha);
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
            let beta_unsynchronizable = diff_at(path, beta_sync.as_ref(), beta);
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
            let alpha_unsynchronizable = diff_at(path, alpha_sync.as_ref(), alpha);
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
        if self.mode == SyncMode::TwoWaySafe {
            self.result.conflicts.push(Conflict {
                root: path.to_owned(),
                alpha_changes: alpha_non_deletion,
                beta_changes: beta_non_deletion,
            });
        } else {
            let beta_unsynchronizable = diff_at(path, beta_sync.as_ref(), beta);
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
        let beta_sync = beta.and_then(Node::synchronizable_subtree);
        let beta_non_deletion = non_deletion_changes(&diff_at(path, ancestor, beta_sync.as_ref()));
        if beta_non_deletion.is_empty() {
            let beta_unsynchronizable = diff_at(path, beta_sync.as_ref(), beta);
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
                    old: beta.cloned(),
                    new: alpha.and_then(Node::synchronizable_subtree),
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
        let beta_sync = beta.and_then(Node::synchronizable_subtree);
        let beta_unsynchronizable = diff_at(path, beta_sync.as_ref(), beta);
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
                old: beta.cloned(),
                new: alpha.and_then(Node::synchronizable_subtree),
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
    let mut reconciler = Reconciler {
        mode,
        result: Reconciliation::default(),
    };
    reconciler.reconcile("", ancestor, alpha, beta);
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
}
