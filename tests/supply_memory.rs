//! Supplying a file streams it: the supplier's memory and its time to the
//! first frame do not grow with the file (F-H7).
//!
//! A counting global allocator measures the peak live heap while a stream
//! is drained, which is why this is a binary of its own, with one test:
//! nothing else may allocate while it measures.

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::{self, File};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use autobahn::endpoint::local::{EndpointOptions, LocalEndpoint, SUPPLY_TARGET_BYTES};
use autobahn::endpoint::{Endpoint, FileRequest, StagingNeed, TransferFrame};
use autobahn::rsync::{BlockHash, Signature};
use autobahn::tree::{Content, Digest};

/// The system allocator, counting live bytes and their peak.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize) {
    let live = LIVE.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

// SAFETY: every call is forwarded to the system allocator unchanged; the
// counters only observe it.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = System.alloc(layout);
        if !pointer.is_null() {
            grew(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        System.dealloc(pointer, layout);
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let moved = System.realloc(pointer, layout, size);
        if !moved.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            grew(size);
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// A sparse file of `size` zero bytes: no disk, and fast to read.
fn sparse(path: &Path, size: u64) {
    File::create(path)
        .and_then(|file| file.set_len(size))
        .expect("the file should be creatable");
}

/// The digest the scan recorded for `name` at the root.
fn scanned_digest(endpoint: &mut LocalEndpoint, name: &str) -> Digest {
    let snapshot = endpoint.scan().expect("the scan should succeed");
    let node = snapshot
        .root
        .as_ref()
        .and_then(|root| root.child(name))
        .expect("the file should be scanned");
    match &node.content {
        Content::File { digest, .. } => *digest,
        other => panic!("{name} scanned as {other:?}"),
    }
}

/// A destination's claim to hold one block that matches nothing here —
/// which forces a real delta, whose output is the whole file.
fn one_fake_block() -> Signature {
    Signature {
        block_size: 1024,
        last_block_size: 1024,
        hashes: vec![BlockHash {
            weak: 0x1234_5678,
            strong: [0xAB; 32],
        }],
    }
}

fn need(name: &str, digest: Digest, signature: Signature) -> StagingNeed {
    StagingNeed {
        request: FileRequest {
            path: name.into(),
            digest,
        },
        signature,
    }
}

/// Drains a supply of one need, returning the peak live heap above what
/// was live when it began, and the bytes of content it carried.
fn drain_measuring(endpoint: &mut LocalEndpoint, need: StagingNeed) -> (usize, u64) {
    let baseline = LIVE.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    endpoint
        .supply_open(vec![need])
        .expect("supply should open");
    let mut carried = 0u64;
    loop {
        let frames = endpoint
            .supply_pull(usize::MAX)
            .expect("supply should pull");
        if frames.is_empty() {
            break;
        }
        for frame in &frames {
            match frame {
                TransferFrame::Op(autobahn::rsync::Op::Data(data)) => carried += data.len() as u64,
                TransferFrame::EndOfFile { error } => assert!(error.is_none(), "{error:?}"),
                _ => {}
            }
        }
    }
    (PEAK.load(Ordering::Relaxed) - baseline, carried)
}

/// The time the first frame of a supply of one need takes to come back.
fn time_to_first_frame(endpoint: &mut LocalEndpoint, need: StagingNeed) -> Duration {
    endpoint
        .supply_open(vec![need])
        .expect("supply should open");
    let started = Instant::now();
    let frames = endpoint.supply_pull(2).expect("supply should pull");
    let elapsed = started.elapsed();
    assert!(
        matches!(frames.first(), Some(TransferFrame::Begin { .. })),
        "{frames:?}"
    );
    // Closing the stream stops whatever it had begun.
    endpoint
        .supply_open(Vec::new())
        .expect("supply should open");
    elapsed
}

#[test]
fn supplying_a_file_streams_it_in_bounded_memory_and_time() {
    let keep = tempfile::tempdir().expect("temporary directory should be creatable");
    let root = keep.path().join("root");
    fs::create_dir_all(&root).expect("the root should be creatable");
    // One-shot: every scan walks the disk. The files are made just before
    // the scans that must see them, and a watcher's report of that can
    // arrive later (macOS's FSEvents does), letting a scan serve a snapshot
    // from before the file existed.
    let mut endpoint = LocalEndpoint::new(
        root.clone(),
        keep.path().join("staging"),
        EndpointOptions {
            one_shot: true,
            ..EndpointOptions::default()
        },
    )
    .expect("the endpoint should be creatable");

    // Memory: the whole file never sits in the supplier, with or without
    // a (hostile) signature to delta against.
    const SIZE: u64 = 256 << 20;
    let bound = 4 * SUPPLY_TARGET_BYTES;
    sparse(&root.join("big.bin"), SIZE);
    let digest = scanned_digest(&mut endpoint, "big.bin");
    for (label, signature) in [
        ("no signature", Signature::default()),
        ("one fake block", one_fake_block()),
    ] {
        let (peak, carried) = drain_measuring(&mut endpoint, need("big.bin", digest, signature));
        assert_eq!(carried, SIZE, "{label}: the whole file was not carried");
        assert!(
            peak < bound,
            "{label}: supplying {SIZE} bytes peaked at {peak} bytes live (bound {bound})"
        );
    }

    // Time: the first frame of a 1 GiB file comes back as fast as that of
    // any other.
    fs::remove_file(root.join("big.bin")).expect("the file should be removable");
    sparse(&root.join("huge.bin"), 1 << 30);
    let digest = scanned_digest(&mut endpoint, "huge.bin");
    for (label, signature) in [
        ("no signature", Signature::default()),
        ("one fake block", one_fake_block()),
    ] {
        let elapsed = time_to_first_frame(&mut endpoint, need("huge.bin", digest, signature));
        assert!(
            elapsed < Duration::from_secs(1),
            "{label}: the first frame of 1 GiB took {elapsed:?}"
        );
    }
}
