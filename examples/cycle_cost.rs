//! What in a cycle scales with the size of the tree?
//!
//! Latency for a single edit measured 45.7ms over 6,636 entries and 61.8ms
//! over 62,952, which implies a per-entry term. At Chromium's half a million
//! entries that term would dominate everything else, so it is worth knowing
//! what it is before assuming it is irreducible.
//!
//! This runs the source side of a cycle after a one-file edit and times the
//! phases separately. It also asks the question that decides whether the
//! per-entry cost is removable at all: after an incremental rescan, do the
//! untouched subtrees still share storage with the previous snapshot? If
//! they do, a three-way walk could skip them by pointer comparison instead
//! of descending into them.
//!
//! Usage: cargo run --release --example cycle_cost -- <root> [root...]

use std::path::PathBuf;
use std::time::Instant;

use autobahn::endpoint::local::{EndpointOptions, LocalEndpoint};
use autobahn::endpoint::Endpoint;
use autobahn::tree::{nodes_share_storage, reconcile, Content, Node, SyncMode};

fn main() {
    let roots: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    if roots.is_empty() {
        eprintln!("usage: cycle_cost <root> [root...]");
        std::process::exit(1);
    }

    println!(
        "{:>9} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>12}",
        "entries", "rescan", "reconc", "validate", "encode", "write", "TOTAL", "shared dirs"
    );
    println!("{}", "-".repeat(84));

    for root in &roots {
        let staging = std::env::temp_dir().join(format!("cycle-cost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        let mut endpoint =
            LocalEndpoint::new(root.clone(), staging.clone(), EndpointOptions::default())
                .expect("endpoint should be creatable");

        // The converged state: this snapshot stands in for the ancestor and
        // for both endpoints, which is what a quiesced session holds.
        let settled = endpoint.scan().expect("scan should succeed");
        let entries = count(settled.root.as_ref());

        // One file changes, exactly as a single save would do.
        let victim =
            first_file(settled.root.as_ref(), String::new()).expect("corpus should contain a file");
        let path = root.join(&victim);
        let mut content = std::fs::read(&path).expect("victim should be readable");
        content.extend_from_slice(b"\nedited\n");
        std::fs::write(&path, &content).expect("victim should be writable");

        let started = Instant::now();
        let edited = endpoint.scan().expect("rescan should succeed");
        let rescan = started.elapsed().as_secs_f64() * 1000.0;

        // Reconcile the edited alpha against the settled ancestor and a beta
        // that has not yet seen the edit — the exact three trees a cycle
        // reconciles when one file changes on one side.
        let started = Instant::now();
        let result = reconcile(
            settled.root.as_ref(),
            edited.root.as_ref(),
            settled.root.as_ref(),
            SyncMode::TwoWaySafe,
        );
        let reconcile_ms = started.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(
            result.beta_transitions.len(),
            1,
            "one edit should yield one transition"
        );

        // The decisive question: how much of the tree is provably untouched
        // by pointer alone? Copy-on-write should rewrite only the path from
        // the root down to the edited file, leaving every sibling subtree
        // pointer-identical to the one in the previous snapshot.
        let (shared, total) = share(settled.root.as_ref(), edited.root.as_ref());

        // The two remaining per-cycle costs that walk the whole hierarchy:
        // the ancestor is validated and then serialized and written to disk
        // synchronously, every cycle that changes anything.
        let started = Instant::now();
        edited
            .root
            .as_ref()
            .expect("root")
            .validate(true)
            .expect("valid");
        let validate_ms = started.elapsed().as_secs_f64() * 1000.0;

        let ancestor_path = staging.with_extension("ancestor");
        let started = Instant::now();
        let data = bincode::serialize(&edited.root.clone()).expect("ancestor encodes");
        let encode_ms = started.elapsed().as_secs_f64() * 1000.0;
        let started = Instant::now();
        let temporary = ancestor_path.with_extension("tmp");
        std::fs::write(&temporary, &data).expect("ancestor writes");
        std::fs::rename(&temporary, &ancestor_path).expect("ancestor publishes");
        let write_ms = started.elapsed().as_secs_f64() * 1000.0;

        println!(
            "{:>9} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>7}/{:<4}",
            entries,
            rescan,
            reconcile_ms,
            validate_ms,
            encode_ms,
            write_ms,
            rescan + reconcile_ms + validate_ms + encode_ms + write_ms,
            shared,
            total
        );

        println!(
            "{:>9} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8.1} {:>12}",
            "",
            "",
            "",
            "",
            "",
            "ancestor",
            data.len() as f64 / 1_048_576.0,
            "MB"
        );
        std::fs::write(&path, &content[..content.len() - 8]).expect("victim should be restorable");
        let _ = std::fs::remove_file(&ancestor_path);
        let _ = std::fs::remove_dir_all(&staging);
    }

    println!();
    println!("'shared dirs' counts directories whose children vector is the same");
    println!("allocation in both snapshots, over the directories compared. A high");
    println!("ratio means a three-way walk could prune by pointer instead of");
    println!("descending, turning a per-entry cost into a per-change one.");
}

fn count(node: Option<&Node>) -> usize {
    match node {
        None => 0,
        Some(node) => match &node.content {
            Content::Directory(children) => {
                1 + children.iter().map(|c| count(Some(c))).sum::<usize>()
            }
            _ => 1,
        },
    }
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

/// Counts directories that share their children allocation across the two
/// snapshots, stopping at each shared subtree rather than descending — which
/// is exactly what a pruning walk would do.
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
