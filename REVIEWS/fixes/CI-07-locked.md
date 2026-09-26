# CI-07: `--locked` on every cargo command in automation

**Findings:** L-38, the `--locked` part (OPUS).
**Status:** proposed. Not included: declaring a minimum Rust version, or pinning a toolchain file.

## Problem

CI and the release run `cargo build` and `cargo test` without `--locked`. If `Cargo.lock` doesn't match `Cargo.toml`, cargo updates it quietly and builds with whatever it resolved, instead of failing. A release could then ship dependency versions nobody reviewed.

## Proposed resolution

- Add `--locked` to every `cargo build`, `cargo test` and `cargo clippy` in `ci.yml`, `release.yml`, `spec-full.yml`, `apps/macos/build.sh` and `scripts/build-agents.sh`.
- Also add it to the build commands in the docs, which the Linux tray guide already uses.

## Tests

- CI passes.
- A deliberately stale `Cargo.lock` on a branch fails CI with cargo's "lock file needs to be updated" error.
