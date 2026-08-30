# Plan: closing the five open risks

Follows from PLAN.md's open-risks section. Ordered so that the two items
sharing an on-disk format change land together, the cheap independent
item rides between the big ones, and every hot-path change is A/B
measured before it is committed — analysis estimates were wrong every
time they were tried; the ten-minute local harness was not.

## Phase A — intent records and journal hardening (risks 1 and 4)

One phase because both change the journal record format, and the format
should break once, not twice. The compatibility epoch goes to 2 with it;
pre-existing dev journals fail decode closed with a clear message, which
is acceptable before any deployment exists.

**A1. Record format.** The journal payload becomes a typed entry:

    enum JournalEntry {
        /// The changes a completed cycle achieved (today's payload).
        Achieved(Vec<Change>),
        /// The paths a cycle is about to mutate, written before the
        /// first endpoint transition.
        Intent { paths: Vec<String> },
    }

Record and checkpoint digests widen to cover the full record — generation
and length included — closing the undetected-header-bit-flip gap: a
flipped generation currently makes valid records skippable and
normalizable away.

**A2. Intent semantics.** Before any endpoint transition, the session
appends `Intent` carrying the union of alpha- and beta-transition paths
at the current generation. The achieved record that completes the cycle
consumes it. On open, an intent *not* followed by an achieved record at
its generation is unresolved: the store surfaces its paths, and the
session removes them from the in-memory ancestor before the first
reconcile. Provenance for those paths is honestly "unknown":

- if the transition landed and the user then deliberately reverted, the
  two sides differ with no ancestor — two-way-safe surfaces a conflict
  instead of silently overwriting the revert (the whole point);
- if both sides agree (the transition landed, nothing was reverted, or
  it never landed and nothing changed), reconciliation re-records the
  agreement and the taint evaporates;
- the acknowledged cost: a crash mid-cycle can turn what would have been
  a clean propagation into a surfaced conflict. Noise on crash recovery
  is the price of never resolving that ambiguity destructively.

Unresolved intents from repeated crashes union. Normalization preserves
an unresolved trailing intent — it is information, not debris.

**A3. Optional power-loss durability.** A `durability = "power"` session
option makes `record()` sync each append. Off by default; the point is
that the choice becomes the user's instead of the design's. Compaction
is already fsync-ordered regardless.

**A4. Proof.** Extend the crash-point enumeration: `Intend` joins the
op alphabet; cuts between intent and achieved must reopen with the
intent surfaced; cuts elsewhere must not surface anything. Hand-model
the power-loss states compaction's sync barriers permit and pin each
one, as the reset and normalization states already are. A session-level
seam (`#[cfg(test)]` fail-after-transition) proves the revert scenario
ends in conflict, not overwrite.

**A5. Measure.** The intent append adds one small write to every
mutating cycle. A/B on the 63k local harness before committing; the
budget is zero measurable p50 movement (the append is micro-seconds,
but that is an estimate, and estimates lose).

## Phase B — the `--checksum` escape hatch (risk 5)

A control-socket verb (`autobahn verify`, wired like pause/resume) sets
a once flag on the session; the next scan disables digest reuse and
re-reads every file. Content that changed without its metadata moving —
forged timestamps, the retained residuals — becomes visible to that scan
and flows through ordinary reconciliation. Files whose digest changed
under matching metadata are logged loudly, because each one is evidence
of the exact class the metadata gate cannot see. Periodic scheduling can
come later; the verb is the escape hatch. Small, independent, an
afternoon with tests.

## Phase C — the three missing harnesses (risk 2)

In value order:

**C1. Observer generation protocol.** Deterministic state-machine test:
two simulated sessions driving one `RootObserver` with a fake watcher,
enumerating interleavings of scan-start, publish, invalidate,
filesystem write, event delivery, baseline offer, and distrust. The
invariant: no snapshot is ever served as current across a generation it
did not observe, and a transition's lease always reflects the scan its
transitions were reconciled from.

**C2. Staging and transition lifecycle.** In-process fault harness over
the local fixture: inject faults at every boundary — after stage_begin,
mid-receive (truncate the staged file), between deletion-first and
creation, after transition before fold — then rerun to quiescence and
assert: convergence, no wrong digest on disk (manifest-verified), every
target path old/new/reported, ancestor equal to an acknowledged outcome.

**C3. Reconnect.** A byte-cutting proxy between controller and a real
agent process: record a canonical exchange, then cut at every frame
boundary, reconnect, rerun, and assert convergence with no response
from the dead connection satisfying a new request — and measure how
wide the remote half of the intent window actually is.

## Phase D — the independent review (risk 3)

The deliverable this side can produce is `INVARIANTS.md`: every
invariant the system claims, stated precisely, with the code that
enforces it and the test that checks it — scan reflects a point in
time, ancestor means both sides agreed, digest identifies content, a
path resolves once, one owner per pair of trees, provenance survives
policy. An external reviewer (human, or at minimum a fresh lineage with
different framing) then attacks the *invariants*, not the diff. This is
deliberately last: phases A–C change what the invariants document says.

## Order and shape

    A (intents + format + durability + proof + A/B)   — one full session
    B (verify verb)                                   — small
    C1, C2 (observer, staging harnesses)              — one session
    C3 (reconnect proxy)                              — half session
    D (INVARIANTS.md, external handoff)               — small + external

Each phase ends the standard gate: full suite, clippy, fmt, crash
scripts, smoke, and — for A — the latency A/B.

## Status

All phases are complete. A and B landed as the intent-record and
verify-verb commits; C1's interleaving sweep found two generation-
protocol holes (fixed and mutation-checked before it first passed);
C2 moved the intent window off the staging phase so mid-transfer
crashes recover conflict-free; C3 measured the remote intent window at
3 of 29 frame-boundary cuts. D produced `INVARIANTS.md`. Hot-path
changes were A/B measured flat (p50 51.9/52.0 before, 52.2/50.7
after).
