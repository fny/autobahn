# F-M-TEST: The tests assert what they claim, in isolation, and say when they skip

**Findings:**
- M-46: the standing-watch test's stop condition is too weak (ASTRA F34, failed on macOS).
- M-47: the connection-cut oracle accepts lost user versions (ASTRA F33).
- M-53: unit tests use the real `~/.autobahn`, share session ids, and leak environment changes (OPUS §5).
- M-54: tests that pass without running (OPUS §5).
- **New:** `fan_out_races::two_betas_edit_different_files_and_both_land` failed on Linux arm64 in CI run 35946282106 (`tests/e2e.rs:1869`).

**Status:** proposed. Medium. Do M-53 first: the other tickets add tests that would inherit these isolation problems.

## Problems and proposed resolutions

### M-46: the standing-watch test

**Problem.** It has changed since the review. It now waits up to four cycles for a non-skipped beta scan (`tests/e2e.rs:1719-1734`), and its comment says "never lost, at most one cycle later". But it still stops at the *first* cycle that scans beta, and then requires alpha to hold beta's edit. A scan triggered by something else, such as a late event from an earlier transition, ends the loop before the kernel has reported this edit. The assertion then fails, although the edit would have arrived a cycle later. ASTRA saw this fail twice on macOS.

**Resolution.** Loop until alpha holds `"edited on beta"`, and fail only after a bound: 4 cycles or 20 s. Assert separately that at least one of those cycles scanned beta, which is what the test is about. Test late events and the polling fallback with the deterministic hooks the observer tests already use, not timing.

### M-47: the connection-cut oracle

**Problem.** After each cut, `every_cut_connection_recovers_to_a_safe_tree` accepts either version of `modify.txt` on each side, and either absent or created for `created.bin` (`:885-887`). It then requires the two trees to be equal if there were no conflicts. Both sides reverting to the *old* `modify.txt`, or both losing `created.bin`, pass. That would be silent loss of the user's latest version. It is a false negative in the test, not evidence that the code does it.

**Resolution.** After recovery with no conflicts, require the *new* versions on both sides: `modify.txt` holds `new_bytes`, and `created.bin` exists. With conflicts, require each new version to survive somewhere, on one side or in a conflict copy. Extend the scenario to cover a deletion, a file-to-directory change, and a restart between the ancestor record and the transition.

### M-53: isolation

**Problem.**
- **Tests write to the real home.** `create_endpoint` builds staging under `$HOME/.autobahn/staging` (`src/transport/mod.rs:918-929`). The mux tests call it through `serve_agent` with the real `$HOME`. During OPUS's review they rewrote `~/.autobahn/staging/mux-test-17-beta.scancache` and ran `remove_dir_all` under it, on a machine running a live supervisor.
- **One session id for every mux test.** All eight use `mux-test-{root.len()}` (`src/transport/mux.rs:644`), which comes out as `mux-test-17` for every temporary directory. Parallel tests therefore share staging and scan caches.
- **Global environment changes.** `src/transport/install.rs:570` sets `HOME`. `tests/supervisor.rs:1186-1187` sets `AUTOBAHN_SSH` and `AUTOBAHN_AGENTS_DIR` and never unsets them. `:2379` sets the attach command variable, which is removed only on success (`:2465`), so a panic leaks it. Environment variables are global to the process, and tests run in parallel. Edition 2024 makes `set_var` `unsafe` for exactly this reason.

**Resolution.**
- `create_endpoint` takes its state area from `crate::paths`, which honours `AUTOBAHN_HOME`, instead of reading `$HOME` directly. Add a test-only constructor that takes the state area as an argument, and use it in the mux tests, each with its own temporary directory.
- Generate session ids with `session_identifier(…)` over each test's own paths. T1-2's validation requires that anyway.
- Replace `set_var` in tests:
  - pass the value through an explicit parameter or a builder, as with `Supervisor::with_ssh(…)`;
  - where a child process needs it, set it on that child with `Command::env`;
  - where a global really is required, serialize those tests behind one lock and restore the value in a drop guard.
- Add a CI check that fails if a test run changes anything under the real `~/.autobahn`. Snapshot the directory's listing before and after.

### M-54: tests that pass without running

**Problem.** The TLC replay tests print "skipped" and return when `AUTOBAHN_TLC` is unset (`tests/spec_replay.rs:694-696`), and the runner reports them as `ok`. The non-UTF-8 file-name test does the same on APFS.

**Resolution.**
- Mark the TLC tests `#[ignore = "needs TLC: set AUTOBAHN_TLC=1 and run with --ignored"]`. The CI `spec` job, which already sets `AUTOBAHN_TLC=1`, runs them with `-- --ignored`. A local run then shows them as ignored, not passed.
- For platform-dependent tests, detect the platform at the top and mark them ignored there, with the reason. `#[cfg_attr(target_os = "macos", ignore = "APFS refuses non-UTF-8 names")]` is enough.

### New: the fan-out test that failed on Linux arm64

**Problem.** `two_betas_edit_different_files_and_both_land` (`tests/e2e.rs:~1851`) failed in CI run 35946282106 at line 1869, on Linux arm64 only. It uses `interleave` to run session 2's whole cycle at a chosen point in session 1's, `BeforeAlphaTransition`. Whether this is a flaky test or a real race is unknown from one run.

**Resolution.**
- Rerun that test 200 times on an arm64 runner or machine: `cargo test --release --test e2e two_betas_edit_different_files -- --test-threads=1`, in a loop.
- If it fails, capture the report and both trees, and file a correctness ticket. F-H3 changes the fold offer these interleavings exercise, so try again after F-H3 before investigating further.
- If it never fails, look for timing assumptions in `look_again` or the harness, and fix them the same way as M-46.

## Tests

This ticket is about tests. It is done when:
- the standing-watch test passes 200 times in a row on macOS;
- the cut oracle catches a deliberately injected rollback of `modify.txt`, as a mutation check;
- a full test run leaves `~/.autobahn` unchanged;
- `cargo test` lists the TLC tests as ignored.
