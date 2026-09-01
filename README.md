# Autobahn

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
autobahn up
```


## Motivation

 - I don't like running agents on my computer.
 - I have a several VMs where my agents have free reign to `rm -rf`
 - I want to see edits live on my machine.
 - I want my agents to see edits live on their machines.
 - I want to use tools on my machine to interact with my code.

Enter [Mutagen](https://github.com/mutagen-io/mutagen) which promised snappy file sync. Aside from the clunky UX, it scaled well to tens of thousands of files but at hundreds of thousands of files, RAM began to explode.

## Why autobahn?

- **Fast.** A cold sync of a 40,000-file, 227MB tree to another region
  takes about 5 seconds; after that, a changed file lands on the other
  side in ~150ms. Transfers send only deltas, LZ4-compressed.
- **Light.** The remote agent uses ~9MB of memory. The supervisor managing
  many sessions uses ~40MB. There is no background daemon.
- **Zero remote setup.** Nothing to install on the far side — autobahn
  streams its own agent over the same SSH connection on first contact,
  and upgrades roll out host by host the same way.
- **Safe by default.** Three-way reconciliation against a remembered
  baseline means autobahn knows the difference between "you deleted this
  file" and "this file never existed here" — so it never propagates a
  deletion it can't justify, refuses to overwrite files that changed
  mid-sync, and halts entirely if a whole sync root disappears.
- **Honest about conflicts.** If both sides changed the same file, the
  default mode reports the conflict and touches nothing.

## Installing

```sh
curl -fsSL https://raw.githubusercontent.com/fny/autobahn/master/scripts/install.sh | sh
```

That installs the command onto your `PATH` and the agent bundle into
`~/.autobahn/agents`, which is where the controller looks when it needs
to bootstrap a host whose platform differs from your own. `--prefix`
chooses where the command goes, `--no-agents` skips the bundle, and
`--version` pins a release. (While this repository is private, the
script needs the GitHub CLI: private release assets are not served over
plain download URLs.)

`AUTOBAHN_PREFIX` and `AUTOBAHN_HOME` set the same two destinations
from the environment, for a non-interactive install.

To do it by hand instead, grab a binary from [Releases](../../releases):
each release ships
`autobahn-<os>-<arch>` binaries (Linux binaries are static — they run on
any distribution), an `autobahn-agents.tar.gz` bundle, and `SHA256SUMS`.

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

The bundle is looked for in `AUTOBAHN_AGENTS_DIR`, then
`~/.autobahn/agents`, then an `agents` directory beside the binary — so
a bundle that travels with a relocatable binary keeps working.

Or build from source with `cargo build --release` (Rust stable, Unix only).

## Getting started

Describe what should stay in sync in `~/.autobahn/config.toml`. Each
**group** fans one source root (the *alpha*) out to any number of
destinations (the *betas*):

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

Then run it:

```sh
autobahn up                 # run every configured session, forever
```

That's the whole setup. Both sides watch their filesystems natively
(inotify/FSEvents), so edits on either end propagate within a fraction of
a second; the configured interval is only a fallback heartbeat. Remote
endpoints use the scp-style `[user@]host:path` syntax you already know,
key-based SSH auth, and *either* side of a group may be remote — you can
pull from a build server, or relay between two remote hosts through your
machine.

Working with a running (or stopped) supervisor:

```sh
autobahn up --once         # one pass over everything, then exit
autobahn status            # what every session last did
autobahn status project    # ...filtered to one group
autobahn status .          # ...to whatever syncs the working directory
autobahn status ~/project  # ...or any folder inside a synchronized root
autobahn status --conflicts   # list every conflicting path, not a count

# Poke a running supervisor:
autobahn flush             # sync everything right now
autobahn pause project     # suspend a group (drops its connections)
autobahn resume project
autobahn reset project     # forget the baseline; next cycle merges both
                           # sides additively (resurrects deletions)
autobahn verify project    # next cycle re-reads every byte, catching
                           # content whose metadata never moved
autobahn clean --dry-run   # what state belongs to sessions no longer
autobahn clean             # in the config; then remove it
```

Removing a group from the config stops its sessions but keeps their
state, so that adding the group back later resumes from memory rather
than re-merging two drifted trees. `clean` is how that state is
eventually let go: it removes ancestors, status records, staged content,
and endpoint locks for any session the config no longer describes.
Anything a running session holds is skipped, and the files in the
synchronized trees are never touched.

`scripts/mi` runs a guided tour of all of this against throwaway
directories — every command, and every state a session can report,
printed as the binary actually produces them.

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
| `durability` | both | `process` (default) or `power`. The default survives a crashed process; `power` additionally syncs each journal append to stable storage, trading a little latency for power-loss durability. Records that announce a transition are synced either way whenever a remote endpoint is involved. |
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

The modes differ only in six situations. Everything else — an
unchanged file, a new file on alpha, a rename — behaves identically in
all four. This is what each mode actually does, case by case:

| | `two-way-safe` | `two-way-resolved` | `one-way-safe` | `one-way-replica` |
|---|---|---|---|---|
| Alpha edits a file | → beta | → beta | → beta | → beta |
| Beta edits a file | → alpha | → alpha | stays on beta, **reported as a conflict** | **overwritten** from alpha |
| Both edit the same file | **conflict**; both sides keep their own | alpha's version wins, silently | **conflict**; both sides keep their own | alpha's version wins, silently |
| Alpha deletes a file | → beta | → beta | → beta | → beta |
| Beta creates a new file | kept | kept | kept | **deleted** |
| Beta deletes a file | → alpha | → alpha | restored from alpha | restored from alpha |

Three things in that table surprise people:

**`one-way-safe` is not "ignore beta".** It refuses to overwrite
anything beta changed, and *tells you* — a file edited on beta is
reported as a conflict every cycle until you resolve it. That is the
mode's whole point: alpha pushes outward, but never destroys work that
appeared on the far side. If you want beta's edits silently discarded,
you want `one-way-replica`.

**`one-way-replica` deletes files it has never seen.** Beta is made
*identical* to alpha, so logs, caches, and anything else generated on
beta are removed. Never point it at a directory the far side also
writes to.

**Deletions propagate in every mode**, including the one-way ones —
deleting on alpha deletes on beta. What varies is only the reverse
direction. (A deletion large enough to look like a vanished disk halts
the session instead; see the safety rules.)

#### Modes and fan-out

When one alpha fans out to several betas, each destination is its own
session, and the mode decides what happens when two betas change the
same file at once. Under `two-way-safe`, whichever lands first reaches
alpha and the other session reports a conflict — both edits survive,
one needs a human. Under `two-way-resolved`, the second edit
overwrites the first everywhere, silently, because "alpha wins" and
alpha is now whatever arrived most recently. Neither is wrong, but the
second only suits a fan-out you push *from* rather than edit at both
ends.

### Overlapping and nested roots

Several sessions may share a root exactly. That is the fan-out, star,
and relay shape above, and it is ordinary: those sessions share one
watcher and one scan of the root, and each write is validated against
the scan it was reconciled from.

*Nesting* is different, and is refused when either endpoint is written:

```
sessions 'dist@/web/dist' and 'project@/backup/project': endpoint
/srv/project/dist is nested inside /srv/project and at least one of
them is written; two sessions cannot safely write one tree region from
independent ancestors. Add it to the outer group's `ignores` if the
outer session should leave that subtree alone
```

The reason is the ancestor. Two sessions writing one region each keep
their own record of what was last agreed, so each reads the other's
writes as user edits and propagates them back — indefinitely, with
neither able to notice. Sharing a root exactly avoids this because the
sessions share one observation of it; nesting gives them genuinely
separate views, so it cannot.

"Written" is the test, not the mode. An alpha is written only in the
two-way modes; a beta is written in every mode. So two one-way sources
reading overlapping trees are legal — nothing writes the shared region
— while any nesting involving a destination, or a two-way source, is
not.

**Unless the outer session ignores the inner root.** Then they do not
overlap at all: the outer never scans, writes, or records anything
beneath that path. This is how you synchronize a project and ship its
build output somewhere else:

```toml
[groups.project]
alpha = "~/project"
mode = "two-way-safe"
ignores = ["dist"]          # the outer session leaves it alone
betas = ["build.example.com"]

[groups.dist]
alpha = "~/project/dist"    # nested, but excluded above
mode = "one-way-replica"
betas = ["web.example.com:/srv/www"]
```

Both run side by side: the first destination receives the project
without `dist`, the second receives `dist`. Remove the `ignores` line
and the configuration is refused again.

The check sees only one configuration load. Two autobahn processes with
separate config files can still nest their endpoints, because neither
can see the other — see the support boundaries.

## One-off syncs and scripting

Underneath the supervisor sits a single-session command, useful for
trying a pairing before committing it to the config, and for scripts that
need a sync that converges and *exits* with a status code:

```sh
# One bidirectional pass, then exit:
autobahn sync ~/project /mnt/backup/project

# Local ↔ remote over SSH:
autobahn sync ~/project user@host:/srv/project

# Keep watching, like a one-group supervisor:
autobahn sync ~/project user@host:/srv/project --watch

# Mirror exactly, ignoring build artifacts:
autobahn sync ~/project host:/srv/project \
    --watch --mode one-way-replica --ignore target --ignore '*.log'
```

One-shots share session state with the supervisor (same roots → same
session), so deletions propagate correctly across runs, conflicts are
detected across runs, and interrupted transfers resume. A `sync` and a
supervisor can never race the same pairing: each session's state is
exclusively locked while it runs.

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

Unix only. The full test suite runs green on **Linux (x86-64 and
arm64), macOS (Apple Silicon), and FreeBSD**; CI covers all four on
demand. macOS is a first-class target, not a build target: its Unicode
normalization, case-folding, and atomic-creation behaviors are
implemented against the platform's own primitives and exercised on real
APFS volumes. Windows would be a port rather than a build target.

Transport is SSH (or any stdio subprocess) — no Docker, no daemon, no
port forwarding.

## Development

```sh
cargo test                    # unit + end-to-end suites (e2e spawns real agents)
cargo clippy --all-targets
scripts/mi                    # a guided tour of every command and state
scripts/build-agents.sh       # cross-build the agents bundle
gh workflow run ci.yml        # Linux, ARM Linux, macOS and FreeBSD
```

CI is manual rather than push-triggered: the repository is private, and
macOS runner minutes bill at ten times the Linux rate.

The correctness work — the invariants the design claims, the code that
enforces each one, the tests that check it, and the residuals
deliberately left open — is written down in `docs/correctness/`.
`INVARIANTS.md` is the entry point, and was itself the subject of an
independent adversarial review whose confirmed findings are fixed.

Autobahn is a from-scratch Rust distillation of the architecture that
emerged from a deep memory/performance overhaul of [Mutagen]'s
synchronization engine: enum-based trees with name-sorted, copy-on-write
shared children; scan metadata resident on the nodes themselves; linear-
merge reconciliation; streaming transfers end to end. What required
convention and adversarial review to keep safe in Go, the borrow checker
and `Arc::make_mut` enforce structurally here.

[Mutagen]: https://github.com/mutagen-io/mutagen

## Support boundaries

- **Local filesystems.** Synchronization roots are expected to live on
  local filesystems (ext4, XFS, APFS, and the like). Network mounts —
  NFS, SMB/CIFS, FUSE — are best-effort: client-side attribute caching
  can hide another client's writes from both scanning and the checks
  that guard destructive operations, change notification is absent or
  incomplete, and lock semantics depend on the server. If a root must
  live on a network mount, treat this client as the only writer.
  autobahn prints a warning when it detects such a root.
- **One owner per pair of trees.** Two sessions synchronizing the same
  pair of roots are excluded per user on one machine, even across
  different `--state-root`/`--state-dir` settings. *Sharing* one root
  across sessions is fine and pinned by tests — fan-out, star and relay
  topologies all work, because those sessions share one watcher and one
  scan of that root. What is not supported is the same pair of trees
  driven from *different machines*, different Unix users, or different
  state roots: the exclusion lock is local to one of those, so nothing
  detects the overlap and deliberate changes can be silently undone.
- **Timestamp-preserving rewrites.** A tool that rewrites a file with
  identical length while restoring its modification time (reproducible
  builds, `touch -r`) defeats metadata-based change detection, as it
  does in every synchronizer of this design. `autobahn verify` is the
  escape hatch: the next cycle re-reads every byte, so such content
  becomes visible and is synchronized normally.
- **Live databases and other multi-file formats.** A SQLite database is
  three interdependent files changing many times per second, and a
  synchronizer captures them file by file. Syncing one *in one
  direction* works — the copy lags while writes are in flight and
  catches up within a cycle or two of them stopping — but a database
  written on **both** sides produces a conflict that nothing can merge,
  because two diverged databases cannot be reconciled as bytes. Keep
  such files on one side (ignore them, or use a one-way mode), or
  synchronize a snapshot (`sqlite3 app.db ".backup snap.db"`) rather
  than the live file.
