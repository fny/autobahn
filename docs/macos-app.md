# The menu bar app

```sh
apps/macos/build.sh          # builds Autobahn.app
open apps/macos/Autobahn.app # or drag it to /Applications
```

The app is a way to launch `autobahn tray`, not a second implementation
of it: the same binary, the same `resolve` a terminal would run. What the
bundle adds is an *identity*. macOS attaches a notification's icon to the
bundle that sent it, and a bare executable has none — which is why
`-appIcon` is ignored from the command line and every alert wears the
icon of whatever ran it. Inside the bundle the icon is autobahn's.

## What it shows

The Autobahn sign — two lanes to the horizon under a bridge — drawn in
the menu bar's own ink, black or white, with the state of every session
in a dot at its corner: green when all are synchronized, amber when any
is in conflict, red when any is halted or unreachable. When nothing is
running the sign fades and the dot is gone. It follows the system
appearance, so a switch between light and dark redraws it on the next
poll. And a menu with the detail:
each group, each destination with its state, and under each conflict the
ways to settle it (show the diff; keep alpha's, keep that destination's,
keep both), which run the same `resolve` a terminal would. The menu also
starts, stops, and restarts the login service and opens its log.

When no `on_alert` hook is configured, the tray raises desktop
notifications itself, under exactly the rules the hook would use — a
condition must hold before it counts, only something *joining* the set
in trouble is news, a cascade is gathered into one, and recovery is
silent. See [Alerts](./alerts.md) for the rules. When a hook *is*
configured, the tray stays quiet: the hook is the one place
notifications come from, and two sources with identical rules would
still mean everything twice.

It is a view over `status --json`, polled every few seconds, and holds
no state of its own. macOS and Linux (with a system tray).

## Starting it

Open `apps/macos/Autobahn.app`, or drag it to `/Applications` and open it
there. **It does not start at login on its own**: add it under System
Settings → General → Login Items. The supervisor is separate and already
survives logout through `autobahn install`; the app only watches it.

From a terminal, `autobahn tray` runs the same menu bar app — but only in
a binary built with `--features tray`. A plain build answers that it has
no menu bar app. `build.sh` builds that binary into `target/tray`
(`AUTOBAHN_TRAY_TARGET` moves it), never `target/release`: the login
service runs `target/release/autobahn` through a symlink, and an app build
must not replace it.

## The icon

The app's icon is `Autobahn.icon`, an Icon Composer bundle at the root of
the repository. Edit it in Icon Composer (it ships inside Xcode), and
`build.sh` compiles it with Xcode's asset compiler, `actool`, exactly as
Xcode would: the bundle gets `Assets.car`, carrying the light, dark and
tinted variants macOS 26 draws, and `Autobahn.icns` as the flat fallback
older systems use. Without Xcode, `build.sh` falls back to the committed
`assets/autobahn.icns`.

`scripts/build-icon.sh` regenerates the committed files from the bundle:
`assets/autobahn.icns` and `assets/notification.png` — the icon every
alert wears, embedded in the binary — both from `actool`, and
`assets/autobahn.png`, the artwork at 1024 pixels. That last one comes
from Icon Composer's exporter, `ictool`, and is full bleed: the squircle
runs edge to edge, which suits a README or a website but is about a
quarter larger than an app icon should be. Rerun the script after
changing the bundle.

The menu bar glyph is not this icon. It is the Autobahn sign, drawn in
code in `src/tray.rs`; `assets/sign.svg` is the same shape at full size.

## Signing and notarising

`release.sh` is the other half, and only for an app someone downloads:
it signs with a Developer ID certificate, sends the result to Apple to be
scanned, and staples the verdict to the bundle so Gatekeeper trusts it
offline. A copy that arrives by `scp`, or through autobahn itself, is
never quarantined and never needs any of that.

Before the first release, store the notarisation credentials once:

```sh
xcrun notarytool store-credentials autobahn \
    --apple-id you@example.com --team-id TEAMID --password <app-specific>
```

## A guided tour

`scripts/mi` runs a guided tour of all of this against throwaway
directories — every command, and every state a session can report,
printed as the binary actually produces them.

## See also

- [Alerts](./alerts.md) — the hook, which is where the rules live
- [The shop](./shop.md) — the terminal counterpart
- [Conflicts](./conflicts.md) — what the menu's resolve items do
