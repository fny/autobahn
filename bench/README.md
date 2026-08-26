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
| `partitions.py` | At image-bake time: full deterministic walk of each corpus, manifest (path, size, digest), and per-cell agent partitions with disjointness *asserted*, all written as JSON consumed identically by both hosts. |
| `observer.py` | Destination-side verifier. Also implements the floor mode, where it writes the announced payload itself so the harness path is measured with no sync tool in the loop. |
| `agents.py` | The edit workload. Partition-file driven; the measuring agent's working set is fixed at 40 files **regardless of agent count**, so agent count varies load only. |
| `sampler.py` | One resource series per tool: timestamped RSS and CPU-jiffy totals for a process tree, sliced per phase by timestamps recorded in the results — never process-lifetime peaks. |
| `job.py` | Runs one job on host A: for each tool — clean state, cold sync (content-verified), quiescence (verified), idle CPU, workload, final divergence check, teardown. Emits JSONL. |
| `toysync.py` | A deliberately dumb local "sync tool" (watch + copy loop) used by the smoke test to validate the measurement plumbing end to end without EC2 or either real tool. |
| `smoke.sh` | Local end-to-end test of the harness itself. Run it after any harness change. |
| `orchestrate.py` | AWS lifecycle: bake the golden image (corpus cloned once, subsets and partitions computed once — byte-identical on every host), launch pairs, dispatch jobs, collect, aggregate, terminate. |
| `aggregate.py` | Turns collected JSONL into the report tables: median across repeats, spread, and every caveat the data carries. |

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
   the source's. Idle CPU is sampled only after verified quiescence.
5. **Resources are windowed.** Peaks and means are computed per phase from
   a timestamped series. (Previously lifetime peaks were captioned as
   per-workload peaks.)
6. **The harness floor is measured**, per pair, before any tool runs.
   Sub-100ms latencies are reported alongside it, not as engine time.
7. **Warmup is excluded and reported.** The first samples after a workload
   starts are recorded but kept out of the percentiles.
8. **Timeouts are censored data, not crashes.** A sample that exceeds the
   per-edit deadline is recorded as `>deadline` and counted; it neither
   kills the run nor silently vanishes.
9. **Every record carries provenance**: schema version, run id, pair, job,
   cell, repeat, tool, tool versions, commit, and phase timestamps. Every
   run that starts is either completed or recorded as failed — nothing is
   dropped without a record. (Previously a valid run was discarded
   silently.)
10. **State is destroyed between tools and between jobs** — sessions,
    staging, daemons, destination trees — and the cleanliness is checked,
    not assumed.

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
