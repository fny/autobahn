#!/bin/sh

# Quick and dirty notification script for autobahn alerts.
# Use it directly or as a model for creating your own notification scripts.
# Warning: this hasn't been tested extensively.
#
# Autobahn sets these variables before it runs this script:
#
#   $AUTOBAHN_SUMMARY      one line that says what happened, or how many
#   $AUTOBAHN_DETAIL       one indented line for each session that needs you
#   $AUTOBAHN_ICON         the full path to Autobahn's icon
#   $AUTOBAHN_STATES       the states involved, separated by commas
#   $AUTOBAHN_ALERT_COUNT  how many sessions need you
#   $AUTOBAHN_EVENT        "alert" the first time, "repeat" after that
#
# The login service runs with a short PATH and few environment variables.
# So commands are written with their full path, and the bus address is
# worked out below.
set -eu

case "$(uname -s)" in
Darwin)
    # terminal-notifier can show a subtitle. Homebrew installs it in one
    # of two places, depending on the chip.
    for notifier in \
        /opt/homebrew/bin/terminal-notifier \
        /usr/local/bin/terminal-notifier
    do
        [ -x "$notifier" ] || continue
        exec "$notifier" \
            -title autobahn -group autobahn \
            -appIcon "$AUTOBAHN_ICON" \
            -subtitle "$AUTOBAHN_DETAIL" \
            -message "$AUTOBAHN_SUMMARY"
    done

    # osascript is built into macOS, so it is always there. It shows one
    # line, and a click on it does nothing. The summary is passed as an
    # argument, never as part of the AppleScript, because it can contain a
    # file name that someone else chose.
    exec /usr/bin/osascript \
        -e 'on run argv' \
        -e 'display notification (item 1 of argv) with title "autobahn"' \
        -e 'end run' \
        "$AUTOBAHN_SUMMARY"
    ;;
Linux)
    # notify-send reaches the desktop over the session bus. A service
    # started by your own systemd gets the bus address. A service started
    # by the system does not, so the address is built from your user id.
    if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
        DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$(id -u)/bus"
        export DBUS_SESSION_BUS_ADDRESS
    fi
    if command -v notify-send >/dev/null 2>&1; then
        # The urgency is normal, not critical. A conflict needs attention
        # today. It does not need a notification that never goes away.
        exec notify-send \
            --app-name autobahn \
            --icon "$AUTOBAHN_ICON" \
            "$AUTOBAHN_SUMMARY" \
            "$AUTOBAHN_DETAIL"
    fi
    ;;
esac

# There is no notifier, or this host has no desktop. The message goes to
# standard error, which ends up in the log.
echo "autobahn: $AUTOBAHN_SUMMARY" >&2
