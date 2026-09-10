#!/bin/bash
# Signs, notarises and staples Autobahn.app for another machine.
#
# Gatekeeper refuses an app it cannot attribute, and it will not attribute
# one signed ad-hoc or with an Apple Development certificate — those are
# for the machine that built them. Distribution needs a Developer ID
# Application certificate, and then Apple's own scan: notarisation is that
# scan, and stapling attaches its verdict to the app so it is trusted with
# no network.
#
# Only needed for an app someone downloads. A copy that arrives by scp or
# by autobahn itself is never quarantined, so Gatekeeper never asks.
#
#   apps/macos/release.sh                       # build, sign, notarise, staple
#   AUTOBAHN_NOTARY_PROFILE=work release.sh     # a different stored credential
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
APP="apps/macos/Autobahn.app"
PROFILE="${AUTOBAHN_NOTARY_PROFILE:-autobahn}"

IDENTITY=$(security find-identity -v -p codesigning 2>/dev/null |
           grep -o '"Developer ID Application: [^"]*"' | head -1 | tr -d '"') || true
if [ -z "$IDENTITY" ]; then
    cat >&2 <<'MISSING'
No Developer ID Application certificate in the keychain.

An Apple Development certificate is not the same thing: it signs for the
machines you own, and Gatekeeper still refuses the result elsewhere.

  Xcode → Settings → Accounts → your Apple ID
        → Manage Certificates… → + → Developer ID Application
MISSING
    exit 1
fi

AUTOBAHN_SIGN_IDENTITY="$IDENTITY" apps/macos/build.sh "$APP"

if ! xcrun notarytool history --keychain-profile "$PROFILE" >/dev/null 2>&1; then
    cat >&2 <<MISSING
No stored notary credential named "$PROFILE". Make one once:

  xcrun notarytool store-credentials $PROFILE \\
    --apple-id <your-apple-id> --team-id <TEAMID> \\
    --password <app-specific password from appleid.apple.com>
MISSING
    exit 1
fi

# notarytool takes an archive, never a bare bundle.
ZIP="$(mktemp -d)/Autobahn.zip"
ditto -c -k --keepParent "$APP" "$ZIP"
xcrun notarytool submit "$ZIP" --keychain-profile "$PROFILE" --wait
# Stapling the app, not the archive: the ticket has to travel inside the
# thing that gets opened.
xcrun stapler staple "$APP"
spctl -a -vvv "$APP"
echo
echo "notarised. Ship an archive made *after* stapling:"
echo "  ditto -c -k --keepParent $APP Autobahn.zip"
