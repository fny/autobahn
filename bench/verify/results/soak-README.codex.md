# Soak, 29 August 2026

This historical record applies to the measured setup. See [Current benchmarks](../../../docs/benchmarks.codex.md) for later results.

The run used separate source and destination machines, a 62,952-file tree, and two hours of continuous editing. Resource samples were taken every thirty seconds.

Earlier attempts recorded resources but no valid latency data because of harness defects.

## Latency

Twelve ten-minute rounds ran ten agents each, including one measuring agent.

| | value |
|---|---|
| samples | 8,777 |
| **censored** | **0** |
| p50, first round | 66.6 ms |
| p50, last round | 70.4 ms |
| p90 range across rounds | 94.1 – 107.2 ms |

The last-round median was within four milliseconds of the first. All 8,777 measured edits propagated within the deadline. The session completed 58,932 cycles with no logged errors.

## Resources

| | start | end | drift |
|---|---|---|---|
| inotify watches | 6,625 | 6,625 | **0** |
| file descriptors | 11 | 11 | **0** |
| staging files | 0 | 0 | 0 |
| staging bytes | 0 | 0 | 0 |
| resident memory | 39,968 kB | 53,568 kB | +34% |

Inotify watches remained at one per directory. File descriptors stayed constant. Staging counts and bytes remained zero in the recorded samples.

RSS increased 34%, but its growth rate declined:

| window | slope |
|---|---|
| first 10 minutes | +687.6 kB/min |
| 10 to 60 minutes | +20.0 kB/min |
| 60 to 120 minutes | +9.8 kB/min |

This pattern is consistent with allocator retention or caches approaching their working size. It is evidence of stabilization, not proof that no leak exists.

Extending the final rate unchanged gives about 13.7 MB/day. That projection assumes the decline stops. Continued halving instead suggests roughly one megabyte of remaining growth.

Two hours provides too little evidence to select either extrapolation. Sampling a third and fourth hour can show whether the decline continues.

## Earlier harness defects

The following defects affected earlier attempts:

1. `pkill -f 'benchmark [o]bserver'` also matched a later `~/bench/benchmark observer 9911` argument in the same shell command. It killed the shell before observer startup. Cleanup and launch now use separate calls and process-name matching.
2. `launch.py` did not write `~/bench/peer-ip`, unlike `orchestrate.py`. A soak started through that path used an empty observer address.
3. The watch metric counted inotify instances instead of watch descriptors. It reported one rather than 6,625.

The harness also counted destination files on the source host, where the path was absent, and referenced a default corpus no longer in the image.
