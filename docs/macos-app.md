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
running the sign is struck through — the mark a wifi icon uses for *off*
— and the dot is gone. Its ink is the menu bar's
own, read from the status item's button — which matters on macOS 26,
where the menu bar picks black or white from the wallpaper behind it, so
a light system over a dark wallpaper still has a white menu bar. When the
bar's ink changes, the sign follows on the next poll. And a menu with the detail:
each group, each destination with its state, and under each conflict the
ways to settle it (show the diff; keep alpha's, keep that destination's,
keep both), which run the same `resolve` a terminal would. The menu also
starts, stops, and restarts the login service and opens its log.

Choices are queued, not run where you click. They go to a worker thread
and run in the order you made them, and the menu says how many are
waiting. So a second choice made while the first is still running is
kept rather than lost, and a slow resolve cannot freeze the menu bar.

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

The app's icon is `assets/Autobahn.icon`, an Icon Composer bundle. Edit it in Icon Composer (it ships inside Xcode), and
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

A downloaded app has to be signed with a Developer ID certificate and
notarised — scanned by Apple, with the verdict stapled inside the bundle
so Gatekeeper trusts it offline. A copy that arrives by `scp`, or through
autobahn itself, is never quarantined and needs none of that.

`apps/macos/release.sh` does the whole thing, on a laptop or in CI:

```sh
apps/macos/release.sh     # build, sign, notarise, staple
```

On a laptop it signs with the Developer ID certificate in your keychain
and notarises with credentials stored once:

```sh
xcrun notarytool store-credentials autobahn \
    --apple-id you@example.com --team-id TEAMID --password <app-specific>
```

### In CI

Pushing a `v*` tag runs `.github/workflows/release.yml`, whose `mac` job
builds and signs everything macOS on one runner: the two command-line
binaries, signed and notarised by `apps/macos/notarize-cli.sh`, and the
app, by the same `release.sh` as above, attached to the release as
`Autobahn-macos-aarch64.zip`. It is the only job holding the certificate,
and it uses the protected `release` environment, which must hold five
secrets:

| secret | what it is |
|---|---|
| `DEVELOPER_ID_P12` | the Developer ID Application certificate and its private key, exported as a `.p12` and base64-encoded |
| `DEVELOPER_ID_P12_PASSWORD` | the password the `.p12` was exported with |
| `NOTARY_API_KEY` | an App Store Connect API key, the contents of its `AuthKey_….p8` file |
| `NOTARY_KEY_ID` | that key's ID — the part of the filename after `AuthKey_` |
| `NOTARY_ISSUER_ID` | the issuer ID shown above the key list in App Store Connect → Users and Access → Integrations |

The certificate can sign anything as you, so it is kept where it can do
the least harm:

- The environment requires approval, so a release waits for you before
  any secret reaches a runner.
- Pull requests from forks never receive secrets, and the job runs only
  on tags, which only people with write access can push.
- The certificate goes into a throwaway keychain that is deleted when the
  job ends, whether it passed or failed.

If the certificate ever leaks, revoke it in your Apple Developer account.

The app is Apple Silicon only; the command-line binaries cover Intel as
well. A command-line binary cannot be stapled, so Gatekeeper checks its
notarisation online the first time it runs. The runner's default
Xcode may be older than 26, whose `actool` is the only one that compiles
the Icon Composer bundle; the job picks Xcode 26 when the runner has it,
and otherwise `build.sh` uses the committed `assets/autobahn.icns`, which
is the same icon without the macOS 26 variants.

## A guided tour

`scripts/mi` runs a guided tour of all of this against throwaway
directories — every command, and every state a session can report,
printed as the binary actually produces them.

## See also

- [Alerts](./alerts.md) — the hook, which is where the rules live
- [The shop](./shop.md) — the terminal counterpart
- [Conflicts](./conflicts.md) — what the menu's resolve items do
