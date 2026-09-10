#!/bin/bash
# Renders the shipped icons from Autobahn.icon.
#
# Autobahn.icon is an Icon Composer bundle, which only Xcode compiles.
# This reproduces it closely enough to ship without one: the manifest's
# gradient, its glass layer, and the rounded square macOS expects. Rerun
# it whenever the bundle changes; the results are committed so a plain
# `cargo build` needs neither ImageMagick nor a Mac.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
command -v magick >/dev/null || { echo "needs ImageMagick (brew install imagemagick)" >&2; exit 1; }

GLYPH="Autobahn Transparent.png"
SIZE=1024
# The manifest's fill, converted from display-p3 to sRGB, and its stop at
# 70% of the height — below that the gradient is done.
TOP="#8B51A7"; BOTTOM="#550033"; STOP=717
RADIUS=229                       # macOS rounds an app icon by about 22%
WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT

magick -size ${SIZE}x${STOP} gradient:"$TOP-$BOTTOM" \
       \( -size ${SIZE}x$((SIZE-STOP)) xc:"$BOTTOM" \) -append "$WORK/fill.png"
# The glass layer, kept as it was drawn: the artwork already fades along
# its own edges, so tinting it flat white throws away the fade that makes
# it read as a road. It is only lightened and made slightly translucent,
# over a dark shadow that sits it on the fill.
magick "$GLYPH" -resize $((SIZE*56/100))x "$WORK/glyph.png"
magick "$WORK/glyph.png" -alpha extract -blur 0x20 \
       -background black -alpha shape \
       -channel A -evaluate multiply 0.45 +channel "$WORK/shadow.png"
magick "$WORK/fill.png" -alpha set \
  "$WORK/shadow.png" -gravity center -geometry +0+40 -composite \
  \( "$WORK/glyph.png" -modulate 118 -channel A -evaluate multiply 0.88 +channel \) \
     -gravity center -geometry +0+18 -composite \
  "$WORK/flat.png"
# The rounded square, applied as the alpha channel rather than drawn over
# the art, so the corners are transparent and not merely dark.
magick -size ${SIZE}x${SIZE} xc:black \
       -fill white -draw "roundrectangle 0,0 $((SIZE-1)),$((SIZE-1)) $RADIUS,$RADIUS" \
       -alpha off "$WORK/mask.png"
magick "$WORK/flat.png" -alpha set "$WORK/mask.png" \
       -alpha off -compose CopyOpacity -composite assets/autobahn.png

# The .icns macOS wants, from the same source.
ICONSET="$WORK/autobahn.iconset"; mkdir -p "$ICONSET"
for s in 16 32 128 256 512; do
  magick assets/autobahn.png -resize ${s}x${s}      "$ICONSET/icon_${s}x${s}.png"
  magick assets/autobahn.png -resize $((s*2))x$((s*2)) "$ICONSET/icon_${s}x${s}@2x.png"
done
iconutil -c icns "$ICONSET" -o assets/autobahn.icns

# The one the binary carries: notifications never show it larger than a
# couple of hundred points, and the full-size art would be most of the
# executable.
magick assets/autobahn.png -resize 256x256 -strip assets/notification.png
echo "wrote assets/notification.png, assets/autobahn.png ($(sips -g pixelWidth assets/autobahn.png | tail -1 | tr -d ' ')) and assets/autobahn.icns"
