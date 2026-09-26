# MAC-7: The 120-second full walk is the whole of autobahn's cost on battery

**Findings:** new, from MAC-BENCH 2. **Status:** proposed, Low (a laptop comfort item, not a correctness one).

## Problem

A running session walks both trees in full every 120 seconds (`FULL_SCAN_INTERVAL`, `src/endpoint/observer.rs`), because filesystem events can be lost. Measured on battery — macOS 26.5.1, Apple M4, commit `d7c2e21`, 30 one-minute `powermetrics` samples, a 160,000-file corpus, both roots local, machine otherwise idle:

| | CPU ms/s | Energy impact |
|---|---|---|
| mean | 55.16 | 76.51 |
| median | 51.93 | 64.72 |
| max | 123.53 | 163.00 |

The samples alternate, because the walk lands in every other window:

- **walking (15 samples):** mean energy **152.9**, ~110 CPU ms/s for a full minute — about 11% of one core;
- **idle (15 samples):** mean energy **0.10**, ~1 CPU ms/s.

So one walk costs about 6.6 seconds of CPU, there is one every two minutes, and the total is **about 3.3 minutes of CPU per hour** on a machine where nothing changes. In the same samples `sentineld` averaged 504 and `WindowServer` 116: while it walks, autobahn out-consumes the window server.

Between walks it costs nothing. The walk is the entire idle cost.

## Proposed resolution

Lengthen the interval when the machine runs on battery — 120 s on power, 600 s unplugged. macOS reports the power source (`IOPSCopyPowerSourcesInfo`), and Linux has `/sys/class/power_supply`. The cost of a walk does not change, only its frequency, so this is about a fifth of the energy: ~40 seconds of CPU per hour.

Two alternatives, both larger, recorded so the cheap one is not mistaken for the only one:

- **Walk only when the watcher admits it lost something.** FSEvents and inotify can both report dropped events. The walk exists to distrust exactly that claim, so this needs its own argument before it is believed.
- **Make the walk cheaper rather than rarer.** Nothing here says it can be; it is already parallel.

## Tests

- On battery, the interval is the longer one; on power, the shorter; and a change of power source is picked up without a restart.
- The interval is still configurable, and an explicit setting wins over both defaults.
- A session that has lost events still walks promptly, whatever the power source.

## Not run

The 600-second comparison window was skipped by decision: the arithmetic follows from the per-walk cost. Running it would only test whether a colder page cache makes each walk dearer.
