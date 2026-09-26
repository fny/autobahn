# BENCH-5: Aggregation and examples report what they claim

**Findings:** M-51 (ASTRA F31), M-55 (ASTRA), M-56 (ASTRA).
**Status:** proposed. This is a fault under the bench standard: it produces wrong numbers.

## Problem

- **Contaminated jobs count toward cold sync (M-51).** `bench/aggregate.py` finds tainted jobs (`find_tainted`, `:57`). One kind is a job whose destination count does not match its cell, the fan-out width leak in the six contaminated jobs of bench-1789947877. Latency and resource aggregation leave those jobs out, but cold-sync aggregation (`:255`) does not. So a contaminated job that eventually verified still contributes a headline cold-sync time to the wrong cell.
- **CPU totals can go down (M-55).** `bench/job.py` sums cumulative CPU samples in rounded time buckets, without carrying each host's latest value forward. A host missing from one bucket makes the total dip, which distorts the CPU figure for that window.
- **The cycle-cost example overstates (M-56).** `examples/cycle_cost.rs` counts a full synchronous ancestor serialization and write in its reported total. Production cycles use the journal instead.

## Proposed resolution

- **One exclusion rule.** Every aggregation calls one `excluded(job, tool)` function built from `find_tainted`, so cold sync, latency and resources all drop the same jobs. The report lists excluded jobs with their reason, as latency already does.
- **Per-host deltas for CPU.** Compute each host's CPU change over the window from its own first and last samples, then sum across hosts. Buckets are no longer needed.
- **Label the example.** Add a comment in `cycle_cost.rs`, and a line in its output, saying that the total includes a full ancestor write that production does not do. If the number is used anywhere, report the ancestor write separately.

## Tests

- An aggregation fixture holds one job that verified its digest but had the wrong destination count. That job is absent from cold-sync results and listed as excluded.
- A CPU fixture with one host missing from a middle bucket gives the same total as with the bucket present.
- Re-run `aggregate.py` over bench-1789947877 and confirm the six known contaminated jobs are excluded from cold sync.
