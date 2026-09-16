# Modes

A mode is a direction and a policy. The direction is whether changes
flow both ways or only from alpha to beta. The policy is what happens
when the two sides disagree about a file: it is reported as a
**conflict** and left alone, or **alpha** wins.

| | conflict | alpha wins |
|---|---|---|
| **two-way** | `two-way-conflict` | `two-way-alpha` |
| **one-way** | `one-way-conflict` | `one-way-alpha` |
| **peering** (experimental) | `peering-conflict-experimental` | `peering-alpha-experimental` |

Off the grid there is one more, `two-way-paranoid`: `two-way-conflict`
that also refuses to trust a large directory going empty or missing on
one side. See [Large directories](#large-directories) below.

There is no default. Direction is never guessed, so a group (or the
defaults) must say which it wants.

## Which one do I want?

| Mode | Reach for it when… |
|---|---|
| `two-way-conflict` | You edit on both sides and want nothing lost, ever. |
| `two-way-paranoid` | As above, and a disk that unmounts mid-session must not empty the other side. |
| `peering-conflict-experimental` | As `two-way-conflict`, and a beta takes the lead while the alpha is away. See [Peering](./peering.md). |
| `two-way-alpha` | You edit on both sides but alpha is the truth when they collide. |
| `one-way-conflict` | Deploy-ish flows where the remote side may hold extra files (logs, caches). |
| `one-way-alpha` | Backups, artifact distribution — beta should be *identical*. Also spelled `mirror`. |

## What each mode actually does

The modes differ only in six situations. Everything else — an unchanged
file, a new file on alpha, a rename — behaves identically in all four.

| | `two-way-conflict` | `two-way-alpha` | `one-way-conflict` | `one-way-alpha` |
|---|---|---|---|---|
| Alpha edits a file | → beta | → beta | → beta | → beta |
| Beta edits a file | → alpha | → alpha | stays on beta, **reported as a conflict** | **overwritten** from alpha |
| Both edit the same file | **conflict**; both sides keep their own | alpha's version wins, silently | **conflict**; both sides keep their own | alpha's version wins, silently |
| Alpha deletes a file | → beta | → beta | → beta | → beta |
| Beta creates a new file | kept | kept | kept | **deleted** |
| Beta deletes a file | → alpha | → alpha | restored from alpha | restored from alpha |

Three things in that table surprise people:

**`one-way-conflict` is not "ignore beta".** It refuses to overwrite
anything beta changed, and *tells you* — a file edited on beta is
reported as a conflict every cycle until you resolve it. That is the
mode's whole point: alpha pushes outward, but never destroys work that
appeared on the far side. If you want beta's edits silently discarded,
you want `one-way-alpha`.

**`one-way-alpha` deletes files it has never seen.** Beta is made
*identical* to alpha, so logs, caches, and anything else generated on
beta are removed. Never point it at a directory the far side also
writes to.

**Deletions propagate in every mode**, including the one-way ones —
deleting on alpha deletes on beta. What varies is only the reverse
direction. (A sync root that empties or vanishes on one side halts the
session instead, in every mode; see the [safety rules](./safety.md).)

The earlier spellings — `two-way-safe`, `two-way-resolved`,
`one-way-safe`, `one-way-replica` — are still accepted, so existing
configurations keep working.

## Large directories

Three-way reconciliation reads a directory that is empty on one side and
full on the other, where the last synchronized state had it full, as
one side deleting every entry — and carries the deletions across. That
is usually right. It is also exactly what an unmounted disk looks like
(the mountpoint stays, empty), and what some tools leave behind (`git
gc` packs a directory of loose refs and leaves the directory).

`two-way-paranoid` is `two-way-conflict` with two more rules for a
directory the last synchronized state records with **eight or more
entries** beneath it:

- **Emptied on one side** — present on both, empty on one — is reported
  as a **conflict** at the directory, and nothing beneath it moves until
  it is settled. `resolve --keep` the full side to restore the other;
  `--keep` the empty side to let the emptying through, which removes the
  directory everywhere.
- **Gone on one side** while the other holds exactly what was last
  synchronized is **restored**, not deleted. To remove such a directory
  under this mode, remove it on both sides, or empty it on one and
  resolve in that side's favour.

Smaller directories, and directories the other side also changed, follow
the ordinary rules. The other four modes have neither rule: an emptied
directory is deletions, and they propagate.

## Peering (experimental)

The peering modes are the two-way modes plus failover: while the alpha
is away for longer than a configured wait, the first beta that is up
leads the others, and the alpha gets the lead back when it returns.
Reconciliation is unchanged — `peering-conflict-experimental` reconciles
as `two-way-conflict`, `peering-alpha-experimental` as `two-way-alpha`,
with the configured alpha winning wherever it is involved whoever leads.
The whole of it is in [Peering](./peering.md).

## Modes and fan-out

When one alpha fans out to several betas, each destination is its own
session, and the mode decides what happens when two betas change the
same file at once. Under `two-way-conflict`, whichever lands first reaches
alpha and the other session reports a conflict — both edits survive,
one needs a human. Under `two-way-alpha`, the second edit overwrites the
first everywhere, silently, because "alpha wins" and alpha is now
whatever arrived most recently. Neither is wrong, but the second only
suits a fan-out you push *from* rather than edit at both ends.

## See also

- [Configuration](./configuration.md) — where `mode` goes
- [Conflicts](./conflicts.md) — settling a disagreement once it is reported
- [Safety](./safety.md) — the deletions that are refused in every mode
- [Peering](./peering.md) — failover for the star, experimental
