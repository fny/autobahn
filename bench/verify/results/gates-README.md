# Two gates before building the ancestor journal

Both reviewers asked for these before any work started. Run on one
source/destination pair of `c6i.4xlarge`, against the Chromium corpus
(504,960 files), with the destination pre-seeded so the session begins
converged rather than paying a cold sync.

Script: `bench/verify/gates.sh`.

## Gate 1: does anyone wait for the ancestor tail? — YES

The cycle transitions beta *before* it persists the ancestor, so the tail
never delays an isolated save. It does occupy the worker, so a second save
landing inside that window waits for it. Ten of each, same session:

| | n | min | p50 | max |
|---|---|---|---|---|
| isolated save | 10 | 199.1 ms | **205.0 ms** | 307.1 ms |
| second of two, 100 ms apart | 9 | 437.9 ms | **480.7 ms** | 518.6 ms |

**A second save costs 276 ms more than an isolated one.** That is the tail,
and it is squarely perceptible. The gate passes: the ancestor rewrite has a
waiting user in an ordinary editing rhythm.

Two things worth recording against earlier estimates:

- An isolated save at this scale is **205 ms**, not the ~47 ms projected from
  the local phase table. The local figure counted only source-side rescan and
  reconcile; the real path also carries the destination's own scan of 505k
  entries, the transfer, and the apply.
- The consecutive-save penalty is **276 ms**, larger than the 179 ms the local
  measurement predicted, because that measurement wrote the ancestor to a
  local SSD rather than EBS.

## Gate 2: do the reconcile inputs share storage? — NO, zero

The sharing probe ran on every reconcile of a real session, reporting the
three relationships a three-way walk could prune by:

```
[sharing] ancestor-alpha 0.0%  ancestor-beta 0.0%  alpha-beta 0.0%
```

**Identical across all 50 reconciles.** Not rare — zero.

This settles the question permanently. The 2,499/2,501 figure that motivated
reconcile pruning was an artifact of `cycle_cost.rs` passing one local scan as
two of reconcile's three arguments; it measured how well an incremental
rescan preserves storage against the previous scan, which is a different
question with a different answer. Codex caught the aliasing, fable predicted
the production result would be ~0 on all three, and it is.

The probe was removed once it answered, since the answer will not change:
the ancestor is decoded from disk, beta is decoded from the wire, and neither
decoding shares allocations with anything.

## What this authorises

- **Build the ancestor journal.** 276 ms, measured, with a waiter.
- **Take incremental validation with it.** Its sharing is same-lineage —
  `apply()` preserves untouched subtrees, so previous-ancestor against
  next-ancestor is 599/601 — and so is unaffected by this result.
- **Do not build reconcile pruning.** The pointers it needs do not exist.
