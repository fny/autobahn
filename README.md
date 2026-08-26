# Autobahn

**Fast, safe file synchronization over SSH.**

Autobahn keeps directories in sync — between two folders on your machine,
or between your machine and any host you can `ssh` into. Edit locally,
and your changes appear on the remote side in a fraction of a second.
Changes made remotely flow back just as fast.

```sh
autobahn sync ~/project dev-server:~/project
```

That's the whole setup. No daemon to run, nothing to install on the remote
host — autobahn installs its own agent over the same SSH connection on
first contact.

## Why autobahn?

- **Fast.** A cold sync of a 40,000-file, 227MB tree to another region
  takes about 5 seconds; after that, a changed file lands on the other
  side in ~150ms. Transfers send only deltas, LZ4-compressed.
- **Light.** The remote agent uses ~9MB of memory. The supervisor managing
  many sessions uses ~40MB. There is no background daemon.
- **Safe by default.** Three-way reconciliation against a remembered
  baseline means autobahn knows the difference between "you deleted this
  file" and "this file never existed here" — so it never propagates a
  deletion it can't justify, refuses to overwrite files that changed
  mid-sync, and halts entirely if a whole sync root disappears.
- **Honest about conflicts.** If both sides changed the same file, the
  default mode reports the conflict and touches nothing.

## Installing

Grab a binary from [Releases](../../releases): each release ships
`autobahn-<os>-<arch>` binaries (Linux binaries are static — they run on
any distribution), an `autobahn-agents.tar.gz` bundle, and `SHA256SUMS`.

```sh
# Put your platform's binary on your PATH:
install -m 755 autobahn-linux-x86_64 ~/.local/bin/autobahn
```

If the machines you sync with share your platform, that's everything: the
running binary doubles as the agent it installs remotely. If your fleet
spans platforms (say, a Mac syncing to Linux servers), also unpack the
agents bundle next to the binary — autobahn picks the right agent for each
host automatically:

```sh
tar xzf autobahn-agents.tar.gz -C ~/.local/bin   # creates ~/.local/bin/agents/
```

Or build from source with `cargo build --release` (Rust stable, Unix only).

## Your first sync

```sh
# Two local folders, bidirectional:
autobahn sync ~/project /mnt/backup/project

# Local ↔ remote over SSH (key-based auth):
autobahn sync ~/project user@host:/srv/project

# Keep watching and syncing until interrupted:
autobahn sync ~/project user@host:/srv/project --watch

# Mirror exactly (remote becomes a replica), ignoring build artifacts:
autobahn sync ~/project host:/srv/project \
    --watch --mode one-way-replica --ignore target --ignore '*.log'
```

Remote roots use the scp-style `[user@]host:path` syntax you already know,
and *either* side may be remote — you can pull from a build server, or even
relay between two remote hosts through your machine. Both sides watch
their filesystems natively (inotify/FSEvents), so `--watch` reacts to
changes on either end within a fraction of a second.

## Keeping many things in sync

One-off `sync` commands are fine for experiments. For the syncs you want
*always* running, describe them once in a config file and let the
supervisor run them all:

```toml
# ~/.autobahn/config.toml

[defaults]                  # inherited by every group; any key can be
mode = "two-way-safe"       # overridden per group
ignores = [".git"]
interval = 5                # heartbeat seconds between cycles

[groups.project]
alpha = "~/project"         # the source root you edit
betas = [                   # everywhere it fans out to
  "build.example.com",              # inherits the alpha path (~/project
                                    # in *that* host's home)
  "user@lab.example.com:/srv/project",
  "/mnt/backup/project",            # local paths work too
]
ignores = ["target"]        # appended to the defaults' ignores

[groups.dotfiles]
alpha = "~/.config/shell"
mode = "one-way-replica"
betas = ["build.example.com"]
```

```sh
autobahn up                 # run every configured session, forever
autobahn up --once         # one pass over everything, then exit
autobahn status            # what every session last did
autobahn status project    # ...filtered to one group

# Poke a running supervisor:
autobahn flush             # sync everything right now
autobahn pause project     # suspend a group (drops its connections)
autobahn resume project
autobahn reset project     # forget the baseline; next cycle merges both
                           # sides additively (resurrects deletions)
```

Each (alpha, beta) pair becomes its own session, and sessions are
independent: a host being down just means its session retries with backoff
and heals the moment the host answers — the others never notice. Sessions
targeting the same host share one SSH connection.

The config file is the source of truth. There is no separate registry of
sessions to drift out of date: what the file says is what runs.

### All the options

| Key | Where | What it does |
|---|---|---|
| `alpha` | group | The source root: a local path, or `[user@]host:path`. |
| `betas` | group | Destinations: local paths and/or remote specs. A remote beta with no path inherits the alpha's path. |
| `mode` | both | Synchronization mode — see the table below. |
| `ignores` | both | Gitignore-style patterns. Defaults' patterns apply first, then the group's. |
| `interval` | both | Seconds between heartbeat cycles (watching makes this a fallback, not the reaction time). |
| `symlink_mode` | both | `raw` (sync verbatim, default), `portable` (validate portability), or `ignore`. |
| `file_mode` / `directory_mode` | both | Octal permissions for created files/directories (default `600`/`700`). |
| `max_file_size` | both | Files larger than this (e.g. `"100MB"`, `"2GiB"`) stay on disk but are left out of syncing — never mistaken for deletions. |
| `max_entry_count` | both | If a scan finds more entries than this, the cycle fails — a guard against pointing a session at the wrong directory. |
| `staging` | both | Where in-flight content lives: `state` (default), `beside-root` (same filesystem as the root — guarantees rename-speed publishing), or `inside-root` (for roots that are the only writable place on their host). |
| `default_owner` / `default_group` | both | Ownership for created entries (`name`, `1000`, or `id:1000`), resolved on each endpoint's own host. Needs chown rights. |
| `agent_command` | group | Advanced: reach remote endpoints through this command instead of SSH. |
| `disabled` | top level | Host names to skip everywhere. A disabled beta host drops that beta; a disabled alpha host drops its whole group. |

"Both" means the key works in `[defaults]` and per group, with the group
winning. An endpoint spec is treated as remote unless it visibly looks like
a local path (starts with `.`, `/`, or `~`, or has a `/` before any `:`).

### Which mode do I want?

| Mode | Behavior | Reach for it when… |
|---|---|---|
| `two-way-safe` (default) | Changes flow both ways; conflicts are reported and left alone. | You edit on both sides and want nothing lost, ever. |
| `two-way-resolved` | Both ways; conflicts resolve in alpha's favor. | You edit on both sides but alpha is the truth when they collide. |
| `one-way-safe` | Alpha → beta only; beta's own changes are never overwritten. | Deploy-ish flows where the remote side may hold extra files (logs, caches). |
| `one-way-replica` | Beta is an exact mirror of alpha. | Backups, artifact distribution — beta should be *identical*. |

## What's happening under the hood

Every cycle: scan both sides (accelerated by a persisted cache — unchanged
files are never re-read), reconcile the two scans three-way against the
remembered ancestor, transfer only what's needed as rsync-style deltas,
stage incoming content safely off to the side, verify its integrity, then
swap it into place with atomic renames. A file being written mid-transfer
is detected by digest and simply retried next cycle — partial content
never lands.

Autobahn adapts to each filesystem it touches, probing per root:
executable bits are propagated around volumes that can't store them,
names recompose to NFC on decomposing (HFS+-style) volumes, and
case-insensitive volumes refuse case-colliding siblings instead of
corrupting them.

Everything autobahn keeps lives under one directory, `~/.autobahn`, on
every machine it touches — configuration, per-session state, status
records, installed agents, and staged content. Removing it is a full
uninstall (aside from the binary itself).

Remote hosts need nothing pre-installed. Connections invoke a versioned
agent path (`~/.autobahn/bin/autobahn-<version>`); when it's missing —
a fresh host, or your first connect after upgrading — the controller
probes the platform, streams the matching agent into place over the same
SSH connection, and retries. Upgrades therefore roll out host by host,
automatically, on first contact.

### Safety rules

- Deleting or emptying a synchronization root **halts the session** rather
  than propagating the deletion.
- Transitions verify on-disk state against what was scanned before
  replacing or removing anything; concurrent modifications become reported
  problems, never data loss.
- A corrupt ancestor is an error, not a silent reset (a reset would
  resurrect deletions).
- A missing *source* root is an error, not an empty source (a typo'd path
  plus a mirroring mode must not empty the destination).

## Scope

Unix only: Linux today; macOS behaviors (Unicode normalization, case
handling, executability propagation) are implemented and release binaries
build for Darwin. Transport is SSH (or any stdio subprocess) — no Docker,
no daemon, no port forwarding.

## Development

```sh
cargo test                    # unit + end-to-end suites (e2e spawns real agents)
cargo clippy --all-targets
scripts/build-agents.sh       # cross-build the agents bundle
```

Autobahn is a from-scratch Rust distillation of the architecture that
emerged from a deep memory/performance overhaul of [Mutagen]'s
synchronization engine: enum-based trees with name-sorted, copy-on-write
shared children; scan metadata resident on the nodes themselves; linear-
merge reconciliation; streaming transfers end to end. What required
convention and adversarial review to keep safe in Go, the borrow checker
and `Arc::make_mut` enforce structurally here.

[Mutagen]: https://github.com/mutagen-io/mutagen
