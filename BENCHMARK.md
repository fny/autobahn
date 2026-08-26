# Chromium-scale benchmark: autobahn vs mutagen

A head-to-head measurement of bidirectional synchronization of the
Chromium source tree between two EC2 instances, covering cold
synchronization, edit-propagation latency, memory, and idle CPU.

**Headline:** autobahn synchronized the same tree with **6.4× less
controller memory**, propagated edits **7.5× faster**, and used **5×
less CPU while idle**. Cold synchronization was 1.2× faster. Every
number below is an externally verified measurement, not a tool's own
report of itself.

## Setup

| | |
|---|---|
| Hosts | 2 × EC2 `c5.2xlarge` (8 vCPU, 15GB RAM), us-east-2, same subnet, 150GB gp3 (6000 IOPS / 500 MB/s) |
| OS | Ubuntu 24.04, kernel 6.14 |
| Corpus | Chromium `src` @ depth-1 clone of `github.com/chromium/chromium` |
| Corpus size | **504,528 files, 46,127 directories, 19 symlinks, 5.5GB** (`.git` excluded) |
| autobahn | 0.2.0 (`77314fa`), release build, static musl |
| mutagen | 0.19.0-dev, release build |
| Mode | `two-way-safe`, both tools |
| Ignores | `/.git`, `/out` — verified to yield identical path sets on both tools |
| Fallback interval | 5s on both (`interval = 5`, `--watch-polling-interval=5`) |
| SSH | key-based, no external multiplexing, agent auto-installed by both tools |

Both tools ran one at a time, never concurrently. Destination roots
were empty at the start of each cold run, and both tools installed
their own remote agent on first contact.

## Methodology

The benchmark design was reviewed by an independent model (GPT-5.6) with
an explicit brief to find measurements that would lie. Three of its
corrections shaped what is reported here.

**Latency is measured in one clock domain.** Timestamps from two hosts
cannot be subtracted — clock skew would silently become part of the
result. Instead, a destination-side observer is told in advance what
content to expect; the writer performs the edit, records `T0` from
`CLOCK_MONOTONIC_RAW` immediately after the final `rename()` returns,
and records `T1` when the observer's acknowledgement arrives over a
persistent TCP connection. Both timestamps come from the *same* clock on
the writing host. The observer acknowledges only after reading the file
and confirming its SHA-256 matches, so a creation event alone never
counts as propagation. The measured interval therefore *includes* the
destination's verification read and the acknowledgement's return trip:
every latency below is a conservative upper bound. The control channel's
own round trip was 0.17ms (p50) and is not subtracted.

**Completion is verified externally.** No tool's status output is
trusted to define "done". The destination is polled independently until
its file count reaches the expected 504,528, giving a bracket at 10s
granularity, and the result is then verified by comparing full path
manifests and content digests between the two hosts.

**Memory is sampled from the whole process tree.** Both tools spawn
children (SSH, session workers, agents) whose memory belongs to them, so
RSS is summed across each tool's process tree once per second rather
than read from a single PID.

## Results

### Cold synchronization

First contact: no agent installed, empty destination, tool not running.

| | mutagen | autobahn | |
|---|---|---|---|
| Cold sync, 504k files / 5.5GB | **125.0s** | **101.4s** | autobahn 1.23× faster |
| (10s poll bracket) | 113–125s | 91–101s | |
| First bytes at destination | ~57s | ~55s | comparable |

Both tools spend the first ~55 seconds the same way: scanning half a
million files on each side, reconciling, and beginning to stage. The
difference is in the transfer and application phase that follows.

**Correctness verified.** After autobahn's run, the two trees had
identical path sets (`md5` of sorted `find` output matched exactly:
`210da4b6…`), identical entry counts (550,674), and identical content
across a 1,009-file digest sample (`e461d71a…`). The only difference was
directory permission bits (775 on the source, 700 on the destination) —
autobahn's configured `directory_mode` default, not a synchronization
error.

### Edit propagation latency

Time from a completed write on one host to verified identical content on
the other. 40 trials, 64KB atomic replacements (temp file + `rename`).

| | mutagen | autobahn | |
|---|---|---|---|
| p50 | **9,692ms** | **1,298ms** | autobahn 7.5× faster |
| p90 | 9,765ms | 1,310ms | |
| p99 | 9,769ms | 1,322ms | |
| min–max | 9,342–9,771ms | 1,233–1,399ms | |

A second variant repeatedly edits one file, which is mutagen's most
favorable case — its portable watching natively tracks only recently
modified content, so a first touch of a "cold" path waits for a polling
cycle:

| Repeated edits to a single file | mutagen | autobahn |
|---|---|---|
| p50 | 4,441ms | **1,367ms** |
| min–max | 4,310–7,630ms | 1,186–1,438ms |

Even on its warm path mutagen is 3.2× slower than autobahn is on a cold
one. This is the clearest effect of the incremental scanning work: a
cycle no longer pays a full metadata sweep of half a million files just
to discover that one of them changed.

### Under a 10-agent workload

Ten concurrent processes editing files across the tree on a
coding-agent-like cadence (a few files per second each, atomic replace,
random sizes 2–64KB), while one of them measures verified round trips.

| | mutagen | autobahn | |
|---|---|---|---|
| p50 | **9,424ms** | **2,276ms** | autobahn 4.1× faster |
| p90 | 13,033ms | 2,656ms | 4.9× |
| p99 | 13,510ms | 4,440ms | 3.0× |
| max | 13,556ms | 4,729ms | |

Both degrade under churn, as expected — a cycle that finishes finds more
work waiting. Autobahn's tail stays bounded at ~4.7s where mutagen's
reaches ~13.6s.

### Memory

Summed RSS across each tool's process tree.

| | mutagen | autobahn | |
|---|---|---|---|
| Controller, peak | **2,123MB** | **333MB** | 6.4× less |
| Controller, steady | 1,633MB | 264MB | 6.2× less |
| Remote agent, peak | 1,501MB | 307MB | 4.9× less |
| Remote agent, steady | 1,078MB | 102MB | 10.6× less |
| **Total footprint (steady)** | **2,711MB** | **366MB** | **7.4× less** |

This is the difference the project was started over, measured at the
scale where it matters: keeping half a million files in sync costs
mutagen roughly 2.7GB across both machines, and autobahn roughly 0.37GB.

### Idle CPU

No changes being made; both tools watching and heartbeating on a 5s
interval.

| | mutagen | autobahn | |
|---|---|---|---|
| Controller | **64%** of a core | **12%** of a core | 5.3× less |
| Remote agent | 68% of a core | 3% of a core | 22× less |

Mutagen's portable watching polls, which at this tree size means a
continuous rescan of 500k files on both ends — two thirds of a core
burned on each machine to observe that nothing happened. Autobahn's
12% is its own remaining inefficiency (see below), not watching cost.

## What limits autobahn now

The measurements point at three specific costs, none of which are
transfer or watching:

1. **The ancestor and scan cache are 52MB each and rewritten whenever
   content changes.** At 104MB of serialization per changed cycle, this
   is a large part of the ~1.3s propagation floor. Persisting the
   ancestor incrementally, or asynchronously, would attack it directly.
2. **Reconciliation walks the whole tree every cycle.** Scanning is now
   incremental, but the three-way merge still visits all 550k nodes even
   when both sides share the same `Arc`. A pointer-equality short-circuit
   would make reconciliation proportional to the change rather than the
   tree, and would also account for most of the 12% idle CPU.
3. **Cycles are not paced.** Under sustained churn the only spacing is a
   100ms settle window, so cycles run back to back. A minimum inter-cycle
   cooldown would trade a little latency for a lot of CPU headroom.

## Caveats

- Cold-sync timing has 10s granularity from the completion poll; the
  brackets are reported rather than a false-precision midpoint.
- Latency samples are 40 per configuration (15 for the single-file
  variant) — enough to separate effects of this size, not enough for
  confident tail statistics beyond p90.
- Both tools ran with their own wire encoding (autobahn LZ4-framed with
  SSH compression disabled; mutagen its own). Internal compression is
  treated as part of each product rather than something to be equalized.
- mutagen ran in its default portable watch mode. This is its
  recommended configuration, but it is polling-assisted rather than
  purely native, which is visible in both the idle CPU and the
  cold-path latency numbers.
- `.git` was excluded from the corpus, per mutagen's own guidance for
  synchronizing source trees. A run including it would be a different
  (and harsher) test for both tools.
- Instances were not pinned against EBS or network burst-credit
  variation beyond choosing a non-burstable instance family and
  provisioned gp3 throughput.
