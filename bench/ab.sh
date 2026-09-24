#!/bin/bash
#
# ab.sh — the ten-minute gate: does this change move steady-state latency?
#
# Every hot-path change to autobahn is A/B measured before it ships, and
# this is the measurement. It runs two binaries in interleaved legs over
# one synthetic corpus, each leg a cold sync followed by a window of
# simulated editing agents, and reports the latency percentiles side by
# side. Interleaving (A, B, A, B, ...) is what makes the comparison honest
# on a shared machine: any drift in the machine's state lands on both.
#
# The gate exists because analysis estimates of these costs were wrong
# every time they were tried. Twice it has caught a regression that was
# argued to be free — a 40% latency cost from a guard that walked the
# tree, and 6 ms of p50 from an fsync — and once it rejected a dependency
# upgrade that stalled for six seconds on two legs in five.
#
# Usage:
#   bench/ab.sh <binary-A> <binary-B> [--legs N] [--seconds S] [--agents A]
#               [--corpus DIR] [--scale K] [--label-a NAME] [--label-b NAME]
#               [--remote HOST]
#
# With --remote, the destination is reached over SSH to HOST and the leg's
# own binary serves as the agent there, by path, so a variant under test is
# what runs on both sides and nothing is installed under ~/.autobahn/bin —
# where a real controller's agent may already live. HOST must be this
# machine (`localhost`, or an alias with latency shaped onto it): the
# destination, its manifests and the observer stay local, so the script
# checks that HOST sees this machine's files and refuses it otherwise.
# Separate hosts are the orchestrator's job (bench/orchestrate.py).
#
# A leg fails, and the script exits nonzero, if the subject or the
# observer exits early or the cold sync does not converge in ten minutes.
#
# The corpus is generated on first use (bench/corpus.py, the `code` shape,
# 40,000 files at scale 1) and reused. `--corpus DIR` uses DIR instead, as
# the pristine source: it is only ever read, and each leg copies it into a
# working corpus of its own under the script's work directory. Needs
# python3, rsync, and a Rust toolchain for the harness. Runs on macOS and
# Linux; a leg is roughly two minutes at the defaults.
#
# Each run works in a fresh private directory (mktemp -d), removed when it
# ends. The generated corpus is kept between runs in a private cache,
# $AB_CACHE (default ~/.cache/autobahn-ab), which must belong to you; the
# reports and logs of each run are kept in a private directory of their
# own, named at the end.
#
set -euo pipefail

A=""; B=""; LEGS=2; SECONDS_PER_LEG=60; AGENTS=10; SCALE=1; REMOTE=""
LABEL_A="a"; LABEL_B="b"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CACHE="${AB_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/autobahn-ab}"
SOURCE=""

usage() { awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "${BASH_SOURCE[0]}"; }

while [ $# -gt 0 ]; do
    case "$1" in
        --legs) LEGS="$2"; shift 2 ;;
        --seconds) SECONDS_PER_LEG="$2"; shift 2 ;;
        --agents) AGENTS="$2"; shift 2 ;;
        --corpus) SOURCE="$2"; shift 2 ;;
        --scale) SCALE="$2"; shift 2 ;;
        --label-a) LABEL_A="$2"; shift 2 ;;
        --label-b) LABEL_B="$2"; shift 2 ;;
        --remote) REMOTE="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        -*) echo "unknown option $1" >&2; usage >&2; exit 2 ;;
        *) if [ -z "$A" ]; then A="$1"; elif [ -z "$B" ]; then B="$1"; else echo "too many arguments" >&2; exit 2; fi; shift ;;
    esac
done
if [ -z "$A" ] || [ -z "$B" ]; then usage >&2; exit 2; fi
[ -x "$A" ] || { echo "not executable: $A" >&2; exit 2; }
[ -x "$B" ] || { echo "not executable: $B" >&2; exit 2; }
A="$(cd "$(dirname "$A")" && pwd)/$(basename "$A")"
B="$(cd "$(dirname "$B")" && pwd)/$(basename "$B")"

# The run's scratch space: private, fresh, and gone when the run ends. A
# fixed path under /tmp could have been made by someone else first, and
# the tool's configuration — agent_command included — is written here.
WORK="$(mktemp -d "${TMPDIR:-/tmp}/autobahn-ab.XXXXXX")"
WORK="$(cd "$WORK" && pwd)"
RESULTS="$(mktemp -d "${TMPDIR:-/tmp}/autobahn-ab-reports.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
# The working corpus the legs edit. It is always under $WORK, whatever
# --corpus says: a leg deletes and refills it.
CORPUS="$WORK/corpus"
if [ -n "$SOURCE" ]; then
    [ -d "$SOURCE" ] || { echo "not a directory: $SOURCE" >&2; exit 2; }
    PRISTINE="$(cd "$SOURCE" && pwd)"
    case "$PRISTINE/" in "$WORK"/*)
        echo "--corpus $SOURCE is inside the work directory $WORK, which legs delete" >&2; exit 2 ;;
    esac
    case "$WORK/" in "$PRISTINE"/*)
        echo "the work directory $WORK is inside --corpus $SOURCE, which is only read" >&2; exit 2 ;;
    esac
    PARTITIONS="$WORK/partitions-given.json"
else
    # The cache holds only what this user generated: refuse one made by
    # anyone else, and keep it private.
    mkdir -p "$CACHE"
    [ -O "$CACHE" ] || { echo "the corpus cache $CACHE is not yours; set AB_CACHE" >&2; exit 2; }
    chmod 700 "$CACHE"
    CACHE="$(cd "$CACHE" && pwd)"
    PRISTINE="$CACHE/pristine-code-$SCALE"
    PARTITIONS="$CACHE/partitions-$SCALE.json"
fi

# Deletes scratch paths, and refuses anything not strictly inside $WORK:
# a backstop, so no future edit can turn a leg's cleanup on a directory
# the script did not make.
scrub() {
    local path
    for path in "$@"; do
        case "$path" in
            "$WORK"/*) ;;
            *) echo "refusing to delete $path: it is not under $WORK" >&2; exit 1 ;;
        esac
        case "/$path/" in
            */../*) echo "refusing to delete $path: it climbs out with .." >&2; exit 1 ;;
        esac
        rm -rf -- "$path"
    done
}

# The harness: the measurement plane, compiled so its own overhead is
# small and measured rather than large and guessed.
# Existence is not the test — a build tree synchronized from another
# platform leaves a binary that cannot execute here — so the harness is
# rebuilt whenever the one present does not run.
BM="$HERE/harness/target/release/benchmark"
if ! "$BM" manifest cheap "$HERE" >/dev/null 2>&1; then
    echo "building the harness..."
    (cd "$HERE/harness" && CARGO_TARGET_DIR="$HERE/harness/target" cargo build --release >/dev/null 2>&1) \
        || { echo "harness build failed" >&2; exit 1; }
fi

# --remote runs the tool's agent over SSH but keeps everything else here,
# so HOST has to see this machine's filesystem. Proven, not assumed: HOST
# must read back a token written here a moment ago.
if [ -n "$REMOTE" ]; then
    token="$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')"
    printf '%s\n' "$token" > "$WORK/remote-check"
    seen="$(ssh -o BatchMode=yes -o ConnectTimeout=10 "$REMOTE" cat "$(printf %q "$WORK/remote-check")" 2>/dev/null || true)"
    rm -f "$WORK/remote-check"
    if [ "$seen" != "$token" ]; then
        echo "--remote $REMOTE: that host does not see this machine's files (or ssh failed)." >&2
        echo "ab.sh keeps the destination, its checks and the observer local, so --remote" >&2
        echo "only supports this machine under another name; bench/orchestrate.py covers" >&2
        echo "separate hosts." >&2
        exit 2
    fi
fi

# The corpus, generated once and kept pristine; every leg restores it, so
# the agents' edits from one leg never leak into the next.
# Generated inside the cache and renamed into place, so a run that is
# interrupted never leaves a half-written corpus to be reused.
if [ ! -d "$PRISTINE" ]; then
    echo "generating the corpus (code shape, scale $SCALE)..."
    generating="$(mktemp -d "$CACHE/.generating.XXXXXX")"
    python3 "$HERE/corpus.py" code "$generating/corpus" --scale "$SCALE" >/dev/null
    mv "$generating/corpus" "$PRISTINE"
    rmdir "$generating"
fi
if [ ! -f "$PARTITIONS" ]; then
    "$BM" partitions "$PRISTINE" "$PARTITIONS.tmp" >/dev/null
    mv "$PARTITIONS.tmp" "$PARTITIONS"
fi

# Every autobahn this script started, and nothing else. A leg's binaries
# may have any name, so they are tracked by PID rather than matched by
# name — the earlier version of this script matched by name, and a
# variant binary named differently survived every cleanup and contaminated
# the next leg's numbers.
STARTED=()
cleanup() {
    for pid in "${STARTED[@]:-}"; do
        if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; fi
    done
    rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

# Ends the run with a failed leg. The EXIT trap stops what it started.
fail_leg() {
    local leg="$1" why="$2" log="${3:-}"
    echo "FAIL: leg $leg: $why (logs in $RESULTS)" >&2
    if [ -n "$log" ] && [ -f "$log" ]; then
        echo "--- last lines of $log ---" >&2
        tail -n 20 "$log" >&2
    fi
    exit 1
}

leg() {
    local name="$1" binary="$2" port="$3"
    local dest="$WORK/dest-$name" state="$WORK/state-$name"
    scrub "$dest" "$state" "$CORPUS"
    mkdir -p "$dest" "$state"
    rsync -a "$PRISTINE/" "$CORPUS/"
    local expected
    expected=$("$BM" manifest cheap "$CORPUS")

    "$BM" observer "$port" --root "$dest" > "$RESULTS/observer-$name.log" 2>&1 &
    local observer=$!
    STARTED+=("$observer")
    sleep 1
    kill -0 "$observer" 2>/dev/null \
        || fail_leg "$name" "the observer exited at start" "$RESULTS/observer-$name.log"

    if [ -n "$REMOTE" ]; then
        printf '[groups.g]\nalpha = "%s"\nmode = "two-way-conflict"\ninterval = 5\nbetas = ["%s:%s"]\nagent_command = "ssh %s %s agent"\n' \
            "$CORPUS" "$REMOTE" "$dest" "$REMOTE" "$binary" > "$WORK/$name.toml"
    else
        printf '[groups.g]\nalpha = "%s"\nmode = "two-way-conflict"\ninterval = 5\nbetas = ["%s"]\n' \
            "$CORPUS" "$dest" > "$WORK/$name.toml"
    fi
    local t0 t1
    t0=$(python3 -c 'import time; print(time.time())')
    "$binary" watch --config "$WORK/$name.toml" --state-root "$state" > "$RESULTS/tool-$name.log" 2>&1 &
    local tool=$!
    STARTED+=("$tool")
    # Cold sync: until the destination's manifest matches the source's.
    # A subject that exits has failed the leg now, not in ten minutes, and
    # a cold sync that never converges fails it too: the latency window
    # after it would measure a destination that was never in sync.
    local converged=""
    for _ in $(seq 1 1200); do
        if [ "$("$BM" manifest cheap "$dest" 2>/dev/null)" = "$expected" ]; then
            converged=1
            break
        fi
        kill -0 "$tool" 2>/dev/null \
            || fail_leg "$name" "$binary exited during the cold sync" "$RESULTS/tool-$name.log"
        sleep 0.5
    done
    [ -n "$converged" ] \
        || fail_leg "$name" "the cold sync did not converge within 600s" "$RESULTS/tool-$name.log"
    t1=$(python3 -c 'import time; print(time.time())')
    local cold
    cold=$(python3 -c "print(f'{$t1 - $t0:.1f}')")

    local report
    report=$("$BM" agents --root "$CORPUS" --peer-root "$dest" \
        --observer "127.0.0.1:$port" --partitions "$PARTITIONS" \
        --side a --agents "$AGENTS" --seconds "$SECONDS_PER_LEG" \
        --label "$name" --nonce $((RANDOM * 7919 + $$)) 2> "$RESULTS/agents-$name.err") \
        || fail_leg "$name" "the agents failed" "$RESULTS/agents-$name.err"
    kill -0 "$tool" 2>/dev/null \
        || fail_leg "$name" "$binary exited during the latency window" "$RESULTS/tool-$name.log"

    for pid in "${STARTED[@]}"; do kill "$pid" 2>/dev/null || true; done
    wait 2>/dev/null || true
    STARTED=()
    echo "$report" > "$RESULTS/report-$name.json"
    python3 - "$name" "$cold" "$RESULTS/report-$name.json" <<'PY'
import json, sys
name, cold, path = sys.argv[1:4]
d = json.load(open(path))
print(f"  {name:10s} cold={cold:>6s}s  n={d.get('samples'):>4}  censored={d.get('censored', 0)}  "
      f"p50={d.get('p50_ms'):>6}  p90={d.get('p90_ms'):>6}  p99={d.get('p99_ms'):>7}")
PY
}

echo "A = $A"
echo "B = $B"
echo "corpus: $PRISTINE  agents: $AGENTS  window: ${SECONDS_PER_LEG}s  legs: $LEGS each, interleaved${REMOTE:+  destination: over ssh to $REMOTE}"
echo
port=7400
for i in $(seq 1 "$LEGS"); do
    leg "$LABEL_A$i" "$A" $((port++))
    leg "$LABEL_B$i" "$B" $((port++))
done

echo
python3 - "$RESULTS" "$LABEL_A" "$LABEL_B" "$LEGS" <<'PY'
import json, statistics, sys
work, a, b, legs = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
def series(label):
    out = []
    for i in range(1, legs + 1):
        d = json.load(open(f"{work}/report-{label}{i}.json"))
        out.append(d["p50_ms"])
    return out
sa, sb = series(a), series(b)
print(f"p50 {a}: {sa}  median {statistics.median(sa):.1f}")
print(f"p50 {b}: {sb}  median {statistics.median(sb):.1f}")
delta = statistics.median(sb) - statistics.median(sa)
if legs < 2:
    # One leg each measures nothing about the machine's own variance, so
    # no difference can be called a difference.
    print(f"B relative to A: {delta:+.1f} ms of p50 — no verdict from one leg each; "
          f"run with --legs 2 or more")
else:
    spread = max(max(sa) - min(sa), max(sb) - min(sb))
    verdict = ("within the run-to-run spread" if abs(delta) <= spread
               else ("SLOWER" if delta > 0 else "faster"))
    print(f"B relative to A: {delta:+.1f} ms of p50 — {verdict} (spread {spread:.1f} ms)")
print(f"reports: {work}/report-*.json")
PY
