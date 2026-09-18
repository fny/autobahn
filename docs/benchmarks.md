# Benchmark: autobahn vs mutagen

> **These numbers describe autobahn 0.3.0.** The current version is
> 0.4.0, and three of its changes move measured quantities: two make it
> slightly slower, and one removes the cause of its multi-second
> outliers. None alters a conclusion here, but the specific figures
> below are no longer what the current binary produces — see
> [Currency](#currency-what-changed-since-these-numbers) at the end.

autobahn 0.3.0 against mutagen 0.19.0-dev, on matched pairs of `c6i.4xlarge` instances in one AWS availability zone. Four corpus sizes, three concurrency levels, ten repeats of each of fifteen cells: **150 jobs, 300 tool-runs, ~440,000 latency samples.**

The full per-cell tables are in [the benchmark matrix](./benchmark-matrix.md). The method, and the defects it was designed to prevent, are in [bench/README.md](../bench/README.md). An implementation explanation of the gaps is in [Why mutagen is slower](./mutagen.md).

## Headline

| | autobahn | mutagen | |
|---|---|---|---|
| Propagate one edit, Chromium, 1 agent | **188 ms** | 7,118 ms | 37.8× |
| Propagate one edit, 40k files, 10 agents | **52 ms** | 1,854 ms | 35.4× |
| Peak memory, Chromium | **249 MB** | 2,033 MB | 8.2× |
| CPU while idle, Chromium | **0.2%** | 50% | 250× |
| First sync, Chromium | 423 s | 418 s | ~1% |

**autobahn is faster in every one of the fifteen cells**, by between 1.5× and 37.8×.

## What was measured

The time from a file being written on one machine to exactly those bytes being readable on the other, confirmed by digest. Both timestamps come from one clock on the writing host, so no clock difference between machines can enter a sample. Each sample includes the confirming read and the reply's network trip, so every number is an upper bound on the tool's own time. The harness measures its own overhead separately: **0.6 ms median across all 150 jobs.**

While that agent measures, other agents edit their own disjoint file sets at a coding cadence. The agent count varies the load and nothing else — the measured agent always edits the same fixed 40 files.

## Latency

Median propagation, milliseconds, ten repeats of each cell:

| corpus | 1 agent | 10 agents | 100 agents |
|---|---|---|---|
| **4k files** | 41 / 62 | 40 / 1,037 | 78 / 2,690 |
| **40k files** | 52 / 286 | 52 / 1,854 | 128 / 1,602 |
| **2 × 40k files** | 56 / 329 | 59 / 1,924 | 170 / 1,771 |
| **Chromium (505k)** | 188 / 7,118 | 356 / 8,521 | 782 / 7,664 |

*autobahn / mutagen. Bidirectional cells are in the matrix document.*

One pattern runs through every row. **autobahn's latency barely moves with tree size or agent count. mutagen's moves with both.** On the 4k corpus mutagen goes from 62 ms to 2,690 ms as agents go from 1 to 100, on a tree that never grew. autobahn goes from 41 ms to 78 ms.

The reason is structural and is set out in [Why mutagen is slower](./mutagen.md): no official mutagen build has recursive watching on Linux, so every change event triggers a full scan of the whole tree. autobahn scans only what changed.

## Memory and CPU

Peak resident memory during the workload, and CPU as a percentage of one core:

| corpus | autobahn | mutagen | ratio |
|---|---|---|---|
| 4k files | 16 MB | 48 MB | 3.1× |
| 40k files | 33 MB | 171 MB | 5.2× |
| 2 × 40k files | 71 MB | 330 MB | 4.7× |
| Chromium, 1 agent | 249 MB | 2,033 MB | 8.2× |
| Chromium, 100 agents | 321 MB | 1,903 MB | 5.9× |

The ratio grows with file count, which is the signature of a per-file cost rather than fixed overhead. The marginal cost of one more file is approximately 4,022 bytes for mutagen and 507 bytes for autobahn.

Two CPU results are worth separating out.

**Idle.** With the tree synchronized and nothing happening, autobahn used **0.2%** of a core on Chromium. mutagen used **50%** — half a core, permanently, to poll a tree that is not changing.

**The destination host.** In the one-direction cells the destination does no editing. mutagen still used 114–125% of a core there, against 16–35% for autobahn. The receiving side rescans and reserializes the whole tree on every cycle.

## First sync is a storage question

| corpus | autobahn | mutagen |
|---|---|---|
| Chromium (505k files) | 423 s | 418 s |
| 40k files | 34.0 s | 31.8 s |
| 4k files | 8.4 s | 5.8 s |

The two tools land within about one percent of each other on Chromium. Transferring half a million small files is bound by device IOPS, not by either tool. mutagen is modestly ahead on the smaller trees.

**First sync is not where these tools differ.** The difference is in what it costs to stay caught up afterward.

These numbers were taken with the corpus faulted fully into the volume and the page cache dropped before each run. Without that control the measurement reports storage behavior rather than tool behavior — see [bench/README.md](../bench/README.md#lessons-paid-for).

## Where autobahn is weakest

Two results go against it, and both are reported here rather than left in the matrix.

**Heavy bidirectional load.** At `chromium-100-bidir` — 200 agents editing 505,000 files in both directions at once — autobahn keeps the better median (5,413 ms against 11,600 ms) but its 90th percentile is **worse**: 16,451 ms against mutagen's 15,831 ms. This is not the engine degrading under load. It is the session-restart defect described next, which fired 30 times across this cell's ten runs. This cell also produced most of the run's skipped ticks, so the offered edit rate fell slightly short for both tools.

**Occasional multi-second outliers, from a self-inflicted session restart.** In several cells autobahn's 99th percentile is far above its 90th. The cause is now established, and it is the same cause as the bidirectional result above.

Under sustained churn, a file can be rewritten between the moment autobahn stages it and the moment it applies it. The cycle then reports missing staged content and retries. `MAXIMUM_FOLLOW_UP_CYCLES` bounds that retry at six cycles and then **fails the whole attempt**, even when every one of those cycles successfully applied dozens of other changes. The supervisor treats the failure as a dead session: it drops the session, waits out a retry interval, reconnects, and rescans. Nothing propagates for several seconds, the open-loop workload queues, and every queued edit completes at once — which is why the outliers form an arithmetic ramp rather than a spread.

The resource traces show it directly. Both hosts fall to zero CPU together, resident memory drops on both sides as the session's trees are freed, then CPU spikes as the replacement session rescans. Counting those events:

| cell | runs with a restart | restarts | worst latency |
|---|---|---|---|
| `chromium-100-bidir` | 9 of 10 | 30 | 40.5 s |
| `4k-100` | 6 of 10 | 8 | 6.2 s |
| `chromium-1` | 1 of 10 | 2 | 30.4 s |
| `chromium-10-bidir` | 1 of 10 | 1 | 30.9 s |

Every cell with a multi-second outlier has restarts. Every cell without restarts has a clean tail. It reproduces locally in one run with 100 agents against a 4,000-file corpus over SSH, and autobahn names it in its own log: `staged content was still missing after 6 cycles; source content is changing faster than it can be transferred`.

It needs the network path. The identical workload local-to-local never triggers it, because local staging is a copy that wins the race against the writers.

**This is a defect, not a limit.** The retry cap counts cycles rather than lack of progress, so a busy tree exhausts it while synchronizing perfectly well. Restarting is also the worst available response: it costs a reconnection and a full rescan, during which nothing moves at all. Failing only when a cycle applies *no* transitions would distinguish a genuinely stuck session from a merely busy one.

**Fixed since.** That is now what happens (`db09681`). When the follow-up cycles run out, the supervisor keeps the work they did and carries on; the session stays up and the watcher paces the next attempt. It fails only when the *same content at the same digest* is missing on two cycles running, which means staging is not producing it at all rather than racing the writers. The binary measured here predates the fix — the restarts counted above are the defect firing — and it has not been re-measured, so the tail figures in this document still stand until a re-run replaces them.

## Confidence

- **150 of 150 jobs completed.** One run was excluded: a destination host became network-unreachable mid-job, the cleanliness check could not verify the remote state, and the run was recorded as failed rather than reported.
- **Zero censored samples.** No edit anywhere exceeded the 120-second deadline.
- **Per-run medians cluster within a few percent** across ten different machine pairs, so the results reflect the tools rather than the hardware.
- **Both tools ran in every job**, back to back on the same pair, in an order randomized per job, so machine identity and ordering cancel out.
- mutagen ran with a 5-second poll interval, which is better than its 10-second default.

Raw JSONL for every job is in `bench/results-bench-1787811723/`.

## Currency: what changed since these numbers

**The session restart behind the multi-second outliers is gone** — see [Where autobahn is weakest](#where-autobahn-is-weakest). That should improve the 99th-percentile column; it is not yet measured.

The measurements above were taken at 0.3.0. Since then the correctness work described in [`correctness/`](./correctness/) landed, and every change touching a hot path was A/B measured on a 63,000-file local corpus before it shipped. Most measured flat. Two did not, and both make autobahn slower:

**Steady-state latency: about +6 ms of p50, remote sessions only.** An intent record is now written to the ancestor journal before the first transition of a mutating cycle, and it is synced to stable storage whenever a *remote* endpoint takes part — without that ordering, a power loss on the controller can drop the record while the peer's machine keeps the write it announced, and a revert made while the tool was down is then silently overwritten. The A/B that priced it (Linux, EBS, `fdatasync` ≈ 2.9 ms idle):

| Build | p50 across runs |
|---|---|
| before the change | 52.1, 53.4, 51.3, 53.2 ms |
| syncing every cycle | 59.5, 58.1 ms |
| syncing only for remote sessions (shipped) | 50.9, 52.8, 52.4 ms |

Every cell in this benchmark is a remote pairing, so the sync applies throughout it. The headline 52 ms for 40k files at 10 agents should be read as roughly **58 ms** until a re-run says otherwise — about 32× mutagen's 1,854 ms rather than 35×.

**Cold sync: about +1.2 s per 63,000 files, roughly 16%.** Content published on its last use is now re-hashed immediately before the rename that puts it in place, so a staged file altered between its receive verification and its publication cannot enter the tree under a digest it no longer matches. Measured across five interleaved pairs on the A/B corpus, median cold sync moved from **7.6 s to 8.8 s**. Proportionally that would be a couple of seconds on this document's 423 s Chromium cold sync — small, but not zero.

**Everything else held.** The same five pairs, re-run against current HEAD after all the correctness work landed, show no steady-state drift at all: median p50 **52.9 ms before, 53.5 ms after**, with the per-run spread (52.5–55.2 against 52.0–53.6) larger than the difference between them. Intent records, the doubled write announcement, generation gating, and the mass-disappearance guard cost nothing measurable on the local path.

**What this section is not.** These deltas come from a different corpus (63k files, not 40k or 505k), a different topology (one machine, not a pair), and a different workload than the benchmark. They are an honest adjustment, not a substitute measurement. Only a re-run on matched instance pairs produces citable 0.4.0 figures; the raw A/B reports behind the table above are kept alongside the benchmark aggregates.

## See also

- [The benchmark matrix](./benchmark-matrix.md) — every cell, every percentile
- [Why mutagen is slower](./mutagen.md) — where the gaps come from, in mutagen's code
- [How autobahn works](./how-it-works.md) — the design these numbers come from
- [bench/README.md](../bench/README.md) — the method, and how to run it yourself
