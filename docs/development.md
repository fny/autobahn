# Development

```sh
cargo build --release --locked  # Rust stable, Unix only
cargo test --release --locked   # unit + end-to-end suites (e2e spawns real agents)
cargo clippy --release --locked --all-targets -- -D warnings   # as CI runs it
scripts/mi                      # a guided tour of every command and state
scripts/build-agents.sh         # cross-build the agents bundle
gh workflow run ci.yml          # Linux, ARM Linux and macOS
```

## Build profiles and allocators

Shipping binaries use `--profile dist`: release optimization with one codegen unit. Recorded edits were about 10% faster, with builds about 40% slower. Routine development and tests use `--release`.

Static Linux builds target `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`. They use mimalloc and require a musl C compiler.

For x86-64, install `musl-tools`. For aarch64, the build uses zig from `pip install ziglang` through `scripts/zig-cc`. Both the release workflow and `scripts/build-agents.sh` support it.

At 420k files, recorded remote edits took 264 ms with musl’s allocator and 208 ms with mimalloc on x86-64. Graviton results were 190 ms and 130 ms.

`tune_allocator` commits arenas on demand and returns freed memory after 50 ms. This avoids waiting for another allocation on an idle thread.

The tuning halved idle memory on small trees and after large transfers. It had no measurable edit cost through 200,000 files and added about 2 ms to 19 ms at 420,000. `MIMALLOC_*` variables can override it.

## CI

CI runs on pushes to `main` and pull requests. It skips changes limited to Markdown, `docs/`, `bench/`, README artwork, or the release workflow.

The macOS job runs the suite and builds an ad-hoc-signed tray app. Release certificates are unavailable to ordinary CI. New pushes cancel older runs on the same branch.

The macOS job waits for Linux to pass. For a change that cannot affect macOS, `[skip mac]` in the head commit message or PR title skips that job. Only the final commit of a push is checked.

## Targeted tests

Run the suite that covers the change:

```sh
cargo test --release --lib -- scan::        # one module
cargo test --release --test supervisor      # the supervisor integration suite
cargo test --release --test e2e             # real agents over stdio
```

Changes to reconciliation or scanning also require supervisor and e2e suites.

Integration tests use a private `HOME` through `tests/common::isolate_home`. Endpoint locks and agent staging live under `$HOME/.autobahn`, even with explicit state-root overrides. Isolation keeps test artifacts out of real user state.

Build the tray separately:

```sh
CARGO_TARGET_DIR=target/tray cargo build --release --locked --features tray
```

`apps/tray/build.sh` uses this directory. A build in `target/release` can replace the executable used by the login service.

## Desktop app and shared text

Dash is a separate binary behind `--features dash`, built with GPUI Kit and the configuration schema. `apps/dash/build.sh` and `.github/workflows/dash.yml` specify Rust 1.98.0:

```sh
cargo +1.98.0 build --release --locked --features dash --bin autobahn-dash --target-dir target/dash
```

Build the CLI separately and place it beside Dash or in a supported installation path.

On macOS, `apps/dash/build.sh` creates an ad-hoc-signed bundle. It includes `target/release/autobahn` if present.

Linux build packages appear in `.github/workflows/dash.yml`. Runtime also requires a display server and Vulkan driver. See [Dash](./app.md).

Ordinary CI covers the CLI, library, and macOS tray. Dash has a separate workflow, so ordinary CI success does not establish that Dash builds.

Shared strings live in `assets/words/en.toml` and load through `src/words.rs`. Catalog tests check interface usage. `src/surface.rs` contains the shared UI model and configuration editor.

## The A/B gate

Measure hot-path changes before release:

```sh
bench/ab.sh <binary-A> <binary-B> --legs 5
```

The harness interleaves two binaries over one synthetic corpus and compares latency percentiles. Interleaving spreads machine-state drift across both builds.

A difference smaller than variation between legs is noise. A difference that reverses across runs is also inconclusive. Use five legs for small effects.

`--remote HOST` tests through SSH with each leg’s own agent binary.

`bench/netem.sh` uses a loopback alias and `tc netem` for round-trip-sensitive changes. A LAN can hide these costs. With 10 ms one-way delay, one extra round trip adds about 20 ms.

The `50k-burst` and `chromium-burst` cells copy a module five times per job and measure each burst through convergence. They measure cycle work rather than isolated-edit latency.

See [Benchmark harness](../bench/README.md) and [Benchmarks](./benchmarks.md).

## Compatibility epochs

If a change makes builds disagree on wire or tree semantics, increment `COMPATIBILITY_EPOCH` in `src/protocol.rs`. This includes scan and ignore rules.

After an increment, rebuild the agent bundle before restarting the supervisor. A stale released `MANIFEST` causes refusal before upload. A stale hand-built bundle without one fails the remote handshake. See [State](./state.md#compatibility-epochs).

## The specification

TLA+ models in `spec/` cover reconciliation across one primary and multiple replicas, plus p2p. TLC explores bounded configurations.

CI’s `spec` job runs `spec/check.sh quick` with the version pinned in `spec/tla2tools.version`. It then runs `AUTOBAHN_TLC=1 cargo test --release --locked --test spec_replay --test spec_p2p_replay -- --include-ignored`.

Replay tests drive real `reconcile()` and lease code through TLC traces and compare results with `Match`. Without TLC, these tests remain `#[ignore]`d.

`spec/check.sh` without arguments includes liveness and three-replica models. The manual `spec-full.yml` workflow does the same. P2P liveness can take hours and requires a large machine.

See [Specification guide](../spec/README.md) for modes, bounds, and modeling lessons.

## Correctness

[invariants.md](./correctness/invariants.md) records guarantees, implementation points, and tests. [accepted-risks.md](./correctness/accepted-risks.md) records unresolved risks and their rationale.

## Lineage

Autobahn is a Rust implementation derived from architectural work on [Mutagen](https://github.com/mutagen-io/mutagen) memory use and performance.

It uses enum-based trees, sorted copy-on-write children, metadata on nodes, linear-merge reconciliation, and streaming transfers. Rust ownership and `Arc::make_mut` enforce parts of the sharing discipline.

