# Why mutagen is slower and uses more memory

The [benchmark](../BENCHMARK.md) measured mutagen at roughly **2 GB of
resident memory** against autobahn's 273 MB on a Chromium checkout, and a
**7–9 second** median propagation latency against autobahn's fraction of a
second. This document explains where those numbers come from in mutagen's
implementation.

It is not a criticism of mutagen's engineering. Mutagen is a careful,
mature tool, and most of what follows traces back to two decisions —
protobuf as the in-memory tree representation, and a scan-based rather
than event-based model of change — that buy real things elsewhere. What
the benchmark measures is what those decisions cost at half a million
files on Linux.

**Scope.** Citations are against upstream `master` (`6ccfeaa`,
post-`v0.18.0`), which is what the benchmarked `0.19.0-dev` binary was
built from. Read them with
`git show master:<path>`. Where a number below is modeled or measured on
different hardware rather than taken from the benchmark, it says so.

---

## 1. Memory

### 1.1 The entry costs 120 bytes, and 44 of them are protobuf

`pkg/synchronization/core/entry.pb.go:113`:

```go
type Entry struct {
	state         protoimpl.MessageState  //  8 B
	Kind          EntryKind               //  4 B
	Contents      map[string]*Entry       //  8 B
	Digest        []byte                  // 24 B
	Executable    bool                    //  1 B
	Target        string                  // 16 B
	Problem       string                  // 16 B
	unknownFields protoimpl.UnknownFields // 24 B
	sizeCache     protoimpl.SizeCache     //  4 B
}
```

`unsafe.Sizeof` reports **120 bytes**, landing in Go's 128-byte size
class. Of that, `state`, `unknownFields`, and `sizeCache` are **44 bytes
(37%) of protobuf machinery carrying no filesystem information**. Another
32 bytes — `Target` and `Problem` — are dead for every regular file; the
generated comments confirm `Target` is non-empty only for symlinks and
`Problem` only for problematic entries (`entry.pb.go:126`).

The tree is a protobuf message that happens to live in memory, rather than
an in-memory structure that happens to serialize. That is what makes it
cheap to send and expensive to hold.

For comparison, autobahn's `Node` is 96 bytes with no serialization
overhead, and its children are a sorted `Vec` rather than a
`map[string]*Entry` — which removes the per-entry map slot as well.

Building a real 505,000-file tree with 20-byte SHA-1 digests (the default
hash: `pkg/synchronization/version.go:40`) and measuring `HeapAlloc` with
the GC disabled gives **~227 bytes per file** once digest arrays, leaf-name
strings, and amortized map slots are counted — about 115 MB for one tree.

**One tree is not 2 GB.** The answer is how many of them there are.

### 1.2 Roughly eight whole-tree structures are resident at once

| # | Structure | Where |
|---|---|---|
| 1 | `e.snapshot` — the last scan's tree | `endpoint/local/endpoint.go:158` |
| 2 | `e.cache` — 505k-entry digest cache | `endpoint.go:162` |
| 3 | `e.ignoreCache` — 505k-entry ignore cache | `endpoint.go:167` |
| 4 | `previous` — the poll loop's **second copy** of the tree | `endpoint.go:611` |
| 5 | `lastSavedCache` — the cache-saver's retained older cache | `endpoint.go:549` |
| 6 | `ancestor` — loaded at session start, held for its life | `controller.go:865` |
| 7 | `βSnapshot.Content` — beta's tree, freshly decoded each cycle | `remote/client.go:343` |
| 8 | `lastSnapshotBytes` — beta's **serialized** snapshot, retained permanently | `remote/client.go:34` |

Items 1–5 exist in *both* processes. Items 6–8 exist only in the daemon,
which is why the source host measures higher than the destination.

Two of these deserve attention. The digest cache is keyed by **full
slash-joined path** (`cache.pb.go:126`), so every deep Chromium path is
stored again as a map key — measured at ~326 bytes per entry, more than
the tree node it accelerates. And `CacheEntry.ModificationTime` is a
**separately heap-allocated `*timestamppb.Timestamp`**
(`cache.pb.go:33`, allocated at `scan.go:234`): a second allocation per
file, in a 64-byte size class, to hold an `int64` and an `int32`.

### 1.3 Then Go doubles it

Mutagen sets no `GOGC`, no `GOMEMLIMIT`, and never calls
`debug.SetGCPercent` or `FreeOSMemory` — a repo-wide grep finds no
non-test match. At the default `GOGC=100`, the heap goal is twice the live
set, so peak RSS lands near **2× live heap** plus runtime overhead.

Constructing the resident set above literally and measuring it:

```
destination (agent):   647 MB live × 2  ≈ 1,294 MB     measured 1,430 MB
source (daemon):       902 MB live × 2  ≈ 1,803 MB     measured 2,065 MB
```

The model sits 10–15% below both measurements, with the remainder
explained by per-cycle transients (§1.5) and runtime overhead. That is
close enough to say the ~2 GB is understood rather than mysterious.

### 1.4 Why the ratio grows with file count

The benchmark showed mutagen using 2.6× autobahn's memory at 4,000 files
but 7.6× at 505,000. Taking the marginal cost between those two points:

| | per additional file |
|---|---|
| mutagen | ~4,022 B |
| autobahn | ~507 B |

The model predicts ~3,570 B/file for mutagen, within 11% of measured. The
fixed offset — Go runtime, gRPC, protobuf registries — is what dominates
at 4,000 files and makes the ratio look modest there. **The gap is
per-entry, so it widens with every file you add.**

### 1.5 Per-cycle garbage on top

Every full scan allocates three complete new containers: a new `Cache` map
with 505k fresh string keys (`scan.go:843`), a new `IgnoreCache`
(`scan.go:854`), and a new `Entry` tree — a fresh map per directory
(`scan.go:383`) and a fresh `&Entry{}` per file (`scan.go:257`) — plus a
newly allocated path string per entry (`scan.go:438`). That is on the
order of 200 MB of fresh allocation per full scan, and §2 explains how
often a full scan happens.

`core.Apply` additionally deep-copies the tree on every cycle that has
changes (`apply.go:22`), reallocating every directory entry and every
directory content map: measured at +26.6 MB and 157 ms for a 505k tree.

Notably, **reconciliation itself is memory-cheap** — `reconcile.go:523`
produces only change lists, with a slim per-change copy. The cost is in
scanning, copying, and serializing, not in the diffing logic.

---

## 2. Latency

### 2.1 No official Linux build has recursive watching

This is the root cause of the Linux latency numbers, and it is stronger
than "the SSPL build is required".

```go
//go:build !(darwin && cgo) && !(linux && mutagensspl && mutagenfanotify) && !windows
const RecursiveWatchingSupported = false
```
— `pkg/filesystem/watching/watch_recursive_unsupported.go:1`

Native recursive watching on Linux requires **both** the `mutagensspl`
*and* `mutagenfanotify` build tags. But `scripts/build.go:189` adds only
`mutagensspl`, and `mutagenfanotify` appears in exactly one place in the
entire repository:

```
images/sidecar/linux/Dockerfile:20
```

**So no officially built mutagen CLI or agent has recursive watching on
Linux — SSPL or not. Only the sidecar container image does.** On an
ordinary Linux host, recursive watching is off.

With recursive watching unavailable, the default `portable` watch mode
reifies to **polling** (`endpoint.go:240`), and the default poll interval
is **10 seconds** (`version.go:122`). The benchmark configured 5 seconds,
which is more favorable than the default.

### 2.2 The inotify assist is capped at 50 watches

Mutagen does use inotify on Linux, but as an accelerator rather than a
watcher:

```go
inotifyDefaultMaximumWatches = 50
```
— `watch_non_recursive_linux.go:22`

Watches are LRU with eviction (`:61`), and they are established only for
paths that **changed between two consecutive poll scans**
(`endpoint.go:776`). On a 505,000-file tree, at most 50 recently-changed
paths are natively watched at any moment. Everything else waits for the
ticker.

### 2.3 Every event triggers a full scan

This is the part that matters most. Whether woken by the ticker or by an
inotify event, both paths converge on the same code
(`endpoint.go:731`):

1. take `scanLock`, set `e.accelerate = false` (`:732`)
2. **`e.scan(ctx, nil, nil)` — a complete walk of the entire root** (`:742`)
3. `!snapshot.Equal(previous)` — full recursive comparison (`:769`)
4. `core.Diff(previous.Content, snapshot.Content)` — full recursive diff,
   run on every iteration whenever an inotify watcher is live, which on
   Linux portable means always (`:776`)
5. `previous = snapshot` — retaining that second full tree (`:789`)

**inotify shortens detection latency; it never reduces cost.** One changed
file and a hundred changed files cost the same full scan.

"Accelerated scanning", the default mode (`version.go:82`), means two
different things depending on watch mode (`endpoint.go:1073`). Under
recursive watching it is genuinely incremental — baseline plus recheck
paths. Under poll-based watching, `recheckPaths` is nil by construction
(`endpoint.go:153`) and acceleration degrades to *returning the snapshot
the poll loop already computed*. The RPC is fast because the cost was paid
elsewhere, not because it was avoided.

### 2.4 The pipeline, stage by stage

Measured on a 505k-entry tree (different hardware from the benchmark, so
read the magnitudes rather than the absolute milliseconds):

| Stage | Cost | Where |
|---|---|---|
| `Entry.Equal` deep compare | 226 ms | `endpoint.go:769` |
| `core.Diff` over full trees | 446 ms | `endpoint.go:777` |
| `proto.Marshal` (deterministic) → 25.6 MB | **732 ms** | `remote/server.go:312` |
| `DeltifyBytes` over 25.6 MB | 53 ms | `remote/server.go:318` |
| `BytesSignature` | 52 ms | `remote/client.go:248` |
| `PatchBytes` | 54 ms | `remote/client.go:321` |
| `proto.Unmarshal` → a brand-new 505k tree | **358 ms** | `remote/client.go:344` |
| `Snapshot.EnsureValid` | 72 ms | `remote/client.go:353` |
| `Entry.Copy` (deep, preserving leaves) | 157 ms | `apply.go:23` |
| `EnsureValid` on the new ancestor | 89 ms | `controller.go:1398` |
| ancestor `proto.Marshal` + 25.6 MB disk write | ~732 ms | `controller.go:1405` |

Plus the `readdir`/`lstat` walk of 505,000 entries, which is I/O bound.

At 4,000 files every one of these is sub-millisecond, and latency is
dominated instead by two debounce windows — 10 ms and 20 ms
(`endpoint.go:31`) — plus RPC round trips. Thirty milliseconds of debounce
and about thirty of work is very close to the 62 ms the benchmark
measured.

**There is no threshold and no mode switch.** Every stage is O(entries),
so the same pipeline that costs 60 ms at 4k costs seconds at 505k. The
degradation is continuous.

### 2.5 Two mechanisms, and which one dominates is untested

The 7–9 second median is consistent with either (a) the pipeline cost
above, which is always paid, or (b) waiting on the poll ticker because the
edited path was not among the 50 LRU inotify watches, which for a 5-second
interval has an expected wait of 2.5 seconds.

Both are real; the source alone cannot say which dominated. The
distinguishing experiment is cheap — rerun with a 1-second poll interval.
If the median drops sharply, ticker latency dominated; if it barely moves,
pipeline cost did. That experiment has not been run.

---

## 3. Why concurrency makes it worse on the *same* tree

The benchmark found that on an unchanged 4,000-file corpus, mutagen went
from 62 ms with one writer to 1,028 ms with ten and 2,680 ms with a
hundred. The tree never grew, so this is not a scale effect. Four
mechanisms compound:

**The debouncers reset rather than expire.** `state/coalescer.go:64`:

```go
case <-c.strobes:
	timer.Stop()
	…
	timer.Reset(window)   // every strobe pushes the deadline out
```

The documented behavior is that "an event will only be sent after Strobe
hasn't been called for the coalescing window period"
(`coalescer.go:80`). With many writers producing events continuously, the
10 ms and 20 ms timers **keep resetting and do not fire until a quiet gap
appears**. A fixed 30 ms cost becomes one that scales with writer density.
This is the most likely single explanation for the progression.

**One global lock per endpoint.** `scanLock` is a one-slot semaphore
(`endpoint.go:145`) covering the poll goroutine's scan, `Scan`, and
`Transition` (`:732`, `:1042`, `:1287`). Work queues rather than overlaps.

**Watch thrashing past 50 paths.** With a hundred writers touching a
hundred distinct files, the 50-entry LRU evicts a watch before its file is
written again, so those writes fall back to the ticker. This is why the
100-writer case degrades disproportionately relative to 10.

**Events are dropped during scans.** The delivery channel is unbuffered
(`watch_non_recursive_linux.go:57`) and the raw queue holds 50. While the
poll goroutine is inside a scan it is not draining, and the notify layer
discards non-blockingly (`internal/third_party/notify/watcher_inotify.go:281`).
`IN_Q_OVERFLOW` is discarded rather than surfaced
(`watcher_inotify.go:300`). A lost event pushes detection to the next
tick.

There is also a feedback path: every `Transition` that changed disk clears
acceleration and strobes the poll signal (`endpoint.go:1393`), forcing an
extra full scan and an extra cycle per batch.

---

## 4. Why the destination burns more than a core

In the unidirectional cells the destination host does no editing, yet
mutagen sustained 104–124% of a core there against roughly a third of a
core for autobahn.

The receiving endpoint does not do less work than the sender. Per `Scan`
RPC (`remote/server.go:297`) it runs a **full** scan — the preceding
`Transition` cleared acceleration (`endpoint.go:1395`), so the accelerated
branch is unavailable — then `proto.Marshal` with `Deterministic: true`,
which additionally **sorts the keys of every one of the ~40,000 content
maps**, then `DeltifyBytes` over the resulting 25.6 MB.

Concurrently and independently, the agent's own poll goroutine runs every
interval regardless of any RPC: full scan, full `Equal`, full `Diff`,
inotify re-establishment.

Writing the one small file that actually changed is a rounding error
beside re-scanning, re-serializing, and re-deltifying the entire tree.

---

## 5. What this does not say

**Mutagen is not slow everywhere.** On a 4,000-file tree with a single
writer it beat autobahn's median in the benchmark (61 ms vs 101 ms at the
time), because at that size its pipeline is sub-millisecond and its
debounce windows are shorter. Autobahn has since reduced its own settle
delay, but the point stands: none of the costs here matter until the tree
is large or the churn is high.

**The design has upsides.** A protobuf tree serializes without a
conversion step, which is exactly what a network-transparent design wants.
Scan-based change detection is robust in ways event-based detection is
not — it cannot miss a change because a watch was evicted or a queue
overflowed, which is precisely the failure mode autobahn has to guard
against with a periodic full scan and an `incomplete` flag. Mutagen
converges correctly under conditions where an event-driven tool must fall
back.

**And the platform matters.** On macOS, and in the Linux sidecar image,
recursive watching *is* available, `recheckPaths` is populated, and
accelerated scanning is genuinely incremental. Much of §2 would read
differently there. The benchmark measured Linux hosts running an official
build, which is the configuration a Linux user actually gets.

---

## 6. Caveats

- The residency model in §1.3 is constructed from the list of resident
  fields and measured by allocating equivalents, **not** from a live heap
  profile of mutagen synchronizing a real Chromium tree. It agrees with
  the measurement to 10–15%, which is good evidence but not proof.
  `GODEBUG=gctrace=1` plus `runtime/pprof` would settle it directly.
- Stage timings in §2.4 were measured on different hardware from the
  benchmark. Relative magnitudes and O(n) scaling hold; absolute
  milliseconds do not transfer.
- The synthetic tree used for measurement assumes 40,000 directories and
  representative name lengths. Per-entry byte figures scale with those
  assumptions.
- §2.5 names two candidate mechanisms for the 7–9 second median and does
  not determine which dominates.
- Upstream has in-flight work targeting several of these behaviors —
  a compact tree representation, replacing the path-keyed digest cache
  with node metadata, inline cache modification times, and retaining the
  last snapshot instead of its serialized form. This document describes
  the released implementation the benchmark measured, and those changes
  would invalidate parts of it.
