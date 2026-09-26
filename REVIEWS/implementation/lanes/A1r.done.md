# Lane A1r: done (replay of lane-A1 onto integration)

## Replayed commits
- T1-1 done f961c96 (from c5fc052): open_scanned supply gate. Two conflicts in src/endpoint/local.rs: the `std::os::unix::fs` import (kept A2's DirBuilderExt; A1's OpenOptions import merged cleanly) and the end-of-file test modules (kept A2's `apply_path_tests` and A1's `confinement_tests`, one after the other).
- T1-5 done 7181e8f (from f6e2302): conflict at the top of stage_begin. Order is now A1's receive-state discard, then A1's per-request validation, then A2's `prepare_staging_root` (in place of A1's `create_dir_all`, so A2's symlinked/private staging checks stay). Second conflict: kept A2's `copy_into_private` next to A1's `open_base`/`base_signature(File)`.
- LOCAL-03 done (A1's part) 92d8f13 (from 85b7ee5): the only conflict was a test-module doc comment (took A1's rename to `supply_receive_tests`). Follow-ups amended in, and named in the commit message: removed `copy_verifying`, which had no callers left (A2's publish path uses `copy_into_private`); split `temporary_name` into `temporary_name` plus `temporary_name_at(purpose, count)`, so the planted-symlink test plants at real upcoming names again. After A2's keyed token it had been passing vacuously. It plants 4 names, fewer than staging_temporary's 8 attempts. Mutation check: with `File::create` in staging_temporary, the test fails.
- F-H7 done 741bec9 (from 1150671): applied cleanly, including tests/supply_memory.rs.
- F-M-STAGE (M-31 only) partial, as it was in A1, 93bf619 (from 7b7005d): applied cleanly.

## Last full-suite run (`isolated lane-A1r cargo test --release --no-fail-fast`, at 93bf619)
test result: ok. 529 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 63.21s
test result: ok. 44 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
test result: FAILED. 34 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 24.70s   (only a_standing_watch_lets_the_next_cycle_skip_the_beta_scan, the known flake; it passed 3/3 when re-run alone)
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.96s
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.96s
test result: ok. 63 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 5.85s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.34s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.76s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
cargo fmt --check and clippy --release --all-targets -D warnings: clean.

## For the integrator
- `open_base` (A1) still duplicates the parent walk in A2's `resolve_confined`. It could become `resolve_confined` plus the O_NOFOLLOW|O_NONBLOCK open. I left it alone because that is a refactor, not a conflict resolution.
- stage_begin's reserved-prefix check is redundant if validate_path already refuses TEMPORARY_PREFIX names after A2's T1-4. It is harmless.
- T1-1's INVARIANTS wording is still with the wave-2 docs lane.
