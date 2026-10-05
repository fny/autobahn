# Development

```sh
cargo build --release --locked  # Rust stable, Unix only
cargo test --release --locked   # unit + end-to-end suites (e2e spawns real agents)
cargo clippy --release --locked --all-targets -- -D warnings   # as CI runs it
scripts/mi                      # a guided tour of every command and state
scripts/build-agents.sh         # cross-build the agents bundle
gh workflow run ci.yml          # Linux, ARM Linux and macOS
```

## Build Profiles and Allocators

Shipping binaries use `--profile dist`: release optimization with one codegen unit. Recorded edits were about 10% faster, with builds about 40% slower. Routine development and tests use `--release`.

Static Linux builds target `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`. They use mimalloc and require a musl C compiler.

For x86-64, install `musl-tools`. For aarch64, the build uses zig from `pip install ziglang` through `scripts/zig-cc`. Both the release workflow and `scripts/build-agents.sh` support it.

At 420k files, recorded remote edits took 264 ms with musl’s allocator and 208 ms with mimalloc on x86-64. Graviton results were 190 ms and 130 ms.

`tune_allocator` commits arenas on demand and returns freed memory after 50 ms. This avoids waiting for another allocation on an idle thread.

The tuning halved idle memory on small trees and after large transfers. It had no measurable edit cost through 200,000 files and added about 2 ms to 19 ms at 420,000. `MIMALLOC_*` variables can override it.

## CI

CI runs on pushes to `main` and pull requests. It skips changes limited to Markdown, `docs/`, the licence files, the README artwork, or the release workflow.

The macOS job runs the suite and builds an ad-hoc-signed tray app. Release certificates are unavailable to ordinary CI. New pushes cancel older runs on the same branch.

Every job runs on every push that reaches them; nothing is opt-in. The macOS job waits for Linux and is the longest at about fifteen minutes, but linux, linux-arm and spec are all done around eight whether it runs or not, and runner time is free on a public repository — so skipping it would buy a green tick sooner and nothing else.

The library is also tested under the `app` and the `tray` features, one job each (`features-app`, `features-tray`), since no other job turns either on. They run alongside the rest, on Linux, with the system libraries the app links against.

The app build is a separate workflow on its own path filter, eight minutes across three runners in parallel, finishing inside the time CI takes anyway.

A release needs `linux`, `linux-arm`, `mac`, `spec`, `features-app` and `features-tray` to have passed on the tagged commit. Since they all run on every push to main, tagging a commit that is green is enough; `release.yml` names the recovery when it is not.

## Targeted Tests

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

## Desktop App and Shared Text

The Desktop App is a separate binary behind `--features app`, built with GPUI Kit and the configuration schema. `apps/app/build.sh` and `.github/workflows/app.yml` specify Rust 1.98.0:

```sh
cargo +1.98.0 build --release --locked --features app --bin autobahn-app --target-dir target/app
```

Build the CLI separately and place it beside the desktop app or in a supported installation path.

On macOS, `apps/app/build.sh` creates an ad-hoc-signed bundle. No copy of `autobahn` goes inside it: the window finds the command beside itself first, so a bundled one would override the installed copy the supervisor is actually running. A release replaces the ad-hoc signature with a Developer ID one — see [Releases](./releases.md#signing-and-notarising-macos).

Linux build packages appear in `.github/workflows/app.yml`. Runtime also requires a display server and Vulkan driver. See [Desktop App](./app.md).

Ordinary CI covers the CLI, library, and macOS tray. The Desktop App has a separate workflow, so ordinary CI success does not establish that the Desktop App builds.

Shared strings live in `assets/words/en.toml` and load through `src/words.rs`. Catalog tests check interface usage. `src/surface.rs` contains the shared UI model and configuration editor.

### The Menu Bar App Bundle

`apps/tray/build.sh` builds `Autobahn Tray.app`. It signs with the best identity in your keychain; `--unsigned` stops at the assembled bundle and touches no keychain, and a path argument builds somewhere else, resolved from where you run it. The version the app reports comes from `Cargo.toml`, written into `Info.plist` at build time — so build with `build.sh`, never by copying the template.

`build.sh` puts the binary in `target/tray` (`AUTOBAHN_TRAY_TARGET` moves it), never `target/release`: the login service runs `target/release/autobahn` through a symlink, and an app build must not replace it.

### The Icon

`assets/Autobahn.icon` is an Icon Composer bundle (Icon Composer ships inside Xcode). `build.sh` compiles it with Xcode's `actool`, exactly as Xcode would: the bundle gets `Assets.car` carrying the light, dark, and tinted variants macOS 26 draws, plus `Autobahn.icns` as the flat fallback for older systems. Without Xcode it falls back to the committed `assets/autobahn.icns`.

`scripts/build-icon.sh` regenerates the committed files from the bundle: `assets/autobahn.icns` and `assets/notification.png` — the icon every alert wears, embedded in the binary — both from `actool`, and `assets/autobahn.png`, the artwork at 1024 pixels. That last comes from Icon Composer's `ictool` and is full bleed: the squircle runs edge to edge, which suits a README or a website but is about a quarter larger than an app icon should be. Rerun the script after changing the bundle.

The menu bar glyph is not this icon. It is the Autobahn sign, drawn in code in `src/menubar.rs`; `assets/sign.svg` is the same shape at full size.

### A Guided Tour

`scripts/mi` runs a guided tour against throwaway directories — every command, and every state a session can report, printed as the binary actually produces them.

## The A/B Gate

Measure hot-path changes before release:

```sh
bench/ab.sh <binary-A> <binary-B> --legs 5
```

The harness interleaves two binaries over one synthetic corpus and compares latency percentiles. Interleaving spreads machine-state drift across both builds.

A difference smaller than variation between legs is noise. A difference that reverses across runs is also inconclusive. Use five legs for small effects.

`--remote HOST` tests through SSH with each leg’s own agent binary.

`bench/netem.sh` uses a loopback alias and `tc netem` for round-trip-sensitive changes. A LAN can hide these costs. With 10 ms one-way delay, one extra round trip adds about 20 ms.

The `50k-burst` and `chromium-burst` cells copy a module five times per job and measure each burst through convergence. They measure cycle work rather than isolated-edit latency.

The harness lives in `bench/`, which is not tracked — hundreds of megabytes of corpora and run output. Its own `README.md` documents it; the figures it produced are in [Benchmarks](./benchmarks.md).

## Compatibility Epochs

If a change makes builds disagree on wire or tree semantics, increment `COMPATIBILITY_EPOCH` in `src/protocol.rs`. This includes scan and ignore rules.

After an increment, rebuild the agent bundle before restarting the supervisor. A stale released `MANIFEST` causes refusal before upload. A stale hand-built bundle without one fails the remote handshake. See [State](./state.md#compatibility-epochs).

## The Specification

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

## See Also

- [Architecture](./architecture.md): Scanning, reconciliation, and transfer design
- [Invariants](./correctness/invariants.md): Guarantees and the tests that enforce them
- [Specification](../spec/README.md): Formal models and implementation replay
- [Benchmarks](./benchmarks.md): Recorded performance comparisons
- [Releases](./releases.md): Release builds, signing, and distribution
- [Desktop App](./app.md): Desktop app behavior
- [Menu Bar Item](./tray.md): Tray app behavior and platform limitations
