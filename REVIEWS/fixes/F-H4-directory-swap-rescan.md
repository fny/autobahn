# F-H4: An incremental scan re-reads a directory that was replaced

**Findings:** H-4 (OPUS H2, reproduced).
**Status:** proposed. High; fix before v1.

## Problem

`DirtyPaths::mark` (`src/scan/mod.rs:152-163`) sets `relist = true` only on the marked path's *parent*. The marked entry itself gets a dirty node with `relist = false` and no children.

When that entry is a directory, `scan_directory` (`:588` onward) takes its "entry list unchanged" branch. It walks the *baseline's* children, and because none of them is marked, it adopts every one as it stands, without reading the disk.

So a directory replaced by rename keeps its old contents in the scan. OPUS synced a tree from A to B, then ran `mv A/live A/old && mv A/staging A/live` on A. Afterwards B showed:
- `live/f1` still holding the old content;
- `old/f1` holding the old content too;
- `staging/` deleted.

The new content existed only on A until the periodic full walk, up to 120 s later. Lease validation prevents a destructive overwrite, but for two minutes the replica holds the wrong tree and has lost the only second copy of `staging/`.

Triggers:
- atomic deploy swaps;
- `rmdir x; mkdir x; populate`;
- renaming a directory over an empty one.

This breaks invariant I1: the observer was told about the change and still served a snapshot that didn't reflect it. It also breaks the documented promise that an incremental scan equals a full one.

## Proposed resolution

- **Relist the marked entry.** In `mark`, set `relist = true` on the marked entry's own node as well as on its parent:
  ```rust
  node.relist = true;
  if let Some(name) = name {
      node.children.entry(name.to_owned()).or_default().relist = true;
  }
  ```
  For a file, `relist` is never consulted. For a directory, it costs one `readdir` per marked directory per scan. A new or renamed directory then has its entries listed and compared, and anything that isn't in the baseline is walked.
- **Optional:** also compare a marked directory's `(dev, ino)` with the baseline's, where the baseline records it, and treat a mismatch as an unmarked new directory, which means a full walk of it. Relisting alone catches a swap only one level deep. The inode check catches replaced contents at any depth.
- **Recursive events.** Check the macOS FSEvents path too. There, a whole-subtree event should already mark recursively. Confirm it reaches the same relist.

## Tests

- **Scan-level, extending `incremental_scans_agree_with_full_scans`,** for three shapes: a swap with `mv live old && mv staging live`, a `rmdir x; mkdir x` followed by populating it, and a rename over an empty directory. In each case the incremental scan must equal a full scan.
- **Session-level:** repeat OPUS's reproduction. After one cycle, B shows the new `live/`, and `staging/` has gone only because it was moved.
- **Mutation check:** revert the one-line change, and confirm the swap case fails.
- **Randomized ops:** add a directory-swap operation to the observer's randomized op set.
