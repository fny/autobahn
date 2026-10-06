#!/bin/sh
# Every view of the app, against fixtures, without touching this machine.
#
#   scripts/views.sh list                 the fixtures and what each is for
#   scripts/views.sh show calm conflicts  open one, to tweak how it looks
#   scripts/views.sh sheet                photograph all of them, tiled
#   scripts/views.sh docs                 remake the five pictures the docs use
#
# The fixtures are built by examples/fixtures.rs into target/views. Each
# one is a configuration and a state root the app reads as if it were
# real, plus an `env` file saying whether the command is installed and
# what the login service is doing — the two answers that are not in a
# state root.
set -eu

here=$(cd "$(dirname "$0")/.." && pwd)
views="$here/target/views"
shots="$here/target/views-shots"
cargo="cargo +1.98.0"

# Every pane the window can open on. `config:<section>` opens the
# configuration on one section, which is how a picture of a section is
# taken without a hand on the mouse.
panes="groups hosts conflicts log service config"

build() {
  $cargo build --quiet --features app --bin autobahn-app
  # Built, not `cargo run`: `launch` gives the app a fixture's own HOME,
  # and cargo under that HOME has no ~/.cargo to work from.
  $cargo build --quiet --example fixtures
  $cargo run --quiet --example fixtures -- "$views" >/dev/null
}

# Whether a supervisor is running is read from a socket, not a file, so
# a fixture needs something answering on it. This starts one and the
# trap takes it down, however the script ends.
served=
serve() {
  at=$1
  [ -f "$at/home/.local/bin/autobahn" ] || return 0   # nothing installed to supervise
  # A stale socket from a killed run answers nothing; the server
  # removes it before binding, and this keeps the wait below honest.
  rm -f "$at/home/.autobahn/control.sock"
  "$here/target/debug/examples/fixtures" "$at" --serve >/dev/null 2>&1 &
  served=$!
  # The socket has to exist before the app looks for it.
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    [ -S "$at/home/.autobahn/control.sock" ] && return 0
    sleep 0.2
  done
  echo "the fixture supervisor did not start; the footer will say none is running" >&2
}
hang_up() {
  [ -n "$served" ] && kill "$served" 2>/dev/null
  served=
}
trap hang_up EXIT INT TERM

# Runs the app against one fixture. Everything after the fixture name is
# handed to the binary, so `shoot` and `show` share one launcher.
launch() {
  fixture=$1
  shift
  at="$views/$fixture"
  [ -d "$at" ] || { echo "no fixture named $fixture — try: $0 list" >&2; exit 1; }
  # The fixture's own answers. Exported rather than passed, because the
  # library reads them from the environment wherever it asks.
  # shellcheck disable=SC2046
  export $(grep -v '^#' "$at/env" | xargs)
  serve "$at"
  "$here/target/debug/autobahn-app" \
    --config "$at/home/.autobahn/config.toml" \
    --state-root "$at/home/.autobahn" \
    "$@"
  hang_up
}

case "${1:-show}" in
list)
  $cargo run --quiet --example fixtures -- --list
  ;;

show)
  build
  fixture=${2:-calm}
  pane=${3:-}
  if [ -n "$pane" ]; then
    launch "$fixture" --pane "$pane"
  else
    launch "$fixture"
  fi
  ;;

sheet)
  build
  rm -rf "$shots"
  mkdir -p "$shots"
  for fixture in $("$here/target/debug/examples/fixtures" --list | awk '{print $1}'); do
    # A fixture with no command installed draws the splash whatever pane
    # is asked for, so asking for seven is six identical pictures.
    case "$fixture" in
      fresh) wanted="welcome" ;;
      *)     wanted="$panes" ;;
    esac
    for pane in $wanted; do
      into="$shots/$fixture"
      mkdir -p "$into"
      launch "$fixture" --pane "$pane" --shoot "$into" >/dev/null
      # The binary names its own file after the pane; the sheet needs the
      # fixture in the name too, since every fixture has a `log`.
      taken="$into/kit-${pane%%:*}.png"
      [ -f "$taken" ] && mv "$taken" "$shots/$fixture-${pane%%:*}.png"
    done
    rmdir "$shots/$fixture" 2>/dev/null || true
  done
  count=$(find "$shots" -name '*.png' | wc -l | tr -d ' ')
  echo "$count views in $shots"
  if command -v montage >/dev/null 2>&1; then
    # ImageMagick has no default font on macOS, so name one. Labelled,
    # because a sheet of twenty windows is unreadable without knowing
    # which fixture each one came from.
    font=/System/Library/Fonts/Supplemental/Arial.ttf
    [ -f "$font" ] || font=Helvetica
    montage -label '%t' "$shots"/*.png \
      -tile 4x -geometry '420x+12+12' -background '#1c1c1e' -fill '#f2f2f7' \
      -font "$font" -pointsize 13 "$shots/sheet.png"
    echo "$shots/sheet.png"
  else
    echo "install imagemagick for the tiled sheet (brew install imagemagick)" >&2
  fi
  ;;

docs)
  # The pictures README.md and docs/app.md show, from the fixtures they
  # were always taken from: the welcome splash with nothing installed,
  # three panes of a calm fleet, and the conflicts pane of a troubled
  # one with its first diff open.
  build
  rm -rf "$shots"
  mkdir -p "$shots"
  # The window as it opens by default. A calm fleet's three groups end
  # halfway down it, so that picture is taken in a shorter window and
  # ends with them.
  take() {
    fixture=$1
    pane=$2
    name=$3
    height=$4
    into="$shots/$fixture"
    mkdir -p "$into"
    AUTOBAHN_DESK_WIDTH=1240 AUTOBAHN_DESK_HEIGHT="$height" \
      launch "$fixture" --pane "$pane" --shoot "$into" >/dev/null
    mv "$into/kit-$pane.png" "$here/assets/screenshots/$name.png"
    echo "assets/screenshots/$name.png"
  }
  take fresh welcome welcome 820
  take calm groups groups 520
  take calm config configuration 820
  take calm service service 820
  take trouble conflicts:diff conflicts 820
  ;;

*)
  sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'
  exit 1
  ;;
esac
