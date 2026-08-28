# Fan-out sharing in autobahn: design review

Reviewed against the code at `d43fa70` (2026-08-27). Every claim in the prompt was
checked against the source; corrections and refinements are marked ▸.

## 0. Verification of the situation

The core claims are all accurate:

- `Config::plans()` fans one group into one `SessionPlan` per beta — the loop at
  src/config.rs:468 inside `plans()` (src/config.rs:276).
- The supervisor runs one thread per plan: src/supervisor/mod.rs:240–242 in
  `run_watch` (src/supervisor/mod.rs:185 is the per-plan spawn in `run_once`; both
  are one-thread-per-session).
- `connect()` (src/supervisor/mod.rs:614) builds a fresh `LocalEndpoint` for the
  alpha in every session (closure at :624, invoked at :711). Each endpoint carries
  its own `last_snapshot` (src/endpoint/local.rs:139), its own on-disk scan cache
  (`staging_root.with_extension("scancache")`, src/endpoint/local.rs:357), its own
  staging root (per-session state dir, src/supervisor/mod.rs:646–652), and its own
  `ChangeWatcher` (src/endpoint/local.rs:148, created in `scan` at :805 and
  `await_change` at :1026).
- `dirty_paths` consumes the watcher record via `take()`
  (src/endpoint/local.rs:471), and the incremental scanner silently adopts
  anything unmarked — the test at src/scan/mod.rs:888 (`an_unmarked_tree_is_
  adopted_whole`) is exactly the failure a shared-but-drained queue would cause.
- `await_change` degrades to sleeping out the timeout when a watcher cannot be
  created (src/endpoint/local.rs:1026–1032).

Refinements worth having before designing:

▸ **The degradation at the watch ceiling is worse than "silent polling."**
`await_change` re-attempts `ChangeWatcher::new` on *every call* with no backoff
and no reporting. `Session::await_change` slices at 125 ms per endpoint
(src/session/mod.rs:195–202), so an exhausted session attempts a full recursive
watch registration — a walk of every directory, registering watches until the
limit, then tearing them all down on drop — roughly eight times per second,
forever. At chromium scale that is a sustained syscall storm, not a quiet
fallback. Whatever tier you ship first, adding backoff-plus-log to this path is
a five-line fix worth doing immediately.

▸ **The 8192 default is dated on Linux.** Since kernel 5.11,
`fs.inotify.max_user_watches` defaults to ~1% of RAM (commonly ~500k–1M on dev
machines). The ceiling is real — 10 sessions × ~40k chromium directories ≈ 400k
watches, competing with IDEs and other watchers — but "often 8192" overstates it
on modern hosts. Also note the ceiling is Linux-specific: notify uses FSEvents
on macOS, where recursive watches have no per-directory cost. Check what your
deployment actually runs before letting tier 1's urgency be set by the 8192
figure.

▸ **Only healthy sessions pay the 10× cost.** A paused worker drops its whole
session (src/supervisor/mod.rs:408), a failed attempt drops it in `conclude`
(:350–356), and `reset` drops it too (:421–427) — and the endpoint's watcher,
snapshot, and staging handles die with it. This matters for tier 1's lifetime
design (see Q2) and means backoff already sheds watches today.

▸ **A 10× cost you did not list, possibly the largest felt one:** scan-cache
write amplification. On any cycle whose scan differs from baseline,
`store_scan_cache` clones and serializes the *entire snapshot*
(src/endpoint/local.rs:389–394) — ~52–57 MB at chromium scale per the 2026-08-26
measurements — and `transition` stores it *again* after folding (:1119). A
single edited file at 10-way fan-out is therefore on the order of **20 full
snapshot serializations ≈ 1 GB of state-directory writes and ~1.6 s of
serialize CPU, per edit**. Page cache absorbs none of this; it is the one 10×
cost that scales with change frequency rather than tree size. Measure it first
(Q5) — it may reorder your tiers by itself.

---

## Q1. Is the tier 0 boundary right? What else must not be shared?

**The ancestor invariant is correct and well-grounded.** The provenance argument
at src/session/mod.rs:425–439 generalizes exactly as you say: the ancestor for
(alpha, betaᵢ) encodes the *history of that pair*, and betas legitimately
diverge. "Share observations, never provenance" is the right slogan. But the
boundary as stated is incomplete. The full list of must-stay-private state:

1. **The ancestor** (and its path, and `save_ancestor`'s synchronous write).
   As stated. ✔

2. **The session's reconciliation basis — as an identity, not just a tree.**
   The transition-safety contract (src/endpoint/local.rs:1055–1059 and the
   module doc at :11–18) is "the retained snapshot *is* the scan these
   transitions were reconciled from." Under tier 2 the tree *storage* may be
   shared, but each session must keep its own `Arc` reference to the snapshot
   it reconciled from. This is nearly free (an Arc clone) and it is what keeps
   the contract an identity rather than a coincidence. See Q3 for why violating
   it is survivable but expensive.

3. **`quiesced` / `settled_alpha` / `settled_beta`** (src/session/mod.rs:105–109)
   — per-session by nature; they record what *this* session's last settled cycle
   saw. Sharing storage behind them is fine (that is the point of the pointer
   check); sharing the fields is not.

4. **The supply and receive stream state.** `LocalEndpoint` has exactly one
   `supply: Option<SupplyState>` and one `receive: Option<ReceiveState>` slot
   (src/endpoint/local.rs:141–145). Ten sessions staging concurrently through
   one shared endpoint object would trample these. This is the concrete reason
   "share the endpoint" must never be the design — share a *watcher* and a
   *scan service* behind per-session endpoint objects; never share the
   `LocalEndpoint` itself.

5. **Staging roots.** Content-addressed staging looks temptingly shareable
   (idempotent, digest-named), but `publish_file`'s move-on-last-use
   optimization makes a staging directory single-owner: the last publish of a
   digest **renames the staged file away** (src/endpoint/local.rs:1602–1666,
   driven by `staged_uses`, :1283–1288). A second session sharing the directory
   would find content stolen out from under its own pending publishes —
   producing `missing_staged` churn at best. Sharing staging requires
   cross-session use-counting; that is tier 3 work, so staging stays private
   until then. ✔ (your instinct to defer was right, and this is the specific
   mechanism that makes it mandatory rather than prudent.)

6. **The session lock stays per-session — and note what it no longer covers.**
   The `SessionLock` (src/session/mod.rs:612) guards the *session state
   directory*: ancestor, staging, status. It must stay per-session. But once a
   watcher/scan service is shared across sessions, that shared object lives in
   a synchronization domain **no session lock protects**. It needs its own
   mutex, its own lifetime (refcounted, keyed by *canonicalized* root —
   `connect` canonicalizes at src/supervisor/mod.rs:633 — plus the
   scan-affecting options: ignores, symlink mode, max file size; two groups
   over the same root with different ignore sets must not share a scan), and a
   story for the cross-process case: a manual `sync` in another process will
   still build its own endpoint over the same root, which is safe (observations
   duplicate harmlessly) but means the shared scan cache file must tolerate
   last-writer-wins from a foreign process. The existing atomic-rename write
   plus the metadata gate on cache adoption (src/endpoint/local.rs:363–371)
   already make that safe.

Shareable, and worth actively sharing:

- **Probed `FilesystemBehavior`** — a per-volume property, probed once per
  endpoint at first scan (src/endpoint/local.rs:793–796). Ten endpoints probe
  ten times (30 scratch files in the root at startup); harmless, but it belongs
  to the shared observer naturally.
- **The persisted scan cache** — under tier 2 there should be exactly one per
  (root, options), owned by the scan service, replacing ten per-session copies
  and the write amplification described above.
- **`last_full_scan`** — becomes scan-service state under tier 2, with one
  subtlety: `transition` clears it when problems prove the snapshot wrong
  (src/endpoint/local.rs:1104–1106). Under sharing, *any* session's transition
  problem must clear the **shared** service's `last_full_scan` — the shared
  baseline is what was proven wrong. If this invalidation stays session-local,
  the other nine sessions keep adopting a snapshot known to be stale at that
  path.

---

## Q2. Tier 1: per-cursor watcher

**The N-cursor semantics are right; choose fan-out-at-write, not a shared log.**
Two implementations satisfy "one watcher, N independent cursors":

- **(a) Fan-out at write:** the watcher callback appends each event's paths into
  N independent `PendingChanges`, one per registered session, each with its own
  8192 cap, its own `give_up()`, and its own `sync_channel(1)` wake. `take()`
  stays per-session and keeps today's semantics verbatim.
- **(b) Shared append-only log with per-session cursor indices:** shares path
  storage, but needs compaction at the minimum cursor, an awkward per-reader
  `give_up`, and sequence bookkeeping.

Take (a). The worst-case memory difference is 8192 paths × N ≈ a few MB — noise
next to one snapshot copy — and (a) is a mechanical refactor of code whose
overflow and give-up behavior is already tested. (b) is machinery you will
delete in tier 2 anyway (see Q4).

**Pause / reset / backoff:** already answered by the supervisor's structure. All
three paths drop the `Session`, hence the endpoint, hence its cursor
registration (src/supervisor/mod.rs:408, :350–356, :421–427) — so make cursor
deregistration a `Drop` impl and there is no unbounded growth from inactive
sessions, because inactive sessions hold no cursor at all. The remaining growth
case is a *healthy but slow* session (mid-transfer for minutes while siblings
cycle fast): its cursor grows to `MAXIMUM_PENDING_PATHS`, gives up, and that
session's next scan is full — exactly today's busy-tree behavior, per-session,
with no correctness exposure. That is the right bound; do not add another.

Lifetime details that need explicit answers in the implementation:

- The shared watcher is refcounted in a registry keyed by (canonical root,
  options); it drops when the last endpoint deregisters — so a whole group
  failing releases its watches, as today.
- Watcher death (the `Disconnected` arm, src/endpoint/local.rs:1046–1051)
  currently drops and recreates per-endpoint. Shared, it must invalidate for
  *all* registrants — bump a generation counter; every session's next scan goes
  full, as a fresh watch demands (src/endpoint/local.rs:805–810).
- `change_activity` (src/endpoint/local.rs:1017) must report the session's own
  cursor, not a global count — `settle()` (src/session/mod.rs:231) samples it to
  detect a burst *this session has not yet consumed*, and a global count would
  hold every session hostage to the slowest consumer's backlog.

**Simpler alternatives considered and rejected:** fanotify's filesystem-wide
marks need CAP_SYS_ADMIN; a "one session watches, others poll" scheme gives up
latency for N−1 betas; raising `max_user_watches` is an ops mitigation worth
documenting but leaves the per-session registration walk and the churn-loop
failure mode in place.

---

## Q3. Tier 2: shared scan

**A pure time-based freshness window is not sound; an event-gated cache is —
and it is simpler.** The failure mode of "reuse if younger than 1 s" is not
data loss by itself; it depends on who consumes the dirty record. The bad
ordering: cached scan taken at T₀; change lands at T₁; session B cycles at T₂
with T₂−T₀ inside the window and is served the T₀ scan. If B's cycle also
consumed (or advanced) B's view of the change record, the change is now
unobserved by B until the 120 s full-scan safety net — precisely the
src/scan/mod.rs:888 failure, resurrected one level up. And if B's record is
*not* consumed, B wakes immediately, is served the stale scan again, and hot-
loops until the window expires.

The fix dissolves the window: **tag every scan with the watcher's event
sequence number at scan start, and serve from cache iff the sequence is
unchanged.** Then the cache is valid until the next event — arbitrarily long
during idle, which is strictly better than 1 s — and a change always forces
exactly one fresh scan, which every waiting session then shares. No tunable, no
staleness bound to reason about. (Keep the age check only as a belt if it
comforts you; it does no work once sequence-gating exists.)

This also collapses tier 1's machinery: with a shared scan there is once again
**one** consumer of the dirty-path record — the scan service. Sessions no longer
need path cursors at all, only a per-session "last seen sequence" for
`await_change` and `settle`. See Q4.

**The quiesced short-circuit gets better, and stays sound.** Cached serves hand
every session the same `Arc` storage, so `nodes_share_storage`
(src/tree/mod.rs:378) fires across sessions, not just within one. The
shortcut's premise — same storage ⇒ nothing changed since that scan was taken
(src/session/mod.rs:281–299) — is a property of the *scan*, not of which
session requested it, so sharing does not weaken it. Ten idle sessions settle
on one tree with zero per-session copies.

**Transition validation: your hazard is real, but its consequence is refusals
and noise, not data loss — I verified the mechanics.** All destructive
validation is *expectation-driven*: removals validate disk against the
transition's own `old` (`remove_entry`, src/endpoint/local.rs:1713;
`validate_file`, :1381–1408, takes the expected digest **from the
expectation**, and additionally requires disk metadata to match the scanned
record); `remove_directory` explicitly leaves any on-disk entry the expectation
does not account for in place (:1776–1778); replacements validate the old
digest the same way (:1907); creations refuse over any existing content
(:1451–1453). Walk the dangerous interleaving: sessions A and B both reconcile
from shared scan S; A applies alpha transitions (two-way mode) and folds,
advancing the baseline to S′; B then transitions with changes reconciled from
S. At any path A touched, either the scanned digest no longer matches B's
expected digest, or the metadata no longer matches — **refusal**, reported as a
problem, retried next cycle. At paths A did not touch, S and S′ agree and
validation is identical. Notably, A's new files under a directory B deletes
survive because `remove_directory` spares unaccounted entries. So sharing a
folded baseline cannot silently destroy data.

Why you should *still* keep the per-session reconciliation basis (Q1 item 2):
every one of those refusals is a `Problem`, and problems clear `last_full_scan`
(src/endpoint/local.rs:1104–1106) — under sharing, that forces a **fan-out-wide
full rescan** per cross-session interleaving, plus "problems" status noise for
states that are not problems. Validating against the session's own basis makes
those interleavings resolve as ordinary next-cycle work instead.

**Fold: mutate the shared baseline (serialized), do not invalidate it.** Your
instinct — "the fold must invalidate the shared state rather than mutate it
under the other nine sessions" — has the right worry and the wrong remedy. There
is no mutation-under-feet hazard to defend against: `fold_transition`
(src/endpoint/mod.rs:130) runs `apply()`, a copy-on-write graft that builds a
*new* tree sharing unchanged subtrees (src/tree/mod.rs:4–7); sessions holding
`Arc`s to S keep S bit-for-bit unchanged, and only the service's cache pointer
advances. Folds describe on-disk reality (achieved results, refusals included —
src/endpoint/local.rs:1108–1127), so folding them into the shared baseline under
the service lock keeps the next scan's digest reuse intact. Invalidating
instead would force a full re-stat-and-digest sweep of half a million files
after *every* two-way alpha write — the most expensive possible response to the
best-understood change in the system. Sequential folds compose (`apply` is a
path-keyed graft), and the sequence counter already forces the next serve to be
a fresh scan against the folded baseline.

---

## Q4. Ordering, and cheaper alternatives

**The tier boundaries are right; the seam between 1 and 2 is not.** Tier 1's
end-state (N per-session path cursors) is machinery tier 2 deletes (one
consumer plus per-session sequence numbers). Building them as separate projects
means building the fan-out-at-write cursor registry, testing it, and then
removing it. Instead: build one **shared alpha observer** object from the
start — owns the watcher, the pending record, the event sequence counter, the
probed behavior, and (in its second act) the scan cache and baseline. Ship it
in two acts if you want two shippable stages, but design act one's interfaces
(registration keyed by canonical root + options, sequence numbers exposed,
`Drop` deregistration) so act two changes its internals, not its callers.
If the benchmark says the scan-cache write amplification is as bad as the
arithmetic suggests, consider shipping both acts together — the combined design
is *simpler* than tier 1's standalone form.

**Relay topology: expressible today, but it changes semantics — it is an ops
workaround, not the design answer.** You can chain groups in configuration
right now (A→B₁, then B₁→B₂ …; remote alphas are supported,
src/config.rs:324–347) with zero code. But a relay changes what users get:
each hop's ancestor makes B₁ the *source of record* for B₂ — conflicts and
`two-way-resolved`'s "alpha wins" resolve in favor of the upstream hop, not the
true alpha; latency compounds per hop; a mid-tree node's failure stalls its
whole subtree; and betas must run agents capable of being sources. Hub-and-
spoke from the alpha is the semantically correct shape for "N independent
replicas of A." Recommend relay in docs for users hitting watch limits today;
do not build toward it.

**One-multi-beta-session (one controller, one alpha endpoint, N betas) is the
asymptote — name it, do not start with it.** It gets tiers 1–2 (and most of a
sane tier 3) *structurally*, with no cross-thread sharing at all: one scan per
cycle feeds N reconciliations against N ancestors. But it reworks the
supervisor's entire per-session operational model — per-beta status files,
locks, pause/reset/backoff, connection healing (src/supervisor/mod.rs
throughout) — and couples every beta's cycle cadence to one controller loop
(one slow beta stalls, or you reinvent per-beta threading inside the session
and are back where you started). The shared-observer plan preserves the
operational model with a far smaller blast radius. If the observer ever grows a
third mutex, revisit this.

**Tier 3: deferred is right, and "probably never" is my actual forecast.**
Two structural limits cap its value: (1) deltas are per-beta — each
`StagingNeed` carries that beta's base signature, so supplied frames are only
identical across betas when signatures are empty, i.e. cold syncs and pure
creations; (2) on those cold paths, the alpha-side *digesting* is scan work
(tier 2 already de-duplicates it), and the remaining per-beta supply cost is
reading bytes the page cache already holds after the first beta, chunking them
(src/endpoint/local.rs:617–634), and network egress that is per-beta no matter
what. Ten-way network egress dominates ten-way `memcpy`. Measure after tier 2
(Q5.5); only a demonstrated alpha-CPU-bound cold fan-out justifies touching the
transfer path, where — as you say — mistakes corrupt content.

**Cheapest changes with real benefit, orthogonal to the tiers:**
1. Backoff + logging on failed watcher creation (the churn loop, ▸ above).
2. Document `fs.inotify.max_user_watches` (and that macOS is unaffected).
3. If tier 2 slips: de-duplicate the *scan cache* alone (one file per root, or
   even just skipping the second store in `transition` when the fold's storage
   matches — :1119 rewrites the cache the scan-path store at :867 may have just
   written). This attacks the 1 GB-per-edit amplification without any live
   sharing.

---

## Q5. What to measure before committing to tiers 2 and 3

Your built cell (1α × 10β, chromium, cold) answers tier-2's cold half. Run it
plus three cheap companions, and collect *these* numbers:

1. **Cold fan-out (the built cell), 1β vs 10β:** makespan scaling; supervisor
   RSS (expect ~10 independent trees ≈ +0.5–1 GB at 505k files — page cache
   does not absorb heap); alpha CPU split between scan/digest and supply
   (perf/flamegraph); total bytes written under the state root (scancache
   amplification); peak inotify watch count vs the host's limit.
2. **Idle steady state, 1 h, post-sync:** per-session CPU and syscall rate. The
   dominant term should be ten staggered full scans per `FULL_SCAN_INTERVAL`
   (120 s) — i.e. a full 505k-entry stat walk every ~12 s somewhere in the
   process. Verify state-dir writes are ~zero when quiesced (unchanged scans
   share storage and skip the store, src/endpoint/local.rs:863–868 — confirm).
3. **Warm edit under fan-out (the decisive one):** touch one file; measure (a)
   p50/p95 propagation per beta, (b) bytes written to the state root per edit,
   (c) serialize CPU. Arithmetic predicts ~20 snapshot serializations ≈ 1 GB
   and ~1.6 s CPU per edit at N=10; if measurement confirms even half of that,
   tier 2 (or at minimum the shared scan cache) is justified by this line
   alone, independent of scan CPU.
4. **At the watch ceiling:** artificially lower `max_user_watches` below the
   tree's directory count; measure the re-registration churn (CPU, syscalls/s)
   and confirm propagation latency degrades to interval/full-scan cadence.
   This calibrates how much of tier 1 is "remove a ceiling" vs "stop a stampede."
5. **Tier-3 gate (only after tier 2):** rerun the cold cell; if alpha is
   network-bound (expected), tier 3 is dead; if alpha-CPU-bound, attribute it —
   only supply-side read/chunk time (not scan/digest) argues for tier 3.

Decision rules: **tier 2** is worth it if (2) scales ~linearly with N, or (3)
shows material write amplification, or (1) shows RSS you care about. **Tier 3**
only on a failed (5) gate.

---

## Summary of disagreements

1. Tier 0 is correct but under-specified: add the reconciliation-basis
   identity, supply/receive state, and staging roots (move-on-last-use makes
   them single-owner) to the never-share list; the session lock stays but no
   longer covers the new shared object, which needs its own lock, key, and
   refcounted lifetime.
2. Tier 1: per-cursor is right; implement as fan-out-at-write, lifetime via
   `Drop` (pause/backoff already drop endpoints); growth is bounded by the
   existing 8192-give-up semantics per cursor. But don't build it as a
   standalone artifact — it is scaffolding tier 2 deletes.
3. Tier 2: a time-based freshness window is unsound (or hot-loops) depending on
   cursor consumption; event-sequence gating is both correct and simpler, and
   eliminates the window parameter. Validation safety survives sharing either
   way (expectation-driven refusals — verified in the transitioner), so the real
   argument for the per-session basis is avoiding refusal noise and the
   problem-triggered full-scan stampede. Fold into the shared baseline
   (CoW makes it safe); do not invalidate.
4. Ordering: merge tiers 1–2 into one shared-observer design (two shippable
   acts); relay is a config-level ops workaround with different semantics, not
   a destination; a multi-beta Session is the clean asymptote but the wrong
   first move; tier 3 will likely never clear its measurement gate.
5. The measurement most likely to change your priorities is not scan CPU — it
   is per-edit scan-cache write amplification (~1 GB/edit at N=10 by
   arithmetic), plus the watcher-creation churn loop at the ceiling, which
   deserves a five-line fix regardless of everything above.
