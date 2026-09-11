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
#
# One script for both, so a release built by CI is signed exactly the way
# one built on a laptop is.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
APP="apps/macos/Autobahn.app"
PROFILE="${AUTOBAHN_NOTARY_PROFILE:-autobahn}"
ZIP_OUT="${AUTOBAHN_RELEASE_ZIP:-}"

IDENTITY="${AUTOBAHN_SIGN_IDENTITY:-}"
if [ -z "$IDENTITY" ]; then
    IDENTITY=$(security find-identity -v -p codesigning 2>/dev/null |
               grep -o '"Developer ID Application: [^"]*"' | head -1 | tr -d '"') || true
fi
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

# The notary credential is settled before the build, not after it: a
# missing one should cost a second, not a minute of compiling first.
if [ -n "${AUTOBAHN_NOTARY_KEY:-}" ]; then
    : "${AUTOBAHN_NOTARY_KEY_ID:?AUTOBAHN_NOTARY_KEY also needs AUTOBAHN_NOTARY_KEY_ID}"
    : "${AUTOBAHN_NOTARY_ISSUER:?AUTOBAHN_NOTARY_KEY also needs AUTOBAHN_NOTARY_ISSUER}"
    [ -f "$AUTOBAHN_NOTARY_KEY" ] || { echo "no notary key at $AUTOBAHN_NOTARY_KEY" >&2; exit 1; }
    NOTARY=(--key "$AUTOBAHN_NOTARY_KEY" --key-id "$AUTOBAHN_NOTARY_KEY_ID"
            --issuer "$AUTOBAHN_NOTARY_ISSUER")
else
    if ! xcrun notarytool history --keychain-profile "$PROFILE" >/dev/null 2>&1; then
        cat >&2 <<MISSING
No stored notary credential named "$PROFILE". Make one once:

  xcrun notarytool store-credentials $PROFILE \\
    --apple-id <your-apple-id> --team-id <TEAMID> \\
    --password <app-specific password from appleid.apple.com>

Or pass an App Store Connect API key through AUTOBAHN_NOTARY_KEY,
AUTOBAHN_NOTARY_KEY_ID and AUTOBAHN_NOTARY_ISSUER.
MISSING
        exit 1
    fi
    NOTARY=(--keychain-profile "$PROFILE")
fi

AUTOBAHN_SIGN_IDENTITY="$IDENTITY" apps/macos/build.sh "$APP"

# notarytool takes an archive, never a bare bundle.
ZIP="$(mktemp -d)/Autobahn.zip"
ditto -c -k --keepParent "$APP" "$ZIP"
xcrun notarytool submit "$ZIP" "${NOTARY[@]}" --wait
# Stapling the app, not the archive: the ticket has to travel inside the
# thing that gets opened. A rejected submission has no ticket, so this is
# also where a failed notarisation stops the script.
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
