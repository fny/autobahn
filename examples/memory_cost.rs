//! What does a scanned tree cost in memory, and how close is that to the
//! minimum the data itself requires?
//!
//! Resident size alone cannot answer that: it includes the allocator's slack,
//! the page cache accounting, and whatever the binary itself needs. This
//! measures resident size around a scan, then computes what the snapshot's
//! own contents must occupy — one `Node` per entry, plus its name, plus the
//! children vector each directory owns. The ratio between the two is the
//! part worth attacking.
//!
//! Usage: cargo run --release --example memory_cost -- <root> [root...]

use std::mem::size_of;
use std::path::PathBuf;

use autobahn::endpoint::local::{EndpointOptions, LocalEndpoint};
use autobahn::endpoint::Endpoint;
use autobahn::tree::{Content, Node, Snapshot};

fn main() {
    let roots: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    if roots.is_empty() {
        eprintln!("usage: memory_cost <root> [root...]");
        std::process::exit(1);
    }

    println!(
        "sizeof: Node {}B, Content {}B, Snapshot {}B",
        size_of::<Node>(),
        size_of::<Content>(),
        size_of::<Snapshot>()
    );
    println!();
    println!(
        "{:>9} {:>10} {:>11} {:>11} {:>9} {:>9}",
        "entries", "rss delta", "theoretical", "overhead", "B/entry", "min B/e"
    );
    println!(
        "{:>9} {:>10} {:>11} {:>11} {:>9} {:>9}",
        "---------", "----------", "-----------", "-----------", "---------", "---------"
    );

    for root in &roots {
        let staging = std::env::temp_dir().join(format!(
            "memory-cost-{}-{}",
            std::process::id(),
            root.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));
        let mut endpoint =
            LocalEndpoint::new(root.clone(), staging.clone(), EndpointOptions::default())
                .expect("endpoint should be creatable");

        // A first scan warms the page cache and the allocator, so the
        // measured delta reflects the snapshot rather than start-up noise.
        let _ = endpoint.scan().expect("warm-up scan should succeed");
        drop(endpoint);

        let before = resident_bytes();
        let mut endpoint =
            LocalEndpoint::new(root.clone(), staging.clone(), EndpointOptions::default())
                .expect("endpoint should be creatable");
        let snapshot = endpoint.scan().expect("scan should succeed");
        let after = resident_bytes();

        let stats = measure(&snapshot);
        let delta = after.saturating_sub(before);
        println!(
            "{:>9} {:>9.1}M {:>10.1}M {:>10.1}M {:>9.0} {:>9.0}",
            stats.entries,
            delta as f64 / 1_048_576.0,
            stats.theoretical as f64 / 1_048_576.0,
            (delta as f64 - stats.theoretical as f64) / 1_048_576.0,
            delta as f64 / stats.entries as f64,
            stats.theoretical as f64 / stats.entries as f64,
        );
        drop(snapshot);
        drop(endpoint);
        let _ = std::fs::remove_dir_all(&staging);
    }

    println!();
    println!("theoretical counts one Node per entry, its name's bytes, and the");
    println!("Vec<Node> each directory owns. It excludes allocator slack, which is");
    println!("what the overhead column is measuring.");
}

struct Stats {
    entries: usize,
    theoretical: usize,
}

fn measure(snapshot: &Snapshot) -> Stats {
    fn walk(node: &Node, stats: &mut Stats) {
        stats.entries += 1;
        // The node itself, wherever it lives, plus its name's heap bytes.
        stats.theoretical += size_of::<Node>() + node.name.len();
        match &node.content {
            Content::Directory(children) => {
                // The children vector's own allocation is counted through
                // its elements above; add the Arc's control block once.
                stats.theoretical += 2 * size_of::<usize>();
                for child in children.iter() {
                    walk(child, stats);
                }
            }
            Content::Symlink { target } => stats.theoretical += target.len(),
            Content::Problematic { message } => stats.theoretical += message.len(),
            _ => {}
        }
    }
    let mut stats = Stats {
        entries: 0,
        theoretical: 0,
    };
    if let Some(root) = snapshot.root.as_ref() {
        walk(root, &mut stats);
    }
    stats
}

/// Resident set size in bytes, from the kernel rather than the allocator —
/// the allocator's own view would hide slack, which is the thing in question.
fn resident_bytes() -> usize {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kilobytes: usize = rest
                .trim()
                .trim_end_matches(" kB")
                .parse()
                .unwrap_or_default();
            return kilobytes * 1024;
        }
    }
    0
}
