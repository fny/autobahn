# Benchmark: Autobahn vs mutagen

The latest recorded comparison is **Autobahn 0.4.0 against mutagen 0.19.0-dev**, on Linux EC2 hosts. The full September 28, 2026 run covers **32 cells, five repeats, 160 jobs**. Five patch cells were subsequently replaced with complete paired measurements through October 1. Each cell identifies its build in the [benchmark matrix](./benchmark-matrix.md#provenance); these are measured revisions, not a claim that today's HEAD has been benchmarked.

The [committed aggregate](../benchmarks/2026-10-01.json) holds every published figure. The [harness guide](../bench/README.md) describes how the measurements are made. The older [0.3.0 report](./benchmarks-0.3.md) and [matrix](./benchmark-matrix-0.3.md) remain available as historical records.

## Headline

| Measurement | Autobahn | mutagen | Ratio |
|---|---:|---:|---:|
| Small-file edit, Chromium, 1 editor, p50 | **23.8 ms** | 6,232.2 ms | 261.9× |
| Small-file edit, 50k subset, 10 editors, p50 | **13.4 ms** | 1,809.8 ms | 135.1× |
| Large-file patch, Chromium, 1 editor, p50 | **33.1 ms** | 7,575.9 ms | 228.9× |
| Peak controller memory, Chromium, 1 editor | **479 MiB** | 2,081 MiB | 4.3× |
| Idle controller CPU, Chromium, 1 editor | **0.1% of a core** | 49.9% | rounded measurements |
| First sync, Chromium, one destination | **228.1 s** | 454.8 s | 2.0× |

The small-file, memory, CPU, and first-sync rows use build `e7b3ac0`; the patch row uses `0c72865`. Memory is the median of per-run peak RSS during the workload. It is controller-side process-tree memory, including transport children, not the sum of both hosts. The idle CPU values are rounded to a tenth of a percentage point, so a precise speedup ratio would be misleading.

## What was measured

A measuring editor writes bytes on one host and waits until an observer can read exactly those bytes on the destination. Both timestamps come from the writing host's monotonic clock. Digest verification and the observer's reply are included, so the number is an upper bound on propagation time. The base run's harness floor was 0.7 ms median.

Background editors use disjoint file sets; increasing their count changes load without changing the measuring editor's small-file working set. The workload is open-loop with bounded outstanding edits. A skipped tick means the requested load could not be issued, and remains reported in the matrix. No latency sample in this published aggregate was censored at the deadline, and no tool-run was excluded.

The matrix distinguishes small-file replacements, large-file patches, bidirectional editing, ten-destination fan-out, bursts, and first synchronization. Ordinary latency cells start with seeded destinations. First-sync cells start empty and include digest verification; their timing should not be inferred from a latency cell's startup.

## Tails and load

A median is not the whole result. At 100 editors on each side of Chromium, the base build measured p50 **232.9 ms** alpha-to-beta and **299.0 ms** beta-to-alpha, with p99 **1,239.4 ms** and **1,305.5 ms**. Those figures remain in the matrix because that cell has not been rerun on the later audit build.

The October 1 patch runs do measure that later build. On one-destination Chromium, p50/p90/p99 were **33.1/44.4/48.2 ms** for one editor and **34.0/44.8/58.1 ms** for ten. With ten destinations, they were **42.0/67.0/107.8 ms**. These cells exercise the fixes that defer newly written temporary files and move the timed full walk off foreground scans. They do not establish the effect on every other cell.

Some mutagen patch runs skipped substantial numbers of ticks. Their slower completion reduced the offered edit rate; the matrix exposes both sample counts and skipped ticks rather than treating the tools as receiving identical achieved load.

## Memory, CPU, and first sync

Memory depends on the workload and topology. The headline Chromium small-file cell uses 479 MiB on the controller; the later ten-destination Chromium patch cell uses about 2.14 GiB. Neither is a universal per-file memory cost. The matrix includes each host's idle and workload figures so these cases can be compared directly.

Dedicated first-sync medians were **4.9 s vs 7.3 s** for the 5k subset, **21.9 s vs 52.1 s** for the 50k subset, and **228.1 s vs 454.8 s** for Chromium. Ten-destination Chromium was **221.3 s vs 505.4 s**. Different cells run on different machine groups; the fan-out result does not imply that adding destinations makes a single machine faster.

## Currency: what changed since these numbers

The published matrix is assembled from three Autobahn revisions: `e7b3ac0` for the base, `98598b8` for the two 50k patch cells, and `0c72865` for the three Chromium patch cells. Both tools' latency and resource measurements are replaced together for each updated cell. The [provenance table](./benchmark-matrix.md#provenance) identifies all four source runs.

The older report's estimated “+6 ms” correction and projected cold-sync cost are not used here: these tables use recorded measurements. The historical report is useful for understanding earlier fixes, but its corpus, compiler/runtime choices, and harness differ. Compare the two tools within a cell; use interleaved A/B runs to attribute a change to one code revision.

These results cover Linux hosts in one AWS region. They do not measure the desktop app, macOS filesystem behavior, WAN links, or every workload. [Why mutagen is slower](./mutagen.md) explains the implementation inspected for the older comparison, with its source revision pinned; it is not a fresh review of upstream mutagen.
