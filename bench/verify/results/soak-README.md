# Soak, 29 August 2026

Two hours of continuous editing against a 62,952-file tree, one source and
one destination on separate machines, sampling both every thirty seconds.

This is the first soak that produced latency data. The two before it
measured resources only, because the probe was misconfigured; see the end
of this file for what was wrong, since all of it was in the harness rather
than in the tool.

## Latency: nothing degrades

12 rounds of ten minutes, ten agents each, one measuring:

| | value |
|---|---|
| samples | 8,777 |
| **censored** | **0** |
| p50, first round | 66.6 ms |
| p50, last round | 70.4 ms |
| p90 range across rounds | 94.1 – 107.2 ms |

Latency at the end of two hours is within four milliseconds of the start,
and not one edit in 8,777 went unpropagated. 58,932 synchronization cycles
completed with zero errors logged.

## Resources: nothing leaks

| | start | end | drift |
|---|---|---|---|
| inotify watches | 6,625 | 6,625 | **0** |
| file descriptors | 11 | 11 | **0** |
| staging files | 0 | 0 | 0 |
| staging bytes | 0 | 0 | 0 |
| resident memory | 39,968 kB | 53,568 kB | +34% |

Watches are the sharpest signal and did not move by one: exactly one per
directory, held steady through two hours of churn, so watches are not
leaked as they are re-established. Staging stayed empty, which says
content-addressed staged files are reclaimed rather than accumulated.

Resident memory grew 34%, which needs the shape rather than the endpoints
to interpret:

| window | slope |
|---|---|
| first 10 minutes | +687.6 kB/min |
| 10 to 60 minutes | +20.0 kB/min |
| 60 to 120 minutes | +9.8 kB/min |

**The growth rate halves each hour.** A leak has a constant slope; a decaying
one is an allocator holding arenas and caches reaching their working size.
Taken at face value the final rate would be +13.7 MB/day, but that number
assumes the decay stops, and nothing in the series suggests it does — if the
halving continues, the remaining total growth is on the order of a megabyte.

Two hours yields only two slope estimates, so this is evidence of
convergence rather than proof of it. A longer run would settle it, and the
cheap version is to sample a third and fourth hour and check the slope keeps
halving.

## What was wrong with the earlier attempts

Three defects, all in the harness, none in the tool:

1. `pkill -f 'benchmark [o]bserver'` used the bracket trick so the pattern
   would not match itself — but the same command line went on to name
   `~/bench/benchmark observer 9911`, which it does match. The line killed
   its own shell before starting the observer, so every sample was refused a
   connection. Now two calls, matched by process name, which cannot match a
   command line at all.
2. `launch.py` never wrote `~/bench/peer-ip`. Only `orchestrate.py` dispatch
   did, so a soak launched the other way read an empty address and pointed
   its observer at nothing.
3. The watch metric counted inotify *instances* — always one — rather than
   watch descriptors. It reported 1 for a tree holding 6,625.

The destination file count was also being taken on the source, where the
path does not exist, and the default corpus no longer existed in the image.
