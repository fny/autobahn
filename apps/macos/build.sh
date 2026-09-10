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
# Signed with the best identity in the keychain. An unsigned bundle is
# refused a notification identity altogether, which is what the bundle is
# for; ad-hoc earns one but is anonymous, so macOS treats every rebuild as
# an unrelated app and the notification permission is asked for again.
# A real certificate keeps that grant across builds.
IDENTITY="${AUTOBAHN_SIGN_IDENTITY:-}"
if [ -z "$IDENTITY" ]; then
    for want in "Developer ID Application" "Apple Development"; do
        found=$(security find-identity -v -p codesigning 2>/dev/null |
                grep -o "\"$want: [^\"]*\"" | head -1 | tr -d '"') || true
        [ -n "$found" ] && { IDENTITY="$found"; break; }
    done
fi
if [ -n "$IDENTITY" ]; then
    # The hardened runtime and a secure timestamp are what notarising
    # later requires, and cost nothing now.
    codesign --force --deep --options runtime --timestamp \
             --sign "$IDENTITY" "$APP" 2>/dev/null ||
    codesign --force --deep --sign "$IDENTITY" "$APP"   # no network for the timestamp
    echo "signed as: $IDENTITY"
else
    codesign --force --sign - "$APP"
    echo "signed ad-hoc (no certificate in the keychain)"
fi
# Registered with Launch Services, so the identity resolves before the app
# has ever been opened from the Finder.
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
  -f "$APP" 2>/dev/null || true
echo "built $APP"
echo "  open $APP        # or drag it to /Applications"
