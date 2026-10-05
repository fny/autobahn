# Scope and Support Boundaries

## Supported Operating Environments

- **Operating Systems:**
  - **Linux:** `x86_64` and `aarch64` (glibc and musl).
  - **macOS:** Apple Silicon (`aarch64`) and Intel (`x86_64`). The desktop apps are Apple Silicon only. APFS case-folding, Unicode normalization forms, and atomic rename flags are natively handled.
- **Transport Mechanism:** Standard SSH (`OpenSSH`) or standard input/output subprocesses. No external daemon or port exposure required.
- **Root Geometry:** Synchronization roots must resolve to directories. Individual files cannot serve as root endpoints (synchronize the parent directory and ignore surrounding files).

## Architectural Boundaries & Constraints

### Filesystem Types: Local vs. Network Mounts

Autobahn requires local POSIX-compliant filesystems (e.g., ext4, XFS, Btrfs, APFS).

- **Network Mounts (NFS, SMB/CIFS, FUSE):** Supported only on a best-effort, single-writer basis. Remote file attribute caching can mask modifications from scanners and safety validation checks. Change events are frequently dropped or unsupported by the kernel driver. Autobahn warns when network mount points are detected.

### One Supervisor per Folder

A folder is synchronized by one supervisor. That supervisor may run as many sessions over it as the configuration asks for — fanning one source out to several destinations is a supported topology, because one supervisor decides in order what happens to the folder.

Two supervisors writing one folder is not supported. Each keeps its own record of what it last agreed, neither knows the other exists, and they can undo each other's work.

In ordinary use this costs nothing: there is one supervisor, the login service, and it owns everything in the configuration. A second one arrives only deliberately — a `--state-root` or `AUTOBAHN_HOME` override, a manual `autobahn sync` beside the running service, another user account, or another machine against shared storage.

What is enforced, and what is not:

|  |  |
| :-- | :-- |
| One supervisor per state root | Enforced — a lock on `<state-root>/supervisor`. |
| One session per _pair_ of folders | Enforced machine-wide per user, by a lock named for the pair and kept in the real `~/.autobahn` so an override cannot dodge it. |
| One supervisor per _folder_ | **Not enforced.** The pair lock catches the same two folders twice; it does not catch one folder paired with something different. |

The gap is the last row: two configurations that both name `~/Workspace`, each syncing it somewhere else, take different pair locks and both run. See [accepted risks §4](./correctness/accepted-risks.md#4-separate-supervisors-can-write-to-the-same-folder).

### Timestamp-Preserving File Rewrites

Tools that modify file contents while deliberately preserving file sizes and modification timestamps (`touch -r`, certain reproducible build packaging tools) evade standard mtime-based change detection.

- **Remediation:** Execute `autobahn verify <group>` to force full content-hashing on the subsequent synchronization pass.

### Live Multi-File Databases (e.g., SQLite WAL)

Active transactional databases frequently update multiple files concurrently (`.db`, `.db-wal`, `.db-shm`). While one-way mirroring propagates these changes cleanly once writes pause, bidirectional editing on both endpoints will inevitably generate file conflicts that cannot be resolved at the byte level.

- **Recommendation:** Exclude active databases from bidirectional groups or synchronize point-in-time database backups (`sqlite3 db.sqlite ".backup backup.sqlite"`).

### Linux Inotify Descriptor Limits

Large directory hierarchies on Linux require one `inotify` watch descriptor per directory.

- If `fs.inotify.max_user_watches` is exhausted, Autobahn logs a warning and falls back to polling at the group's `interval`. It retries the watch every 30 seconds, so raising the limit with `sysctl` takes effect without a restart:
  ```sh
  sudo sysctl -w fs.inotify.max_user_watches=524288
  ```

### Atomic File Saves on macOS

Certain editors (such as Vim or JetBrains IDEs) save files by creating a hidden temporary copy and renaming it over the destination.

- On Linux, Autobahn detects open write descriptors and delays synchronization until the file is closed.
- macOS `FSEvents` does not emit file close notifications; consequently, saving a very large file on macOS may require two rapid sync cycles to complete propagation. No data is lost.

## See Also

- [Safety](./safety.md): Guarantees and existing safeguards
- [Accepted Risks](./correctness/accepted-risks.md): Unresolved risks and possible fixes
- [Architecture](./architecture.md): Design choices behind the support boundaries
- [P2P](./p2p.md): Experimental failover and its access requirements
- [Roadmap](./wishlist.md): Proposed features and platform support
