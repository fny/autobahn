# Lane 3ar: done (replay of lane-3a onto integration)

- f35d771 (from 54271d3) HYG-4 leftovers: **done**, applied cleanly.
- 359e77b (from 46b7d12) one-shot open_endpoints: **done**, applied cleanly.
- a247ada (from 6fe5757) M-46 observer seam tests: **done**, applied cleanly.
- 8012a18 (from cb45ef3) fanout.sh path: **done**, applied cleanly.
- fd44d75 (from b1b056e) resolve's flush: **done**. One conflict in src/main.rs: HYG-3 (e159cd2) had put a `///` doc on `run_resolve`, and 3a had put the cfg(test) `RESOLVE_FLUSHES` thread_local in the same place. Kept both, with the thread_local first and the doc comment directly on `run_resolve`.

## Last full suite (`isolated lane-3ar cargo test --release --no-fail-fast`)
test result: ok. 610 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 56.86s
test result: ok. 50 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.26s
test result: ok. 37 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 27.38s
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.12s
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.25s
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.26s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 2.84s
test result: ok. 1 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 2.00s
test result: ok. 72 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 5.87s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.38s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.76s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

## For the integrator
- cargo fmt --check and clippy --release --all-targets -D warnings are clean.
- Lane 3a's items still stand: the non-Linux `ChangeWatcher::new` `hold_events` seam isn't compiled on Linux, so check it on macOS CI. The other bench/verify scripts still default to /home/ubuntu/Workspace/autobahn.
