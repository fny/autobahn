# Why mutagen is slower and uses more memory

The [benchmark](./benchmarks.md) measured mutagen at 2,033 MB of peak
memory against autobahn's 249 MB on a Chromium checkout. It measured a 7 to
9 second median propagation latency against autobahn's fraction of a
second. This document explains where those numbers come from in mutagen's
code.

This is not a criticism of mutagen's engineering. Mutagen is a mature tool.
Most of what follows comes from two decisions that buy real things
elsewhere: protobuf as the in-memory tree, and scan-based rather than
event-based change detection. The benchmark measures what those decisions
cost at half a million files on Linux.

Citations are against upstream `master` (`6ccfeaa`, after `v0.18.0`), which
is what the benchmarked `0.19.0-dev` binary was built from. Read them with
`git show master:<path>`.

## Memory

### The entry costs 120 bytes, and 44 of them are protobuf

`pkg/synchronization/core/entry.pb.go:113` defines the tree node. Three
fields — `state`, `unknownFields`, and `sizeCache` — take **44 bytes, or
37% of the struct**. They hold no filesystem information. `Target` and
`Problem` take another 32 bytes, and both are empty for every regular file.

The tree is a protobuf message that lives in memory. It is not an in-memory
structure that serializes. That makes it cheap to send and expensive to
hold. Autobahn's `Node` is 96 bytes with no serialization fields, and its
children are a sorted list rather than a map.

A real 505,000-file tree measures approximately **227 bytes for each file**
once digests, names, and map slots are counted. That is 115 MB for one
tree. **One tree is not 2 GB.**

### Approximately eight whole trees are resident

| Structure | Where |
|---|---|
| `e.snapshot`, the last scan | `endpoint/local/endpoint.go:158` |
| `e.cache`, the digest cache | `endpoint.go:162` |
| `e.ignoreCache` | `endpoint.go:167` |
| `previous`, the poll loop's second copy | `endpoint.go:611` |
| `lastSavedCache` | `endpoint.go:549` |
| `ancestor`, held for the session | `controller.go:865` |
| beta's decoded tree | `remote/client.go:343` |
| `lastSnapshotBytes`, beta's serialized tree | `remote/client.go:34` |

The first five exist in both processes. The last three exist only in the
daemon, which is why the source host measures higher than the destination.

Two are notable. The digest cache is keyed by full path
(`cache.pb.go:126`), so every deep path is stored again as a map key. It
measures approximately 326 bytes for each entry, more than the tree node it
accelerates. And each cache entry allocates a separate `*timestamppb.Timestamp`
(`cache.pb.go:33`) to hold one `int64` and one `int32`.

### Then Go doubles it

Mutagen sets no `GOGC` and no `GOMEMLIMIT`, and never calls
`debug.SetGCPercent`. At the default, the heap goal is twice the live set,
so peak memory lands near **2× the live heap**.

Building that resident set and measuring it gives 647 MB live for the
destination and 902 MB for the source. Doubled, that is 1,294 MB and 1,803
MB, against 1,430 MB and 2,065 MB measured. The model sits 10 to 15% low,
and per-cycle garbage explains the rest.

Those two measured peaks come from the run the model was built against.
The published matrix is a separate run with the same shape: 1,361 MB and
2,033 MB for Chromium with one agent.

### Why the ratio grows with file count

The benchmark measured a 3.1× gap at 4,000 files and an 8.2× gap at 505,000.
The marginal cost of one more file is approximately **4,022 bytes** for
mutagen and **507 bytes** for autobahn. Fixed overhead dominates at 4,000
files, which is why the ratio looks small there. The gap is per-entry, so it
widens with every file.

Each full scan also allocates three new containers and a new path string for
each entry (`scan.go:843`, `:854`, `:438`). That is roughly 200 MB of fresh
allocation for each scan.

## Latency

### No official Linux build has recursive watching

This is the root cause, and it is stronger than "the SSPL build is needed".

```go
//go:build !(darwin && cgo) && !(linux && mutagensspl && mutagenfanotify) && !windows
const RecursiveWatchingSupported = false
```
— `pkg/filesystem/watching/watch_recursive_unsupported.go:1`

Recursive watching on Linux needs **both** the `mutagensspl` and
`mutagenfanotify` tags. But `scripts/build.go:189` adds only `mutagensspl`.
The tag `mutagenfanotify` appears in one place in the whole repository:
`images/sidecar/linux/Dockerfile:20`.

**Thus no official mutagen CLI or agent has recursive watching on Linux.
Only the sidecar image does.** Without it, the default portable mode falls
back to polling (`endpoint.go:240`) at a 10 second interval
(`version.go:122`). The benchmark used 5 seconds, which is better than the
default.

### The inotify assist watches at most 50 paths

Mutagen uses inotify on Linux as an accelerator, not as a watcher. The
limit is 50 watches with LRU eviction
(`watch_non_recursive_linux.go:22`). It watches only paths that changed
between two poll scans (`endpoint.go:776`). On a 505,000-file tree, at most
50 recently changed paths are watched.

### Every event triggers a full scan

The ticker and an inotify event reach the same code (`endpoint.go:731`).
That code disables acceleration, runs **a complete walk of the root**
(`:742`), compares the whole tree (`:769`), and diffs the whole tree
(`:776`).

**inotify shortens detection. It never reduces cost.** One changed file and
one hundred changed files cost the same scan.

"Accelerated scanning" means two different things (`endpoint.go:1073`).
Under recursive watching it is genuinely incremental. Under poll-based
watching, `recheckPaths` is always nil (`endpoint.go:153`), so acceleration
means returning the tree the poll loop already built. The RPC is fast
because the cost moved, not because it went away.

At 505,000 entries the stages cost seconds: 226 ms to compare, 446 ms to
diff, 732 ms to marshal 25.6 MB, 358 ms to unmarshal into a new tree, and
another 732 ms to write the ancestor. At 4,000 files every stage is below a
millisecond, and two debounce windows of 10 ms and 20 ms dominate instead.
That matches the 62 ms the benchmark measured.

**There is no threshold and no mode switch.** Every stage is O(entries), so
the degradation is continuous.

Two mechanisms fit the 7 to 9 second median: the pipeline cost above, which
is always paid, and the wait for the next poll tick when the edited path is
not one of the 50 watches. The source cannot say which dominates. A run with
a 1 second interval would separate them, and that test has not been run.

## Why concurrency makes it worse on the same tree

On an unchanged 4,000-file tree, mutagen went from 62 ms with one writer to
1,037 ms with ten and 2,690 ms with a hundred. The tree never grew. Four
mechanisms compound.

**The debouncers reset instead of expiring.** `state/coalescer.go:64` calls
`timer.Reset(window)` for each strobe. The documentation says an event is
sent only after `Strobe` has been quiet for the window. With many writers,
the 10 ms and 20 ms timers keep resetting and do not fire until a quiet gap
appears. A fixed cost becomes one that scales with writer density. This is
the most likely single cause.

**One global lock.** `scanLock` is a one-slot semaphore
(`endpoint.go:145`) that covers the poll scan, `Scan`, and `Transition`.
Work queues instead of overlapping.

**Watch thrashing above 50 paths.** With a hundred writers on a hundred
files, the LRU evicts a watch before its file is written again. Those writes
then wait for the ticker. This is why 100 writers degrade more than 10.

**Dropped events.** The delivery channel is unbuffered
(`watch_non_recursive_linux.go:57`). While the poll goroutine is inside a
scan, nothing drains it, and the notify layer discards events
(`internal/third_party/notify/watcher_inotify.go:281`). A lost event waits
for the next tick.

## Why the destination burns more than a core

In the one-direction cells the destination does no editing. On Chromium
mutagen still used 114 to 125% of a core there, against 16 to 35% for
autobahn.

The receiving endpoint does not do less work. For each `Scan` it runs a full
scan, because the preceding `Transition` disabled acceleration
(`endpoint.go:1395`). It then marshals the tree with `Deterministic: true`,
which **sorts the keys of all 40,000 content maps**, and deltifies the
resulting 25.6 MB (`remote/server.go:297`). Its own poll loop runs at the
same time, whatever the RPC does.

Writing the one small file that changed is a rounding error next to
rescanning and reserializing the whole tree.

## What this does not say

**Mutagen's small-tree performance is genuinely good.** Before autobahn
changed its settle logic, mutagen led the 4,000-file single-writer cell at
61 ms against 101 ms. Autobahn now leads that cell at 41 ms, and leads every
other cell in the matrix. But none of the costs in this document bite at
that size.

**The design has upsides.** A protobuf tree needs no conversion step to
travel, which is what a network-transparent design wants. Scan-based
detection cannot miss a change because a watch was evicted or a queue
overflowed. That is exactly the failure mode autobahn must guard against
with a periodic full scan and an incomplete flag.

**The platform matters.** On macOS, and in the Linux sidecar image,
recursive watching works and accelerated scanning is genuinely incremental.
Much of the latency section would read differently there. The benchmark
measured Linux hosts with an official build, which is what a Linux user
gets.

## Caveats

- The memory model is built from the list of resident fields and measured
  with equivalents. It is not a heap profile of mutagen on a real Chromium
  tree. It agrees with the measurement to 10 to 15%.
- Stage timings come from different hardware than the benchmark. The
  relative sizes hold. The absolute milliseconds do not transfer.
- Upstream has in-flight work on several of these behaviors, including a
  compact tree and an inline cache timestamp. This document describes the
  released code that the benchmark measured.

## See also

- [Benchmarks](./benchmarks.md) — the measurements this document explains
- [The benchmark matrix](./benchmark-matrix.md) — every cell, every percentile
- [How autobahn works](./how-it-works.md) — autobahn's side of the same decisions
