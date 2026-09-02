# The benchmark

Measures autobahn against another synchronization tool across a matrix of
corpus sizes and agent counts, on pairs of EC2 hosts. It answers one
question precisely — **how long after a file is written does it become
readable, byte for byte, on the other machine** — and two supporting ones:
what each tool costs in memory and CPU while it keeps up, and how long a
first synchronization takes.

Most of the design here is a direct answer to a way an earlier, ad-hoc
version of this benchmark produced numbers that were wrong while looking
plausible. The defects that were caught are recorded in
[Lessons](#lessons-paid-for) rather than quietly fixed, because each one
is a way a benchmark can lie while appearing to work.

---

## Quick start

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

Run `./smoke.sh` after any change to the harness. It exercises the real
binary end to end — manifests, partitions, observer, agents, floor,
censoring, a complete `job.py` run and an `aggregate.py` pass — against a
deliberately dumb subject whose behavior is known, and asserts on the
result. It is the harness's contract with itself.

---

## How many machines, and which

`--budget` is a number of vCPUs, not a number of machines, because that is
what the account's quota actually limits. The planner spends it.

Cells run on the smallest instance their corpus needs, measured rather than
guessed — peak CPU was 0.7 cores on 5k, 1.7 on 50k, 2.3 on two 50k trees
and 3.3 on Chromium. Giving every host sixteen vCPUs wasted most of the
quota, and the quota is what caps parallelism.

Groups come in shapes: a width (one source plus its destinations) and an
instance size. Jobs of one shape cannot run on a smaller one, so the budget
has to be divided between shapes. A shape holding C units of work on N
groups finishes at C/N, and the whole run finishes when the slowest shape
does — so the fastest split is the one where every shape finishes together.
Setting C/N equal across shapes and spending the whole budget gives N
directly, with no search.

Jobs are then placed longest-first, and instance size is a floor rather
than a match: a Chromium group will take a 5k job when it would otherwise
sit idle.

One consequence worth knowing before planning a run: `chromium-10-fan` —
ten destinations each pulling half a million files — costs more than every
other cell combined. Dropping it, or running it at fewer repeats, roughly
halves a full matrix.

---

## Vocabulary

| Term | Meaning |
|---|---|
| **Cell** | One configuration: corpus × agent count × direction × destinations. `chromium-10` is Chromium with ten agents to one destination; `chromium-10-fan` is the same source feeding ten. |
| **Job** | One cell executed on one host pair, running **both tools back to back in a randomized order**. The unit of scheduling. |
| **Repeat** | The whole set of jobs, run again. Statistics are computed across repeats. |
| **Group** | One source host followed by its destinations. Two machines for a pairwise cell, eleven for a fan-out. A group runs its jobs serially; groups run concurrently. |
| **Agent** | A thread simulating a developer's editor, rewriting files on a coding cadence. One agent per editing side also measures. |
| **Floor** | The harness's own contribution to a measured latency, measured per pair before any tool runs. |
| **Censored** | An edit that exceeded its deadline. Counted and held in the percentiles, never dropped. |

---

## What is actually measured

The measuring agent picks a file, generates its content, and **announces
the exact bytes it is about to write** to an observer process on the
receiving host. It then writes the file locally and waits for the observer
to acknowledge that precisely those bytes are readable at the destination
path. The interval between the local write completing and the
acknowledgement arriving is one sample.

Four properties make that number trustworthy:

**One clock.** Both timestamps are taken from the same monotonic clock on
the writing host. No cross-host clock skew can enter a latency sample, so
the measurement does not depend on NTP discipline between machines. Clock
offset *is* measured, but only to align resource windows, where a
sub-second error is irrelevant.

**Content, not events.** The observer acknowledges on a digest match, not
on a filesystem event or a size match. A tool that creates the file early
and fills it later, or stages and renames, cannot produce a false
acknowledgement — wrong or partial bytes hash wrong and the observer keeps
polling. Size is checked first only to avoid hashing a file mid-write.

**Upper bounds, with the overhead quantified.** A sample includes the
observer's verification read and the acknowledgement's return trip, so it
is strictly larger than the tool's own propagation time. The floor phase
measures that overhead by running the same arm/write/verify/acknowledge
protocol with the harness itself performing the write. It has consistently
measured **0.5–0.8 ms**, against results in the tens to thousands of
milliseconds.

**Non-replayable payloads.** Payload bytes come from a generator seeded
with a nonce unique to the run and job, derived by SHA-256 so it is
reproducible from the recorded value. Content from any earlier run cannot
satisfy this run's verification, which closes the hole where a stale
destination file makes a tool look instantaneous.

---

## The corpora

All four come from one Chromium checkout, cloned once at bake time and
recorded by commit SHA in `plan.json`:

| Corpus | Contents |
|---|---|
| `chromium` | The full checkout, ~505,000 files |
| `sub50k` | Whole top-level directories totalling ≈50,000 files — a tenth |
| `sub50k-b` | Different directories, disjoint from `sub50k` |
| `sub5k` | ≈5,000 files — a hundredth |

Symbolic links are deleted from the corpus at bake time. The two tools
have different symlink policies, which would make convergence ambiguous
without telling us anything about propagation speed. `.git` and `out` are
excluded from synchronization and from every manifest, so the tools are
never held responsible for content they were told to ignore.

Because the corpus is built once into the image, every pair in a run
synchronizes byte-identical trees.

---

## Partitions: what each agent may touch

Working sets are computed once at bake time by `benchmark partitions` and
shipped in the image, so every host agrees on them exactly.

The layout is `[measured A][measured B][pool A][pool B]`, drawn from a
complete, sorted walk of the corpus filtered to files between 256 B and
256 KB. The generator asserts two properties and refuses to write the file
otherwise:

- **The measured set is identical at every agent count.** The measuring
  agent edits the same 40 files whether 1 or 100 agents are running. Agent
  count therefore varies *load* and nothing else. In the earlier harness
  the working set shrank as agent count grew, which changed edit locality
  and made a tool look *faster* under ten times the load.
- **Everything is disjoint** — measured against background, background
  against background pairwise, and side A against side B entirely. Two
  agents writing one file would be a conflict, and a safe two-way mode
  would refuse it; that is a correctness test, not a latency measurement.

Generation also refuses a walk that recorded any error, so an incomplete
sampling frame can never produce "verified" partitions.

---

## A job, step by step

For each tool, in an order randomized per job:

1. **Destroy state** on both hosts — sessions, daemons, staging,
   installed remote agents, both release and `-dev` data directories —
   then *verify* the processes are gone and the directories no longer
   exist.
2. **Restore sources.** Every file any workload may edit is restored from
   a pristine copy baked into the image, so the second tool synchronizes
   exactly the bytes the first one did.
3. **Clear destinations.**
4. **Prepare storage.** Fault the corpus in once per host, then drop the
   page cache. Both tools then read a hydrated volume from disk. See
   [Lessons](#lessons-paid-for).
5. **Cold sync** until the destination's digests match the source's, with
   both the cheap count match and the digest-verified time recorded.
6. **Wait for quiescence**, verified by identical digests on both sides
   across a ten-second gap.
7. **Idle window** — sample resources with the tool running and nothing
   to do.
8. **Verify partitions** on both hosts, before the workload window opens,
   so this walk does not pollute the workload's resource attribution.
9. **Workload.** Agents edit for a fixed duration; the measuring agent
   records latencies.
10. **Reconvergence check** — the trees must agree again.
11. **Destroy state** again.

Failure at any step is recorded as a typed record, never a silent skip.

---

## Load

The workload is **open-loop**: the measuring agent issues edits on its own
cadence regardless of whether earlier edits have been acknowledged. A
closed loop would let a slow tool throttle its own offered load and then
report flattering latencies, which is precisely backwards.

Bounds and their reasons:

- **Up to 32 measured edits in flight.** Beyond that a tick is skipped and
  counted rather than queueing without limit. `skipped_ticks` appears in
  every latency row: it means the offered rate fell short, either because
  the bound was saturated or because all eight random file probes hit
  files with a verification already pending.
- **A file with an edit in flight is not re-edited**, so each pending
  verification watches a stable target.
- **Warmup is excluded and reported.** The first ten seconds of samples
  are recorded but kept out of the statistics, classified by the edit's
  true T0.
- **Background agents** edit their own disjoint sets on the same cadence
  and report achieved edit counts, write errors, and panics, so
  undelivered load is visible rather than assumed.

---

## Statistics

**Percentiles are computed over attempts, not successes.** Censored
edits — those exceeding the 120-second deadline — occupy the top
positions. A percentile landing among them is reported as over the
deadline rather than as a flattering finite number computed from the
survivors. The same rule applies to failed floor probes.

**Censoring is decided when the acknowledgement is matched**, on the
writer's clock. An acknowledgement arriving after the deadline is
censored, never converted into a fast sample by racing a sweep.

**Two views of central tendency.** Pooled percentiles over every raw
sample across repeats are the primary figure; the median of per-run
medians, with its spread, is reported beside it. The spread is the honest
signal of whether a result is machine-dependent — across twenty repeats on
fifteen machine pairs, per-run medians have clustered within 1.5%.

**Tainted runs are excluded and named.** A tool-run is tainted by a failed
reconvergence, an unverified cold sync, a workload error in *any*
direction, an idle window that never settled, or background write errors
and panics. Tainted runs contribute neither latency nor resources, and
appear in the report under `tainted_runs` with the reason.

**Delivered is reconciled against planned.** The full plan is persisted
before anything runs. A job that never reported is listed as missing
rather than silently absent from an average.

---

## Resources

A sampler tracks each tool's whole process tree once per second:
pattern-matched seeds plus every transitive descendant via `/proc` PPid
chains, so transport children and remote agents count even though their
argv never mentions the tool. CPU is a monotone cumulative total — a
process that exits banks its last observed jiffies into a persistent
base — so windowed differences stay valid across process churn.

Patterns are **comma-separated alternatives**, because tools disagree
about how they present themselves in `/proc`: a daemon may re-exec with a
bare basename rather than the path it was launched from. A single pattern
is one wrong guess away from sampling nothing.

Figures are windowed per phase from the timestamped series, with the CPU
baseline taken from the last sample *before* the window (so startup work
inside the window is counted) and remote timestamps corrected by the
measured clock offset. The workload window is bounded by the offered-load
window the agents themselves report, not by process lifetime, so a
tool-dependent drain does not stretch it.

A series in which no process was ever matched is reported as
`resource_sampling_never_matched` and excluded — a confident zero is worse
than a missing number.

---

## Integrity invariants

Each answers a specific way an earlier version of this benchmark was
wrong.

1. **Agent count varies load only** — the measured working set is fixed.
2. **Partitions are precomputed from a complete walk and asserted
   disjoint**, within and across sides.
3. **Both tools run on the same pair, in an order randomized per job**, so
   machine identity and ordering cancel instead of confounding.
4. **Convergence is content-verified**, and only a walk with zero errors
   may certify it — two walks failing identically must not read as equal.
5. **Resources are windowed per phase**, never lifetime peaks captioned as
   per-phase ones.
6. **The floor is measured** per pair, before any tool runs.
7. **Warmup is excluded and reported**, classified by true T0.
8. **Timeouts are censored data**, holding their percentile positions.
9. **Every record carries provenance** — schema, run, pair, job, cell,
   repeat, tool, tool versions, binary SHA-256s, corpus commit, and phase
   timestamps.
10. **State is destroyed between tools and between jobs**, and the
    cleanliness is checked rather than assumed — processes *and*
    directories.
11. **Load is open-loop**, with payloads seeded per run so no earlier
    run's content can satisfy a verification.
12. **Process hygiene cannot kill the harness.** Cleanup matches exact
    process names or bracketed full paths, never a bare substring — the
    job spec itself contains both tools' names — and local test mode
    touches nothing but the toy subject.
13. **A measurement that could not be taken is reported as missing**,
    never as zero.

---

## Lessons paid for

Two defects produced numbers that looked entirely reasonable. Both were
caught before publication, and the guards against them are now part of the
harness.

**Cold sync measured the storage, not the tools.** A volume restored from
a snapshot faults its blocks in lazily. Whichever tool ran *first* in a job
paid a large one-time cost — 434 s on Chromium — and whichever ran
*second* read from page cache and paid 125 s, regardless of which tool it
was. The artifact scaled exactly with corpus size (+309 s Chromium, +17 s
40k, +1.3 s 4k), which is the signature of block hydration rather than of
anything either tool does. Randomized tool order meant it averaged out in
expectation, but at three repeats the draw was uneven enough to bias the
result. Step 4 of the job sequence is the fix.

**A tool's memory read as a confident zero.** One tool re-execs its daemon
with a bare basename, so a full-path process pattern matched nothing and
every memory figure for it reported 0 MB — not a gap in the data, but a
plausible-looking zero that would have been published. Hence alternative
patterns, and hence the aggregator refusing an all-zero series.

The general lesson is the one both share: a measurement that silently
degrades to a believable number is more dangerous than one that fails
loudly, so the harness now prefers to fail loudly.

---

## Known limits

- Latencies include observer verification and one network return trip by
  design. They are upper bounds; the floor phase quantifies the overhead.
- The floor is sequential, while a workload can hold up to 32
  verifications in flight per direction. The floor does not characterize
  that concurrency. The observer bounds it instead: each pending
  verification polls at 500 µs for its first second, then backs off to
  1 ms — about 1,000 polls per second per pending verification, so up to
  ~32,000 per second per direction at full capacity. Worst-case added
  detection delay after backoff is one slow interval, ~1 ms.
- The open loop is bounded at 32 in-flight edits, so a sufficiently slow
  tool receives less offered load. `skipped_ticks` records it in every
  row. Those runs are *not* discarded: skipping correlates with slowness,
  and discarding them would delete the censored counts that evidence it.
- First-sync times at Chromium scale are dominated by device IOPS for both
  tools, so that measurement has little power to separate them.
- Cold-sync completion polling has 5 s granularity; digest verification
  adds a bounded, recorded delay after the count first matches.
- Comparisons across separate runs are weaker than comparisons within a
  job. A before/after on one tool is best done by staging both builds and
  running them as two entries in the same job, which the design supports
  and which pairs them on one machine.

---

## Files

| Path | Role |
|---|---|
| `harness/` | **The measurement plane, in Rust, as one static binary** (`benchmark`): tree walks and manifests, partition generation and verification, the observer, the agent workload, the floor, and the resource sampler. Compiled so both hosts run byte-identical instruments, the walk rules exist in exactly one place, a hundred agents are threads in one small process, and the observer can poll at 500 µs. |
| `harness/src/walk.rs` | The single definition of what counts as a synchronizable file, with every failure class counted. |
| `harness/src/partitions.rs` | Working-set generation and the disjointness assertions. |
| `harness/src/observer.rs` | Destination-side verifier: per-request worker threads, replies multiplexed by sequence. |
| `harness/src/agents.rs` | The open-loop workload, the measuring agent, censoring, and the floor protocol. |
| `harness/src/sampler.rs` | Process-tree resource sampling. |
| `job.py` | Runs one job on host A: the eleven steps above, emitting JSONL with full provenance. Supports a local mode for the smoke test. |
| `orchestrate.py` | AWS lifecycle: bake, launch, wire and *verify* observers, persist the plan before dispatch, collect, destroy. |
| `aggregate.py` | JSONL to tables: pooled percentiles with censoring, per-run spread, windowed resources, delivered-vs-planned, taint rules, problems. |
| `toysync.py` | A deliberately dumb local subject (a copy loop) with known behavior, for the smoke test. |
| `smoke.sh` | Local end-to-end test of the harness itself. Run after any change. |
| `report/` | The published report. |
| `results-*/` | Raw JSONL per run, plus the plan and per-pair driver logs. |

## The local gate: `bench/ab.sh`

The matrix above is the benchmark of record and needs a pair of EC2
hosts. The gate that every hot-path change passes before it ships is
smaller, runs on one machine in about ten minutes, and lives here too:

    bench/ab.sh target/release/autobahn-before target/release/autobahn-after

It runs the two binaries in interleaved legs over a generated 40,000-file
corpus (`bench/corpus.py`), each leg a cold sync and then a window of
simulated editing agents, and reports p50/p90/p99 side by side with a
verdict against the run-to-run spread. Interleaving is what makes it
honest on a shared machine. The raw reports of every gate run that
shaped a decision are in `bench/ab-reports/`.

Three lessons are built into the script rather than left to be
relearned: processes are tracked by PID, because a variant binary with a
different name once survived every cleanup and contaminated the next
leg; the harness is rebuilt if the one present cannot execute, because a
build tree synchronized from another platform leaves one that cannot;
and one leg each yields no verdict, because it measures nothing about
the machine's own variance.
