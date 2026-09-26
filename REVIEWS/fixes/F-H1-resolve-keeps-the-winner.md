# F-H1: `resolve --keep` never deletes the version it keeps

**Findings:** H-1 (ASTRA F01, reproduced; OPUS H1).
**Status:** proposed. Stage 1 is for v1. Stage 2 follows.

## Problem

`run_resolve` (`src/main.rs:2487`) settles a path by *retiring the losing side's copy*. It removes that copy through a transition, then flushes, and leaves the ordinary cycle to carry the winner across (`:2684-2703`). It never touches the ancestor.

That works only when the winner has changed since the ancestor. On the next cycle the loser then looks deleted, the winner looks edited, and the edit propagates. When the winner still matches the ancestor, the loser looks deleted and the winner looks unchanged. Three-way reconciliation then propagates the **deletion**, and the file the user kept is gone from every side.

The command accepts exactly that shape. When no recorded conflict matches, "the paths are taken at their word" (`:2592-2600`), which is how a file is forced to match a side without a conflict. Triggers:

- **Resolving an in-sync path**, for example to force it to match a side. ASTRA reproduced this: `resolve … keep.txt --keep alpha --yes` reported "one version kept", and `keep.txt` vanished from both roots.
- **Running the same `resolve` twice,** or clicking a stale tray menu entry after a cycle already settled the conflict. Tray actions are queued.
- **`resolve group ./`,** which normalizes to `""`. `node_at` returns the root, so the whole synchronizable tree is retired.
- **`--keep both` on an in-sync path,** which renames the file on every side rather than keeping it.
- **Keeping beta in a one-way mode.** The one-way modes never carry beta's content to alpha, so retiring alpha's copy can't make beta's win. In `two-way-alpha-strict`, alpha's deletion beats beta's edit, so keeping beta deletes it.

## Stage 1: refuse what would delete the winner (v1)

A guard, with no change to how resolution works:

- **Refuse the root.** An empty or root path is an error.
- **Skip paths that already agree.** `resolve` already scans both sides. If the winner and the loser already hold the same content at a path, report "already the same on every side" and retire nothing. That covers repeated resolves, stale tray clicks, and most stale conflict records.
- **Refuse when the winner still matches the ancestor.** Open the session's ancestor read-only. The journal is append-only, and a torn tail reads as absent. Refuse any path where the winner's content equals the ancestor's, with a message: "keeping <side> here would delete it: <side> has not changed since the last sync, so removing the other copy reads as a deletion. Edit the file on <side> first, or wait for the fix to forcing a match." If the ancestor can't be read, refuse rather than guess.
- **Refuse modes that can't do it.** Refuse `--keep <beta>` in the one-way modes. Refuse it in `two-way-alpha-strict` when alpha would lose by deletion. Name the mode in the message.
- **`--keep both` follows the same rules.** Skip it when the sides agree, and refuse it when the kept name would be deleted.

## Stage 2: resolution states its outcome (after v1)

Retiring the loser can't express "make both sides this version" when the winner is unchanged. The ancestor has to say so. Proposed:

- **Forget the path in the ancestor.** Resolution retires the loser, and also removes the path from the ancestor, through a journal record. The next cycle then sees the winner as a creation, with the ancestor absent, the winner present and the loser absent. A creation propagates in every two-way mode, for a file, a symlink or a whole directory. For `--keep both`, the renamed copy is a creation too.
- **Go through the owner.** The ancestor belongs to the session worker, and `resolve` currently takes no session lock. When a supervisor is running, send the resolution over the control socket as a new request. The worker applies it between cycles, holding its own lock: retire, forget, then cycle. When none is running, `resolve` takes the session lock itself and writes the journal record.
- **One-way modes.** Keeping beta stays refused. It can't be expressed without a mode that carries beta's content to alpha.
- **Fan-out.** When the winner is one of several betas, the ancestor of every session in the group forgets the path, not just the winning session's.

## Tests

- **Stage 1**, as supervisor or CLI integration tests:
  - an in-sync file, resolved with `--keep alpha` twice, survives on both sides, and the second run says the sides are already the same;
  - `resolve group ./` is refused;
  - a path where only beta changed, resolved with `--keep alpha`, is refused with the "would delete" message and both sides are untouched;
  - `--keep <beta>` in `one-way-alpha` is refused.
- **Stage 2:**
  - every mode and winner, on an already-agreed file, a real conflict and a directory, stays correct over three more cycles;
  - a resolve sent while a cycle is running is applied after that cycle;
  - with fan-out, the other betas end with the winner's version.
- **Mutation check.** Remove the stage 1 guard, and confirm the in-sync test fails by losing the file.
