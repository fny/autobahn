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
#
# CI has no keychain to store a notary profile in, so it passes an App
# Store Connect API key instead, and asks for the stapled app archived:
#
#   AUTOBAHN_NOTARY_KEY=AuthKey_XXXXXXXXXX.p8 AUTOBAHN_NOTARY_KEY_ID=XXXXXXXXXX \
#   AUTOBAHN_NOTARY_ISSUER=<issuer uuid> AUTOBAHN_RELEASE_ZIP=Autobahn.zip \
#   apps/macos/release.sh
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
source apps/macos/notary.sh
APP="apps/macos/Autobahn.app"
ZIP_OUT="${AUTOBAHN_RELEASE_ZIP:-}"

# Both settled before the build, not after it: a missing credential should
# cost a second, not a minute of compiling first.
signing_identity
notary_credential

AUTOBAHN_SIGN_IDENTITY="$IDENTITY" apps/macos/build.sh "$APP"

# notarytool takes an archive, never a bare bundle.
ZIP="$(mktemp -d)/Autobahn.zip"
ditto -c -k --keepParent "$APP" "$ZIP"
notarize "$ZIP"
# Stapling the app, not the archive: the ticket has to travel inside the
# thing that gets opened.
xcrun stapler staple "$APP"
spctl -a -vvv "$APP"
echo
if [ -n "$ZIP_OUT" ]; then
    # Archived after stapling, so the ticket is inside what ships.
    ditto -c -k --keepParent "$APP" "$ZIP_OUT"
    echo "notarised and archived: $ZIP_OUT"
else
    echo "notarised. Ship an archive made *after* stapling:"
    echo "  ditto -c -k --keepParent $APP Autobahn.zip"
fi
