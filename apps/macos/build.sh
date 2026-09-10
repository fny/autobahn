#!/bin/bash
# Assembles Autobahn.app around the tray build.
#
# The app exists for one thing the command line cannot have: an identity.
# macOS attaches a notification's icon to the bundle that sent it, and a
# bare executable has no bundle — which is why `-appIcon` is ignored and
# every alert wore a terminal's icon. Inside this bundle the icon is
# autobahn's, and the menu bar app stops borrowing someone else's name.
#
# It wraps the same binary the terminal runs: the app is a way to launch
# `autobahn tray`, not a second implementation of it.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
APP="${1:-apps/macos/Autobahn.app}"

cargo build --release --features tray
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp apps/macos/Info.plist "$APP/Contents/Info.plist"
cp target/release/autobahn "$APP/Contents/MacOS/autobahn"
cp assets/autobahn.icns "$APP/Contents/Resources/autobahn.icns"
# Ad-hoc signed: unsigned bundles are refused a notification identity, and
# a self-signature is enough for one that never leaves this machine.
codesign --force --sign - "$APP" 2>/dev/null || echo "note: could not sign; notifications may fall back" >&2
# Registered with Launch Services, so the identity resolves before the app
# has ever been opened from the Finder.
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
  -f "$APP" 2>/dev/null || true
echo "built $APP"
echo "  open $APP        # or drag it to /Applications"
