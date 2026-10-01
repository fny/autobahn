# PEER-7: Ancestor replica adoption and trust

**Findings:** H-11 (ASTRA F06), L-10 (KIMI ABN-L7), and fabricated history, issue 6 of the peering trust discussion.
**Status:** H-11 and L-10 fixed, 2026-09-30. The false-history mitigation proposed below is not done, on purpose (see below); a different one is, the same day: see "False history, checked".

**Follow-up, 2026-09-30: the honest failures of the same mechanism.** A copy that lagged when a switch happened left the next leader an older history, and adoption compared generation numbers, which say nothing once two histories have parted: a copy that lagged at a takeover carries on from where it lagged. The effects were deleted files coming back, spurious conflicts, and rarely an edit lost. Two fixes: a handoff brings the peer's copy level first, and the automatic handback waits until the alpha's copy is confirmed level (`Session::level_the_copy`); and adoption takes whichever store was written later on the host (`AncestorStore::last_written`), both being stamped by that host's clock. A lineage fingerprint was considered and not built: it would tell that two histories parted, not which is the later agreement.

**Why the mitigation was left out.** An alpha that keeps its own ancestor after a handback reconciles the attached session against history from before the failover. When the handback happens, the two trees are level, so most differences read as the same change on both sides. The window after the handback is the problem. A file created during the failover and deleted on the alpha before the alpha's first cycle reads as a creation on the beta against the old ancestor, and comes back. That undoes a user's deletion in the honest case to guard against a dishonest leader, and by the ticket's own account it does not stop careful fabrication.

**False history, checked (2026-09-30).** A copy is a leader's account of what was agreed, so adoption now checks it twice (`adopt_newer_copy`):
- **Provenance.** The agent notes the writer of every copy, from the lease its channel was accepted at (`AncestorCopy::written_by`). A host takes up a copy only from the member its session is with, by host. This stops a leader planting history for a session it is not part of, which let a host's stale files overwrite the other side's.
- **Consistency with this side.** A history is agreed by both sides, so where it records something other than what this host holds, this host's file must have changed since the copy was written, by the file's change time on the clock that stamped the copy. Paths where it has not are set aside: forgotten, so the next cycle reconciles them as new. A bad record turns into a conflict, never an overwrite. Unlike the mitigation below, the honest case loses nothing: a path an honest copy disagrees on is one a conflict holds, and stays one.

What is left is a leader stating things true of this host's own tree, which is no more than changing its own files and letting the session carry the change. The same checks catch a buggy leader.

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
