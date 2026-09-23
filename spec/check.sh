#!/bin/bash
# check.sh — model-check spec/Autobahn.tla in every mode, with TLC.
#
#   spec/check.sh              all three modes, two betas, every property
#   spec/check.sh strict       one mode
#   spec/check.sh strict_n3    three betas under symmetry: invariants only
#   spec/check.sh --traces DIR validate replay traces (see tests/spec_replay.rs)
#
# Needs Java 11+ and tla2tools.jar: set TLA2TOOLS, or it is fetched into
# ~/.local/lib on first use.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
JAR="${TLA2TOOLS:-$HOME/.local/lib/tla2tools.jar}"
if [ ! -f "$JAR" ]; then
    mkdir -p "$(dirname "$JAR")"
    curl -fsSL -o "$JAR" https://github.com/tlaplus/tlaplus/releases/latest/download/tla2tools.jar || exit 1
fi
tlc() { java -XX:+UseParallelGC -Xmx2g -cp "$JAR" tlc2.TLC -workers auto -cleanup "$@"; }
if [ "${1:-}" = "--traces" ]; then
    dir="$2"; failed=0
    for t in "$dir"/Trace*.tla; do
        [ -f "$t" ] || continue
        cp "$HERE/Autobahn.tla" "$dir/"
        if ! (cd "$dir" && tlc -config "$(basename "${t%.tla}").cfg" "$(basename "$t")" > "${t%.tla}.log" 2>&1); then
            echo "REJECTED $(basename "$t") — see ${t%.tla}.log"; failed=1
        fi
    done
    [ $failed = 0 ] && echo "every trace is a behavior of the spec"
    exit $failed
fi
modes="${*:-conflict alpha strict}"
status=0
for mode in $modes; do
    echo "== $mode"
    (cd "$HERE" && tlc -config "Autobahn_$mode.cfg" MC.tla) | grep -E "Error|violated|states generated|distinct states|Finished|Deadlock|Temporal" || status=1
done
exit $status
