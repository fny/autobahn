# Soak, 27 August 2026

Four and a half hours of continuous editing against a 41,515-file tree,
one source and one destination, sampling both hosts every thirty seconds.

**Result: nothing leaks.**

| | start | end | drift |
|---|---|---|---|
| source memory | 24,756 kB | 25,462 kB | +2.9% |
| source file descriptors | 11 | 11 | 0 |
| source inotify watches | 4,175 | 4,175 | **0** |
| destination memory | 13,166 kB | 13,142 kB | −0.2% |
| destination descriptors | 6 | 6 | 0 |
| staging files | 1 | 1 | 0 |
| staging bytes | 8,396 kB | 8,396 kB | 0 |

The watch count is the one to read first: it is exactly one per directory
and never moved, so watches are not leaked as they are re-established.
Staging held one file and 8.4 MB throughout, which says content-addressed
staging is reclaimed rather than accumulated over a long session — the
question the soak existed to answer.

The load was real: 115,884 synchronization cycles with **zero errors**,
over 171,000 background edits, and 8,668 files updated on the destination
in the final twenty minutes alone.

## What this run does not show

Latency. The measuring agent was pointed at an observer on the source host
watching a destination path that only exists on the *destination* host, so
no edit could ever be confirmed and all 3,527 were censored. The censoring
machinery behaved correctly; the probe was misconfigured. `soak.sh` now
starts the observer on the destination and reads its address from
`~/bench/peer-ip`.

Resource conclusions are unaffected — they depend on the tool being under
sustained load, which the cycle count and destination churn confirm.
