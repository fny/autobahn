# Autobahn

Fast, safe, SSH-focused bidirectional file synchronization.

Autobahn keeps a local directory synchronized with another directory — local
or on the far side of an SSH connection — using three-way reconciliation
against a persisted ancestor, rsync-style delta transfer, and a
safety-first transition engine that refuses to destroy content it didn't
expect to find.

It is a from-scratch Rust distillation of the architecture that emerged from
a deep memory/performance overhaul of [Mutagen]'s synchronization engine
(see `../mutagen`): enum-based trees with name-sorted, copy-on-write shared
children; scan metadata resident on the nodes themselves rather than in a
path-keyed cache; linear-merge reconciliation; and streaming transfers end
to end. What required convention and adversarial review to keep safe in Go —
immutable shared hierarchies, copy-on-write without aliasing bugs — the
borrow checker and `Arc::make_mut` enforce structurally here.

## Usage

```sh
# Bidirectional sync between a local directory and a remote one.
autobahn sync ./project user@host:/home/user/project

# Watch continuously; mirror exactly; ignore build artifacts.
autobahn sync ./project user@host:/srv/project \
    --watch --mode one-way-replica --ignore target --ignore '*.log'
```

Remote roots use scp-style `[user@]host:path` syntax. The CLI runs the
agent over `ssh` (key-based auth; streams are LZ4- and SSH-compressed) and
speaks a framed, version-checked protocol over stdio. Agents install
themselves: connections invoke a versioned agent path
(`~/.autobahn/bin/autobahn-<version>`), and when it's missing the
controller probes the remote platform, streams a matching binary into
place (from `AUTOBAHN_AGENTS_DIR`, an `agents/` directory beside the
executable — build one with `scripts/build-agents.sh` — or, on a
same-platform fleet, the running executable itself), and retries. There is
no daemon; state (the synchronization ancestor, staged content, and a scan
cache that accelerates cold starts) lives under
`~/.autobahn/sessions/<session-id>`.

Both sides watch their roots natively (inotify/FSEvents), so `--watch` and
the supervisor react to changes — local and remote — within a fraction of a
second; the configured interval is only a fallback heartbeat. Filesystem
behavior is probed per root: executability bits are propagated around
volumes that can't store them, names recompose to NFC on decomposing (HFS+)
volumes, and case-insensitive volumes refuse case-colliding siblings
instead of corrupting them. Created files default to conservative 0600/0700
permissions (configurable per group or via `--file-mode`/`--directory-mode`),
and symbolic links can be synchronized raw (default), validated as portable,
or ignored (`--symlink-mode`, or `symlink_mode` per group).

### Supervising many sessions

For more than a one-off sync, a declarative configuration fans **groups** —
one local alpha directory each — out to any number of destinations, and a
supervisor runs every resulting session in parallel:

```toml
# ~/.config/autobahn/config.toml
# Top-level keys (like `disabled`) must precede the first section header.
disabled = ["flaky.example.com"]

[defaults]
mode = "two-way-safe"
ignores = [".git"]
interval = 5            # seconds between cycles

[groups.project]
alpha = "~/project"
ignores = ["target"]
betas = ["build.example.com", "user@lab.example.com:/srv/project"]

[groups.dotfiles]
alpha = "~/.config/shell"
mode = "one-way-replica"
betas = ["build.example.com", "/mnt/backup/shell"]
```

```sh
autobahn up             # supervise every configured session continuously
autobahn up --once      # one pass over every session, then exit
autobahn status         # recorded state of every session, grouped by group
autobahn status project # ... filtered to one group (optionally + host)

# Control a running supervisor (group and host filters optional):
autobahn flush          # wake sessions for an immediate cycle
autobahn pause project  # suspend a group (drops connections and locks)
autobahn resume project
autobahn reset project  # discard the baseline: next cycle merges both
                        # sides additively (resurrects deletions)
```

A beta is remote (`[user@]host[:path]`) unless it visibly denotes a local
path (a leading `.`, `/`, or `~`, or a `/` before any `:`). A remote beta
without a path inherits the group's alpha path as written, so a
home-relative alpha resolves against each remote host's own home. The
configuration is the source of truth: there is no session registry to drift
from it, and no reachability probe to go stale — a session whose destination
is down simply fails its cycle, backs off exponentially, and heals the
moment the host answers again, without affecting its siblings. Each session
records its state to `~/.autobahn/status/` after every attempt, which is
what `status` reads (from any process, running supervisor or not).

Local roots must be absolute or `~`-relative (a working-directory-relative
root would mean different trees under a service than in a shell), and each
session's state directory is exclusively locked while a session runs, so
two processes — two supervisors, or a supervisor and a manual `sync` — can
never race the same session.

### Synchronization modes

| Mode | Behavior |
|---|---|
| `two-way-safe` (default) | Propagates changes both ways; conflicts are reported and left in place. |
| `two-way-resolved` | Both ways; conflicts resolve in alpha's (the first root's) favor. |
| `one-way-safe` | Alpha → beta only; beta-side changes are never overwritten or reverse-propagated. |
| `one-way-replica` | Beta is an exact mirror of alpha. |

### Safety

- Deleting or emptying a synchronization root halts the session rather than
  propagating the deletion.
- Transitions verify on-disk state against what was scanned before
  replacing or removing anything; concurrent modifications become reported
  problems, never data loss.
- A corrupt ancestor is an error, not a silent reset (a reset would
  resurrect deletions).

## Scope

Unix only (Linux; macOS-oriented behaviors — Unicode normalization, case
handling, executability propagation — are implemented and awaiting a macOS
build target). SSH (or any stdio subprocess) transport only — no Docker,
no daemon, no forwarding. Native filesystem watching with an interval
heartbeat as fallback.

## Development

```sh
cargo test          # unit + end-to-end suites (e2e spawns real agent subprocesses)
cargo clippy --all-targets
```

[Mutagen]: https://github.com/mutagen-io/mutagen
