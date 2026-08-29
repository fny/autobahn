# Correctness plan

Four reviews (archived beside this file) produced twenty-five distinct
defects. Four of the worst were then reproduced empirically against the
release binary before this plan was written:

- An overlapping one-way-replica configuration deleted its own alpha root
  and reported success.
- An emptied single-child root deleted beta's entire good copy.
- A user file named `.autobahn-tmp-notes` was silently never synchronized.
- A deletion made while a path was ignored was resurrected when the
  ignore was removed.

IDs below: C1.n = review 1 (codex), F1 = review 2 (fable adjudication),
F2.n = review 3 (fable adversarial), C2.n = review 4 (codex sweep).

Each phase lands with tests that reproduce the defect first, and the full
suite green. Phases are ordered by (severity × how ordinary the trigger
is), with verification infrastructure built *before* the fixes it checks.

## Phase 1 — configurations and guards that destroy data outright

- [x] **C2.1** Reject configurations where one local root contains
      another (same session and across sessions). Canonicalize and
      compare; remote overlaps documented as unverifiable.
- [x] **C1.1 + F1** Replace the emptied-root guard's child-count proxy
      with a magnitude check: halt when one side lost more than a
      threshold of the entries the ancestor holds, wherever in the tree
      the loss falls. Flip the test that pins the broken behaviour.
- [x] **C2.6** Skip only autobahn's actual temporary grammar, not every
      name starting with `.autobahn-tmp`; surface reserved-name
      collisions as scan problems rather than silence.

## Phase 2 — the ancestor store, fixed under an enumerating adversary

- [x] **V1** Crash-point enumeration harness (proptest): drive
      record/checkpoint/reset/open sequences against an in-memory
      reference; for every byte-length prefix of the resulting files,
      reopen and assert acknowledged records survive and nothing
      unacknowledged is fabricated. Cut between the syscalls of
      checkpoint() and reset() as well. Expected to fail on C1.9, C1.10,
      C1.11 before the fixes; green after.
- [x] **C1.9 / C1.10** open() truncates what replay did not consume —
      stale prefixes, torn tails, partial headers — so later appends can
      never sit behind dead bytes. Heals damaged journals on next open.
- [x] **C1.11** reset() removes the journal before the checkpoint.
- [x] **C1.8** checkpoint() syncs the temporary before rename and the
      directory before truncating the journal. Off the latency path.
- [x] **C1.12** Checkpoints carry the same truncated-BLAKE3 digest that
      journal records already do.
- [x] **C1.7** Correct the doc overclaim: the append is process-crash
      durable, not power-loss durable.

## Phase 3 — the scan cannot be allowed to lie about content

- [ ] **F2.1** On a size mismatch after read, keep the *original* stat's
      metadata so the next scan is forced to re-read; never adopt the
      fresh stat beside a digest of bytes it does not describe.
- [ ] **F2.2** The racy-timestamp rule: a digest is not reusable when its
      recorded mtime is not strictly older than the scan's start.
- [ ] **F1-chain** publish_file records metadata from a stat of the
      *staged temporary* before the rename, never the target after it,
      so an achieved node can never describe a foreign file.

## Phase 4 — transition hardening

- [ ] **C1.4** Creations publish with RENAME_NOREPLACE on Linux (fall
      back elsewhere): a creation carries no old-content expectation, so
      refusing to replace anything is strictly correct.
- [ ] **C1.6** The publish copy path digests what it copies and refuses
      on mismatch; the rename path verifies staged content on reuse.
- [ ] **C2.2** A created root is probed before its children are created,
      so name-folding filesystems are known before collisions can be
      published as successes.
- [ ] **F2.5** Deletions apply before creations within a transition
      batch, closing the case-fold rename transient.

## Phase 5 — identity and exclusivity

- [ ] **C2.3** The resolved local path travels in the SessionPlan;
      connect() refuses when re-resolution differs from the identity the
      plan was built from.
- [ ] **C2.4 / F2.4** An advisory lock keyed by resolved endpoint
      identity, in a shared location independent of --state-root, so two
      state directories cannot own the same trees concurrently.

## Phase 6 — provenance must survive policy changes

- [ ] **C2.5** Reconciliation removes an ancestor entry only when both
      sides are genuinely absent; when either side is Untracked the
      entry is preserved, so provenance survives ignore/size/symlink
      policy changes and a deletion made while excluded stays a deletion.

## Phase 7 — property-based reconcile, and the full gate

- [ ] **V2** Property tests over generated trees: transitions converge
      the sides absent conflicts; no transition touches an
      already-agreed path; one-way-safe never emits an alpha transition;
      untracked content never appears in an emitted `new`.
- [ ] Full suite, clippy, fmt, crash script (single and fan-out), smoke.

## Documented rather than fixed, deliberately

- **C1.2** intent records before transitions (closes the last
  revert-window; architectural, designed for separately — the journal
  format was chosen to make it cheap to add).
- **C1.5** dirfd-relative traversal (symlink race; a transitioner-wide
  refactor, scheduled when that code is next open).
- **C1.3** stat-as-identity for forged timestamps (industry-standard
  tradeoff; F2.2 removes the un-forged case, a periodic full-rehash
  escape hatch is future work).
- **F2.3** NFS/SMB attribute caching (support boundary stated in README;
  detection-and-warn is future work).
- **F2.6** watcher case-fold marking (bounded by the 120s full scan).
- **C2.4-manual** `sync --state-dir` remains an escape hatch; Phase 5's
  endpoint lock closes the dangerous overlap.
