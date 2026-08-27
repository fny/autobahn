#!/bin/bash
# Long-running soak: continuous churn against a large tree, sampling the
# things a 150-second benchmark cannot see — resident memory, file
# descriptors, watch counts, and the size of the content-addressed staging
# directory, which nothing prunes during a session.
#
# Usage: soak.sh <hours> [corpus]
set -u
HOURS=${1:-6}
CORPUS=${2:-sub40k-a}
BM=~/bench/benchmark
OUT=~/soak
mkdir -p "$OUT"; rm -f "$OUT"/*.log "$OUT"/*.jsonl

rm -rf ~/.autobahn ~/.autobahn-dev
ssh -n dest "rm -rf ~/dest/$CORPUS && mkdir -p ~/dest/$CORPUS"
# The observer lives on the destination, watching the tree it receives.
ssh -n dest "pkill -f 'benchmark [o]bserver' 2>/dev/null; setsid nohup ~/bench/benchmark observer 9911 > ~/observer.log 2>&1 < /dev/null &"
sleep 2
cat > "$OUT/ab.toml" <<TOML
[groups.soak]
alpha = "$HOME/corpus/$CORPUS"
mode = "two-way-safe"
interval = 5
betas = ["dest:$HOME/dest/$CORPUS"]
TOML
setsid nohup ~/autobahn up --config "$OUT/ab.toml" > "$OUT/autobahn.log" 2>&1 < /dev/null &
sleep 60   # let the first synchronization finish

# One sample per 30s: RSS, fds, inotify watches, staging entries, staging bytes.
(
  echo "epoch rss_kb fds watches staging_files staging_kb dest_files"
  while true; do
    pid=$(pgrep -x autobahn | head -1)
    [ -z "$pid" ] && { echo "$(date +%s) AUTOBAHN_GONE"; sleep 30; continue; }
    rss=$(awk '/VmRSS/{print $2}' /proc/$pid/status 2>/dev/null)
    fds=$(ls /proc/$pid/fd 2>/dev/null | wc -l)
    watches=$(find /proc/$pid/fd -lname anon_inode:inotify 2>/dev/null | wc -l)
    stag=$(find ~/.autobahn -type d -name 'staging*' 2>/dev/null | head -1)
    sfiles=$( [ -n "$stag" ] && find "$stag" -type f 2>/dev/null | wc -l || echo 0)
    skb=$( [ -n "$stag" ] && du -sk "$stag" 2>/dev/null | cut -f1 || echo 0)
    dfiles=$(find ~/dest/$CORPUS -type f 2>/dev/null | wc -l)
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
  # The observer must run on the DESTINATION host and watch the
  # destination's own copy. Pointing it at 127.0.0.1 measures a path that
  # does not exist on this host, so every edit is correctly censored and
  # the run yields resource data but no latency. Learned the hard way.
  "$BM" agents --root ~/corpus/$CORPUS --peer-root ~/dest/$CORPUS \
    --observer "$(cat ~/bench/peer-ip | head -1):9911" \
    --partitions ~/corpus/$CORPUS.bench/partitions.json \
    --side a --agents 10 --seconds 600 --label "soak-$ROUND" --nonce $((RANDOM*ROUND+7)) \
    >> "$OUT/agents.jsonl" 2>> "$OUT/agents.err"
  echo "round $ROUND done at $(date -u +%H:%M:%S)" >> "$OUT/progress.log"
done
kill $SAMPLER 2>/dev/null
pkill -x autobahn 2>/dev/null
echo "SOAK COMPLETE after $ROUND rounds" >> "$OUT/progress.log"
