# Autobahn

*Subsecond file sync with German precision.*

**Your files stay on your machine. Your work runs anywhere else.** Autobahn keeps a folder identical across every machine you use — laptop, server, GPU box — in about 50 ms.

```sh
curl -fsSL https://github.com/fny/autobahn/releases/latest/download/install.sh | sh
```

Then [three steps to your first sync](#start-here). Or read [how it works](docs/how-it-works.md) first.

## The problem

- Editing over SSH, `sshfs` or NFS puts the network in the middle of every keystroke and every save.
- Agents that run `--dangerously` should do it on a machine that is not yours — but the files have to be on yours.
- When an agent goes wrong, you want a local `git checkout`, not an SSH session.
- Syncing tools that do handle big trees ask for a daemon that eats gigabytes of RAM, or a cloud account, or both.

## Why Autobahn

- **Fast.** One changed file lands on the other side in about 50 ms on a 40,000-file tree, and under 200 ms on a half-million-file Chromium checkout.
- **Light.** 33 MB of memory for 40,000 files, 249 MB for Chromium. 0.2% of a core while idle.
- **Safe.** Three-way reconciliation against a remembered baseline, so "you deleted this" never gets confused with "this was never here". Pick the risk you want per group.
- **Yours.** No cloud service, no account, no third party. SSH to machines you already have, and nothing to install on them — Autobahn sends its own agent, and upgrades the same way.

## Start here

**1. Install it.**

```sh
curl -fsSL https://github.com/fny/autobahn/releases/latest/download/install.sh | sh
```

**2. Say what stays in sync.** Each **group** fans one source root (the *alpha*) out to any number of destinations (the *betas*).

```toml
# ~/.autobahn/config.toml

[defaults]                  # inherited by every group; any key can be
mode = "two-way-conflict"   # overridden per group
ignores = [".git", "node_modules"]

[groups.project]
alpha = "~/project"         # the source root you edit
betas = [
  "build.example.com",              # inherits the alpha path
  "user@lab.example.com:/srv/project",
  "/mnt/backup/project",            # local paths work too
]
ignores = ["target"]        # appended to the defaults' ignores
```

**3. Run it.**

```sh
autobahn watch              # every session, here, until Ctrl-C
autobahn install            # ...or as a login service that survives logout
```

That is the whole setup. Both sides watch their own filesystem natively (FSEvents, inotify), so an edit on either end is on the other in a fraction of a second.

## It is faster, by a lot

| | autobahn | mutagen | |
|---|---|---|---|
| Propagate one edit, Chromium (505k files) | **188 ms** | 7,118 ms | 37.8× |
| Propagate one edit, 40k files, 10 agents | **52 ms** | 1,854 ms | 35.4× |
| Peak memory, Chromium | **249 MB** | 2,033 MB | 8.2× |
| CPU while idle, Chromium | **0.2%** | 50% | 250× |
| First sync, Chromium | 423 s | 418 s | ~1% |

Autobahn wins fourteen of fifteen cells. The tie is the first sync, which is bound by the disk, not by either tool. Measured at 0.3.0; what has moved since is in [Benchmarks](docs/benchmarks.md), and [Why mutagen is slower](docs/mutagen.md) traces each gap to the code that causes it.

## Why you can trust it with your files

Autobahn runs the author's own fleet every day: 20 sessions across five hosts, one of them a 215,000-file tree that has moved 13 GB without losing a byte.

Under that is a design that refuses rather than guesses. A root that vanishes halts the session instead of propagating the deletion. Content is staged, verified against its digest, and only then renamed into place. Both sides speak a version-checked protocol and refuse to talk across a mismatch. Where a guarantee stops, [Safety](docs/safety.md) says so plainly, and the invariants that hold it up are written down in [Correctness](docs/correctness/).

**[Install it](#start-here)** and point it at one folder you care about.

## Told when it needs you

One line in the config, and the hold times, coalescing and repeat rules are built in:

```toml
on_alert = "~/.autobahn/on-alert.sh"   # written for you by `autobahn init`
```

There is also `autobahn mi`, a terminal view of every session, and an experimental [menu bar app](docs/macos-app.md) for macOS.

## Experimental

Real, shipped, and still moving. Expect the wording, the keys and the shape to change between releases.

- **Peering** (dangerously experimental) — a beta takes the lead when the alpha is away, and gives it back. It has known security and collision issues that are not fixed in this release; any peer that can lead is trusted with every other peer. Read [Peering](docs/peering.md) before enabling it.
- **The menu bar app** and the **alert hook example** — see their pages.

## AI disclaimer

This project was heavily vibe coded, and with great vibe coding comes great responsibility. So I have: scanned every line in this repo, run extensive soak testing, used Autobahn myself for weeks, had guardrail-free models run security scans, and put it through thousands of benchmark runs. Most of the internal documentation was first drafted by LLMs — forgive the lingering Claudeisms.

## Installing it other ways

```sh
curl -fsSL https://github.com/fny/autobahn/releases/latest/download/install.sh | sh
```

The installer puts `autobahn` on your `PATH` and copies the agent bundle into `~/.autobahn/agents`, which is what bootstraps hosts on other platforms. `--bin-dir` moves the command (default `~/.local/bin`), `--no-agents` skips the bundle (usually a mistake), `--version` picks a release. Every download is checked against the release's `SHA256SUMS`, and the installer refuses when it cannot be; `--insecure` is only for an old release that publishes none. `AUTOBAHN_BIN_DIR` and `AUTOBAHN_HOME` set the same two from the environment, for a non-interactive install. Later, `autobahn update` does it all again, with `--dry-run` and `--version TAG`.

By hand: grab a binary from [Releases](https://github.com/fny/autobahn/releases).

```sh
install -m 755 autobahn-linux-x86_64 ~/.local/bin/autobahn
mkdir -p ~/.autobahn && tar xzf autobahn-agents.tar.gz -C ~/.autobahn   # if your fleet spans platforms
```

Or build from source with `cargo build --release --locked` (Rust stable, Unix only).

## Documentation

**Using it** — [Configuration](docs/configuration.md) · [Modes](docs/modes.md) · [Ignores](docs/ignores.md) · [Alerts](docs/alerts.md) · [Commands](docs/commands.md) · [Conflicts](docs/conflicts.md) · [TUI](docs/shop.md) · [Menu bar app](docs/macos-app.md) · [Logging](docs/logging.md) · [State](docs/state.md)

**Understanding it** — [Safety](docs/safety.md) · [How it works](docs/how-it-works.md) · [Overlapping and nested roots](docs/nesting.md) · [Scope and support boundaries](docs/support-boundaries.md)

**Measuring it** — [Benchmarks](docs/benchmarks.md) · [The benchmark matrix](docs/benchmark-matrix.md) · [Why mutagen is slower](docs/mutagen.md)

**Working on it** — [Development](docs/development.md) · [Releases](docs/releases.md) · [Correctness](docs/correctness/)

## Scope

Unix only: Linux (x86-64 and arm64) and macOS (Apple Silicon), with macOS a first-class target, not a build target. Transport is SSH. Roots must live on local filesystems; network mounts are best-effort. The full list of what is and is not covered is in [Scope and support boundaries](docs/support-boundaries.md).

<!-- ─────────────────────────────────────────────────────────────────────
     The previous README follows, kept for merging. Delete it, and this
     marker, once everything worth keeping has moved up.
     ───────────────────────────────────────────────────────────────── -->

# Autobahn <picture><source media="(prefers-color-scheme: dark)" srcset="assets/sign-white.svg"><img src="assets/sign.svg" alt="" height="32"></picture>

*Subsecond file sync with German precision.*

Sold? Jump to [Getting Started](#getting-started).

## Why Autobahn?

- **Fast** A changed file lands on the other side in about 50 ms on a 40,000-file tree, and in under 200 ms on a half-million-file Chromium checkout. Not too shabby.
- **Light** 33 MB for a 40,000-file tree and 249 MB for Chromium. Idle it uses 0.2% of a core.
- **Zero remote setup.** Autobahn streams its own sync agent over SSH on first contact, and upgrades roll out the same way.
- **Safe.** Autobahn performs three-way reconciliation against a remembered baseline. "You deleted this file" never conflicts with "this file never existed here." You can even calibrate your risk tolerance with different sync modes. [How errors are prevented](docs/safety.md).
- **User friendly.** A menu bar app on macOS shows every session at a glance and settles conflicts from a menu. Experimental (i.e. unstable API) but functional. On Linux the same tray is an unverified [build-it-yourself experiment](docs/macos-app.md#on-linux-experimental-unverified).

 - Editing files on a remote machine over SSH or NFS is slow and clunky.
 - You can have a fleet of agents run `--dangerously` on one or many machines while keeping only the files on your machine.
 - Your editor, your git, your diff tool — all local, on the same files the agents are writing.
 - When an agent goes wrong, the fix is a local git checkout, not an SSH session.
 - 0 latency vs terminal vim
 - No cloud service, no account, no third party. Just SSH to machines you already have.


## What?

Autobahn keeps directories in sync across machines in fractions of a second.

```toml
# ~/.autobahn/config.toml
[defaults]
mode = "two-way-conflict"

[groups.formula1]
alpha = "~/Designs" # pick a folder
betas = [
  "ubuntu@audi.de:~/Workspace/Designs", # Specify as much as you want
  "ec2-user@mercedes-benz.de",
  "porsche.de"
]

[groups.vibe]
mode = "two-way-alpha" # alpha wins on conflicts
alpha = "~/vibecoding"
betas = [
  # Specify the targets (from ssh config)
  "ubuntu@audi.de:~/Workspace/Designs",
  "ec2-user@mercedes-benz.de",
  "porsche.de"
]
```

```sh
autobahn watch # watch once
autobahn install # or run as a service
```


## Getting Started

First install Autobahn:

```sh
curl -fsSL https://github.com/fny/autobahn/releases/latest/download/install.sh | sh
```

Then update `~/.autobahn/config.toml` to list your sync groups. For details on configuration options, see [Configuration](docs/configuration.md).

Afterwards run `autobahn install` to install the service. For installation customizations like home directory and more see [Custom Installation](#custom-installation).

## Getting started

Describe what should stay in sync in `~/.autobahn/config.toml`. Each **group** fans one source root (the *alpha*) out to any number of destinations (the *betas*):

```toml
# ~/.autobahn/config.toml

[defaults]                  # inherited by every group; any key can be
mode = "two-way-conflict"       # overridden per group
ignores = [".git"]

[groups.project]
alpha = "~/project"         # the source root you edit
betas = [                   # everywhere it fans out to
  "build.example.com",              # inherits the alpha path
  "user@lab.example.com:/srv/project",
  "/mnt/backup/project",            # local paths work too
]
ignores = ["target"]        # appended to the defaults' ignores
```

Then run it:

```sh
autobahn watch              # every configured session, here, until Ctrl-C
autobahn install            # ...or as a login service that survives logout
```

Both sides watch their filesystems natively (inotify/FSEvents), so edits on either end propagate within a fraction of a second. Remote endpoints use the scp-style `[user@]host:path` syntax you already know, key-based SSH auth, and *either* side of a group may be remote.

To be told when something needs you, add one line at the top of the config — the hold times, coalescing and the rest are built in:

```toml
on_alert = "terminal-notifier -title autobahn -message \"$AUTOBAHN_SUMMARY\""
```

On a Mac there is also an experimental [menu bar app](docs/macos-app.md) that shows the state of every session at a glance and settles conflicts from a menu.

That's the whole setup. Everything else is in the documentation.

## Benchmarks

<!-- Todo update this if posbile-->
| | autobahn | mutagen | |
|---|---|---|---|
| Propagate one edit, Chromium (505k files) | **188 ms** | 7,118 ms | 37.8× |
| Propagate one edit, 40k files, 10 agents | **52 ms** | 1,854 ms | 35.4× |
| Peak memory, Chromium | **249 MB** | 2,033 MB | 8.2× |
| CPU while idle, Chromium | **0.2%** | 50% | 250× |
| First sync, Chromium | 423 s | 418 s | ~1% |

autobahn is faster in all fifteen cells. The one row where the two tie is the first sync, which is bound by the disk rather than by either tool; where they differ is in what it costs to stay caught up afterwards.

These figures were measured at 0.3.0. What has moved since, and why, is in [Benchmarks](docs/benchmarks.md); [Why mutagen is slower](docs/mutagen.md) traces each gap to mutagen's code.

## AI Disclaimer

This project was heavily vibe coded, and with great vibe coding comes great responsibilitiy. I have:

- Scanned all the code in this repo
- Run extensive soak testing
- Used Autobahn on my own for weeks
- Had guardrail-free models do cybersecurity scans
- Run Autobahn successfully through thousands of benchmarks

Most of the internal documentation was orignally written by LLMs. Forgive me for the lingering Claudeisms.

## Custom Installation

```sh
curl -fsSL https://github.com/fny/autobahn/releases/latest/download/install.sh | sh
```

- Installs `autobahn` to your `PATH`
- Copies the agent bundle into `~/.autobahn/agents` to bootstrap hosts with on different platform (i.e. arch)
- `--bin-dir` chooses where the command goes (default `~/.local/bin`)
- `--no-agents` skips the agent bundle (generally a bad idea)
- `--version` specifies a release

`AUTOBAHN_BIN_DIR` and `AUTOBAHN_HOME` set the same two destinations from the environment, for a non-interactive install.

To update run `autobahn update`. `--dry-run` and `--version TAG` options exist too.

To do it by hand instead, grab a binary from [Releases](/fny/autobahn/releases): each release ships `autobahn-<os>-<arch>` binaries (Linux binaries are static — they run on any distribution), an `autobahn-agents.tar.gz` bundle, and `SHA256SUMS`. The macOS binaries are signed and notarised, and the menu bar app ships beside them as `Autobahn-macos-aarch64.zip`.

```sh
# Put your platform's binary on your PATH:
install -m 755 autobahn-linux-x86_64 ~/.local/bin/autobahn
```

If your fleet spans platforms (e.g. a Mac syncing to Linux servers), also unpack the agents bundle, and autobahn picks the right agent for each host automatically:

```sh
mkdir -p ~/.autobahn
tar xzf autobahn-agents.tar.gz -C ~/.autobahn    # creates ~/.autobahn/agents/
```

Or build from source with `cargo build --release --locked` (Rust stable, Unix only).

## Expiremntal Features

 - Peering mode: this is dangerous.
 - 

## Documentation

**Using it**

- [Configuration](docs/configuration.md) — every key, where it lives, what it defaults to
- [Modes](docs/modes.md) — the sync modes, case by case, and which to pick
- [Ignores](docs/ignores.md) — patterns, ignore files, negations
- [Alerts](docs/alerts.md) — the one hook, and when it fires
- [Commands](docs/commands.md)
- [Conflicts](docs/conflicts.md)
- [TUI](docs/shop.md) — `autobahn mi`
- [Menu bar app](docs/macos-app.md) — `Autobahn.app`, experimental
- [Loging](docs/logging.md) — levels and what `debug` adds
- [State](docs/state.md) — `~/.autobahn`, `clean`, agents

**Understanding it**

- [Safety](docs/safety.md) — how errors are prevented, and where the guarantees stop
- [How autobahn works](docs/how-it-works.md) — the design and its reasoning
- [Overlapping and nested roots](docs/nesting.md)
- [Scope and support boundaries](docs/support-boundaries.md)

**Measuring it**

- [Benchmarks](docs/benchmarks.md) — autobahn against mutagen, and what has changed since
- [The benchmark matrix](docs/benchmark-matrix.md) — every cell, every percentile
- [Why mutagen is slower](docs/mutagen.md) — each gap, traced to mutagen's code

**Working on it**

- [Development](docs/development.md) — building, targeted tests, the A/B gate
- [Releases](docs/releases.md) — what ships, prereleases, and `autobahn update`
- [Correctness](docs/correctness/) — the invariants and what enforces them

## Scope

Unix only: Linux (x86-64 and arm64) and macOS (Apple Silicon), with macOS a first-class target rather than a build target. Transport is SSH. Roots must live on local filesystems: network mounts are best-effort. The full list of what is and is not covered is in [Scope and support boundaries](docs/support-boundaries.md).
