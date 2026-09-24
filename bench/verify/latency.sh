#!/bin/bash
# What sets the floor on edit-to-propagate latency?
#
# Run the same measurement against two corpus sizes. If latency is the same
# for both, the floor is a constant - round trip plus the settle window. If
# it scales with entry count, the floor is the scan and reconcile over the
# tree, which is a different problem with a different fix.
set -euo pipefail
C=${1:-sub5k}
EDITS=${2:-20}
AB=${AB:-$HOME/autobahn}
BM=${BM:-$HOME/bench/benchmark}
SRC=$HOME/corpus/$C

# Fail at once on a binary that is missing or does not run, before any
# host is touched.
"$AB" --version > /dev/null 2>&1 || { echo "autobahn at $AB does not run" >&2; exit 1; }
[ -x "$BM" ] || { echo "no benchmark harness at $BM" >&2; exit 1; }

ssh -n dest "pkill -x 'autobahn(-linux-x86_64)?' 2>/dev/null; sleep 0.5; \
  rm -rf ~/dest/$C ~/.autobahn ~/.autobahn-dev; mkdir -p ~/dest/$C" >/dev/null 2>&1
pkill -x autobahn 2>/dev/null || true
rm -rf ~/.autobahn ~/.autobahn-dev ~/state; mkdir -p ~/state
# Probe files from any earlier run would be seen instantly by the poller,
# with timestamps minutes old, and would dominate every percentile.
rm -rf "$HOME"/corpus/*/probe
ssh -n dest 'rm -rf ~/dest/*/probe ~/arrivals.txt' >/dev/null 2>&1
mkdir -p "$SRC/probe"

printf '[groups.g]\nalpha = "%s"\nmode = "two-way-conflict"\ninterval = 5\nbetas = ["dest:%s/dest/%s"]\n' \
  "$SRC" "$HOME" "$C" > ~/lat.toml
setsid "$AB" watch --config ~/lat.toml --state-root ~/state > ~/lat.log 2>&1 &
AUTOBAHN=$!
sleep 1
kill -0 "$AUTOBAHN" 2>/dev/null \
  || { echo "autobahn did not start; the end of its log:" >&2; tail -n 20 ~/lat.log >&2; exit 1; }

expected=$("$BM" manifest cheap "$SRC")
converged=""
for _ in $(seq 1 300); do
  if [ "$(ssh -n dest "$BM manifest cheap ~/dest/$C" 2>/dev/null)" = "$expected" ]; then
    converged=1; break
  fi
  kill -0 "$AUTOBAHN" 2>/dev/null \
    || { echo "autobahn exited; the end of its log:" >&2; tail -n 20 ~/lat.log >&2; exit 1; }
  sleep 1
done
[ -n "$converged" ] || { echo "corpus $C did not converge in 300s" >&2; exit 1; }
echo "corpus $C converged ($(find "$SRC" -type f | wc -l) files); measuring $EDITS edits"

# The destination side polls locally and stamps arrival with its own clock;
# the file carries the source's send time in its contents. EC2 instances
# share the Amazon time source, so the skew between the two clocks is well
# under a millisecond - small next to the tens of milliseconds in question.
ssh dest "cat > ~/poll.sh" <<'POLL'
#!/bin/bash
seen=""
end=$(( $(date +%s) + 200 ))
while [ "$(date +%s)" -lt "$end" ]; do
  for f in ~/dest/$1/probe/e*; do
    [ -e "$f" ] || continue
    case " $seen " in *" $f "*) continue;; esac
    sent=$(cat "$f" 2>/dev/null)
    [ -z "$sent" ] && continue
    now=$(date +%s.%N)
    awk -v a="$sent" -v b="$now" 'BEGIN{printf "%.1f\n", (b-a)*1000}'
    seen="$seen $f"
  done
done
POLL
ssh -n dest 'chmod +x ~/poll.sh' 2>/dev/null
# shellcheck disable=SC2088  # the tilde is the destination's, expanded there
ssh -n dest "~/poll.sh $C" > ~/arrivals.txt 2>/dev/null &
poller=$!
sleep 2

for i in $(seq 1 "$EDITS"); do
  date +%s.%N > "$SRC/probe/e$i"
  sleep 4
done
sleep 5
kill "$poller" 2>/dev/null || true
kill "$AUTOBAHN" 2>/dev/null || true

sort -n ~/arrivals.txt > ~/sorted.txt
n=$(wc -l < ~/sorted.txt)
if [ "$n" -lt 3 ]; then echo "only $n samples; measurement failed"; exit 1; fi
p50=$(awk -v n="$n" 'NR==int(n*0.5)+0||NR==int(n*0.5)+1{print;exit}' ~/sorted.txt)
p95=$(awk -v n="$n" 'NR==int(n*0.95)||NR==int(n*0.95)+1{print;exit}' ~/sorted.txt)
min=$(head -1 ~/sorted.txt); max=$(tail -1 ~/sorted.txt)
echo "$C: n=$n  min ${min}ms  p50 ${p50}ms  p95 ${p95}ms  max ${max}ms"
