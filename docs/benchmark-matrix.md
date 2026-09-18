# Benchmark matrix: autobahn 0.3.0 vs mutagen 0.19.0-dev

> **Every figure here is autobahn 0.3.0.** At 0.4.0 the latency cells
> should be read about 6 ms higher (an intent record is synced before
> each mutating cycle of a remote session) and the first-synchronization
> cells slightly higher (published content is re-hashed at the moment of
> publication). The reasoning and the measurements are in
> [the summary](./benchmarks.md#currency-what-changed-since-these-numbers).
> Both changes buy correctness properties, and neither changes a
> conclusion in these tables.

Every cell, every percentile. The summary and the interpretation are in [the benchmark summary](./benchmarks.md). The method is in [bench/README.md](../bench/README.md).

**Setup.** Matched pairs of `c6i.4xlarge` instances, one AWS availability zone, 200 GB gp3 volumes. Chromium at 504,940 files with symbolic links removed and `.git` and `out` excluded. Fifteen cells, ten repeats each: 150 jobs and 300 tool-runs. Both tools run in every job, back to back on the same pair, in an order randomized per job. mutagen used a 5-second poll interval, better than its 10-second default.

**Integrity.** 150 of 150 jobs completed. Zero censored samples. One run excluded, when a destination host became unreachable and the cleanliness check refused to certify it. Harness overhead measured 0.6 ms median across all 150 jobs, against results in the tens to thousands of milliseconds.

## Propagation latency

All figures are milliseconds. `⇄` marks a bidirectional cell, which runs a separate measuring agent in each direction. Ratio is mutagen's median over autobahn's. "Skipped" counts ticks where the measuring agent could not issue an edit, which records an offered-load shortfall.

| corpus | agents | direction | ab p50 | ab p90 | ab p99 | mu p50 | mu p90 | mu p99 | ratio | samples | skipped |
|---|---|---|---|---|---|---|---|---|---|---|---|
| Chromium | 1 | a-to-b | 188 | 286 | 16,188 | 7,118 | 9,369 | 10,710 | **37.8×** | 1,729 | 3 |
| Chromium | 10 | a-to-b | 356 | 525 | 2,061 | 8,521 | 12,079 | 18,622 | **23.9×** | 1,741 | 2 |
| Chromium | 100 | a-to-b | 782 | 2,396 | 4,414 | 7,664 | 11,560 | 25,705 | **9.8×** | 1,732 | 2 |
| Chromium ⇄ | 1 | a-to-b | 835 | 1,272 | 2,567 | 7,226 | 9,967 | 30,807 | **8.7×** | 1,739 | 0 |
| Chromium ⇄ | 1 | b-to-a | 854 | 1,276 | 2,839 | 8,756 | 11,631 | 33,942 | **10.3×** | 1,719 | 1 |
| Chromium ⇄ | 10 | a-to-b | 1,132 | 1,524 | 19,026 | 6,368 | 9,472 | 32,334 | **5.6×** | 1,724 | 31 |
| Chromium ⇄ | 10 | b-to-a | 1,152 | 1,586 | 18,697 | 6,788 | 9,909 | 25,862 | **5.9×** | 1,732 | 24 |
| Chromium ⇄ | 100 | a-to-b | 4,984 | 15,064 | 32,483 | 11,067 | 15,310 | 17,937 | **2.2×** | 1,691 | 59 |
| Chromium ⇄ | 100 | b-to-a | 5,842 | 17,838 | 36,912 | 12,134 | 16,352 | 18,825 | **2.1×** | 1,678 | 75 |
| 40k | 1 | a-to-b | 52 | 135 | 186 | 286 | 1,064 | 2,786 | **5.5×** | 1,733 | 0 |
| 40k | 10 | a-to-b | 52 | 81 | 142 | 1,854 | 4,259 | 5,127 | **35.4×** | 1,569 | 0 |
| 40k | 100 | a-to-b | 128 | 226 | 352 | 1,602 | 3,772 | 5,387 | **12.5×** | 1,743 | 0 |
| 2×40k | 1 | a-to-b | 55 | 136 | 216 | 303 | 1,036 | 2,586 | **5.5×** | 1,724 | 0 |
| 2×40k | 1 | a-to-b | 58 | 139 | 223 | 355 | 1,100 | 2,868 | **6.1×** | 1,743 | 0 |
| 2×40k | 10 | a-to-b | 57 | 89 | 141 | 1,919 | 4,280 | 5,150 | **33.9×** | 1,744 | 0 |
| 2×40k | 10 | a-to-b | 61 | 97 | 151 | 1,928 | 4,345 | 5,350 | **31.6×** | 1,734 | 0 |
| 2×40k | 100 | a-to-b | 158 | 270 | 396 | 1,689 | 3,955 | 5,574 | **10.7×** | 1,726 | 0 |
| 2×40k | 100 | a-to-b | 181 | 304 | 433 | 1,853 | 4,178 | 5,581 | **10.2×** | 1,755 | 0 |
| 4k | 1 | a-to-b | 41 | 118 | 142 | 62 | 800 | 2,390 | **1.5×** | 1,780 | 0 |
| 4k | 10 | a-to-b | 40 | 64 | 104 | 1,037 | 3,114 | 4,821 | **25.7×** | 1,747 | 0 |
| 4k | 100 | a-to-b | 78 | 133 | 4,204 | 2,690 | 4,953 | 5,459 | **34.6×** | 1,737 | 0 |

### Reading the latency table

**autobahn leads every cell**, from 1.5× on the smallest to 37.8× on Chromium with a single writer.

**The two tools scale differently.** autobahn's median moves from 40 ms to 782 ms across the whole matrix, a factor of about 20 spanning a 125× change in tree size and a 100× change in concurrency. mutagen's moves from 62 ms to 11,600 ms, a factor of 187.

**The 99th percentile carries a defect, not a scaling limit.** In several cells autobahn's 99th percentile sits far above its 90th. The cause is a self-inflicted session restart: under churn a file is rewritten between staging and application, the cycle reports missing staged content, and after six such cycles autobahn fails the whole attempt even though each cycle applied dozens of other changes. The supervisor then drops the session, backs off, reconnects and rescans, and nothing propagates meanwhile. Restarts number 30 across `chromium-100-bidir`, 8 across `4k-100`, and 2 in a single `chromium-1` run. Every cell with a multi-second outlier has them, and every cell without them has a clean tail. Details in [the summary](./benchmarks.md#where-autobahn-is-weakest). The defect has since been fixed; the summary says how.

**mutagen's ordering is not monotonic in agent count.** On the 40k corpus it is slower at 10 agents (1,854 ms) than at 100 (1,602 ms). Both are far above its single-agent figure of 286 ms. Under continuous churn its coalescing timers reset rather than expire, so latency tracks writer density in a way that is not simply proportional to it.

## Memory and CPU

Peak resident memory and mean CPU, as a percentage of one core, median across repeats. "Source" is the editing host, "dest" the receiving one. Sampling covers each tool's whole process tree, including its remote agent.

### Workload

| corpus, agents | host | autobahn RSS | mutagen RSS | ratio | autobahn CPU | mutagen CPU |
|---|---|---|---|---|---|---|
| 4k, 1 | source | 16 MB | 48 MB | **3.1×** | 1.1% | 6.1% |
| 4k, 1 | dest | 5 MB | 31 MB | **6.1×** | 0.3% | 4.0% |
| 4k, 10 | source | 17 MB | 48 MB | **2.8×** | 5.8% | 4.1% |
| 4k, 10 | dest | 5 MB | 32 MB | **6.0×** | 2.1% | 3.5% |
| 4k, 100 | source | 19 MB | 49 MB | **2.6×** | 17.7% | 13.2% |
| 4k, 100 | dest | 6 MB | 35 MB | **6.1×** | 10.9% | 8.9% |
| 40k, 1 | source | 33 MB | 171 MB | **5.2×** | 5.4% | 50.1% |
| 40k, 1 | dest | 16 MB | 121 MB | **7.5×** | 1.5% | 36.3% |
| 40k, 10 | source | 39 MB | 168 MB | **4.3×** | 34.6% | 17.1% |
| 40k, 10 | dest | 17 MB | 128 MB | **7.4×** | 7.4% | 15.6% |
| 40k, 100 | source | 41 MB | 180 MB | **4.4×** | 35.2% | 38.1% |
| 40k, 100 | dest | 18 MB | 132 MB | **7.4×** | 22.8% | 40.6% |
| 2×40k, 1 | source | 55 MB | 328 MB | **6.0×** | 13.0% | 115.2% |
| 2×40k, 1 | dest | 30 MB | 251 MB | **8.4×** | 3.2% | 75.7% |
| 2×40k, 10 | source | 65 MB | 303 MB | **4.7×** | 87.7% | 36.5% |
| 2×40k, 10 | dest | 31 MB | 267 MB | **8.7×** | 20.6% | 39.4% |
| 2×40k, 100 | source | 71 MB | 330 MB | **4.7×** | 82.2% | 78.5% |
| 2×40k, 100 | dest | 31 MB | 274 MB | **8.8×** | 48.2% | 80.7% |
| Chromium, 1 | source | 249 MB | 2,033 MB | **8.2×** | 60.2% | 153.1% |
| Chromium, 1 | dest | 173 MB | 1,361 MB | **7.9×** | 16.1% | 117.4% |
| Chromium, 10 | source | 272 MB | 1,964 MB | **7.2×** | 102.9% | 97.2% |
| Chromium, 10 | dest | 173 MB | 1,448 MB | **8.3×** | 31.8% | 113.9% |
| Chromium, 100 | source | 321 MB | 1,903 MB | **5.9×** | 79.4% | 103.1% |
| Chromium, 100 | dest | 180 MB | 1,447 MB | **8.1×** | 35.0% | 124.5% |
| Chromium ⇄, 1 | source | 449 MB | 2,054 MB | **4.6×** | 88.7% | 150.9% |
| Chromium ⇄, 1 | dest | 239 MB | 1,375 MB | **5.8×** | 43.4% | 132.8% |
| Chromium ⇄, 10 | source | 452 MB | 2,083 MB | **4.6×** | 89.2% | 128.0% |
| Chromium ⇄, 10 | dest | 238 MB | 1,378 MB | **5.8×** | 47.0% | 102.9% |
| Chromium ⇄, 100 | source | 449 MB | 1,921 MB | **4.3×** | 60.0% | 116.7% |
| Chromium ⇄, 100 | dest | 239 MB | 1,358 MB | **5.7×** | 44.4% | 87.5% |

### Idle

| corpus, agents | host | autobahn RSS | mutagen RSS | ratio | autobahn CPU | mutagen CPU |
|---|---|---|---|---|---|---|
| 4k, 1 | source | 15 MB | 46 MB | **3.2×** | 0.1% | 0.4% |
| 4k, 1 | dest | 6 MB | 28 MB | **5.1×** | 0.0% | 0.4% |
| 4k, 10 | source | 15 MB | 46 MB | **3.1×** | 0.1% | 0.4% |
| 4k, 10 | dest | 6 MB | 28 MB | **5.1×** | 0.0% | 0.4% |
| 4k, 100 | source | 14 MB | 46 MB | **3.2×** | 0.1% | 0.4% |
| 4k, 100 | dest | 6 MB | 28 MB | **5.1×** | 0.0% | 0.4% |
| 40k, 1 | source | 29 MB | 162 MB | **5.7×** | 0.1% | 3.6% |
| 40k, 1 | dest | 19 MB | 113 MB | **5.8×** | 0.0% | 3.9% |
| 40k, 10 | source | 29 MB | 163 MB | **5.7×** | 0.1% | 3.7% |
| 40k, 10 | dest | 20 MB | 110 MB | **5.6×** | 0.0% | 3.6% |
| 40k, 100 | source | 29 MB | 161 MB | **5.6×** | 0.1% | 3.5% |
| 40k, 100 | dest | 20 MB | 116 MB | **5.9×** | 0.0% | 3.6% |
| 2×40k, 1 | source | 47 MB | 289 MB | **6.2×** | 0.2% | 8.1% |
| 2×40k, 1 | dest | 38 MB | 245 MB | **6.4×** | 0.1% | 7.8% |
| 2×40k, 10 | source | 47 MB | 292 MB | **6.3×** | 0.2% | 7.8% |
| 2×40k, 10 | dest | 38 MB | 240 MB | **6.3×** | 0.1% | 8.1% |
| 2×40k, 100 | source | 47 MB | 292 MB | **6.3×** | 0.2% | 8.1% |
| 2×40k, 100 | dest | 38 MB | 244 MB | **6.4×** | 0.1% | 7.8% |
| Chromium, 1 | source | 193 MB | 1,712 MB | **8.9×** | 0.2% | 49.9% |
| Chromium, 1 | dest | 200 MB | 1,358 MB | **6.8×** | 3.0% | 51.5% |
| Chromium, 10 | source | 192 MB | 1,791 MB | **9.3×** | 0.2% | 50.0% |
| Chromium, 10 | dest | 200 MB | 1,327 MB | **6.6×** | 3.0% | 51.0% |
| Chromium, 100 | source | 194 MB | 1,777 MB | **9.2×** | 0.2% | 50.0% |
| Chromium, 100 | dest | 200 MB | 1,287 MB | **6.4×** | 3.0% | 52.3% |
| Chromium ⇄, 1 | source | 192 MB | 1,803 MB | **9.4×** | 0.2% | 50.9% |
| Chromium ⇄, 1 | dest | 200 MB | 1,331 MB | **6.7×** | 3.0% | 53.0% |
| Chromium ⇄, 10 | source | 194 MB | 1,768 MB | **9.1×** | 0.2% | 50.4% |
| Chromium ⇄, 10 | dest | 200 MB | 1,303 MB | **6.5×** | 3.0% | 52.8% |
| Chromium ⇄, 100 | source | 194 MB | 1,749 MB | **9.0×** | 0.2% | 49.7% |
| Chromium ⇄, 100 | dest | 200 MB | 1,354 MB | **6.8×** | 3.0% | 52.5% |

### Reading the resource tables

**Memory scales with the tree, not with the agent count.** autobahn holds Chromium in 249 MB with one agent and 321 MB with a hundred. The ratio against mutagen grows with file count — 3.1× at 4,000 files, 8.2× at 505,000 — which is the signature of a per-file cost rather than fixed overhead.

**Idle CPU is the starkest single number.** With the tree synchronized and nothing happening, mutagen uses half a core on Chromium. autobahn uses two tenths of one percent.

**The receiving host is not idle for mutagen.** In one-direction cells it does no editing, yet mutagen sustains more than a core there. It rescans and reserializes the whole tree on every cycle.

An implementation account of all three findings is in [Why mutagen is slower](./mutagen.md).

## First synchronization

Time to first digest-verified convergence, with the corpus faulted into the volume and the page cache dropped beforehand.

| corpus | autobahn | mutagen |
|---|---|---|
| Chromium (505k files) | 423.3 s | 418.0 s |
| 40k files | 34.0 s | 31.8 s |
| 2 × 40k files | 35.5 s / 38.5 s | 30.0 s / 38.1 s |
| 4k files | 8.4 s | 5.8 s |

The two land within about one percent on Chromium. Half a million small files is bound by device IOPS rather than by either tool, so this measurement has little power to separate them. mutagen is modestly ahead on the smaller trees.

Without the storage controls these numbers are meaningless: an uncontrolled run charged the first tool 434 s and the second 125 s for identical work, purely from snapshot block loading. See [bench/README.md](../bench/README.md#lessons-paid-for).

---

Raw JSONL for all 150 jobs, the plan, and per-pair driver logs are in `bench/results-bench-1787811723/`.

## See also

- [Benchmarks](./benchmarks.md) — the summary, and what has changed since
- [Why mutagen is slower](./mutagen.md) — the implementation account
- [bench/README.md](../bench/README.md) — the method
