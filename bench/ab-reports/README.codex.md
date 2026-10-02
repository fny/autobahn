# A/B reports

These raw reports support the [historical 0.3.0 currency analysis](../../docs/benchmarks-0.3.codex.md) and performance claims in hot-path commits.

They compare Autobahn builds on one machine. The separate EC2 benchmark compares Autobahn with mutagen.

Most files contain `bench/harness` `agents` results for a 63,000-file corpus and ten simulated editors. The `n6`/`n8` series uses a different setup described later.

| Prefix | Result |
| --- | --- |
| `preA` / `phaseA` | Intent records added no measurable latency. |
| `preC` / `phaseC` | Observer and staging fixes added no measurable latency. |
| `preF` / `fixes` | Syncing intent cost about 6 ms p50. `fixes1`–`fixes2` synced every cycle. `fixes3`–`fixes5` used the shipped remote-only rule. |
| `n6` / `n8` | The watcher upgrade was rejected. `n8a` and `n8d` reached p99 5,703 ms and 6,158 ms. `n6` remained at or below 125 ms. |
| `dpreF` / `dhead` | Five interleaved pairs showed p50 52.9 ms before and 53.5 ms after. Cold sync rose from 7.6 s to 8.8 s after publication rehashing. |

An unrelated mutagen sync ran during `dpreF`/`dhead` and consumed about 8% of an eight-core machine. Cold-sync values of 25.2 s and 34.1 s were excluded as cache/contention outliers.

Stray supervisors holding session locks contaminated `preA3` at 75.8 ms and `phaseA4` at 72.2 ms. Clean reruns support the stated conclusions.

The `n6`/`n8` series used macOS and 6,000 files. Other series used Linux and 63,000 files. Compare within a series, not across these environments.
