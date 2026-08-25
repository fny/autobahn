//! The session controller.
//!
//! A session synchronizes two endpoints through repeated cycles of scan →
//! reconcile → stage → transition → ancestor update, with the ancestor (the
//! last fully synchronized state) persisted between runs so that three-way
//! reconciliation can distinguish "changed on one side" from "changed on the
//! other". The controller is the hub: endpoints never communicate directly,
//! so either side may be local or remote.

use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::endpoint::{Endpoint, FileRequest, TransitionOutcome};
use crate::tree::{
    apply, path_join, reconcile, Change, Conflict, Content, Node, Problem, SyncMode,
};

/// The number of transfer frames pumped between endpoints per round trip
/// during staging. Larger batches amortize protocol round trips; each frame
/// is bounded by the rsync maximum data operation size.
const SUPPLY_BATCH_SIZE: usize = 256;

/// A synchronization halt requiring explicit user intervention, raised when
/// a cycle would perform a change so sweeping that it more likely reflects
/// an accident (or a vanished filesystem) than intent.
#[derive(Debug, thiserror::Error)]
pub enum SafetyHalt {
    /// A synchronization root was deleted in its entirety.
    #[error("halted: the synchronization root was deleted on one side; delete the other side manually (or recreate the root) and run again")]
    RootDeletion,
    /// A previously non-trivial synchronization root was emptied on exactly
    /// one side.
    #[error("halted: one side's synchronization root was emptied; propagate the deletion manually or restore the content, then run again")]
    RootEmptied,
}

/// A report of one synchronization cycle.
#[derive(Debug, Default)]
pub struct CycleReport {
    /// The number of transitions applied to alpha.
    pub alpha_transitions: usize,
    /// The number of transitions applied to beta.
    pub beta_transitions: usize,
    /// Conflicts identified during reconciliation (left unresolved in safe
    /// modes).
    pub conflicts: Vec<Conflict>,
    /// Scan problems from alpha.
    pub alpha_scan_problems: Vec<Problem>,
    /// Scan problems from beta.
    pub beta_scan_problems: Vec<Problem>,
    /// Transition problems from alpha.
    pub alpha_transition_problems: Vec<Problem>,
    /// Transition problems from beta.
    pub beta_transition_problems: Vec<Problem>,
    /// Whether or not either endpoint reported missing staged content
    /// (warranting an immediate follow-up cycle).
    pub missing_staged_files: bool,
}

impl CycleReport {
    /// Indicates whether or not the cycle applied any transitions.
    pub fn changed(&self) -> bool {
        self.alpha_transitions > 0 || self.beta_transitions > 0
    }
}

/// A synchronization session between two endpoints.
pub struct Session {
    /// The alpha endpoint.
    alpha: Box<dyn Endpoint + Send>,
    /// The beta endpoint.
    beta: Box<dyn Endpoint + Send>,
    /// The synchronization mode.
    mode: SyncMode,
    /// The persisted ancestor path.
    ancestor_path: PathBuf,
    /// The current ancestor hierarchy.
    ancestor: Option<Node>,
}

/// Computes a stable session identifier from the two endpoint
/// specifications, used to isolate persisted state.
pub fn session_identifier(alpha_spec: &str, beta_spec: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(alpha_spec.as_bytes());
    hasher.update(&[0]);
    hasher.update(beta_spec.as_bytes());
    let digest = hasher.finalize();
    let bytes = digest.as_bytes();
    let mut identifier = String::with_capacity(32);
    for byte in &bytes[..16] {
        identifier.push_str(&format!("{byte:02x}"));
    }
    identifier
}

impl Session {
    /// Creates a session between the provided endpoints, loading any
    /// persisted ancestor from the session state directory (which is created
    /// if needed).
    pub fn new(
        alpha: Box<dyn Endpoint + Send>,
        beta: Box<dyn Endpoint + Send>,
        mode: SyncMode,
        state_directory: PathBuf,
    ) -> Result<Session> {
        fs::create_dir_all(&state_directory).with_context(|| {
            format!(
                "unable to create session state directory {}",
                state_directory.display()
            )
        })?;
        let ancestor_path = state_directory.join("ancestor");
        let ancestor = load_ancestor(&ancestor_path)?;
        Ok(Session {
            alpha,
            beta,
            mode,
            ancestor_path,
            ancestor,
        })
    }

    /// Runs one synchronization cycle: scan both endpoints, reconcile,
    /// stage and apply transitions, and update the persisted ancestor.
    pub fn run_cycle(&mut self) -> Result<CycleReport> {
        let mut report = CycleReport::default();

        // Scan both endpoints in parallel.
        let (alpha_snapshot, beta_snapshot) = {
            let alpha = &mut self.alpha;
            let beta = &mut self.beta;
            std::thread::scope(|scope| {
                let alpha_scan = scope.spawn(move || alpha.scan());
                let beta_result = beta.scan();
                let alpha_result = alpha_scan.join().expect("scan thread panicked");
                (alpha_result, beta_result)
            })
        };
        let alpha_snapshot = alpha_snapshot.context("alpha scan failed")?;
        let beta_snapshot = beta_snapshot.context("beta scan failed")?;
        if let Some(root) = &alpha_snapshot.root {
            report.alpha_scan_problems = root.problems();
        }
        if let Some(root) = &beta_snapshot.root {
            report.beta_scan_problems = root.problems();
        }

        // Safety: if the ancestor root was a directory with non-trivial
        // content and exactly one side now presents an empty (or absent)
        // root, then halt rather than propagate what is more likely an
        // unmounted or wiped filesystem than an intentional mass deletion.
        if one_side_emptied_root(
            self.ancestor.as_ref(),
            alpha_snapshot.root.as_ref(),
            beta_snapshot.root.as_ref(),
        ) {
            bail!(SafetyHalt::RootEmptied);
        }

        // Reconcile.
        let reconciliation = reconcile(
            self.ancestor.as_ref(),
            alpha_snapshot.root.as_ref(),
            beta_snapshot.root.as_ref(),
            self.mode,
        );
        report.conflicts = reconciliation.conflicts;

        // Safety: refuse to propagate a root deletion.
        let contains_root_deletion = reconciliation
            .alpha_transitions
            .iter()
            .chain(reconciliation.beta_transitions.iter())
            .any(Change::is_root_deletion);
        if contains_root_deletion {
            bail!(SafetyHalt::RootDeletion);
        }

        // Stage and transition each side. Content flowing to beta is
        // supplied by alpha and vice versa.
        let beta_outcome = if reconciliation.beta_transitions.is_empty() {
            None
        } else {
            stage(
                self.alpha.as_mut(),
                self.beta.as_mut(),
                &reconciliation.beta_transitions,
            )?;
            Some(
                self.beta
                    .transition(reconciliation.beta_transitions.clone())
                    .context("beta transition failed")?,
            )
        };
        let alpha_outcome = if reconciliation.alpha_transitions.is_empty() {
            None
        } else {
            stage(
                self.beta.as_mut(),
                self.alpha.as_mut(),
                &reconciliation.alpha_transitions,
            )?;
            Some(
                self.alpha
                    .transition(reconciliation.alpha_transitions.clone())
                    .context("alpha transition failed")?,
            )
        };

        // Fold transition results into ancestor changes: each transition's
        // achieved content becomes the ancestor's new content at that path.
        let mut ancestor_changes = reconciliation.ancestor_changes;
        let mut fold = |transitions: &[Change], outcome: &TransitionOutcome| {
            for (transition, result) in transitions.iter().zip(outcome.results.iter()) {
                ancestor_changes.push(Change {
                    path: transition.path.clone(),
                    old: None,
                    new: result.clone(),
                });
            }
        };
        if let Some(outcome) = &beta_outcome {
            fold(&reconciliation.beta_transitions, outcome);
            report.beta_transitions = reconciliation.beta_transitions.len();
            report.beta_transition_problems = outcome.problems.clone();
            report.missing_staged_files |= outcome.missing_staged_files;
        }
        if let Some(outcome) = &alpha_outcome {
            fold(&reconciliation.alpha_transitions, outcome);
            report.alpha_transitions = reconciliation.alpha_transitions.len();
            report.alpha_transition_problems = outcome.problems.clone();
            report.missing_staged_files |= outcome.missing_staged_files;
        }

        // Apply the ancestor changes, validate the result (the ancestor must
        // only ever contain synchronizable content — this is the safety net
        // against reconciliation defects reaching disk), and persist it.
        if !ancestor_changes.is_empty() {
            let new_ancestor = apply(self.ancestor.as_ref(), &ancestor_changes)
                .map_err(|message| anyhow::anyhow!("ancestor update failed: {message}"))?;
            if let Some(root) = &new_ancestor {
                root.validate(true)
                    .map_err(|message| anyhow::anyhow!("new ancestor is invalid: {message}"))?;
            }
            save_ancestor(&self.ancestor_path, new_ancestor.as_ref())?;
            self.ancestor = new_ancestor;
        }

        Ok(report)
    }
}

/// Collects the file content dependencies of a transition list: the paths
/// and digests of every file in the transitions' target content, excluding
/// files whose transition only changes executability (which transitions
/// perform in place without staged content).
pub fn transition_dependencies(transitions: &[Change]) -> Vec<FileRequest> {
    fn collect(path: &str, node: &Node, requests: &mut Vec<FileRequest>) {
        match &node.content {
            Content::File { digest, .. } => requests.push(FileRequest {
                path: path.to_owned(),
                digest: *digest,
            }),
            Content::Directory(children) => {
                for child in children.iter() {
                    collect(&path_join(path, &child.name), child, requests);
                }
            }
            _ => {}
        }
    }
    let mut requests = Vec::new();
    for transition in transitions {
        if let (
            Some(Node {
                content:
                    Content::File {
                        digest: old_digest, ..
                    },
                ..
            }),
            Some(Node {
                content:
                    Content::File {
                        digest: new_digest, ..
                    },
                ..
            }),
        ) = (&transition.old, &transition.new)
        {
            if old_digest == new_digest {
                continue;
            }
        }
        if let Some(new) = &transition.new {
            collect(&transition.path, new, &mut requests);
        }
    }
    requests
}

/// Stages the content needed by `transitions` onto the destination endpoint,
/// supplying it from the source endpoint in streamed batches.
fn stage(
    source: &mut dyn Endpoint,
    destination: &mut dyn Endpoint,
    transitions: &[Change],
) -> Result<()> {
    let requests = transition_dependencies(transitions);
    if requests.is_empty() {
        return Ok(());
    }
    let needs = destination
        .stage_begin(requests)
        .context("unable to begin staging")?;
    if needs.is_empty() {
        return Ok(());
    }
    source
        .supply_open(needs)
        .context("unable to open supply stream")?;
    loop {
        let frames = source
            .supply_pull(SUPPLY_BATCH_SIZE)
            .context("unable to pull file content")?;
        if frames.is_empty() {
            break;
        }
        destination
            .stage_push(frames)
            .context("unable to push file content")?;
    }
    Ok(())
}

/// Detects the emptied-root condition: the ancestor was a directory with
/// two or more immediate children, and exactly one side now presents a
/// childless (or absent) directory root while the other retains content.
fn one_side_emptied_root(
    ancestor: Option<&Node>,
    alpha: Option<&Node>,
    beta: Option<&Node>,
) -> bool {
    let ancestor_children = match ancestor {
        Some(node) if matches!(node.content, Content::Directory(_)) => node.children().len(),
        _ => return false,
    };
    if ancestor_children < 2 {
        return false;
    }
    let side_empty = |side: Option<&Node>| match side {
        None => true,
        Some(node) => node.children().is_empty(),
    };
    let alpha_empty = side_empty(alpha);
    let beta_empty = side_empty(beta);
    alpha_empty != beta_empty
}

/// Loads a persisted ancestor, treating a missing file as an absent
/// ancestor and failing on corruption (an unreadable ancestor must not be
/// silently discarded, since that would resurrect deletions).
fn load_ancestor(path: &PathBuf) -> Result<Option<Node>> {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("unable to read ancestor"),
    };
    let ancestor: Option<Node> =
        bincode::deserialize(&data).context("unable to decode ancestor")?;
    if let Some(root) = &ancestor {
        root.validate(true)
            .map_err(|message| anyhow::anyhow!("persisted ancestor is invalid: {message}"))?;
    }
    Ok(ancestor)
}

/// Persists the ancestor atomically (write-temporary-then-rename).
fn save_ancestor(path: &PathBuf, ancestor: Option<&Node>) -> Result<()> {
    let data = bincode::serialize(&ancestor.cloned()).context("unable to encode ancestor")?;
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, data).context("unable to write ancestor")?;
    fs::rename(&temporary, path).context("unable to publish ancestor")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Digest, FileMetadata};

    fn file(name: &str, digest_byte: u8) -> Node {
        Node {
            name: name.into(),
            content: Content::File {
                digest: [digest_byte; 32] as Digest,
                executable: false,
                metadata: FileMetadata::default(),
            },
        }
    }

    #[test]
    fn transition_dependencies_collects_files_and_skips_executability_changes() {
        let mut executable_only = file("a", 1);
        if let Content::File { executable, .. } = &mut executable_only.content {
            *executable = true;
        }
        let transitions = vec![
            Change {
                path: "a".into(),
                old: Some(file("a", 1)),
                new: Some(executable_only),
            },
            Change {
                path: "d".into(),
                old: None,
                new: Some(Node::directory("d", vec![file("x", 2), file("y", 3)])),
            },
        ];
        let requests = transition_dependencies(&transitions);
        let paths: Vec<&str> = requests.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["d/x", "d/y"]);
    }

    #[test]
    fn emptied_root_detection() {
        let ancestor = Node::directory("", vec![file("a", 1), file("b", 2)]);
        let empty = Node::directory("", vec![]);
        assert!(one_side_emptied_root(
            Some(&ancestor),
            Some(&empty),
            Some(&ancestor)
        ));
        assert!(!one_side_emptied_root(
            Some(&ancestor),
            Some(&empty),
            Some(&empty)
        ));
        assert!(!one_side_emptied_root(
            Some(&ancestor),
            Some(&ancestor),
            Some(&ancestor)
        ));
        let trivial = Node::directory("", vec![file("a", 1)]);
        assert!(!one_side_emptied_root(
            Some(&trivial),
            Some(&empty),
            Some(&trivial)
        ));
    }

    #[test]
    fn ancestor_persistence_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ancestor");
        let ancestor = Node::directory("", vec![file("a", 1)]);
        save_ancestor(&path, Some(&ancestor)).unwrap();
        let loaded = load_ancestor(&path).unwrap().unwrap();
        assert!(loaded.content_equal(&ancestor, true));
        save_ancestor(&path, None).unwrap();
        assert!(load_ancestor(&path).unwrap().is_none());
    }

    #[test]
    fn corrupt_ancestor_is_an_error_not_a_reset() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ancestor");
        fs::write(&path, b"garbage").unwrap();
        assert!(load_ancestor(&path).is_err());
    }
}
