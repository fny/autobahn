# Autobahn Tray (Experimental)

<picture><source media="(prefers-color-scheme: dark)" srcset="../assets/tray-bar-light.svg"><img src="../assets/tray-bar-dark.svg" alt="The menu bar item in its four states: idle, good, attention, bad" width="328"></picture>

Autobahn Tray is a tray (menu bar) item that shows you the real-time health of your sessions:

| Status Icon | Meaning |
| :--- | :--- |
| **Green** | All sessions synchronized |
| **Amber** | A conflict or blocked path needs you |
| **Red** | A session is halted, unreachable, or erroring |
| **Struck through (no dot)** | The supervisor isn't running |

The icon updates within 3 seconds.

## Menu Controls & Actions

Clicking the tray item provides visibility into your sync topology and allows direct intervention:

- **Topology & Status:** Lists every group and destination alongside its current state.
- **Conflict Resolution:** For conflicting files, you can immediately diff changes, keep the primary's version, keep the remote destination's version, or retain both.
- **Supervisor Control:** Start, stop, or restart the login background service, or inspect its active log file.
- **Config Warnings:** If the running supervisor rejects an edited configuration file, an inline alert appears in the menu and triggers a one-time desktop alert. Existing sessions continue under the last valid configuration (see [live reload](./configuration.md#live-reload-behavior)).

## Usage

### 1. Bundled with the Desktop App

The [Desktop App](./app.md) includes the menu bar item in the same unified process.

1. Open the **Service** pane.
2. Select **Menu Bar** (runs only the menu icon) or **Both** (runs the menu icon alongside the desktop window).

*Recommended if you already use the desktop app.*

### 2. Standalone Binary

If you only want a status light, without the desktop app's GUI framework (GPUI) or graphics overhead, compile the standalone `autobahn` binary with the `tray` flag.

#### macOS Application Bundle

Build the self-contained macOS `.app` bundle from source:

```sh
apps/tray/build.sh                  # Builds "Autobahn Tray.app"
open "apps/tray/Autobahn Tray.app"  # Launch directly, or move to /Applications
```

> **Why use the `.app` bundle?**
> macOS links desktop notification badges and icons to the originating app bundle. Running the raw executable directly causes notifications to display generic system or terminal icons instead of the Autobahn logo.

#### CLI Invocations

On both Linux and macOS, you can invoke the tray directly from your terminal. This requires a build compiled with `--features tray`.

```sh
autobahn tray --config path/to/config.toml --state-root ~/.autobahn
```

*Note: Omitted flags default to `AUTOBAHN_HOME` or `~/.autobahn`.*

## Notification Pipeline

Notifications respect the following precedence rules:

- **Default Behavior (No Hook):** The tray sends native desktop notifications following the standard engine rules (alerts require sustained failure states, only newly broken targets trigger notifications, cascades are grouped, and recoveries remain silent). See [Alerts](./alerts.md).
- **Custom Hook Active:** If an `on_alert` hook is defined, the tray suppresses internal notifications to prevent duplicated alerts.
- **Inside the Desktop App:** The tray owns the alert mechanism; the App's master notification toggle governs it directly. The App only raises notifications on its own behalf if the tray is inactive. See [the app documentation](./app.md#notifications).

## Autostart at Login

- **Tray Application:** Does not configure startup hooks automatically. Add it manually to your login items:
  - **macOS:** *System Settings → General → Login Items*
  - **Linux:** Add `autobahn tray` to your desktop environment's autostart list.
- **Supervisor Daemon:** Registered independently via `autobahn install`. The background sync service persists regardless of the tray's autostart state.

## Linux Support

While the desktop app's menu bar item comes with the Linux app on the releases page, the **standalone tray executable on Linux is experimental**:

> [!WARNING]
> **Use at your own risk.** Standalone Linux tray binaries are not distributed in official release tags, nor are they continuously verified in CI. Build failures or runtime regressions may occur without notice.

Notifications leverage standard Freedesktop specifications (`org.freedesktop.Notifications`), and file diffs launch via `xdg-open`.

### Prerequisites

- A functioning Rust toolchain (`rustup`).
- A system tray host supporting **AppIndicator** or **StatusNotifierItem** (native on KDE; GNOME requires the *AppIndicator and KStatusNotifierItem Support* shell extension).
- GTK 3 development headers (Debian/Ubuntu packages shown below):

```sh
sudo apt install libgtk-3-dev libxdo-dev libayatana-appindicator3-dev
```

### Build and Run

To prevent build artifacts from overwriting an active supervisor installation, isolate your target directory:

```sh
git clone https://github.com/fny/autobahn && cd autobahn
CARGO_TARGET_DIR=target/tray cargo build --release --locked --features tray
./target/tray/release/autobahn tray
```

### Known Linux Caveats

- **Missing Icon on Launch:** Certain tray libraries require GTK loop initialization explicitly bound to the event thread; some window managers may fail to display the indicator icon.
- **Dependency Overhead:** Enabling the `tray` feature pulls in GTK 3 and libnotify, which are absent from standard headless CLI builds (inspect `Cargo.lock` and `deny.toml` for details).

---

## See Also

- [Desktop App](./app.md): The desktop app and its built-in menu bar item
- [Alerts](./alerts.md): Notification rules and custom hooks
- [Conflicts](./conflicts.md): Conflict resolution actions available from the menu
- [Terminal Interface](./shop.md): Session monitoring and control in a terminal
- [Development](./development.md#the-menu-bar-app-bundle): How to build the tray bundle
- [Releases](./releases.md#signing-and-notarising-macos): macOS signing and notarization
