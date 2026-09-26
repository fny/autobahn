# Lane F2r: done (replay of lane-F2 onto integration)

- LOCAL-02 done 1a8cb73 (from 7613321): conflicts resolved — config.rs: integration split Config::load into load+parse, so the loose-permissions warning sits in `load` before `parse` (a supervisor reload via `parse` doesn't re-warn); main.rs run_sync: integration's topology/OwnState/secrets checks kept, F2's private_dir of the session directory added after them; resolve_state_root keeps `exclude_state_root` then `prepare_state_root`.
- LOCAL-04 done c83be21 (from 92426da): applied cleanly (tray.rs auto-merged next to integration's invocation::diff_command).
- LOCAL-05 done bc7987f (from 6d1913d): conflict with integration's client timeouts in control.rs — kept connect_client/send_within/probe_within/Probe::Unresponsive; F2's `connect()` became `refuse_another_user`, applied after connect_client in send_within, a foreign peer makes probe_within Absent and supervisor_is_running false (a timed-out connect still counts as running, as integration had it). Root-only tests pass under sudo; mutation check (removing the refusal) fails the test.
- LOCAL-02 follow-up done 04839d0: sync-config and watch now check roots against the state root before creating it (new locate_state_root), keeping F-H25's "a refused sync writes nothing"; tests/supervisor.rs a_name_with_control_characters_is_printed_escaped parses only stdout as JSON (stderr carries LOCAL-02's config warning under umask 002).

## Last full-suite run (isolated lane-F2r cargo test --release --no-fail-fast)
test result: ok. 550 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 55.97s
test result: ok. 44 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.03s
test result: ok. 35 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 24.82s
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.26s
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.97s
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.96s
test result: ok. 72 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 5.63s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.34s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.76s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
(fmt --check and clippy --release --all-targets -D warnings clean.)

## For the integrator
- Still needed from F2: `cargo check --features tray` on macOS (src/tray.rs run_action Action::Diff is uncompiled on Linux).
- Root-only tests in control.rs skip unless root; they passed here under sudo.
- Config::load warns on a group-writable config: on a box with umask 002 any test that mixes stderr into parsed output may see it (only one did, fixed).
- F2's other follow-ups (C2 ancestor temporaries, F1 updater temporaries) still stand.
