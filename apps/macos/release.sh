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
#   apps/macos/release.sh --sign-only <app>     # sign, notarise, staple a built app
#   AUTOBAHN_NOTARY_PROFILE=work release.sh     # a different stored credential
#
# --sign-only compiles nothing. CI builds with `build.sh --unsigned` first
# and only then imports the certificate, so no dependency's build script or
# proc macro runs while the signing identity is usable.
#
# CI has no keychain to store a notary profile in, so it passes an App
# Store Connect API key instead, and asks for the stapled app archived:
#
#   AUTOBAHN_NOTARY_KEY=AuthKey_XXXXXXXXXX.p8 AUTOBAHN_NOTARY_KEY_ID=XXXXXXXXXX \
#   AUTOBAHN_NOTARY_ISSUER=<issuer uuid> AUTOBAHN_RELEASE_ZIP=Autobahn.zip \
#   apps/macos/release.sh
set -euo pipefail
usage() { echo "usage: $0 [--sign-only path/to/Autobahn.app]" >&2; exit 2; }
BUILD=yes
APP="apps/macos/Autobahn.app"
case $# in
    0) ;;
    2) [ "$1" = --sign-only ] || usage
       BUILD=no
       # The caller's path, resolved before leaving their directory.
       APP="$2"; [[ "$APP" = /* ]] || APP="$PWD/$APP" ;;
    *) usage ;;
esac
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
source apps/macos/notary.sh
ZIP_OUT="${AUTOBAHN_RELEASE_ZIP:-}"

if [ "$BUILD" = no ] &&
   { [ ! -f "$APP/Contents/Info.plist" ] || [ ! -x "$APP/Contents/MacOS/autobahn" ]; }; then
    echo "not a built Autobahn.app: $APP (apps/macos/build.sh --unsigned makes one)" >&2
    exit 1
fi

# Both settled before the build, not after it: a missing credential should
# cost a second, not a minute of compiling first.
signing_identity
notary_credential

if [ "$BUILD" = yes ]; then apps/macos/build.sh --unsigned "$APP"; fi

# Cleared again because a bundle copied in (from another job, say) is
# tagged anew, and codesign refuses one carrying com.apple.provenance.
xattr -cr "$APP"
# The hardened runtime and a secure timestamp are both what notarisation
# requires, so there is no fallback without the timestamp here: better to
# fail now than after an upload. No --deep, as in build.sh.
codesign --force --options runtime --timestamp --sign "$IDENTITY" "$APP"
codesign --verify --strict "$APP"
echo "signed as: $IDENTITY"

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
