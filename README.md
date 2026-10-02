# Autobahn <picture><source media="(prefers-color-scheme: dark)" srcset="assets/sign-white.svg"><img src="assets/sign.svg" alt="" height="32"></picture>

*Subsecond sync with German precision.*

Keep your files in sync as fast as you or an agent edits them across a fleet of machines.

```sh
curl -fsSL https://github.com/fny/autobahn/releases/latest/download/install.sh | sh
```

You can also point your agent at [INSTALL.md](INSTALL.md) for interactive installation or use the GUI:

<<put image of installation view in autobahn next to example view of groups>>

## The Problem

- Browsing and files over SSH or NFS is clunky.
- Agents that run `--dangerously` should do it in a VM elsewhere, but you can't use your local tools.
- Sync tools handle big trees require gigs of RAM, or a cloud account, or both.

## Why Autobahn

- **Fast as hell.** Changes propage in about 50 ms on a 40,000-file tree, and under 200 ms on a half-million-file Chromium checkout.
- **Lightweight.** 33 MB of memory for 40,000 files, 249 MB for Chromium. 0.2% of a core while idle.
- **Safe.** Different sync modes to pick the risk you want per group, all verified empirically and by proof.
- **Secure.** Audited to death by Kimi 3 and GLM 5.3.
- **Privacy first.** No cloud service, no account, no third party.

## Start here

After you [install Autobahn](INSTALL.md) you need to set up your configuration. By default, the configuration is written to `~/.autobahn/config.toml`. You can edit it by hand or use [the app](docs/app.md).

```toml
# ~/.autobahn/config.toml

[defaults]                  # inherited by every group; any key can be
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

Joy has several modes depending on how hands off you want to be. I reccomend `two-way-conflict` which resolves changes on its own unless two files don't match up.

| Mode | Policy | Best For... |
| --- | --- | --- |
| `two-way-conflict` | Report conflicts | Editing on both sides without risking data loss. |
| `two-way-alpha` | Alpha wins | Active editing on both sides, but Alpha is the primary authority. |
| `two-way-alpha-strict` | Alpha wins (strict) | Alpha is authoritative, and Alpha's deletions must override Beta's edits. |
| `one-way-conflict` | Beta changes pause | Deployments where Beta generates local files (logs, caches) that Alpha must not touch. |
| `one-way-alpha`<br>*(alias: `mirror`)* | Alpha mirrors strictly | Backups and releases where Beta must be an exact, identical replica of Alpha. |
| `peering-*-dangerously-experimental` | Conflict or Alpha | Multi-node failover when Alpha goes offline. *(See [Peering](./peering.md))* |

See [Modes](./docs/modes.md) for a complete behavior matrix and [Conflicts](./docs/conflicts.md) for resolution strategies.

## Benchmarks

Autobahn was inspired by [Mutagen](https://mutagen.io/). I originally set out to optimize RAM use but quickly got carried away squeezing every possible second out of a sync.

<<TODO MAKE SURE THIS IS UP TO DATE>>
| | autobahn | mutagen | |
|---|---|---|---|
| Propagate one edit, Chromium (505k files) | **188 ms** | 7,118 ms | 37.8× |
| Propagate one edit, 40k files, 10 agents | **52 ms** | 1,854 ms | 35.4× |
| Peak memory, Chromium | **249 MB** | 2,033 MB | 8.2× |
| CPU while idle, Chromium | **0.2%** | 50% | 250× |
| First sync, Chromium | 423 s | 418 s | ~1% |

See [Benchmarks](docs/benchmarks.md), and [Why Mutagen is Slower](docs/mutagen.md) for details.

## Safety

I have been running this on my on fleet every day: 20 sessions across five hosts. One of them is a 13 GB, 215,000-file tree.

Autobahn has also undergone a battery of tests and benchmarks including a 24-hour soak test and formal verification where suitable.

See [Safety](docs/safety.md) for gurantees and the related invariants in [Correctness](docs/correctness/).


## Quality of Life (Beta)

Autobahn comes with an [App](docs/app.md), an [TUI](docs/shop.md), and a standalone [Tray](TODO). For alerts, you can even set one line in your config to run a script every time an alert fires:

```toml
on_alert = "~/.autobahn/on-alert.sh"   # written for you by `autobahn init`
```

None of this has undergone nearly the same level of testing as `autobahn` itself, so consider them beta features.

## AI Disclaimer

This project was heavily vibe coded, and with great vibe coding comes great responsibility. So I have: scanned every line in this repo, run extensive soak testing, used Autobahn myself for weeks, had guardrail-free models run security scans, and put it through thousands of benchmark runs. Most of the internal documentation was first drafted by LLMs. Please forgive the lingering Claudeisms.

## Contributing

- I won't accept PRs. I prefer my slop of your slop, so instead file an issue for a bug report or (small) feature request.
- Bug reports should come with detailed context from a human or LLM.
- Feature requests should be small with high impact.
- Have a greater vision? Go fork yourself: Autobahn is considered near complete. If there's something crazy you really want, set up your own repo.


## Documentation

**Using it** — [Configuration](docs/configuration.md) · [Modes](docs/modes.md) · [Ignores](docs/ignores.md) · [Alerts](docs/alerts.md) · [Commands](docs/commands.md) · [Conflicts](docs/conflicts.md) · [TUI](docs/shop.md) · [Menu bar app](docs/macos-app.md) · [Logging](docs/logging.md) · [State](docs/state.md)

**Understanding it** — [Safety](docs/safety.md) · [How it works](docs/how-it-works.md) · [Overlapping and nested roots](docs/nesting.md) · [Scope and support boundaries](docs/support-boundaries.md)

**Measuring it** — [Benchmarks](docs/benchmarks.md) · [The benchmark matrix](docs/benchmark-matrix.md) · [Why mutagen is slower](docs/mutagen.md)

**Working on it** — [Development](docs/development.md) · [Releases](docs/releases.md) · [Correctness](docs/correctness/)

## Limitations

Unix only: Linux (x86-64 and arm64) and macOS (Apple Silicon), with macOS a first-class target rather than a build target. Transport is SSH. Roots must live on local filesystems: network mounts are best-effort. The full list of what is and is not covered is in [Scope and support boundaries](docs/support-boundaries.md).
