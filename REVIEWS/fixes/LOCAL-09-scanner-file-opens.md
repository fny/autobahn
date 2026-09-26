# LOCAL-09: Scanner opens files without following swaps

**Findings:** M-16 (KIMI ABN-M17; OPUS), and the file-level half of H-24 (KIMI ABN-H10).
**Status:** proposed. This is the cheap partial fix. The directory-level half is LOCAL-10.

## Problem

`digest_file` (`src/scan/mod.rs:1007-1022`) opens by path with no flags, after an `lstat` taken earlier. A file swapped in between can do three things:
- **A FIFO** blocks `open()` forever, and the session never completes a scan.
- **A symlink to `/dev/zero`,** or a file that keeps growing, reads forever at full CPU.
- **A symlink to a file outside the root** is digested and later supplied to the peer.

## Proposed resolution

Apply the T1-1 pattern to `digest_file`:
- Open with `O_NOFOLLOW | O_NONBLOCK`.
- `fstat` the handle. Require a regular file whose device and inode match the `lstat`.
- Read at most the `lstat` size plus a small allowance.

Any mismatch goes down the existing "changed during scan" path, so the entry is rescanned later rather than failing the scan.

## Tests

- A file swapped for a FIFO between `lstat` and open does not hang the scan. Use a test hook between the two.
- A file swapped for a symlink to an outside file is not digested.
- A file that grows during the read ends as "changed during scan".
