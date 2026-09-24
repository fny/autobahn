#!/bin/bash
# Long-running soak: continuous churn against a large tree, sampling the
# things a 150-second benchmark cannot see — resident memory, file
# descriptors, watch counts, and the size of the content-addressed staging
# directory, which nothing prunes during a session.
#
# Usage: soak.sh <hours> [corpus]
set -euo pipefail
HOURS=${1:-6}
CORPUS=${2:-sub50k}
AB=${AB:-$HOME/autobahn}
BM=${BM:-$HOME/bench/benchmark}
OUT=~/soak
STATE="$OUT/state"

# Fail at once on a binary that is missing or does not run, before any
# host is touched.
"$AB" --version > /dev/null 2>&1 || { echo "autobahn at $AB does not run" >&2; exit 1; }
[ -x "$BM" ] || { echo "no benchmark harness at $BM" >&2; exit 1; }

mkdir -p "$OUT"; rm -f "$OUT"/*.log "$OUT"/*.jsonl
rm -rf "$STATE"
ssh -n dest "rm -rf ~/dest/$CORPUS && mkdir -p ~/dest/$CORPUS"
# The observer lives on the destination, watching the tree it receives.
# Two calls, and matched by process name rather than command line: a
# -f pattern for "benchmark observer" also matches the very command that
# starts one, so a single line would kill its own shell before the
# observer existed. Nothing else on the destination runs as `benchmark`.
ssh -n dest "pkill -x benchmark 2>/dev/null; true" || true
ssh -n dest "setsid nohup ~/bench/benchmark observer 9911 --listen 0.0.0.0 --root ~/dest/$CORPUS > ~/observer.log 2>&1 < /dev/null &"
sleep 2
ssh -n dest "pgrep -x benchmark > /dev/null" \
  || { echo "the observer did not start on the destination"; exit 1; }
sleep 2
cat > "$OUT/ab.toml" <<TOML
[groups.soak]
alpha = "$HOME/corpus/$CORPUS"
mode = "two-way-conflict"
interval = 5
betas = ["dest:$HOME/dest/$CORPUS"]
TOML
setsid nohup "$AB" watch --config "$OUT/ab.toml" --state-root "$STATE" \
  > "$OUT/autobahn.log" 2>&1 < /dev/null &
AUTOBAHN=$!
sleep 1
kill -0 "$AUTOBAHN" 2>/dev/null \
  || { echo "autobahn did not start; the end of its log:" >&2; tail -n 20 "$OUT/autobahn.log" >&2; exit 1; }
sleep 59   # let the first synchronization finish

# One sample per 30s: RSS, fds, inotify watches, staging entries, staging bytes.
(
  # A sampler that stops at its first failed probe samples nothing.
  set +e +o pipefail
  echo "epoch rss_kb fds watches staging_files staging_kb dest_files"
  while true; do
    pid=$(pgrep -x autobahn | head -1)
    [ -z "$pid" ] && { echo "$(date +%s) AUTOBAHN_GONE"; sleep 30; continue; }
    rss=$(awk '/VmRSS/{print $2}' "/proc/$pid/status" 2>/dev/null)
    fds=$(find "/proc/$pid/fd" -mindepth 1 -maxdepth 1 2>/dev/null | wc -l)
    # Watch descriptors, not inotify instances. The instance count is
    # always one and says nothing; the watch count is one per directory
    # and is the number that would reveal a leak.
    watches=$(cat "/proc/$pid/fdinfo/"* 2>/dev/null | grep -c '^inotify wd:')
    stag=$(find "$STATE" -type d -name 'staging*' 2>/dev/null | head -1)
    sfiles=$( [ -n "$stag" ] && find "$stag" -type f 2>/dev/null | wc -l || echo 0)
    skb=$( [ -n "$stag" ] && du -sk "$stag" 2>/dev/null | cut -f1 || echo 0)
    dfiles=$(ssh -n -o ConnectTimeout=5 dest "find ~/dest/$CORPUS -type f 2>/dev/null | wc -l" 2>/dev/null || echo 0)
    echo "$(date +%s) ${rss:-0} $fds $watches $sfiles $skb $dfiles"
    sleep 30
  done
) > "$OUT/samples.log" 2>&1 &
SAMPLER=$!

# Churn in repeated bursts for the requested duration.
END=$(( $(date +%s) + HOURS*3600 ))
ROUND=0
while [ "$(date +%s)" -lt "$END" ]; do
  ROUND=$((ROUND+1))
  kill -0 "$AUTOBAHN" 2>/dev/null || {
    echo "autobahn exited before round $ROUND" | tee -a "$OUT/progress.log" >&2
    tail -n 20 "$OUT/autobahn.log" >&2
    kill "$SAMPLER" 2>/dev/null || true
    exit 1
  }
  # The observer must run on the DESTINATION host and watch the
  # destination's own copy. Pointing it at 127.0.0.1 measures a path that
  # does not exist on this host, so every edit is correctly censored and
  # the run yields resource data but no latency. Learned the hard way.
  "$BM" agents --root ~/corpus/"$CORPUS" --peer-root ~/dest/"$CORPUS" \
    --observer "$(head -1 ~/bench/peer-ip):9911" \
    --partitions ~/corpus/"$CORPUS".bench/partitions.json \
    --side a --agents 10 --seconds 600 --label "soak-$ROUND" --nonce $((RANDOM*ROUND+7)) \
    >> "$OUT/agents.jsonl" 2>> "$OUT/agents.err" \
    || echo "round $ROUND: the agents failed (see agents.err)" >> "$OUT/progress.log"
  echo "round $ROUND done at $(date -u +%H:%M:%S)" >> "$OUT/progress.log"
done
kill "$SAMPLER" 2>/dev/null || true
kill "$AUTOBAHN" 2>/dev/null || true
echo "SOAK COMPLETE after $ROUND rounds" >> "$OUT/progress.log"
