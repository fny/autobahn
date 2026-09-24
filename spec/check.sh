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
# Needs Java 11+ and tla2tools.jar: set TLA2TOOLS, or it is fetched into
# ~/.local/lib on first use.
#
# A mode passes only if TLC exits 0, prints no violation, error or
# deadlock, and says it finished: TLC exits 0 on some failures (a missing
# config, for one), and prints "Error" on others that exit nonzero.
set -u -o pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
JAR="${TLA2TOOLS:-$HOME/.local/lib/tla2tools.jar}"
if [ ! -f "$JAR" ]; then
    mkdir -p "$(dirname "$JAR")"
    curl -fsSL -o "$JAR" https://github.com/tlaplus/tlaplus/releases/latest/download/tla2tools.jar || exit 1
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
