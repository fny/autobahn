# PEER-7: Ancestor replica adoption and trust

**Findings:** H-11 (ASTRA F06), L-10 (KIMI ABN-L7), and fabricated history, issue 6 of the peering trust discussion.
**Status:** deferred, not in v1. Documented in `docs/peering.md`.

## Problem

- **An older replica can overwrite newer local history.** `adopt_newer_copy` (`src/peering.rs:734`) treats a missing checkpoint file as generation zero and ignores a valid journal. An older replica then overwrites newer local history.
- **An oversized record can wedge a follower.** The 1 GiB record cap is enforced when reading the journal, not when appending to it (`src/session/ancestor.rs:721-731`). A record over the cap can be written, and every later open then fails.
- **A dishonest leader can send false history.** It then steers reconciliation. Defending fully would need independent rescans, which is out of scope under the tier 2 decision.

## Proposed resolution

- Use the journal-aware `stored_generation` for the local generation, and test for a replica by checking both the checkpoint and the journal.
- Enforce the record cap on append. An oversized change set is checkpointed instead of appended.
- Mitigation for false history: the alpha keeps its own ancestor as the authority for its own side, and adopts a replica only when it has no local history. This does not stop careful fabrication.

## Tests

- A journal-only local ancestor at generation 10 is kept when the replica is at generation 9.
- A journal-only replica at generation 9 is adopted over a local generation 5.
- A record over the cap is refused on append, and the journal still opens afterwards.
