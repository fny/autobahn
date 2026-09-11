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

# Built into its own target directory, never the default one. The login
# service runs the binary at target/release through a symlink, and a
# feature build into the same place silently replaced it — a superset, so
# nothing broke, but the service was one restart away from running
# whatever the last unrelated build left there. Same fragility as a stale
# agent bundle, and the same fix: separate outputs, nothing shared.
TARGET="${AUTOBAHN_TRAY_TARGET:-target/tray}"
CARGO_TARGET_DIR="$TARGET" cargo build --release --features tray
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp apps/macos/Info.plist "$APP/Contents/Info.plist"
cp "$TARGET/release/autobahn" "$APP/Contents/MacOS/autobahn"
# The icon is compiled from assets/Autobahn.icon by Xcode's own asset compiler,
# exactly as Xcode would build it: Assets.car carries the full Icon
# Composer rendering (light, dark and tinted, with the glass) that macOS
# 26 draws, and Autobahn.icns is the flat fallback older systems use.
# Without Xcode, the committed approximation stands in.
if xcrun --find actool >/dev/null 2>&1; then
    xcrun actool assets/Autobahn.icon --compile "$APP/Contents/Resources" \
          --app-icon Autobahn --platform macosx --minimum-deployment-target 11.0 \
          --output-partial-info-plist "$(mktemp)" >/dev/null
    ICON_NAME=Autobahn
else
    echo "no Xcode: using the approximated assets/autobahn.icns" >&2
    cp assets/autobahn.icns "$APP/Contents/Resources/autobahn.icns"
    ICON_NAME=autobahn
fi
/usr/libexec/PlistBuddy -c "Set :CFBundleIconFile $ICON_NAME" "$APP/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Add :CFBundleIconName string $ICON_NAME" "$APP/Contents/Info.plist" 2>/dev/null ||
/usr/libexec/PlistBuddy -c "Set :CFBundleIconName $ICON_NAME" "$APP/Contents/Info.plist"
# macOS tags a copied executable with com.apple.provenance, and codesign
# refuses a bundle carrying one — with errSecInternalComponent, which
# says nothing about attributes and sends you looking at the key instead.
# Signing the same binary outside a bundle succeeds, which is what makes
# it so misleading.
xattr -cr "$APP"
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
    # No --deep: Apple deprecated it, and on a bundle whose only code is
    # the executable it produces a signature claiming resources that are
    # not there. The fallback drops the timestamp, which needs the network.
    codesign --force --options runtime --timestamp \
             --sign "$IDENTITY" "$APP" 2>/dev/null ||
    codesign --force --options runtime --sign "$IDENTITY" "$APP"
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
