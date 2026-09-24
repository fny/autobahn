#!/bin/sh
#
# Installs autobahn and the agent bundle it uses to bootstrap remote
# hosts.
#
#   curl -fsSL https://github.com/fny/autobahn/releases/latest/download/install.sh | sh
#
# or, having cloned the repository:
#
#   scripts/install.sh [--bin-dir DIR] [--version TAG] [--no-agents] [--insecure]
#
# The command goes on your PATH; the agents go under ~/.autobahn, which is
# where autobahn keeps everything else it owns. The agents are what let a
# controller install itself onto a host of a *different* platform — a Mac
# driving Linux servers, say. They are installed by default because the
# alternative is discovering they are missing months later, on the day you
# add a host of a new architecture.
#
# Every download is checked against the release's SHA256SUMS before
# anything is installed, and a SHA256SUMS that cannot be had stops the
# install. Only --insecure (AUTOBAHN_INSECURE=1 in the piped form) lets an
# old release that publishes no checksums through, and it never excuses a
# checksum that does not match.
#
set -eu

REPO="fny/autobahn"
# The earlier name, AUTOBAHN_PREFIX, is still honoured: it was never a
# prefix in the GNU sense (the binary went straight into it, not into
# its bin/), which is why it was renamed.
BIN_DIR="${AUTOBAHN_BIN_DIR:-${AUTOBAHN_PREFIX:-$HOME/.local/bin}}"
STATE_HOME="${AUTOBAHN_HOME:-$HOME/.autobahn}"
VERSION="latest"
WITH_AGENTS=1
INSECURE="${AUTOBAHN_INSECURE:-0}"

usage() {
    cat <<'USAGE'
Usage: install.sh [options]

  --bin-dir DIR   Install the command here (default: ~/.local/bin,
                  or $AUTOBAHN_BIN_DIR)
  --version TAG   Install this release rather than the latest one
  --no-agents     Skip the agent bundle. Only safe when every host you
                  synchronize with shares this machine's platform; the
                  bundle can be added later by re-running this script.
  --insecure      Install even when the release's SHA256SUMS cannot be
                  downloaded (AUTOBAHN_INSECURE=1 does the same). Only for
                  an old release that publishes no checksums: nothing then
                  checks that the files are the ones that were published.
  --help          Show this message
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        --bin-dir|--prefix) BIN_DIR="${2:?$1 needs a directory}"; shift 2 ;;
        --version) VERSION="${2:?--version needs a tag}"; shift 2 ;;
        --no-agents) WITH_AGENTS=0; shift ;;
        --insecure) INSECURE=1; shift ;;
        --help|-h) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

say() { printf '%s\n' "$*"; }
die() { printf 'install: %s\n' "$*" >&2; exit 1; }
warn() { printf 'install: %s\n' "$*" >&2; }

# the tools this script needs
have() { command -v "$1" >/dev/null 2>&1; }

# Every download returns 0 when it got the file, 4 when the server said
# the file does not exist (a 404), and 1 for anything else. The difference
# matters for SHA256SUMS: a release that never published one is an old
# release, and a download that failed is a network, a proxy, or somebody
# blocking that one file. Whatever did not arrive whole is removed, so an
# error page never lands in a file that is then checksummed or run.
if have curl; then
    http() {
        code="$(curl -sSL -o "$2" -w '%{http_code}' "$1" 2>/dev/null)" || code="000"
        case "$code" in
            2??) return 0 ;;
            404) rm -f "$2"; return 4 ;;
            *) rm -f "$2"; return 1 ;;
        esac
    }
elif have wget; then
    http() {
        if wget -S -O "$2" "$1" 2>"$2.log"; then
            rm -f "$2.log"
            return 0
        fi
        missing=0
        grep -q 'HTTP/[0-9.]* 404' "$2.log" && missing=1
        rm -f "$2" "$2.log"
        [ "$missing" -eq 1 ] && return 4
        return 1
    }
else
    die "neither curl nor wget is available"
fi

# Downloads one release asset by name.
#
# The GitHub CLI is used when it is present and logged in, and the plain
# release URL otherwise. Both serve the same assets; the plain URL needs
# no credentials at all. AUTOBAHN_RELEASE_BASE replaces the release URL
# (a mirror, or a test) and always uses it.
if [ -z "${AUTOBAHN_RELEASE_BASE:-}" ] && have gh && gh auth status >/dev/null 2>&1; then
    fetch() {
        if gh release download ${VERSION_TAG:+"$VERSION_TAG"} \
            --repo "$REPO" --pattern "$1" --output "$2" --clobber >/dev/null 2>"$2.log"; then
            rm -f "$2.log"
            return 0
        fi
        missing=0
        grep -qi 'no assets match' "$2.log" && missing=1
        rm -f "$2" "$2.log"
        [ "$missing" -eq 1 ] && return 4
        return 1
    }
else
    fetch() { http "$BASE/$1" "$2"; }
fi

have tar || die "tar is not available"

if have sha256sum; then
    checksum() { sha256sum "$1" | cut -d' ' -f1; }
elif have shasum; then
    checksum() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
    die "neither sha256sum nor shasum is available"
fi

# which build
# The same mapping the controller uses when it probes a remote host, so
# the name resolved here is the name the release publishes.
system="$(uname -s | tr '[:upper:]' '[:lower:]')"
machine="$(uname -m)"
case "$machine" in
    arm64) machine="aarch64" ;;
    amd64) machine="x86_64" ;;
esac
PLATFORM="$system-$machine"

case "$PLATFORM" in
    linux-x86_64|linux-aarch64|darwin-x86_64|darwin-aarch64) ;;
    *) die "no build for $PLATFORM (builds exist for linux and darwin on x86_64 and aarch64)" ;;
esac

if [ "$VERSION" = "latest" ]; then
    BASE="https://github.com/$REPO/releases/latest/download"
    VERSION_TAG=""
else
    BASE="https://github.com/$REPO/releases/download/$VERSION"
    VERSION_TAG="$VERSION"
fi
BASE="${AUTOBAHN_RELEASE_BASE:-$BASE}"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT INT TERM

say "Installing autobahn ($VERSION) for $PLATFORM"

status=0
fetch "autobahn-$PLATFORM" "$WORK/autobahn" || status=$?
if [ "$status" -ne 0 ]; then
    if [ -z "${AUTOBAHN_RELEASE_BASE:-}" ] && have gh; then
        die "unable to download the $PLATFORM build for $REPO ($VERSION).
Check that the release exists and that 'gh auth status' succeeds."
    fi
    die "unable to download the $PLATFORM build from $BASE.
Check that the release exists."
fi

# The bundle is downloaded before anything is installed, so it is verified
# with the binary and a bad one installs nothing at all. It is the asset
# that matters most: the controller uploads it to every remote host and
# runs it there.
AGENTS=0
if [ "$WITH_AGENTS" -eq 1 ]; then
    status=0
    fetch "autobahn-agents.tar.gz" "$WORK/agents.tar.gz" || status=$?
    case "$status" in
        0) AGENTS=1 ;;
        4)
            say "  warning: no agent bundle in this release; hosts on other"
            say "           platforms than $PLATFORM cannot be bootstrapped"
            ;;
        *) die "unable to download the agent bundle from $BASE. Retry, or pass --no-agents." ;;
    esac
fi

# verify before installing
# A checksum file is only evidence if it came from the release; it is
# fetched separately and every download is compared against its own entry,
# matched by exact name. A checksum file that cannot be had stops here:
# blocking that one file must not be a way to install unverified bytes.
status=0
fetch "SHA256SUMS" "$WORK/SHA256SUMS" || status=$?
if [ "$status" -eq 0 ]; then
    # Prints the entry for exactly this asset name. `sha256sum -b` writes
    # the name as `*name`, and that is the same entry.
    expected_for() {
        awk -v name="$1" '{ entry = $2; sub(/^\*/, "", entry) }
            entry == name && $1 ~ /^[0-9a-fA-F]+$/ && length($1) == 64 { print $1; exit }' \
            "$WORK/SHA256SUMS"
    }
    verify() {
        expected="$(expected_for "$2")"
        [ -n "$expected" ] || die "SHA256SUMS has no entry for $2. Refusing to install."
        actual="$(checksum "$1")"
        [ "$expected" = "$actual" ] || die "checksum mismatch for $2
  expected $expected
  actual   $actual
Refusing to install. Retry, or report this."
    }
    verify "$WORK/autobahn" "autobahn-$PLATFORM"
    [ "$AGENTS" -eq 1 ] && verify "$WORK/agents.tar.gz" "autobahn-agents.tar.gz"
    say "  checksums verified"
else
    if [ "$status" -eq 4 ]; then
        why="the release publishes no SHA256SUMS"
    else
        why="unable to download SHA256SUMS from $BASE"
    fi
    if [ "$INSECURE" != "1" ]; then
        if [ "$status" -eq 4 ]; then
            die "$why, so nothing can be verified. Refusing to install.
Only an old release lacks checksums. To install one anyway, pass --insecure
(or set AUTOBAHN_INSECURE=1): nothing will then check that the files are the
ones that were published."
        fi
        die "$why. Refusing to install unverified files: retry.
The download failed rather than being missing, which a network, a proxy or
somebody blocking that one file can cause."
    fi
    warn "WARNING: $why."
    warn "WARNING: --insecure: installing UNVERIFIED files. Nothing checks that"
    warn "WARNING: they are the ones that were published."
fi

# The bundle's members are checked before it is unpacked: every one stays
# under agents/. Current tar refuses absolute and `..` names by itself;
# checking here says so, and covers the tar versions that do not.
if [ "$AGENTS" -eq 1 ]; then
    members="$(tar tzf "$WORK/agents.tar.gz")" || die "the agent bundle is not a readable archive"
    printf '%s\n' "$members" > "$WORK/members"
    while IFS= read -r member; do
        case "$member" in
            /*|..|../*|*/..|*/../*)
                die "the agent bundle has a member that would land outside its directory: $member" ;;
            agents|agents/*|./agents|./agents/*) ;;
            *) die "the agent bundle has an unexpected layout (member $member)" ;;
        esac
    done < "$WORK/members"
fi

# the command
mkdir -p "$BIN_DIR"
chmod 755 "$WORK/autobahn"
# Written to a temporary alongside the target and renamed, so a running
# autobahn is never a half-written file.
mv "$WORK/autobahn" "$BIN_DIR/.autobahn.install.$$"
mv "$BIN_DIR/.autobahn.install.$$" "$BIN_DIR/autobahn"
say "  installed $BIN_DIR/autobahn"

# the agents
if [ "$AGENTS" -eq 1 ]; then
    mkdir -p "$STATE_HOME"
    # The archive carries a top-level agents/ directory, so this lands
    # at $STATE_HOME/agents. Extracted to a temporary first and swapped
    # in, so an interrupted download cannot leave a half-populated
    # bundle that a later connection would try to install from.
    rm -rf "$WORK/extract" && mkdir -p "$WORK/extract"
    tar xzf "$WORK/agents.tar.gz" -C "$WORK/extract"
    [ -d "$WORK/extract/agents" ] || die "the agent bundle has an unexpected layout"
    rm -rf "$STATE_HOME/agents.previous"
    if [ -d "$STATE_HOME/agents" ]; then
        mv "$STATE_HOME/agents" "$STATE_HOME/agents.previous"
    fi
    mv "$WORK/extract/agents" "$STATE_HOME/agents"
    rm -rf "$STATE_HOME/agents.previous"
    count="$(find "$STATE_HOME/agents" -type f -name 'autobahn-*' | wc -l | tr -d ' ')"
    say "  installed $count agents in $STATE_HOME/agents"
elif [ "$WITH_AGENTS" -eq 0 ]; then
    say "  skipped the agent bundle (--no-agents)"
fi

# is it reachable?
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *)
        say ""
        say "$BIN_DIR is not on your PATH. Add it:"
        say ""
        say "    export PATH=\"$BIN_DIR:\$PATH\""
        say ""
        ;;
esac

# a starting configuration, the first time
# A fresh install gets the template `autobahn init` writes: every mode
# explained, one example group commented out, so the next step is to edit
# a file rather than to find out what goes in one. An existing
# configuration is never touched.
if [ ! -e "$STATE_HOME/config.toml" ]; then
    if AUTOBAHN_HOME="$STATE_HOME" "$BIN_DIR/autobahn" init >/dev/null 2>&1; then
        say "  wrote a starting configuration to $STATE_HOME/config.toml"
    fi
fi

say ""
say "Done. Next:"
say ""
say "    ${EDITOR:-vi} $STATE_HOME/config.toml                    # describe what stays in sync"
say "    autobahn watch                                     # and run it"
say "    autobahn sync ~/project user@host:/srv/project     # or try one pairing"
say "    autobahn --help                                    # everything else"
