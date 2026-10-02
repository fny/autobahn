# The benchmark

The harness compares synchronization tools across corpus sizes, editor counts, directions, and destinations on EC2 hosts.

It measures write-to-readable latency with exact content verification, tool memory and CPU, and first-sync time. The [lessons section](#lessons-paid-for) records earlier measurement defects and their fixes.

## Quick start

Run these commands from `bench/`. AWS commands create billable resources. The local smoke test does not.

```bash
# Verify the harness itself, locally, in about six minutes. No AWS.
./smoke.sh

# Build the golden image: clone the corpus, build subsets, compute
# partitions, install both tools. ~20 minutes.
python3 orchestrate.py bake --profile PROFILE --region REGION

# Run the matrix. Prints the run id and the destroy command.
python3 orchestrate.py run --profile PROFILE --region REGION \
    --ami AMI --budget 2000 --repeats 10

# One cell only, for a before/after on a single change.
python3 orchestrate.py run --profile PROFILE --region REGION \
    --ami AMI --budget 400 --repeats 10 --cells 5k-1

# Turn JSONL into tables.
python3 aggregate.py results-RUN_ID/

# Terminate everything. Add --keep-ami to reuse the image.
python3 orchestrate.py destroy --profile PROFILE --region REGION --run RUN_ID
```

After changing the harness, run `./smoke.sh`. It exercises manifests, partitions, observer, agents, floor measurement, censoring, a complete `job.py` run, and aggregation against a known local subject.

## How many machines, and which

`--budget` specifies vCPUs, matching the account quota.

The planner uses the smallest instance suitable for each corpus. Measured peak demand was 0.7 cores for 5k, 1.7 for 50k, 2.3 for two 50k trees, and 3.3 for Chromium.

Groups differ by destination count and instance size. A group must meet a job’s minimum size and width.

For a shape with C units of work on N groups, expected completion scales as C/N. The planner balances that ratio across shapes within the budget.

Jobs run longest-first. Larger groups can accept smaller jobs to reduce idle time.

`chromium-10-fan` costs more than the other cells combined. Omitting it or reducing its repeats can roughly halve a full matrix run.

## Vocabulary

| Term | Meaning |
| --- | --- |
| Cell | Corpus × agent count × direction × destinations. `chromium-10` has ten editors and one destination. `chromium-10-fan` has ten destinations. |
| Job | One scheduled cell repetition, with both tools in randomized order on the same hosts |
| Repeat | Another execution of the complete job set |
| Group | One source plus its destinations: two hosts for a pair or eleven for ten-way fan-out |
| Agent | A thread simulating an editor. One per editing side also measures latency. |
| Floor | Harness contribution to latency, measured before tool runs |
| Censored | An edit beyond its deadline, retained in percentile positions |

A group runs jobs serially. Groups run concurrently.

## What is actually measured

The measuring agent selects a file, creates content, and announces the expected bytes to a destination observer. It writes locally and waits until the observer confirms the exact destination content.

A sample starts when the local write completes and ends when acknowledgement returns.

Both timestamps use one monotonic clock on the writer. Clock offset is measured only to align resource windows.

The observer requires a digest match. File events and matching size alone do not establish completion. Size is an initial filter to avoid hashing incomplete writes.

Latency includes verification and reply transit, so it is an upper bound on tool propagation. The floor uses the same protocol with the harness performing the transfer. Recorded medians are 0.5–0.8 ms.

Payloads derive from a run-and-job-specific nonce through SHA-256. Recorded seeds make them reproducible. Content from earlier runs cannot satisfy a new verification.

## The corpora

All corpora derive from one Chromium checkout, recorded by SHA in `plan.json`:

| Corpus | Contents |
|---|---|
| `chromium` | The full checkout, ~505,000 files |
| `sub50k` | Whole top-level directories totalling ≈50,000 files — a tenth |
| `sub50k-b` | Different directories, disjoint from `sub50k` |
| `sub5k` | ≈5,000 files — a hundredth |

Bake-time preparation removes symbolic links because the tools have different link policies. It excludes `.git` and `out` from synchronization and manifests.

A single baked image gives every group identical initial content.

## Partitions: what each agent may touch

`benchmark partitions` creates working sets at bake time from a complete sorted walk of files between 256 B and 256 KB.

The layout is `[measured A][measured B][pool A][pool B]`. Generation requires these properties:

- The measured set contains the same forty files at every editor count.
- Measured and background sets are disjoint.
- Background sets are mutually disjoint.
- Side A and side B sets are disjoint.
- The source walk reports no errors.

A shrinking measured set previously made higher concurrency appear faster by changing locality. Shared files would instead test conflict behavior, not propagation latency.

## A job, step by step

Each job runs both tools in randomized order. For each tool:

1. Remove sessions, daemons, staging, remote agents, and release and `-dev` state on both hosts. Check that processes and directories are gone.
2. Restore every editable source file from the pristine image copy.
3. Clear destinations.
4. Read the corpus to hydrate storage once per host. Drop page cache before the tool run.
5. Seed destinations for latency cells, or start `coldsync-*` cells empty. Record count-match and digest-verified completion separately.
6. Check matching digests across a ten-second quiet period.
7. Sample idle resources.
8. Check partitions on both hosts before the workload resource window.
9. Run editors for the fixed duration and record latency.
10. Check reconvergence.
11. Remove tool state again.

Every failure produces a typed record.

## Load

The measuring agent issues edits on its own schedule, independent of earlier acknowledgements.

At most 32 measured edits remain outstanding. Further ticks are skipped and counted. A tick also skips if all eight random file probes select pending files.

A file with pending verification is not edited again. This gives each request a stable target.

The first ten seconds form warmup. Those samples remain recorded but are excluded by their actual edit-start time, T0.

Background agents use separate files and the same cadence. They report completed edits, write errors, and panics so achieved load can be checked.

## Statistics

Percentiles include all attempts. Edits beyond the 120-second deadline occupy the highest positions. A percentile within censored results is reported as beyond the deadline, not as a finite success-only value.

Floor probes follow the same rule. The writer classifies censoring when matching the acknowledgement, so a late reply cannot race a timeout sweep.

Primary results pool raw samples across repeats. Reports also show the median and spread of per-run medians. In one twenty-repeat run across fifteen pairs, medians clustered within 1.5%.

A tool-run is tainted by failed reconvergence, unverified cold sync, workload errors in either direction, failure to settle before idle sampling, or background errors/panics.

Tainted runs contribute neither latency nor resources. `tainted_runs` records each exclusion and reason.

The plan is persisted before dispatch. Missing jobs remain visible in the report.

## Resources

The sampler follows each tool’s complete process tree once per second. Pattern-matched seeds expand through `/proc` PPid descendants, including transport children and remote agents.

Exited processes contribute their last observed CPU jiffies to a cumulative base. Window differences remain valid across process churn.

Process patterns are comma-separated alternatives. This covers daemons that re-execute under a bare basename.

Resource windows use phase timestamps. CPU baselines come from the last sample before the window so work at its start is counted. Measured clock offsets align remote samples.

The workload window follows the agents’ offered-load interval, not their process lifetime. A slow drain cannot extend the resource window.

A series that never matches a process is excluded as `resource_sampling_never_matched`.

## Integrity invariants

The harness requires these properties:

1. Editor count changes load without changing the measured working set.
2. Partitions come from a complete walk and remain disjoint within and across sides.
3. Each job runs both tools on the same hosts in randomized order.
4. Convergence requires matching content and zero walk errors.
5. Resource measurements use phase windows.
6. Each pair receives a floor measurement before tool runs.
7. Warmup uses true T0 and remains separately recorded.
8. Timed-out attempts retain their percentile positions.
9. Records include schema, run, pair, job, cell, repeat, tool versions, binary hashes, corpus commit, and phase timestamps.
10. Cleanup removes processes and directories between tools and jobs.
11. Workload scheduling is open-loop, with unique payload seeds per run.
12. Process cleanup cannot match the harness itself.
13. Missing measurements remain missing rather than becoming zero.

Cleanup uses exact process names or bracketed full paths. A bare substring can match the job specification, which includes both tool names. Local test mode touches only the toy subject.

## Lessons paid for

### Snapshot hydration biased first sync

Snapshot-backed volumes load blocks lazily. An early run charged the first tool 434 s on Chromium and the second 125 s, regardless of identity.

The extra cost scaled with corpus size: 309 s for Chromium, 17 s for 40k, and 1.3 s for 4k.

Randomized order balances this only with sufficient repeats. Three repeats produced an uneven draw. The harness now hydrates storage before measurement and drops page cache between runs.

### Process matching reported false zero memory

One tool re-executed its daemon with a bare basename. A full-path pattern missed it and reported 0 MB.

Alternative patterns now cover both forms. The aggregator rejects unmatched series rather than reporting zero.

## Known limits

Latency includes observer reads and a return network trip. The floor measures this overhead.

Floor probes are sequential, while workloads can have 32 pending verifications per direction. The floor does not measure this concurrency.

Each pending observer polls every 500 µs for its first second, then every 1 ms. At full capacity after backoff, that is about 32,000 polls per second per direction. Backoff can add about 1 ms of detection delay.

The 32-edit bound reduces achieved load for slow tools. `skipped_ticks` records that reduction. Such runs remain included because excluding them would bias results toward faster completion.

Chromium first sync is often dominated by device IOPS. Completion polling has five-second granularity, and digest verification adds a recorded delay after count match.

Within-job comparisons are stronger than separate runs. For a code change, stage both binaries as entries in the same job or use the local A/B gate.

## Files

| Path | Purpose |
| --- | --- |
| `harness/` | Rust `benchmark` binary for walks, manifests, partitions, observer, editors, floor, and resources |
| `harness/src/walk.rs` | Shared file-selection rules and walk-error accounting |
| `harness/src/partitions.rs` | Working sets and disjointness checks |
| `harness/src/observer.rs` | Verification workers and sequence-multiplexed replies |
| `harness/src/agents.rs` | Open-loop editors, censoring, and floor protocol |
| `harness/src/sampler.rs` | Process-tree sampling |
| `job.py` | Per-group job sequence and JSONL provenance, including local smoke mode |
| `orchestrate.py` | Image build, hosts, observer checks, persisted plans, collection, and teardown |
| `aggregate.py` | Percentiles, run spread, phase resources, exclusions, missing jobs, and problems |
| `toysync.py` | Known copy-loop subject |
| `smoke.sh` | Local end-to-end harness test |
| `../docs/benchmarks.md`, `../docs/benchmark-matrix.md` | Current summary and tables |
| `../benchmarks/2026-10-02.json` | The one complete set: aggregate and provenance |
| `results-*/` | Raw JSONL, plan, and driver logs; not committed |

The compiled harness gives both hosts identical instruments and shared walk rules. Editor threads and 500 µs polling remain in one small native program.

## The local gate: `bench/ab.sh`

The local gate compares two Autobahn builds in about ten minutes:

```sh
# From the repository root:
bench/ab.sh target/release/autobahn-before target/release/autobahn-after --legs 5
```

It interleaves legs over a generated 40,000-file corpus from `bench/corpus.py`. Each leg includes cold sync and an editor workload.

Reports compare p50, p90, and p99 against variation between legs.

`--corpus DIR` uses a supplied corpus as read-only input. Each leg edits a copy inside the script’s work directory. Deletion outside that directory is refused.

Each run creates a private `mktemp -d` work directory and removes it on exit. The generated corpus is cached in `$AB_CACHE`, default `~/.cache/autobahn-ab`, which must belong to the current user. Reports and logs remain in a separate private directory printed at completion.

The script tracks processes by PID so renamed binaries cannot survive cleanup. It rebuilds a harness that cannot execute, including binaries copied from another platform.

One leg per build produces no verdict because it cannot measure variation.
