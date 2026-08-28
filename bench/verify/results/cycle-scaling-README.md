# What scales with tree size on the latency path

Single-edit latency measured 45.7ms over 6,636 entries and 61.8ms over
62,952. The 16ms difference implied a per-entry term worth understanding,
because at Chromium's half a million entries it would dominate everything
else.

All of this is source-side CPU and disk, so it measures locally with no
network and no fan-out confound. Harness: `examples/cycle_cost.rs`. It scans
a corpus, edits one file, rescans, and times each whole-tree phase of the
resulting cycle.

## Result

Per single-file edit:

| entries | rescan | reconcile | validate | encode | write | total | ancestor |
|---|---|---|---|---|---|---|---|
| 5,089 | 0.4 | 0.4 | 0.1 | 0.6 | 0.3 | 1.7ms | 0.4 MB |
| 20,141 | 0.7 | 1.5 | 0.4 | 1.8 | 0.9 | 5.4ms | 1.6 MB |
| 63,601 | 1.7 | 4.6 | 1.9 | 8.3 | 8.0 | 24.4ms | 5.1 MB |
| 502,501 | 10.0 | 37.3 | 13.8 | 54.2 | 111.3 | **226.6ms** | **40.6 MB** |

The 63k total lines up with the 16ms difference seen over the network, so
these five phases account for the per-entry term.

**At Chromium scale a single saved file costs 227ms of whole-tree work and
writes a 40.6 MB file.** Note that this was measured, not extrapolated: a
linear projection from the smaller trees predicted ~145ms and would have
understated it, because the write cost grows faster than entry count once
the ancestor stops fitting comfortably in the page cache. The write column
is the most disk-dependent number here and varies between runs; the encode
column does not.

## Where it goes, and what the data already knows

At 502,501 entries:

- **Ancestor persistence is 73%** — encode 54.2ms plus write 111.3ms. Every
  edit re-serializes the entire hierarchy with bincode and writes all 40.6 MB
  of it to disk, to record that one file changed.
- **Reconcile is 16%** (37.3ms) — a full three-way walk.
- **Validate is 6%** (13.8ms) — the whole ancestor is re-validated each cycle.
- **Rescan is 4%** (10.0ms) — already incremental.

The striking part is that the tree already knows almost none of it changed.
Copy-on-write rewrites only the path from the root to the edited file, so
**2,499 of 2,501 directories are pointer-identical between the two
snapshots**. The information needed to skip 99.9% of this work is present in
the data structure and is not being used.

`tree::nodes_share_storage` exists and `tree/diff.rs:32` already prunes with
it. `tree/reconcile.rs` does not.

## What could be done, in order of value

1. **Stop rewriting the whole ancestor.** This is 73% of the cost and the
   only item that also does I/O proportional to the tree. The synchronous
   write itself should not be made asynchronous — the comment at
   `session/mod.rs:415` sets out exactly why, and it is right: the ancestor
   carries provenance, and a stale one turns a deliberate revert into
   content that gets silently overwritten. But durability-before-proceeding
   does not require rewriting everything. A journal of the cycle's changes,
   appended and fsynced, with periodic compaction, preserves the property
   while making the write proportional to the edit rather than the tree.
2. **Prune reconcile by pointer.** Where the ancestor and a side share a
   subtree allocation, nothing in it changed. Worth 16%. The open question
   is whether the beta side shares storage too: beta arrives from a remote
   scan, but `ScanUnchanged` returns the previous snapshot's own Arcs
   (`endpoint/remote.rs:187`), so it may well hold in the common case. That
   needs checking before the work is scoped, not assuming.
3. **Validate only what changed.** Worth 6%, and the same pruning idea — a
   subtree that was validated when written and has not been touched since
   does not need revalidating.

Items 2 and 3 are the same mechanism applied twice and would likely share an
implementation. Item 1 is separate, larger, and touches the durability story,
so it deserves its own design and review.
