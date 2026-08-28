//! What does the source actually spend per destination?
//!
//! The EC2 fan-out measurement showed the source paying about 3.3 seconds of
//! CPU per additional destination, but that figure covers a whole session:
//! scan, reconcile, supply, and transport. Before trying to share any of it,
//! this attributes the cost to a phase.
//!
//! It runs entirely locally and has no fan-out confound, because it measures
//! only source-side work — there are no destinations at all. Supply frames
//! are drained and dropped.
//!
//! Usage: cargo run --release --example supply_cost -- <root> [passes]

use std::path::PathBuf;
use std::time::Instant;

use autobahn::endpoint::local::{EndpointOptions, LocalEndpoint};
use autobahn::endpoint::{Endpoint, FileRequest, StagingNeed, TransferFrame};
use autobahn::protocol::Request;
use autobahn::rsync::Signature;
use autobahn::transport::Connection;
use autobahn::tree::{reconcile, Content, Node, Snapshot, SyncMode};

fn main() {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().expect("usage: supply_cost <root> [passes]"));
    let passes: usize = args
        .next()
        .map(|value| value.parse().expect("passes should be a number"))
        .unwrap_or(4);

    let staging = std::env::temp_dir().join(format!("supply-cost-{}", std::process::id()));
    let mut endpoint =
        LocalEndpoint::new(root.clone(), staging.clone(), EndpointOptions::default())
            .expect("endpoint should be creatable");

    // Warm the page cache first. Cold-disk cost is real but it is paid once
    // for all destinations, so including it would flatter the sharing case.
    let _ = endpoint.scan().expect("warm-up scan should succeed");

    let started = Instant::now();
    let snapshot = endpoint.scan().expect("scan should succeed");
    let scan_time = started.elapsed();

    let requests = file_requests(&snapshot);
    let bytes: u64 = total_bytes(&root, &requests);
    println!(
        "root {}: {} files, {:.1} MB",
        root.display(),
        requests.len(),
        bytes as f64 / 1_048_576.0
    );
    println!(
        "scan (shared across destinations): {:.2}s\n",
        scan_time.as_secs_f64()
    );

    // Reconciliation is pure over the three trees, so it measures exactly
    // here. It is per-session work that no observer shares: every
    // destination reconciles the same alpha tree against its own beta. In a
    // cold sync beta and the ancestor are both empty.
    let started = Instant::now();
    let reconciliation = reconcile(None, snapshot.root.as_ref(), None, SyncMode::TwoWaySafe);
    let reconcile_time = started.elapsed().as_secs_f64();
    println!(
        "reconcile (per destination, not shared): {reconcile_time:.2}s          -> {} transitions\n",
        reconciliation.beta_transitions.len()
    );

    // Every destination in a cold sync needs every file with no base to
    // delta against, which is exactly an empty signature.
    let needs: Vec<StagingNeed> = requests
        .iter()
        .map(|request| StagingNeed {
            request: request.clone(),
            signature: Signature::default(),
        })
        .collect();

    // Each destination gets its own batch of frames, which the transport
    // then bincode-encodes and LZ4-compresses before writing to its own ssh
    // stream. Every one of those stages is paid per destination today, and
    // every one of them operates on identical bytes when the destinations
    // need the same content. Timing them separately says which is worth
    // sharing.
    println!("per-destination pipeline, by stage:");
    let (mut supply_total, mut encode_total, mut compress_total) = (0.0, 0.0, 0.0);
    let (mut wire_bytes, mut encoded_bytes) = (0usize, 0usize);
    for pass in 1..=passes {
        let started = Instant::now();
        let batches = collect(&mut endpoint, needs.clone());
        let supply = started.elapsed().as_secs_f64();

        let started = Instant::now();
        let encoded: Vec<Vec<u8>> = batches
            .iter()
            .map(|batch| bincode::serialize(batch).expect("frames should encode"))
            .collect();
        let encode = started.elapsed().as_secs_f64();

        // bincode 1.3's `serialize` runs a sizing traversal and then an
        // encoding traversal, and `send_frame` then copies the result a
        // third time while assembling the payload. `serialize_into` a
        // reused buffer removes the sizing pass and the allocation. If that
        // is most of the encode cost, it is ten lines and no protocol
        // change — and it shrinks whatever a shared cache could save.
        let started = Instant::now();
        let mut scratch: Vec<u8> = Vec::with_capacity(16 * 1024 * 1024);
        for batch in &batches {
            scratch.clear();
            bincode::serialize_into(&mut scratch, batch).expect("frames should encode");
        }
        let encode_reused = started.elapsed().as_secs_f64();

        let started = Instant::now();
        let mut wire = 0usize;
        for payload in &encoded {
            if payload.len() >= 256 {
                wire += lz4_flex::block::compress(payload).len();
            } else {
                wire += payload.len();
            }
        }
        let compress = started.elapsed().as_secs_f64();

        supply_total += supply;
        encode_total += encode;
        compress_total += compress;
        wire_bytes = wire;
        encoded_bytes = encoded.iter().map(Vec::len).sum();
        println!(
            "  pass {pass}: supply {supply:.2}s  encode {encode:.2}s  \
             (reused buffer {encode_reused:.2}s)  compress {compress:.2}s",
        );
    }

    // The stage-by-stage numbers above time bincode and LZ4 directly. This
    // times the real path instead — Connection::send, which is what every
    // destination actually runs — so the frame-assembly change is measured
    // where it ships rather than in a synthetic stand-in. The writer is a
    // sink, so this is pure CPU with no peer and no pipe.
    let batches = collect(&mut endpoint, needs.clone());
    let requests: Vec<Request> = batches.into_iter().map(Request::StagePush).collect();
    let mut send_times = Vec::new();
    for _ in 0..5 {
        let mut connection =
            Connection::from_streams(Box::new(std::io::empty()), Box::new(std::io::sink()));
        let started = Instant::now();
        for request in &requests {
            connection.send(request).expect("frame should send");
        }
        send_times.push(started.elapsed().as_secs_f64());
    }
    send_times.sort_by(|a, b| a.partial_cmp(b).expect("times compare"));
    println!(
        "\nreal send path (Connection::send over a sink), 5 runs: {}",
        send_times
            .iter()
            .map(|t| format!("{t:.3}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!("  median {:.3}s", send_times[2]);

    // The whole sharing design rests on this: two sessions with identical
    // need lists must produce byte-identical batches, or a cache keyed on
    // (need-list, batch ordinal) would serve one destination another's
    // bytes. Verify it rather than assume it.
    let first = collect(&mut endpoint, needs.clone());
    let second = collect(&mut endpoint, needs.clone());
    let identical = first.len() == second.len()
        && first.iter().zip(second.iter()).all(|(a, b)| {
            bincode::serialize(a).expect("encodes") == bincode::serialize(b).expect("encodes")
        });
    println!(
        "\nbatch determinism: {} batches, identical across two supply streams: {}",
        first.len(),
        if identical { "yes" } else { "NO" }
    );

    let per = |total: f64| total / passes as f64;
    let each = per(supply_total) + per(encode_total) + per(compress_total);
    println!("\nper destination, averaged over {passes} passes:");
    println!(
        "  supply    {:.2}s  ({:.0}%)",
        per(supply_total),
        100.0 * per(supply_total) / each
    );
    println!(
        "  encode    {:.2}s  ({:.0}%)",
        per(encode_total),
        100.0 * per(encode_total) / each
    );
    println!(
        "  compress  {:.2}s  ({:.0}%)",
        per(compress_total),
        100.0 * per(compress_total) / each
    );
    println!(
        "  reconcile {reconcile_time:.2}s  ({:.0}%)",
        100.0 * reconcile_time / (each + reconcile_time)
    );
    println!("  total     {:.2}s", each + reconcile_time);
    println!(
        "\nscan, already shared by the observer: {:.2}s",
        scan_time.as_secs_f64()
    );
    println!(
        "encoded {:.1} MB, on the wire {:.1} MB after LZ4",
        encoded_bytes as f64 / 1_048_576.0,
        wire_bytes as f64 / 1_048_576.0
    );
    let _ = std::fs::remove_dir_all(&staging);
}

/// Runs one full supply stream, returning the batches exactly as the
/// controller would pump them, so the encode and compress stages downstream
/// see the same units the transport sees.
fn collect(endpoint: &mut LocalEndpoint, needs: Vec<StagingNeed>) -> Vec<Vec<TransferFrame>> {
    endpoint.supply_open(needs).expect("supply should open");
    let mut batches = Vec::new();
    loop {
        let batch = endpoint.supply_pull(1024).expect("supply should pull");
        if batch.is_empty() {
            break;
        }
        batches.push(batch);
    }
    batches
}

fn file_requests(snapshot: &Snapshot) -> Vec<FileRequest> {
    fn collect(node: &Node, path: &str, requests: &mut Vec<FileRequest>) {
        match &node.content {
            Content::File { digest, .. } => requests.push(FileRequest {
                path: path.to_owned(),
                digest: *digest,
            }),
            Content::Directory(children) => {
                for child in children.iter() {
                    let child_path = if path.is_empty() {
                        child.name.clone()
                    } else {
                        format!("{path}/{}", child.name)
                    };
                    collect(child, &child_path, requests);
                }
            }
            _ => {}
        }
    }
    let mut requests = Vec::new();
    if let Some(root) = snapshot.root.as_ref() {
        collect(root, "", &mut requests);
    }
    requests
}

fn total_bytes(root: &std::path::Path, requests: &[FileRequest]) -> u64 {
    requests
        .iter()
        .filter_map(|request| std::fs::metadata(root.join(&request.path)).ok())
        .map(|metadata| metadata.len())
        .sum()
}
