# LOCAL-11: No in-place chmod on hardlinked files

**Findings:** L-3 (KIMI ABN-L3).
**Status:** proposed. It is cheap enough to do before LOCAL-10.

## Problem

When only the executable bit changes, the transition path chmods the file in place (`src/endpoint/local.rs:~2607`). A local user can hardlink a file they cannot change into the synced tree, and a sync then changes that file's mode.

On Linux, `fs.protected_hardlinks` usually stops the hardlink being created. macOS has no such protection. With a root agent (LOCAL-08), this could make `/etc/sudoers` world-readable or executable.

## Proposed resolution

Before an in-place mode change, check `st_nlink` on the opened file. If it is greater than 1, publish the mode change the way content changes are published: copy to a staging temporary, set the mode, and rename over the file. This breaks the link rather than changing the shared inode.

## Tests

- A file with a second hardlink outside the root has its executable bit changed through a sync. The outside link keeps its original mode.
