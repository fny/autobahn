# Where the source's CPU actually goes, and whether the rising marginal is contention

Two measurements requested in the payload-cache review (`docs/reviews/supply-sharing-review-fable.md`).

## What could not be measured, and the substitute

Fable asked for `perf stat` comparing **instructions** against **cycles** per destination: flat instructions with rising cycles would mean SMT or memory-bandwidth contention, while rising instructions would mean genuinely superlinear work.

That test cannot be run on these instances. Virtualized EC2 exposes no hardware PMU — `perf stat -e instructions,cycles` reports `<not supported>`, and `/sys/bus/event_source/devices/` lists only software, tracepoint, and breakpoint sources. A bare-metal instance would expose the PMU, but the smallest metal option has 96–128 vCPUs, which removes the very contention being tested.

The substitute answers the same question with wall-clock and CPU time alone: **run the identical sweep on a source with four times the CPUs.** If the rising marginal is contention it must flatten; if it is superlinear work it cannot. The source was stopped, resized from `c6i.2xlarge` (8 vCPU) to `c6i.8xlarge` (32 vCPU), and restarted — the private IP survives a stop/start, so the same four destinations and the same wiring were reused with nothing else changed.

## Symbol attribution (width 4, `perf record -F 999 -e cpu-clock -g`)

15,000 samples over one width-4 cold sync of 62,952 files. By process:

| process | share |
|---|---|
| autobahn | 76.4% |
| ssh | 22.4% |
| inotify thread | 1.2% |

Within autobahn, as a share of all samples:

| symbol | share |
|---|---|
| `lz4_flex::compress_internal` | 20.4% |
| `mux::Shared::send` (bincode encode + frame assembly, inlined) | 13.1% |
| `lz4_flex::count_same_bytes` | 7.2% |
| `rep_movs_alternative` (kernel copy) | 2.6% |
| `blake3::avx2::hash8` | 2.0% |
| `memcpy` | 1.7% |
| malloc + free | 1.9% |

**LZ4 compression is 27.6% of all source CPU — the single largest item, and larger than encoding.** Expressed against autobahn's own CPU (the metric the fan-out sweep reported), compression is ~36% and frame assembly ~17%, so about half of the per-destination cost is spent on byte-identical input. That agrees with the independent local attribution (1.61s of 3.27s, 49%), reached by a completely different method.

Two things this corrects:

- **The local harness understated compression.** Its synthetic corpus repeated a 4 KiB random block, which LZ4 resolves with long cheap matches; it reported encode 0.72s against compress 0.29s. On the real corpus the order reverses. Any future attribution work should use the real corpus.
- **It inverts the review's sub-choice.** The review suggested caching *uncompressed* bincode and letting each session compress, as the simpler start that captures "0.72 of the 1.0". On real data that captures the smaller half. Caching *post-compression* payloads is the bigger prize, and the review's warning still applies: the prepared path must then bypass the outer compression attempt rather than re-compressing incompressible bytes.
- **ssh is 22.4% and is not shareable.** Note the fan-out sweep's "source CPU" came from `/proc/<pid>/stat` for the autobahn process only, so ssh's share sits on top of the 3.27s per destination rather than inside it. Together with egress, this is the true per-destination floor that no cache can touch.

## Contention: the same sweep on 8 vCPUs and on 32

Identical corpus, identical four destinations, identical script. Only the source's instance type changed.

| width | 8 vCPU CPU s | 32 vCPU CPU s | 8 vCPU elapsed | 32 vCPU elapsed |
|---|---|---|---|---|
| 1 | 4.3 | 4.0 | 50.8 | 52.0 |
| 2 | 6.9 | 6.1 | 52.4 | 53.4 |
| 3 | 10.2 | 8.5 | 50.6 | 50.1 |
| 4 | 14.1 | 11.0 | 51.3 | 50.3 |

| source | least-squares fit | marginals | drift |
|---|---|---|---|
| 8 vCPU | 0.70 + 3.27 x width | 2.6, 3.3, 3.9 | **+1.30** |
| 32 vCPU | 1.54 + 2.35 x width | 2.16, 2.37, 2.50 | **+0.34** |

**The rising marginal is mostly contention.** Quadrupling the source's CPUs collapsed the drift in the marginal from +1.30 to +0.34, a 74% reduction, and cut width-4 CPU by 22% while width-1 moved only 8%. A per-session cost that were genuinely superlinear in width could not shrink by adding cores; one that comes from four sessions and four ssh processes competing for four physical cores must. The residual +0.34 says a small superlinear term survives — four sessions on 32 vCPUs still share memory bandwidth and the connection's writer mutex — but it is minor next to what the cores removed.

This is the conclusion the review wanted from the instructions-versus-cycles split, reached another way: the answer is "contention," which means removing CPU work relieves the remaining work superlinearly. Every CPU-second a payload cache removes also de-contends the sessions still running, so its benefit on a CPU-constrained source is larger than its own arithmetic suggests.

**And the decisive negative result: 32 vCPUs bought no wall clock at all.** Elapsed was ~50s at every width on both sources. Four times the CPU changed nothing a user waits on, because this cell moves 520 MB in ~50s — roughly 10 MB/s, set by the link, not the processor. That confirms the review's network-bound diagnosis directly rather than by inference.

## What this means for the payload cache

The case is narrower than the CPU numbers alone suggest, and the review had it right: **sharing buys headroom, not makespan**, until either the links get fast or the source gets small. It pays on a source whose CPU is the binding constraint — a laptop, a small VM, a same-rack or fat-NIC fan-out where the transfer window collapses — and buys nothing measurable on a source like this one, where the network is the wall.

The frame-assembly fix already shipped (commit `c7bdd3b`, 38% off the send path) needed no such precondition, which is the argument for having done it first.
