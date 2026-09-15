# Autobahn <picture><source media="(prefers-color-scheme: dark)" srcset="assets/sign-white.svg"><img src="assets/sign.svg" alt="" height="32"></picture>

*Subsecond sync with German precision.*

Sold? Jump to [Getting Started](#getting-started).

## What

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

## Motivation

 - I don't like running agents on my computer
 - I want to edit files along side them
 - I want to use local tools to interact the files

Enter [Mutagen](https://github.com/mutagen-io/mutagen) which promised snappy file sync for small files. Aside from the clunky UX, it scaled decently to tens of thousands of files, but at hundreds of thousands of files, RAM began to baloon to gigs.

Hence Autobahn was born.

## Why Autobahn?

- **Fast** A changed file lands on the other side in about 50 ms on a
  40,000-file tree, and in under 200 ms on a half-million-file Chromium
  checkout. Not too shabby.
- **Light** 33 MB for a 40,000-file tree and 249 MB for Chromium. Idle,
  it uses 0.2% of a core.
- **Zero remote setup.** Autobahn streams its own sync agent over SSH
  on first contact, and upgrades roll out the same way.
- **Safe.** Autobahn performs three-way reconciliation against a remembered
  baseline. "You deleted this file" never conflicts with "this file never
  existed here." You can even calibrate your risk tolerance with different
  sync modes. [How errors are prevented](docs/safety.md).
- **User friendly.** We have a TUI and a tray item that works on most
  operating systems.

## Benchmarks

<!-- Todo update this if posbile-->
| | autobahn | mutagen | |
|---|---|---|---|
| Propagate one edit, Chromium (505k files) | **188 ms** | 7,118 ms | 37.8× |
| Propagate one edit, 40k files, 10 agents | **52 ms** | 1,854 ms | 35.4× |
| Peak memory, Chromium | **249 MB** | 2,033 MB | 8.2× |
| CPU while idle, Chromium | **0.2%** | 50% | 250× |
| First sync, Chromium | 423 s | 418 s | ~1% |

autobahn is faster in all fifteen cells. The one row where the two tie is
the first sync, which is bound by the disk rather than by either tool;
where they differ is in what it costs to stay caught up afterwards.

These figures were measured at 0.3.0. What has moved since, and why, is in
[Benchmarks](docs/benchmarks.md); [Why mutagen is slower](docs/mutagen.md)
traces each gap to mutagen's code.

## Getting Started

First run

```sh
curl -fsSL https://raw.githubusercontent.com/fny/autobahn/main/scripts/install.sh | sh
```

Then run `autobahn init` to create an `~/.autobahn/config.toml` example and modify it. For details on configuration options, see [Configuration](docs/configuration.md).

For installation customizations like home directory and more see [Custom Installation](#custom-installation).

## AI Warning

With great vibe coding comes great responsibilitiy. I have:

 - Scanned all the code in this repo
 - Run extensive soak testing
 - Used Autobahn on my own for weeks
 - Had guardrail-free models do cybersecurity scans
 - Run Autobahn successfully through thousands of benchmarks

Most of the internal documentation is written by LLMs. I've audited and edited alsmost all of it

## Custom Installation

```sh
curl -fsSL https://raw.githubusercontent.com/fny/autobahn/main/scripts/install.sh | sh
```

 - Installs `autobahn` to your `PATH`
 - Copies the agent bundle into `~/.autobahn/agents`, which is where the controller looks when it needs
to bootstrap a host whose platform differs from your own.
 - `--prefix` chooses where the command goes
 - `--no-agents` skips the agent bundle (generally a bad idea)
 - `--version` pins a release.

`AUTOBAHN_PREFIX` and `AUTOBAHN_HOME` set the same two destinations
from the environment, for a non-interactive install.

To do it by hand instead, grab a binary from [Releases](../../releases):
each release ships `autobahn-<os>-<arch>` binaries (Linux binaries are
static — they run on any distribution), an `autobahn-agents.tar.gz`
bundle, and `SHA256SUMS`. The macOS binaries are signed and notarised, and
the menu bar app ships beside them as `Autobahn-macos-aarch64.zip`.

```sh
# Put your platform's binary on your PATH:
install -m 755 autobahn-linux-x86_64 ~/.local/bin/autobahn
```

If the machines you sync with share your platform, that's everything: the
running binary doubles as the agent it installs remotely. If your fleet
spans platforms (say, a Mac syncing to Linux servers), also unpack the
agents bundle, and autobahn picks the right agent for each host
automatically:

```sh
mkdir -p ~/.autobahn
tar xzf autobahn-agents.tar.gz -C ~/.autobahn    # creates ~/.autobahn/agents/
```

Or build from source with `cargo build --release` (Rust stable, Unix only).

## Getting started

Describe what should stay in sync in `~/.autobahn/config.toml`. Each
**group** fans one source root (the *alpha*) out to any number of
destinations (the *betas*):

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

Both sides watch their filesystems natively (inotify/FSEvents), so edits
on either end propagate within a fraction of a second. Remote endpoints
use the scp-style `[user@]host:path` syntax you already know, key-based
SSH auth, and *either* side of a group may be remote.

To be told when something needs you, add one line at the top of the
config — the hold times, coalescing and the rest are built in:

```toml
on_alert = "terminal-notifier -title autobahn -message \"$AUTOBAHN_SUMMARY\" -execute \"$AUTOBAHN_OPEN\""
```

On a Mac there is also a [menu bar app](docs/macos-app.md) that shows the
state of every session at a glance and settles conflicts from a menu.

That's the whole setup. Everything else is in the documentation.

## Documentation

**Using it**

- [Configuration](docs/configuration.md) — every key, where it lives, what it defaults to
- [Modes](docs/modes.md) — the four sync modes, case by case, and which to pick
- [Ignores](docs/ignores.md) — patterns, ignore files, negations
- [Alerts](docs/alerts.md) — the one hook, and when it fires
- [Commands](docs/commands.md)
- [Conflicts](docs/conflicts.md)
- [TUI](docs/shop.md) — `autobahn mi`
- [Menu bar app](docs/macos-app.md) — `Autobahn.app`
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
- [Correctness](docs/correctness/) — the invariants and what enforces them

The [documentation index](docs/README.md) lists every page.

## Scope

Unix only: Linux (x86-64 and arm64), macOS (Apple Silicon), and FreeBSD,
with macOS a first-class target rather than a build target. Transport is
SSH. Roots must live on local filesystems: network mounts are best-effort. The full list of what is
and is not covered is in [Scope and support boundaries](docs/support-boundaries.md).
