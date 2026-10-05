# Logging and Diagnostics

Autobahn records supervisor operations and diagnostic traces to capture transient events, connection changes, and error conditions.

## Log Locations & Output Streams

- **Background Service:** When running as a login service (`launchd` or `systemd`), logs are written to `~/.autobahn/service.log`.
- **Interactive Foreground (`watch`):** Outputs a terminal status UI when connected to an interactive TTY. When redirected to a file or pipe, it emits timestamped text logs. Use `autobahn watch --log` to force plain text logging in a terminal.

## Log Levels

Configure verbosity via `~/.autobahn/config.toml` or override at runtime:

```toml
# ~/.autobahn/config.toml
log_level = "normal"  # "quiet" | "normal" (default) | "debug"
```

```sh
# Runtime overrides
AUTOBAHN_LOG=debug autobahn watch
autobahn watch --debug
```

### Verbosity Definitions

| Level | Events Logged |
| :-- | :-- |
| `quiet` | Errors and critical safety halts only. |
| `normal` _(default)_ | Errors, summary lines for cycles that transferred data, and updates to the conflict/blocked path queues. |
| `debug` | Detailed stage timings, SSH connection durations, digest verifications, and staging retry traces. |

## Log Rotation and Retention

Autobahn automatically manages log rotation:

- When `service.log` reaches its size threshold, it is rotated to `service.log.1`.
- Running `autobahn clean` purges archived log files while preserving the active log.

## See Also

- [Commands](./commands.md): Status inspection and session diagnostics
- [Configuration](./configuration.md#top-level-settings): The `log_level` setting
- [State](./state.md): Log locations and cleanup behavior
- [Desktop App](./app.md): The Log pane
- [Alerts](./alerts.md): Notifications for errors, conflicts, and recovery
