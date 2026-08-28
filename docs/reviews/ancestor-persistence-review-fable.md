# Ancestor persistence & reconcile pruning: adjudication

Third opinion on /tmp/cycle-brief.md and /tmp/codex-cycle.md, verified against the
code at HEAD (2026-08-28). Codex's two corrections both check out; my main additions
are (a) the sharing problem is worse than codex says — it applies to alpha too, not
just beta, which changes the Q3 ranking, and (b) the brief's headline number
conflates two different latencies, which changes the Q4 answer.

## Verified facts the adjudication rests on

- **Codex's durability correction is right.** `save_ancestor` is `fs::write` +
  `fs::rename` with no `sync_all` on the temp file and no fsync of the containing
  directory (src/session/mod.rs:678–684). It is process-crash-safe, not
  power-loss-safe. (Nuance: ext4's `auto_da_alloc` heuristic flushes data before a
  rename-over-existing commits, so in practice on default ext4 the hole is small —
  but that is a filesystem heuristic, not a contract, and XFS/btrfs make no such
  promise.)
- **Codex's benchmark critique is right.** examples/cycle_cost.rs:65–69 passes
  `settled.root` as both ancestor and beta, and the 2,499/2,501 sharing figure at
  :82 compares `settled` against `edited` — two generations of one local scan
  lineage. It measures what `diff()` could prune, not what `reconcile()`'s three
  inputs share.
- **Cycle ordering** (src/session/mod.rs:363–441): stage and transition — the point
  where the edit lands on beta — happen *before* ancestor validate/encode/write.
  This matters enormously for Q4.
- **Reconcile's ancestor changes are slim**: in the agree-but-ancestor-disagrees
  case it emits directory nodes with *emptied* children
  (src/tree/reconcile.rs:112–127). So directory-valued ancestor updates from
  reconciliation never graft a scan's `Arc`s.

## Q1: Journal, or go straight to Merkle/CAS?

**Journal. Codex is right, and the Merkle store is worse than "more machinery" —
it buys nothing user-visible over the journal.** The journal makes the durable
write proportional to the edit (a length-framed, checksummed, generation-stamped
record of the cycle's `ancestor_changes` + achieved changes, replayed via `apply()`
which is deterministic and order-preserving — replay must keep record order, since
reconcile emits parent-before-child slim-dir changes that `apply()` resolves in
sequence). A content-addressed node store with an atomic root pointer also makes
writes proportional to the edit, but adds: garbage collection or repacking of dead
nodes; a cold-load path that either does random I/O over ~500k small blobs or
reinvents packing; per-node hashing on every fold; and a larger fatal-corruption
surface under the existing "corrupt ancestor is fatal, never a reset" policy
(src/session/mod.rs:689–701). The `Arc` CoW trees make the *in-memory* sharing
free, but nothing about the on-disk CAS is free. Revisit only if journal compaction
ever measurably hurts — it won't, because compaction is exactly today's full write
moved off the latency path and run occasionally instead of per-edit.

**One decoupling codex missed: fsync is orthogonal to the journal.** Since today's
write is not power-loss-durable, a journal *without* fsync is the true like-for-like
replacement — same durability class as today, ~1ms appends instead of 165ms
rewrites. fsync-per-record is an optional durability *upgrade* with its own price
(a few ms per cycle on a good disk, much more on busy EBS), and it should be
benchmarked and decided separately, not bundled. Codex framed the fsync as part of
the journal design; I'd ship the journal first in today's durability class and make
the fsync a follow-up decision with its own measurement.

Also worth stating: the journal ends the async-write debate by dominating both
positions. My own 2026-08-26 review argued a behind-only, ordered, join-before-
lock-release async writer was acceptable; your brief forbids async; codex says a
volatile fence just moves the revert hole to crash-restart. The journal gets the
latency async would have bought with a *smaller* crash window than today's
synchronous write — because today's window, in which beta has transitioned but the
ancestor hasn't persisted, is precisely the encode+write duration: ~165ms at 502k.
The journal shrinks that to the append time. The fix improves the exact invariant
the synchronous write exists to protect.

## Q2: Is a two-of-three shortcut recoverable, or genuinely unsafe?

**Recoverable in principle, moot in practice — and there is a better-founded
equivalent.** In principle: `share(ancestor, alpha)` at a subtree legitimately
reduces the three-way below that point to a two-way walk of ancestor-vs-beta
(and symmetrically for beta), *provided* the reduction still emits the full
mode-specific outcomes — transitions, conflicts, ancestor updates — rather than
returning early. Codex's list of unsafe early-returns is correct; careful
reductions are not on it.

But it's moot, because **cross-lineage pointer sharing doesn't exist in
production — on either side, not just beta.** Codex caught the beta half (a
deserialized ancestor and a wire-decoded beta snapshot share nothing) but the same
lineage argument kills the alpha half, which the brief's 2,499/2,501 figure
silently assumed:

- The ancestor's storage lineage is: deserialized from disk at session start, then
  evolved by `apply()`. `apply()` grafts come from (a) reconcile's ancestor
  changes, which are *slim* — file/symlink nodes with no Arcs, or directory nodes
  with deliberately emptied children (reconcile.rs:114–120) — and (b) achieved
  transition results. For the dominant flow (alpha edits file, propagates to
  beta), the achieved node comes from *beta's* transition outcome, not from
  alpha's scan tree. At no point does an alpha scan directory `Arc` enter the
  ancestor.
- Therefore `share(ancestor, alpha)` is as vaporous as `share(ancestor, beta)`.
  Even codex's "unquestionably safe" conjunction would essentially never fire.
  The only real cross-lineage sharing is the one codex identified: directory-
  valued transition results folded into both the beta model and the ancestor from
  the same decoded outcome (src/endpoint/mod.rs:108–143) — rare, and decaying.

The two-of-three that *is* worth having is **lineage-relative**, where sharing is
guaranteed by construction: extend the existing quiesced shortcut
(src/session/mod.rs:288–299). Today it fires only when both sides are pointer-
unchanged since settlement. The extension: when `quiesced` and
`settled_beta == new_beta` (pointer — guaranteed available, since `ScanUnchanged`
clones `last_snapshot`'s Arcs, remote.rs:187–190, and local rescans adopt baseline
storage), reconcile **only the paths in `diff(settled_alpha, new_alpha)`** — a
same-lineage diff that tree/diff.rs:32 already prunes by pointer — treating
everything outside those paths as the settled cycle proved it. This is exactly as
safe as the existing shortcut because it is the same proof, restricted: settlement
established three-way agreement everywhere, and same-lineage pointer equality
proves non-change since. It turns the 37ms three-way walk into O(changed paths)
without ever consulting cross-lineage pointers.

## Q3: What first, what never

Given persistence is 73%, reconcile+validate 22%, and the sharing caveat guts the
cross-lineage version of the 22%:

1. **Journal — first.** It attacks the 73%, and (Q4) the three costs behind it
   that people actually hit: cycle-tail serialization between consecutive edits,
   burst workloads (git checkout at 502k = many cycles × 40.6MB rewrites), and
   fan-out write amplification (ancestors are per-session: 10 betas = 406MB of
   ancestor writes per edit, on top of the scan-cache amplification found
   earlier). Plus the ~100× smaller crash window, free.
2. **Incremental validate against the previous ancestor — second.** This is the
   one pointer-pruning candidate whose sharing is real, because it is
   same-lineage: `apply()` CoW guarantees untouched subtrees of the new ancestor
   share storage with the old, validated one. Codex's trust rule is exactly
   right (skip shared subtrees; still check child-name invariants at every
   changed vector; fully validate grafted storage; never extend trust to
   "shared with alpha/beta"). ~30 lines, kills 13.8ms of tail, safe by
   induction on the already-validated chain.
3. **Reconcile pruning as specified in the brief — never, as written.** The
   pointer facts it needs do not occur in production; the benchmark that
   motivated it measured its own aliasing. If profiling at target scale ever
   shows reconcile's 37ms mattering in the save-to-visible path, build the
   lineage-relative restricted reconcile from Q2 instead — but it's below the
   journal and the validate fix in value, and unlike them it touches
   reconciliation semantics.
4. **Merkle/CAS ancestor — no.** Dominated by the journal on every axis that has
   a user attached (see Q1).

One instrument before any of 2–3: codex's suggestion to count the three sharing
relationships on real reconcile inputs is right, and cheap. I predict
ancestor–alpha ≈ ancestor–beta ≈ 0 and alpha–beta ≈ 0 after restart; confirming
that costs an afternoon and permanently retires candidate 2.

## Q4: Is the 227ms real? (blunt)

**Partially. The headline is ~5× overstated for the thing a user feels, and you
should know that before you build — but the journal still clears the bar the
supply cache didn't, for reasons the brief doesn't state.**

The brief says "one save costs 226.6ms source-side" and calls it the latency
path. But run_cycle transitions beta *before* it persists the ancestor
(session/mod.rs:363–392 vs :418–441). Decomposed against what anyone waits for:

- **On the save→beta-visible path: rescan 10.0 + reconcile 37.3 ≈ 47ms.** The
  file is already on beta when validate/encode/write begin. For an isolated
  save, the journal changes perceived latency by zero.
- **After the transition: validate 13.8 + encode 54.2 + write 111.3 ≈ 179ms.**
  This delays *cycle completion* — when the worker returns to `await_activity`
  and can react to the next change.

So the honest question is whether the 179ms tail is waited on. It is, in three
real cases: (1) **consecutive edits** — save, look, save again: the second save's
cycle queues behind the first's tail, so rapid iteation eats up to ~180ms extra
per turnaround; (2) **bursts** — a branch switch or build touching thousands of
files runs many cycles, each paying a full 40.6MB rewrite, follow-up cycles
included; (3) **fan-out** — per-session ancestors mean the tail also converts to
406MB/edit of disk writes at width 10, stacked on the scan-cache amplification;
disk bandwidth is a resource sessions genuinely contend for. And unlike the
supply-sharing case — CPU removed from a network-bound window, elapsed flat,
nobody waiting — this cost sits *serially inside the per-edit loop*, and the same
fix shrinks the ancestor-staleness crash window from ~165ms to ~1ms. A fix that
is simultaneously a latency win where anyone waits and a strengthening of the
invariant you refuse to weaken is not idle optimization.

But apply the discipline test you're asking for, before building:

1. **Measure the metric users feel, not the phase table:** save-to-beta-visible
   for the *second* of two saves 100ms apart, at 502k. If the ~180ms
   serialization shows up there, the journal has a waiting user. (~an hour with
   the existing bench harness.)
2. **Ask who runs 502k-entry trees.** At 63k the whole cycle is ~60ms and none of
   this is perceptible; the case rests entirely on chromium-scale trees
   (optionally × fan-out) being a workload your actual users run rather than
   your benchmark corpus. You know the fleet; I don't. If nobody syncs >200k
   entries, file the brief as due diligence and walk away.

If both come back positive — and for chromium-at-any-fanout I expect they will —
build the journal, take the incremental validate alongside it, and skip the rest.
The supply-cache lesson wasn't "don't optimize"; it was "make something wait for
it first." The 179ms has three named waiters and a correctness bonus; the 47ms
has none worth chasing yet.
