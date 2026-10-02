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

- **Fast as hell.** Delivers sub-30ms propagation times for small-file updates across trees containing hundreds of thousands of files.
- **Lightweight.** Employs immutable shared-tree structures in memory, requiring significantly less RAM and idle CPU than conventional sync daemons.
- **Safe.** Choose a sync policy per group, backed by tests and bounded formal models. See [Safety](docs/safety.md) for the guarantees and their limits.
- **Reviewed.** Findings from the security reviews are fixed or recorded; the ones that remain are in [accepted risks](docs/correctness/accepted-risks.md), each with its reasoning.
- **Privacy first.** No cloud service, no account, no third party.

## Quick Start

After you [install Autobahn](INSTALL.md) you need to set up your configuration. By default, the configuration is written to `~/.autobahn/config.toml`. You can edit it by hand or use [the app](docs/app.md).

Each group connects one root, the primary, to one or more destinations, the replicas. Sync can be one-way, bidirectional, or P2P (experimental.)

```toml
# ~/.autobahn/config.toml

[defaults]
mode = "two-way-conflict"   # sync modes explained below
ignores = [".git", "node_modules"]

[groups.project]
primary = "~/project"
replicas = [                   # sync targets
  "user@audi.de:/srv/car",  #  - fully specified
  "mercedes-benz.de",       #  - inherits primary path
  "/mnt/backup/project",    #  - local paths work too
]
ignores = ["target"]        # appended to the defaults' ignores
```

```sh
autobahn watch              # monitor every session as a one off
autobahn install            # or install as a login service
```

## Sync Modes

Autobahn has several sync modes with different resolution strsategies.

Start with `two-way-conflict` for editing on both sides. It propagates changes in either direction and reports competing edits for you to resolve.

| Mode | Behavior |
| --- | --- |
| `two-way-conflict` | Bidirectional sync with conflict reports |
| `two-way-primary` | Favors the primary in conflicts, with deletion safeguards |
| `two-way-primary-strict` | Favors the primary, including its deletions |
| `one-way-conflict` | Reports changes on the replica that prevent a safe copy |
| `one-way-primary` (alias: `mirror`) | Makes the replica match the primary |
| `p2p-*-dangerously-experimental` | Lets a replica lead while the primary is offline |

For mode details, see [Modes](docs/modes.md) and [Conflict Resolution](docs/conflicts.md).

Read [P2P](docs/p2p.md) before P2P use.


## Benchmarks

Autobahn began as an effort to reduce the memory use of [Mutagen](https://mutagen.io/) which offers similar sync features, and then I got carried away.

| Measurement | Autobahn | mutagen | Ratio |
|---|---:|---:|---:|
| Small-file edit, Chromium, 1 editor, p50 | **23.8 ms** | 6,232.2 ms | 261.9× |
| Small-file edit, 50k subset, 10 editors, p50 | **13.4 ms** | 1,809.8 ms | 135.1× |
| Peak controller memory, Chromium, 1 editor | **479 MiB** | 2,081 MiB | 4.3× |
| Idle controller CPU, Chromium | **0.1% of a core** | 49.9% | rounded values |
| First sync, Chromium | **228.1 s** | 454.8 s | 2.0× |

See [Benchmarks](docs/benchmarks.md) for details.

## Safety

### Empirically

I have been running this on my own fleet every day: 20 sessions across five hosts. One of them is a 13 GB, 215,000-file tree. Autobahn has also undergone a battery of tests and benchmarks including a 24-hour soak test.

### Formally

Autobahn prevents data loss through strict operational invariants:

- **Three-Way Reconciliation:** Tracks a shared ancestor to accurately distinguish deletions, modifications, and concurrent edits.
- **Atomic File Transitions:** All file writes stage content to temporary paths and swap into place using atomic filesystem operations.
- **Fail-Closed Guarantees:** Any ambiguous state, communication failure, or unexpected filesystem mutation results in a pause rather than accidental overwrites.

See [Safety](docs/safety.md) for guarantees and the related invariants in [Correctness](docs/correctness/).


## UI Goodness

In addition to the standard CLI, several user interfaces are available:
- **[Autobahn Dash](docs/app.md):** Experimental graphical management application.
- **[Terminal UI](docs/shop.md):** Interactive curses-based console monitor (`autobahn shop`).
- **[Menu Bar App](docs/macos-app.md):** Lightweight status monitor for macOS.
- **[Alert Hooks](docs/alerts.md):** Event notification script support (`on_alert`).

```toml
on_alert = "~/.autobahn/on-alert.sh"   # written for you by `autobahn init`
```

None of this has undergone nearly the same level of testing as `autobahn` itself, so consider them experimental.

## AI Disclaimer

This project was heavily vibe coded, and with great vibe coding comes great responsibility. So I have: scanned every line in this repo, run extensive soak testing, used Autobahn myself for weeks, had guardrail-free models run security scans, and put it through thousands of benchmark runs. Most of the internal documentation was first drafted by LLMs. Please forgive the lingering Claudeisms.

## Contributing

- I won't accept PRs. I prefer my slop over your slop, so instead file an issue for a bug report or (small) feature request.
- Bug reports should come with detailed context from a human or LLM.
- Feature requests should be small with high impact.
- Have a greater request? Go fork yourself. ;D

## Documentation

### Operations & Configuration

- [Installation Guide](INSTALL.md)
- [Configuration Reference](docs/configuration.md)
- [Command-Line Reference](docs/commands.md)
- [Sync Modes](docs/modes.md)
- [Ignore Rules](docs/ignores.md)
- [Conflict Resolution](docs/conflicts.md)
- [Git Repository Best Practices](docs/git.md)
- [Logging & Diagnostics](docs/logging.md)
- [State Directory Layout](docs/state.md)

### Architecture & Design

- [System Architecture & Internals](docs/how-it-works.md)
- [Safety Guarantees](docs/safety.md)
- [Limitations](docs/limitations.md)
- [Failover P2P (Experimental)](docs/p2p.md)

### Verification & Performance
- [Benchmark Results](docs/benchmarks.md)
- [Full Benchmark Matrix](docs/benchmark-matrix.md)
- [Invariants](docs/correctness/invariants.md)
- [Accepted risks](docs/correctness/accepted-risks.md)
- [Development Guide](docs/development.md)
- [Release Process](docs/releases.md)


## System Requirements & Limitations

- **Supported Platforms:** Linux (`x86_64`, `aarch64`) and macOS (`Apple Silicon`).
- **Filesystems:** Requires local POSIX filesystems. Network filesystems (NFS, SMB, CIFS) receive best-effort support only.
- **Editor Saves on macOS:** On macOS, atomic save operations (write-temporary and rename) lack immediate completion events from the kernel, occasionally requiring an extra polling cycle compared to Linux. Details are available in [How Autobahn Works](docs/how-it-works.md).
