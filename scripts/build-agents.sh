#!/bin/sh
# Builds the agent bundle: one binary per platform, named
# autobahn-<os>-<arch>, in dist/agents/. The controller locates this
# directory via AUTOBAHN_AGENTS_DIR or as an `agents` directory beside its
# own executable, and installs the right binary onto remote hosts
# automatically on first connect.
#
# Linux agents are built against musl and statically linked, so one binary
# runs on any distribution regardless of libc version. Targets whose
# toolchains aren't installed are skipped with a note (macOS agents can only
# be built on macOS; add the targets there with `rustup target add`).
set -eu
cd "$(dirname "$0")/.."

OUT="dist/agents"
mkdir -p "$OUT"

build() {
    target="$1"
    platform="$2"
    if ! rustup target list --installed | grep -q "^$target$"; then
        echo "skipping $platform ($target not installed; add with: rustup target add $target)"
        return 0
    fi
    echo "building $platform..."
    if ! cargo build --release --target "$target"; then
        echo "skipping $platform (build failed; a cross linker may be required)"
        return 0
    fi
    cp "target/$target/release/autobahn" "$OUT/autobahn-$platform"
    echo "  -> $OUT/autobahn-$platform"
}

build x86_64-unknown-linux-musl linux-x86_64
build aarch64-unknown-linux-musl linux-aarch64
build x86_64-apple-darwin darwin-x86_64
build aarch64-apple-darwin darwin-aarch64

echo "bundle contents:"
ls -l "$OUT"
