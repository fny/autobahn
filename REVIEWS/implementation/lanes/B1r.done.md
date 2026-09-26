# Lane B1r: done (replay of lane-B1 onto integration e2342d4)

- F-H3: done, 7ea1b12 (from a100592). A transition offers its fold at its lease's generation. Conflict: in `LocalEndpoint::transition`, integration's `let swept = Mutex::new(HashSet::new());` (3e64b42) and B1's `let lease_generation = self.seen_generation;` sat next to each other. Kept both lines; nothing else conflicted.
- F-M-OBS: done, b84f154 (from 3125675). Conflict: in `Scanner::probe_entry` (src/scan/mod.rs), integration's F-H25 state-root check (429d653) and B1's switch to `IgnoreSet::traversal`. Kept the state-root check first, unchanged, so it still beats any negation. Then comes B1's `let Some(ignored) = self.ignores.traversal(..) else { Untracked }`. local.rs, observer.rs and ignore.rs merged cleanly.

## Checks
fmt and clippy (`-D warnings`) are clean after each commit. endpoint:: + scan:: lib tests: 180 passed.

## Last full suite (`isolated lane-B1r cargo test --release --no-fail-fast`)
test result: ok. 542 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 57.31s
test result: ok. 44 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.03s
test result: ok. 35 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 24.85s
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.95s
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.96s
test result: ok. 63 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 5.62s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.55s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.76s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
(The known flake a_standing_watch_lets_the_next_cycle_skip_the_beta_scan passed this run.)

## For the integrator
- The watcher's traversal (`IgnoreSet::traversal`, shared with the scanner since M-25) does not know about F-H25's state roots, so a root containing ~/.autobahn gets watch events from autobahn's own state writes. The scanner still reports that directory as untracked, so the result is correct; the only cost is extra dirty marks and wakes. This was already true on integration before the replay. Worth a follow-up ticket if it matters.
- The rest of B1.done.md's notes still apply (`invalidate` returns u64, the observer test harness now watches, and there is no status display of watch state).
