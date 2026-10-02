# What scales with tree size on the latency path

This historical record describes an earlier implementation. See [Current benchmarks](../../../docs/benchmarks.codex.md) for later results.

Single-edit latency was 45.7 ms at 6,636 entries and 61.8 ms at 62,952. The roughly 16 ms increase motivated a breakdown of work proportional to tree size.

`examples/cycle_cost.rs` measures local source CPU and disk work without network or fan-out. It scans a corpus, edits one file, rescans, and times whole-tree phases.

## Result

| entries | rescan | reconcile | validate | encode | write | total | ancestor |
|---|---|---|---|---|---|---|---|
| 5,089 | 0.4 | 0.4 | 0.1 | 0.6 | 0.3 | 1.7ms | 0.4 MB |
| 20,141 | 0.7 | 1.5 | 0.4 | 1.8 | 0.9 | 5.4ms | 1.6 MB |
| 63,601 | 1.7 | 4.6 | 1.9 | 8.3 | 8.0 | 24.4ms | 5.1 MB |
| 502,501 | 10.0 | 37.3 | 13.8 | 54.2 | 111.3 | **226.6ms** | **40.6 MB** |

The 63k total was consistent in scale with the network experiment’s additional cost.

As of 2026-09-24, production cycles append ancestor changes to a journal. The table measures the earlier full synchronous ancestor write, and the example labels that distinction.

Without encode and write, the totals are 0.9, 2.6, 8.2, and 61.1 ms.

At Chromium scale, the measured total was 226.6 ms with a 40.6 MB ancestor. A projection from smaller trees gave about 145 ms and underestimated write cost.

The write column depends strongly on storage and cache state. Encode cost was more stable.

## Cost breakdown

At 502,501 entries:

- Ancestor encoding and writing used 165.5 ms, about 73%.
- Reconciliation used 37.3 ms, about 16%.
- Validation used 13.8 ms, about 6%.
- Incremental rescan used 10.0 ms, about 4%.

The local snapshots shared 2,499 of 2,501 directory allocations after one edit. `tree::nodes_share_storage` exposed that sharing, and `tree/diff.rs:32` already used it.

This initially suggested a reconciliation shortcut. The later [gate experiment](gates-README.codex.md) showed that production reconciliation inputs did not share those allocations.

## Proposed work at the time

Replace whole-ancestor rewriting with a synchronous change journal and periodic compaction. This targets the largest cost while preserving provenance durability. `session/mod.rs:415` explains why stale ancestor history can overwrite deliberate reverts.

Investigate reconciliation pruning only after checking sharing among real inputs. `ScanUnchanged` reused a previous remote snapshot’s Arcs (`endpoint/remote.rs:187`), but that did not establish sharing with the ancestor.

Validate only changed ancestor subtrees where same-lineage sharing proves that an earlier validation still applies.

The journal required its own durability design and review. Reconciliation and validation pruning used related pointer techniques but required different sharing evidence.
