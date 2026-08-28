# Three limits: cold sync throughput, latency floor, memory

Minimal investigations into the three numbers nobody had explained. Two
needed a real peer and used one source/destination pair; the memory work is
entirely local.

## 1. Cold sync throughput — we are at the destination's filesystem ceiling

A cold sync moved 496 MB in ~45s, about 10 MB/s, on instances whose network
does gigabits. That looked like a tenfold gap. It is not a gap.

Four measurements on the same corpus (62,952 files, 496 MB), same pair:

| what | time | rate |
|---|---|---|
| raw ssh stream, 496 MB of zeros | 1.2s | **413 MB/s** |
| `tar` over ssh, same tree | 41.8s | 11.9 MB/s |
| local `untar` on the destination, **no network** | 45.6s | 10.9 MB/s |
| autobahn cold sync | 45.3s | 10.9 MB/s |

The transport does 413 MB/s, so it is not the constraint. Creating those
63,000 files on the destination with no network involved at all takes 45.6s
— and autobahn's whole sync takes 45.3s, very slightly *less*, because the
transfer overlaps the writes.

**The bottleneck is the destination creating files, and autobahn is already
at that ceiling.** `tar` over ssh is 8% faster, which is the only headroom
visible, and it buys that by doing none of the digesting, staging, renaming,
or verification that makes a sync resumable and safe.

This corrects a claim made earlier in this work, that a tenfold throughput
opportunity was sitting unexploited. There is no such opportunity here. The
earlier reasoning assumed that because the number was far below the NIC's
capability, the NIC was the relevant ceiling. The relevant ceiling was the
filesystem, which nobody had measured.

The consequence for the payload-cache design is real: on faster destination
storage the ceiling rises, and the source's CPU becomes the constraint sooner
than these numbers suggest.

## 2. Latency floor — mostly a deliberate debounce

Single-file edits, propagated to a real remote destination, timed by embedding
the send time in the file and stamping arrival on the destination (both
instances share the Amazon time source; measured skew was well under the
values in question).

| corpus | entries | min | p50 |
|---|---|---|---|
| sub5k | 6,636 | 44.7 ms | **45.7 ms** |
| sub50k | 62,952 | 61.1 ms | **61.8 ms** |

Ten times the entries costs 16 ms, so the floor is mostly fixed. Decomposed:

- **~20 ms is the settle window.** `SETTLE` is 100 ms maximum sampled in
  `QUIET` slices of 20 ms (`src/supervisor/mod.rs:370`). An isolated edit
  clears after one quiet slice, so ~20 ms is paid deliberately, to avoid
  syncing halfway through a burst of writes.
- **~25 ms is the cycle itself** — scan, reconcile, transfer, apply, verify.
- **~16 ms at 63k entries scales with the tree**, roughly 0.29 µs per entry.

So the floor is about half deliberate debounce and half real work, and the
network is negligible (in-connection round trip is well under a millisecond
intra-AZ). Lowering `QUIET` would cut latency directly at the cost of more
redundant cycles on bursty writers. The ~25 ms cycle cost is the part that
would need actual optimization, and it has never been profiled.

## 3. Memory — the tree is not what costs

Resident size around a scan, against what the snapshot's contents must
occupy (one `Node` per entry at 96 bytes, plus names, plus each directory's
children vector):

| entries | RSS delta | theoretical | bytes/entry |
|---|---|---|---|
| 5,089 | 0.4 MB | 0.5 MB | 73 |
| 20,141 | 1.6 MB | 2.0 MB | 86 |
| 63,601 | 4.9 MB | 6.3 MB | 81 |

**A 63,000-entry tree costs about 5–6 MB.** RSS delta reads slightly below
theoretical because the allocator reuses pages freed by the warm-up scan, so
treat these as the same number: the tree is at its natural size, with no
per-entry bloat to remove.

That reframes the 99 MB the daemon was measured at during a ten-destination
fan-out. The tree is ~6% of it. The rest is buffers, and the arithmetic is
suggestive: `SUPPLY_TARGET_BYTES` is 8 MB, and a sending thread holds an
encode buffer and a compression buffer of comparable size, so a session
actively supplying can hold on the order of 24–32 MB transiently, with
several sessions overlapping.

**That is a hypothesis, not a measurement.** It has not been tested, and it
is the obvious next step for anyone reducing memory: attribute the resident
set during an active multi-session transfer rather than at rest.

One thing that *was* checked: the frame scratch introduced in `c7bdd3b`
retains up to 16 MB per thread, which would be a real cost if those threads
were long-lived. They are not — the pusher is a scoped thread created per
staging phase (`src/session/mod.rs:537`), so its thread-local scratch is
released when the phase ends.

## Harness notes

The latency measurement produced wrong answers twice before producing a right
one, both times through stale state rather than anything in the code under
test. First, `ssh -n` redirects stdin from `/dev/null`, so the heredoc that
was meant to write the destination-side poller silently wrote an empty file
and no samples were collected. Then the poller's glob spanned every corpus,
so probe files left by the failed run were read as fresh arrivals, with
timestamps minutes old — visible as percentiles in the 90-second range next
to a p50 of 62 ms, and as sample counts of 40 for 20 edits.

Both were caught by the numbers being obviously impossible rather than by any
check in the harness. A measurement that can silently read another run's
leftovers should assert its own inputs are fresh, the way the cold-sync
script asserts its destinations are empty before starting its clock.
