# shellcheck shell=bash
# Sourced by build.sh: the version the app claims, and the Info.plist that
# claims it. Plain shell and sed, no Apple tools, so apps/macos/test.sh
# can check both on any machine.

# Prints the [package] version from the Cargo.toml given. Only that table's:
# a dependency's `version` key is somebody else's number.
crate_version() {
    local manifest="$1" version
    version=$(awk '
        /^[[:space:]]*\[/ { in_package = ($0 ~ /^[[:space:]]*\[package\][[:space:]]*$/); next }
        in_package && /^[[:space:]]*version[[:space:]]*=/ {
            sub(/^[^=]*=[[:space:]]*"/, ""); sub(/".*$/, ""); print; exit
        }' "$manifest")
    # A value that is written into XML and through sed has to be plain:
    # digits, letters and the separators a semantic version uses.
    if [[ ! "$version" =~ ^[0-9][0-9A-Za-z.+-]*$ ]]; then
        echo "no usable [package] version in $manifest: '$version'" >&2
        return 1
    fi
    printf '%s\n' "$version"
}

# Writes the bundle's Info.plist from the committed template, with the
# version and the icon filled in. The template carries placeholders, not a
# number, so a copy that skipped this step is visibly wrong rather than
# quietly a release behind.
render_info_plist() {
    local template="$1" out="$2" version="$3" icon="$4"
    sed -e "s/__AUTOBAHN_VERSION__/$version/g" \
        -e "s/__AUTOBAHN_ICON__/$icon/g" "$template" > "$out"
    if grep -q '__AUTOBAHN_[A-Z]*__' "$out"; then
        echo "a placeholder is left in $out" >&2
        return 1
    fi
}
