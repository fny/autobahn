# Safety rules

Every cycle: scan both sides (accelerated by a persisted cache —
unchanged files are never re-read), reconcile the two scans three-way
against the remembered ancestor, transfer only what is needed as
rsync-style deltas, stage incoming content safely off to the side,
verify its integrity, then swap it into place with atomic renames. A
file being written mid-transfer is detected by digest and simply retried
next cycle — partial content never lands.

Three-way reconciliation is what makes the rules below possible: with a
remembered baseline, autobahn knows the difference between "you deleted
this file" and "this file never existed here", so it never propagates a
deletion it cannot justify.

## The rules

- **Deleting or emptying a synchronization root halts the session**
  rather than propagating the deletion. A root that was a directory with
  real content, and is now empty or absent on exactly one side, is more
  likely an unmounted or wiped filesystem than an intentional mass
  deletion. A halt is a safety refusal: retrying will never clear it,
  and it needs a person.
- **Transitions verify on-disk state against what was scanned** before
  replacing or removing anything. Concurrent modifications become
  reported problems, never data loss.
- **A corrupt ancestor is an error, not a silent reset.** A reset would
  resurrect deletions. `autobahn reset` exists for when that is what you
  want, and requires the group name so it is never a default.
- **A missing *source* root is an error, not an empty source.** A typo'd
  path plus a mirroring mode must not empty the destination. This is
  also what stops a deletion from travelling through a nested session
  whose root was inside an ignored path.
- **Ignored content is never overwritten** — "do not synchronize this"
  cannot become "replace it with the peer's copy" — but it is removed
  along with a directory that is deleted around it. Content that could
  not be *read* blocks even that: nobody has seen what is there, so
  removing the directory around it is not a decision anyone made.
- **The default mode reports conflicts and touches nothing.** If both
  sides changed the same file, both versions survive until someone picks.

## Filesystems

Autobahn adapts to each filesystem it touches, probing per root:
executable bits are propagated around volumes that cannot store them,
names recompose to NFC on decomposing (HFS+-style) volumes, and
case-insensitive volumes refuse case-colliding siblings instead of
corrupting them.

## Further reading

The design and its reasoning are in [How autobahn works](./HOW-IT-WORKS.md).
The invariants the design claims, the code that enforces each one, the
tests that check it, and the residuals deliberately left open are in
[`correctness/INVARIANTS.md`](./correctness/INVARIANTS.md), which was
itself the subject of an independent adversarial review whose confirmed
findings are fixed.

## See also

- [Modes](./modes.md) — deletions propagate in every mode; what varies is the reverse
- [Overlapping and nested roots](./nesting.md) — why two ancestors over one region is refused
- [Support boundaries](./support-boundaries.md) — where these guarantees stop
