# Lane F1r — done (replay of lane-F1 onto integration 94a22ae)

- REL-1 step 1: done — b68242f (from 169bc2c) — applied cleanly.
- LOCAL-06: done — 25e3c8a (from 5c94d1d) — applied cleanly.
- F-M-UPD: done — d5e0cd1 (from 5bbdd5f) — no textual conflict (main.rs auto-merged), but it did not compile: integration's 910ae3f added `Probe::Unresponsive`. update.rs `running_build` now maps it to `RunningBuild::Absent`, so a wedged supervisor is polled past and never counts against the update, like a socket that never answered. Amended into the commit and described in its message.
- LOCAL-08: done — a796560 (from fcef177) — conflict in src/main.rs: integration's `Unsettled` error type (exit code 2) and F1's `refuse_root()` were both added at the same place. Kept both. Checked that integration added no new `Command` variants since wave-0, so `refuse_root`'s command list still covers every command.

## Final full suite (isolated lane-F1r cargo test --release --no-fail-fast)
test result: ok. 588 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 73.33s
test result: ok. 44 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
test result: FAILED. 34 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 24.56s   (e2e: only a_standing_watch_lets_the_next_cycle_skip_the_beta_scan, the known flake; passes when rerun alone)
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.12s
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.26s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.90s
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.96s
test result: ok. 72 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 5.62s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.50s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.76s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
cargo fmt --check and clippy -D warnings are clean at a796560.

## For the integrator
Everything in /home/ubuntu/lanes/F1.done.md "For the integrator" still applies:
- release.yml must publish scripts/install.sh as the release asset `install.sh`.
- Check that the macOS build compiles (the service.rs platform module was only reviewed by eye).
- Root-only tests skip unless euid is 0. Run them in a root container. They were not run under sudo in this replay.
- After a confirmed update, autobahn.previous is removed. This is a behaviour change.
