# The benchmark harness

Measures autobahn against mutagen across a matrix of corpus sizes and
agent counts, on pairs of EC2 hosts. This directory is the harness that
replaced the ad-hoc scripts behind `BENCHMARK-MATRIX.md`, after an audit
found flaws in them; every one of those flaws has a designed answer here,
listed at the bottom.

## Vocabulary

- **Cell** — one configuration: corpus × session count × agent count ×
  direction. The matrix in `orchestrate.py` defines them.
- **Job** — one cell executed on one host pair, running **both tools back
  to back in randomized order**. The unit of scheduling.
- **Repeat** — the full set of jobs, run again. Statistics are computed
  across repeats.
- **Pair** — two EC2 hosts (A: source + driver, B: destination +
  observer). A pair runs its jobs serially; pairs run concurrently.

## The measurement, in one paragraph

Simulated agents rewrite files at a coding cadence. One agent per editing
side also *times* its edits: it announces the exact content it is about to
write to an observer on the receiving host, writes, and waits for the
observer to acknowledge that exactly those bytes are readable at the
destination path. Both timestamps are taken from one monotonic clock on
the writing host, so no cross-host skew can enter, and the interval
includes the observer's verification read and the acknowledgement's return
trip — every reported latency is an upper bound on the tool's own
propagation time. The harness's own contribution to that upper bound is
measured (the *floor* phase) and reported alongside.

## Files

| File | Role |
|---|---|
| `harness/` | **The measurement plane, in Rust, as one static binary** (`benchmark`): tree walks and manifests, partition generation and verification, the observer, the agent workload, the floor, and the resource sampler. Compiled so both hosts run byte-identical instruments, the walk rules exist once, a hundred agents are threads in one small process, and the observer polls at 500µs. |
| `job.py` | Runs one job on host A: for each tool — clean state (verified), cold sync (digest-verified), quiescence (digest-verified), idle window, workload, reconvergence check, teardown. Emits JSONL with full provenance. Supports a local mode for the smoke test. |
| `toysync.py` | A deliberately dumb local "sync tool" (copy loop) used by the smoke test as a subject with known behavior. |
| `smoke.sh` | Local end-to-end test of the harness itself, including a complete `job.py` run against toysync and an `aggregate.py` pass over its output. Run it after any harness change. |
| `orchestrate.py` | AWS lifecycle: bake the golden image (corpus cloned once, symlinks stripped, subsets and partitions computed once — byte-identical on every host), launch pairs, wire and *verify* observers, persist the full plan before dispatch, collect results and logs, destroy everything including the AMI. |
| `aggregate.py` | Turns collected JSONL into report tables: pooled-sample percentiles with censored attempts carried as "over deadline", medians across repeats with spread, phase-windowed clock-offset-corrected resources, delivered-vs-planned accounting, and tainted-run exclusion. |

## Invariants the design enforces

Each of these answers a specific defect found in the previous harness.

1. **Agent count varies load only.** The measuring agent edits the same
   fixed 40-file set at every agent count. (Previously the working set
   shrank as agent count grew, which changed edit locality and made
   mutagen look *faster* under 10× load.)
2. **Partitions are precomputed, sampled from a complete walk, and
   disjointness is asserted** — within a side, and across sides for
   bidirectional cells. (Previously each host early-stopped its own walk;
   contiguous prefixes, unverified disjointness.)
3. **Both tools run on the same pair, in an order randomized per job.**
   (Machine identity and ordering effects cancel across repeats instead of
   confounding the tool comparison.)
4. **Convergence is content-verified.** Cold sync completes when the
   destination's manifest — sizes and digests, not a file count — matches
   the source's, and only a walk that saw zero errors can certify it: two
   walks failing identically never compare two trees equal. Idle CPU is
   sampled only after verified quiescence.
5. **Resources are windowed.** Peaks and means are computed per phase from
   a timestamped series. (Previously lifetime peaks were captioned as
   per-workload peaks.)
6. **The harness floor is measured**, per pair, before any tool runs.
   Sub-100ms latencies are reported alongside it, not as engine time.
7. **Warmup is excluded and reported.** The first samples after a workload
   starts are recorded but kept out of the percentiles.
8. **Timeouts are censored data, not crashes.** A sample that exceeds the
   per-edit deadline is recorded as `>deadline` and counted; it neither
   kills the run nor silently vanishes — and it occupies its position in
   the percentiles, so a percentile landing among censored attempts
   reports a lower bound, never a flattering finite number. The censoring
   verdict is made at acknowledgement-match time on the writer's clock, so
   a late acknowledgement can never convert a timeout into a fast sample.
9. **Every record carries provenance**: schema version, run id, pair, job,
   cell, repeat, tool, tool versions, binary digest, and phase timestamps.
   Every run that starts is either completed or recorded as failed, and
   delivered results are reconciled against the persisted plan — nothing
   is dropped without a record. (Previously a valid run was discarded
   silently.)
10. **State is destroyed between tools and between jobs** — sessions,
    staging, daemons, installed agents, destination trees — and the
    cleanliness is checked, not assumed. Edited source files are restored
    from a pristine baked copy before each tool, so the second tool syncs
    the same bytes the first one did.
11. **The workload is open-loop.** The measuring agent issues edits on its
    cadence regardless of pending acknowledgements, so a slow tool faces
    the same offered load as a fast one. Payload streams are seeded from a
    per-run nonce, so no earlier run's content can satisfy a verification.
12. **Process hygiene cannot kill the harness.** Cleanup matches exact
    process names and full executable paths, never command-line substrings
    (the job spec itself contains both tools' names), and local test mode
    touches nothing but the toy subject.

## Known limits, stated up front

- Latencies include observer verification and one network return trip by
  design; they are upper bounds. The floor phase bounds the overhead.
- mutagen runs its default portable watch mode, which on a stock Linux
  build reifies to poll-assisted watching (native recursive watching
  exists only behind SSPL fanotify builds). This is the configuration a
  Linux user actually gets, and it is the dominant term in mutagen's
  detection latency. Its poll interval is set to 5s — *better* than its
  10s default.
- Cold-sync completion polling has 5s granularity; digest verification
  adds a bounded, recorded delay after the count first matches.
