# Cold sync against fan-out width

**Question.** When one source feeds N destinations a cold (empty-destination) sync, does the source pay N times, or does the shared observer save work?

**Why this needed real machines.** Two earlier local measurements answered "7.3x" and "6.4x" and both were artifacts. Running ten local destinations puts all ten destinations' own work on the one host doing the measuring, so the curve that came out described the test, not the tool. The only fix is destinations on their own machines.

## Setup

Five `c6i.2xlarge` machines in us-east-2: one source and four destinations wired as `dest1`..`dest4`. Corpus `sub50k` (62,952 files, 520 MB). Widths 1 through 4, three repeats each. Caches dropped on all five machines before every run, and every destination asserted empty before the clock starts. Convergence is polled with the cheap manifest (probes run concurrently) and confirmed by content digest afterwards.

Script: `bench/verify/coldfan.sh`.

## Results

| width | elapsed s (3 runs) | mean | source CPU s | CPU per destination |
|---|---|---|---|---|
| 1 | 49.4, 53.6, 49.4 | 50.8 | 4.3 | 4.30 |
| 2 | 53.7, 53.5, 50.1 | 52.4 | 6.9 | 3.45 |
| 3 | 50.2, 50.9, 50.8 | 50.6 | 10.2 | 3.40 |
| 4 | 50.9, 50.9, 52.2 | 51.3 | 14.1 | 3.53 |

## Reading it

**Wall clock is flat.** Means of 50.8, 52.4, 50.6, 51.3 seconds across widths 1 to 4, a spread of 1.8s with no trend in it. Four destinations cost the same elapsed time as one. The destinations work in parallel, so fan-out is free in the dimension a user actually waits on. This is the headline and it holds across the whole range tested.

**Source CPU is close to linear, and the fixed part is small.** A least squares fit over the four points gives:

source CPU = 0.70 + 3.27 x width      (seconds)

So roughly 0.7s is paid once regardless of destination count, and about 3.3s is paid per destination. The fixed part is the scan and digest of the tree, which the shared observer deduplicates. The per-destination part is the supply path: reading and chunking content for each stream. That part is irreducible, because each destination needs the bytes sent to it.

**A two-point fit overstated the sharing.** Widths 1 and 2 alone imply a fixed cost of 2 x 4.3 - 6.9 = 1.7s. The four-point regression puts the intercept at 0.7s. The earlier two-width measurement therefore credited the shared observer with roughly 40% of a single sync's source CPU; the honest figure is about 16%. Sharing is real, but it is a smaller effect than two points suggested.

**The marginal cost per destination rises.** Adding the 2nd, 3rd, and 4th destination cost 2.6s, 3.3s, and 3.9s of source CPU. Those increments are well outside the within-width spread (about 0.2 to 0.5s), so the trend is real rather than noise. The linear fit's residuals are correspondingly U-shaped (+0.33, -0.34, -0.31, +0.32).

A plausible mechanism is contention: the source has 8 vCPUs and at width 4 is running four sessions alongside four ssh processes encrypting their streams. Confirming that would need widths past 4, and a source with more cores to separate contention from a genuine property of the fan-out.

## What this does and does not establish

Established: elapsed time does not grow with fan-out width through 4; the source's fixed cost is small but nonzero; per-destination cost dominates.

Not established: the shape past width 4. Extrapolating the linear fit to ten destinations gives about 33s of source CPU, but since the marginal cost is still rising at width 4, treat 33s as a lower bound rather than an estimate. The earlier extrapolation of 29s, made from two points, was too optimistic.

## Harness notes

Three defects in this script were found and fixed while running it, all of which would have produced confident and wrong numbers:

1. The destination reset killed itself. `pkill -f '[a]utobahn'` matches full command lines, and the same command line named `~/.autobahn` in its `rm` arguments, so pkill matched its own shell and killed it before the `rm` ran. Destinations kept their files and reported 2.8s "cold" syncs. Fixed by matching process names exactly with `pkill -x`.
2. A bare `wait` for the concurrent probes also waited on the autobahn daemon started with `&`, which never exits, so the first run hung indefinitely. Fixed by waiting on the probe PIDs specifically.
3. The convergence poll originally computed a full content digest on every destination every second, which would have loaded the destinations continuously and distorted the measurement. Fixed by polling with the cheap manifest and digesting once, after the clock stops.

The assertion that every destination is empty before the clock starts is what caught the first of these. It is worth keeping.
