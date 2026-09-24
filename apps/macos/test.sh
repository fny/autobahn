#!/bin/bash
# Checks the app's build scripts on any machine, macOS or not.
#
# build.sh runs here in a copy of the tree, with stand-ins for cargo and
# the Apple tools, so what is checked is the script's own logic: which
# version the bundle claims, and what it assembles. Signing, actool and
# notarisation are Apple's, and only a Mac can check them.
#
#   apps/macos/test.sh
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
FAILED=0

fail() { echo "FAIL: $*" >&2; FAILED=1; }
pass() { echo "ok: $*"; }

# A copy of the tree build.sh needs, around a Cargo.toml at a test version.
# A dependency table with its own `version` comes first, so only the
# [package] table's can be the one read.
tree() {
    local dir="$1" version="$2"
    mkdir -p "$dir/apps/macos" "$dir/assets"
    cp "$HERE"/*.sh "$HERE/Info.plist" "$dir/apps/macos/"
    cp "$ROOT/assets/autobahn.icns" "$dir/assets/"
    cat > "$dir/Cargo.toml" <<TOML
[dependencies.early]
version = "1.2.3"

[package]
name = "autobahn"
version = "$version"
edition = "2021"

[dependencies]
late = { version = "4.5.6" }
TOML
}

# Stand-ins that log their calls. cargo leaves an executable where the
# real build would; actool is missing, as without Xcode 26; the keychain
# holds one Developer ID identity; Apple accepts every submission; ditto
# writes the archive it is asked for.
stubs() {
    local bin="$1"
    mkdir -p "$bin"
    for tool in cargo xcrun xattr codesign security ditto spctl plutil; do
        cat > "$bin/$tool" <<STUB
#!/bin/bash
echo "$tool \$*" >> "$WORK/calls"
STUB
        chmod +x "$bin/$tool"
    done
    cat >> "$bin/cargo" <<'STUB'
mkdir -p "$CARGO_TARGET_DIR/release"
printf '#!/bin/sh\n' > "$CARGO_TARGET_DIR/release/autobahn"
chmod +x "$CARGO_TARGET_DIR/release/autobahn"
STUB
    cat >> "$bin/xcrun" <<'STUB'
case "$1 ${2:-}" in
    "--find actool"|"actool "*) exit 1 ;;
    "notarytool submit") echo '{"status":"Accepted","id":"stub-id"}' ;;
esac
STUB
    cat >> "$bin/security" <<'STUB'
[ "$1" = find-identity ] &&
    echo '  1) 0123456789 "Developer ID Application: Test (TEAM123456)"'
exit 0
STUB
    cat >> "$bin/plutil" <<'STUB'
[ "$1" = -extract ] && { [ "$2" = status ] && echo Accepted || echo stub-id; }
exit 0
STUB
    cat >> "$bin/ditto" <<'STUB'
: > "${!#}"
STUB
}

# Runs one of the copied scripts with the stand-ins first on PATH, and the
# call log emptied so it holds only this run's.
run() {
    : > "$WORK/calls"
    PATH="$WORK/bin:$PATH" AUTOBAHN_NOTARY_KEY="$WORK/key.p8" \
        AUTOBAHN_NOTARY_KEY_ID=KEY AUTOBAHN_NOTARY_ISSUER=ISSUER "$@" > "$WORK/out" 2>&1
}

# The line number of the first logged call matching a pattern, or nothing.
first_call() { grep -n -m1 -E "$1" "$WORK/calls" | cut -d: -f1 || true; }

plist_value() {
    # The <string> after a key, in the one-key-per-line layout Info.plist uses.
    sed -n "/<key>$2<\/key>/{n;s/.*<string>\(.*\)<\/string>.*/\1/p;}" "$1"
}

stubs "$WORK/bin"
: > "$WORK/key.p8"

# The committed template claims no version, so a stale one cannot ship.
for key in CFBundleShortVersionString CFBundleVersion; do
    value=$(plist_value "$HERE/Info.plist" "$key")
    if [[ "$value" =~ [0-9] ]]; then
        fail "the committed Info.plist names a version for $key: $value"
    else
        pass "the committed Info.plist has no version for $key ($value)"
    fi
done

# build.sh with Cargo.toml at a test version: the bundle reports it.
tree "$WORK/signed" 9.8.7
if run "$WORK/signed/apps/macos/build.sh"; then
    plist="$WORK/signed/apps/macos/Autobahn.app/Contents/Info.plist"
    for key in CFBundleShortVersionString CFBundleVersion; do
        value=$(plist_value "$plist" "$key")
        if [ "$value" = 9.8.7 ]; then pass "build.sh writes $key from Cargo.toml"
        else fail "build.sh wrote $key = '$value', not 9.8.7"; fi
    done
    for key in CFBundleIconFile CFBundleIconName; do
        value=$(plist_value "$plist" "$key")
        if [ "$value" = autobahn ]; then pass "build.sh names the fallback icon in $key"
        else fail "build.sh wrote $key = '$value', not autobahn"; fi
    done
    if grep -q '__' "$plist"; then fail "a placeholder survived into the bundle's Info.plist"
    else pass "no placeholder survives into the bundle's Info.plist"; fi
    if [ -x "$WORK/signed/apps/macos/Autobahn.app/Contents/MacOS/autobahn" ]; then
        pass "build.sh puts the executable in the bundle"
    else fail "build.sh left no executable in the bundle"; fi
else
    fail "build.sh failed:"; cat "$WORK/out" >&2
fi

# A Cargo.toml without a package version is refused, not guessed at.
tree "$WORK/noversion" 1.0.0
sed -i.bak '/^version = "1.0.0"/d' "$WORK/noversion/Cargo.toml"
if run "$WORK/noversion/apps/macos/build.sh"; then
    fail "build.sh built with no version in Cargo.toml"
else
    pass "build.sh refuses a Cargo.toml with no package version"
fi

# Nor is a version that would not survive being written into XML.
tree "$WORK/hostile" '1.0.0</string>'
if run "$WORK/hostile/apps/macos/build.sh"; then
    fail "build.sh built with the version '1.0.0</string>'"
else
    pass "build.sh refuses a version with markup in it"
fi

# build.sh --unsigned: the whole bundle, and nothing that touches a
# signing identity. This is what CI builds before any certificate exists.
tree "$WORK/unsigned" 2.0.0
UNSIGNED="$WORK/unsigned/out/Autobahn.app"
if run "$WORK/unsigned/apps/macos/build.sh" --unsigned "$UNSIGNED"; then
    if [ "$(plist_value "$UNSIGNED/Contents/Info.plist" CFBundleVersion)" = 2.0.0 ] &&
       [ -x "$UNSIGNED/Contents/MacOS/autobahn" ]; then
        pass "build.sh --unsigned assembles the bundle where it is told"
    else fail "build.sh --unsigned left an incomplete bundle at $UNSIGNED"; fi
    if grep -qE '^(codesign|security) ' "$WORK/calls"; then
        fail "build.sh --unsigned reached for signing:"; grep -E '^(codesign|security) ' "$WORK/calls" >&2
    else pass "build.sh --unsigned never runs codesign or security"; fi
else
    fail "build.sh --unsigned failed:"; cat "$WORK/out" >&2
fi

# release.sh --sign-only: signs, notarises and staples that bundle, and
# never compiles anything — so no build script runs while the identity is
# usable.
if run "$WORK/unsigned/apps/macos/release.sh" --sign-only "$UNSIGNED"; then
    if grep -q '^cargo ' "$WORK/calls"; then fail "release.sh --sign-only ran cargo"
    else pass "release.sh --sign-only runs no cargo"; fi
    if grep -qF "codesign --force --options runtime --timestamp --sign Developer ID Application: Test (TEAM123456) $UNSIGNED" "$WORK/calls"
    then pass "release.sh --sign-only signs the bundle with the Developer ID, hardened and timestamped"
    else fail "release.sh --sign-only did not sign $UNSIGNED as expected:"; cat "$WORK/calls" >&2; fi
    sign=$(first_call '^codesign --force'); submit=$(first_call '^xcrun notarytool submit')
    staple=$(first_call "^xcrun stapler staple $UNSIGNED\$")
    if [ -n "$sign" ] && [ -n "$submit" ] && [ -n "$staple" ] &&
       [ "$sign" -lt "$submit" ] && [ "$submit" -lt "$staple" ]; then
        pass "release.sh --sign-only signs, then notarises, then staples"
    else fail "release.sh --sign-only: sign at ${sign:-never}, submit at ${submit:-never}, staple at ${staple:-never}"; fi
else
    fail "release.sh --sign-only failed:"; cat "$WORK/out" >&2
fi

# ...and refuses what is not a bundle, before touching the keychain.
if run "$WORK/unsigned/apps/macos/release.sh" --sign-only "$WORK/unsigned/nothing.app"; then
    fail "release.sh --sign-only accepted a bundle that does not exist"
elif grep -qE '^(codesign|security|xcrun) ' "$WORK/calls"; then
    fail "release.sh --sign-only reached for Apple's tools with no bundle"
else pass "release.sh --sign-only refuses a missing bundle"; fi

if run "$WORK/unsigned/apps/macos/release.sh" --sign-only; then
    fail "release.sh --sign-only ran with no bundle named"
else pass "release.sh --sign-only needs a bundle named"; fi

# release.sh with no arguments still does everything, in that order: the
# credentials checked first, then the build, then signing.
tree "$WORK/laptop" 3.0.0
if run "$WORK/laptop/apps/macos/release.sh"; then
    ident=$(first_call '^security find-identity'); build=$(first_call '^cargo build')
    sign=$(first_call '^codesign --force'); staple=$(first_call '^xcrun stapler staple')
    if [ -n "$ident" ] && [ -n "$build" ] && [ -n "$sign" ] && [ -n "$staple" ] &&
       [ "$ident" -lt "$build" ] && [ "$build" -lt "$sign" ] && [ "$sign" -lt "$staple" ]; then
        pass "release.sh alone checks the identity, builds, signs and staples"
    else fail "release.sh alone: identity at ${ident:-never}, build at ${build:-never}, sign at ${sign:-never}, staple at ${staple:-never}"; fi
    if [ "$(grep -c '^codesign --force' "$WORK/calls")" -eq 1 ]; then
        pass "release.sh alone signs the bundle once"
    else fail "release.sh alone signed $(grep -c '^codesign --force' "$WORK/calls") times"; fi
else
    fail "release.sh with no arguments failed:"; cat "$WORK/out" >&2
fi

if run "$WORK/laptop/apps/macos/release.sh" --bogus; then
    fail "release.sh accepted --bogus"
else pass "release.sh refuses an unknown argument"; fi

if [ "$FAILED" -ne 0 ]; then echo "apps/macos/test.sh: FAILED" >&2; exit 1; fi
echo "apps/macos/test.sh: all passed"
