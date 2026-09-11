# Modes

A mode is a direction and a policy. The direction is whether changes
flow both ways or only from alpha to beta. The policy is what happens
when the two sides disagree about a file: it is reported as a
**conflict** and left alone, or **alpha** wins.

| | conflict | alpha wins |
|---|---|---|
| **two-way** | `two-way-conflict` | `two-way-alpha` |
| **one-way** | `one-way-conflict` | `one-way-alpha` |

There is no default. Direction is never guessed, so a group (or the
defaults) must say which it wants.

## Which one do I want?

| Mode | Reach for it when… |
|---|---|
| `two-way-conflict` | You edit on both sides and want nothing lost, ever. |
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
direction. (A deletion large enough to look like a vanished disk halts
the session instead; see the [safety rules](./safety.md).)

The earlier spellings — `two-way-safe`, `two-way-resolved`,
`one-way-safe`, `one-way-replica` — are still accepted, so existing
configurations keep working.

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
