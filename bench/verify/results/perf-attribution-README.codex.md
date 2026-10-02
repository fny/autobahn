# Source CPU attribution and fan-out contention

This historical record applies to the measured setup. See [Current benchmarks](../../../docs/benchmarks.codex.md) for later results.

The payload-cache review in `docs/reviews/supply-sharing-review-fable.md` requested instruction/cycle attribution and a contention check.

## Measurement limits and substitute

The intended `perf stat` comparison used instructions and cycles per destination. Flat instruction counts with rising cycles could indicate contention. Rising instruction counts could indicate additional work.

These virtualized EC2 instances exposed no hardware PMU. `perf stat -e instructions,cycles` returned `<not supported>`. `/sys/bus/event_source/devices/` contained only software, tracepoint, and breakpoint sources.

The smallest available bare-metal option had 96–128 vCPUs, which would change the resource constraint under investigation.

Instead, the experiment resized the source from `c6i.2xlarge` with eight vCPUs to `c6i.8xlarge` with thirty-two. Stop/start preserved the private IP, allowing the same four destinations and configuration.

## Symbol attribution

A width-four cold sync of 62,952 files produced 15,000 samples with `perf record -F 999 -e cpu-clock -g`.

| process | share |
|---|---|
| autobahn | 76.4% |
| ssh | 22.4% |
| inotify thread | 1.2% |

The following symbols are shares of all samples:

| symbol | share |
|---|---|
| `lz4_flex::compress_internal` | 20.4% |
| `mux::Shared::send` (bincode encode + frame assembly, inlined) | 13.1% |
| `lz4_flex::count_same_bytes` | 7.2% |
| `rep_movs_alternative` (kernel copy) | 2.6% |
| `blake3::avx2::hash8` | 2.0% |
| `memcpy` | 1.7% |
| malloc + free | 1.9% |

LZ4 compression totaled 27.6% of source CPU. Relative to Autobahn CPU alone, compression was about 36% and frame assembly about 17%.

Together, they accounted for about half the per-destination work on identical input. An independent local method estimated 1.61 of 3.27 seconds, or 49%.

The real corpus reversed the synthetic result. Repeated random 4 KiB blocks gave LZ4 cheap long matches, yielding 0.72 seconds encoding and 0.29 seconds compression. Future attribution needs representative content.

The review proposed caching uncompressed bincode as a simpler first step. These measurements favored caching compressed payloads because compression was the larger cost. That prepared path must bypass outer compression rather than attempt to compress the same bytes again.

SSH used 22.4% of total source CPU. The fan-out sweep measured only Autobahn’s `/proc/<pid>/stat`, so this SSH cost was additional to its 3.27 seconds per destination.

Encryption and egress remain per-destination work.

## Contention: eight versus thirty-two vCPUs

Only the source instance size changed:

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

The increase in marginal CPU fell from +1.30 to +0.34 seconds, a 74% reduction. Width-four CPU fell 22%, while width one changed about 8%.

This supports contention as the main cause of the rising marginal. The remaining increase can reflect shared memory bandwidth, the connection writer mutex, or other costs. The experiment did not isolate those components.

Removing repeated CPU work can also reduce contention for the work that remains.

Elapsed time stayed near fifty seconds at every width on both sources. Extra CPU capacity did not reduce completion time for this roughly 520 MB transfer.

The original analysis attributed the roughly 10 MB/s rate to the network. The separate [limits experiment](limits-README.codex.md) identified destination file creation as the constraint on its comparable corpus. The CPU sweep alone establishes that source CPU was not the elapsed-time bottleneck.

## Implications for payload caching

Caching can reduce source CPU and provide capacity for more sessions. These runs did not show an elapsed-time benefit.

It is more likely to affect completion on a CPU-constrained laptop or small VM, or with faster transport and destination storage.

The frame-assembly change `c7bdd3b` had already reduced send-path cost by 38%. The cache proposal required additional evidence about the active constraint.
