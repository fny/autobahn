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
mode = "two-way-conflict"       # overridden per group
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
mode = "one-way-alpha"
betas = ["build.example.com"]
```

Then run it:

```sh
autobahn watch              # every configured session, here, until Ctrl-C
```

On a terminal, `watch` is a live `autobahn status` that repaints as
sessions report; piped to a file it logs one line per event instead.
To keep syncing when no terminal is:

```sh
autobahn install            # register a login service, and start it
autobahn stop               # stop it (it returns at the next login)
autobahn start
autobahn restart            # after editing the config, or upgrading
autobahn uninstall          # stop it, and unregister it
```

The service is launchd on macOS and a systemd user unit on Linux —
inspect it with `launchctl` or `systemctl --user` like any other — and
it logs to `~/.autobahn/service.log`. There is no daemon of autobahn's
own and nothing backgrounds itself: `start` with no service installed
says so and points at `install` or `watch`.

That's the whole setup. Both sides watch their filesystems natively
(inotify/FSEvents), so edits on either end propagate within a fraction of
a second; the configured interval is only a fallback heartbeat. Remote
endpoints use the scp-style `[user@]host:path` syntax you already know,
key-based SSH auth, and *either* side of a group may be remote — you can
pull from a build server, or relay between two remote hosts through your
machine.

Asking a running supervisor things, whether it is `watch` or the service:

```sh
autobahn status            # what every session is doing, or last did
autobahn status project    # ...filtered to one group
autobahn status .          # ...to whatever syncs the working directory
autobahn status ~/project  # ...or any folder inside a synchronized root
autobahn status --conflicts   # list every conflicting path, not a count
autobahn status --live     # ...repainting, as it happens (Ctrl-C leaves)

autobahn sync              # one pass over every session, then exit
autobahn flush             # sync everything right now
autobahn reset project     # forget the baseline; next cycle merges both
                           # sides additively (resurrects deletions)
autobahn verify project    # next cycle re-reads every byte, catching
                           # content whose metadata never moved
autobahn clean --dry-run   # what state belongs to sessions no longer
autobahn clean             # in the config; then remove it

autobahn mi                # the shop: watch it work, and clear the queue
                           # (? explains every word on the screen)
```

When a session has been working long enough that its silence would look
like death — a cold sync, a first scan, an unreachable host — `status`
says what it is doing, how long it has been at it, and, where the numbers
allow an honest one, an estimate:

```
~/Workspace/Voltai voltai
  ubuntu@fny.voltai.party:~/Workspace
    status: scanning, 6m20s elapsed
      alpha: 412,331 of ~1,470,000 entries (28%), about 14m left
      beta: scanning for 6m20s
    mode: two-way-conflict
```

Routine cycles say nothing. They finish in well under a second, and a
line that flickered into "scanning" every few seconds would report
nothing while hiding what the reader came for — so a phase earns the line
only after the session has been working for five seconds, counted across
the whole run rather than restarted at each step.

To watch it happen rather than sample it, `autobahn status --live`
repaints twice a second and shows every phase however brief. It is a
read-only window onto whatever supervisor is already running — the login
service, or a `watch` in another terminal. (`watch` is the same display,
but it also does the synchronizing.)

Both scroll, with the keys a pager has trained everyone to try: arrows
and `j`/`k` by the line, PgUp/PgDn and space/`b` by the screen, `g`/`G`
or Home/End for the ends, `q` to leave. A footer says where you are in
the list. The content keeps refreshing underneath while you move around
in it, and leaving — by `q` or Ctrl-C — gives the terminal back with the
scrollback intact.

A session between cycles is described by how its last cycle ended, as
before; so is a paused one, and one backing off from an error, both of
which the recorded status already names. The estimate is withheld unless the phase has been running long
enough to have a rate and has a total to measure against — a first scan
of a tree nothing has ever counted reports its progress and its elapsed
time, and no estimate. A remote scan happens inside one request on the
far side, so it reports that it is running and for how long, without
counts.

### What a session's state means

Two questions, not one word list. **Did the cycle run?** If it did not:
`unreachable` (the host is not answering — usually clears itself),
`halted` (a safety refusal — retrying will *never* clear it), or `errored`
(it failed for some other reason; the message is the evidence). If it did
run, the tree is in sync except for what the cycle could not carry:
`conflicts` (both sides changed a path — you pick a winner) and `blocked`
(a path could not be read or written — you fix the filesystem).

Conflicts and blocked paths co-occur, so both counts are reported rather
than one hiding the other.

### Being told

A supervisor running as a login service is invisible by design, which
means a conflict or a permission that stopped working sits there with
nobody told. `[alerts]` runs a command when that happens:

```toml
[alerts]
on_alert    = "terminal-notifier -title autobahn -appIcon \"$AUTOBAHN_ICON\" \\
               -subtitle \"$AUTOBAHN_DETAIL\" -message \"$AUTOBAHN_SUMMARY\""
alert_after = "30s"

[alerts.after]
unreachable = "5m"    # a sleeping laptop deserves patience
halted      = "0s"    # a safety halt does not
```

`on_alert` is the only hook. Which states are alerting is in the summary
it is handed, not in which hook is chosen — a hook per state only moved
the branching out of the command and into the config, and every state
ends the same way, with someone opening a terminal. Four rules make it
usable rather than maddening:

- **Nothing runs while everything is healthy.** Silence is the normal
  state.
- **Nothing runs when it clears, either.** An all-clear asks for no
  action, and a stream of notifications that ask for nothing is what
  teaches you to stop reading the ones that do.
- **A condition must hold for `alert_after` before it counts.** A wifi
  handover that takes every session unreachable for eight seconds is never
  mentioned — it fixed itself — and a sleeping laptop produces one
  notification rather than fifteen. `[alerts.after]` tunes that per state,
  which is timing rather than routing: a blip means something different
  for a sleeping laptop than for a safety halt.
- **An alert fires when the set of sessions in trouble changes**, never on
  repetition. `repeat_after` opts into a nag; it is off by default.
- **Trouble that comes and goes is reported once.** A conflict on a file
  two machines are both editing appears, clears, and returns all day.
  Everything must stay clear for `settle_after` (15 minutes by default)
  before a return counts as news rather than as the same trouble
  continuing — otherwise one flapping session is a notification a minute.

Hooks get `$AUTOBAHN_SUMMARY` (one line: the whole story when there is
one thing wrong, a count when there are several — `voltai → fny: 1
conflict`, `boite is unreachable — 5 groups paused`, `2 groups need you,
1 host away`), `$AUTOBAHN_DETAIL` (one indented line per thing, for a
hook that can show more than a headline), `$AUTOBAHN_ALERT_COUNT`,
`$AUTOBAHN_STATES`, `$AUTOBAHN_EVENT`, `$AUTOBAHN_ICON` (autobahn's own
icon, written into the state directory so a notifier can point at it),
and the full `status --json` document on standard input. They run off the
cycle and cannot affect or delay synchronization: a hook is killed if it
outstays `timeout`, and is skipped while a previous one is still running.

The service runs under launchd or systemd with a sparse environment, so
give commands absolute paths — and on Linux, `notify-send` needs
`DBUS_SESSION_BUS_ADDRESS`.

When two sides disagree about a file, `status` names it and these three
settle it:

```sh
autobahn issues                     # everything that needs you, grouped by cause
autobahn issues voltai autobahn     # ...under one folder
autobahn conflicts                  # every conflict, with what each side holds
autobahn conflicts ~/project        # ...for whatever group syncs that folder
autobahn conflicts voltai autobahn  # ...to one folder inside the group
autobahn conflicts --depth 1        # roll up: which top-level folders, and how many
autobahn conflicts --filter vulns   # only paths containing "vulns"
autobahn conflicts --filter '*.ts'  # ...or matching a glob, at any depth
autobahn diff ./src/main.rs         # the two sides of a file, as a unified diff
autobahn diff project src/main.rs   # same file, by group and root-relative path

autobahn resolve ./src/main.rs --keep alpha       # my version wins, everywhere
autobahn resolve project src/main.rs --keep boite # boite's version wins, everywhere
autobahn resolve project src/main.rs --keep both  # keep alpha's; the loser is
                                                  # renamed aside as main.rs.boite
autobahn resolve voltai autobahn --keep alpha     # every conflict under one folder
autobahn resolve project a.rs b.rs c.rs --keep alpha  # several at once, one pass
autobahn resolve ~/project --all --keep boite     # every conflict in the group
```

A winner is named as `status` names it: `alpha`, or a destination's host
(or path). Its version reaches alpha and every other destination, so one
command settles a conflict across a whole fan-out — including
destinations whose own conflict was with a *third* version.

It asks before it acts, unless you pass `--yes` (`-y`).

What it does is retire the *losing* version, not copy the winning one:
the losing side's copy is removed, or moved aside for `--keep both`, and
the next cycle carries the winner across. That is why it settles a
conflict between a file and a whole directory, which no amount of
copying bytes can do — reconciliation already propagates one side's
content over the other's deletion, for a file, a symbolic link, or a
tree alike.

Two things follow. The removal goes through the same transition path a
cycle uses, so an entry that changed since the command started is
refused and reported rather than destroyed; run the command again to
settle it. And the winner arrives on the next cycle, so the command
flushes the supervisor before returning. Without a supervisor running,
run `autobahn sync` once. Nothing here touches the ancestor.

`--depth` turns a long list into a map of where the trouble is —
seven hundred paths under one folder are one fact about that folder —
and each level tells you how to look inside the next. `--filter` takes a
plain word (matched anywhere in the path, ignoring case) or a glob:
without a slash it matches at any depth, with one it is anchored to the
root.

`autobahn status --json` and `autobahn conflicts --json` print all of
this as one versioned document, for scripts and user interfaces. Each
session carries a `progress` object while a supervisor is running —
`--filter` applies to the JSON too, while `--depth`, being a way of
reading a list, does not.

### The shop

```sh
autobahn mi
```

An easter egg that turned useful. Every session is an order, an order
fills as its transfer does, and the shop is open when a supervisor
answers and shuttered when none does. Every number on it is real — it
reads the same `status --json` document as everything else.

```
  ◉ OPEN   🥖 AUTOBÁNH MÌ   15 customers · 12,480 files · 3.4 GB · 1 filling · 2.1 MB/s

  ▸ voltai   → fny     🥖[▓▓▓▓▓░░░░░░░]  served    filling · 1,204 of 8,530
    vibe     → boite   🥖[▓▓▓▓▓▓▓▓▓▓▓▓]  disputed  2 waiting
    voltai   → boite   🥖[▓░░░░░░░░░░░]  disputed  checking the pantry · 14s · 1 waiting
```

An order is always *something* — served, disputed, out of stock — and
sometimes also *doing* something. The first has the coloured word and
never gives it up. The second has a column of its own, filled only once
the work has gone on long enough to be worth mentioning: the same rule
`status` applies, so a routine scan is never announced and one that
drags names itself without displacing the outcome.

**The counter** is the useful half. `ret` opens any order — where it syncs
from and to, its mode, how many cycles it has run and how much it has
carried — and then its issues as a tree: cause, then place, then path.
Every level of that tree can be acted on, so one keypress settles a whole
directory or a single file.

```
  ┌──────────────────────────────────────────────────────────────┐
  │ the counter  voltai → fny.voltai.party            3 waiting  │
  │ ▾ 2 conflicts                       both sides changed these │
  │     happy                                    deleted on ours │
  │     voltagen                                 deleted on ours │
  │ ▸ 1 blocked on alpha                       unicode collision │
  │ ▾ 20 blocked on beta            Permission denied (os error) │
  │   ▾ azure/backend/.ruff_cache/0.9.10/                     16 │
  │       10497280429343070344                                   │
  │   ▸ arcturus/frontend/apps/web/public/static/              4 │
  └──────────────────────────────────────────────────────────────┘
```

`↑↓` move, `ret` or `→` opens a branch, `←` closes it and then the counter.
On a conflict, `o` keeps ours, `t` keeps theirs, `b` keeps both — and each
asks before it acts, because resolution overwrites a file someone edited
on every destination in the group. It then runs the same `resolve` you
would type, on whichever paths the selected level covers. Blocked paths autobahn cannot clear
itself, since the commands are `sudo` over ssh and a password prompt has
nowhere to appear, so `c` copies the fix to the clipboard instead. `f`
rushes an order, and `q` closes the shop.

Under the counter, the last few lines the supervisor wrote — the only
view of the log there is.

### The menu bar app

```sh
apps/macos/build.sh          # builds Autobahn.app
open apps/macos/Autobahn.app # or drag it to /Applications
```

`release.sh` is the other half, and only for an app someone downloads:
it signs with a Developer ID certificate, sends the result to Apple to be
scanned, and staples the verdict to the bundle so Gatekeeper trusts it
offline. A copy that arrives by `scp`, or through autobahn itself, is
never quarantined and never needs any of that.

The app is a way to launch `autobahn tray`, not a second implementation
of it: the same binary, the same `resolve` a terminal would run. What the
bundle adds is an *identity*. macOS attaches a notification's icon to the
bundle that sent it, and a bare executable has none — which is why
`-appIcon` is ignored from the command line and every alert wears the
icon of whatever ran it. Inside the bundle the icon is autobahn's.

The menu bar icon is a dot in the colour of the worst session: green when
everything is synchronized, amber for conflicts, red for a halt. The menu
lists every group and session, and any conflicting path opens a submenu
offering to keep alpha's version, the destination's, or both.

An icon whose colour is the state of every session — green when all
are synchronized, yellow when any is in conflict, red when any is halted
or unreachable, grey when nothing is running — and a menu with the
detail: each group, each destination with its state, and under each
conflict the ways to settle it (show the diff; keep alpha's, keep that
destination's, keep both), which run the same `resolve` a terminal
would. A session entering conflict, halting, or going unreachable
raises a desktop notification, as does its recovery. The menu also
starts, stops, and restarts the login service and opens its log.

It is a view over `status --json`, polled every few seconds, and holds
no state of its own. macOS and Linux (with a system tray).

`scripts/mi` runs a guided tour of all of this against throwaway
directories — every command, and every state a session can report,
printed as the binary actually produces them.

Removing a group from the config stops its sessions but keeps their
state, so that adding the group back later resumes from memory rather
than re-merging two drifted trees. `clean` is how that state is
eventually let go: it removes ancestors, status records, staged content,
and endpoint locks for any session the config no longer describes.
Anything a running session holds is skipped, and the files in the
synchronized trees are never touched.

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

A mode is a direction and a policy. The direction is whether changes
flow both ways or only from alpha to beta. The policy is what happens
when the two sides disagree about a file: it is reported as a
**conflict** and left alone, or **alpha** wins.

| | conflict | alpha wins |
|---|---|---|
| **two-way** | `two-way-conflict` (default) | `two-way-alpha` |
| **one-way** | `one-way-conflict` | `one-way-alpha` |

| Mode | Reach for it when… |
|---|---|
| `two-way-conflict` | You edit on both sides and want nothing lost, ever. |
| `two-way-alpha` | You edit on both sides but alpha is the truth when they collide. |
| `one-way-conflict` | Deploy-ish flows where the remote side may hold extra files (logs, caches). |
| `one-way-alpha` | Backups, artifact distribution — beta should be *identical*. Also spelled `mirror`. |

The modes differ only in six situations. Everything else — an
unchanged file, a new file on alpha, a rename — behaves identically in
all four. This is what each mode actually does, case by case:

| | `two-way-conflict` | `two-way-alpha` | `one-way-conflict` | `one-way-alpha` |
|---|---|---|---|---|
| Alpha edits a file | → beta | → beta | → beta | → beta |
| Beta edits a file | → alpha | → alpha | stays on beta, **reported as a conflict** | **overwritten** from alpha |
| Both edit the same file | **conflict**; both sides keep their own | alpha's version wins, silently | **conflict**; both sides keep their own | alpha's version wins, silently |
| Alpha deletes a file | → beta | → beta | → beta | → beta |
| Beta creates a new file | kept | kept | kept | **deleted** |
| Beta deletes a file | → alpha | → alpha | restored from alpha | restored from alpha |

Three things in that table surprise people:

**`one-way-conflict` is not "ignore beta".** It refuses to overwrite
anything beta changed, and *tells you* — a file edited on beta is
reported as a conflict every cycle until you resolve it. That is the
mode's whole point: alpha pushes outward, but never destroys work that
appeared on the far side. If you want beta's edits silently discarded,
you want `one-way-alpha`.

**`one-way-alpha` deletes files it has never seen.** Beta is made
*identical* to alpha, so logs, caches, and anything else generated on
beta are removed. Never point it at a directory the far side also
writes to.

**Deletions propagate in every mode**, including the one-way ones —
deleting on alpha deletes on beta. What varies is only the reverse
direction. (A deletion large enough to look like a vanished disk halts
the session instead; see the safety rules.)

The earlier spellings — `two-way-safe`, `two-way-resolved`,
`one-way-safe`, `one-way-replica` — are still accepted, so existing
configurations keep working.

#### Modes and fan-out

When one alpha fans out to several betas, each destination is its own
session, and the mode decides what happens when two betas change the
same file at once. Under `two-way-conflict`, whichever lands first reaches
alpha and the other session reports a conflict — both edits survive,
one needs a human. Under `two-way-alpha`, the second edit
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
overlap at all: the outer never scans, records, or writes into that
path. This is how you synchronize a project and ship its build output
somewhere else:

```toml
[groups.project]
alpha = "~/project"
mode = "two-way-conflict"
ignores = ["dist"]          # the outer session leaves it alone
betas = ["build.example.com"]

[groups.dist]
alpha = "~/project/dist"    # nested, but excluded above
mode = "one-way-alpha"
betas = ["web.example.com:/srv/www"]
```

Both run side by side: the first destination receives the project
without `dist`, the second receives `dist`. Remove the `ignores` line
and the configuration is refused again.

One thing an ignore does *not* protect against, and it is worth knowing
before you arrange it this way: **deleting the directory above an
ignored path takes the ignored path with it.** If `~/project` is
deleted, `dist` goes too, and the inner session then finds its root
missing. An ignore says which files synchronization carries, not which
files exist, and a deletion is an instruction about the directory —
obeying it halfway would leave a tree that is neither deleted nor
synchronized and that nothing can ever clear.

The inner session stops there rather than passing the loss on: a
missing source root is an error, so `web.example.com` keeps its copy
and waits for a person. That is the protection — not that the inner
tree cannot be deleted, but that its deletion never travels.

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

# Keep watching, like a one-group `watch`:
autobahn sync ~/project user@host:/srv/project --watch

# Mirror exactly, ignoring build artifacts:
autobahn sync ~/project host:/srv/project \
    --watch --mode one-way-alpha --ignore target --ignore '*.log'
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
  plus a mirroring mode must not empty the destination). This is also what
  stops a deletion from travelling through a nested session whose root was
  inside an ignored path.
- Ignored content is never **overwritten** — "do not synchronize this"
  cannot become "replace it with the peer's copy" — but it is removed
  along with a directory that is deleted around it. Content that could not
  be *read* blocks even that: nobody has seen what is there, so removing
  the directory around it is not a decision anyone made.

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
