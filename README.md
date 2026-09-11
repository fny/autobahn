# Autobahn <picture><source media="(prefers-color-scheme: dark)" srcset="assets/sign-white.svg"><img src="assets/sign.svg" alt="" height="32"></picture>

*Real-time sync with unmatched speed and German safety standards.*

## What

Autobahn keeps directories in sync across machines in fractions of a second.

```toml
# ~/.autobahn/config.toml
[groups.formel1]
alpha = "~/golfwagen"
betas = [
  "audi.de",
  "mercedes-benz.de",
  "porsche.de",
  "man.eu",
]
```

```sh
autobahn watch
```

## Motivation

 - I don't like running agents on my computer.
 - I have a several VMs where my agents have free reign to `rm -rf`
 - I want to see edits live on my machine.
 - I want my agents to see edits live on their machines.
 - I want to use tools on my machine to interact with my code.

Enter [Mutagen](https://github.com/mutagen-io/mutagen) which promised snappy file sync. Aside from the clunky UX, it scaled well to tens of thousands of files but at hundreds of thousands of files, RAM began to explode.

## Why autobahn?

- **Fast.** A changed file lands on the other side in about 50 ms on a
  40,000-file tree, and in under 200 ms on a half-million-file Chromium
  checkout. Transfers send only deltas, LZ4-compressed.
- **Light.** 33 MB for a 40,000-file tree and 249 MB for Chromium. Idle,
  it uses 0.2% of a core. There is no background daemon.
- **Zero remote setup.** Nothing to install on the far side — autobahn
  streams its own agent over the same SSH connection on first contact,
  and upgrades roll out host by host the same way.
- **Safe by default.** Three-way reconciliation against a remembered
  baseline means autobahn knows the difference between "you deleted this
  file" and "this file never existed here" — so it never propagates a
  deletion it can't justify, refuses to overwrite files that changed
  mid-sync, and halts entirely if a whole sync root disappears.
  [How errors are prevented](docs/safety.md).
- **Honest about conflicts.** If both sides changed the same file, the
  default mode reports the conflict and touches nothing.

## The numbers

Against mutagen, on matched pairs of AWS machines — fifteen cells, ten
repeats each, about 440,000 latency samples:

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

## Installing

```sh
curl -fsSL https://raw.githubusercontent.com/fny/autobahn/master/scripts/install.sh | sh
```

That installs the command onto your `PATH` and the agent bundle into
`~/.autobahn/agents`, which is where the controller looks when it needs
to bootstrap a host whose platform differs from your own. `--prefix`
chooses where the command goes, `--no-agents` skips the bundle, and
`--version` pins a release.

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
- [Commands](docs/commands.md) — `status`, `sync`, `flush`, `reset`, `verify`, and one-off syncs
- [Conflicts](docs/conflicts.md) — `issues`, `conflicts`, `diff`, `resolve`
- [The shop](docs/shop.md) — `autobahn mi`
- [The menu bar app](docs/macos-app.md) — `Autobahn.app`
- [The log](docs/logging.md) — levels and what `debug` adds
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
SSH — no Docker, no daemon, no port forwarding. Roots must live on local
filesystems; network mounts are best-effort. The full list of what is
and is not covered is in [Scope and support boundaries](docs/support-boundaries.md).
