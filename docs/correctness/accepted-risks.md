# Accepted Risks

This document describes unresolved risks that limit [the invariants](./invariants.md). Each section explains what can go wrong, why the risk remains, when to reconsider it, and possible fixes.

## 1. An Unmounted Disk Can Look Like Deleted Files

**Risk.** Autobahn protects against missing mounts only when it previously recorded the path as a mount. If a disk is absent during the first scan, its mount directory looks like an ordinary empty directory.

The disk can later appear, and autobahn treats its files as creations. If the disk disappears again without the path entering `remembered`, autobahn treats those files as deletions. The mount guard does not intervene.

**Existing safeguards.** A scan records the mount boundaries it crosses in `sessions/<id>/mounts`. Each cycle checks these paths through `Session::account_for_mounts`. A path passes if it is still mounted or contains content.

The ancestor records the last state that both endpoints agreed on. If a remembered mount path is empty but the ancestor records children there, autobahn halts the session. With `ignore_mounts = true`, autobahn instead excludes the path on both sides.

For a mount that autobahn never recorded, only the general deletion safeguards remain:

- `guard_dir_deletes_over` limits deletions by entry count, but it is **off unless configured**. A single large file can stay below the count threshold.
- The guard against an empty root requires at least two ancestor entries. It does not protect an individual subdirectory.
- Autobahn separately refuses a missing primary.

**Why the risk remains.** An unmounted disk and an intentional deletion can produce the same result: an empty directory where files used to be. The directory contents alone cannot distinguish them. A byte threshold can block intentional large deletions without proving that a mount is missing.

**When to reconsider.** Data loss from disks absent during the first scan can justify further work. So can a deployment that can declare its expected mounts before synchronization.

**Possible fixes.** An expected-mount configuration can identify missing disks, but only if the list stays current. An omitted mount has no protection.

The scanner can also ask the kernel whether each directory is a mount point, even without a previous mount record. It can use `statfs` or compare `st_dev` with the parent directory. The scanner already visits these directories.

This reduces the risk to mounts that are absent whenever autobahn checks. No scan can distinguish a disk absent at every scan from an ordinary empty directory.

## 2. A File Can Change After the Last Check

**Risk.** Before autobahn replaces or removes an entry, it checks that the entry still matches the last scan. A program can save changes between that check and the operation. Autobahn catches and restores most such saves, but these cases remain:

- **Replacement without atomic exchange.** FreeBSD, other BSDs, and some network and FUSE filesystems do not support the required exchange operation. A save after the final check can be overwritten. The check occurs immediately before replacement, so the remaining window is microseconds.
- **Writes through an open file.** A program can keep a file open while autobahn replaces or removes it. Later writes go to the old version, which autobahn deletes. On Linux, autobahn checks for open files and waits up to 30 seconds. Writes after that limit can be lost. So can writes from a program that opens the file between the check and the operation. macOS has no equivalent check, and network filesystems do not grant the leases that this check requires.
- **Creation without a no-replace flag.** Linux uses `RENAME_NOREPLACE`, and macOS uses `RENAME_EXCL`, to prevent creation over a file that appeared after the check. Other platforms, and filesystems without these flags, retain this race.

**Existing safeguards.** `Transitioner::put_in_place` and `Transitioner::remove_checked_file` in `src/endpoint/local.rs` protect replacements and removals in these ways:

- **A final check on every platform.** Autobahn checks the target again immediately before the operation. This catches saves made during the seconds that autobahn can spend copying or verifying a large replacement file. Test: `a_save_landing_while_the_replacement_is_prepared_is_never_replaced`.
- **Atomic exchange on Linux and macOS.** Autobahn swaps the files with `RENAME_EXCHANGE` on Linux or `RENAME_SWAP` on macOS. It then checks the displaced file against the validated version. If the displaced file contains a new save, autobahn swaps it back and rejects the replacement. The next cycle reconciles the disagreement. Test: `a_save_landing_after_the_last_check_is_swapped_back_not_replaced`.
- **Rename before removal on every platform.** Autobahn moves the file aside and checks it again. It then deletes the file or restores it. Test: `a_save_landing_while_a_file_is_removed_is_put_back`.
- **A check for open files on Linux.** Autobahn requests a write lease, which succeeds only if no other program has the file open. It immediately releases the lease. If another program holds the file open, autobahn defers replacement for up to 30 seconds. Tests: `a_file_another_program_has_open_is_left_for_now_then_replaced` and `a_file_held_open_past_the_grace_is_replaced_anyway`.

If autobahn cannot restore a saved file, it keeps the file beside the original path as `<name>.kept` and reports it.

**Resolved risk: symbolic links that redirect operations outside the root.** The old implementation let a local process replace a checked directory with a symbolic link before autobahn acted. This redirected operations outside the synchronization root. With `--allow-root`, the affected paths included anything accessible to the daemon.

Autobahn now resolves the directory once and holds each directory open through a descriptor. On Linux, it uses `openat2` with `RESOLVE_BENEATH`. Elsewhere, it uses `openat` with `O_NOFOLLOW`. Operations use the held descriptor and do not follow symbolic links. This mechanism is in `src/endpoint/dir.rs`.

File reads, moves, and delta-base opens use this directory walk. Staging also uses a directory descriptor held open from the initial check. With `staging = "inside-root"`, a local writer can replace the staging directory with a link. That replacement does not redirect staging operations.

These tests deliberately replace directories with links during the vulnerable window. They show that operations stay in the resolved directory:

- `a_held_directory_is_not_redirected_by_a_link_swapped_in_for_it`
- `a_parent_replaced_by_a_link_after_resolution_does_not_redirect_a_move`
- `a_staging_directory_replaced_by_a_link_after_its_check_does_not_redirect_staging`.

**Remaining exception: supplying content.** `open_scanned` still opens a file by name, outside this directory walk. It compares the opened file's inode and size with the scan to reject redirected opens. On filesystems without inode numbers, it compares only the size.

**Why the risk remains.** The remaining save races require an atomic compare-and-replace operation or cooperation from the program that writes the file. No platform offers that atomic operation. These races require a write during a microsecond window, or continued writes through an open file beyond the grace period.

**When to reconsider.** Lost saves from programs that write in place on macOS can justify more protection. So can a requirement for the same guarantees on BSD systems.

**Possible fixes.** Autobahn can retain a replaced file for a few seconds and check it again before deletion. This can catch further writes through an open file. Content supply can also use the same directory walk as reads and moves.

## 3. Network Filesystems Can Hide Changes

**Risk.** Autobahn warns at startup for NFS, SMB/CIFS, and FUSE roots. Support is best effort and assumes a single writer.

Attribute caches can hide another client's writes from both scans and checks before destructive operations. NFS defaults can cache attributes for up to 60 seconds. Filesystem watcher events can also be absent.

**Why the risk remains.** Fixes depend on the protocol and server. Close-to-open consistency requires autobahn to reopen files on every read path. Lease handling also varies. Neither approach restores the notification behavior of a local filesystem.

**When to reconsider.** Support for network mounts with multiple clients requires a broader product commitment. Evidence of common NFS use with a single writer can justify narrower improvements.

**Possible fixes.** The first step is `fstat` after `open` on destructive paths. A further option is a mode that accounts for the mount type and disables digest reuse. Tests against a real NFS server are necessary before autobahn expands its support guarantees.

## 4. Separate Supervisors Can Write to the Same Folder

**Risk.** Autobahn supports [one supervisor per folder](../limitations.md#one-supervisor-per-folder), but it does not enforce that limit. Two configurations can pair the same folder with different endpoints. Each supervisor then writes to the shared folder using its own ancestor.

**Existing safeguards.** Within one configuration, the loader rejects nested writable endpoints and warns about equal endpoints. Equal endpoints support fan-out: one source synchronized to several destinations. One supervisor coordinates those sessions.

The endpoint-pair lock prevents a second run of the same pair. It applies across the machine for one user and uses the real `~/.autobahn` directory. Neither `--state-root` nor `--state-dir` bypasses it. Different machines, users, and `AUTOBAHN_HOME` directories are outside its scope.

Different endpoint pairs take different locks, even if they share a folder. As a result, the locks allow the conflicting configurations described here.

**Why the risk remains.** The folder relationships are the same as supported fan-out within one configuration. Separate processes lack the supervisor that coordinates those sessions. Rejecting all shared endpoints also rejects fan-out. Allowing separate supervisors leaves enforcement to the documented restriction.

A full fix requires endpoint locks on every host, including agents. The protocol must handle stale locks, lock acquisition order, and deadlocks between supervisors. Intent records already reduce the consequences: concurrent runs tend to produce conflicts instead of silently replacing files.

**When to reconsider.** Broader changes to the agent protocol can provide an opportunity to add endpoint locks. Evidence of common synchronization from multiple machines into shared storage can also justify the work.

**Possible fix.** Advisory locks can use the resolved endpoint identity under the default state root of the endpoint host. Read-only primaries in one-way synchronization can use shared locks. Writable endpoints require exclusive locks. The existing pair lock can remain.

## 5. Changed Content Can Retain the Same Metadata

**Risk.** A scan reuses a recorded checksum if the file's modification time, size, inode, and type are unchanged (`src/scan/mod.rs`). A program can rewrite a file in place, keep its length, and restore its timestamp. All four values then match, so both full and incremental scans miss the changed content.

Reproducible build tools deliberately set fixed timestamps. Their output can contain different bytes but appear unchanged to the scanner. `touch -r` also copies timestamps deliberately. A writer can use these techniques to hide changes.

The racy-timestamp margin protects against accidental edits within one timestamp interval. It does not protect against deliberate timestamp restoration.

**Why the risk remains.** Checksum reuse lets scans skip most file contents and accounts for much of autobahn's speed. rsync, Git's index, and Mutagen make the same tradeoff.

**When to reconsider.** Reports of build output that silently fails to synchronize can justify changes. So can a deployment that must detect deliberate metadata restoration.

**Available mitigation.** `autobahn verify` already forces autobahn to read file contents on the next cycle. A regular schedule limits how long a hidden change can remain undetected. For example, cron can request verification each week:

```
0 3 * * 0  autobahn verify
```

A systemd timer or launchd job can do the same. No new feature is necessary. An internal configuration option only moves the schedule into autobahn.

**Verification does not detect tampering.** `verify` only disables checksum reuse. Autobahn reconciles each discovered change normally and does not identify it as a previously hidden change. If verification finds a tampered file, autobahn propagates it like any other edit. Before verification, the hidden change stays local only because autobahn has not detected it.

Scheduled verification supports correctness for build output. It does not provide tamper detection or containment.

## 6. P2P Trusts Every Machine in the Group

**Risk.** P2P is dangerously experimental because its trust model gives every replica access to its peers. This access allows leadership to move between hosts.

By default, peer traffic uses SSH without command restrictions. A key that allows leadership transfer also allows arbitrary commands on other peers as the SSH user. One compromised member can compromise the rest of the group.

**Optional safeguards.** Two settings restrict peer access. Both are off by default:

- `manage_keys = true` gives each replica a dedicated key with the gate restriction `restrict,command="…autobahn-gate gate"`. The gate allows only `agent`, `p2p attach`, and a signed `gate install`.
- `roots = [...]` in `~/.autobahn/host.toml` limits the directory trees that an agent serves. Without this restriction, even a gated agent runs with the user's privileges and serves any requested path.

**Unresolved leadership collisions.** The leadership lease is a file at `~/.autobahn/p2p/lease.json`. Autobahn renews it once per cycle. Handling collisions between two peers that both believe they lead remains unresolved. `src/config.rs` records this beside the mode names.

**Why the risk remains.** The configuration template and `docs/p2p.md` label the mode dangerously experimental. The mode name itself is `p2p-conflict-dangerously-experimental`, which the configuration must explicitly select.

The access restrictions exist and are documented. They remain off by default because they change what peers can do to each other.

**When to reconsider.** These issues must be resolved before P2P loses the "dangerously experimental" label. That requires restricted keys and a root whitelist by default, plus a resolution for leadership collisions.

**Possible fixes.** `manage_keys` can default to true so that the gate restricts peer commands by default. If a host lacks `host.toml`, autobahn can refuse the P2P group, as it already refuses a missing ignore file. See [P2P security boundaries and access control](../p2p.md#security-boundaries--access-control).

## See Also

- [Invariants](./invariants.md): Guarantees, implementation references, and tests
- [Safety](../safety.md): An overview of the safeguards
- [Limitations](../limitations.md): Supported environments and operational restrictions
- [Configuration](../configuration.md): Deletion guards, mount handling, and host access policy
- [Commands](../commands.md): The `verify` command for forced content reads
- [P2P Security](../p2p.md#security-boundaries--access-control): Restricted SSH keys and directory access controls
