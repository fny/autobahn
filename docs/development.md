# Development

```sh
cargo build --release         # Rust stable, Unix only
cargo test --release          # unit + end-to-end suites (e2e spawns real agents)
cargo clippy --all-targets
scripts/mi                    # a guided tour of every command and state
scripts/build-agents.sh       # cross-build the agents bundle
gh workflow run ci.yml        # Linux, ARM Linux, macOS and FreeBSD
```

CI runs on every push to `main` and every pull request, except changes
that cannot affect a build — Markdown, `docs/`, `bench/`, the README
artwork, and the release workflow. There is one macOS job, which runs the
suite and then builds the app, signed ad-hoc; the certificate belongs to
the release alone. A newer push cancels an older run of the same branch.

macOS runners are the slow and scarce ones, so the macOS job waits for the
Linux job and runs only if it passed — a change that fails there is broken
anyway. For a change that cannot touch macOS, put `[skip mac]` in the
head commit's message, or in a pull request's title, and the macOS job is
skipped while everything else runs. Only the head commit of a push is
read, so the marker has to be on the last commit you push.

## Targeted tests

The full suite takes minutes. Run the part that covers the change:

```sh
cargo test --release --lib -- scan::        # one module
cargo test --release --test supervisor      # the supervisor integration suite
cargo test --release --test e2e             # real agents over stdio
```

Reconciliation and scanning are shared by everything, so a change there
runs the supervisor and e2e suites too.

The integration suites run with a private `HOME`
(`tests/common::isolate_home`). Endpoint locks and agent-side staging live
under `$HOME/.autobahn` by design, so that two processes disagreeing about
their state root still meet at the lock — which meant that, before this,
every test run left staging directories and locks in your real state root.

Build the tray feature into its own target directory:

```sh
CARGO_TARGET_DIR=target/tray cargo build --release --features tray
```

That is what `apps/macos/build.sh` does. The login service runs
`target/release/autobahn`, and a feature build there replaces it.

## The A/B gate

Every hot-path change is measured before it ships, because analysis
estimates of these costs have been wrong every time they were tried:

```sh
bench/ab.sh <binary-A> <binary-B> --legs 5
```

It runs two binaries in interleaved legs over one synthetic corpus and
reports latency percentiles side by side. Interleaving is what makes the
comparison honest on a shared machine: any drift in the machine's state
lands on both. A difference smaller than the leg-to-leg spread is noise;
a difference that reverses sign between runs is certainly noise. Three
legs cannot tell a small effect from chance — use five.

`bench/README.md` covers the rest of the harness, and
[Benchmarks](./benchmarks.md) the published comparison against mutagen.

## Compatibility epochs

A change that breaks the wire protocol, or that makes two versions
disagree about a tree — a scan rule, an ignore rule — must bump
`COMPATIBILITY_EPOCH` in `src/protocol.rs`. See
[State](./state.md#compatibility-epochs) for how it is enforced. After a
bump, the agents bundle must be rebuilt before the supervisor is
restarted, or the stale bundle is uploaded under the new name and every
session fails its handshake.

## Correctness

The correctness work — the invariants the design claims, the code that
enforces each one, the tests that check it, and the residuals
deliberately left open — is written down in
[`correctness/`](./correctness/). [`INVARIANTS.md`](./correctness/INVARIANTS.md)
is the entry point. Adversarial reviews of specific subsystems are in
[`reviews/`](./reviews/).

## Lineage

Autobahn is a from-scratch Rust distillation of the architecture that
emerged from a deep memory/performance overhaul of [Mutagen]'s
synchronization engine: enum-based trees with name-sorted, copy-on-write
shared children; scan metadata resident on the nodes themselves; linear-
merge reconciliation; streaming transfers end to end. What required
convention and adversarial review to keep safe in Go, the borrow checker
and `Arc::make_mut` enforce structurally here. [Why mutagen is slower](./mutagen.md)
sets out where the difference comes from in mutagen's code.

[Mutagen]: https://github.com/mutagen-io/mutagen

## See also

- [How autobahn works](./how-it-works.md) — the design and its reasoning
- [Safety](./safety.md) — what the tests pin
- [State](./state.md) — agents, epochs, and what lives in `~/.autobahn`
