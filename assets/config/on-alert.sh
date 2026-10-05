#!/bin/sh

# Quick and dirty notification script for autobahn alerts.
#
# Use it as is or as a model for creating your own notification scripts.
#
# Environment variables exported by Autobahn before execution:
#
#   $AUTOBAHN_SUMMARY      Single-line event summary or alert count
#   $AUTOBAHN_DETAIL       Indented details for each session requiring action
#   $AUTOBAHN_ICON         Absolute path to the Autobahn icon
#   $AUTOBAHN_STATES       Comma-separated list of active states
#   $AUTOBAHN_ALERT_COUNT  Number of sessions requiring attention
#   $AUTOBAHN_EVENT        Trigger type ("alert" for initial, "repeat" for re-notifies)
#
# Execution context:
#   Executed by login services under a minimal PATH and environment.
#   Commands require full binary paths, and dynamic variables (like D-Bus)
#   must be resolved manually.
set -eu

case "$(uname -s)" in
Darwin)
    # Try terminal-notifier (supports subtitles; path depends on Apple Silicon
    # vs. Intel Homebrew)
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

    # macOS fallback: osascript is always available. Pass summary as an argument
    # to avoid injection vulnerabilities.
    exec /usr/bin/osascript \
        -e 'on run argv' \
        -e 'display notification (item 1 of argv) with title "autobahn"' \
        -e 'end run' \
        "$AUTOBAHN_SUMMARY"
    ;;
Linux)
    # Ensure D-Bus session bus address is set when executed under system-level
    # daemon context
    if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
        DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$(id -u)/bus"
        export DBUS_SESSION_BUS_ADDRESS
    fi
    if command -v notify-send >/dev/null 2>&1; then
        # Send desktop notification using default (non-persistent) urgency
        exec notify-send \
            --app-name autobahn \
            --icon "$AUTOBAHN_ICON" \
            "$AUTOBAHN_SUMMARY" \
            "$AUTOBAHN_DETAIL"
    fi
    ;;
esac

# Fallback for headless environments, non-desktop hosts, or missing notification
# binaries.
echo "autobahn: $AUTOBAHN_SUMMARY" >&2
