# Alerts

A supervisor running as a login service is invisible by design, which
means a conflict or a permission that stopped working sits there with
nobody told. `on_alert` runs a command when that happens:

```toml
on_alert = "terminal-notifier -title autobahn -appIcon \"$AUTOBAHN_ICON\" \\
            -subtitle \"$AUTOBAHN_DETAIL\" -message \"$AUTOBAHN_SUMMARY\" \\
            -execute \"$AUTOBAHN_OPEN\""
```

That is the whole of it. It sits at the top level of the config because
it is the one thing about alerting anyone should have to write.

`on_alert` is the only hook. Which states are alerting is in the summary
it is handed, not in which hook is chosen — a hook per state only moved
the branching out of the command and into the config, and every state
ends the same way, with someone opening a terminal.

The service runs under launchd or systemd with a sparse environment, so
give commands absolute paths — and on Linux, `notify-send` needs
`DBUS_SESSION_BUS_ADDRESS`.

## What the hook receives

| Variable | What it holds |
|---|---|
| `$AUTOBAHN_SUMMARY` | One line. The whole story when one thing is wrong, a count when several — `voltai → fny: 1 conflict`, `boite is unreachable — 5 groups paused`, `2 groups need you, 1 host away`. |
| `$AUTOBAHN_DETAIL` | One indented line per thing, for a notifier that shows more than a headline. |
| `$AUTOBAHN_OPEN` | A ready command that opens [the shop](./shop.md) on the full detail. For `terminal-notifier -execute` and tray items — a notification holds one line, and this is where it leads. |
| `$AUTOBAHN_ICON` | Absolute path to autobahn's icon, written into the state directory so a notifier can point at it. |
| `$AUTOBAHN_STATES` | Comma-separated state names present. |
| `$AUTOBAHN_ALERT_COUNT` | How many sessions are in the set. |
| `$AUTOBAHN_EVENT` | `alert` or `repeat`. |
| stdin | The full `status --json` document. |

Hooks run off the cycle and cannot affect or delay synchronization: a
hook is killed if it outstays its timeout, and is skipped while a
previous one is still running.

## The five states, and how long each must hold

A condition must hold before it counts, and how long is built in and
tuned per state, because a sleeping laptop and a safety halt do not
deserve the same patience:

| state | holds for | why |
| --- | --- | --- |
| `halted` | 0s | a safety halt is never transient |
| `conflicts`, `blocked` | 30s | needs a person, but not this second |
| `errored` | 2m | transient failures heal in a cycle or two |
| `unreachable` | 5m | a sleeping laptop is the common case |

What each state means is in [Commands](./commands.md#what-a-sessions-state-means).

## When it fires

Six rules make it usable rather than maddening:

- **Nothing runs while everything is healthy.** Silence is the normal
  state.
- **Nothing runs when it clears, either.** An all-clear asks for no
  action, and a stream of notifications that ask for nothing is what
  teaches you to stop reading the ones that do.
- **A condition must hold without a break.** A wifi handover that takes
  every session unreachable for eight seconds is never mentioned — it
  fixed itself. Lapse for one cycle and the clock restarts, so a host
  flapping just under the threshold never crosses it.
- **An alert fires when something *joins* the set of sessions in
  trouble**, never on repetition, and never on recovery. A cascade
  coming back one host at a time used to notify on the way up as loudly
  as on the way down; a session that recovers and fails again inside the
  same episode is the trouble already reported, not new trouble.
- **A cascade is held and reported once.** A closing laptop does not take
  its sessions together: each goes when its own connection times out,
  seconds apart, so every arrival changed the set and every change was
  news. The window (60 seconds) runs from the first arrival nobody has
  been told about, not from the latest, so a steady trickle cannot hold
  the notification back indefinitely.
- **Trouble that comes and goes is reported once.** A conflict on a file
  two machines are both editing appears, clears, and returns all day.
  Everything must stay clear for 15 minutes before a return counts as
  news rather than as the same trouble continuing — otherwise one
  flapping session is a notification a minute.

## `[advanced.alerts]`

The alerter's timing. These are not preferences — they are the values
that make it correct, and there is no second right answer a reader would
discover by trying. The section exists so that finding yourself in it is
itself the message.

| Key | Default | What it governs |
|---|---|---|
| `alert_after` | 30s | How long a condition must hold before it counts. Written here, it replaces the whole per-state table rather than sitting behind it. |
| `[advanced.alerts.after]` | see above | Per-state hold times, keyed by state name. Overrides the built-in table one state at a time. |
| `coalesce_after` | 60s | How long a grown set is held so a cascade arrives as one notification. |
| `settle_after` | 15m | How long everything must stay clear before trouble returning counts as news. |
| `repeat_after` | never | Re-fire an unchanged set. Off, and usually should be: a notification that returns while you are already working on it teaches you to ignore it. |
| `timeout` | 30s | How long the hook may run before it is killed. |

A configuration written against the old `[alerts]` section is told where
each key went rather than refused as an unknown field.

## See also

- [Configuration](./configuration.md) — the top-level keys
- [The shop](./shop.md) — what `$AUTOBAHN_OPEN` opens
- [The menu bar app](./macos-app.md) — an icon in the colour of the worst session
- [The log](./logging.md) — the evidence, for the alerts that clear themselves
