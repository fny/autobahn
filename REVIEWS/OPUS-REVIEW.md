# Autobahn: full-repository code review

**Reviewer:** Claude Opus 5.5 · **Date:** 2026-09-23 · **Revision:** `1180499` (main)

**Scope:** every tracked source file. That is about 45k lines of Rust across `src/`, `tests/` and `examples/`, plus the install and build scripts, the CI and release workflows, the TLA+ spec bridge and the docs that make promises about behaviour. The review covers five areas: code quality, security, reliability, performance and test gaps.

**Method:**
- Seven parallel reviewers each took one subsystem and read it in full, following calls across module boundaries: the write path, the wire and transport, reconciliation and the journal, scan and observer, the supervisor and configuration, the CLI and release, and the test suite.
- I then checked every finding rated High or above against the code myself. Where it was cheap, the finding was also reproduced on a scratch copy of the repo or with the release binary on throwaway trees. The working tree was not modified, apart from this file.
- The other review files in the repo root (ASTRA, DEEPSEEK, GLM) were deliberately not read, so that this review stays independent.

**Confidence labels:**
- **Reproduced:** observed by running code.
- **Confirmed:** the whole code path was traced by hand.
- **Likely:** the mechanism is confirmed, but whether it bites depends on the environment or on timing.

---

## Verdict

This is an unusually careful codebase:
- The invariants are written down, and tests are mutation-checked against them.
- There is crash-point enumeration: the journal is cut at every byte, and the connection is cut at every frame boundary.
- A TLA+ spec is replayed against the real reconciler.
- Comments explain *why*.

Most of what follows falls outside the region that machinery covers. The spec models only files and directories in the three two-way modes. The harnesses do not generate ignored or untracked content, directory swaps, deep trees or hostile headers. That region holds **one reproduced data-loss bug that defeats a headline safety guarantee**, plus a handful of other silent-loss or stale-replica paths.

Separately, **CI on `main` is red right now**, so the spec job and the macOS job have not run on the current head.

### Top findings

| # | Sev | Finding | Confidence |
|---|---|---|---|
| C1 | Critical | An ignored leftover (e.g. `.DS_Store`) in an emptied root defeats the emptied-root halt, and every file on the other side is deleted, in every mode including `two-way-paranoid` | Reproduced |
| H1 | High | `resolve --keep X` on a path that is not in conflict deletes that path on **every** side, including X | Confirmed |
| H2 | High | A directory swapped by rename (`mv live old && mv staging live`) keeps its old content in the incremental scan. The peer shows the old tree, and `staging/` is deleted there, for up to 120 s | Reproduced |
| H3 | High | Regression in today's `17715e9`: `offer_baseline`'s stale-offer refusal can no longer fire, so a shared root can roll its baseline back | Confirmed |
| H4 | High | A directory chain about 1,700 deep overflows the scanner's 2 MiB thread stack. That aborts the process, which kills every session, and a login service then crash-loops | Reproduced |
| H5 | High | Supplying a file buffers its entire delta (the whole file, when there is no base) in memory. A multi-GB file can OOM the supervisor or the agent | Confirmed |
| H6 | High | Size-excluded files (`max_file_size`) are deleted when their parent directory is deleted, and an edit that grows a file past the limit can be destroyed. The docs promise the opposite | Confirmed |
| H7 | High | Peering (experimental): the fence is checked only when a lease is presented, and lease check-then-write is not atomic, so two controllers can write one root | Confirmed |
| H8 | High | CI on `main` is red: 3 clippy errors, and 2 FreeBSD test failures | Confirmed (run 35920255058) |

---

## 1. Correctness and reliability

### C1. Critical: the emptied-root halt is defeated by ignored or untracked leftovers
**Where:** `src/session/mod.rs:1081-1093` (`one_side_emptied_root`) and `src/tree/reconcile.rs` (the paranoid rule skips `path.is_empty()`).

```rust
let gone = |side: Option<&Node>| match side {
    None => true,
    Some(node) => node.children().is_empty(),
};
```

The scanner records ignored entries, FIFOs, sockets and oversized files as `Content::Untracked` children (`src/scan/mod.rs:874-876`). `.DS_Store` is in the default ignore list (`config.rs:89`). A root that has lost every synchronizable entry but still holds one ignored entry is therefore **not** "gone":
- the halt does not fire;
- reconciliation recurses into the root and sees every ancestor child as deleted on that side;
- it emits a deletion for every file on the other side.

**Reproduced:** I added a probe test to a scratch copy. It used an ancestor with 20 files, alpha with only an `Untracked` `.DS_Store`, and beta unchanged, in mode `TwoWayParanoid`. The result was `halted=false`, and `beta_transitions` held a deletion for all 20 files.

**Realistic triggers:**
- An unmounted volume whose bare mountpoint has a `.DS_Store` or `.git`.
- A wipe that leaves `.git` or `node_modules`.
- A restore tool that recreates only dotfiles.

This is the exact case invariant I7 exists for ("an unmounted disk is far more likely than a deliberate wipe").

**Fix:** decide emptiness by *synchronizable* children, e.g. `node.children().iter().all(|c| !c.content.synchronizable())`, in both `one_side_emptied_root` and the paranoid `empty` closure. Add this shape to `emptied_root_detection`, and teach the reconcile proptest generator to place `Untracked` nodes at every depth, including the root.

### H1. High: `resolve` on a path that is not in conflict deletes it everywhere
**Where:** `src/main.rs:2292-2300` ("the paths are taken at their word"), and the retirement loop at `2410-2530`.

When no recorded conflict matches, `resolve` still retires the "losing" side's copy by removing it through a transition. It never touches the ancestor. So on the next cycle:
- the losing side is "deleted since the ancestor";
- the winning side is "unchanged";
- ordinary three-way reconciliation propagates the **deletion** to the winner.

The file the user asked to keep is gone from every side. `--keep both` on an in-sync path renames it everywhere instead.

**Triggers:**
- Running the same `resolve … --yes` twice.
- A stale tray menu click after the cycle already settled the conflict (tray actions are queued).
- `resolve group some/in-sync/file`.
- `resolve group ./`, which normalises to `""`. `node_at` then returns the root, so the whole synchronizable tree is retired.

**Fix:**
- Refuse paths that are not in the recorded conflicts, unless the winner actually differs from the ancestor.
- Reject an empty path.
- Add a supervisor test: resolve an in-sync file twice and assert that it survives.

### H2. High: directory→directory replacement is invisible to incremental scans
**Where:** `DirtyPaths::mark` (`src/scan/mod.rs:140-151`) sets `relist` only on the *parent*. `scan_directory` (`:554-589`) then walks the **baseline's** child list for the marked directory and adopts the unmarked children without calling `stat`.

**Reproduced:** a tree was synced A→B, then `mv A/live A/old && mv A/staging A/live` ran on A. B showed `live/f1=old1` and `old/f1=old1`, and `staging/` was **deleted** on B. The new content existed only on A for about 120 s, until the periodic full scan.

**Triggers:**
- Atomic deploy swaps.
- `rmdir x; mkdir x; populate`.
- Renaming over an empty directory.

This breaks I1: the observer *was told* about the change and still served a snapshot that did not reflect it. It also breaks the documented contract that "incremental equals full". Lease validation prevents a destructive overwrite, but for two minutes the replica holds the wrong tree and has lost the only second copy of `staging/`.

**Fix:** set `relist = true` on the marked node itself (one extra `readdir` per marked directory), or have directory Create and Rename events request a relist. Add a dir-swap case to `incremental_scans_agree_with_full_scans`.

### H3. High: regression, the stale baseline offer is no longer refused
**Where:** `src/endpoint/local.rs:1505-1507` and `1543`, introduced in `17715e9` (today).

```rust
self.observer.invalidate(...);
self.seen_generation = self.observer.generation();   // current, not the lease's
...
self.observer.offer_baseline(folded.clone(), self.seen_generation);
```

`offer_baseline` refuses only when `based_on < state.baseline_generation` (`observer.rs:520`). `based_on` is now always the newest generation, so the refusal described in the I1 bullet "`offer_baseline` refuses an offer based on an older generation" can never fire.

**Scenario:** two sessions share a root (fan-out, star). A leases at g5. An external edit to `x` moves the generation to g6, and B's scan consumes `x`'s mark. A transitions `y` and offers `fold(S5 + y)`. The offer is accepted, and the baseline rolls back to the old `x`. The next incremental scan has only `y` marked, so it serves the stale `x` as current for up to 120 s.

The same line also swallows wake-ups: an external event that lands between the lease scan and the post-write announcement is folded into `seen_generation`, so `watch_poll` doesn't wake for it.

The existing tests call `offer_baseline` directly with hand-picked generations, so they cannot see this.

**Fix:** capture the lease generation before the first `invalidate` and offer with that. Advance `seen_generation` only by the bumps the endpoint caused itself. Add a two-endpoint test that goes through `LocalEndpoint::transition`.

### H4. High: deep trees overflow the stack and abort everything
**Where:** `src/scan/mod.rs:531` onwards: `scan_directory` → `walk` → `scan_entry` recursion on default 2 MiB threads. No code in the repo sets `stack_size`.

**Reproduced:**
- `autobahn sync` on a `d/d/d/…` chain succeeded at depth 1,400.
- It aborted at depths 1,700 and 1,850 with `fatal runtime error: stack overflow` (exit 134).
- The path at that depth is about 3.5 KB, which is below `PATH_MAX`, so the ENAMETOOLONG guard never triggers.

A stack overflow is an abort, not a panic, so every session in the supervisor or the agent dies. The tree is still there after a restart, so a login service crash-loops.

`recount`, `tree::apply`, `diff`, `validate`, reconcile and serde are all recursive in the same way. Worse, a hostile or corrupt agent can send a deeply nested `Node` (conceded as I9-B) and crash the controller's router thread.

**Fix:** either spawn scan, session and router threads with an explicit large stack (64–256 MiB of virtual space is free until it is touched), or cap depth and mark deeper directories `Problematic`. A depth-limited `Deserialize` for `Content::Directory` closes I9-B cheaply, since legitimate depth is at most about 2,048.

### H5. High: supply holds a whole file in memory
**Where:** `src/endpoint/local.rs:787-872` (`buffer_delta` → `supply_from`).

The doc comment says the buffer holds "the size of that file's *delta*, not the file". When there is no base (a new file, or a full rewrite), the delta *is* the file, and `supply_from` reads all of it into `pending` before a single frame is returned. The `deltify` branch does the same thing through its callback.

`max_file_size` defaults to unlimited. Syncing a 20 GB VM image or database dump allocates about 20 GB on the source. On the controller the source is the supervisor process, so every session dies with it.

**Fix:** keep an open `File` in `SupplyState` and read lazily in `supply_pull`. Run `deltify` on a helper thread feeding a bounded `sync_channel`. The alternate-path fallback still works if it is tried only before a file's first frame goes out.

### H6. High: excluded content is destroyed by directory deletion
**Where:**
- `src/endpoint/local.rs:~2495-2531`: the "excluded content goes with the directory" branch.
- `src/tree/reconcile.rs:53-85`: `blocking`, whose comment claims a deletion "leaves [excluded entries] exactly where [they are]".
- `docs/configuration.md:73`: oversized files "stay on disk … never mistaken for deletions".

`Untracked` covers more than pattern ignores. It also covers files over `max_file_size`, sockets, FIFOs and devices, and symlinks under `SymlinkMode::Ignore`. There are two shapes:
1. **Beta-only big file.** Beta has `data/dump.sql` (2 GB, over the limit, beta-only) and alpha runs `rm -rf data`. Beta's transition removes `dump.sql` through the excluded branch. That file existed nowhere else.
2. **Edit past the limit.** In `two-way-conflict`, alpha grows `d/a` past the limit (an *edit*) while beta deletes `d`. `alpha_diff` sees `a` as removed, the "both purely deletions" branch fires, and alpha's edited file is deleted.

The deliberate design, documented in `docs/ignores.md:63`, is that *pattern-ignored* content goes with its directory. Size and special-file exclusions were swept into the same rule by accident, and the comment on `blocking` contradicts the transitioner.

**Fix:**
- Carry a reason on `Untracked`: `Ignored`, `TooLarge` or `Special`.
- Let only `Ignored` be removed by a directory deletion. Everything else refuses and surfaces a conflict.
- When diffing, treat an entry that is `Untracked` on one side but present in the ancestor as "not deleted".
- Fix the `blocking` comment.

### H7. High (experimental feature): the peering fence is not a fence
**Where:** `src/transport/mod.rs:548-590` and `src/peering.rs:227-277`.
- `fence` is set only when a `Request::Lease` is refused. Once a channel's lease is accepted, every later `Transition` or `StagePush` on that channel is allowed without reading `lease.json` again.
- Nothing renews the lease during a cycle. A leader in a cycle longer than `ttl + failover_after` (150 s) keeps writing to a host that another beta has taken over.
- `write_lease` is read-check-write with no lock. Two candidates presenting the same term to one host can both be `Accepted`.
- The temp name is `.<name>.<pid>.tmp`. Two channels in one agent process share it.
- There is no fsync, so a power loss can roll the term back.

`docs/peering.md` says "No root is ever written by two controllers, whatever the network does". The feature is behind `peering-experimental`, which is the only reason this is not Critical.

**Fix:**
- Revalidate the lease on every write request.
- `flock` around check-and-write, and use unique temp names with fsync.
- Renew on a timer rather than per cycle.
- Have the agent refuse writes once `renewed_at + ttl` has passed on its own clock.

**Related peering bugs (all Confirmed, Medium):**
- **Handoff never completes with plain groups or a paused session.** `handed >= sessions` counts every plan (`supervisor/mod.rs:493-497` and `304-318`), but plain and paused plans never call `handed_one`.
- **Plain groups stop while the alpha follows a beta.** `Role::Follower` runs only `attach_as_agent` (`peer.rs:291-320`).
- **`peering yield --to <beta>` does not hand off to that beta.** `to` is never validated, and `follow()` never checks `lease.leader == star.name`. A typo in `--to` means nobody leads for 150 s.
- **Backoff can outlast the takeover wait.** Backoff reaches 300 s plus up to 24% jitter, while renewal is per cycle. After a blip of about a minute, a beta takes over from a healthy alpha.
- **Some leases ignore the configured TTL.** `for_alpha` and both yield paths use `DEFAULT_PEERING_TTL`.

### H8. High: CI is red on `main`
Run 35920255058 on `1180499`:
- **Linux:** clippy fails with `large_enum_variant` (`local.rs:1588`, `enum Receiving`) and two `type_complexity` errors (`remote.rs:773`, `:786`), all from code added today. `spec` and `mac` both depend on `linux`, so **the TLC replay and the macOS suite have not run on the current head**.
- **FreeBSD:** `update::tests::this_machine_has_a_published_build` and `the_update_naming_matches_the_agent_bundle_naming` fail with "no release build for freebsd-x86_64". CI tests a platform the release does not ship, and the tests assume that every platform the suite runs on has a release build.
- The README and `support-boundaries.md` advertise FreeBSD, but `install.sh`, `build-agents.sh` and `release.yml` don't build it. A FreeBSD host can't be bootstrapped.

Locally, `cargo test --release` passes (478 tests, 0 failures, 4m05s). `cargo fmt --check` is clean.

### Medium: reliability and correctness

| # | Finding | Where | Conf. |
|---|---|---|---|
| M1 | **A corrupted length field in a middle journal record is read as a torn tail.** Every later acknowledged record is dropped, and normalization makes that permanent, which rolls the ancestor back silently. The digest covers the generation and payload but not the length. Fix: checksum the header, and treat the file as torn only when the header is valid. | `session/ancestor.rs:727-739` | Confirmed |
| M2 | **One-way modes never converge when the beta copy holds ignored content.** The deletion's `old` is `beta.cloned()`, not `beta_sync`. Removal refuses, the next cycle re-proposes it, and this repeats forever. `unsynchronizable_content_never_travels` checks only `new`. | `tree/reconcile.rs:566-570`, `656-660` | Likely |
| M3 | **Leftover `.autobahn-tmp-apply-*` files are never cleaned up and wedge directory deletion.** The scan hides them. `remove_directory` treats one as "unexpected content", which is a disagreement, so the baseline is distrusted and a full walk runs every cycle, indefinitely. They also leak GBs after a crash mid-copy. | `local.rs:2233`, `2498-2509`; `scan/mod.rs:612` | Confirmed |
| M4 | **A worker panic kills a session silently, and logging can panic.** `println!`/`eprintln!` panic on EPIPE (watch piped to a process that exits) or ENOSPC (a full disk under `service.log`). There is no `catch_unwind`, so the status file keeps saying "synchronized" and no alert fires. | `supervisor/mod.rs:706-760`; `logging.rs:115-140` | Confirmed |
| M5 | **A panic in an agent channel thread leaves the controller waiting forever.** The request was forwarded, no response is ever sent, and no timeout exists. | `transport/mod.rs:446-461`, `540-748` | Confirmed |
| M6 | **No timeouts during connection setup, while the per-host pool lock is held.** There is no `ConnectTimeout`. The handshake read, platform probe, upload and channel-open have no deadlines. One hung login (an NFS rc file, a conda init) blocks every session to that host. | `transport/mux.rs:126-132`, `253`, `605-631`; `install.rs` | Confirmed |
| M7 | **The polling fallback serves a cached snapshot for up to 120 s, not the 5 s interval.** With no watcher, the generation moves only on announced writes, so external edits are invisible until the full scan. This is the exact fallback used when `max_user_watches` is exhausted. | `endpoint/observer.rs:346-351` | Confirmed |
| M8 | **Directories re-included by a negation are never watched.** `watch_tree` skips ignored directories without checking `holds_a_re_inclusion`, so edits under `vendor/keep.txt` wait about 120 s. | `local.rs:233-238` | Reproduced |
| M9 | **A replaced root keeps a dead watcher until restart.** After `mv A A.old && cp -a A.old A`, every later edit waits for the full scan, and events from `A.old` mark unrelated paths. Fix: record the root's `(dev, ino)`; rebuild on `MoveSelf`/`DeleteSelf` or on a mismatch. | observer and watcher | Reproduced |
| M10 | **Runtime watch-extension errors are discarded.** `let _ = watch_tree(...)`, which contradicts the startup contract at `:205-209`. Once past `max_user_watches`, new directories go unwatched with no log line. | `local.rs:406` | Confirmed |
| M11 | **A staging base that changes mid-stream fails the whole cycle.** If the destination base is truncated between `stage_begin` and the push, `patch` hits EOF and the entire staging stream errors. A *source*-side change is handled quietly, and this should be too: discard that file and retransfer it. | `local.rs:922-973`; `rsync/mod.rs:379-398` | Confirmed |
| M12 | **Alert hooks can hang forever, and their stderr is lost.** `stdin.write_all` runs before the timeout loop, so a hook that does not read stdin, given a large report, blocks indefinitely and later alerts are skipped. stderr is piped and never read, although `docs/alerts.md` says it goes to the log. | `alerts.rs:488-523` | Confirmed |
| M13 | **`sync` exits 0 with conflicts or blocked paths**, although the docs sell it for scripts that need a status code. | `main.rs:952-986`, `1165-1190` | Confirmed |
| M14 | **Client calls to the control socket have no timeout.** A wedged supervisor freezes `status`, the shop, and the whole tray, whose `refresh()` runs on the event loop. | `supervisor/control.rs:369-383`; `tray.rs:325-362` | Likely |
| M15 | **`start`/`restart` validate the default config, not the `--config` baked into the unit.** `install` also writes relative `--config`/`--state-root` paths into units whose working directory differs. | `main.rs:595`, `606`; `service.rs:100-105` | Confirmed |
| M16 | **The systemd unit is unquoted.** `ExecStart` and `Environment` are joined with spaces, so a path containing a space, `%`, `$` or `"` breaks the unit. | `service.rs:427-437` | Confirmed |
| M17 | **The install script assumes a POSIX login shell.** It uses `tmp=…`, `$$` and `{ …; }`, which fail under fish and tcsh, so those hosts can never be bootstrapped. The fake-ssh test runs `/bin/sh -c`, so it can't see this. Wrap the script in `sh -c '…'`. | `transport/install.rs:213-221` | Likely |
| M18 | **A failed scan loses the dirty marks it consumed.** `result?` returns after `take_dirty` without resetting `last_full_scan`. The per-caller `max_entry_count` bail also returns before the baseline is updated. | `observer.rs:392`, `401-409` | Confirmed |

### Low: reliability and correctness (abridged)
- **Keep-both rename can overwrite.** `rename()` (keep-both) checks existence and then calls plain `fs::rename`. Use the existing `publish_rename(…, false)` (`local.rs:1353-1371`).
- **Parallel publish race on a shared digest.** One thread decrements the use count and another moves the staged blob before the first opens it. The result is a spurious "staged content unavailable" and an extra cycle (`local.rs:2181-2240`). Open the file before decrementing.
- **Journal directory entry not synced.** A journal created by `checkpoint()` never has its directory entry synced, and a later durable `intend()` skips the directory sync (`ancestor.rs:507-514`, `398-401`).
- **Intents can be lost at open.** A format-upgrade rewrite at open truncates the journal while intents are unresolved, and `stored_generation()` discards the unresolved list (`ancestor.rs:238-268`).
- **Compaction failure fails a good cycle.** A compaction that fails after a durable append fails the cycle that succeeded. It loops on FUSE or network homes where directory fsync always fails (`ancestor.rs:376-381`).
- **Silent truncation of results.** `achieved_changes` `zip`s results and transitions, so a count mismatch truncates silently and leaves a stale ancestor (`endpoint/mod.rs:119-129`).
- **Scan-delta desync on reassembly error.** The agent anchors `last_sent` on the header while the controller keeps its old snapshot. It is safe only because every caller drops the session on error. Set `last_snapshot = None` on any reassembly failure (`remote.rs:141-187`).
- **Stderr relay stops early.** The relay stops at the first non-UTF-8 line, and later agent diagnostics are lost (`transport/mod.rs:131-132`).
- **Every install failure is `Unreachable`.** This includes permanent ones ("no agent binary for freebsd", remote disk full), which then retry forever (`remote.rs:413-418`).
- **Upload errors hide the cause.** `upload_agent` reports "Broken pipe" instead of the remote's stderr. Its stdout is piped and never read (`install.rs:231-234`).
- **Rollback mixes versions.** Update rollback restores the old binary but keeps the new agent bundle (`update.rs:137-146`, `395-405`).
- **Reload reads the file twice.** It validates a second read of the file, not the bytes it compared, so a half-written read marks good bytes as applied (`reload.rs:183-209`).
- **Every reload stops everything.** Any reload stops every session, SSH connection and control socket. Diff the plans by identifier instead (`supervisor/mod.rs:584-593`).
- **Pool slots are never evicted.** A host removed from the config keeps its ssh process and agent until exit (`mux.rs:534-632`).
- **Racy-mtime protection is lost.** An incremental scan stamps `scanned_at = now` on adopted subtrees, so a later full scan trusts digests recorded while their mtime was racy (`scan/mod.rs:253`, `295`).
- **`scanning` flag stuck after a panic.** The observer's `scanning` flag is not reset on a walk panic or `EAGAIN` from `spawn`, so every caller loops on the 60 s wait (`observer.rs:367`, `390`).
- **Wildcard negations are dead.** A wildcard negation under an ignored directory (`vendor` + `!vendor/*.patch`) has no effect, and it is no longer reported (`ignore.rs:103`). Inside a region, *any* negation (e.g. `!*.md`) re-includes.
- **`select` keeps one relative path.** With nested groups, the last match overwrites the relative path (`main.rs:1433-1435`).
- **Progress keyed by host.** Progress is keyed by `(group, host)`, so two betas on one host show each other's progress (`main.rs:3249`).
- **`scripts/mi` is broken.** It calls a nonexistent `up` subcommand and writes to the real `~/.autobahn`.
- **Docs disagree with the code on `one-way-conflict`.** The "Alpha deletes a file beta edited" row in `docs/modes.md:37` does not match what the code does.

---

## 2. Security

Threat model as documented: the agent is trusted at the level SSH authenticates it. The findings below are either outside that boundary (local users, supply chain) or places where the code falls short of what I9 itself promises.

### Medium
| # | Finding | Where | Conf. |
|---|---|---|---|
| S1 | **Scan-delta header used before validation, which breaks I9.** `Vec::with_capacity(header.length)` is agent-chosen, so a huge value aborts the allocator. `header.block_size` goes unchecked into `rsync::signature` (`vec![0; block_size]`, or one hash per byte). The expansion check runs once per batch, so one 64 MiB frame of `Blocks{0,N}` ops can expand millions of times before it is caught. Any corrupt agent can abort the supervisor. Fix: derive the block size locally, cap `length`, and bound each op before `patch`. | `endpoint/remote.rs:212-235` | Confirmed |
| S2 | **A destination can make a local source read any file.** `supply_from` calls `self.root.join(path)` and `File::open` with no validation, so absolute paths, `..` and intermediate symlinks all work. Needs come from the remote's `stage_begin_finish()` and are never checked against the requests that were sent. A compromised dev box can pull `~/.ssh/id_ed25519` off the laptop. Fix: `resolve_relative`, and require that `last_snapshot` records that `(path, digest)`. | `local.rs:840`; `session/mod.rs:980-1010` | Confirmed |
| S3 | **Staging is world-readable and follows planted symlinks.** The staging dirs are `create_dir_all` (0755) and blobs are `File::create` (0644), although the code sets 0600/0700 defaults because "trees frequently hold credentials". Temp names are predictable, and `BesideRoot` staging lives in the root's *parent*. There is no `O_EXCL`/`O_NOFOLLOW`, and `set_permissions(&staged)` runs before the regular-file check. Anyone who can write to that parent can plant a symlink and get files truncated or chmodded. | `local.rs:956`, `1108`, `2221-2233`, `2972` | Confirmed |
| S4 | **The attach socket has no peer-credential check.** Any process that can connect and write `alpha\n` becomes the leader's alpha endpoint and can serve a fabricated tree. The control socket does check (`SO_PEERCRED`). The attach socket is not chmodded and relies on umask. | `supervisor/peer.rs:127-133`, `206-227` | Confirmed |
| S5 | **SSH inherits the user's `ssh_config` for connections that last for days.** Only `BatchMode`, `ServerAlive*` and `Compression` are set. `ForwardAgent yes` exposes the controller's ssh-agent to the remote for the supervisor's lifetime. `RequestTTY force` corrupts the binary stream, and `LocalForward` plus `ExitOnForwardFailure` breaks reconnects. Add `-T -o ForwardAgent=no -o ForwardX11=no -o ClearAllForwardings=yes -o PermitLocalCommand=no -o ConnectTimeout=…`. | `transport/mod.rs:73-84` | Confirmed |
| S6 | **Supply chain: checksums only, no signatures.** `SHA256SUMS` is fetched from the same release as the binaries, so it proves integrity but not authenticity. `autobahn update` then pushes the downloaded agent bundle to every SSH host, so one malicious release reaches the whole fleet. `install.sh` is weaker still: it does **not** verify `autobahn-agents.tar.gz`, although `docs/releases.md` says "every download", and it installs *unverified* when `SHA256SUMS` is missing, where `update` fails closed. Fix: sign `SHA256SUMS` (minisign or cosign) with a key pinned in the binary, or publish build attestations. Verify the tarball, and fail closed. | `update.rs:111-128`, `504-515`; `scripts/install.sh:128-184` | Confirmed |
| S7 | **Release workflow hardening.** `permissions: contents: write` is set at workflow level, so the `mac` job, which holds the Developer ID `.p12` and notary key, gets a write token and runs `dtolnay/rust-toolchain@stable` (a mutable ref). No action is SHA-pinned. `ci.yml` has no `permissions:` block. TLC is fetched from `releases/latest` with no checksum. Fix: `permissions: {}` at the top, write access only on the release job, and SHA-pin the actions. | `.github/workflows/*.yml` | Confirmed |
| S8 | **Predictable scratch directories in the shared `/tmp`.** `autobahn diff` uses `temp_dir()/autobahn-diff-<pid>` via `create_dir_all` and then `fs::write`, which follows symlinks. The tray's diff file is named from the synced path. On multi-user Linux another user can pre-create either and redirect the writes. Use `tempfile` (already in the lock) or an exclusive 0700 `DirBuilder`. | `main.rs:2064-2090`; `tray.rs:729-732`; `update.rs:721-729` | Confirmed |
| S9 | **Shell and AppleScript strings are built from untrusted paths.** The `issues` fix suggestions (`ssh {dest} 'sudo chown -R {user} {root}/{where_}'`) are copied to the clipboard by the shop and run under `sudo`, and a path containing `$(…)` or `;` executes. `user` is actually the hostname when there is no `user@`. The `on-alert.sh` example that `init` writes puts `$AUTOBAHN_SUMMARY`, which includes remote error text, inside AppleScript source (`config.rs:162`). Pass it as `argv` instead. | `main.rs:1600-1616`; `config.rs:162` | Confirmed |
| S10 | **Paths reach `resolve` as flags.** The tray and shop run `resolve <group> <path> --keep …` without `--`, so a top-level file named `--all` settles every conflict in the group. | `tray.rs:717`, `726`; `shop.rs:437` | Confirmed |
| S11 | **Terminal escapes pass through.** File names, conflict paths and remote error strings are printed raw in `status`, `issues`, the shop and the pager. A filename with OSC 52 can write the user's clipboard. | `main.rs:3370-3403`, `1812-1894` | Confirmed |

### Low / Info
- **`prune_agents` builds an unquoted remote `rm`** from names the remote side listed, although the comment directly above it forbids patterns (`install.rs:313-318`). Use an allowlist or `"$@"`.
- **Fallback control socket directory in `/tmp`.** `/tmp/autobahn-<uid>` (used when the state-root path is over 100 bytes) accepts a pre-created directory, and the `chmod` result is discarded. The client never checks the server's uid (`control.rs:238-266`).
- **State root permissions are incidental.** The state root is 0700 only as a side effect of `control::bind`. A manual `sync`, or a state root using the fallback socket, leaves `ancestor` files at 0644 with every path and digest in the tree.
- **Pushed peering files are trusted like the host's own configuration.** `agent_command` survives `derive_star`, and session identifiers are only `trim()`ed before being joined into paths (`peering.rs:540-548`, `transport/mod.rs:885`). This matters for users who lock a key to `command="autobahn agent"`.
- **The scanner walks by path.** A directory swapped for a symlink during its subtree walk exports content from outside the root, and a file swapped for a FIFO blocks `open()` indefinitely with `scanning = true`. RETAINED §2 covers only writes. A dirfd walk (`openat` + `O_NOFOLLOW|O_NONBLOCK`) fixes both and removes O(depth) path resolution per entry.
- **Digest reuse ignores ctime** (`scan/mod.rs:1119-1123`). `touch -r` and `cp -p` over an existing inode are invisible, and ctime would catch them. RETAINED §5 cites git as precedent, but git records ctime.
- **Dead `Response::Scan` variant.** It is accepted by the controller without the `validate()` that `reassemble` applies (`remote.rs:136-140`).
- **`bincode 1.3` is unmaintained** (RUSTSEC-2025-0141) and is the wire format. The epoch mechanism makes a migration feasible. The tray feature pulls in unmaintained GTK3 bindings and an unsound `glib 0.18`.

---

## 3. Performance

| # | Finding | Where | Impact |
|---|---|---|---|
| P1 | **Reconcile walks the whole tree on every non-idle cycle and allocates a path `String` at every node.** There is no storage-sharing shortcut, although reconcile is a pure function of `(ancestor, alpha, beta, mode)`. Memoize on pointer identity with the last settled inputs, and build paths lazily. | `tree/reconcile.rs:291` | O(tree) per edit; the main scaling cost after the scan |
| P2 | **The transition fold does not save the rehash that `how-it-works.md` claims.** `fold_transition` keeps the lease's `scanned_at`, and published files have fresh mtimes, so the racy rule re-reads every file just written. On a cold sync larger than RAM that is a second full read. Correct the doc, or record per-node "trusted since" times. | `endpoint/mod.rs` fold; `scan/mod.rs` `reusable_digest` | 2× read on cold sync |
| P3 | **`digest_paths` walks the whole snapshot on every supply failure.** A burst of 10k failures on a 500k-node tree is about 5×10⁹ allocations. Build a `HashMap<Digest, Vec<path>>` lazily, once. | `local.rs:800-806`, `877-902` | Quadratic in failure bursts |
| P4 | **`apply` removes changes with `Vec::remove` one at a time.** Emptying a flat 100k-entry directory is O(k·n) in the ancestor apply, both folds and journal replay. Do one merge pass per parent. | `tree/apply.rs:61` | Pathological on flat directories |
| P5 | **Staging on another filesystem (the default placement) reads every last-use file twice.** It rehashes, fails the rename with EXDEV, then copies while verifying. Compare `st_dev` once and skip the move attempt. | `local.rs:2221-2240` | 2 reads + 1 write per file |
| P6 | **The change record keeps duplicates and, on macOS, events from ignored subtrees.** `PendingChanges::record` appends every event path. A build in ignored `target/` or `node_modules` fills the 8,192 cap and forces a full scan each cycle. The transition's own temp files also consume that budget. Filter through the ignore set, dedupe, and drop `autobahn_temporary` names. | `local.rs:302-331` | Needless full walks |
| P7 | **Encode and LZ4 run while holding the shared writer mutex.** One 8 MiB `StagePush` blocks every other session to that host. `std::sync::Mutex` is unfair. Encode outside the lock and hold it only for `write_all`. | `transport/mux.rs:390-396`; `transport/mod.rs:849-857` | Head-of-line blocking across sessions |
| P8 | **The observer builds the watcher while holding its state lock.** Every session on that root stalls for the whole initial walk, about 20 s on large trees, and again every 30 s on retry. | `observer.rs:263-277` | Stalls at startup and after watch loss |
| P9 | **`propagate_executability` rebuilds the whole tree every cycle on exFAT/FAT**, where every file reports as executable. This also defeats `nodes_share_storage` downstream. | `tree/executability.rs:124-135` | Loses the "nothing changed is free" property on those volumes |
| P10 | **Scratch buffer retention misses its own case.** It keeps about 17 MB per thread, which is about 500 MB RSS at 30 sessions. For 16 MiB chunks the compressed maximum is *over* the retention limit, so the buffer is freed and re-zeroed on every chunk. | `transport/mod.rs:1043-1137` | Memory and CPU |
| P11 | **Idle waits poll instead of blocking.** 25 ms slices in backoff and pause, a 50 ms accept poll on control and 250 ms on attach. That is over 1,000 wakeups a second with 30 failing or paused sessions. Use a `Condvar` per worker. | `supervisor/*` | Idle CPU at scale |

Info:
- **Release profile.** Add `strip = true` and `codegen-units = 1`. The agent binary uploaded to every host is unstripped.
- **Double sort.** `read_directory` sorts twice.
- **Per-entry costs.** Every entry costs `lstat` on a full path plus four allocations. A dirfd walk removes both.
- **Serial hashing.** BLAKE3 hashing is single-threaded per file (`update_mmap_rayon` would help on VM images).
- **Shop log read.** The shop reads the whole service log every 1.2 s to show three lines.

---

## 4. Code quality

**Structure.** The largest files are `local.rs` (5,125 lines), `main.rs` (4,023), `config.rs` (2,926), `supervisor/mod.rs` (2,776) and `session/mod.rs` (2,239). The long functions that make auditing hard:
- `Config::plans()` is about 540 lines, and the `group.x.or(defaults.x)` pattern repeats about 12 times.
- `run_resolve` is about 425 lines, `run_issues` about 265 and `run_clean` about 220.
- `serve_channel` is about 250 lines and mixes the fence, anchoring, staging and transitions. H7 and the anchor desync are both easier to miss because of this.
- `run_cycle` is about 290 lines, with near-duplicate alpha and beta stage/intend/transition blocks.
- `publish_file` is about 160 lines and has a side effect inside an `&&` chain.
- `scan_directory` is about 280 lines, with duplicated `Pending::Walk` handling.

Suggested splits:
- `src/cli/` with one file per verb, plus `cli/format.rs`. `shop.rs` currently calls private items in `main.rs`.
- `supervisor/{worker,status,peering}.rs`.
- A `resolve_settings()` in `config.rs`.
- One `apply_side()` in `session`.
- A `ChannelState` with a method per request in `transport`.

**Reconciliation policy is implicit.** `handle_disagreement_bidirectional` repeats "blocking → conflict, else transition" eight times. Which `old` it passes (ancestor, `side_sync` or the raw side) and whether `blocking` sees `alpha` or `alpha_sync` varies with no stated reason, and that variation is the root of M2 and part of H6. One helper with an explicit policy argument would make those choices reviewable. The `writable` check in `config.rs` uses `matches!`, so a new two-way mode would silently be treated as read-only in the nesting check. Use an exhaustive `match`.

**Misplaced doc comments are systemic.** At least 15 places have a doc comment attached to the wrong item, or two docs fused together, apparently where functions were inserted between a doc and its item. Examples:
- `local.rs:177-180` (a sentence cut off mid-way), `726-736`, `2681-2686`, `509-510`
- `observer.rs:240-246`, `301-305`
- `scan/mod.rs:813-820`
- `main.rs:1004`, `1154`, `2116`, `2614-2633` (`run_clean`'s doc on `run_init`)
- `tray.rs:748-762`
- `session/mod.rs:96`, `872`
- `transport/mod.rs:86-88`, `984-985`
- `progress.rs`

In a codebase whose comments carry the design argument, these actively mislead. A `clippy::empty_line_after_doc_comments` / `missing_docs` pass, or a quick review with `cargo doc` open, would catch most of them.

**Comments and docs that contradict the code:**
- The `blocking` comment in `reconcile.rs` (see H6).
- The `sanitize` comment ("reconciliation never puts unsynchronizable content into an expectation"), which is false for the one-way modes.
- `how-it-works.md` Decision 4 (see P2).
- `docs/modes.md:37` (one-way-conflict).
- `ssh_argv` ("autobahn never copies or bootstraps it").
- `hold_paused` ("a paused session holds no resources", but pooled SSH is kept).
- `INVARIANTS.md` I9 cites `oversized_frames_are_rejected_on_send`, which no longer exists (removed in `84b5fc7`). The I9 text about outgoing and oversized messages is stale now that messages reassemble up to 4 GiB. That 4 GiB total is allocated per message, so "refused before allocation" holds per frame, not per message.
- `main.rs:2797` tells users to restart after editing the config, although reload is live.

**Duplication:**
- `thousands`, `terminal_size`, key parsing and width/truncate helpers exist in both `pager.rs` and `shop.rs`, and `format_age` is copied into `tray.rs`.
- `format_size` (KiB) and `format_bytes` (kB) disagree on units.
- `entries_below` exists twice.
- `canonical_root` and `resolve_for_identity` both canonicalize paths, with different rules.
- Test helpers (`write`/`read`/`file`/`digest_of`) are redefined in 5 to 7 test modules.
- `unsafe { libc::isatty }` appears 5 times where `std::io::IsTerminal` would do.
- Two TOML stacks: config is parsed with `toml 0.8` (`toml_edit 0.20`) and edited with `toml_edit 0.25`.

**Repository hygiene:**
- A stale, unstripped 4 MB `dist/agents/autobahn-linux-x86_64` is committed, and `dist/` is not ignored.
- `bench/` holds 626 tracked files (about 245 MB on disk, most of it results and driver logs), although `.gitignore` excludes `bench/results-bench-*/`.
- `README.md:136` onwards still contains "The previous README follows, kept for merging".
- Three TODO files sit at the root.
- `apps/macos/Info.plist` hard-codes `0.4.0`, which the release guard doesn't check.
- There is no `rust-version` (MSRV) and no `--locked` in CI or release builds.
- `NO_COLOR` is not honoured, and ANSI codes are printed to non-terminals.

**Dead code:** `let _ = shown;`, `let _ = inner;`, `let _ = intent_recorded;`, `let _ = root;`, the `Response::Scan` path, and an immediately-invoked closure in `attempt_once`.

---

## 5. Test gaps

### Suite health
- **Local results:** 478 passed, 0 failed, 0 ignored.
- **Two tests that pass without running.** The TLC replay tests return early without `AUTOBAHN_TLC=1` and still report `ok`. So does the non-UTF-8 test on APFS. Use `#[ignore]` or a visible skip so that CI cannot quietly lose them.
- **Unit tests write to the real `~/.autobahn`.** `tests/common::isolate_home` protects only the integration suites. The `transport/mux.rs` tests call `serve_agent` → `create_endpoint` with the real `$HOME`, and they rewrote `~/.autobahn/staging/mux-test-17-beta.scancache` during this review. They also `remove_dir_all` under it, on a machine that runs a live agent and peering lease from the same directory.
- **Shared session ids.** All 8 mux tests use one session id (`mux-test-{root.len()}` comes out as `mux-test-17` for every tempdir), so parallel tests share staging and a scan cache.
- **Global environment changes:**
  - `install.rs:399` sets `HOME`.
  - `supervisor.rs:1177` sets `AUTOBAHN_SSH` and `AUTOBAHN_AGENTS_DIR` and never unsets them.
  - `ATTACH_COMMAND_VARIABLE` is removed only on success, so a panic leaks it.
  - These become `unsafe` under edition 2024.
- **Negative checks after fixed sleeps** (500 ms, 3 s, 300 ms) can pass even when the behaviour is broken. Wall-clock upper bounds (a scan in under 1 s) are risky on the FreeBSD VM.
- **Coverage is estimated**, not measured: `cargo-llvm-cov` is not installed.
  - **High:** reconcile, ancestor, the session fault harness, config, framing.
  - **Low:** `tree/diff` and `tree/apply` (6 tests, although scan deltas travel on the wire), `supervisor/control.rs`, `update::run`.
  - **None:** `service.rs`. `tray.rs` has 1 test, which never runs in CI.

### Missing tests, in priority order (each confirmed absent)
1. **An emptied root holding only `Untracked` children** (C1). Also: `Untracked` nodes at *every* depth in the reconcile proptest generator, in every mode including `TwoWayStrict`, with a "no silent loss" property: every side change since the ancestor is either propagated or conflicted.
2. **Resolving an in-sync path, twice** (H1), and `resolve group ./`.
3. **Incremental equals full for dir→dir replacement:** swap, `rmdir`+`mkdir`, rename-over (H2).
4. **Two endpoints sharing one observer, through the real `transition()`:** offer refusal and wake-up (H3).
5. **A deep-tree scan** at about 2,000 levels, and a deeply nested `Node` decoded on the router thread (H4, I9-B).
6. **Supplying a file larger than a small cap**, asserting the peak `pending` size (H5).
7. **A size-excluded file, FIFO or ignored symlink inside a deleted directory**, and an edit that crosses `max_file_size` against a sibling deletion (H6).
8. **A peering lease accepted at a higher term while the old leader is mid-cycle**, and two channels presenting leases concurrently (H7). Also: handoff with mixed plain and peering plans, and `yield --to`.
9. **A hostile `ScanDeltaHeader`:** huge `length`, `block_size` of 0, 1 or `u32::MAX`, and an expansion bomb. Also a multi-frame message past a sane cap (S1).
10. **`supply_from` with absolute, `..`, symlinked or unrequested paths** (S2).
11. **Permissions of staging entries and temp files, and a planted symlink at a temp name** (S3).
12. **The attach and control sockets refusing a peer with another uid**, and the socket file mode.
13. **A middle journal record with a flipped length field** (M1). An open that upgrades the format while an intent is unresolved.
14. **A stale `.autobahn-tmp-apply-*` in a directory being deleted** (M3).
15. **The polling fallback (`suppress_watching`) with an *unannounced* external write** (M7). A re-included directory's watch (M8). A root replaced while watched (M9). A runtime `watch_tree` failure (M10).
16. **A worker panic or stdout write failure** (M4). A panic in an agent channel (M5). Connection-setup and handshake timeouts (M6).
17. **Property test `apply(base, diff(base, target)) == target` over random trees.** A randomized rsync round trip including a forced weak-hash collision.
18. **`service.rs`:** unit and plist output with spaces, `%`, `"`, `&` and `<` in paths. `rotate_log` at its threshold.
19. **`update::run` with a fake fetcher:** tampered binary, tampered bundle, missing `SHA256SUMS`, rollback restoring the bundle. `install.sh` under shellcheck, plus a smoke test against a draft release.
20. **Reload** that removes a running group, or changes its mode or betas (only adding a group is tested today). A reload while a cycle is in flight.
21. **CLI exit codes;** `status --json` schema stability; `init --force`; `enable`/`disable` on a symlinked config; `clean --dry-run`.
22. **Spec coverage.** Spec and replay cover only three two-way modes, files and directories, and perfect transitions. Add `two-way-paranoid` and the one-way modes to `MODES`, and add refused or partial transitions. C1, H6 and M2 all sit outside the spec's alphabet: the spec has no notion of a subtree that "left tracked scope".

### CI gaps
- `fmt` and `clippy` run only on Linux with default features. `--features tray` is never linted, or even compiled, on Linux, though the docs claim Linux tray support.
- There is no `cargo audit` or `cargo deny` job.
- Release tags build without running tests, and a tag can point at a commit whose CI was skipped by `paths-ignore` or `[skip mac]`.
- The examples are compiled but never run.

---

## 6. What is done well

- **Invariant-driven engineering:**
  - `INVARIANTS.md` names the enforcing code and the checking test for every claim, and many of those tests are mutation-checked.
  - Nearly every cited test exists. The one exception is noted above.
- **Crash-safety harnesses:**
  - The journal is cut at every byte.
  - A real agent connection is cut at every frame boundary, in both directions.
  - A phase-by-phase fault harness covers staging and transitions.
- **Ancestor discipline:**
  - Writes are synchronous, and intents are recorded before any transition and fsynced when a remote endpoint is involved.
  - After a crash the affected paths are tainted, so a crash surfaces as a conflict rather than an overwrite.
  - A compaction whose directory sync can't be confirmed never truncates the journal.
  - Corruption fails closed rather than resetting.
- **The write path:**
  - Every mutation is validated against the exact lease snapshot.
  - Parent directories are walked component by component with `symlink_metadata`.
  - Removal is bottom-up and accounts for every entry.
  - Creation is atomic via `RENAME_NOREPLACE` / `renamex_np`.
  - Metadata is taken from the staged inode before the rename.
- **The frame decoder:** every length is capped before allocation, compressed size is declared and capped, and unknown flags are rejected. Adversarial tests cover random bytes, every truncation offset and bomb-shaped frames.
- **The observer's generation protocol:** record-then-advance, the serve gate, and announcements both before and after writes. It is argued carefully and tested with a randomized interleaving sweep. The holes found here are all at call sites and edges, not in the protocol itself.
- **Structural sharing:** it makes "nothing changed" nearly free, and its one-directional contract ("can prove agreement, never difference") is written down.
- **Operational care:**
  - The control socket checks peer credentials.
  - `--` precedes the host in every ssh argv.
  - `autobahn update` verifies before moving anything, runs the new binary before swapping and rolls back on a failed restart.
  - Backoff jitter is applied after the cap.
  - Config has `deny_unknown_fields` everywhere and reports every error at once.
- **Comments explain why.** That made this review much faster. The problems are refactor leftovers, not a lack of intent.

---

## 7. Recommended order

1. **Fix C1** (a one-line predicate change plus tests) and **turn CI green** (3 clippy fixes, and gate the FreeBSD update tests), so the spec and macOS jobs run again.
2. **H1** (refuse `resolve` on paths that are not in conflict) and **H3** (offer with the lease generation). Both are small and both lose or roll back data.
3. **H6 and M2:** carry a reason on `Untracked` and make the reconcile policy explicit. Extend the proptest generator and the spec alphabet so this whole class stays covered.
4. **H2, H4, H5:** relist a marked directory, give threads explicit stacks or cap depth, and supply lazily.
5. **Security quick wins:** S1 (validate the header), S2 (validate supply paths), S3 (0700/0600 with `O_EXCL|O_NOFOLLOW`), S5 (ssh options), S7 (workflow permissions and pinning), and the `install.sh` tarball check with fail-closed behaviour.
6. **Isolate `HOME` in the unit tests**, and give the mux tests unique session ids.
7. **Peering (H7 and related)** before the feature loses its `-experimental` tag.
8. **Performance P1 to P3 and P6**, then the structural refactors in §4.
