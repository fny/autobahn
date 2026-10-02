# Autobahn Dash (experimental)

Dash is the desktop window over Autobahn's supervisor. It shows groups, hosts, conflicts, logs, service controls, and a configuration editor. It can also show the menu bar icon in the same process. The standalone [tray app](./macos-app.md) remains a separate build.

## Builds and startup

The `.github/workflows/dash.yml` workflow builds macOS Apple Silicon and Linux x86-64/arm64 archives and publishes successful platforms to the moving `dash-latest` prerelease. This is an experimental, unsigned channel, separate from the signed command-line releases. Check the workflow's release notes for the build commit and available platforms; a failed platform build may be absent.

On macOS, open `Autobahn Dash.app`. These test builds are not Developer ID signed or notarized; the workflow's release notes explain the quarantine step for a downloaded copy. On Linux, extract the archive and run `./autobahn-dash`, keeping its companion `autobahn` executable beside it. Linux needs a graphical session (Wayland or X11), a Vulkan driver, and the desktop libraries listed in the workflow. Building on Linux does not establish that every desktop's tray integration works.

Dash accepts `--config FILE` and `--state-root DIRECTORY`; otherwise it uses the default configuration and state root (`AUTOBAHN_HOME`, or `~/.autobahn`). It finds the CLI beside itself first, then in `~/.local/bin`, `/usr/local/bin`, `/opt/homebrew/bin`, and PATH. Keep the companion CLI matched to the running supervisor; a version mismatch is reported in status.

If no CLI is found, a welcome screen offers to run the embedded installer or copy the shell installation command. Installer output appears in `install.log` under the state root. Then configure a group before starting the service; see [Installation](../INSTALL.md).

## The panes

| Pane | What it does |
|---|---|
| Groups | Shows the configured groups, destinations, current work, and issues. |
| Hosts | Collects destinations by host and provides host controls. |
| Conflicts | Shows disagreements and diffs, with actions to keep a side or both copies. |
| Log | Reads the supervisor log, or installer output during setup. |
| Service | Shows the program and service state, with installation, start, stop, restart, and cleanup controls. |
| Config | Edits top-level settings, defaults, and groups; groups can be added, renamed, or removed. |

Conflict actions use the same CLI operations described in [Conflicts](./conflicts.md). The app does not merge file contents. A conflict may concern a file's executable bit, a symbolic link, or a directory even when there is no text diff to display.

## Editing configuration

Edits remain in the form until **Save**. The editor validates through Autobahn's configuration loader and reports errors at the affected fields; invalid configuration is not saved. **Reload** reads the file again and discards pending form changes. Optional switches preserve the distinction between inheriting a value and explicitly setting it.

Advanced fields fold away. Experimental controls become visible after five clicks on the Autobahn wordmark; their meanings and limits remain those in [Configuration](./configuration.md), [Alerts](./alerts.md), and [P2P](./p2p.md).

A running supervisor picks up a saved configuration through [live reload](./configuration.md#editing-it-while-it-runs). If live reload is disabled, restart it to apply the file.

## Window and menu bar

Choose a window, a menu bar item, or both in the Service pane. This machine's choice is saved as `presence = "window"`, `"menubar"`, or `"both"` in `dash.toml` under the state root; it is not part of the fleet configuration. The default is both. Notifications follow the [alert rules](./alerts.md), with a configured `on_alert` hook taking precedence over the app's notifications.

The supervisor is independent of the window. Register it with `autobahn install` or the service controls to start it at login. Add the app to your operating system's login/autostart settings if you want the interface to start too.

## Building it

See [Development](./development.md#desktop-app-and-shared-text) for the Rust toolchain, separate target directory, companion CLI, and macOS bundling script. `autobahn update` updates the CLI and agent bundle; obtain a new Dash build separately.
