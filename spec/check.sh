#!/bin/bash
# check.sh — model-check spec/Autobahn.tla in every mode, with TLC.
#
#   spec/check.sh              all three modes, two betas, every property
#   spec/check.sh strict       one mode
#   spec/check.sh quick        the star at three edits, half a minute: CI's check
#   spec/check.sh strict_n3    three betas under symmetry: invariants only
#   spec/check.sh peering_conflict_safety   the failover protocol (Peering.tla)
#   spec/check.sh --traces DIR validate replay traces (see tests/spec_replay.rs)
#
# Needs Java 11+ and the tla2tools.jar that spec/tla2tools.version pins.
# It is fetched into ~/.local/lib on first use, or set TLA2TOOLS to a copy
# of your own. Either way a jar whose SHA-256 is not the pinned one is
# refused; TLA2TOOLS_TRUST=1 runs the one TLA2TOOLS names regardless.
#
#   spec/check.sh --fetch      only fetch and verify the jar, and say which
#
# A mode passes only if TLC exits 0, prints no violation, error or
# deadlock, and says it finished: TLC exits 0 on some failures (a missing
# config, for one), and prints "Error" on others that exit nonzero.
set -u -o pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
pin() { sed -n "s/^$1=//p" "$HERE/tla2tools.version"; }
VERSION="$(pin version)"; URL="$(pin url)"; SHA256="$(pin sha256)"
if [ -z "$VERSION" ] || [ -z "$URL" ] || [ -z "$SHA256" ]; then
    echo "spec/tla2tools.version must give version, url and sha256" >&2; exit 2
fi
sha256() {
    if command -v sha256sum > /dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1
}
# verify JAR — whether JAR holds the pinned bytes. Says why when it does not.
verify() {
    local got
    got="$(sha256 "$1")"
    [ "$got" = "$SHA256" ] && return 0
    echo "refusing $1: its sha256 is $got," >&2
    echo "  but spec/tla2tools.version pins tla2tools $VERSION at $SHA256." >&2
    echo "  Delete it to fetch the pinned jar, or run it anyway with TLA2TOOLS=$1 TLA2TOOLS_TRUST=1." >&2
    return 1
}
JAR="${TLA2TOOLS:-$HOME/.local/lib/tla2tools-$VERSION.jar}"
if [ ! -f "$JAR" ]; then
    mkdir -p "$(dirname "$JAR")" || exit 1
    part="$(mktemp "$JAR.XXXXXX")" || exit 1
    if ! curl -fsSL -o "$part" "$URL"; then rm -f "$part"; exit 1; fi
    verify "$part" || { rm -f "$part"; exit 1; }
    mv "$part" "$JAR" || exit 1
elif [ -n "${TLA2TOOLS:-}" ] && [ "${TLA2TOOLS_TRUST:-}" = 1 ]; then
    echo "running $JAR unverified: TLA2TOOLS_TRUST=1" >&2
else
    verify "$JAR" || exit 1
fi
if [ "${1:-}" = "--fetch" ]; then
    echo "tla2tools $VERSION ($SHA256) at $JAR"
    exit 0
fi
tlc() { java -XX:+UseParallelGC -Xmx2g -cp "$JAR" tlc2.TLC -workers auto -cleanup "$@"; }
# judge LOG CODE — whether a TLC run that exited CODE, with its output in
# LOG, passed. Says why when it did not.
judge() {
    if [ "$2" -ne 0 ]; then
        echo "FAILED: TLC exited $2"; return 1
    elif grep -qE "violated|Error:|Deadlock" "$1"; then
        echo "FAILED: TLC reported an error"; return 1
    elif ! grep -q "^Finished in" "$1"; then
        echo "FAILED: TLC did not finish"; return 1
    fi
}
if [ "${1:-}" = "--traces" ]; then
    dir="${2:-}"
    [ -d "$dir" ] || { echo "no trace directory: '$dir'" >&2; exit 2; }
    failed=0; count=0
    for t in "$dir"/Trace*.tla; do
        [ -f "$t" ] || continue
        count=$((count + 1))
        cp "$HERE/Autobahn.tla" "$HERE/Reconcile.tla" "$HERE/Peering.tla" "$dir/"
        log="${t%.tla}.log"
        (cd "$dir" && tlc -config "$(basename "${t%.tla}").cfg" "$(basename "$t")") > "$log" 2>&1
        if ! judge "$log" $? > /dev/null; then
            echo "REJECTED $(basename "$t") — see $log"; failed=1
        fi
    done
    # No traces is a broken caller, not a spec every trace agrees with.
    [ $count = 0 ] && { echo "no Trace*.tla in $dir" >&2; exit 1; }
    [ $failed = 0 ] && echo "every trace ($count) is a behavior of the spec"
    exit $failed
fi
modes="${*:-conflict alpha strict}"
log="$(mktemp)"
trap 'rm -f "$log"' EXIT
status=0
for mode in $modes; do
    echo "== $mode"
    case "$mode" in
        peering_*) cfg="Peering_${mode#peering_}.cfg"; module=MCPeering.tla ;;
        *) cfg="Autobahn_$mode.cfg"; module=MC.tla ;;
    esac
    (cd "$HERE" && tlc -config "$cfg" "$module") > "$log" 2>&1
    code=$?
    grep -E "Error|violated|states generated|distinct states|Finished|Deadlock|Temporal" "$log"
    judge "$log" $code || status=1
done
exit $status
