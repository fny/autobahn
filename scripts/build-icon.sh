#!/bin/bash
# Renders the committed icon files from Autobahn.icon, exactly as Xcode
# builds it.
#
# Autobahn.icon is an Icon Composer bundle. This script used to imitate
# it with ImageMagick — a gradient, a blurred shadow, a rounded mask — so
# every change in Icon Composer had to be re-imitated here by hand. Now
# Xcode's own tools draw every file.
#
# Two tools, for two jobs, because they do not draw the same thing:
#
#   actool  compiles the bundle as Xcode does for an app. Its output sits
#           on Apple's icon grid — the squircle fills 824 of every 1024
#           pixels, with a margin — so it is the right size beside every
#           other app. Everything macOS shows as an app icon comes from
#           here: the notification icon, and the fallback .icns.
#   ictool  Icon Composer's exporter. Its output is full bleed, the
#           squircle edge to edge, which is right for a README or a
#           website and about a quarter too large as an app icon. Even
#           rendered at the grid size and given the margin, a third of its
#           pixels differ from actool's, so it is not a substitute.
#
# The app bundle does not use these files: apps/macos/build.sh compiles
# Autobahn.icon with actool at build time, which also carries the dark and
# tinted variants. These are for everything else, and are committed so a
# plain `cargo build` needs neither Xcode nor a Mac. Rerun this whenever
# the bundle changes.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

xcrun --find actool >/dev/null 2>&1 || { echo "needs Xcode (actool not found)" >&2; exit 1; }
ICTOOL="$(xcode-select -p)/../Applications/Icon Composer.app/Contents/Executables/ictool"
[ -x "$ICTOOL" ] || { echo "needs Xcode with Icon Composer (no ictool at $ICTOOL)" >&2; exit 1; }
WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT

# What Xcode would put in an app: Assets.car, and the flat .icns that
# systems without Icon Composer support fall back to.
xcrun actool Autobahn.icon --compile "$WORK" --app-icon Autobahn \
      --platform macosx --minimum-deployment-target 11.0 \
      --output-partial-info-plist "$WORK/partial.plist" >/dev/null
cp "$WORK/Autobahn.icns" assets/autobahn.icns

# The one the binary embeds for notifications, taken from that same .icns
# at 256 pixels — no larger is ever shown, and the full-size art would be
# most of the executable.
iconutil -c iconset "$WORK/Autobahn.icns" -o "$WORK/Autobahn.iconset"
magick "$WORK/Autobahn.iconset/icon_128x128@2x.png" -strip PNG32:assets/notification.png

# The artwork at full size and full bleed, for documentation. Not an app
# icon: see the note on ictool above.
"$ICTOOL" Autobahn.icon --export-image --output-file "$WORK/autobahn.png" \
    --platform macOS --rendition Default --width 1024 --height 1024 --scale 1 >/dev/null
magick "$WORK/autobahn.png" -strip PNG32:assets/autobahn.png

echo "wrote assets/autobahn.icns and assets/notification.png (actool), assets/autobahn.png (ictool, full bleed)"
