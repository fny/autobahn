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
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# Ad hoc, deliberately: see the note at the top.
codesign --force --sign - "$APP" >/dev/null 2>&1 || true

echo "built $APP"
echo "  open \"$APP\""
