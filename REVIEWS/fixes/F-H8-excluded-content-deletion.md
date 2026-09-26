# F-H8: Deleting a directory removes only pattern-ignored content with it

**Findings:** H-8 (OPUS H6; KIMI ABN-M11).
**Status:** proposed. High; fix before v1.

## Problem

When a directory is deleted, `remove_directory` removes any entry the last scan recorded as `Content::Untracked`, along with the directory (`src/endpoint/local.rs:2512-2560`). The comment there says "Excluded content goes with the directory around it." That is the documented design for *pattern-ignored* content (`docs/ignores.md`), such as `.git` and `node_modules`.

But the scanner records much more as `Untracked`. Besides ignored entries, it covers:
- files over `max_file_size`;
- FIFOs, sockets and devices;
- symlinks under the `ignore` symlink mode.

See `src/scan/mod.rs:949`, `:963`, `:981`, `:1023` and `:1122`. None of those is something a user asked to leave out of the sync, and `docs/configuration.md:73` promises that oversized files "stay on disk … never mistaken for deletions". Meanwhile the `blocking` comment in `src/tree/reconcile.rs:68-73` claims that a deletion "leaves the excluded entries where they are", which is the opposite of what the endpoint does.

Two shapes lose data:

1. **A beta-only large file.** Beta has `data/dump.sql`, 2 GB, over the limit and present only on beta. Alpha runs `rm -rf data`. Beta's transition removes `dump.sql` through the excluded-content branch, and that file existed nowhere else.
2. **An edit past the limit.** In `two-way-conflict`, alpha grows `d/a` past the limit, which is an edit, while beta deletes `d`. `alpha_diff` sees `a` as removed, the "both purely deletions" branch fires, and alpha's edited file is deleted.

## Proposed resolution

This needs no change to the snapshot or wire format.

- **At removal, remove only what an ignore pattern matches.** In the excluded-content branch, check the entry's path against the endpoint's own ignore set, which it already holds. If a pattern matches, remove it as today. If not, the entry was excluded by size, type or symlink mode: refuse, report a problem ("left in place: excluded by size" or "by type"), and leave the directory standing with that entry. The directory then survives only partly, which is the honest outcome.
- **In reconcile, an entry excluded on one side but in the ancestor is a change, not a deletion.** When a side records `Untracked` where the ancestor held synchronizable content, treat it as a change reconciliation can't see. It blocks a sibling or parent deletion, which becomes a conflict, the way unreadable content already does. This covers shape 2 whatever the reason for the exclusion. A file that became pattern-ignored also counts, which is a conservative result.
- **Fix the comments.** Make the `blocking` comment in `reconcile.rs` and the one at `local.rs:2531` say what the code does after the change. Keep the `docs/ignores.md` wording, which describes pattern ignores only.
- **Later, optional.** Carry an exclusion reason on `Untracked` (`Ignored`, `TooLarge`, `Special`) so reconcile can tell them apart without the ignore set. That changes the scan and wire formats and bumps the epoch. Ancestors are unaffected, because they hold only synchronizable content. Not needed for this fix.

## Tests

- **Shape 1:** a beta-only 2 GB-class file, with a small `max_file_size` in the test, under a directory alpha deletes. It survives on beta, a problem is reported, and the rest of the directory is gone.
- **Shape 2:** alpha grows `d/a` past the limit while beta deletes `d`. The result is a conflict, and alpha's `d/a` survives.
- A FIFO under a deleted directory survives, with a problem reported.
- **Regression:** an ignored `.git` under a deleted directory is still removed with it, as today.
- **Property test:** covered by F-C1's `Untracked`-at-every-depth generator and its "no silent loss" property.
