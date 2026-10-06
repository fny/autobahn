#!/bin/sh
# Writes the Debian or Ubuntu packages a Linux binary needs at run time, in
# the form the Linux archives ship as APT_REQUIREMENTS.txt:
#
#   scripts/apt-requirements.sh target/release/autobahn-app stage/APT_REQUIREMENTS.txt
#
# Two kinds of library are read off the binary: the ones it is linked
# against (its NEEDED entries, not the whole closure ldd would print) and
# the ones it opens by name while running, which ldd never sees: Wayland
# and EGL for the window, and the Vulkan loader, which wgpu asks for by
# name. Each is found on this machine and traced to the package that owns
# it; apt brings in what those packages need. The build image's
# distribution, architecture and package versions go in the header, with
# the commit from AUTOBAHN_COMMIT when the build set one, so a reader knows
# exactly what the list was true of.
#
# Linux only: it asks dpkg, so it runs where the binary was built.
set -eu

binary=$1
out=$2

# The path the loader would use for a library, from ldd's view of this
# machine. The loader itself and the vdso have no "=>" line, and the
# loader is libc's anyway.
resolve() {
    ldd "$binary" | awk -v want="$1" '$1 == want && $2 == "=>" && $3 ~ /^\// {print $3}'
}

packages=
for soname in $(objdump -p "$binary" | awk '/NEEDED/ {print $2}'); do
    case "$soname" in
        ld-linux*|ld-*|linux-vdso*) continue ;;
    esac
    path=$(resolve "$soname")
    if [ -z "$path" ]; then
        echo "no library on this machine for $soname" >&2
        exit 1
    fi
    # dpkg prints "libgtk-4-1:amd64: /usr/lib/..."; the package is before
    # the first colon. On a merged-/usr system ldd prints /lib/... and
    # dpkg knows the file under /usr/lib, so the real path is the second
    # try.
    owner=$(dpkg -S "$path" 2>/dev/null | head -n 1 | cut -d: -f1)
    if [ -z "$owner" ]; then
        owner=$(dpkg -S "$(realpath "$path")" 2>/dev/null | head -n 1 | cut -d: -f1)
    fi
    if [ -z "$owner" ]; then
        echo "no package owns $path ($soname)" >&2
        exit 1
    fi
    packages="$packages $owner"
done

# The package that owns a library the loader knows by this name, or
# nothing: a name the build image has no library for is not listed, since
# these are opened when wanted rather than at start.
owner_by_name() {
    path=$(ldconfig -p | awk -v want="$1" '$1 == want {print $NF; exit}')
    [ -n "$path" ] || return 0
    owner=$(dpkg -S "$path" 2>/dev/null | head -n 1 | cut -d: -f1)
    if [ -z "$owner" ]; then
        owner=$(dpkg -S "$(realpath "$path")" 2>/dev/null | head -n 1 | cut -d: -f1)
    fi
    echo "$owner"
}
# Names written into the binary, plus the Vulkan loader, which ash asks
# for through its own path. An unversioned name is a development alias of
# a versioned one that is listed beside it.
by_name=$(strings -n 6 "$binary" | grep -oE 'lib[A-Za-z0-9_+-]*\.so(\.[0-9]+)*' | sort -u)
for soname in $by_name libvulkan.so.1; do
    case "$soname" in
        *.so) continue ;;
    esac
    owner=$(owner_by_name "$soname")
    if [ -n "$owner" ]; then
        packages="$packages $owner"
    fi
done
# shellcheck disable=SC2086
packages=$(printf '%s\n' $packages | sort -u)

# Which part of the app a package serves: GTK and what comes with it is
# the menu bar item; everything else is the window.
section_of() {
    case "$1" in
        libgtk-*|libglib*|libgobject*|libgio*|libgdk*|libgraphene*|libpango*|libcairo*|libharfbuzz*) echo tray ;;
        *) echo app ;;
    esac
}
list() {
    for package in $packages; do
        if [ "$(section_of "$package")" = "$1" ]; then
            echo "$package"
        fi
    done
}

distribution=$(. /etc/os-release && echo "$PRETTY_NAME")
architecture=$(dpkg --print-architecture)
commit=${AUTOBAHN_COMMIT:-}
built_from=
if [ -n "$commit" ]; then
    built_from=" from commit $(echo "$commit" | cut -c1-7)"
fi

{
    cat <<HEADER
# $distribution ($architecture) packages the autobahn-app binary needs at run time.
#
# After unpacking the archive, install these before running ./autobahn-app:
#
#   grep -v '^#' APT_REQUIREMENTS.txt | xargs sudo apt-get install -y
#
# Generated at build time by reading the shared libraries autobahn-app is
# linked against, and the ones it opens by name while running, and asking
# dpkg which package owns each one (scripts/apt-requirements.sh in the
# repository). Debian and other Ubuntu releases use the same names, give
# or take a suffix.
#
# Aside from the packages here you also need:
#   - a Vulkan driver for your GPU: mesa-vulkan-drivers for Intel and AMD,
#     the vendor's driver for NVIDIA
#   - a graphical session, Wayland or X11
#
# Built$built_from on $distribution ($architecture).
#
# The package versions on the build image were:
HEADER
    for package in $packages; do
        dpkg-query -W -f='#   ${Package} ${Version}\n' "$package"
    done
    printf '\n# App\n'
    list app
    printf '\n# Tray item\n'
    list tray
} > "$out"

echo "$out: $(echo "$packages" | wc -l | tr -d ' ') packages"
