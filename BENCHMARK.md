# Benchmark: autobahn vs mutagen

autobahn 0.3.0 against mutagen 0.19.0-dev, on matched pairs of
`c6i.4xlarge` instances in one AWS availability zone. Four corpus sizes,
three concurrency levels, ten repeats of each of fifteen cells: **150 jobs,
300 tool-runs, ~440,000 latency samples.**

The full per-cell tables are in [BENCHMARK-MATRIX.md](BENCHMARK-MATRIX.md).
The method, and the defects it was designed to prevent, are in
[bench/README.md](bench/README.md). An implementation explanation of the
gaps is in [docs/MUTAGEN.md](docs/MUTAGEN.md).

## Headline

| | autobahn | mutagen | |
|---|---|---|---|
| Propagate one edit, Chromium, 1 agent | **188 ms** | 7,118 ms | 37.8× |
| Propagate one edit, 40k files, 10 agents | **52 ms** | 1,854 ms | 35.4× |
| Peak memory, Chromium | **249 MB** | 2,033 MB | 8.2× |
| CPU while idle, Chromium | **0.2%** | 50% | 250× |
| First sync, Chromium | 423 s | 418 s | ~1% |

**autobahn is faster in every one of the fifteen cells**, by between 1.5×
and 37.8×.

## What was measured

The time from a file being written on one machine to exactly those bytes
being readable on the other, confirmed by digest. Both timestamps come from
one clock on the writing host, so no clock difference between machines can
enter a sample. Each sample includes the confirming read and the reply's
network trip, so every number is an upper bound on the tool's own time. The
harness measures its own overhead separately: **0.6 ms median across all
150 jobs.**

While that agent measures, other agents edit their own disjoint file sets at
a coding cadence. The agent count varies the load and nothing else — the
measured agent always edits the same fixed 40 files.

## Latency

Median propagation, milliseconds, ten repeats of each cell:

| corpus | 1 agent | 10 agents | 100 agents |
|---|---|---|---|
| **4k files** | 41 / 62 | 40 / 1,037 | 78 / 2,690 |
| **40k files** | 52 / 286 | 52 / 1,854 | 128 / 1,602 |
| **2 × 40k files** | 56 / 329 | 59 / 1,924 | 170 / 1,771 |
| **Chromium (505k)** | 188 / 7,118 | 356 / 8,521 | 782 / 7,664 |

*autobahn / mutagen. Bidirectional cells are in the matrix document.*

One pattern runs through every row. **autobahn's latency barely moves with
tree size or agent count. mutagen's moves with both.** On the 4k corpus
mutagen goes from 62 ms to 2,690 ms as agents go from 1 to 100, on a tree
that never grew. autobahn goes from 41 ms to 78 ms.

The reason is structural and is set out in [docs/MUTAGEN.md](docs/MUTAGEN.md):
no official mutagen build has recursive watching on Linux, so every change
event triggers a full scan of the whole tree. autobahn scans only what
changed.

## Memory and CPU

Peak resident memory during the workload, and CPU as a percentage of one
core:

| corpus | autobahn | mutagen | ratio |
|---|---|---|---|
| 4k files | 16 MB | 48 MB | 3.1× |
| 40k files | 33 MB | 171 MB | 5.2× |
| 2 × 40k files | 71 MB | 330 MB | 4.7× |
| Chromium, 1 agent | 249 MB | 2,033 MB | 8.2× |
| Chromium, 100 agents | 321 MB | 1,903 MB | 5.9× |

The ratio grows with file count, which is the signature of a per-file cost
rather than fixed overhead. The marginal cost of one more file is
approximately 4,022 bytes for mutagen and 507 bytes for autobahn.

Two CPU results are worth separating out.

**Idle.** With the tree synchronized and nothing happening, autobahn used
**0.2%** of a core on Chromium. mutagen used **50%** — half a core,
permanently, to poll a tree that is not changing.

**The destination host.** In the one-direction cells the destination does no
editing. mutagen still used 117–125% of a core there, against 16–35% for
autobahn. The receiving side rescans and reserializes the whole tree on
every cycle.

## First sync is a storage question

| corpus | autobahn | mutagen |
|---|---|---|
| Chromium (505k files) | 423 s | 418 s |
| 40k files | 34.0 s | 31.8 s |
| 4k files | 8.4 s | 5.8 s |

The two tools land within about one percent of each other on Chromium.
Transferring half a million small files is bound by device IOPS, not by
either tool. mutagen is modestly ahead on the smaller trees.

**First sync is not where these tools differ.** The difference is in what it
costs to stay caught up afterward.

These numbers were taken with the corpus faulted fully into the volume and
the page cache dropped before each run. Without that control the measurement
reports storage behavior rather than tool behavior — see
[bench/README.md](bench/README.md#lessons-paid-for).

## Where autobahn is weakest

Two results go against it, and both are reported here rather than left in
the matrix.

**Heavy bidirectional load.** At `chromium-100-bidir` — 200 agents editing
505,000 files in both directions at once — autobahn keeps the better median
(5,413 ms against 11,600 ms) but its 90th percentile is **worse**: 16,451 ms
against mutagen's 15,831 ms. Its tail discipline holds everywhere else and
breaks here. This cell also produced most of the run's skipped ticks, so the
offered edit rate fell slightly short for both tools.

**Occasional multi-second outliers.** In several cells autobahn's 99th
percentile is far above its 90th. On `chromium-1` the 90th percentile is
286 ms and the 99th is 16,188 ms. The per-run medians are tight — 182.7 ms
to 202.0 ms across ten machines — so this is systematic, not one bad run.
About one edit in a hundred stalls for seconds.

The cause is not yet established. The leading candidate on large trees is
the 120-second periodic full scan, which exists to bound how long a missed
filesystem event can persist and which takes seconds on a 505,000-file tree.
That does not explain the 4,204 ms 99th percentile on the 4,000-file corpus
at 100 agents, so there is probably a second mechanism. This is open work.

## Confidence

- **150 of 150 jobs completed.** One run was excluded: a destination host
  became network-unreachable mid-job, the cleanliness check could not verify
  the remote state, and the run was recorded as failed rather than reported.
- **Zero censored samples.** No edit anywhere exceeded the 120-second
  deadline.
- **Per-run medians cluster within a few percent** across ten different
  machine pairs, so the results reflect the tools rather than the hardware.
- **Both tools ran in every job**, back to back on the same pair, in an
  order randomized per job, so machine identity and ordering cancel out.
- mutagen ran with a 5-second poll interval, which is better than its
  10-second default.

Raw JSONL for every job is in `bench/results-bench-1787811723/`.
