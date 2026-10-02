# Two gates before building the ancestor journal

This historical record applies to the measured implementation. See [Current benchmarks](../../../docs/benchmarks.codex.md) for later results.

Reviewers requested two experiments before journal implementation. They used one `c6i.4xlarge` source/destination pair and a pre-seeded Chromium corpus with 504,960 files.

Script: `bench/verify/gates.sh`.

## Gate 1: waiting for ancestor persistence

The measured cycle updated beta before persisting the ancestor. An isolated edit did not wait for that final write. A later edit could arrive while the worker was still occupied.

The experiment measured isolated saves and pairs spaced 100 ms apart in the same session:

| | n | min | p50 | max |
|---|---|---|---|---|
| isolated save | 10 | 199.1 ms | **205.0 ms** | 307.1 ms |
| second of two, 100 ms apart | 9 | 437.9 ms | **480.7 ms** | 518.6 ms |

The second save’s median was about 276 ms higher. This established that ancestor persistence affected an ordinary sequence of edits.

The 205 ms isolated median also exceeded the roughly 47 ms local-phase projection. The local table omitted destination scanning, transfer, and application.

The consecutive-save penalty exceeded the projected 179 ms because the earlier local test used an SSD rather than EBS.

## Gate 2: sharing among reconciliation inputs

A probe examined the three input relationships on every reconciliation:

```
[sharing] ancestor-alpha 0.0%  ancestor-beta 0.0%  alpha-beta 0.0%
```

All fifty reconciliations reported zero sharing.

The earlier 2,499/2,501 result came from `cycle_cost.rs` passing a local scan as two reconciliation arguments. It measured incremental rescan sharing, not sharing among independent production inputs.

Codex identified the aliasing, and fable predicted the zero-sharing result. At this implementation stage, the ancestor came from disk decoding and beta from wire decoding. Neither shared allocations with other inputs.

The probe was removed after the experiment.

## Decisions

Build the ancestor journal to address the measured 276 ms delay.

Include incremental ancestor validation. `apply()` preserved unchanged subtrees between consecutive ancestor versions: 599/601 shared directories in that probe.

Do not implement the proposed cross-input reconciliation pruning based on these measurements. Its required pointer sharing was absent.
