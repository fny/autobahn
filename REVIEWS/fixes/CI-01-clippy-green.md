# CI-01: Make clippy pass so the rest of CI runs

**Findings:** H-17, the clippy half (OPUS H8; ASTRA validation).
**Status:** proposed. Do this first: the `spec` and `mac` jobs both wait on the Linux job, so neither has run on `main` since these errors landed.

## Problem

`cargo clippy --release --all-targets -- -D warnings` fails with three errors:

- `large_enum_variant` on `enum Receiving` (`src/endpoint/local.rs:1609`, reported earlier at `:1588`). Its largest variant is at least 2,064 bytes.
- `type_complexity`, twice, in `src/endpoint/remote.rs`. CI reported them at `:773` and `:786`; the lines may have moved.

Because the Linux job fails, the `spec` job has been skipped on both runs since it was added. TLC has never run in CI.

## Proposed resolution

- **`Receiving`:** box the large variant's payload so the enum stays small. Don't silence the lint: the enum lives in a hot receive path, and the size matters.
- **`type_complexity`:** give the two tuple types a named `type` alias, or a small struct if the fields deserve names.
- Run `cargo clippy --release --all-targets -- -D warnings` locally before pushing.

## Tests

- Clippy passes locally.
- The next CI run reaches the `spec` and `mac` jobs.
