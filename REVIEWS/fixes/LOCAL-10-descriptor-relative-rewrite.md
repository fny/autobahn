# LOCAL-10: Descriptor-relative scanning and transitions

**Findings:** the directory-level half of H-24 (KIMI ABN-H10; OPUS), M-5 race part (KIMI ABN-M3; DEEPSEEK F14), L-5 (KIMI ABN-L6).
**Status:** documented boundary. Not scheduled. This ticket records the eventual fix and the documentation change.

## Problem

The scanner and the transition code work by path. A local process that can write the synced tree can swap a directory for a symlink between a check and the use that follows it:

- **Scanner (H-24).** The swapped subtree is scanned as if it were inside the root, and its files are then supplied to the peer. That is a read leak. For example, a sandboxed app allowed to write a project folder, but not read `~/.ssh`, could use it.
- **Transitions (M-5).** A create, rename, remove or chmod lands outside the root.
- **Watch setup (L-5).** A watch is added on an outside directory. This only causes extra full scans.

`docs/correctness/RETAINED.md` §2 accepts the write half under the single-user model, because such a process can already write the tree. It does not mention reads.

## Decision

This stays a documented boundary. LOCAL-08 keeps the dangerous case, a root agent, behind an explicit override, and LOCAL-09 closes the file-level swap.

## Proposed resolution, for when it is scheduled

- **Scanner.** Walk with directory file descriptors. Open each child with `openat(parent_fd, name, O_DIRECTORY | O_NOFOLLOW)`, list it with `fdopendir`, and stat entries with `fstatat(AT_SYMLINK_NOFOLLOW)`. On Linux, `openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS` gives the guarantee directly. This needs `rustix` or `libc`, because std cannot read a directory from a descriptor. The same change also removes the per-entry full-path lookup, which OPUS noted as a performance cost.
- **Transitions.** `resolve_parent` returns an open parent directory descriptor. Every operation becomes relative to it: `mkdirat`, `symlinkat`, `renameat` (or `renameat2`), `unlinkat`, and `fchmodat` with `AT_SYMLINK_NOFOLLOW`.
- **Watches.** Pass `IN_DONT_FOLLOW` on Linux. Where the watcher library cannot pass it, `lstat` again after adding the watch and drop the watch if the entry is no longer a directory.

## Documentation change, now

Extend RETAINED §2:
- The boundary covers reads as well as writes, and names H-24.
- Running the agent as root needs the LOCAL-08 override. That deployment turns these races into privilege escalation and is the trigger for scheduling this ticket.
