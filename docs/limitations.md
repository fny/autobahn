# Scope and Support Boundaries

## Supported Operating Environments

- **Operating Systems:**
  - **Linux:** `x86_64` and `aarch64` (glibc and musl).
  - **macOS:** Apple Silicon (`aarch64`). APFS case-folding, Unicode normalization forms, and atomic rename flags are natively handled.
- **Transport Mechanism:** Standard SSH (`OpenSSH`) or standard input/output subprocesses. No external daemon or port exposure required.
- **Root Geometry:** Synchronization roots must resolve to directories. Individual files cannot serve as root endpoints (synchronize the parent directory and ignore surrounding files).

## Architectural Boundaries & Constraints

### Filesystem Types: Local vs. Network Mounts
Autobahn requires local POSIX-compliant filesystems (e.g., ext4, XFS, Btrfs, APFS).
- **Network Mounts (NFS, SMB/CIFS, FUSE):** Supported only on a best-effort, single-writer basis. Remote file attribute caching can mask modifications from scanners and safety validation checks. Change events are frequently dropped or unsupported by the kernel driver. Autobahn warns when network mount points are detected.

### Multi-Controller Root Locking
Two sessions synchronizing the identical pair of roots are locked machine-wide per user account to prevent race conditions.
- **Unsupported Topology:** Running two separate Autobahn controllers from different user accounts, distinct machines, or divergent `AUTOBAHN_HOME` locations against the same directory pairs bypasses endpoint lock mechanisms and can cause race conditions.

### Timestamp-Preserving File Rewrites
Tools that modify file contents while deliberately preserving file sizes and modification timestamps (`touch -r`, certain reproducible build packaging tools) evade standard mtime-based change detection.
- **Remediation:** Execute `autobahn verify <group>` to force full content-hashing on the subsequent synchronization pass.

### Live Multi-File Databases (e.g., SQLite WAL)
Active transactional databases frequently update multiple files concurrently (`.db`, `.db-wal`, `.db-shm`). While one-way mirroring propagates these changes cleanly once writes pause, bidirectional editing on both endpoints will inevitably generate file conflicts that cannot be resolved at the byte level.
- **Recommendation:** Exclude active databases from bidirectional groups or synchronize point-in-time database backups (`sqlite3 db.sqlite ".backup backup.sqlite"`).

### Linux Inotify Descriptor Limits
Large directory hierarchies on Linux require one `inotify` watch descriptor per directory.
- If `fs.inotify.max_user_watches` is exhausted, Autobahn logs a warning and automatically falls back to interval polling every 30 seconds until the limit is increased via `sysctl`:
  ```sh
  sudo sysctl -w fs.inotify.max_user_watches=524288
  ```

### Atomic File Saves on macOS
Certain editors (such as Vim or JetBrains IDEs) save files by creating a hidden temporary copy and renaming it over the destination.
- On Linux, Autobahn detects open write descriptors and delays synchronization until the file is closed.
- macOS `FSEvents` does not emit file close notifications; consequently, saving a very large file on macOS may require two rapid sync cycles to complete propagation. No data is lost.
