# Benchmark matrix: autobahn 0.3.0 vs mutagen, under agent workloads

Six workloads across four corpus sizes, between two `t3.xlarge` instances,
measuring how each tool behaves when coding agents are actively editing the
tree it is synchronizing.

> **This report was revised after an independent methodology audit found
> real flaws in the first version.** One published result was backwards
> (mutagen appeared to get *faster* under 10× the load), which turned out
> to be partly a genuine property of its watch mode and partly an artifact
> of the harness. Several other numbers were mis-captioned, and the
> bidirectional table has been withdrawn pending re-measurement. What each
> number is worth is stated inline, and §"What is not trustworthy here"
> lists everything withdrawn. Raw measurements are in `benchmarks/`.

**Headline, stated carefully:** autobahn was faster on every workload
measured and used less memory in every configuration. The *size* of its
latency advantage over mutagen depends heavily on edit locality — from
about **6× to about 26×** — because mutagen's default watch mode has two
very different latency classes and the harness did not control which one
it exercised. Autobahn's own latency is locality-independent, and its idle
CPU advantage on large trees (well under 1% of a core against 90–123%) is
not in question.

## Setup

| | |
|---|---|
| Hosts | 2 × EC2 `t3.xlarge` (4 vCPU, 16GB), us-east-2, same subnet, 200GB gp3 (6000 IOPS / 500 MB/s), **unlimited CPU credits** so no burst throttling |
| OS | Ubuntu 24.04 |
| autobahn | 0.3.0 (`17eb722`), release build, static musl |
| mutagen | 0.19.0-dev, release build |
| Mode | `two-way-safe` on both, 5s fallback interval, `.git` and `out` ignored |
| Corpora | Chromium `src` (depth-1 clone) and three subsets built from whole directories of it |

| Corpus | Files | Directories | Size |
|---|---|---|---|
| `chromium` | 504,436 | 46,127 | 5.5GB |
| `sub40k-a` | 41,486 | 4,164 | 472MB |
| `sub40k-b` | 38,236 | 3,057 | 384MB |
| `sub4k` | 5,689 | 339 | 94MB |

**The agent workload.** Each simulated agent rewrites a file every 0.3–1.2s
(atomic replace, 2–64KB random content) from its own partition of a
candidate list, at a coding cadence rather than a build's. One agent per
side additionally times each of its own edits end to end.

The candidate list is **the first 4,000 files in `os.walk` order** (or
`80 × agents`, whichever is larger), then sorted. On Chromium that is 0.8%
of the tree and contiguous, not a uniform sample — a limitation that
matters for interpreting the results, and one the first version of this
report described incorrectly.

**Latency methodology.** The writer announces the content it is about to
produce to an observer on the far side, writes it, and waits for the
observer to acknowledge that exactly those bytes are readable there. Both
timestamps come from one clock on the writing host, so no cross-host skew
enters the result, and the interval *includes* the observer's verification
read and the acknowledgement's return trip. Latencies are therefore
conservative upper bounds — but the harness's own floor (a fixed 50ms
pause before writing, plus the observer's poll interval and the
acknowledgement trip) has **not** been characterized, and could be a
material fraction of autobahn's smallest figures. Treat sub-100ms numbers
as "at or near the harness floor" rather than as engine measurements.

**Every cell is a single run.** Where a configuration happens to have been
run twice, the two results differ by 12%–120%. Read one-off differences
under about 2× as noise.

## Cold synchronization

Measured once per corpus per tool, from process start until the
destination independently reaches the expected file count (5s poll
granularity). Chromium was run twice per tool; the others once.

| Corpus | autobahn | mutagen |
|---|---|---|
| chromium (504k files) | 109.0s, 123.4s | 150.6s, 158.8s |
| sub40k-a (41k) | 10.9s | 11.4s |
| both 40k corpora, two sessions (80k) | 11.8s | 12.8s |
| sub4k (5.7k) | 5.4s | 5.7s |

Only the Chromium result is outside the noise: autobahn is roughly 1.3×
faster there. At 41k files and below the two tools are within a second of
each other, which given single runs and 5s poll granularity is **not a
difference this experiment can resolve.**

## Edit propagation under agent workloads

Verified round trips, milliseconds. One side editing.

| Workload | Tool | n | p50 | p90 | p99 | max |
|---|---|---|---|---|---|---|
| **4k tree, 10 agents** | autobahn | 142 | 73.9 | 115.3 | 148.0 | 214.8 |
| | mutagen | 43 | 1,915.5 | 4,292.4 | 4,630.0 | 4,768.9 |
| **4k tree, 100 agents** | autobahn | 137 | 122.5 | 210.1 | 245.0 | 251.7 |
| | mutagen | 71 | 744.8 | 1,703.6 | 4,556.6 | 4,608.5 |
| **40k tree, 10 agents** | autobahn | 126 | 133.2 | 291.2 | 406.1 | 433.6 |
| | mutagen | 38 | 2,183.6 | 4,039.2 | 4,404.0 | 4,561.5 |
| **Two 40k trees, 10 agents each** | autobahn | 125 / 126 | 145.1 / 135.6 | 311.3 / 246.6 | 441.9 / 391.9 | 565.6 / 511.5 |
| | mutagen | 37 / 39 | 1,972.8 / 1,652.2 | 4,128.0 / 4,285.3 | 4,776.6 / 4,481.8 | 5,576.6 / 4,554.8 |
| **chromium, 10 agents** | autobahn | 65 | 695.7 | 3,722.1 | 6,989.3 | 16,444.5 |
| | mutagen | 8 | 18,241.3 | 19,933.4 | 19,933.4 | 20,966.6 |

Sample counts differ by construction, not by sampling: each measuring
agent edits on a fixed cadence and waits for verification, so a slower
tool completes fewer round trips in the same window. The relationship
`n ≈ window / (0.75s + latency)` reproduces every row, and no measurement
failed in any run.

### Why mutagen appears faster under more load

The 4k rows are backwards — mutagen's p50 improves from 1,916ms to 745ms
when the agent count goes from 10 to 100 — and understanding why
undermines the simple headline.

Mutagen's default portable watch mode has **two latency classes**: paths
it has recently seen modified are watched natively and detected at once;
everything else waits for the next 5s poll. The harness ties each agent's
working set to the agent count (`files[i::count]`), so raising the agent
count also *shrinks and heats* each agent's partition:

- **At 10 agents** the measuring agent cycles over ~569 files, revisiting
  any given one roughly every 2.5 minutes. Every measured edit is
  effectively a cold path. The result — p50 1,916ms with p90 4,292ms — is
  the signature of a 5s poll (mean ≈ half the interval, tail ≈ the
  interval), not of scanning cost.
- **At 100 agents** partitions are ~29 files and the whole system sees
  ~130 edits/s, so the native watch set stays saturated and cycles fire
  continuously; even a cold edit is swept up by a cycle another agent
  triggered. p50 745ms is roughly one cycle. The p99 of 4,557ms is the
  poll still showing through.

So these two rows measure **detection latency under different locality**,
not a scaling property. The consequence for the cross-tool comparison: a
fixed-agent-count row is internally fair — both tools faced identical
edits — but the *uniform-random cold* edit pattern is mutagen's worst case
for detection. The 16× and 26× figures are the cold end of a
locality-dependent range. **The fairest single engine-versus-engine number
here is the 4k/100-agent row, at about 6×** (123ms vs 745ms), where
detection latency is largely removed from both sides.

Autobahn shows none of this because it watches natively throughout: its
latency tracks the size of the change, not how recently the path was
touched.

## Memory

**These are process-lifetime peaks, not per-workload peaks.** The sampler
reports the maximum since the process started, so a workload row inherits
the cold sync's peak whenever that was higher — provably so for autobahn's
agent, whose figure (313,108kB) is byte-identical across the cold sync and
both Chromium workloads. The comparison between tools is sound; the
attribution to a phase is not.

| Corpus | | autobahn | mutagen |
|---|---|---|---|
| chromium (peak over cold sync + workloads) | controller | 446MB | 2,111MB |
| | agent | 306MB | 1,478MB |
| two 40k sessions | controller | 126MB | 321MB |
| | agent | 59MB | 245MB |
| 40k, 10 agents | controller | 70MB | 170MB |
| | agent | 47MB | 128MB |
| 4k, 100 agents | controller | 18MB | 53MB |
| | agent | 16MB | 33MB |

Mutagen's Chromium controller peak also varied between runs from 1.67GB to
3.74GB; the figure above is the smaller of the two comparable runs.

## Idle CPU

Both tools watching a synchronized tree with nothing happening, as a
percentage of one core. Where a configuration was measured twice, both
figures are given.

| Corpus | | autobahn | mutagen |
|---|---|---|---|
| chromium (504k) | controller | <1%, 8% | 90%, 123% |
| | agent | <1%, <1% | 96%, 103% |
| two 40k sessions | controller | <1% | 14% |
| | agent | <1% | 6% |
| sub40k-a | controller | <1% | 7% |
| | agent | <1% | 6% |
| sub4k | controller | <1% | <1% |

The first version of this report claimed autobahn measured zero "in every
configuration" and quoted mutagen at 90%/96%; both were selective. One
autobahn run measured 8%, and one mutagen run measured over a full core on
each host. The likely cause of the spread is that idle was sampled shortly
after a file-count completion check, which can fire before content has
actually settled — so these are "shortly after convergence" figures, not
guaranteed-quiescent ones.

The direction is nonetheless unambiguous and large: on a half-million-file
tree, mutagen's polling-assisted watching costs on the order of a full
core on *each* machine to establish that nothing changed, while autobahn's
costs almost nothing. An unchanged rescan there is adopted from the
previous scan's storage, reported to the controller as a two-byte marker
rather than a snapshot, and skipped before reconciliation.

## What is not trustworthy here

Listed so that nobody builds on it:

1. **The bidirectional Chromium table is withdrawn.** It rested on a
   single run with 6 measured samples per direction, and a second run of
   the same configuration disagreed by roughly 2× *in both directions and
   in opposite senses* (autobahn 7,881ms vs 3,601ms; mutagen 15,595ms vs
   25,177ms). The earlier run also used overlapping edit partitions, which
   is a different experiment — but the first version of this report
   described it as having produced "no propagation at all", which is
   false: its A→B direction completed a full measurement window. That run
   should have been reported, not silently dropped.
2. **Disjointness in the bidirectional workload was never verified.** Each
   host computed its own candidate list by early-stopping an `os.walk`,
   and two hosts can enumerate a tree in different orders — so "A takes
   evens, B takes odds" does not guarantee disjoint sets. Any overlap
   becomes a conflict, which `two-way-safe` correctly refuses to resolve,
   and would be recorded as latency.
3. **The mutagen 4k/100-agent row must not be read as a scaling result.**
   It is a locality result, for the reasons above.
4. **Per-phase memory attribution** — see the note on the memory table.
5. **Single runs throughout.** Every repeat that exists shows 12%–120%
   spread.

## What still stands

- Autobahn's one-sided latency figures, which are locality-independent and
  have 65–142 samples each.
- Autobahn's idle CPU on small corpora.
- The Chromium cold-sync gap (~1.3×).
- Two concurrent 40k sessions costing the same per-session latency as one
  (145/136ms against 133ms) for 126MB total against 70MB.
- The direction and rough magnitude of every memory comparison.
- Autobahn's weakest result, which is not flattering and is worth keeping:
  under 10 agents on Chromium its p50 is 696ms but p90 is 3.7s and the
  maximum 16.4s. Cycles are not paced, so under sustained churn on a large
  tree they run back to back and an individual edit can wait through
  several. That is a scheduling problem, and it is the next thing to fix.

## The corrected experiment

To clear the withdrawn items, roughly two machine-hours:

- **Fixed working sets.** Give the measuring agent the same 40-file
  partition at every agent count, so agent count varies load *only*. Turns
  the anomaly above into a designed locality axis rather than a confound.
- **Verified disjointness.** Compute the candidate list once on one host
  from a seeded sample of a *complete* walk, ship it to the other, and
  assert the two sides' sets do not intersect before measuring.
- **A characterized floor.** Measure the harness end to end with no
  synchronization tool in the loop, so sub-100ms figures can be reported
  net of it.
- **Three runs per cell**, fresh sessions between workloads, windowed
  rather than lifetime memory sampling, and content-verified rather than
  count-verified convergence before sampling idle.
