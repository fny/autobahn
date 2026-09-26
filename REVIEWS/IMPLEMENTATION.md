# Implementation record

The v1 tickets in `fixes/` were implemented on 2026-09-24, on a dedicated EC2 instance, following `RUNBOOK-parallel-fixes.md`. 31 Claude agents worked in parallel lanes across three waves, plus rebase lanes for conflicts. The integrator landed each lane on a local-only branch, then applied the 127 commits to `main` here as patches, on top of `fd58594`.

## Verification

- **On the instance, the final integration branch (x86-64):**
  - `cargo fmt --check`, clippy with `-D warnings`, and clippy with `--features tray` (the first Linux build of the tray) are all clean;
  - the full suite passed on six consecutive runs;
  - `spec/check.sh quick` passes against real TLC, and so do both TLC replay suites.
- **On arm64** (a short-lived c7g instance, since terminated): the full suite passed, and `two_betas_edit_different_files_and_both_land`, which had failed once in CI, passed 200 of 200 runs.
- **Here,** on the applied `main` (rustc 1.98.0): `cargo fmt --check` and clippy are clean, and the full suite passes: 806 tests, with the 2 TLC replay tests ignored as designed.
- **`a_standing_watch_lets_the_next_cycle_skip_the_beta_scan`** failed 6 of 41 runs before M-46's fix, and passed 200 of 200 after it.

## Commits by ticket

| Commit | Ticket | Subject |
|---|---|---|
| `df5325a` | CI-01 | style: rustfmt on current stable |
| `8344968` | CI-01 | endpoint: a receive state is small, and a watch's verdict has a name |
| `0e85e98` | CI-01 | lint: clippy is clean on every target, not just the library |
| `ccb046e` | LOCAL-01 | fsutil: private directories, files and temporaries are made in one place |
| `674bb37` | F-H29 | text: a value put into a shell command is quoted as one word |
| `1667d19` | F-M-OUT | text: text from elsewhere reaches a terminal or a log with its control character |
| `3a61f13` | F-M-TEST (M-53) | test: an agent under test keeps its state where the test says, and no test chang |
| `85668a8` | T1-2 | protocol: an agent serves only a genuine session identifier and side, checked be |
| `5e43d5a` | HYG-1 | macos: the app reports Cargo.toml's version, and no agent build is committed |
| `4fd86db` | CI-05 | macos: the app can be built with no identity at hand, and signed without compili |
| `c8eb6fd` | F-C1 | safety: a root or directory holding only ignored entries is emptied |
| `3b20e88` | F-H8 (reconcile part only; the endpoint part is lane A2's) | reconcile: an entry excluded where the ancestor held content blocks the deletion |
| `f227563` | F-M-STATE (M-33 only) | reconcile: a one-way transition expects beta's synchronizable content, not its r |
| `9e2201b` | F-L-MISC (L-31 only) | docs(modes): in one-way-conflict, an edit beta made to a file alpha deleted stay |
| `339e4f2` | F-M-STATE (M-32) | ancestor: a journal record's header carries its own checksum, and a length that  |
| `479840c` | F-L-STATE (L-16, L-17, L-18, L-19) | ancestor: the journal's directory entry, its unresolved intents and a cycle's ou |
| `92e5006` | F-H4 | scan: a directory marked itself is read whole, so one swapped in by rename bring |
| `9486bea` | LOCAL-09 | scan: a file is digested only if the handle opened is the regular file the lstat |
| `e2554a9` | F-L-STATE (L-26 only) | scan: an incremental snapshot is stamped with the start of the scan that recorde |
| `269bc65` | F-H25 (scanner backstop only) | scan: autobahn's own state root is never scanned, wherever it falls inside a roo |
| `7d4712d` | F-H5 | threads: a thread that can walk a deep tree has a deep stack, and a deep tree is |
| `87cd702` | T1-3 | local: a read or keep-both move reaches only what is really inside the root |
| `bbdd743` | T1-4, LOCAL-03 | local: a peer cannot create autobahn's names, and staging lives only in a privat |
| `e2610fb` | F-H8 | local: deleting a directory removes only what a pattern ignores along with it |
| `418db0d` | F-M-STAGE (M-29, M-30) | local: a publish holds its staged content before counting its use, and a crashed |
| `c835d9f` | LOCAL-11 | local: a mode change never reaches a file's other hardlinks |
| `4eac723` | T2-1 | remote: a scan delta's header is checked before it sizes anything, and each oper |
| `88cf319` | F-M-SUP (M-12 part A) | transport: every step of setting up an agent connection has a deadline, and no s |
| `c05ecf1` | OPS-3 | install: every remote script runs under sh, whatever the user's login shell |
| `da3d30e` | OPS-4 | transport: autobahn's ssh connections carry fixed options that no ssh_config can |
| `57aec37` | F-M-OUT (M-8, and the relayed-stderr part) | install, transport: a remote name reaches the remote shell only as an allowed, q |
| `77f12e1` | F-L-MISC (L-9, L-22) | remote, install: a scan answered whole is checked like a reassembled one, and a  |
| `bc8e039` | F-H27 | alerts: the example hook hands the summary to AppleScript as data, and a hook is |
| `33821a6` | F-M-SUP (M-35 only) | alerts: a hook runs within its deadline, its complaints reach the log, and a kil |
| `dc04b1c` | F-H2 | config: a manual sync is held to the same topology as a configured session, and  |
| `707ea01` | F-H25 | config: a local root never holds autobahn's own state or configuration, unless i |
| `06ca50f` | F-H12 | config: sessions are told apart by plan, not by label, and betas sharing a host  |
| `36ae6a3` | OPS-6 | config: a root holding credentials is said so once, when a run starts |
| `65df9ac` | OPS-1 | sync: the exit code says whether it converged |
| `0d69c8f` | OPS-2 | clean: the state of a disabled session is kept, and purged only when asked |
| `44132b4` | F-L-MISC (L-7) | sync: a quoted ~ in a manual sync means home |
| `10d4aea` | F-L-MISC (L-28) | ignore: a wildcard negation under an ignored directory is warned about |
| `234d103` | F-L-MISC (L-29) | select: each matched plan keeps its own relative path |
| `141c813` | F-H12 (L-30) | status: a session's progress is found by its identifier, not its group and host |
| `cbc75fe` | F-H1 | resolve: a version kept is never the one deleted |
| `335436b` | F-H29 | issues: a suggested fix command quotes every value it holds |
| `81add20` | F-H30 | invocation: a file named like a flag reaches resolve as a file |
| `f0035fe` | F-M-OUT (M-6 only) | text: names from elsewhere reach the terminal escaped in status, issues, the sho |
| `a24e0d3` | HYG-2 | style: colour only on a terminal, and never against NO_COLOR |
| `2f2b051` | F-H1 | resolve: the guard checks a fan-out as alpha will be, and a nested path once |
| `6f901c5` | CI-02 | spec: check.sh fails when TLC fails, and when it was given no traces |
| `b9a2e3c` | CI-03 | spec: TLC is one pinned release, checked by its SHA-256 before it runs |
| `46caf70` | CI-04 | ci: every workflow token is read-only, and only the publishing job can write |
| `220a5a5` | CI-06 | ci: the release runs no third-party action, and every action is pinned by commit |
| `354145e` | CI-07 | ci: every cargo build, test and clippy in automation runs --locked |
| `c4b7c2e` | CI-08 | ci: a release requires green CI on the commit its tag names |
| `297dcc9` | CI-09 | ci: the macOS job lints and tests the tray |
| `aef50e3` | CI-10 | ci: a weekly advisory audit, and Dependabot for crates and actions |
| `682f244` | CI-05 | ci: the release builds the app before the signing certificate exists |
| `65e8675` | HYG-1 | ci: a release stops if the app would report a version other than the tag's |
| `654c3c5` | REL-1 (step 1, the workflow part) | ci: each release serves the installer from its own tag |
| `8091f45` | F-M-TEST (M-53, the CI check) | ci: the Linux job fails if the test run changes ~/.autobahn |
| `0baf70a` | F-M-TEST (M-54, the CI half) | ci: the spec job runs the TLC replay tests even once they are marked ignored |
| `65b2aea` | (none) | ci: actionlint passes on every workflow |
| `e1bdf60` | CI-08 | ci: the release gate counts only runs that tested the tagged commit itself |
| `a51da44` | BENCH-1 | bench: the observer listens on loopback unless told otherwise, and writes only b |
| `10d62df` | BENCH-2 | bench: ab.sh --corpus only reads the directory it is given |
| `6e84611` | BENCH-3 | bench: the orchestrator never runs a command through a local shell, and checks w |
| `88bc1ce` | BENCH-4 | bench: ab.sh --remote only runs where its checks look, and a failed leg fails th |
| `45cf9a2` | BENCH-5 | bench: every aggregation drops the same runs, CPU is summed per host, and cycle_ |
| `cd6bc32` | BENCH-6 | bench: the verification scripts and the mi tour use the current CLI, and stop wh |
| `f4a79e7` | LOCAL-07 | bench: ab.sh and git-sync.sh work in private directories of their own |
| `cdd1109` | T1-1 | supply: a peer can name only content this side's own scan recorded inside its ro |
| `e55c2f7` | T1-5 | staging: a staging request names a path inside the root, and its base is never r |
| `6191775` | LOCAL-03 | staging: received and locally copied content is private from its first byte, and |
| `7891502` | F-H7 | supply: a file streams to its destination, and the supplier holds about one batc |
| `633303e` | F-M-STAGE | staging: a base that changes mid-stream fails its own file, not the stream |
| `520ffeb` | F-H13 | supervisor: an edit is applied to the sessions it changes, and disabling the las |
| `29f368b` | F-M-SUP | supervisor: a panic stays in its session, and a log line that cannot be written  |
| `56dc392` | F-M-SUP | supervisor: status, the shop and the tray show what the running supervisor runs |
| `e36a8ac` | F-M-OUT | logging: one event is one log line, whatever the names in it hold |
| `99befde` | F-H3 | observer: a transition offers its fold at its lease's generation, and a foreign  |
| `0c39e6c` | F-M-OBS | observer: a scan serves only what a whole watch vouches for, within the caller's |
| `f0b7d75` | LOCAL-02 | state: the state root and the configuration are private to their user |
| `024ebcd` | LOCAL-04 | diff: both sides are compared from private copies, never from the shared /tmp |
| `d42d32c` | LOCAL-05 | control: the socket's fallback directory is the user's own, and a client speaks  |
| `b9ee1c2` | LOCAL-02 | state: a refused sync or watch creates no state root, and a report's JSON is its |
| `4458981` | REL-1 (step 1) | install: every download is verified before anything is installed, and missing ch |
| `e5409ea` | LOCAL-06 | update: what is installed is what was checked, downloaded where nobody else can  |
| `59c7b68` | F-M-UPD | update: an update is confirmed on the build the service runs, and rolls back bin |
| `e219e67` | LOCAL-08 | root: autobahn refuses to run as root unless root is meant, and never under anot |
| `e023f9d` | F-H25 | state: a supervisor's state root is never scanned or watched, whoever built it |
| `7b433fc` | F-H5 | persist: the state writer encodes a deep tree on a deep stack |
| `3fbbced` | F-M-STAGE (M-30, the startup sweep) | local: the first scan of a root sweeps it for crash leftovers, off the cycle's p |
| `694ac77` | LOCAL-03 | local: staging a local file and publishing by copy share one verifying copy |
| `611054c` | F-M-SUP (M-12 part B) | mux: a request that goes silent fails its connection, and slow work is not silen |
| `c65a14d` | P-13 (remote half) | supervisor: a single pass asks its agents not to watch, as it already asked its  |
| `eeeb615` | F-L-MISC (L-9), HYG-4 | protocol: every scan answers as a delta or "unchanged", and nothing else is acce |
| `b1b1463` | F-M-SUP (M-12 part B), P-13, HYG-4 | protocol: compatibility epoch 15, for the progress frame, the one-shot flag and  |
| `f7687c1` | M-46 | e2e: the standing-watch test waits for beta's edit to land, and edits alpha insi |
| `3dd12d1` | M-47 | e2e: a cut or a restart never loses the user's latest version, and the oracle pr |
| `c327c85` | M-54 | tests: a test that cannot run here is listed as ignored, never as passed |
| `619d070` | lane-2d/7 (tray on Linux) | tray: the menu bar app builds, lints and tests on Linux |
| `dd6386e` | F-H12 | session: control, progress, alerts, status, the shop and the tray tell sessions  |
| `553c3d4` | lane-2d/2 (Probe::Unresponsive, from E2's M-12 part A) | status: a supervisor that does not answer is said to be not responding, in statu |
| `74d7ef2` | lane-2d/3 (HYG-2 and F-M-OUT leftovers) | output: doctor's colour and watch's terminal check go through style, and a cycle |
| `2351eba` | lane-2d/4 (run_issues scope) | issues: a path inside nested groups scopes each group to its own name for it |
| `ae2c797` | lane-2d/5 (group names) | config: a group name cannot start with a dash |
| `235c024` | lane-2d/6 (one warning channel for config; L-28, OPS-6) | config: its warnings are one list, gathered once per load and said once, and sta |
| `1a3e5b4` | CI-2f-1 (apps/macos/test.sh in CI) | ci: the app's build scripts are checked on every push, on Linux |
| `f4c8278` | CI-07 (leftover) | apps/macos: the app builds from Cargo.lock as committed, or not at all |
| `4a8ed7e` | CI-2f-3 (bench/harness formatting) | bench/harness: formatted as cargo fmt formats it |
| `9cf3e69` | CI-2f-4 (shellcheck bench scripts) | bench: fanout.sh and alpha-bench.sh pass shellcheck |
| `bd56f24` | CI-2f-5 (cycle_cost flake) | examples: cycle_cost times the rescan a watcher's report would wake |
| `990c16a` | integration | local: the verifying copy pulses progress for both of its callers |
| `b6bb12f` | HYG-3 | docs: every doc comment sits on its item, and says what the code does |
| `7b8dcb9` | OPS-5 | docs: raw symlinks, a hook's environment and ignored ctime are said out loud |
| `bd67173` | T1-1, T1-2, T1-3, T1-4, T1-5, T2-1 (docs) | docs: confinement holds whatever the peer runs, and only integrity assumes genui |
| `b5750eb` | CI-05, CI-03, CI-07, CI-08, OPS-6, F-H2, F-H25, LOCAL-08, F-M-STATE (docs follow-ups) | docs: the docs name the flags, gates, pins and refusals wave 1 added |
| `3451d3f` | HYG-3, T1-1 (docs), LOCAL-08 (docs) | docs: the invariants and refusals say exactly what the code checks |
| `cab1b27` | HYG-3 | docs: I9 names the hierarchy test by the name lane 2a gave it |
| `4f0399c` | HYG-4 | hygiene: no value is computed only to be discarded, and no terminal check is uns |
| `8f1b8ad` | one-shot endpoints (P-13, local half) | supervisor: doctor, diff and resolve open their endpoints one-shot |
| `eb46d11` | F-M-TEST (M-46, deterministic tests) | observer: a late event and the polling fallback are tested by seam, not by timin |
| `5a71d1a` | HYG (fanout.sh hard-coded path) | bench: fanout.sh finds the harness from where the script lives |
| `82bbb91` | F-H1 follow-up (resolve's flush) | resolve: only the sessions it touched are flushed |
| `f78238e` | F-H1 | resolve: the kept version is a creation, and the ancestor's owner makes it so |

## Lane records

`implementation/` holds each lane's brief, notes, completion report (`<lane>.done.md`) and landing log, plus the baseline. The notes record every departure from a ticket, and every cross-lane conflict and how it was resolved.

## After the push (2026-09-24)

Pushing `main` gave the workflow changes their first real CI runs. Linux and arm64 passed at once. The new isolation check, and macOS, which no machine in the effort had run, found the rest:

| Commit | What CI found | Fix |
|---|---|---|
| `64eae7b` | Library unit tests left endpoint locks in the runner's `~/.autobahn`, caught by F-M-TEST's new check | Unit tests use a per-process temporary state root |
| `22e0dd5` | Two tests assumed Linux: `sh` exits 126 for a missing program on macOS, and macOS's listen backlog never fills | Assert failure, not 127; keep the wedged socket either way |
| `c56ddc3` | macOS's 1,024-byte path limit can't hold F-H5's deep chain | Size the chain from `PATH_MAX`, and skip where no overflow can be built |
| `ffa7b43`, `6f95d11` | Supply tests changed files and scanned before FSEvents reported the change | Those test endpoints are one-shot |
| `db28088` | The `diff` test's stand-in used GNU `stat` | Fall back to BSD `stat`; the macOS test step runs `--no-fail-fast` |
| `588407a` | — | `mac-one-test.yml`: run one test on macOS at any commit, for bisecting without a Mac |
| `9daa6c6` | The first macOS clippy run (the tray step) found a Linux-only test helper | Gated to Linux |
| `bca4543` | **A real race:** two `clean`s at once failed, and `clean`'s `remove_dir_all` could delete a lock file another process had just locked, allowing two holders of one lock | `SessionLock::acquire` rechecks that its path still names the locked file; `clean` removes only the lock file it holds, then an empty directory. Regression tests, 20 of 20 |

CI run 36036712065 on `6f95d11` is green on every job: Linux, Linux arm64, spec (real TLC), and macOS (tests, tray lints and tests, app build, bundle check).

`watch_mode_observes_remote_changes_through_the_agent` (from 2026-08-25) failed once on macOS under the parallel suite. It passed 3 of 3 alone, and in every macOS run after that. It is a timing-sensitive test on a loaded 3-core runner, and worth watching.

## REL-1 step 2 (2026-09-26)

`10096af` release: sign the checksums, and update installs only what the release key signed. The key pair was generated on the dev instance; the private key is the `MINISIGN_SECRET_KEY` secret in the `release` environment of `fny/autobahn`, with a copy at `~/.config/autobahn-release-signing/`. The public key is `release.pub`, compiled into the binary and embedded in `scripts/install.sh`. Full suite in release mode: 819 passed, 0 failed.

A debug-build `cargo test` aborts in `persist::deep::a_deep_tree_encodes_on_the_writer_thread`: 100,000 levels exceed the 64 MiB deep stack at debug frame sizes. CI runs `--release`, where it passes. This predates the change (it fails at `6f95d11` too).

