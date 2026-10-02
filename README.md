# Autobahn <picture><source media="(prefers-color-scheme: dark)" srcset="assets/sign-white.svg"><img src="assets/sign.svg" alt="" height="32"></picture>

*Subsecond sync with German precision.*

Keep your files in sync as fast as you or an agent edits them across a fleet of machines.

```sh
curl -fsSL https://github.com/fny/autobahn/releases/latest/download/install.sh | sh
```

Point your coding agent at [INSTALL.md](INSTALL.md) for interactive setup, or see [Autobahn Dash](docs/app.md) for the experimental desktop app.

## The Problem

- Browsing files over SSH or NFS is clunky.
- Agents that run `--dangerously` should do it in a VM elsewhere, but you can't use your local tools.
- Some sync tools require gigs of RAM for big trees, or a cloud account, or both.

## Why Autobahn

- **Fast as hell.** The September 2026 Linux benchmark measured 13.4 ms median for a small-file edit on the 50k subset with ten editors, and 23.8 ms on a half-million-file Chromium checkout with one editor.
- **Lightweight.** That benchmark measured 479 MiB of peak controller memory on the Chromium single-editor workload, versus 2,081 MiB for mutagen, and 0.1% of a core while idle.
- **Safe.** Choose a sync policy per group, backed by tests and bounded formal models. See [Safety](docs/safety.md) for the guarantees and their limits.
- **Reviewed.** Security findings and fixes are recorded in [REVIEWS](REVIEWS/README.md); [known residual risks](docs/correctness/RETAINED.md) remain.
- **Privacy first.** No cloud service, no account, no third party.

## Start here

After you [install Autobahn](INSTALL.md) you need to set up your configuration. By default, the configuration is written to `~/.autobahn/config.toml`. You can edit it by hand or use [the app](docs/app.md).

```toml
# ~/.autobahn/config.toml

[defaults]                  # inherited by every group; settings can be overridden
mode = "two-way-conflict"   # sync modes explained below
ignores = [".git", "node_modules"]

[groups.project]
alpha = "~/project"         # the source root you edit
betas = [                   # sync targets
  "user@audi.de:/srv/car",  #  - fully specified
  "mercedes-benz.de",       #  - inherits the alpha path
  "/mnt/backup/project",    # local paths work too
]
ignores = ["target"]        # appended to the defaults' ignores
```

```sh
autobahn watch              # monitor every session as a one off
autobahn install            # or install as a login service
```

## Sync Modes

Autobahn has five core modes. Start with `two-way-conflict`: it propagates changes in either direction and reports competing edits for you to resolve.

| Mode | Policy | Best For... |
| --- | --- | --- |
| `two-way-conflict` | Report conflicts | Editing on both sides without risking data loss. |
| `two-way-alpha` | Alpha wins | Active editing on both sides, but Alpha is the primary authority. |
| `two-way-alpha-strict` | Alpha wins (strict) | Alpha is authoritative, and Alpha's deletions must override Beta's edits. |
| `one-way-conflict` | Beta changes pause | Deployments where Beta generates local files (logs, caches) that Alpha must not touch. |
| `one-way-alpha`<br>*(alias: `mirror`)* | Alpha mirrors strictly | Backups and releases where Beta must be an exact, identical replica of Alpha. |
| `peering-*-dangerously-experimental` | Conflict or Alpha | Multi-node failover when Alpha goes offline. *(See [Peering](docs/peering.md))* |

See [Modes](./docs/modes.md) for a complete behavior matrix and [Conflicts](./docs/conflicts.md) for resolution strategies.

## Benchmarks

Autobahn was inspired by [Mutagen](https://mutagen.io/). I originally set out to optimize RAM use but quickly got carried away squeezing every possible second out of a sync.

Latest recorded Linux results, refreshed through October 1, 2026: **Autobahn 0.4.0 versus mutagen 0.19.0-dev**. The [matrix](docs/benchmark-matrix.md) identifies the measured build for each cell; these are not measurements of every subsequent commit.

| Measurement | Autobahn | mutagen | Ratio |
|---|---:|---:|---:|
| Small-file edit, Chromium, 1 editor, p50 | **23.8 ms** | 6,232.2 ms | 261.9× |
| Small-file edit, 50k subset, 10 editors, p50 | **13.4 ms** | 1,809.8 ms | 135.1× |
| Peak controller memory, Chromium, 1 editor | **479 MiB** | 2,081 MiB | 4.3× |
| Idle controller CPU, Chromium | **0.1% of a core** | 49.9% | rounded values |
| First sync, Chromium | **228.1 s** | 454.8 s | 2.0× |

See [Benchmarks](docs/benchmarks.md), and [Why Mutagen is Slower](docs/mutagen.md) for details.

## Safety

I have been running this on my own fleet every day: 20 sessions across five hosts. One of them is a 13 GB, 215,000-file tree.

Autobahn has also undergone a battery of tests and benchmarks including a 24-hour soak test and formal verification where suitable.

See [Safety](docs/safety.md) for guarantees and the related invariants in [Correctness](docs/correctness/).


## User interfaces (experimental)

Autobahn comes with an [App](docs/app.md), a [TUI](docs/shop.md), and a standalone [Tray](docs/macos-app.md). For alerts, you can even set one line in your config to run a script every time an alert fires:

```toml
on_alert = "~/.autobahn/on-alert.sh"   # written for you by `autobahn init`
```

None of this has undergone nearly the same level of testing as `autobahn` itself, so consider them experimental features.

**Peering** (dangerously experimental) — a beta takes the lead when the alpha is away, and gives it back. It has known security issues that are not fixed in this release; any peer that can lead is trusted with every other peer. Read [Peering](docs/peering.md) before enabling it.

## AI Disclaimer

This project was heavily vibe coded, and with great vibe coding comes great responsibility. So I have: scanned every line in this repo, run extensive soak testing, used Autobahn myself for weeks, had guardrail-free models run security scans, and put it through thousands of benchmark runs. Most of the internal documentation was first drafted by LLMs. Please forgive the lingering Claudeisms.

## Contributing

- I won't accept PRs. I prefer my slop over your slop, so instead file an issue for a bug report or (small) feature request.
- Bug reports should come with detailed context from a human or LLM.
- Feature requests should be small with high impact.
- Have a greater vision? Go fork yourself: Autobahn is considered near complete. If there's something crazy you really want, set up your own repo.


## Documentation

**Using it** — [Installation](INSTALL.md) · [Desktop app](docs/app.md) · [Git checkouts](docs/git.md) · [Configuration](docs/configuration.md) · [Modes](docs/modes.md) · [Ignores](docs/ignores.md) · [Alerts](docs/alerts.md) · [Commands](docs/commands.md) · [Conflicts](docs/conflicts.md) · [TUI](docs/shop.md) · [Menu bar app](docs/macos-app.md) · [Logging](docs/logging.md) · [State](docs/state.md)

**Understanding it** — [Safety](docs/safety.md) · [How it works](docs/how-it-works.md) · [Overlapping and nested roots](docs/nesting.md) · [Scope and support boundaries](docs/support-boundaries.md)

**Measuring it** — [Benchmarks](docs/benchmarks.md) · [The benchmark matrix](docs/benchmark-matrix.md) · [Why mutagen is slower](docs/mutagen.md)

**Working on it** — [Review archive](REVIEWS/README.md) · [Development](docs/development.md) · [Releases](docs/releases.md) · [Correctness](docs/correctness/)

## Limitations

Unix only: Linux (x86-64 and arm64) and macOS (Apple Silicon), with macOS a first-class target rather than a build target. Transport is SSH. Roots must live on local filesystems: network mounts are best-effort. The full list of what is and is not covered is in [Scope and support boundaries](docs/support-boundaries.md).

**One difference on macOS.** Some programs save a file by writing a temporary copy and renaming it over the original; vim and JetBrains IDEs do. On Linux, Autobahn waits for the program to finish writing before it syncs the file, so it copies only the finished file. macOS does not say when a program has finished writing a file, so there a save like this of a large file can cost an extra round of work before it syncs: a fraction of a second, not an error. Details in [How it works](docs/how-it-works.md#the-settle-as-an-illustration).
