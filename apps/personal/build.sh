#!/bin/sh
# Autobahn Desk — a window over the fleet, for this machine only.
#
# Personal and unshipped: no Developer ID, no notarisation, no release
# asset, no mention in docs/. An ad-hoc signature is enough for a bundle
# that only ever arrives by scp or by autobahn itself, neither of which
# quarantines anything. A downloaded copy would be refused, and should be.
#
#   apps/personal/build.sh            # builds "Autobahn Desk.app"
#   open "apps/personal/Autobahn Desk.app"
#
# The binary goes to target/desk, never target/release: the login service
# runs the latter through a symlink, and a personal build must not replace
# what supervises the fleet.
set -eu
cd "$(dirname "$0")/../.."

TARGET="${AUTOBAHN_DESK_TARGET:-target/desk}"
APP="apps/personal/Autobahn Desk.app"

cargo build --release --features desk --target-dir "$TARGET"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$TARGET/release/autobahn" "$APP/Contents/MacOS/autobahn-desk"

# The bundle runs the window directly: `desk` is hidden from --help, and
# this is the only thing that should ever pass it.
cat > "$APP/Contents/MacOS/Autobahn Desk" <<'LAUNCH'
#!/bin/sh
exec "$(dirname "$0")/autobahn-desk" desk "$@"
LAUNCH
chmod +x "$APP/Contents/MacOS/Autobahn Desk"

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
  <key>CFBundleName</key><string>Autobahn Desk</string>
  <key>CFBundleDisplayName</key><string>Autobahn Desk</string>
  <key>CFBundleIdentifier</key><string>party.voltai.autobahn.desk</string>
  <key>CFBundleExecutable</key><string>Autobahn Desk</string>
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
