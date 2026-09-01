#!/bin/sh
#
# Installs bahn — the autobahn command — and the agent bundle it uses to
# bootstrap remote hosts.
#
#   curl -fsSL https://raw.githubusercontent.com/fny/autobahn/master/scripts/install.sh | sh
#
# or, having cloned the repository:
#
#   scripts/install.sh [--prefix DIR] [--version TAG] [--no-agents]
#
# The command goes on your PATH; the agents go under ~/.autobahn, which is
# where autobahn keeps everything else it owns. The agents are what let a
# controller install itself onto a host of a *different* platform — a Mac
# driving Linux servers, say. They are installed by default because the
# alternative is discovering they are missing months later, on the day you
# add a host of a new architecture.
#
set -eu

REPO="fny/autobahn"
PREFIX="${BAHN_PREFIX:-$HOME/.local/bin}"
STATE_HOME="${BAHN_HOME:-$HOME/.autobahn}"
VERSION="latest"
WITH_AGENTS=1

usage() {
    cat <<'USAGE'
Usage: install.sh [options]

  --prefix DIR    Install the command here (default: ~/.local/bin,
                  or $BAHN_PREFIX)
  --version TAG   Install this release rather than the latest one
  --no-agents     Skip the agent bundle. Only safe when every host you
                  synchronize with shares this machine's platform; the
                  bundle can be added later by re-running this script.
  --help          Show this message
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) PREFIX="${2:?--prefix needs a directory}"; shift 2 ;;
        --version) VERSION="${2:?--version needs a tag}"; shift 2 ;;
        --no-agents) WITH_AGENTS=0; shift ;;
        --help|-h) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

say() { printf '%s\n' "$*"; }
die() { printf 'install: %s\n' "$*" >&2; exit 1; }

# ── the tools this script needs ─────────────────────────────────────
have() { command -v "$1" >/dev/null 2>&1; }

if have curl; then
    # --fail so an HTML error page never lands in a file we then execute.
    http() { curl -fsSL -o "$2" "$1"; }
elif have wget; then
    http() { wget -q -O "$2" "$1"; }
else
    die "neither curl nor wget is available"
fi

# Downloads one release asset by name.
#
# A private repository's browser download URL returns 404 even with a
# token — only the API serves its assets — so the GitHub CLI is preferred
# when it is present and authenticated. Public releases need no
# credentials at all, and fall through to the plain URL.
if have gh && gh auth status >/dev/null 2>&1; then
    fetch() {
        gh release download ${VERSION_TAG:+"$VERSION_TAG"} \
            --repo "$REPO" --pattern "$1" --output "$2" --clobber >/dev/null 2>&1
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

# ── which build ─────────────────────────────────────────────────────
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

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT INT TERM

say "Installing bahn ($VERSION) for $PLATFORM"

# Releases published before the command was renamed carry the old asset
# names; try the current ones first and fall back, so this script works
# across the rename in both directions.
NAME=""
for candidate in bahn autobahn; do
    if fetch "$candidate-$PLATFORM" "$WORK/bahn" 2>/dev/null; then
        NAME="$candidate"
        break
    fi
done
if [ -z "$NAME" ]; then
    if have gh; then
        die "unable to download the $PLATFORM build for $REPO ($VERSION).
Check that the release exists and that 'gh auth status' succeeds."
    fi
    die "unable to download the $PLATFORM build from $BASE.
If $REPO is private, install the GitHub CLI and run 'gh auth login' —
private release assets are not served over plain download URLs."
fi

# ── verify before installing ────────────────────────────────────────
# A checksum file is only evidence if it came from the release; it is
# fetched separately and the binary is compared against its entry. If the
# release publishes no checksums, say so rather than pretending.
if fetch "SHA256SUMS" "$WORK/SHA256SUMS" 2>/dev/null; then
    expected="$(grep " \{1,2\}$NAME-$PLATFORM\$" "$WORK/SHA256SUMS" | cut -d' ' -f1 || true)"
    if [ -z "$expected" ]; then
        die "SHA256SUMS has no entry for $NAME-$PLATFORM"
    fi
    actual="$(checksum "$WORK/bahn")"
    [ "$expected" = "$actual" ] || die "checksum mismatch for $NAME-$PLATFORM
  expected $expected
  actual   $actual
Refusing to install. Retry, or report this."
    say "  checksum verified"
else
    say "  warning: the release publishes no SHA256SUMS; installing unverified"
fi

# ── the command ─────────────────────────────────────────────────────
mkdir -p "$PREFIX"
chmod 755 "$WORK/bahn"
# Written to a temporary alongside the target and renamed, so a running
# bahn is never a half-written file.
mv "$WORK/bahn" "$PREFIX/.bahn.install.$$"
mv "$PREFIX/.bahn.install.$$" "$PREFIX/bahn"
say "  installed $PREFIX/bahn"

# ── the agents ──────────────────────────────────────────────────────
if [ "$WITH_AGENTS" -eq 1 ]; then
    if fetch "$NAME-agents.tar.gz" "$WORK/agents.tar.gz" 2>/dev/null; then
        mkdir -p "$STATE_HOME"
        # The archive carries a top-level agents/ directory, so this lands
        # at $STATE_HOME/agents. Extracted to a temporary first and swapped
        # in, so an interrupted download cannot leave a half-populated
        # bundle that a later connection would try to install from.
        rm -rf "$WORK/extract" && mkdir -p "$WORK/extract"
        tar xzf "$WORK/agents.tar.gz" -C "$WORK/extract"
        [ -d "$WORK/extract/agents" ] || die "the agent bundle has an unexpected layout"
        rm -rf "$STATE_HOME/agents.previous"
        [ -d "$STATE_HOME/agents" ] && mv "$STATE_HOME/agents" "$STATE_HOME/agents.previous"
        mv "$WORK/extract/agents" "$STATE_HOME/agents"
        rm -rf "$STATE_HOME/agents.previous"
        count="$(ls "$STATE_HOME/agents" | wc -l | tr -d ' ')"
        say "  installed $count agents in $STATE_HOME/agents"
    else
        say "  warning: no agent bundle in this release; hosts on other"
        say "           platforms than $PLATFORM cannot be bootstrapped"
    fi
else
    say "  skipped the agent bundle (--no-agents)"
fi

# ── is it reachable? ────────────────────────────────────────────────
case ":$PATH:" in
    *":$PREFIX:"*) ;;
    *)
        say ""
        say "$PREFIX is not on your PATH. Add it:"
        say ""
        say "    export PATH=\"$PREFIX:\$PATH\""
        say ""
        ;;
esac

say ""
say "Done. Next:"
say ""
say "    bahn sync ~/project user@host:/srv/project     # try a pairing"
say "    bahn --help                                    # everything else"
say ""
say "For continuous synchronization, describe your groups in"
say "$STATE_HOME/config.toml and run 'bahn up'."
