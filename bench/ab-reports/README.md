# A/B reports

The raw output behind the "Currency" section of
[BENCHMARK.md](../../BENCHMARK.md), and behind the A/B claims in the
commit messages of every hot-path change.

These are *not* the benchmark. The benchmark measures autobahn against
mutagen on matched pairs of AWS instances; these measure autobahn
against itself on one machine, to answer a narrower question: did this
change move steady-state latency before it shipped? That gate exists
because analysis estimates of these costs were wrong every time they
were tried, and a ten-minute local harness was not.

Each file is one run of `bench/harness`'s `agents` subcommand over a
63,000-file corpus with ten simulated editing agents, reporting the
percentiles of edit-to-visible latency.

| Prefix | Question it answered |
|---|---|
| `preA` / `phaseA` | Did intent records cost anything? (No.) |
| `preC` / `phaseC` | Did the observer and staging fixes cost anything? (No.) |
| `preF` / `fixes` | What does syncing the intent record cost? (~6 ms of p50; `fixes1`–`fixes2` sync every cycle, `fixes3`–`fixes5` sync only for remote sessions, which is what shipped.) |
| `n6` / `n8` | Should the file-watcher dependency be upgraded? (No: `n8a` and `n8d` stalled to p99 5,703 ms and 6,158 ms where `n6` never exceeded 125 ms.) |
| `dpreF` / `dhead` | Has anything drifted since the correctness work? (No: median p50 52.9 ms before against 53.5 ms after, five interleaved pairs. Cold sync moved 7.6 s to 8.8 s, the publish re-hash.) |

Three caveats a reader should carry. The `dpreF`/`dhead` series ran
while an unrelated mutagen synchronization was live on the same host,
costing roughly 8% of an eight-core machine; two of its cold-sync
numbers (25.2 s and 34.1 s) are page-cache and contention outliers and
are excluded from the medians quoted above. The `preA3` (75.8 ms) and `phaseA4`
(72.2 ms) runs were contaminated by stray supervisors from earlier runs
holding session locks; they were re-run, and the clean pairs are the
ones the conclusions rest on. And the `n6`/`n8` series ran on macOS
with a 6,000-file corpus, while the rest ran on Linux with 63,000 —
compare within a series, never across.
