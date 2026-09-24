# Modes

A mode is a direction and a policy. The direction is whether changes flow both ways or only from alpha to beta. The policy is what happens when the two sides disagree about a file: it is reported as a **conflict** and left alone, or **alpha** wins.

| | conflict | alpha wins |
|---|---|---|
| **two-way** | `two-way-conflict` | `two-way-alpha` |
| **one-way** | `one-way-conflict` | `one-way-alpha` |
| **peering** (dangerously experimental) | `peering-conflict-dangerously-experimental` | `peering-alpha-dangerously-experimental` |

Off the grid there are two more. `two-way-paranoid` is `two-way-conflict` that also refuses to trust a large directory going empty or missing on one side; see [Large directories](#large-directories) below. `two-way-alpha-strict` is `two-way-alpha` with its one exception removed: alpha wins every collision, including when alpha's side of it is a deletion; see [Deletions against edits](#deletions-against-edits) below.

There is no default. Direction is never guessed, so a group (or the defaults) must say which it wants.

## Which one do I want?

| Mode | Reach for it when… |
|---|---|
| `two-way-conflict` | You edit on both sides and want nothing lost, ever. |
| `two-way-paranoid` | As above, and a disk that unmounts mid-session must not empty the other side. |
| `peering-conflict-dangerously-experimental` | As `two-way-conflict`, and a beta takes the lead while the alpha is away. See [Peering](./peering.md). |
| `two-way-alpha` | You edit on both sides but alpha is the truth when they collide. |
| `two-way-alpha-strict` | As above, and when alpha deletes something beta was editing, it stays deleted. |
| `one-way-conflict` | Deploy-ish flows where the remote side may hold extra files (logs, caches). |
| `one-way-alpha` | Backups, artifact distribution — beta should be *identical*. Also spelled `mirror`. |

## What each mode actually does

The modes differ only in seven situations. Everything else — an unchanged file, a new file on alpha, a rename — behaves identically in all of them.

| | `two-way-conflict` | `two-way-alpha` | `two-way-alpha-strict` | `one-way-conflict` | `one-way-alpha` |
|---|---|---|---|---|---|
| Alpha edits a file | → beta | → beta | → beta | → beta | → beta |
| Beta edits a file | → alpha | → alpha | → alpha | stays on beta, **reported as a conflict** | **overwritten** from alpha |
| Both edit the same file | **conflict**; both sides keep their own | alpha's version wins, silently | alpha's version wins, silently | **conflict**; both sides keep their own | alpha's version wins, silently |
| Alpha deletes a file | → beta | → beta | → beta | → beta | → beta |
| Alpha deletes a file beta edited | beta's edit comes back to alpha | beta's edit comes back to alpha | **deleted on beta too** | stays on beta, as if beta had created it | **deleted on beta too** |
| Beta creates a new file | kept | kept | kept | kept | **deleted** |
| Beta deletes a file | → alpha | → alpha | → alpha | restored from alpha | restored from alpha |

Three things in that table surprise people:

**`one-way-conflict` is not "ignore beta".** It refuses to overwrite anything beta changed, and *tells you* — a file edited on beta is reported as a conflict every cycle until you resolve it. That is the mode's whole point: alpha pushes outward, but never destroys work that appeared on the far side. If you want beta's edits silently discarded, you want `one-way-alpha`. Nothing ever flows back to alpha, either: when alpha deletes a file beta edited, the edit stays on beta, unreported, and synchronization stops tracking it, like a file beta created.

**`one-way-alpha` deletes files it has never seen.** Beta is made *identical* to alpha, so logs, caches, and anything else generated on beta are removed. Never point it at a directory the far side also writes to.

**Deletions propagate in every mode**, including the one-way ones — deleting on alpha deletes on beta. What varies is only the reverse direction. (A sync root that empties or vanishes on one side halts the session instead, in every mode; see the [safety rules](./safety.md).)

## Deletions against edits

A deletion carries no content, so when one side deletes a file the other side edited, there is nothing to weigh alpha's version against — and letting the deletion win destroys the only copy of the edit. So in `two-way-alpha`, as in `two-way-conflict`, the edit wins: it comes back to the side that deleted. This is the one collision `two-way-alpha` does not hand to alpha, and it shows up most often as a rename: alpha renames a file (a deletion plus a creation) while beta is editing it, and the old name reappears on alpha with beta's edit.

`two-way-alpha-strict` removes the exception. Alpha's deletion is final, and beta's edit under that name is removed with it; a rename on alpha stands. Beta's own additions still flow to alpha, which is what separates it from `one-way-alpha`. Reach for it when alpha is the one place edits are meant to happen and beta's are mistakes to be corrected, not work to be kept.

The earlier spellings — `two-way-safe`, `two-way-resolved`, `one-way-safe`, `one-way-replica` — are still accepted, so existing configurations keep working.

## Large directories

Three-way reconciliation reads a directory that is empty on one side and full on the other, where the last synchronized state had it full, as one side deleting every entry — and carries the deletions across. That is usually right. It is also exactly what an unmounted disk looks like (the mountpoint stays, empty), and what some tools leave behind (`git gc` packs a directory of loose refs and leaves the directory).

`two-way-paranoid` is `two-way-conflict` with two more rules for a directory the last synchronized state records with **eight or more entries** beneath it:

- **Emptied on one side** — present on both, empty on one — is reported as a **conflict** at the directory, and nothing beneath it moves until it is settled. `resolve --keep` the full side to restore the other; `--keep` the empty side to let the emptying through, which removes the directory everywhere.
- **Gone on one side** while the other holds exactly what was last synchronized is **restored**, not deleted. To remove such a directory under this mode, remove it on both sides, or empty it on one and resolve in that side's favour.

Smaller directories, and directories the other side also changed, follow the ordinary rules. The other four modes have neither rule: an emptied directory is deletions, and they propagate.

## Peering (dangerously experimental)

The peering modes are the two-way modes plus failover: while the alpha is away for longer than a configured wait, the first beta that is up leads the others, and the alpha gets the lead back when it returns. Reconciliation is unchanged — `peering-conflict-dangerously-experimental` reconciles as `two-way-conflict`, `peering-alpha-dangerously-experimental` as `two-way-alpha`, with the configured alpha winning wherever it is involved whoever leads. The whole of it is in [Peering](./peering.md).

## Modes and fan-out

When one alpha fans out to several betas, each destination is its own session, and the mode decides what happens when two betas change the same file at once. Under `two-way-conflict`, whichever lands first reaches alpha and the other session reports a conflict — both edits survive, one needs a human. Under `two-way-alpha`, the second edit overwrites the first everywhere, silently, because "alpha wins" and alpha is now whatever arrived most recently. Neither is wrong, but the second only suits a fan-out you push *from* rather than edit at both ends.

## See also

- [Configuration](./configuration.md) — where `mode` goes
- [Conflicts](./conflicts.md) — settling a disagreement once it is reported
- [Safety](./safety.md) — the deletions that are refused in every mode
- [Peering](./peering.md) — failover for the star, dangerously experimental; known security and collision issues
