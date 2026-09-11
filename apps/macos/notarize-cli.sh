#!/bin/bash
# Signs and notarises command-line binaries for download.
#
# A binary downloaded in a browser is quarantined, and Gatekeeper refuses
# to run a quarantined executable it cannot attribute. The curl installer
# never quarantines anything, so it never needed this; a binary taken from
# the Releases page by hand does.
#
# A bare executable cannot be stapled — there is no bundle to carry the
# ticket — so Gatekeeper looks the notarisation up online the first time
# the binary runs. That is Apple's arrangement for command-line tools.
#
# Signs the files it is given IN PLACE. Pass copies: re-signing the binary
# a running process was started from changes the file under it.
#
#   apps/macos/notarize-cli.sh autobahn-darwin-aarch64 autobahn-darwin-x86_64
#
# Credentials as for release.sh: a keychain profile, or an App Store
# Connect API key through AUTOBAHN_NOTARY_KEY, _KEY_ID and _ISSUER.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/notary.sh"

[ $# -gt 0 ] || { echo "usage: $0 <binary>..." >&2; exit 2; }
for binary in "$@"; do
    [ -f "$binary" ] || { echo "no such file: $binary" >&2; exit 1; }
done

signing_identity
notary_credential

for binary in "$@"; do
    # The hardened runtime and a secure timestamp are what notarisation
    # requires of a signature.
    codesign --force --options runtime --timestamp --sign "$IDENTITY" "$binary"
    codesign --verify --strict "$binary"
done

# One submission for all of them: notarytool takes an archive.
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir "$WORK/binaries"
cp "$@" "$WORK/binaries/"
ditto -c -k "$WORK/binaries" "$WORK/binaries.zip"
notarize "$WORK/binaries.zip"
