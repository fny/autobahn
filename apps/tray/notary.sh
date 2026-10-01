# shellcheck shell=bash
# Shared by release.sh and notarize-cli.sh: which identity signs, which
# credential notarises, and a submission that fails loudly. Sourced, not
# run, so a laptop and CI sign and notarise the same way.

# Sets IDENTITY: AUTOBAHN_SIGN_IDENTITY when given, and otherwise the first
# Developer ID Application identity in the keychain search list — which in
# CI is the throwaway keychain the job imported the certificate into.
signing_identity() {
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
        return 1
    fi
}

# Sets NOTARY to notarytool's credential arguments: an App Store Connect
# API key when AUTOBAHN_NOTARY_KEY is set, which is how CI authenticates,
# and otherwise a profile stored in the keychain.
notary_credential() {
    local profile="${AUTOBAHN_NOTARY_PROFILE:-autobahn}"
    if [ -n "${AUTOBAHN_NOTARY_KEY:-}" ]; then
        if [ -z "${AUTOBAHN_NOTARY_KEY_ID:-}" ] || [ -z "${AUTOBAHN_NOTARY_ISSUER:-}" ]; then
            echo "AUTOBAHN_NOTARY_KEY also needs AUTOBAHN_NOTARY_KEY_ID and AUTOBAHN_NOTARY_ISSUER" >&2
            return 1
        fi
        [ -f "$AUTOBAHN_NOTARY_KEY" ] || { echo "no notary key at $AUTOBAHN_NOTARY_KEY" >&2; return 1; }
        NOTARY=(--key "$AUTOBAHN_NOTARY_KEY" --key-id "$AUTOBAHN_NOTARY_KEY_ID"
                --issuer "$AUTOBAHN_NOTARY_ISSUER")
    else
        if ! xcrun notarytool history --keychain-profile "$profile" >/dev/null 2>&1; then
            cat >&2 <<MISSING
No stored notary credential named "$profile". Make one once:

  xcrun notarytool store-credentials $profile \\
    --apple-id <your-apple-id> --team-id <TEAMID> \\
    --password <app-specific password from appleid.apple.com>

Or pass an App Store Connect API key through AUTOBAHN_NOTARY_KEY,
AUTOBAHN_NOTARY_KEY_ID and AUTOBAHN_NOTARY_ISSUER.
MISSING
            return 1
        fi
        NOTARY=(--keychain-profile "$profile")
    fi
}

# Submits an archive to Apple and waits for the verdict. The verdict is
# read from notarytool's output rather than trusted to its exit status, and
# anything but Accepted fails with Apple's own log of why — for a bare
# binary there is no stapling step afterwards to catch a rejection.
notarize() {
    # `verdict`, not `status`: zsh reserves `status`, and a sourced file
    # should not care which shell sources it.
    local archive="$1" result verdict id
    result=$(mktemp)
    xcrun notarytool submit "$archive" "${NOTARY[@]}" --wait --output-format json > "$result"
    verdict=$(plutil -extract status raw -o - "$result" 2>/dev/null || echo unknown)
    id=$(plutil -extract id raw -o - "$result" 2>/dev/null || echo "")
    rm -f "$result"
    if [ "$verdict" != "Accepted" ]; then
        echo "notarisation was not accepted: $verdict" >&2
        if [ -n "$id" ]; then xcrun notarytool log "$id" "${NOTARY[@]}" >&2 || true; fi
        return 1
    fi
    echo "notarised: $verdict ($id)"
}
