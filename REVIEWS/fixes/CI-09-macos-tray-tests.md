# CI-09: Run the tray tests on macOS

**Findings:** M-49, the macOS half (ASTRA F36; OPUS). The Linux half is settled: a build-it-yourself guide plus a wishlist entry.
**Status:** proposed.

## Problem

The macOS CI job runs `cargo test --release` with default features, then builds the app with `--features tray`. So the tray's tests (`src/tray.rs:1295` onward) are compiled into the app but never run, and clippy never looks at tray code.

## Proposed resolution

In the `mac` job:
- add a `cargo test --release --locked --features tray --lib tray::` step, or the full suite with the feature if the time is acceptable;
- add `cargo clippy --release --locked --features tray -- -D warnings`.

Both reuse the job's existing cache.

## Tests

- The next CI run shows the tray tests executing in the `mac` job's log.
