# Retained residuals

The companion to [INVARIANTS.md](./INVARIANTS.md): the risks that are deliberately left open. Each entry records what the residual is, why it was retained, what would change that decision, and what the fix would be if the decision changed — so revisiting any of them starts from the reasoning, not from scratch. None of these are forgotten; all of them are chosen.

## 1. Mounts that were never observed mounted

**The residual.** Scans record mount boundaries, and the session retains them. With `ignore_mounts = false` (the default), a recorded mount that disappears or empties where the ancestor held content halts the session, regardless of its entry count. With `ignore_mounts = true`, recorded mount paths are excluded on both sides, including after an unmount.

A mount that was never observed mounted has no such identity evidence. Below the root its disappearance follows ordinary reconciliation unless `guard_directory_deletes_over` applies. A directory smaller than the configured threshold—including one huge file—does not trigger that additional guard. The whole-root emptying guard has its own threshold of two ancestor entries; a missing alpha root is separately refused.

**Why retained.** An unseen mount disappearing and a deliberate deletion have the same tree shape. A byte threshold would also interrupt routine deletion of large files. Mount tracking closed the known-boundary case; it cannot reconstruct a mount that no scan observed.

**What would change the decision.** Evidence of real losses from mounts appearing and disappearing entirely between scans, or a requirement to declare mount identities before the first scan.

**The fix if changed.** Explicit expected-mount configuration or OS mount-event tracking, with tests for unplugged-at-startup and mounted-between-scans cases. Raising a file-count or byte threshold cannot establish mount identity.

## 2. Pathname TOCTOU outside Linux creations

**The residual.** Creation uses atomic no-replace rename on Linux (`RENAME_NOREPLACE`) and macOS (`RENAME_EXCL`). Everything else keeps a microsecond check/use window: replacements and removals validate against the scan and then rename or unlink by pathname; an editor's save landing in the window is lost; a directory component swapped for a symlink after verification can redirect an operation outside the root. Creations on other platforms and unsupported-flag fallback paths keep the plain-rename window too.

**Why retained.** Full closure requires descriptor-relative traversal — `openat2`/`openat` with `O_NOFOLLOW`, directory file descriptors held across each operation, and rename/unlink relative to those descriptors. That is a rewrite of the transitioner's spine, not a patch, and it deserves its own design and review rather than riding a fix wave. The editor-save window is real but tiny; the symlink redirect requires a cooperating local process, and in the current single-user threat model an actor who can win that race can already write the tree directly.

**What would change the decision.** Running autobahn with more privilege than the tree's writers (a root daemon syncing user-writable trees) — that converts the symlink race from a self-own into privilege escalation, and the refactor stops being optional. That is why root is refused by default: the controller needs `--allow-root` or `experimental.allow_root`, the agent runs as root only for a session that sets `default_owner` or `default_group`, and root under another user's `$HOME` is always refused. Whoever sets one of those has chosen this deployment. Also: the next time the transitioner is opened for substantial work anyway.

**The fix if changed.** The dirfd refactor, plus explicit handling of `ENOSYS`/`EOPNOTSUPP` so unsupported-`renameat2` behavior is a known state rather than an accident. Sequencing note: do it *after* the staging and transition fault harness exists, so the harness can gate it.

## 3. Network filesystems beyond warn-and-document

**The residual.** NFS/SMB/CIFS/FUSE roots warn at startup and the README states the boundary: best-effort, single-writer. Nothing in the code compensates — another client's write can hide behind attribute caching (up to 60 seconds by default on NFS) from both scanning and the checks guarding destructive operations, and no watcher fires for it.

**Why retained.** The honest fixes are protocol-specific and partial: close-to-open consistency helps only if every read path reopens files; lease/delegation awareness is server-dependent; none of it restores inotify. A half-fix would upgrade the warning to an implied promise the code cannot keep. The boundary-plus-warning states the truth.

**What would change the decision.** Multi-client network mounts becoming a supported target rather than a tolerated one — a product decision, not an engineering one. Evidence that single-writer NFS use is common would justify the cheapest hardening step below without the full commitment.

**The fix if changed.** In order of cost: `fstat`-after-`open` revalidation on the destructive paths (close-to-open actually refreshes that on NFS); a mount-aware mode that disables digest reuse entirely (correct, slow, and honest); real integration tests against an actual NFS server before claiming anything.

## 4. Cross-process overlapping configurations

**The residual.** Within one configuration, nested writable endpoints are refused and equal shared endpoints warn. Across processes sharing one user's default state root, only the exactly-identical pair is excluded (the endpoint-pair lock), even with different `--state-root` or `--state-dir` overrides. Different machines, users, or `AUTOBAHN_HOME` directories do not share that lock. Two different-but-overlapping configs in separate processes can still write one tree region from independent ancestors.

**Why retained.** The full fix is endpoint read/write locks acquired on the host that owns each endpoint, including agent-side acquisition for remote roots — a protocol change with its own new failure modes (stale-lock recovery, lock ordering across sessions, deadlock between supervisors). Review round five judged the trigger topology — a *writable* overlap spanning processes — an exotic, deliberate configuration, and the mechanism heavier than the exposure.

**What would change the decision.** The agent protocol being opened for other reasons; or evidence that multi-machine sync into shared storage is a real deployment pattern. The intent records also shrink the harm: overlapping writers produce conflicts more often and silent swaps less often.

**The fix if changed.** Advisory endpoint locks keyed by resolved root identity in the endpoint host's default state root: shared for read-only participation (one-way alphas), exclusive for writable participation. The pair lock stays; this generalizes it.

## 5. Forged timestamps beyond the verify verb

**The residual.** A rewrite with identical length, restored mtime, and a retained inode is invisible to metadata-based change detection — every scan, full scans included, reuses the recorded digest. The racy rule closed the *accidental* same-granule case; deliberate restoration (`touch -r`, reproducible-build tooling, an adversary) remains.

**Why retained.** This is the founding trade of every scan-based synchronizer — rsync's quick check, git's index, mutagen — because the alternative is reading every byte of every file on every scan. `autobahn verify` is the escape hatch: an on-demand full-content re-read that makes the invisible visible and logs each instance as evidence.

**What would change the decision.** Verify-verb logs actually catching divergence in real trees (evidence the case occurs in practice), or a deployment where the adversarial variant is in the threat model.

**The fix if changed.** Scheduled background re-verification — a slow rolling rehash amortized across idle cycles, bounding the maximum age of any trusted digest — rather than per-scan hashing, which would repeal the design's central economy.

## The shape of the whole list

Every entry follows the same pattern: the residual is bounded, the trigger is rare or requires deliberate action, the fix is either disproportionate machinery or a threat-model change, and there is a named, cheaper observable that would reopen the question. That is what "retained" means here — not unexamined, and not unmovable.
