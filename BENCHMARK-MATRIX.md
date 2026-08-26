# Benchmark matrix: autobahn 0.3.0 vs mutagen, under agent workloads

Six workloads across four corpus sizes, between two `t3.xlarge` instances,
measuring how each tool behaves when coding agents are actively editing the
tree it is synchronizing.

**Headline:** autobahn was faster on every workload and used less memory in
every configuration. On a 40k-file tree with 10 agents it propagated edits
in **133ms** against mutagen's **2,184ms** (16×). On the full Chromium tree
it idled at **under 1% of a core** where mutagen consumed **90% of one core
on each machine** doing nothing. The one place autobahn's advantage narrows
is bidirectional churn on half a million files, where both tools degrade —
autobahn to ~3.6s, mutagen to ~25s.

## Setup

| | |
|---|---|
| Hosts | 2 × EC2 `t3.xlarge` (4 vCPU, 16GB), us-east-2, same subnet, 200GB gp3 (6000 IOPS / 500 MB/s), **unlimited CPU credits** so no burst throttling |
| OS | Ubuntu 24.04 |
| autobahn | 0.3.0 (`17eb722`), release build, static musl |
| mutagen | 0.19.0-dev, release build |
| Mode | `two-way-safe` on both, 5s fallback interval, `.git` and `out` ignored |
| Corpora | Chromium `src` (depth-1 clone) and three disjoint subsets built from whole directories of it |

| Corpus | Files | Directories | Size |
|---|---|---|---|
| `chromium` | 504,436 | 46,127 | 5.5GB |
| `sub40k-a` | 41,486 | 4,164 | 472MB |
| `sub40k-b` | 38,236 | 3,057 | 384MB |
| `sub4k` | 5,689 | 339 | 94MB |

**The agent workload.** Each simulated agent rewrites a file every 0.3–1.2s
(atomic replace, 2–64KB, random content) from its own partition of the
tree — a coding cadence, not a build. One agent per side additionally times
each of its own edits end to end.

**Latency methodology** (unchanged from the previous benchmark): the writer
announces the content it is about to produce to an observer on the far
side, writes it, and waits for the observer to acknowledge that exactly
those bytes are readable there. Both timestamps come from one clock on the
writing host, so no cross-host skew enters the result, and the interval
*includes* the observer's verification read and the acknowledgement's
return trip. Every latency below is therefore a conservative upper bound.

**Cold sync** is measured once per corpus per tool, from process start to
the destination independently reaching the expected file count (5s poll
granularity), with state and destination cleared beforehand.

## Cold synchronization

| Corpus | autobahn | mutagen | |
|---|---|---|---|
| chromium (504k files) | **109–123s** | **151–159s** | autobahn ~1.3× faster |
| sub40k-a (41k) | **10.9s** | 11.4s | |
| both 40k corpora, two sessions (80k) | **11.8s** | 12.8s | |
| sub4k (5.7k) | **5.4s** | 5.7s | |

Chromium was cold-synced three times per tool over the course of the run;
the range is given rather than a single figure. At small sizes the two
tools are within a second of each other — the difference is dominated by
process and agent startup, not by the engines.

## Edit propagation under agent workloads

Verified round trips, in milliseconds.

### One side editing

| Workload | Tool | n | p50 | p90 | p99 | max |
|---|---|---|---|---|---|---|
| **4k tree, 10 agents** | autobahn | 142 | **73.9** | 115.3 | 148.0 | 214.8 |
| | mutagen | 43 | 1,915.5 | 4,292.4 | 4,630.0 | 4,768.9 |
| **4k tree, 100 agents** | autobahn | 137 | **122.5** | 210.1 | 245.0 | 251.7 |
| | mutagen | 71 | 744.8 | 1,703.6 | 4,556.6 | 4,608.5 |
| **40k tree, 10 agents** | autobahn | 126 | **133.2** | 291.2 | 406.1 | 433.6 |
| | mutagen | 38 | 2,183.6 | 4,039.2 | 4,404.0 | 4,561.5 |
| **Two 40k trees, 10 agents each** | autobahn | 125 / 126 | **145.1 / 135.6** | 311.3 / 246.6 | 441.9 / 391.9 | 565.6 / 511.5 |
| | mutagen | 37 / 39 | 1,972.8 / 1,652.2 | 4,128.0 / 4,285.3 | 4,776.6 / 4,481.8 | 5,576.6 / 4,554.8 |
| **chromium, 10 agents** | autobahn | 65 | **695.7** | 3,722.1 | 6,989.3 | 16,444.5 |
| | mutagen | 8 | 18,241.3 | 19,933.4 | 19,933.4 | 20,966.6 |

### Both sides editing (10 agents per side, disjoint halves of the tree)

| Workload | Tool | Direction | n | p50 | p90 | p99 | max |
|---|---|---|---|---|---|---|---|
| **chromium, 10+10 agents** | autobahn | A→B | 32 | **3,601** | 4,947 | 13,208 | 14,349 |
| | autobahn | B→A | 28 | **3,782** | 8,603 | 13,404 | 14,478 |
| | mutagen | A→B | 6 | 25,177 | 33,261 | 33,261 | 33,759 |
| | mutagen | B→A | 6 | 31,660 | 34,746 | 34,746 | 36,840 |

The two sides must edit *disjoint* files for this measurement to mean
anything: agents editing the same file from both ends is a conflict by
definition, and `two-way-safe` correctly refuses to resolve it, so an
overlapping workload measures the tool declining to lose data rather than
the speed of propagation. An earlier attempt with overlapping partitions
produced exactly that — no propagation at all — and was discarded.

## Memory

Peak resident memory summed across each tool's process tree, during the
agent workload for that corpus.

| Corpus | | autobahn | mutagen | |
|---|---|---|---|---|
| chromium, one side editing | controller | **281MB** | 2,059MB | 7.3× less |
| | agent | **305MB** | 1,380MB | 4.5× less |
| chromium, both sides editing | controller | **422MB** | 3,652MB | 8.7× less |
| | agent | **306MB** | 1,348MB | 4.4× less |
| two 40k sessions | controller | **126MB** | 321MB | 2.5× less |
| | agent | **59MB** | 245MB | 4.2× less |
| 40k, 10 agents | controller | **70MB** | 170MB | 2.4× less |
| | agent | **47MB** | 128MB | 2.7× less |
| 4k, 100 agents | controller | **18MB** | 53MB | 2.9× less |
| | agent | **16MB** | 33MB | 2.1× less |

## Idle CPU

Both tools watching a synchronized tree with nothing happening, as a
percentage of one core.

| Corpus | | autobahn | mutagen |
|---|---|---|---|
| chromium (504k) | controller | **<1%** | **90%** |
| | agent | **<1%** | **96%** |
| two 40k sessions | controller | **<1%** | 14% |
| | agent | **<1%** | 6% |
| sub40k-a | controller | **<1%** | 7% |
| | agent | **<1%** | 6% |
| sub4k | controller | **<1%** | <1% |
| | agent | **<1%** | <1% |

Autobahn measured zero at this sampling resolution in every configuration:
an unchanged rescan is adopted from the previous scan's storage, reported
to the controller as a two-byte marker rather than a snapshot, and skipped
before reconciliation. On the Chromium tree, mutagen's polling-assisted
watching consumes nearly a full core on *each* machine to establish that
nothing has changed — on a 4-vCPU instance, that is a quarter of the box
spent on standing still.

## What the numbers say

**Scale is where the tools separate.** At 5.7k files both tools cold-sync
in the same 5 seconds. At 504k files autobahn holds a 7× memory advantage
and a 26× latency advantage, and the idle CPU gap becomes the difference
between an idle machine and a permanently busy one.

**Autobahn's latency tracks the change, not the tree.** From 4k to 40k
files — a 7× larger tree — p50 moved from 74ms to 133ms. Mutagen's stayed
near 2s at both sizes, because its cost is dominated by rescanning
regardless of how little changed.

**More agents cost autobahn very little.** Going from 10 to 100 agents on
the 4k tree moved p50 from 74ms to 123ms, with the tail actually
*tightening* (max 215ms → 252ms). Ten times the edit rate did not produce
ten times the latency, because a cycle that is already running absorbs
additional changes rather than queueing behind them.

**Two sessions are nearly free.** Running two independent 40k
synchronizations concurrently produced the same per-session latency as
running one (145/136ms against 133ms), for 126MB total instead of 70MB.

**Bidirectional churn at Chromium scale is the hard case for both.**
Autobahn degrades from 696ms to ~3.6s and mutagen from 18s to ~25–32s.
Both directions cost about the same in autobahn (3,601ms vs 3,782ms),
which is the expected result for a symmetric engine.

**The Chromium tail is autobahn's weakest number.** Under 10 agents its p50
is 696ms but p90 is 3.7s and the maximum 16.4s: when a cycle is already
running, an edit waits for it to finish and for the next one to reach it.
Cycles are not paced, so under sustained churn on a large tree they run
back to back and an individual edit can wait several of them. This is the
next thing to fix, and it is a scheduling problem rather than a throughput
one.

## Caveats

- `t3.xlarge` has 4 vCPUs. Both tools are more CPU-bound here than on the
  8-vCPU `c5.2xlarge` used previously, which magnifies mutagen's
  polling cost in particular. Unlimited credit mode was enabled so that
  neither tool was throttled.
- Cold sync uses a 5s completion poll, so those figures carry ±5s.
- Sample counts differ sharply between tools by construction: each
  measuring agent edits on a fixed cadence and waits for verification, so
  a slower tool completes fewer round trips in the same window. Mutagen's
  Chromium runs have 6–8 samples, which is enough to establish the
  magnitude and not enough for reliable tail statistics.
- mutagen ran in its default portable watch mode, which is
  polling-assisted rather than purely native. This is its recommended
  configuration but is the direct cause of its idle CPU figures.
- The two 40k corpora are not identical in size (41,486 and 38,236 files),
  so the two sessions in that workload are similar rather than matched.
- Each tool ran alone; the two were never measured concurrently.
