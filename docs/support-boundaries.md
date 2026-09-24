# Scope and support boundaries

## Scope

Unix only. The full test suite runs green on **Linux (x86-64 and arm64) and macOS (Apple Silicon)**, and CI covers all three. macOS is a first-class target, not a build target: its Unicode normalization, case-folding, and atomic-creation behaviors are implemented against the platform's own primitives and exercised on real APFS volumes. Windows would be a port rather than a build target.

Transport is SSH (or any stdio subprocess) — no Docker, no daemon, no port forwarding.

Roots must be directories. A single file cannot be a root; sync its parent and ignore the rest.

## Support boundaries

- **Local filesystems.** Synchronization roots are expected to live on local filesystems (ext4, XFS, APFS, and the like). Network mounts — NFS, SMB/CIFS, FUSE — are best-effort: client-side attribute caching can hide another client's writes from both scanning and the checks that guard destructive operations, change notification is absent or incomplete, and lock semantics depend on the server. If a root must live on a network mount, treat this client as the only writer. autobahn prints a warning when it detects such a root.
- **One owner per pair of trees.** Two sessions synchronizing the same pair of roots are excluded per user on one machine, even across different `--state-root`/`--state-dir` settings. *Sharing* one root across sessions is fine and pinned by tests — fan-out, star and relay topologies all work, because those sessions share one watcher and one scan of that root. What is not supported is the same pair of trees driven from *different machines*, different Unix users, or different state roots: the exclusion lock is local to one of those, so nothing detects the overlap and deliberate changes can be silently undone.
- **Timestamp-preserving rewrites.** A tool that rewrites a file with identical length while restoring its modification time (reproducible builds, `touch -r`) defeats metadata-based change detection, as it does in every synchronizer of this design. `autobahn verify` is the escape hatch: the next cycle re-reads every byte, so such content becomes visible and is synchronized normally.
- **Live databases and other multi-file formats.** A SQLite database is three interdependent files changing many times per second, and a synchronizer captures them file by file. Syncing one *in one direction* works — the copy lags while writes are in flight and catches up within a cycle or two of them stopping — but a database written on **both** sides produces a conflict that nothing can merge, because two diverged databases cannot be reconciled as bytes. Keep such files on one side (ignore them, or use a one-way mode), or synchronize a snapshot (`sqlite3 app.db ".backup snap.db"`) rather than the live file.
- **Two git checkouts.** Keeping two working copies in sync *with* their `.git` works, with the machine-local parts of it ignored: `.git/index` (a cache of this machine's stat data), lock files, `.git/logs`, `gc.pid`, `FETCH_HEAD`, `ORIG_HEAD`, `COMMIT_EDITMSG`. Objects, packs, refs, `HEAD` and config carry across cleanly; `bench/git-sync.sh` runs commits on either side, a branch, a checkout, a `gc`, a push and a fetch, and both repositories pass `git fsck` and agree on every ref after each. The one wart: a checkout on one side moves the other side's working tree and `HEAD` but not its index, so `git status` there shows the whole difference as staged until a `git reset`. Run `gc` on one side only.
- **Inotify watch limits on Linux.** A large tree needs one watch per directory. When the kernel's `fs.inotify.max_user_watches` runs out the watcher reports it and the session falls back to interval polling, which still converges but no longer reacts within a fraction of a second. Raise the limit with `sysctl` and persist it.

## See also

- [Safety](./safety.md) — what is guaranteed inside these boundaries
- [Overlapping and nested roots](./nesting.md) — the one-owner rule in practice
- [Commands](./commands.md) — `verify`
