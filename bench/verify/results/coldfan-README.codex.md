# Cold sync against fan-out width

This historical record applies to the measured setup. See [Current benchmarks](../../../docs/benchmarks.codex.md) for later results.

The experiment measured whether source work repeats for each destination or benefits from the shared observer.

Earlier local tests reported 7.3× and 6.4× scaling. They also placed all destination work on the measured source host, so those results did not isolate source cost.

## Setup

The run used five `c6i.2xlarge` instances in us-east-2: one source and `dest1` through `dest4`.

The `sub50k` corpus contained 62,952 files and 520 MB. Widths one through four each ran three times.

Every run dropped caches on all hosts and checked that destinations were empty. Concurrent cheap-manifest probes timed convergence. A content digest check followed after the clock stopped.

Script: `bench/verify/coldfan.sh`.

## Results

| width | elapsed s (3 runs) | mean | source CPU s | CPU per destination |
|---|---|---|---|---|
| 1 | 49.4, 53.6, 49.4 | 50.8 | 4.3 | 4.30 |
| 2 | 53.7, 53.5, 50.1 | 52.4 | 6.9 | 3.45 |
| 3 | 50.2, 50.9, 50.8 | 50.6 | 10.2 | 3.40 |
| 4 | 50.9, 50.9, 52.2 | 51.3 | 14.1 | 3.53 |

## Reading it

Mean elapsed time varied from 50.6 to 52.4 seconds without a trend across widths one through four. Within this tested range, parallel destinations did not increase completion time.

A least-squares fit for source CPU gives:

source CPU = 0.70 + 3.27 × width, in seconds.

This suggests about 0.7 seconds of fixed scan/digest work and 3.3 seconds per destination for content supply.

The two-point fit from widths one and two gave a 1.7-second fixed cost: 2 × 4.3 − 6.9. That credited sharing with about 40% of single-destination CPU. The four-point fit reduces that estimate to about 16%.

Marginal CPU rose by 2.6, 3.3, and 3.9 seconds for additional destinations. These differences exceed the within-width spread of about 0.2–0.5 seconds. Fit residuals were +0.33, −0.34, −0.31, and +0.32.

Contention is a plausible explanation. At width four, the eight-vCPU source runs four sessions and four SSH processes. Wider tests and a larger source can distinguish contention from other scaling costs.

## Limits

The data establishes stable elapsed time only through width four. It also shows that per-destination CPU dominates the smaller shared cost.

The linear fit projects about 33 seconds of source CPU at width ten. The rising marginal makes this an optimistic projection rather than a reliable estimate. The earlier two-point projection of 29 seconds was more optimistic.

## Harness notes

Three defects were corrected:

1. `pkill -f '[a]utobahn'` matched the reset shell because its arguments also named `~/.autobahn`. Cleanup stopped before deletion and produced false 2.8-second cold syncs. Exact-name `pkill -x` fixed it.
2. A bare `wait` also waited for the background supervisor, which never exited. The script now waits only for probe PIDs.
3. Full digest polling every second loaded destinations. The script now polls cheap manifests and checks content once after timing.

The empty-destination assertion detected the first defect and remains part of the procedure.
