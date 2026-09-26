# F-M-STATE: The journal never loses records silently, and one-way modes converge

**Findings:**
- M-32: a corrupted length in a middle journal record reads as a torn tail (OPUS M1).
- M-33: one-way modes never converge when beta holds ignored content (OPUS M2, "likely").

**Status:** proposed. Medium. M-32 can roll the ancestor back silently, so do it first.

## Problems

- **M-32.** `read_journal` (`src/session/ancestor.rs:~843-870`) reads each record's header: generation, length and digest. If `start + length` runs past the end of the file, it assumes a torn tail and stops (`:852-856`). The digest covers the generation and the payload, but not the length.

  So one flipped bit that turns a middle record's length into a larger value looks exactly like a torn tail. Every later record, each one acknowledged, is dropped, and normalization then truncates the file, which makes the loss permanent. The ancestor rolls back to an older state without any error.

  A corrupted length that stays *within* the file already fails, as a digest mismatch, which is the correct outcome. Only lengths pointing past the end are misread.
- **M-33.** The one-way modes build a deletion with `old: beta.cloned()` (`src/tree/reconcile.rs:568`, `:658`). That is beta's raw node, untracked entries included. The two-way paths use the synced subset, `beta_sync`. When beta's copy holds ignored content, the deletion's expected old value doesn't match what the endpoint validates. Removal refuses, the next cycle proposes the same deletion, and it repeats forever. The existing test `unsynchronizable_content_never_travels` checks only `new`, so it can't catch this. OPUS marked this "likely", so confirm it with the test below first.

## Proposed resolution

- **M-32: checksum the header.**
  - **New record format.** A digest over the generation and the length, with the payload digest as today, as a new journal format version. Journals in the old format remain readable, and the next compaction rewrites them.
  - **Tell a torn tail from corruption.** A header whose own checksum is valid, but whose length runs past the end, is a torn tail. A header whose checksum is invalid is corruption, and fails closed, like any other corrupt record.
  - **Old-format journals, until rewritten.** Treat "runs past the end" as a torn tail only when the record is the last one that could start. That means no well-formed header can be found in the remaining bytes. Otherwise, fail closed.
- **M-33: use the synced subset.** In both one-way branches, pass `old: beta_sync.clone()`. Use the same helper as F-C1's reconcile work, so the choice of `old` is written down in one place.

## Tests

- **M-32:** write a journal of five records, then flip the length field of record two to point past the end. Opening it fails closed, rather than returning records one and two alone. A genuinely torn final record is still discarded as today (`a_torn_final_record_is_discarded`). An old-format journal with the same corruption fails closed too.
- **M-33:** in `one-way-alpha` and `one-way-conflict`, alpha deletes `d`, and beta's `d` holds an ignored `.git`. Within two cycles, `d` is removed on beta, or a conflict is reported in the conflict mode, and the same transition is never proposed three times.
- **Property test:** F-C1's `Untracked`-at-every-depth generator, run in the one-way modes.
