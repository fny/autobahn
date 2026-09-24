#!/bin/bash
# check-test.sh — check.sh's own tests: it must fail when TLC fails.
#
#   spec/check-test.sh
#
# Runs check.sh against a stub `java` that plays TLC's part, so it needs
# neither Java nor the real jar, and takes a second. CI runs it before the
# real check, so a check.sh that passes everything cannot pass there.
set -u -o pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# The stub prints $STUB_OUTPUT and exits with $STUB_EXIT, whatever it is
# asked to run.
mkdir "$WORK/bin"
cat > "$WORK/bin/java" <<'STUB'
#!/bin/bash
printf '%s\n' "$STUB_OUTPUT"
exit "$STUB_EXIT"
STUB
chmod +x "$WORK/bin/java"
# A jar that exists, so check.sh fetches nothing.
printf 'not a jar\n' > "$WORK/tla2tools.jar"

FINISHED='Model checking completed. No error has been found.
91433 states generated, 30179 distinct states found, 0 states left on queue.
Finished in 14s at (2026-09-24 06:46:52)'
VIOLATED='Error: Invariant NoLostUpdate is violated.
91433 states generated, 30179 distinct states found, 0 states left on queue.
Finished in 14s at (2026-09-24 06:46:52)'
CRASHED='Error: TLC threw an unexpected exception.
Finished in 00s at (2026-09-24 06:46:33)'

failures=0
# expect <pass|fail> <description> <stub output> <stub exit> <check.sh args...>
expect() {
    local want=$1 what=$2 output=$3 code=$4; shift 4
    local got
    if PATH="$WORK/bin:$PATH" TLA2TOOLS="$WORK/tla2tools.jar" \
        STUB_OUTPUT="$output" STUB_EXIT="$code" \
        "$HERE/check.sh" "$@" > "$WORK/out" 2>&1; then
        got=pass
    else
        got=fail
    fi
    if [ "$got" = "$want" ]; then
        echo "ok    $what"
    else
        echo "WRONG $what: check.sh should $want, but did $got"
        sed 's/^/      | /' "$WORK/out"
        failures=$((failures + 1))
    fi
}

expect fail "a violation with a nonzero exit fails" "$VIOLATED" 17 quick
expect fail "a violation with exit 0 fails" "$VIOLATED" 0 quick
expect fail "an error with exit 0 fails" "$CRASHED" 0 quick
expect fail "no finish with exit 0 fails" "Starting..." 0 quick
expect fail "a nonzero exit with a normal finish fails" "$FINISHED" 1 quick
expect pass "a normal finish passes" "$FINISHED" 0 quick
expect fail "one failing mode among several fails" "$VIOLATED" 0 quick strict

mkdir "$WORK/empty" "$WORK/traces"
expect fail "--traces on an empty directory fails" "$FINISHED" 0 --traces "$WORK/empty"
expect fail "--traces on a missing directory fails" "$FINISHED" 0 --traces "$WORK/missing"
expect fail "--traces without a directory fails" "$FINISHED" 0 --traces
touch "$WORK/traces/Trace0.tla" "$WORK/traces/Trace0.cfg"
expect pass "--traces with an accepted trace passes" "$FINISHED" 0 --traces "$WORK/traces"
expect fail "--traces with a violating trace at exit 0 fails" "$VIOLATED" 0 --traces "$WORK/traces"
expect fail "--traces with a nonzero exit fails" "$FINISHED" 12 --traces "$WORK/traces"

if [ $failures -ne 0 ]; then
    echo "$failures case(s) wrong"
    exit 1
fi
echo "check.sh fails whenever TLC does"
