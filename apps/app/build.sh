#!/bin/sh
# Autobahn — a window over the fleet.
#
#   apps/app/build.sh            # builds "Autobahn.app"
#   open "apps/app/Autobahn.app"
#
# The signature here is ad-hoc, which is enough for a bundle that
# arrives by scp or by autobahn itself, neither of which quarantines
# anything. A release replaces it: apps/tray/release.sh --sign-only
# takes this bundle and signs it with the Developer ID, notarises it
# and staples the ticket, which is what a downloaded copy needs.
#
# The binary goes to target/app, never target/release: the login service
# runs the latter through a symlink, and a personal build must not replace
# what supervises the fleet.
set -eu
cd "$(dirname "$0")/../.."

TARGET="${AUTOBAHN_APP_TARGET:-target/app}"
APP="apps/app/Autobahn.app"

# The commit this build is made from, for the version a person reads. A
# workflow says which. Here git is asked, and a tree with changes in it
# says so. Without git the build names no commit.
AUTOBAHN_COMMIT="${AUTOBAHN_COMMIT:-}"
if [ -z "$AUTOBAHN_COMMIT" ] && command -v git >/dev/null 2>&1; then
    AUTOBAHN_COMMIT=$(git rev-parse --short=7 HEAD 2>/dev/null || true)
    if [ -n "$AUTOBAHN_COMMIT" ] && ! git diff --quiet HEAD 2>/dev/null; then
        AUTOBAHN_COMMIT="$AUTOBAHN_COMMIT, modified"
    fi
fi
export AUTOBAHN_COMMIT

# GPUI Kit wants a newer compiler than the repository's default.
cargo +1.98.0 build --release --features app --target-dir "$TARGET" --bin autobahn-app

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$TARGET/release/autobahn-app" "$APP/Contents/MacOS/autobahn-app"

# The command does not travel in here. The window shells out to
# `autobahn` for the work that is the command's — resolve, diff, clean
# — and a copy inside the bundle would be found before the installed
# one, because `looked_in` checks the executable's own directory first.
# That is the wrong way round: it would override the command the
# supervisor is actually running, on the machine of somebody who
# installed properly, and the mismatch it caused would be reported as
# a mismatch rather than as this. The welcome screen handles the
# machine that has none.

# No launcher script. CFBundleExecutable names the binary directly,
# because a shell script as a bundle's main executable cannot carry the
# hardened runtime: the process that starts is /bin/sh, which is
# Apple's, and notarisation refuses the arrangement. The script existed
# only to redirect to a differently named file, which the plist can do
# for nothing.

# The same icon the tray app wears, compiled the same way: Assets.car
# carries the Icon Composer rendering macOS 26 draws, and the committed
# .icns is the flat fallback for an older system or no Xcode at all.
if xcrun --find actool >/dev/null 2>&1 &&
   xcrun actool assets/Autobahn.icon --compile "$APP/Contents/Resources" \
         --app-icon Autobahn --platform macosx --minimum-deployment-target 11.0 \
         --output-partial-info-plist "$(mktemp)" >/dev/null 2>&1; then
    ICON_NAME=Autobahn
else
    rm -f "$APP/Contents/Resources/Assets.car" "$APP/Contents/Resources/Autobahn.icns"
    cp assets/autobahn.icns "$APP/Contents/Resources/autobahn.icns"
    ICON_NAME=autobahn
fi

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Autobahn</string>
  <key>CFBundleDisplayName</key><string>Autobahn</string>
  <key>CFBundleIdentifier</key><string>vip.faraz.autobahn</string>
  <key>CFBundleExecutable</key><string>autobahn-app</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleIconFile</key><string>$ICON_NAME</string>
  <key>CFBundleIconName</key><string>$ICON_NAME</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# Ad hoc, deliberately: see the note at the top. It is also what lets the
# window post notifications through the system's notification centre,
# which will not hear from a process that is not a signed bundle.
codesign --force --sign - "$APP" >/dev/null 2>&1 || true

echo "built $APP"
echo "  open \"$APP\""
