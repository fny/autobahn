# Adjudication of the correctness review

Verified against the code at HEAD, 2026-08-29. Scope as given: data loss, corruption,
silent divergence. Bottom line up front: **codex is substantially right — 11 of 12
verify, one deserves a downrank rather than a refutation — but it missed one compound
defect, understated #10 (there is an ENOSPC route that needs no crash at all), and
your question 4 has an uncomfortable answer: #9, #10, and #11 are regressions in
kind, not degree, and the journal is currently below the durability class of the
full rewrite it replaced. Fix forward — the fixes are ~30 lines — but land them
before this touches a real tree.**

## What I checked vs. took on trust

Verified line-by-line against current source: #1 (session/mod.rs:592–606, the
reconcile descent at tree/reconcile.rs:212–227, the pinning test at
session/mod.rs:744), #2 (transitions at session/mod.rs:365–394 precede
`ancestor_store.record` at :450–451), #3's mechanism (scan/mod.rs:517–562,
`reusable_digest` at :652–668), #7–#12 (all of src/session/ancestor.rs, read in
full), `validate_against` (tree/mod.rs:382–437), and — at a stroke — every
no-fsync claim: `grep sync_all\|sync_data` over the crate returns **nothing**.
I also verified two things codex didn't mention: supervisor reset was correctly
updated to call `AncestorStore::reset` (supervisor/mod.rs:440 — I checked because
a stale single-file delete there would have been a crash-free resurrection bug,
and it isn't there), and the `record()` error path's interaction with the journal
(see the #10 upgrade below).

Taken on partial trust: the four "ruled out" items. I re-verified
`validate_against`'s induction myself (it is sound: the pointer skip fires only
against the previous ancestor, which is `validate(true)`d on load at
ancestor.rs:100–102 and re-validated on every install; name/sort/uniqueness
invariants are re-checked in full at every non-shared vector, tree/mod.rs:404–419),
and `open()`'s replay-error propagation (ancestor.rs:94–96). The observer
invalidation and transport-mutex arguments match designs I reviewed in detail
earlier in this project's history; I spot-checked their existence, not every path.

## Verdicts on the twelve

**#1 — VERIFIED, and correctly ranked first.** `ancestor_children < 2 → return
false` at session/mod.rs:596–597; a one-child ancestor gets no protection however
large the subtree. The descent then really does emit the child deletion via the
`beta_diff.is_empty()` branch (reconcile.rs:212–227) and `is_root_deletion` only
guards the empty path (tree/mod.rs:123–126). The test at session/mod.rs:744 pins
the broken behavior and will need to flip. Codex is also right that immediate
child count is an indefensible proxy — the fix is a magnitude threshold
(`ancestor.count() >= 2` entries, or "emptied side lost everything and the
ancestor was non-empty"), one line plus the test. I rank this first for the same
reason codex does: no crash, no adversary, no race — an unmounted volume plus a
common directory layout, and the entire good copy on beta is deleted and the
deletion is then folded into the ancestor, making it permanent. **One
generalization codex missed:** the guard's unit is wrong, not just its threshold.
A mount point *below* the root (`root/data` on its own volume) that vanishes
produces the same mass deletion with the root still populated, and no guard
considers it at any threshold. A per-cycle magnitude check ("one side lost >X% of
entries since the ancestor — halt") subsumes both.

**#2 — VERIFIED; architectural, correctly not first.** The ordering is inherent:
achieved results can't be persisted before the transition produces them. The
journal shrank the window from ~165ms of encode+write to an append; it cannot
close it. Closing it requires a write-ahead *intent* record before the
transition (on restart, an intent without a matching achieved record marks those
paths as unknown provenance → treat as conflict rather than "unchanged"). That's
the eventual fix and it's exactly what the journal file format makes cheap to add
later. Every sync tool in this family has this window today; medium priority.

**#3 — VERIFIED mechanically, but I disagree with the ranking.** The code does
what codex says (reuse on mtime-seconds+nanos, size, inode, type bits — note
mode is compared only on type bits, scan/mod.rs:666, so chmod alone doesn't
force a re-read, which is fine since executability is read fresh from the stat).
But "changed bytes with identical nanosecond mtime, size, and inode" is not an
ordinary-editor event; it's `touch -r`-class tooling, reproducible-timestamp
build systems, or an adversary. This is the identical tradeoff rsync's quick
check and Mutagen make, and the brief's own scope weights by how a real user
hits it. I'd rank it as a documented design limitation with two actionable
edges: (a) there is no `--checksum`-equivalent escape hatch — a periodic or
on-demand full rehash would bound divergence for the paranoid; (b) the tool can
*poison its own baseline* into exactly this state with no forgery at all — see
the missed defect below, which is the part worth fixing.

**#4 — VERIFIED; real, tiny window, industry-standard residual — with one cheap
hardening available.** The check/use pairs are as cited (absence check in
`create_change` → overwrite-capable rename in `publish_file`; validate → rename
in `replace_change`; validate → unlink in `remove_entry`). An editor save landing
in the microsecond window is destroyed. Full closure is impossible on POSIX
pathnames, but the *creation* path can be closed outright with
`renameat2(RENAME_NOREPLACE)` on Linux (fall back to the current rename
elsewhere) — a creation carries no old-content expectation, so refusing to
replace anything is strictly correct. Do that; accept the rest as residual, as
Mutagen does.

**#5 — VERIFIED structurally; severity is deployment-dependent.**
`resolve_parent` (local.rs:1217) verifies components by pathname stat and later
operations reuse joined path strings; a concurrent symlink swap redirects the
rename/unlink outside the root. This is the classic symlink-race CVE class; the
real fix is dirfd-relative traversal (`openat` + `O_NOFOLLOW` handles held
across the operation), which is a sizeable refactor of the transitioner. For a
single-user dev-sync deployment the attacker who can win this race can already
write in the tree; for a privileged daemon syncing a tree writable by others it
is privilege escalation. Rank it by which of those autobahn is; document the
threat model now, refactor when transition code is next open.

**#6 — VERIFIED.** Staged content is trusted by digest-shaped filename
(`stage_begin` inventories names, local.rs:754ff), never rehashed at publish,
and never fsynced before the rename that gives it its authoritative name — so
power loss can mint a wrong-content file with a right name, and `create_node`
then constructs the achieved node with the *requested* digest and the corrupt
file's real metadata, which #3's gate will keep accepting forever. Cheap partial
fix: the publish copy path already reads every byte — digest through it
(`DigestingWriter` exists) and refuse on mismatch; the move path and the
stage_begin reuse path need one extra read each, paid only on reuse hits.
Medium: the trigger is power loss or external interference with staging, but the
failure is permanent and invisible afterward.

**#7 — VERIFIED, and accepted by you — but the code lies about it.** The append
is `write_all` with no sync (ancestor.rs:162–169). That's the durability class
you consciously shipped. However the doc comment says "does not return until
they are on disk" (:138–139) and "the durability contract is unchanged" (:22) —
the first claim is false as written (page cache is not disk), and the module
header's claim is true only for the process-crash class. Fix the comments or
add an opt-in fsync; don't let the contract documentation overclaim what a
reviewer will rely on.

**#8 — VERIFIED — and this one should get fsync even under your no-fsync
policy.** Compaction (`checkpoint()`, ancestor.rs:194–213) is
write-temp/rename/truncate-journal with no sync of the temp file or directory:
program order, not crash order, so power loss can keep the truncation and lose
the checkpoint — rolling back *every generation the journal held*, silently.
Your no-fsync tradeoff was justified by per-cycle latency; compaction is rare
and amortized, so `sync_all` on the temp before rename plus a directory sync
before truncation costs nothing anyone waits on. This is the one place the
durability argument doesn't apply, and the amplitude (a whole journal of
generations, not one cycle) is the largest in the store. Fix it now.

**#9 — VERIFIED. Regression in kind.** Crash between checkpoint publication
(:203) and journal truncation (:209) leaves a spent prefix; `open()` correctly
declines to replay it (:91 breaks on generation mismatch) but retires nothing,
and `record()` appends behind it (:162–169, O_APPEND). The next load breaks at
the stale prefix and never reaches the *acknowledged* base-H record. The
existing test at :492–505 checks that the stale journal loads correctly but not
that appends after it survive — the test that would have caught this is exactly
the missing one. The window to enter the state is microseconds, but once
entered, every subsequent below-threshold cycle is silently discarded on the
next restart, indefinitely, until a compaction happens to run. `record()`'s
return value is a lie in this state — that's worse than the old full rewrite,
which never acknowledged-then-lost under process crash.

**#10 — VERIFIED, and codex UNDERSTATED it: there is a crash-free route.** The
torn tail is tolerated at load (:300–302) but never truncated, and appends go to
physical EOF behind it. Codex's trigger is a crash mid-append. But `write_all`
can also fail *partially* on ENOSPC (or quota, or I/O error): `record()` returns
an error, the cycle fails, the worker reconnects — and the partial record's
bytes are still in the journal. When space frees and the next cycle appends, the
acknowledged record sits behind the tear. Depending on the torn header's
declared length versus what follows, the next load either bails "corrupt"
(fail-closed stall — the better outcome) or breaks at the tear and silently
ignores the acknowledged record (the revert hole). A full disk is not an exotic
event on a dev machine. This is the strongest single argument that the journal,
as shipped, is below the old code's durability class. Same family: a *partial
header* (< 24 bytes) at the tail exits the parse loop without even being
recognized as torn, with identical append-behind consequences.

**#11 — VERIFIED; narrow but real, and the fix is a line.** Reset removes
checkpoint then journal (:125). Crash between the two unlinks can leave a
generation-0-compatible journal that replays on the empty store. The ugliest
variant is a *legacy* checkpoint (read as generation 0, :255–258) with base-0
delta records: after a partial reset, those deltas replay against `None` and
reconstruct a *wrong* ancestor, not merely a resurrected one. Reverse the
order — journal first, then checkpoint — and the crash residue becomes "reset
didn't happen yet," which is consistent and retriable rather than corrupt.

**#12 — VERIFIED; inherited, not a journal regression.** The old `save_ancestor`
format had no checksum either. The observation that ~40% of a checkpoint's bytes
are raw digest fields — where any bit flip is structurally valid bincode that
directly misclassifies a file — is correct and makes this worth the 8 bytes.
Add the same truncated-BLAKE3 the journal records already carry, while touching
`checkpoint()` for #8.

**The four ruled-out items:** all correctly ruled out, per the verification
notes above. Ruling out `validate_against` needed the induction argument codex
gave, and it holds.

## What codex missed

1. **Baseline self-poisoning at publish (the missing bridge from #4 to #3).**
   `publish_file` renames staged content into place and then records achieved
   metadata by stat'ing the *target* (local.rs:1478ff → `symlink_metadata(target)`
   after the rename). If an editor replaces the file in that window, the achieved
   node — folded into both the endpoint baseline and the ancestor — pairs the
   *requested* digest with the *editor's file's* metadata. From then on, #3's
   gate matches: the editor's save is invisible to every scan until the file is
   next touched, and if beta changes the file first, `validate_file` passes
   (disk metadata matches the poisoned record, recorded digest matches expected)
   and the editor's content is overwritten with full validation approval. This
   needs no forged timestamps — the tool manufactures the collision itself.
   **Fix is also a simplification:** stat the *staged temporary* after
   `set_permissions`, before the rename — rename preserves inode and mtime, so
   the metadata is identical when uncontended and can never describe a foreign
   file. Cheaper than the current post-rename stat, and closes the chain.
2. **The ENOSPC upgrade of #10** (above): torn tails arise from ordinary write
   errors, not just crashes, which moves #10 up the ranking.
3. **The general form of #1:** sub-root mount points. No child-count threshold
   fixes a vanished `root/data` mount when the root keeps siblings; only a
   per-cycle magnitude guard does.
4. **The doc overclaim** at ancestor.rs:22 and :138–139 ("on disk") — a
   correctness reviewer or future maintainer will build on that stated contract.
5. Minor: `read_journal` fully parses (and digest-checks) *spent* stale-prefix
   records, so corruption inside an irrelevant spent prefix bricks the load —
   a fail-closed stall, out of scope, but retired for free by the #9 fix.

I looked for and did not find missed defects in the areas you flagged as
under-reviewed: reconcile's slim-node ancestor changes replay correctly in
record order; executability propagation is metadata-only exposure with
peer-vouching gated on byte identity; `stage_locally`/`copy_verifying` verify
digests during the copy, so mid-copy changes fail safe; concurrent sessions on
one alpha refuse via lease-metadata mismatch rather than clobbering.

## Question 4, bluntly: is the journal worse than what it replaced?

**Within the durability class you consciously chose — process-crash safety,
no power-loss guarantees — yes, today it is.** The old full rewrite never lost
an acknowledged cycle to a process crash or a write error; the store's whole
contract is "record returns ⇒ replay reproduces it," and #9 (crash in a
microsecond window, then silent indefinite loss of later acknowledged cycles)
and #10 (reachable via ENOSPC with no crash anywhere) both falsify it. #11
regresses reset from effectively-atomic (one unlink) to a two-file sequence
with a corrupt-ancestor variant. #7, #8, #12 are the accepted class or
inherited and don't count against the journal.

**Fix forward, don't revert — on one condition.** The condition: the fixes land
before the journal meets a real user's tree. They are small, localized, and each
has a deterministic file-surgery repro in the style of the existing tests:

- `open()` retires what replay didn't consume: after parsing, if the applied
  records don't account for the journal's physical length — stale prefix (#9),
  torn tail (#10), or trailing partial header — truncate the file to exactly
  the bytes that were applied (zero, in the stale-prefix case). One
  `set_len`, and it heals damaged journals already in the field on next open.
- `reset()` removes the journal before the checkpoint (#11). One-line swap.
- `checkpoint()` syncs the temp file before rename and the directory before
  truncating the journal (#8), and gains the 8-byte payload digest (#12).
  Off the latency path; costs nothing per-cycle.
- Tests: append-after-stale-prefix survives reload; append-after-torn-tail
  survives reload; append-after-partial-header survives reload; reset with
  only the checkpoint removed stays reset.

That's roughly 30 lines of fix and 80 of tests. Reverting instead would
reintroduce the measured 276ms consecutive-save regression, keep #1–#6
untouched, and discard a file format whose defects are now precisely
enumerated — the worst of both. But if for any reason the patch can't land
promptly, revert: a 165ms write that never lies beats a 1ms append that
sometimes does.

## Fix order, given a real user's tree

1. **#1** — one-line threshold change (`count() >= 2`, or better the magnitude
   guard) + flip the pinning test at session/mod.rs:744. Largest blast radius,
   no crash required, trivial fix.
2. **#9 + #10 + #11 as one patch** (open-time journal truncation + reset
   ordering + the four tests). Restores the journal to the durability class it
   claims.
3. **#8 + #12** — fsync and checksum in `checkpoint()` only. Off the latency
   path; biggest-amplitude power-loss hole closed for free.
4. **Missed defect 1** — stat the staged temp instead of the target in
   `publish_file`. Small, and it closes the only path by which an ordinary
   editor turns #3 into silent data loss.
5. **#4 partial** — `RENAME_NOREPLACE` on the creation path.
6. **Backlog, explicitly documented rather than silently deferred:** #2
   (intent records in the journal — the format now makes this cheap), #5
   (dirfd traversal; document the threat model today), #6 (digest-through-copy
   at publish), #3 (a `--checksum`-style rehash escape hatch; otherwise this is
   a documented tradeoff, not a defect).
7. **Also:** correct the durability claims in ancestor.rs's comments to name
   the actual class.

Safe to leave indefinitely: #3 as a design tradeoff (with the escape hatch on
the backlog), #5 under a single-user threat model, and the residual TOCTOU of
#4 beyond the creation path — that residue is the price of pathname-based sync
on POSIX, and everyone in this product category pays it.
