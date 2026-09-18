# Commands

Writing a configuration to start from:

```sh
autobahn init                     # write ~/.autobahn/config.toml
autobahn init --config ./try.toml # ...or somewhere else
autobahn init --force             # replace one, keeping the old beside it
```

It writes the defaults, every mode explained in a comment, and one example group that is commented out — so a fresh install describes nothing and starts nothing until you have edited it and meant it. It refuses to replace a configuration that already exists unless you pass `--force`, which keeps the previous file as `config.toml.bak`. Whatever it writes, it reads back before it reports success.

Asking a running supervisor things, whether it is `watch` or the login service:

```sh
autobahn status            # what every session is doing, or last did
autobahn status project    # ...filtered to one group
autobahn status .          # ...to whatever syncs the working directory
autobahn status ~/project  # ...or any folder inside a synchronized root
autobahn status --conflicts   # list every conflicting path, not a count
autobahn status --live     # ...repainting, as it happens (Ctrl-C leaves)
autobahn status --json     # the same, as one versioned document

autobahn sync              # one pass over every session, then exit
autobahn flush             # sync everything right now
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

`reset` requires the group name. A reset is deliberate, never a default. `clean` is described in [State](./state.md); the conflict commands in [Conflicts](./conflicts.md).

Running it as a service, and keeping it current:

```sh
autobahn install           # register the supervisor as a login service
autobahn start             # start it, stop it, or do both after an edit
autobahn stop
autobahn restart
autobahn uninstall         # stop it and unregister it

autobahn update            # install the latest release over this one
autobahn update --dry-run  # ...or just say what it would install
```

`install` is in [Configuration](./configuration.md); `update`, and what a release contains, in [Releases](./releases.md).

## What `status` shows

When a session has been working long enough that its silence would look like death — a cold sync, a first scan, an unreachable host — `status` says what it is doing, how long it has been at it, and, where the numbers allow an honest one, an estimate:

```
~/Workspace/Voltai voltai
  ubuntu@fny.voltai.party:~/Workspace
    status: scanning, 6m20s elapsed
      alpha: 412,331 of ~1,470,000 entries (28%), about 14m left
      beta: scanning for 6m20s
    mode: two-way-conflict
```

Routine cycles say nothing. They finish in well under a second, and a line that flickered into "scanning" every few seconds would report nothing while hiding what the reader came for — so a phase earns the line only after the session has been working for five seconds, counted across the whole run rather than restarted at each step.

The estimate is withheld unless the phase has been running long enough to have a rate and has a total to measure against — a first scan of a tree nothing has ever counted reports its progress and its elapsed time, and no estimate. A remote scan happens inside one request on the far side, so it reports that it is running and for how long, without counts.

A session between cycles is described by how its last cycle ended; so is a paused one, and one backing off from an error, both of which the recorded status names.

## `--live`

To watch it happen rather than sample it, `autobahn status --live` repaints twice a second and shows every phase however brief. It is a read-only window onto whatever supervisor is already running — the login service, or a `watch` in another terminal. (`watch` is the same display, but it also does the synchronizing.)

Both scroll, with the keys a pager has trained everyone to try: arrows and `j`/`k` by the line, PgUp/PgDn and space/`b` by the screen, `g`/`G` or Home/End for the ends, `q` to leave. A footer says where you are in the list. The content keeps refreshing underneath while you move around in it, and leaving — by `q` or Ctrl-C — gives the terminal back with the scrollback intact.

## What a session's state means

Two questions, not one word list. **Did the cycle run?** If it did not:

- `unreachable` — the host is not answering. Usually clears itself.
- `halted` — a safety refusal. Retrying will *never* clear it. See the [safety rules](./safety.md).
- `errored` — it failed for some other reason; the message is the evidence, and [the log](./logging.md) has the rest.

If it did run, the tree is in sync except for what the cycle could not carry:

- `conflicts` — both sides changed a path. You pick a winner.
- `blocked` — a path could not be read or written. You fix the filesystem.

Conflicts and blocked paths co-occur, so both counts are reported rather than one hiding the other.

## `--json`

`autobahn status --json` and `autobahn conflicts --json` print everything as one versioned document, for scripts and user interfaces. Each session carries a `progress` object while a supervisor is running. `--filter` applies to the JSON too, while `--depth`, being a way of reading a list, does not. The alert hook receives this same document on standard input, and [the shop](./shop.md) and [the menu bar app](./macos-app.md) read nothing else.

## One-off syncs and scripting

Underneath the supervisor sits a single-session command, useful for trying a pairing before committing it to the config, and for scripts that need a sync that converges and *exits* with a status code:

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

One-shots share session state with the supervisor (same roots → same session), so deletions propagate correctly across runs, conflicts are detected across runs, and interrupted transfers resume. A `sync` and a supervisor can never race the same pairing: each session's state is exclusively locked while it runs.

## See also

- [Conflicts](./conflicts.md) — `issues`, `conflicts`, `diff`, `resolve`
- [The shop](./shop.md) — `autobahn mi`
- [State](./state.md) — `clean`, and what lives in `~/.autobahn`
- [Alerts](./alerts.md) — being told without watching
- [Releases](./releases.md) — `update`, and what ships with a release
