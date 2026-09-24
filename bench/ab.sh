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
# With --remote, the destination is reached over SSH to HOST (which may be
# this machine: `localhost`, or an alias with latency shaped onto it) and
# the leg's own binary serves as the agent there, by path, so a variant
# under test is what runs on both sides and nothing is installed under
# ~/.autobahn/bin — where a real controller's agent may already live.
#
# The corpus is generated on first use (bench/corpus.py, the `code` shape,
# 40,000 files at scale 1) and reused. Needs python3, rsync, and a Rust
# toolchain for the harness. Runs on macOS and Linux; a leg is roughly two
# minutes at the defaults.
#
set -u

A=""; B=""; LEGS=2; SECONDS_PER_LEG=60; AGENTS=10; SCALE=1; REMOTE=""
LABEL_A="a"; LABEL_B="b"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORK="${AB_WORK:-${TMPDIR:-/tmp}/autobahn-ab}"
CORPUS=""

usage() { sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

while [ $# -gt 0 ]; do
    case "$1" in
        --legs) LEGS="$2"; shift 2 ;;
        --seconds) SECONDS_PER_LEG="$2"; shift 2 ;;
        --agents) AGENTS="$2"; shift 2 ;;
        --corpus) CORPUS="$2"; shift 2 ;;
        --scale) SCALE="$2"; shift 2 ;;
        --label-a) LABEL_A="$2"; shift 2 ;;
        --label-b) LABEL_B="$2"; shift 2 ;;
        --remote) REMOTE="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        -*) echo "unknown option $1" >&2; usage >&2; exit 2 ;;
        *) if [ -z "$A" ]; then A="$1"; elif [ -z "$B" ]; then B="$1"; else echo "too many arguments" >&2; exit 2; fi; shift ;;
    esac
done
[ -n "$A" ] && [ -n "$B" ] || { usage >&2; exit 2; }
[ -x "$A" ] || { echo "not executable: $A" >&2; exit 2; }
[ -x "$B" ] || { echo "not executable: $B" >&2; exit 2; }
A="$(cd "$(dirname "$A")" && pwd)/$(basename "$A")"
B="$(cd "$(dirname "$B")" && pwd)/$(basename "$B")"

mkdir -p "$WORK"
CORPUS="${CORPUS:-$WORK/corpus-code-$SCALE}"
PRISTINE="$WORK/pristine-code-$SCALE"

# The harness: the measurement plane, compiled so its own overhead is
# small and measured rather than large and guessed.
# Existence is not the test — a build tree synchronized from another
# platform leaves a binary that cannot execute here — so the harness is
# rebuilt whenever the one present does not run.
BM="$HERE/harness/target/release/benchmark"
if ! "$BM" manifest cheap "$HERE" >/dev/null 2>&1; then
    echo "building the harness..."
    (cd "$HERE/harness" && cargo build --release >/dev/null 2>&1) || { echo "harness build failed" >&2; exit 1; }
fi

# The corpus, generated once and kept pristine; every leg restores it, so
# the agents' edits from one leg never leak into the next.
if [ ! -d "$PRISTINE" ]; then
    echo "generating the corpus (code shape, scale $SCALE)..."
    python3 "$HERE/corpus.py" code "$PRISTINE" --scale "$SCALE" >/dev/null || exit 1
fi
if [ ! -f "$WORK/partitions-$SCALE.json" ]; then
    "$BM" partitions "$PRISTINE" "$WORK/partitions-$SCALE.json" >/dev/null || exit 1
fi

# Every autobahn this script started, and nothing else. A leg's binaries
# may have any name, so they are tracked by PID rather than matched by
# name — the earlier version of this script matched by name, and a
# variant binary named differently survived every cleanup and contaminated
# the next leg's numbers.
STARTED=()
cleanup() {
    for pid in "${STARTED[@]:-}"; do
        [ -n "$pid" ] && kill "$pid" 2>/dev/null
    done
}
trap cleanup EXIT INT TERM

leg() {
    local name="$1" binary="$2" port="$3"
    local dest="$WORK/dest-$name" state="$WORK/state-$name"
    rm -rf "$dest" "$state" "$CORPUS"
    mkdir -p "$dest" "$state"
    rsync -a "$PRISTINE/" "$CORPUS/"
    local expected
    expected=$("$BM" manifest cheap "$CORPUS")

    "$BM" observer "$port" --root "$dest" > "$WORK/observer-$name.log" 2>&1 &
    STARTED+=($!)
    sleep 1

    if [ -n "$REMOTE" ]; then
        printf '[groups.g]\nalpha = "%s"\nmode = "two-way-conflict"\ninterval = 5\nbetas = ["%s:%s"]\nagent_command = "ssh %s %s agent"\n' \
            "$CORPUS" "$REMOTE" "$dest" "$REMOTE" "$binary" > "$WORK/$name.toml"
    else
        printf '[groups.g]\nalpha = "%s"\nmode = "two-way-conflict"\ninterval = 5\nbetas = ["%s"]\n' \
            "$CORPUS" "$dest" > "$WORK/$name.toml"
    fi
    local t0 t1
    t0=$(python3 -c 'import time; print(time.time())')
    "$binary" watch --config "$WORK/$name.toml" --state-root "$state" > "$WORK/tool-$name.log" 2>&1 &
    local tool=$!
    STARTED+=("$tool")
    # Cold sync: until the destination's manifest matches the source's.
    for _ in $(seq 1 1200); do
        [ "$("$BM" manifest cheap "$dest" 2>/dev/null)" = "$expected" ] && break
        sleep 0.5
    done
    t1=$(python3 -c 'import time; print(time.time())')
    local cold
    cold=$(python3 -c "print(f'{$t1 - $t0:.1f}')")

    local report
    report=$("$BM" agents --root "$CORPUS" --peer-root "$dest" \
        --observer "127.0.0.1:$port" --partitions "$WORK/partitions-$SCALE.json" \
        --side a --agents "$AGENTS" --seconds "$SECONDS_PER_LEG" \
        --label "$name" --nonce $((RANDOM * 7919 + $$)) 2> "$WORK/agents-$name.err")

    for pid in "${STARTED[@]}"; do kill "$pid" 2>/dev/null; done
    wait 2>/dev/null
    STARTED=()
    echo "$report" > "$WORK/report-$name.json"
    python3 - "$name" "$cold" "$WORK/report-$name.json" <<'PY'
import json, sys
name, cold, path = sys.argv[1:4]
d = json.load(open(path))
print(f"  {name:10s} cold={cold:>6s}s  n={d.get('samples'):>4}  censored={d.get('censored', 0)}  "
      f"p50={d.get('p50_ms'):>6}  p90={d.get('p90_ms'):>6}  p99={d.get('p99_ms'):>7}")
PY
}

echo "A = $A"
echo "B = $B"
echo "corpus: $CORPUS  agents: $AGENTS  window: ${SECONDS_PER_LEG}s  legs: $LEGS each, interleaved${REMOTE:+  destination: over ssh to $REMOTE}"
echo
port=7400
for i in $(seq 1 "$LEGS"); do
    leg "$LABEL_A$i" "$A" $((port++))
    leg "$LABEL_B$i" "$B" $((port++))
done

echo
python3 - "$WORK" "$LABEL_A" "$LABEL_B" "$LEGS" <<'PY'
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
