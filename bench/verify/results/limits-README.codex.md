# Three limits: cold sync throughput, latency floor, memory

This historical record applies to the measured setup. See [Current benchmarks](../../../docs/benchmarks.codex.md) for later results.

Throughput and latency used a real remote peer. Memory tests were local.

## 1. Cold sync throughput

A 496 MB sync took about 45 seconds, or 10 MB/s, despite a much faster network. Four tests used the same 62,952-file corpus and host pair:

| what | time | rate |
|---|---|---|
| raw ssh stream, 496 MB of zeros | 1.2s | **413 MB/s** |
| `tar` over ssh, same tree | 41.8s | 11.9 MB/s |
| local `untar` on the destination, **no network** | 45.6s | 10.9 MB/s |
| autobahn cold sync | 45.3s | 10.9 MB/s |

Raw transport reached 413 MB/s. Local extraction without a network took 45.6 seconds, close to Autobahn’s 45.3 seconds.

These results identify destination file creation as the main constraint in this workload. Transfer overlapped destination writes.

Tar over SSH was about 8% faster. It omitted Autobahn’s digest checks, staging, rename publication, and resumable synchronization behavior.

The data does not support the earlier proposed tenfold throughput opportunity. NIC capacity was not the relevant limit for this many-file workload.

Faster destination storage can shift the constraint toward source CPU, which matters when evaluating payload caching.

## 2. Latency floor

The probe embedded a send timestamp in each file and recorded arrival on the destination. Both hosts used the Amazon time source. Measured skew was small relative to these values.

| corpus | entries | min | p50 |
|---|---|---|---|
| sub5k | 6,636 | 44.7 ms | **45.7 ms** |
| sub50k | 62,952 | 61.1 ms | **61.8 ms** |

The tenfold entry increase added about 16 ms. The analysis divided latency into:

- About 20 ms for one quiet settle slice.
- About 25 ms for scan, reconciliation, transfer, apply, and verification.
- About 16 ms of additional work at 63k entries, roughly 0.29 µs per entry.

At this revision, `SETTLE` capped at 100 ms and `QUIET` used 20 ms slices (`src/supervisor/mod.rs:370`). An isolated edit needed one quiet slice.

The intra-AZ connection round trip was below one millisecond. Lowering `QUIET` could reduce latency but increase redundant work during bursts. The approximately 25 ms base cycle had not yet been profiled.

## 3. Memory

The theoretical estimate includes a 96-byte `Node` per entry, names, and directory child vectors:

| entries | RSS delta | theoretical | bytes/entry |
|---|---|---|---|
| 5,089 | 0.4 MB | 0.5 MB | 73 |
| 20,141 | 1.6 MB | 2.0 MB | 86 |
| 63,601 | 4.9 MB | 6.3 MB | 81 |

A 63,000-entry snapshot used about 5–6 MB. RSS deltas fell slightly below the theoretical estimate because the allocator reused pages from a warmup scan.

The daemon’s earlier ten-destination peak was 99 MB. The tree accounted for about 6% of it.

Buffers were a proposed explanation for the rest. `SUPPLY_TARGET_BYTES` was 8 MB, with additional encoding and compression buffers. An active supply session could retain roughly 24–32 MB transiently.

That estimate was a hypothesis, not measured attribution. Measuring active multi-session memory was the proposed next step.

One buffer lifetime was checked: frame scratch from `c7bdd3b` retained up to 16 MB per thread. The pusher used a scoped thread per staging phase (`src/session/mod.rs:537`), so its thread-local scratch ended with the phase.

## Harness notes

Two setup defects initially invalidated latency results.

`ssh -n` redirected stdin from `/dev/null`, so a heredoc produced an empty destination poller. No samples were collected.

A later poller glob included files from all corpora. Leftover probes appeared as new arrivals with old timestamps, producing 90-second tails beside a 62 ms median and forty samples for twenty edits.

The implausible values exposed these defects. Future runs require explicit fresh-input checks, like the cold-sync harness’s empty-destination assertion.
