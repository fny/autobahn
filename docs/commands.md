# Commands

## Create a Configuration

```sh
autobahn init                     # write ~/.autobahn/config.toml
autobahn init --config ./try.toml # ...or somewhere else
autobahn init --force             # replace one, keeping the old beside it
```

`init` writes the defaults, comments that explain each mode, and a commented example group. No sessions start until you edit the file.

An existing configuration requires `--force` to replace it. The command keeps the previous file as `config.toml.bak` and reads the new file back before reporting success.

## Inspect and Control Sessions

These commands work with a foreground `watch` supervisor or the login service:

```sh
autobahn status            # what every session is doing, or last did
autobahn status work       # ...filtered to one group
autobahn status .          # ...to whatever syncs the working directory
autobahn status ~/Workspace  # ...or any folder inside a synchronized root
autobahn status --conflicts   # list every conflicting path, not a count
autobahn status --live     # ...repainting, as it happens (Ctrl-C leaves)
autobahn status --json     # the same, as one versioned document

autobahn sync              # one pass over every session, then exit
autobahn flush             # sync everything right now
autobahn doctor project    # look, change nothing: both sides, how they
                           # differ, the baseline, and what a reset would do
autobahn reset project     # forget the baseline; next cycle merges both
                           # sides additively (resurrects deletions)
autobahn verify project    # next cycle re-reads every byte, catching
                           # content whose metadata never moved
autobahn clean --dry-run   # what state belongs to sessions no longer
autobahn clean             # in the config; then remove it
autobahn clean --agents    # also prune superseded agents on remote hosts

autobahn mi                # the shop: watch it work, and clear the queue
                           # (? explains every word on the screen)
```

`reset` requires a group name and discards its baseline. The next cycle merges both sides additively, which can restore deleted files.

Before a reset, run `doctor`. It scans both sides and reports whether they match and what a reset will copy. It changes neither the folders nor the baseline and can run beside the supervisor. If both sides match, a reset requires no copies.

See [State](./state.md) for `clean` and [Conflicts](./conflicts.md) for conflict commands.

## Disable and Enable Synchronization

```sh
autobahn disable --host lager    # off everywhere it appears
autobahn enable  --host lager
autobahn disable --group lack    # the whole group, sessions and all
autobahn enable  --group lack
```

These commands edit `~/.autobahn/config.toml` and preserve comments. A host enters or leaves `disabled_hosts`. A group gains or loses `disabled = true`.

An unknown name produces an error with valid names. Disabling preserves session state, so enabling resumes the session. The supervisor normally applies the edit within a few seconds. See [Live reload](./configuration.md#live-reload-behavior).

## Manage the Service and Updates

```sh
autobahn install           # register the supervisor as a login service
autobahn start             # start the registered service
autobahn stop
autobahn restart
autobahn uninstall         # stop it and unregister it
```

`start` and `restart` validate the configuration before changing the service. They reject unknown keys, unsupported modes, and groups without sessions. If validation fails, the service remains unchanged.

A running supervisor performs the same checks on configuration edits. With live reload enabled, edits require no restart. An upgrade requires a restart.

```sh
autobahn update            # install the latest release over this one
autobahn update --dry-run  # ...or just say what it would install
```

See [Releases](./releases.md) for how updates work.

## What `status` Shows

After a session works for five seconds, `status` shows its phase, elapsed time, and available progress:

```
~/Workspace werk
  dev@halle.steinbach.de:~/Workspace
    status: scanning, 6m20s elapsed
      primary: 412,331 of ~1,470,000 entries (28%), about 14m left
      replica: scanning for 6m20s
    mode: two-way-conflict
```

The five-second threshold covers the whole run, across phases. Brief routine cycles do not display a phase line.

An estimate requires a known total and enough elapsed time to measure a rate. A first scan without a known total shows counts and elapsed time only.

After half a second, a remote scan reports its entry count every half second. It provides no estimate because the controller does not know its total.

Between cycles, status describes the last cycle’s outcome. It also identifies paused sessions and sessions in error backoff.

Commands that contact the supervisor send their build identity. A supervisor with a different build rejects the request. Status then explains the mismatch and shows the last recorded state. After a manual upgrade, run `autobahn restart`. `autobahn update` restarts the service itself.

A group with all destinations synchronized and no outstanding work appears on one line:

```
~/Workspace/notes notes  ✓ 2 synchronized · last cycle 4s ago
```

Conflicts, blocked paths, halts, unreachable hosts, pauses, and long scans expand the group. `status <group>` always expands the selected group. `status --all` expands every group. This compact display does not affect `--json`.

## `--live`

`autobahn status --live` refreshes twice a second and shows every phase, including brief ones. It reads an existing supervisor. `watch` uses the same display and also runs synchronization.

Both displays support these controls:

| Keys                   | Action                       |
| ---------------------- | ---------------------------- |
| Arrows or `j`/`k`      | Move one line                |
| PgUp/PgDn or space/`b` | Move one screen              |
| `g`/`G` or Home/End    | Move to the beginning or end |
| `q` or Ctrl-C          | Leave                        |

A footer shows the current position. The display continues to refresh during scrolling. Exiting preserves terminal scrollback.

## Session States

If a cycle cannot run, status reports:

| State | Meaning |
| --- | --- |
| `unreachable` | The host does not answer. This usually clears without intervention. |
| `halted` | A safety check stopped the session. Follow the reported instruction. A missing primary folder clears automatically when the folder returns. |
| `errored` | Another error stopped the cycle. Read the message and [log](./logging.md). |

If the cycle runs but cannot synchronize every path, status reports:

| State | Meaning |
| --- | --- |
| `conflicts` | Both sides changed a path. Choose a resolution. |
| `blocked` | A path cannot be read or written. Correct the filesystem problem. |

Conflicts and blocked paths can occur together. Status reports both counts. See [Safety](./safety.md) for halt conditions.

## `--json`

`autobahn status --json` and `autobahn conflicts --json` produce one versioned document for scripts and interfaces. The current `version` is 4.

Version 3 added `config_notice`, present while the supervisor rejects a configuration edit. Version 4 added `supervisor_mismatch`, present while the supervisor uses another build. Each session has a `progress` object while a supervisor runs.

`--filter` applies to JSON. `--depth` affects the list display only.

Session alerts receive this same document. Configuration-refusal alerts receive the notice object described in [Alerts](./alerts.md). The [terminal interface](./shop.md), [Desktop App](./app.md) and [the menu bar item](./tray.md) use status data.

## One-off Syncs and Scripting

`sync` runs a pairing once and exits, or runs every configured session when no roots are supplied. Without `--watch`, it registers no local or remote filesystem watchers.

For an already synchronized 160,000-file pair, a recorded one-off run took 0.7 s locally and 1.4 s over SSH.

```sh
# One bidirectional pass, then exit:
autobahn sync ~/Workspace /Volumes/Backup/Workspace

# Local ↔ remote over SSH:
autobahn sync ~/Workspace dev@build.audi.de:/home/dev/workspace

# Every session in the configuration, once each:
autobahn sync

# Keep watching, like a one-group `watch`:
autobahn sync ~/Workspace dev@build.audi.de:/home/dev/workspace --watch

# Mirror exactly, ignoring build artifacts:
autobahn sync ~/Workspace build.audi.de:/home/dev/workspace \
    --watch --mode one-way-primary --ignore target --ignore '*.log'
```

A completed command returns one of these exit codes. Across multiple sessions, `1` takes precedence over `2`, which takes precedence over `0`.

| Code | Meaning |
| --- | --- |
| `0` | Every session converged, with no conflicts and no blocked paths. |
| `1` | An error stopped a session: unreachable, halted, a bad configuration. |
| `2` | Every session finished its pass, but conflicts or blocked paths remain. |

One-off runs share session state with the supervisor for the same roots. Deletions and conflicts propagate across runs, and interrupted transfers resume. An exclusive session-state lock prevents a one-off command and supervisor from operating on the same pair concurrently.

## See Also

- [Configuration](./configuration.md): Configuration files, groups, and session settings
- [Conflicts](./conflicts.md): The `issues`, `conflicts`, `diff`, and `resolve` commands
- [State](./state.md): Session baselines, cleanup, and stored files
- [Logging and Diagnostics](./logging.md): Log locations and verbosity controls
- [Terminal Interface](./shop.md): Interactive session control with `autobahn mi`
- [Desktop App](./app.md): Session control through the desktop app
- [Alerts](./alerts.md): Notifications and custom hooks
- [Releases](./releases.md): Updates, release contents, and signature verification
