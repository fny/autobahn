//! Does pointer pruning actually have anything to prune in production?
//!
//! `cycle_cost.rs` reported that 2,499 of 2,501 directories are
//! pointer-identical after a one-file edit, but it passed one local scan as
//! both the ancestor and beta. That measures how well an incremental rescan
//! preserves storage against the *previous scan* — the best case — and says
//! nothing about a session whose ancestor came off disk and whose beta came
//! off the wire, both of which allocate fresh.
//!
//! This measures the three relationships a real reconcile compares.

use std::path::PathBuf;

use autobahn::endpoint::local::{EndpointOptions, LocalEndpoint};
use autobahn::endpoint::Endpoint;
use autobahn::tree::{apply, nodes_share_storage, reconcile, Content, Node, SyncMode};

fn main() {
    let root = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: share_reality <root>"),
    );
    let staging = std::env::temp_dir().join(format!("share-reality-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let mut endpoint =
        LocalEndpoint::new(root.clone(), staging.clone(), EndpointOptions::default())
            .expect("endpoint should be creatable");

    let settled = endpoint.scan().expect("scan should succeed");

    // What a restart produces: the ancestor is decoded from its file, so
    // every allocation is new even though the content is identical.
    let encoded = bincode::serialize(&settled.root).expect("ancestor encodes");
    let reloaded: Option<Node> = bincode::deserialize(&encoded).expect("ancestor decodes");

    let victim = first_file(settled.root.as_ref(), String::new()).expect("a file");
    let path = root.join(&victim);
    let mut content = std::fs::read(&path).expect("readable");
    content.extend_from_slice(b"\nedited\n");
    std::fs::write(&path, &content).expect("writable");
    let edited = endpoint.scan().expect("rescan should succeed");

    println!("after one edit, directories sharing storage:\n");
    report(
        "previous scan  vs edited scan   (what cycle_cost measured)",
        settled.root.as_ref(),
        edited.root.as_ref(),
    );
    report(
        "reloaded ancestor vs edited scan (what a restart gives you)",
        reloaded.as_ref(),
        edited.root.as_ref(),
    );
    report(
        "reloaded ancestor vs previous scan (identical content)",
        reloaded.as_ref(),
        settled.root.as_ref(),
    );

    // The case that matters for incremental validation: the ancestor is not
    // rescanned, it is built by apply() from the previous ancestor. If that
    // preserves untouched subtrees, validation can skip them even when
    // nothing else shares storage.
    let result = reconcile(
        reloaded.as_ref(),
        edited.root.as_ref(),
        reloaded.as_ref(),
        SyncMode::TwoWaySafe,
    );
    // The ancestor advances from *achieved* transition results, so a
    // successful beta transition is what actually reaches it.
    let next = apply(reloaded.as_ref(), &result.beta_transitions).expect("ancestor applies");
    println!();
    report(
        "previous ancestor vs apply()-derived next ancestor",
        reloaded.as_ref(),
        next.as_ref(),
    );
    println!("  ({} change(s) applied)", result.beta_transitions.len());

    std::fs::write(&path, &content[..content.len() - 8]).expect("restorable");
    let _ = std::fs::remove_dir_all(&staging);
}

fn report(label: &str, a: Option<&Node>, b: Option<&Node>) {
    let (shared, total) = share(a, b);
    println!(
        "  {label}: {shared}/{total}  ({:.1}%)",
        100.0 * shared as f64 / total as f64
    );
}

fn first_file(node: Option<&Node>, path: String) -> Option<String> {
    let node = node?;
    match &node.content {
        Content::File { .. } => Some(path),
        Content::Directory(children) => children.iter().find_map(|child| {
            let child_path = if path.is_empty() {
                child.name.clone()
            } else {
                format!("{path}/{}", child.name)
            };
            first_file(Some(child), child_path)
        }),
        _ => None,
    }
}

fn share(a: Option<&Node>, b: Option<&Node>) -> (usize, usize) {
    if nodes_share_storage(a, b) {
        return (1, 1);
    }
    let (mut shared, mut total) = (0, 1);
    if let (
        Some(Node {
            content: Content::Directory(left),
            ..
        }),
        Some(Node {
            content: Content::Directory(right),
            ..
        }),
    ) = (a, b)
    {
        for (x, y) in left.iter().zip(right.iter()) {
            if matches!(x.content, Content::Directory(_)) {
                let (s, t) = share(Some(x), Some(y));
                shared += s;
                total += t;
            }
        }
    }
    (shared, total)
}
