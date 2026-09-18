# The log

The supervisor's log is the only account of a session that outlives the cycle it describes. A transient failure — staging not producing content, a connection dropping mid-frame — clears itself before anyone opens the status page, so the log is where the evidence has to be.

The login service writes it to `~/.autobahn/service.log`. `watch` writes the same lines to standard output when that is a file or a pipe, and shows the live display instead when it is a terminal (`watch --log` forces the lines).

Every line carries a local timestamp.

## Levels

```toml
log = "debug"     # quiet | normal (the default) | debug
```

| level | writes |
|---|---|
| `quiet` | Errors only. What a supervisor with a working fleet should produce. |
| `normal` | Errors, plus one line per cycle that changed something, and the changes to the set of conflicts and blocked paths. |
| `debug` | Everything above, plus the detail needed to explain a cycle after it has gone. |

`AUTOBAHN_LOG=debug` overrides the file for one run, and `watch --debug` does the same, so a level can be turned up while something is being chased without editing anything. An unknown level is refused at startup rather than ignored. Errors are never silenced: `quiet` is the floor, not a way to lose them.

## What `debug` adds

How long connecting took, how long each cycle took and what it moved, and — the one that matters when staging misbehaves — the path and content digest of anything that was asked for and did not arrive, and whether it was the same content as the cycle before. That last distinction is exactly what the staging-failure check turns on: the same content missing twice is a delivery failure, whereas different content is a file being rewritten.

A cycle that changed nothing and finished promptly stays silent even here. At a five-second interval an idle session would otherwise write seventeen thousand lines a day saying so, and the log rotates on size, so debug would evict the very evidence it was turned on to collect. A cycle that moved data, took longer than a second, or failed still gets its line.

## Rotation

Neither launchd nor systemd rotates the log, so autobahn does: when it reaches its cap the previous generation is kept beside it, and `clean` removes that previous generation. The live log is never removed while the supervisor is writing to it, but `clean` reports its size.

## See also

- [Alerts](./alerts.md) — being told, for the things that do not clear themselves
- [State](./state.md) — `clean`, and where the log lives
- [The shop](./shop.md) — the last few log lines, under the counter
