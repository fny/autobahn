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
# real build would; actool is missing, as without Xcode 26; there is no
# signing identity, so a signed build falls back to ad-hoc.
stubs() {
    local bin="$1"
    mkdir -p "$bin"
    for tool in cargo xcrun xattr codesign security; do
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
    echo 'exit 1' >> "$bin/xcrun"
}

plist_value() {
    # The <string> after a key, in the one-key-per-line layout Info.plist uses.
    sed -n "/<key>$2<\/key>/{n;s/.*<string>\(.*\)<\/string>.*/\1/p;}" "$1"
}

stubs "$WORK/bin"

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
if PATH="$WORK/bin:$PATH" "$WORK/signed/apps/macos/build.sh" > "$WORK/out" 2>&1; then
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
if PATH="$WORK/bin:$PATH" "$WORK/noversion/apps/macos/build.sh" > "$WORK/out" 2>&1; then
    fail "build.sh built with no version in Cargo.toml"
else
    pass "build.sh refuses a Cargo.toml with no package version"
fi

# Nor is a version that would not survive being written into XML.
tree "$WORK/hostile" '1.0.0</string>'
if PATH="$WORK/bin:$PATH" "$WORK/hostile/apps/macos/build.sh" > "$WORK/out" 2>&1; then
    fail "build.sh built with the version '1.0.0</string>'"
else
    pass "build.sh refuses a version with markup in it"
fi

if [ "$FAILED" -ne 0 ]; then echo "apps/macos/test.sh: FAILED" >&2; exit 1; fi
echo "apps/macos/test.sh: all passed"
