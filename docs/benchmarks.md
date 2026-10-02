# Benchmark: Autobahn vs mutagen

The latest recorded comparison measures Autobahn v1.0.0 against mutagen 0.19.0-dev.


The [matrix](./benchmark-matrix.md#provenance) identifies the measured builds. These results do not establish the performance of later commits.

The [aggregate](../benchmarks/2026-10-02.json) contains every published figure. See the [harness guide](../bench/README.md) for methods.

## Headline

| Measurement | Autobahn | mutagen | Ratio |
|---|---:|---:|---:|
| Small-file edit, Chromium, 1 editor, p50 | **23.8 ms** | 6,232.2 ms | 261.9× |
| Small-file edit, 50k subset, 10 editors, p50 | **13.4 ms** | 1,809.8 ms | 135.1× |
| Large-file patch, Chromium, 1 editor, p50 | **33.1 ms** | 7,575.9 ms | 228.9× |
| Peak controller memory, Chromium, 1 editor | **479 MiB** | 2,081 MiB | 4.3× |
| Idle controller CPU, Chromium, 1 editor | **0.1% of a core** | 49.9% | rounded measurements |
| First sync, Chromium, one destination | **228.1 s** | 454.8 s | 2.0× |

Memory is the median of per-run peak RSS during the workload. It is controller-side process-tree memory, including transport children, not the sum of both hosts. The idle CPU values are rounded to a tenth of a percentage point, so a precise speedup ratio would be misleading.

## Methodology

A measuring editor writes content on one host and waits for an observer to read exactly that content at the destination.

Both timestamps use the writer’s monotonic clock. Digest verification and the observer reply are included, making latency an upper bound on propagation. The base run’s median harness floor was 0.7 ms.

Background editors use separate file sets. Editor count changes load without changing the measuring editor’s small-file working set.

The open-loop workload limits outstanding edits. Skipped ticks mean that it could not issue the requested load. The matrix reports them. No samples in this aggregate exceeded the deadline, and no tool-run was excluded.

Cells cover replacements, large-file patches, bidirectional edits, ten-destination fan-out, bursts, and first sync. Latency cells use seeded destinations. Dedicated first-sync cells start empty and include digest verification.

## Tails and Latency

On Chromium with 100 editors per side, base-build primary-to-replica p50 was 232.9 ms and replica-to-primary p50 was 299.0 ms. Their p99 values were 1,239.4 ms and 1,305.5 ms. This cell has no later audit-build rerun.

The October 1 Chromium patch runs measured later fixes:

| Workload | p50 | p90 | p99 |
| --- | --- | --- | --- |
| One editor, one destination | 33.1 ms | 44.4 ms | 48.2 ms |
| Ten editors, one destination | 34.0 ms | 44.8 ms | 58.1 ms |
| Ten destinations | 42.0 ms | 67.0 ms | 107.8 ms |

These cells cover temporary-file deferral and moving timed audits off foreground scans. They do not establish effects on other workloads.

Some mutagen patch runs skipped many ticks. Slower completion reduced their achieved edit rate. Sample counts and skipped ticks remain visible so requested load is not confused with achieved load.

## Memory, CPU, and First Sync

Controller memory depends on workload and topology. The Chromium small-file cell used 479 MiB. The later ten-destination Chromium patch cell used about 2.14 GiB.

Neither figure defines a universal per-file cost. The matrix reports idle and workload resources for each host.

Dedicated first-sync medians were:

| Corpus | Autobahn | mutagen |
| --- | --- | --- |
| 5k subset | 4.9 s | 7.3 s |
| 50k subset | 21.9 s | 52.1 s |
| Chromium | 228.1 s | 454.8 s |
| Chromium, ten destinations | 221.3 s | 505.4 s |

Cells used different machine groups. The fan-out result does not show that adding destinations accelerates the same machine.
