# Alerts (Experimental)

Autobahn provides an event-driven notification hook, `on_alert`, to notify users about errors, conflicts, and resolutions.

Define `on_alert` at the top level of `~/.autobahn/config.toml`:

```toml
# Inline
on_alert = 'terminal-notifier -title autobahn -appIcon "$AUTOBAHN_ICON" -subtitle "$AUTOBAHN_DETAIL" -message "$AUTOBAHN_SUMMARY"'

# As a script
on_alert = "~/.autobahn/on-alert.sh"
```

The hook inherits the supervisor’s environment. A foreground `autobahn watch` passes its terminal environment, including exported credentials and tokens. Do not run an untrusted hook from a shell that contains secrets.

On macOS, the service inherits launchd basics such as `HOME`, `USER`, and `TMPDIR`. It uses `PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin` and the `AUTOBAHN_HOME` recorded at installation.

On Linux, it inherits the systemd user manager’s environment, including `HOME`, a default `PATH`, `XDG_RUNTIME_DIR`, and imported variables. It also receives the installed `AUTOBAHN_HOME`.

## Hook Environment Variables

When `on_alert` is executed, Autobahn populates the following environment variables:

| Variable | Description | Example Content |
| :--- | :--- | :--- |
| `$AUTOBAHN_SUMMARY` | High-level summary of active issues | `"myproject → remotehost: 1 conflict"` |
| `$AUTOBAHN_DETAIL` | Indented per-session diagnostic details | `"src/app.rs (content mismatch)"` |
| `$AUTOBAHN_ICON` | Absolute path to the Autobahn application icon | `"/Users/user/.autobahn/icon.png"` |
| `$AUTOBAHN_STATES` | Comma-separated list of alerting states | `"conflicts"`, `"halted"`, or `"config"` |
| `$AUTOBAHN_ALERT_COUNT` | Total number of sessions currently in an alerting state | `"2"` |
| `$AUTOBAHN_EVENT` | Trigger event type | `"alert"`, `"repeat"`, or `"config"` |
| Standard Input (`stdin`) | Full JSON diagnostic payload | Complete output of `autobahn status --json` |

Hooks execute asynchronously and do not block synchronization cycles. Any hook exceeding the configured execution timeout (30 seconds default) is terminated.


## State Hold Durations

To avoid false alarms from transient network interruptions or brief locks, conditions must persist continuously for a specific duration before triggering an alert:

| State | Default Hold Duration | Rationale |
| :--- | :---: | :--- |
| `halted` | `0s` *(immediate)* | Safety halts (damaged ancestor, conflicting roots) are non-transient. Exception: a missing primary root waits 2m to accommodate drive remounts. |
| `conflicts`, `blocked` | `30s` | Requires human intervention, but allows brief window for automated tooling or manual resolution. |
| `errored` | `2m` | Allows transient filesystem or connection errors to heal automatically. |
| `unreachable` | `5m` | Accommodates routine laptop sleep or brief network re-connections. |


## Coalescing and Anti-Flap Behavior

1. **No Alert on Resolution:** The hook fires only when sessions enter a failure state, never on recovery.
2. **Cascade Coalescing (`coalesce_after = 60s`):** When multiple sessions fail in close succession (e.g., a host disconnects multiple groups), notifications are coalesced into a single consolidated alert.
3. **Flap Suppression (`settle_after = 15m`):** If an issue resolves and reoccurs within 15 minutes, it is treated as part of the existing incident to prevent notification spam.

## Timing Overrides (`[experimental.alerts]`)

Tune alert thresholds by configuring `[experimental.alerts]` in `config.toml`:

```toml
[experimental.alerts]
alert_after    = "30s"   # Global hold duration override
coalesce_after = "60s"   # Multi-event aggregation window
settle_after   = "15m"   # Incident closure threshold
timeout        = "30s"   # Script execution timeout
```
