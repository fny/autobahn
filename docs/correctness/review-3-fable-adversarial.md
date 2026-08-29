# Second pass — from the outside in

Adversarial pass over invariants, chains, recovery, and filesystem lies. Nothing
below repeats /tmp/codex-correctness.md or /tmp/fable-correctness.md. Verified
against HEAD, 2026-08-29; code untouched.

The load-bearing observation first: this system's real backstop is **the 120-second
full scan plus metadata-gated refusal on destructive ops**. Almost every attack I
constructed dies against one of those two walls — watcher blindness, torn scans,
hardlink aliasing all produce *bounded, self-healing* staleness. The findings below
are exactly the cases where one of the two walls has a hole: where the poisoned
state *survives a full scan* (because the metadata gate itself was poisoned), or
where the refusal check is answered by something other than the filesystem.

## New findings, ranked

### 1. `scan_file`'s grow-during-read re-stat records a digest for content it never read — and persists it

**scan/mod.rs:546–558.** When the read length disagrees with the pre-read stat,
the code re-stats and records the *fresh* metadata alongside the digest of the
bytes it *already read*. The comment claims "any further change moves the mtime
and forces a re-read" — false for the change that already happened between
read-EOF and the re-stat. Sequence, all ordinary:

1. A build is writing an artifact. Scan stats it at S1 bytes, reads to EOF and
   gets S2 > S1, re-stats and captures (S3 ≥ S2, mtime T3). Recorded: digest of
   the S2-byte prefix, metadata (S3, T3).
2. The writer finishes at S3 inside that window. No further mtime movement ever
   comes.
3. Every later scan — full scans included — matches (S3, T3) and reuses the
   prefix digest. The poisoned pair is written into the persisted scan cache
   (endpoint `store_scan_cache`), so it survives restarts.
4. Reconcile propagates the wrong digest; the receiver's verification rejects
   the real bytes as a mismatch every cycle; after two identical misses
   `run_cycles` declares "staging is failing rather than racing"
   (supervisor/mod.rs:589–593) and the session errors and backs off forever —
   until the file is next touched.

Honest scope note: the terminal state is a *visible* (if misleading) error plus
indefinite divergence, not silent loss — the receiver-side digest verification
is what saves it from being worse. But the trigger is any file whose writer
finishes during a scan, which at chromium scale with build outputs is routine.
The fix is one line of policy: on a size mismatch, keep the **original** stat's
metadata (guaranteeing next scan's stat differs and forces a re-read), instead
of adopting the fresh stat.

### 2. No racy-timestamp guard: a write in the same mtime granule as the recorded digest is invisible — no forgery required

**scan/mod.rs:539, :652–668.** `reusable_digest` demands exact
mtime-seconds+nanos equality, which sounds airtight until you remember where
mtimes come from: the kernel's coarse clock. Two writes within one granule get
*identical* nanosecond mtimes — ~1–4 ms (jiffies) on ext4, 1 s on many NFS
servers and HFS+, 2 s on FAT. Sequence:

1. File modified at T; a cycle's scan stats and digests it shortly after.
2. An editor rewrites it — same length (format-on-save, toggled constant,
   lockfile rewrite) — with the resulting mtime still in granule T.
3. Every subsequent scan, full scans included: stat matches recorded
   (mtime, size, inode) → digest reused → the second write never observed.
4. If beta later changes that file: alpha reads as "unchanged," the alpha
   transition's `validate_file` compares disk stat to the (matching) poisoned
   record and **passes**, and the unobserved write is overwritten with full
   validation approval. Without a beta edit: silent divergence until the file
   is next touched.

This is distinct from the adjudicated #3 (forged/restored timestamps): nothing
here is forged. It is the "racy git" problem, and git's fix applies verbatim:
treat a digest as non-reusable when its recorded mtime is not strictly older
than the recording scan's start (minus one granularity unit). Cost: re-reading
only files modified in the second before a scan — precisely the files worth
re-reading. `FileMetadata` already carries everything needed
(tree/mod.rs:39–50); this is a comparison in `reusable_digest` plus a
scan-start timestamp threaded in.

### 3. On NFS/SMB, every stat-based guard is answered by the client cache, not the filesystem

**local.rs:1257 (`validate_file`), scan/mod.rs:652.** The adjudicated TOCTOU
findings have microsecond windows. Client-side attribute caching (NFS
`acregmin`/`acregmax`, default up to 60 s) widens them to *seconds*, and inotify
is silent for remote writers, so nothing shortens them. Multi-client sequence:

1. Alpha root is an NFS mount; another client writes file X.
2. This client's attribute cache still serves the old (mtime, size, inode) —
   so the scan reuses the old digest, and X reads as unchanged.
3. Beta legitimately modifies X; reconcile emits an alpha transition.
4. `validate_file` stats X — the *cache* answers, matching the scan — validation
   passes, and the rename destroys the other client's write, which was never
   digested by anyone.

No crash, no race in this process; the window is as wide as the attribute
cache. The realistic remedy is not code: state the support boundary (local
filesystems; network mounts best-effort, single-writer), and cheaply detect
(statfs magic) and warn when a root is NFS/CIFS/FUSE. If multi-client NFS is
ever a real deployment, only open-fd revalidation (`fstat` after `open`, which
NFS close-to-open consistency actually refreshes) shrinks it.

### 4. The session lock's "structurally impossible" claim is scoped to one state root — and two state roots occur naturally

**session/mod.rs:615–622 (the claim); main.rs:140–202 (`--state-root` on every
subcommand); paths.rs:46 (default is per-user `~/.autobahn`).** The flock lives
under the state root, so two supervisors with different state roots syncing the
same (alpha, beta) pair never contend. No flag is needed to get there: **two
Unix users on one machine syncing the same shared directory pair** each use
their own `~/.autobahn`. Each session then keeps an *independent ancestor* for
the same pair, and the revert hole opens with no crash anywhere:

1. Supervisors A and B (different users or different `--state-root`) both sync
   `/shared/src ↔ remote`, two-way. A propagates content X; A's ancestor
   records X. B's ancestor still says O — not stale by crash, stale by never
   having observed.
2. A user deliberately reverts the file to O on `/shared/src`.
3. B's next cycle: ancestor O, alpha O, beta X → "beta modified, alpha
   unchanged" → B pulls X back over the revert. Silent, validated.

The agent side enforces nothing either: both controllers initialize the same
session identifier and share the agent's staging namespace without exclusion
(protocol.rs:41–42; temp names are process-unique so nothing collides — meaning
nothing *fails*, either). Fix direction: an advisory lock derived from the
session identity in a location all parties share (the agent's state dir is the
natural chokepoint for remote betas — refuse a second live Initialize for the
same session identifier), plus documentation. Severity is configuration-gated,
but the failure needs no misfortune once the configuration exists.

### 5. Minor: case-fold renames apply creations before deletions, dropping the file from beta for one cycle

Reconcile emits transitions in name-sorted walk order and the batch applies in
that order (session/mod.rs:370–377). Rename `foo.txt → Foo.txt` on alpha sorts
the creation ("F" < "f") before the deletion. On a case-insensitive beta, the
creation's absence check *finds* the old file via folded lookup and refuses
("refusing to create over existing content", local.rs:1287ff); the deletion
then succeeds. Net: for one cycle beta has neither spelling; the next cycle
recreates it, and the ancestor stays consistent throughout (refusals fold as
old content). Transient by construction, but the hardening is one sort: apply
deletions before creations within a transition batch — the standard trick for
exactly this rename shape.

### 6. Minor: watcher event paths are recomposed but never case-folded, so a case-variant event marks the wrong trie node

Dirty marking recomposes NFD components (the `decomposes_unicode` path) but
does no case folding; an event reported under a different case than the
recorded name marks a child that matches nothing, and the modified file is
adopted stale until the 120-second full scan. Bounded, destructive ops still
refuse on metadata mismatch — hygiene, not loss.

## Attacked and held

For the confidence goal, the invariants that survived deliberate attack:

- **Crash anywhere inside a cycle converges.** Reconciliation is state-based,
  not op-based: a crash between beta's and alpha's transitions, or before the
  ancestor record, re-derives the remainder from fresh scans. The *only*
  crash-sensitive state is the ancestor store — which is why the first pass's
  findings concentrate there, and why fixing them closes the class.
- **Refusals cannot corrupt provenance.** A refused transition's achieved
  content is the transition's `old`, which for beta transitions *is* the
  ancestor's content (reconcile.rs:223) — folding it is a no-op on the
  ancestor. I tried to build a poisoned-ancestor chain through refusals and
  could not.
- **Hardlink aliasing** (one inode, two paths; the watcher sees only the
  written-through name): the alias is adopted stale but its on-disk mtime did
  change, so destructive ops refuse and the ≤120 s full scan heals it.
- **Case/normalization sibling collisions** are refused at creation with
  proper case folding *plus* recomposition (local.rs `create_children`) —
  someone did think this through, and it held against Σ/σ/ς-style folds.
- **Torn scans of same-size rewrites** heal: the writer's mtime differs from
  the recorded one, forcing a re-read (the exceptions are exactly findings 1
  and 2 above).
- **Controller/agent model lockstep**: both sides fold transition outcomes with
  the same function on the same inputs (endpoint/mod.rs:127), and bincode
  round-trips exactly; `ScanUnchanged` cannot drift the models apart.
- **Recovery paths**: a killed agent's abandoned `.autobahn-tmp-recv-*`
  temporaries pollute the staged-name inventory harmlessly (prefix names can't
  collide with 64-hex digests — a disk leak, not a correctness issue); an
  aborted cold sync's partial ancestor matches disk truth by construction;
  ENOSPC on checkpoint/publish/status paths fails clean and retries;
  `max_file_size` flip-flops park content as untracked without ever reading as
  deletion.

Cheapest assumption to break in the field, as asked: **"a stat answers for the
filesystem"** — findings 1–3 are all versions of it (stale by racing the write,
stale by clock granularity, stale by a cache), and it is broken by ordinary
builds, ordinary editors, and ordinary mounts respectively.

## The formal-methods question

**Would a model checker have caught #9/#10/#11 mechanically? The right tool
would — but the right tool is not a model checker, it's crash-point
enumeration, and it's cheap.** Be precise about the bug class: no concurrency
(the store runs single-threaded under the session lock — **loom is irrelevant**;
there are no lock-free atomics worth its price here), no deep arithmetic
(**kani would drown in bincode and filesystem stubbing before proving
anything**), no protocol distribution (**stateright/TLA+ would catch them at
10× the cost, checking a spec that then drifts from the 300 lines of Rust it
models**). The bugs are crash-*recovery* bugs: state = journal/checkpoint file
bytes, operations = {record, checkpoint, reset, open}, adversary = a cut at any
byte of any write, plus partial `write_all`. That adversary is enumerable:

1. **A crash-enumeration harness over `AncestorStore` — the single highest
   confidence-per-line investment available to this crate.** Drive a random
   (proptest) or exhaustive-small sequence of operations against a parallel
   in-memory reference (a `Vec` of acknowledged generations). After the
   sequence, take the resulting on-disk bytes and, for **every prefix length**,
   truncate a copy, `open()` it, and assert the two invariants: every
   acknowledged record before the cut's acknowledgment point is reproduced,
   and nothing unacknowledged is fabricated. Additionally cut *between the
   syscalls* of `checkpoint()` and `reset()` (rename-vs-truncate, the two
   unlinks) — trivially done by performing those steps manually with the same
   file operations, since the formats are plain files. This finds #9 (cut
   between rename and truncate, then append, then reopen), #10 (cut mid-append
   — byte-level truncation *is* the torn tail; then append; then reopen), and
   #11 (cut between the unlinks) **deterministically, not probabilistically**
   — the invariant violation is stable once the state is entered. It is the
   disciplined generalization of the file-surgery tests already in
   ancestor.rs's test module: same technique, exhaustive instead of
   anecdotal. Roughly 150–200 lines plus `proptest` in dev-dependencies; a
   day or two including the fixes it will immediately flag. It also *stays*:
   every future format change gets the same adversary for free.
2. **Property-based reconcile testing — second priority, different
   justification.** Reconcile is a hand-port of Mutagen's most subtle logic
   and a defect there is direct, silent data loss with no crash required.
   Properties over generated small trees: (a) absent conflicts, applying the
   emitted transitions to both sides yields content-equal trees; (b) no
   transition or ancestor change touches a path where ancestor, alpha, and
   beta already agree; (c) untracked/problematic content never appears inside
   any emitted `new`; (d) one-way-safe never emits an alpha transition. A tree
   generator is ~100 lines and gets reused by the harness in (1). This would
   *not* have caught the journal bugs; it guards the component whose failures
   are scariest.
3. **Not worth it now:** kani, loom, stateright, TLA+ (reasons above), and a
   power-loss/write-reordering simulator (ALICE-style) — the process-crash
   class must be clean first, and the first pass already prescribes the fsyncs
   for the two places power-loss ordering matters (compaction; eventually
   intent records).

Versus more hand-written tests, honestly: hand-written tests encode the
failure modes someone already imagined — and the existing suite proves the
point, since it contains a stale-journal test that checks *loading* but not
*appending-after*, which is precisely the imagination gap #9 lived in.
Enumeration doesn't need the imagination. For a 300-line state machine with a
byte-addressable adversary, property-plus-enumeration is strictly better per
line than more anecdotes, and this is the rare case where the formal-ish
option is also the *cheap* option. Everything heavier than proptest, skip.
