# TODO

Running list, kept in the repository.

Open work is sorted by what a v1 needs. Everything settled is below, with the measurements that produced it.

## Before v1

Each of these is either a way to lose data, a version mismatch that has already cost an outage, or a shape that is cheap to change now and expensive to change after a v1.

- [x] **`one_file_system = true`, and probably first.** Do not walk into a directory that sits on a different filesystem. `rsync -x`, `tar --one-file-system`, `find -xdev` and `du -x` all do this. It uses the same device number as the item that follows, but needs it only during the walk. It never has to remember one, so it needs no new field, no ancestor change, no carry-forward, and no epoch bump. It also removes the problem in both directions: a mount that appears copies nothing, and a mount that goes away deletes nothing. The mount guard is then only needed for people who turn this off. Decide the default. `true` matches every other tool. The agent needs the option too, so it goes in `Initialize`.

  **Decided 2026-09-23:** named `ignore_mounts`, default `true`. The scan reports the mount points it skipped and the session excludes those paths on both sides, so a real directory on the other side is never copied into the mount. That list is also item 9's; one epoch bump covers both.

  *Why here:* a mount that appears or goes away must not copy or delete anything; the cheap half of the fix

- [x] **The agent bundle has no version, and nothing checks it.** Found by breaking every Linux session on 2026-09-03. The epoch went 5 -> 6, the installer uploaded `agents/autobahn-linux-x86_64` — a cross-build from Sep 1, holding epoch 5 — and named it `autobahn-0.4.0+e6`. The name carries the controller's version; the *content* is whatever file sits in `agents/`. macOS hosts were fine, because for the local platform the installer sends the running executable, which is always current. The handshake refused the mismatch, so nothing was corrupted and the sessions only errored — but the message blamed the remote host. The message now names the bundle and its age. The real fix is for the bundle to state its version: a manifest beside it, or a marker string the installer can find in the bytes, checked before upload. Failing closed there turns a ten-minute outage into a refusal to start. Related: agent bundles are moving to GitHub Actions, which removes the "built by hand somewhere, age unknown" problem at the source. The version check is still worth having — a release artifact can be stale in a workflow too — and CI is the natural place to stamp the bundle with the version the check reads.

  **Decided 2026-09-23, with the next item:** the release writes `agents/MANIFEST` (platform, version, digest per line), and `ensure_agent` refuses to upload a bundle binary whose manifest version is not the controller's, before any ssh. No manifest (a local cross-build) falls back to the handshake message.

  *Why here:* this broke every Linux session once, and cost an hour again on 2026-09-17

- [x] **A rebuilt agent never reaches a host that already has that version.** `remote.rs:341` installs only when the first connect fails, and the binary is named `autobahn-<version>`. A rebuild at the same version runs the old agent forever. Found the hard way: the directory message added in 82976d3 did not appear on `boite`, whose agent is from Sep 2 02:15. It hid because the case I tested failed on alpha, which is local. Fix: name the remote binary by a digest of its content (`autobahn-<version>-<digest8>`), so a changed binary always deploys and an unchanged one never re-uploads. Hashing 4 MB costs a few milliseconds and only on connect. Prune old binaries in `clean`. Until then, remove `~/.autobahn/bin/autobahn-<version>` on the remote by hand after any agent-side change.

  **Decided 2026-09-23:** the remote name carries the content digest, `autobahn-<version>+e<epoch>-<digest8>`: the beta always runs the controller's exact build, and identical bytes are never sent twice.

  *Why here:* the same class: a changed agent that never ships is a silent old version

- [x] **The control socket is not versioned.** Adding two fields to `ProgressSnapshot` made every new CLI read the still-healthy running supervisor as "reports running, but is not answering" until the service was restarted — the bincode frame no longer decoded, and the failure looked like an outage. Same class as the agent epoch, one hop closer to home. Either carry the version in the control request and answer a mismatch with "restart the service to match this build", or make the snapshot self-describing.

  **Decided 2026-09-23:** the first frame carries `protocol::version()`; a mismatch is answered with a `Mismatch { supervisor }` both builds decode, and the CLI says to restart. The first build with it reads an older supervisor as "not answering; try `autobahn restart`".

  *Why here:* a new CLI reads a healthy supervisor as broken; one hop closer to home than the agent epoch

- [x] **Version the journal records too.** The checkpoint states its format; journal records do not. Today it does not matter, because an older checkpoint is converted at open and the journal is retired with it, so records never outlive the build that wrote them. A change that lands without a checkpoint rewrite would break that. Either state the version per record, or write down why the conversion at open is sufficient.

  **Decided 2026-09-23, revised 2026-09-24:** old layouts stay readable. Checkpoint formats 0 to 2 still read (`OLDEST_READABLE_CHECKPOINT`), and so do format 2's journal records, which carry no checksummed header; an open that finds either rewrites the store in the current format at once, so the weaker rule that reads them lasts one open. A golden-bytes test pins the record encoding, so a change to it fails CI and must raise `CHECKPOINT_VERSION`. `read_journal` takes the checkpoint's version. The day a reader is dropped (raising `OLDEST_READABLE_CHECKPOINT`): `autobahn update` asks the new binary which formats it reads; if not this machine's, it upgrades only once every session is settled, and the new build rebuilds the ancestor from two matching sides (item 10). A hand-copied binary that finds an unreadable journal rebuilds when the sides match and otherwise refuses, naming the version to go back to. The golden-bytes test and the version plumbing are in (2026-09-24); the pre-update check and the rebuild land with `doctor`.

  *Why here:* a format that v1 freezes should state its own version while that is still free

- [ ] **`fold_transition` diverges on ~2% of transition-cycles.** The cause of every "baseline could not be reproduced" 21 MB re-send. Both sides run the same fold over the same transitions and outcome, and about one time in fifty the two encodings differ. Perfectly correlated with transitions (sessions with none never miss; five sessions with them missed at 1.2–2.4%). Dormant since the chown and the collision fix removed the perpetual transitions, so there is no live reproduction. To find it: on a miss, have the controller keep its folded encoding and request the agent's, and diff the two trees — the first differing node names the fold rule the two sides disagree on.

  **Decided 2026-09-23:** parked until it recurs; diff-on-miss logging when it does.

  *Why here:* two sides encoding the same tree differently is the one open correctness question

- [x] **That nested case reports as *errored*, not *halted*.** The session stops and protects its peer, so the behaviour is right, but `alpha root ... does not exist` is a plain error — so `on_error` fires where `on_halt` belongs, and `status` says the wrong word. A vanished root is the definition of a safety halt. Deciding this changes which alert hook fires, so it is worth saying out loud before doing it.

  **Done 2026-09-24:** `SafetyHalt::AlphaRootMissing`, recorded `halted` with a message that says why and what to do. It alerts after two minutes, not at once, because a drive often returns with the laptop's wake, and it clears on its own when the folder is back.

  *Why here:* the wrong state word fires the wrong alert, and a vanished root is a halt

- [x] **Merge `[alerts]` into `[defaults]`.** Currently its own top-level table. Open question to settle when doing it: is this purely relocation (tidier config, same global behaviour), or does living under `[defaults]` mean groups may override alerts individually — a different `on_alert` for `voltai` than for `aws`, say? The second is more work (the alerter is currently one state machine over all sessions, and per-group commands would need the firing split by group) but is what `[defaults]` promises everywhere else in the file, so the name would otherwise lie. Config compatibility: keep reading a top-level `[alerts]`, since it is in the wild as of 758e240.

  **Closed 2026-09-24:** the relocation happened (`on_alert` at the top level, timing under `[advanced.alerts]`, `dab4e64`). One hook for every group; per-group hooks not wanted.

  *Why here:* config shape: decide before v1, even if the decision is to leave it

- [x] **The `fan_out_races` e2e tests fail about a third of the time.** Found 2026-09-24: 5 of 10 runs at `5d47b4e`, 3 of 10 after the doctor work, so it predates it. Two of the three race tests (`two_betas_edit_one_file_and_the_later_write_is_refused`, `an_edit_landing_on_alpha_between_a_scan_and_its_transition_is_carried_next_cycle`) assert an outcome that depends on whether the second session's watcher has seen the first session's write by the time it cycles; `events_delivered()` waits a fixed 250 ms. Either wait on the observer's generation instead of a sleep, or find which change since `cbdca75` made the window matter — the settle (`SETTLE`/`QUIET` 25/5) and the standing-watch scan skip are the suspects. CI will see it.

  **Fixed 2026-09-24:** a timing flake in the tests, not in synchronization. Single-threaded they never failed; in parallel the 250 ms pause lost to event delivery under load, and waiting on `await_change` instead was worse — a session's own initial copy leaves late events that answer the wait before the test's write does. The interleaved cycles now read their trees in full (`request_verify`), so they see every write regardless of the watcher: 0 of 20 parallel runs fail. The third test's follow-up cycles on each wake until the edit arrives, asserting no conflict.

## Can wait

Real work, none of it load-bearing for a first release.

- [x] **Record mount boundaries by device number.** The proper fix for the hole reopened by dropping I7-A: a mountpoint removed on eject (macOS `/Volumes`, automounts, or any mount whose parent was on the vanished filesystem) presents as *absent*, which now propagates. Compare each directory's `dev()` with its parent's during the scan — the stat already happens, so the device number is free — carry the boundaries in the snapshot and the session state, and halt when a recorded boundary is empty or absent. That makes the trigger a fact rather than a shape, and lets the size threshold go entirely. Costs: `Snapshot` gains a field, so the compatibility epoch bumps and every agent reinstalls; the mount list must survive incremental scans (which adopt subtrees without visiting them) or it silently empties; A/B the scan hot path before commit.

  **Decided 2026-09-23:** built with `ignore_mounts` (item 1), from the same list of skipped mount points, under the same epoch bump.

  *Why here:* the proper fix; needs a snapshot field and an epoch bump

- [x] **Rebuild an ancestor that cannot be read, when it is safe.** Now justified by corruption alone — versioning covers planned changes. Scan both sides. If they hold identical content, adopt it and carry on: nothing can be resurrected when the two sides already agree, so the fix is provably a no-op. If they differ, do not act. Halt and name `autobahn reset`, because rebuilding then resurrects deletions. Make it loud either way, and refuse a second rebuild on the same session — a disk that corrupts one ancestor will corrupt another, and a silent retry turns a hardware fault into a mystery.

  **Decided 2026-09-24:** built with `doctor` (next item), which shares its "do the two sides match?" check; it also serves the journal plan (item 5).

  *Why here:* today it halts and names `reset`, which is correct if unkind

- [x] **`autobahn doctor <group>`** — promote `examples/probe.rs` to a real command. Read-only: opens both endpoints, scans, and reports what each root looks like plus every directory populated on one side and empty/absent on the other. It answered a question the product could not answer about itself, which is the argument for shipping it. Delete the example when it lands.

  **Decided 2026-09-24:** do it, together with item 10. Also reports whether the ancestor loads and how far each side has drifted from it, so it says before a `reset` whether the reset is free.

  *Why here:* a diagnosis command; nothing depends on it

- [x] **Find out which phase is slow on fny, before optimizing anything.** Paused 2026-09-03 part way through. `examples/cycle_cost` now runs on real roots (cf68e9d) and is built on fny; the next step is simply to run it there against `~/Workspace/arcturus` and `~/Workspace/currents` and read which column dominates — rescan, reconcile, validate, encode, or write. Everything below depends on that answer and none of it should start without it.

  **Closed 2026-09-24:** answered by the warm-walk profile (TODO-SPEED, "The warm walk — explained"): the cost was the one-shot process's watcher registration, now skipped, and the scan itself is parallel.

  *Why here:* everything in performance waits on this measurement

- [x] **Parallel scanning — worth doing only if `rescan` is the column.** The case is not "make alpha faster", it is "make fny faster", and the agent runs the same scanner. Note that `bench/ab.sh` drives two *local* roots, so it cannot see a remote-dominated cost and would report nothing — the instrument has to match the machine the cost is on.

  **Closed 2026-09-24:** done in `c4e4109`, measured (160k-file cold scan 7–34 s → 2.9 s); the agent runs the same scanner.

  *Why here:* only after the measurement says `rescan` is the column

- [x] Parallel scanning — hashing pool first (clear win on first scans), then a parallel walk (pays off every cycle). Both hot-path: A/B gate before commit.

  **Closed 2026-09-24:** the parallel walk (`c4e4109`) hashes in its helpers, which covers the hashing pool too.

  *Why here:* same, with the order to do it in

- [x] Remote scans report no progress counts: the agent scans inside one request and the protocol carries no frame for progress. Needs a new response variant and a compatibility-epoch bump.

  **Decided 2026-09-24:** bundled into the `ignore_mounts` epoch bump. The agent sends a count only while a scan is still running after ~500 ms, from a side thread that stops before the reply; A/B a cold scan and the 1-editor cell before committing.

  *Why here:* needs a protocol response and an epoch bump

- [x] Collapse fully-synchronized groups to one line in `status`, so 15 healthy sessions don't cost 45 lines of scrolling.

  **Decided 2026-09-24:** a group whose destinations are all synchronized and idle is one line (`✓ 3 synchronized · last cycle 4s ago`); any trouble or visible work expands it. `status <group>` and `status --all` expand everything; `--live` and `watch` collapse too; `--json` unchanged.

  *Why here:* 15 healthy sessions cost 45 lines; a reading problem, not a correctness one

- [x] **Three integration tests were broken for a day.** `82976d3` (the resolve confirmation) and the `issues` rename each broke assertions in `tests/supervisor.rs`, and neither was caught, because after each change I ran only `--bins`. The lesson is not "run everything every time" — it is that a change to a command's *output or prompting* has to run that command's integration tests, which are the only place the wording is asserted. Worth a note in the contributing guide.

  **Closed 2026-09-24:** CI runs the full `cargo test --release`, integration suites included, on every push to `main` on four platforms (`98ea283`), so a broken wording assertion fails the first push.

  *Why here:* a note for the contributing guide

## Settled

### UX

- [x] **`conflicts --depth` drill-down hint: backticks and wording.** Done alongside the positional fix: the hint is now `` `autobahn conflicts <group> --depth N` opens the next level ``.

- [x] **`conflicts` took a host where its siblings take a path.** Done. `autobahn conflicts voltai autobahn` scopes to that folder, `--host` filters the destination, and a folder given as the selector scopes too. Depth counts from the scope.

### The vibe halt (found 2026-09-02)

- [x] **The emptied-subtree halt lies and withholds the path.** Done: the absent-directory form (I7-A) is gone, so deliberate deletions propagate; the remaining halt names the path, the side, and the entry count.

- [x] **`conflicts` says "no conflicts" for a halted session.** Done, as part of `issues`: a failed session is listed by its state, and a filter that excludes every path does not exclude it.

### Ancestor format (started 2026-09-02)

- [x] **Version the ancestor checkpoint.** Done. Format 2 states its own version, the digest covers it, and formats 0 and 1 still read and are rewritten in the current form on first open. An unknown format is refused with the versions, the command, and what the command costs. A header-only check runs at startup, before any cycle.

### Found while building the shop (2026-09-03)

- [x] **`resolve` cannot settle a directory conflict.** Done, and not by teaching it to copy directories. It now retires the *losing* version and lets the cycle carry the winner — which is what reconciliation already does for one side's deletion, for a file, a symbolic link, or a tree alike. The removal goes through `transition`, so an entry that moved since the scan is refused rather than destroyed. `--keep both` renames the loser aside, a new endpoint primitive (epoch 6). Covered for all three winners by `resolve_settles_a_conflict_between_a_directory_and_a_file`.

- [x] **A directory conflict is recorded as kind "other".** Done, and the answer was not a mislabel. "other" is `Content::Untracked` or `Content::Problematic` — content that exists but cannot be carried — and it is the *reason* for the conflict, not a detail of it. It is almost never at the conflict's root either: a tree is refused because of one entry beneath it. `conflict_detail` now walks the whole side, counts what cannot be carried, and names an example, preferring an unreadable entry over an excluded one because the first is a fault to fix and the second is policy. `issues` and the shop both show it. Found the live cause of the two `vibe` -> `boite` conflicts this way: `happy` and `voltagen` hold `node_modules` and `.git`, which the default ignores exclude.

- [x] **`resolve` cannot settle a conflict caused by excluded content, and did not say so.** Done, and it was destructive, not merely silent: `--keep <the absent side>` ran the removal, which strips bottom-up, took away every entry it could account for, and left the excluded one — a half-deleted tree with the conflict still open. It then reported "changed while this ran, run again", which is never true of this case. It now checks before touching anything, settles nothing, and names the three ways out. `--keep both` is the one that works, because a rename moves ignored content along with the rest; verified end to end by `resolve_will_not_half_delete_a_tree_holding_excluded_content`. Still open: `issues` offers three winners for these conflicts when only one can help, and should say which.

### Ignored content and deletions (2026-09-03)

- [x] **A deletion could not propagate past ignored content.** Deleting a directory holding a `.git` or a `node_modules` became a conflict that no resolution could settle, which is to say most project directories could never be deleted through synchronization at all. An ignore says which files synchronization *carries*, not which files exist, so a deletion now takes the tree whole. Measured: -5.3 ms of p50, no tail stalls (an earlier design that left the ignored entries behind and hid the leftover directory cost 6-9 s on two A/B legs in five, from a tree-walk on the reconcile hot path).

- [x] **The nested-sync exception.** An ignored path that is another session's root is the one case where this costs something nobody agreed to. No new machinery was needed: the second session finds its root gone and stops, carrying nothing to its own peer. Covered by `a_nested_session_halts_when_an_ignored_path_holding_its_root_is_deleted`.

### Where scan time actually goes (measured 2026-09-03)

Recorded because two plausible optimizations were aimed at the wrong machine, and the reasoning behind each looked sound right up to the measurement.

Metadata walk of the `voltai` roots, cold, `find` excluding `target`:

| | with `mutagen-bench` | without |
|---|---|---|
| Mac (alpha) | 3.4s | ~4s |
| fny (beta)  | 27.9s | 9.2s |

- **The cost is the remote side.** Alpha walks its whole tree in 3-4s. fny took 27.9s, of which `mutagen-bench` was ~19s. That is the entire 22.5s cycle, and ignoring that one directory bought 4.25x (4 cycles per 90s -> 17).

- [x] ~~Scale the full-scan interval with tree size.~~ Written, tested, measured, and dropped. The premise was that the flat 120s `FULL_SCAN_INTERVAL` was costing large roots a full walk every two minutes. It is not: 606,001 entries, fixed schedule 57 cycles per 300s versus 56 for a schedule that followed measured cost. No difference. The number that started it — a 62s walk — came from a *cold one-shot process*; inside a running supervisor the baseline is resident and digest reuse means a full walk of 606k entries is sub-second. Do not reach for this again without measuring a warm supervisor.

### What the fny stalls actually were (measured 2026-09-09)

Every earlier theory was wrong. Instrumenting the agent itself — to a file, because its stderr is discarded — found two independent causes, both now fixed on the live system.

- [x] **The remote watch could never be established.** `~/Workspace` on fny has 325,509 directories before ignores; `fs.inotify.max_user_watches` was 256,423. The recursive inotify watch walked the tree for ~20s, hit `ENOSPC` deep inside a `.venv/__pycache__` it would never scan, failed, and retried every 30s — *inside* `snapshot()`, so every scan request blocked behind it. Watch count oscillated 207k → 127 → 197k. That was the "23s scan every 53s". Mitigated: `max_user_watches = 1048576`, persisted in `/etc/sysctl.d/60-autobahn-inotify.conf`; the watch now holds at 330,612. Worst `unchanged` scan exchange fell 22.4s → 7.1s.

- [x] **The Mac did a full 233k-entry scan every cycle.** Any transition problem calls `distrust_baseline()`, which clears `last_full_scan`. The one blocked path (a unicode-collision twin fny held as two files) was refused every cycle, so every cycle forced a full walk of alpha. Fixed by deleting the NFD twin on fny (hash-identical to the NFC one). Blocked is now 0; full scans went from every cycle to every 120s.

- [x] **Durable fix: do not watch ignored directories.** The sysctl is a per-host mitigation. 39,252 directories need a watch; 325,509 get one. On Linux, notify's `Recursive` walks with `WalkDir` and has no filter hook, and it extends to new subdirectories only for parents registered recursive (`inotify.rs:67`). So: walk with the scanner's ignore rule, add `NonRecursive` watches per directory, and add a watch on `Create(Folder)` for non-ignored paths from a dispatch thread that owns the watcher (the callback cannot). Keep `Recursive` on macOS (FSEvents is native). Agent-side → epoch bump; A/B on fny, not locally. Done in 7119f85 (epoch 8): Linux builds the watch itself with the scanner's ignore rule, one non-recursive watch per directory, extended on the way in for directories that appear. A/B on fny: +0.4 ms p50 inside a 3.1 ms spread, no stalls.

- [x] **A permanent refusal should not force a full rescan every cycle.** `distrust_baseline()` is right when the disk disagreed with the snapshot; it is wrong for a refusal the snapshot predicted (a collision, an unwritable parent). Distinguish the two in `TransitionOutcome`, or rate-limit distrust per path. Done in 7119f85: `Problem.disagreement` marks the six sites where the disk was found to differ from the snapshot; only those distrust the baseline. Tested both ways.

- [x] **Agent stderr is discarded.** `remote.rs:330` uses `spawn_quiet` (`stderr(Stdio::null())`) for the connection that succeeds, so "unable to watch for changes; falling back to interval polling" is never seen. fny's watch was failing every 30s for days with no trace. Pipe agent stderr to the controller log with a host prefix. Done in 2a0764d: held until the handshake, then printed with the host in front or attached to the failure.

- [x] **Progress `seconds` reports epoch time** when a side's scan window was never begun (`seconds=1788990744` in a status sample). Guard the subtraction. Done in 2a0764d: zero means never; an idle side has been scanning for no time.

- [x] **~210s stall after a supervisor restart** on fny: building 330k watches (~50s) plus the first full snapshot. Startup-only; goes away with the durable watch fix (39k watches). Should be gone with 7119f85 (39k watches to build, not 330k); verify on the next restart.

### Carried over, and since resolved

- [x] 21 blocked paths on `fny.voltai.party` — the root-owned `.ruff_cache` and `static/` paths are gone; the one blocked path today is a different thing (a `.trash` `__pycache__` the `fny` group cannot remove).

- [x] `mutagen-bench` (448k files) on both sides of the `voltai` group — the group ignores it.
