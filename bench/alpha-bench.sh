#!/bin/bash
# alpha-bench — what does one source cost when it feeds N destinations?
#
# autobahn fans a config group into one session per beta, and each session
# builds its own endpoint over the same alpha. So ten destinations may mean
# ten scans of one tree, ten copies of it in memory, ten inotify watches per
# directory, and ten scan caches on disk — or the page cache and the
# operating system may absorb most of that. This measures which.
#
# It runs locally and costs nothing. The betas are local directories: a
# remote beta would add an agent per destination, but the duplication being
# measured is on the ALPHA side, and that is identical either way.
#
# Usage: alpha-bench.sh [widths...]      (default: 1 2 5 10)
set -u

AB=${AB:-/home/ubuntu/Workspace/autobahn/target/release/autobahn}
FILES=${FILES:-20000}          # files in the corpus
DIRS=${DIRS:-400}              # directories, since watches are per directory
SETTLE=${SETTLE:-25}           # seconds to let a width converge before sampling
WIDTHS=${*:-1 2 5 10}

WORK=$(mktemp -d)
cleanup() { pkill -x autobahn 2>/dev/null; rm -rf "$WORK"; }
trap cleanup EXIT

echo "building a corpus of $FILES files across $DIRS directories..."
for d in $(seq 1 "$DIRS"); do
  mkdir -p "$WORK/src/dir$d"
done
per_dir=$((FILES / DIRS))
for d in $(seq 1 "$DIRS"); do
  for f in $(seq 1 "$per_dir"); do
    printf 'seed %s %s' "$d" "$f" > "$WORK/src/dir$d/f$f.txt"
  done
done
actual_files=$(find "$WORK/src" -type f | wc -l)
actual_dirs=$(find "$WORK/src" -type d | wc -l)
echo "corpus: $actual_files files, $actual_dirs directories"
echo

printf '%6s %10s %10s %10s %10s %10s %12s\n' \
  betas rss_mb watches fds caches cache_mb cpu_pct
printf '%6s %10s %10s %10s %10s %10s %12s\n' \
  ------ ---------- ---------- ---------- ---------- ---------- ------------

for width in $WIDTHS; do
  pkill -x autobahn 2>/dev/null; sleep 1
  rm -rf "$WORK/state" "$WORK/dst"*; mkdir -p "$WORK/state"

  # One group, `width` betas — exactly the shape a fan-out configuration has.
  {
    echo "[groups.fan]"
    echo "alpha = \"$WORK/src\""
    echo 'mode = "two-way-conflict"'
    echo "interval = 5"
    printf 'betas = ['
    for b in $(seq 1 "$width"); do
      mkdir -p "$WORK/dst$b"
      printf '"%s/dst%s"' "$WORK" "$b"
      [ "$b" -lt "$width" ] && printf ', '
    done
    printf ']\n'
  } > "$WORK/ab.toml"

  setsid "$AB" watch --config "$WORK/ab.toml" --state-root "$WORK/state" \
    > "$WORK/ab-$width.log" 2>&1 &
  sleep "$SETTLE"

  pid=$(pgrep -x autobahn | head -1)
  if [ -z "$pid" ]; then
    printf '%6s %10s\n' "$width" "DIED"
    continue
  fi

  # CPU over a five-second idle window: the tree is converged, so whatever
  # is burned here is the cost of merely watching it.
  j0=$(awk '{print $14+$15}' /proc/"$pid"/stat)
  sleep 5
  j1=$(awk '{print $14+$15}' /proc/"$pid"/stat)
  cpu=$(awk -v a="$j0" -v b="$j1" 'BEGIN{printf "%.1f", (b-a)/100/5*100}')

  rss=$(awk '/VmRSS/{printf "%.1f", $2/1024}' /proc/"$pid"/status)
  # Actual inotify watches, one line per watch descriptor.
  watches=$(cat /proc/"$pid"/fdinfo/* 2>/dev/null | grep -c '^inotify wd:')
  fds=$(find /proc/"$pid"/fd -mindepth 1 -maxdepth 1 2>/dev/null | wc -l)
  caches=$(find "$WORK/state" -name '*.scancache' 2>/dev/null | wc -l)
  cache_kb=$(find "$WORK/state" -name '*.scancache' -printf '%s\n' 2>/dev/null \
             | awk '{s+=$1} END {printf "%.1f", s/1048576}')

  printf '%6s %10s %10s %10s %10s %10s %12s\n' \
    "$width" "$rss" "$watches" "$fds" "$caches" "${cache_kb:-0}" "$cpu"

  pkill -x autobahn 2>/dev/null
done

echo
echo "Reading it: a cost that shares across sessions stays flat as betas"
echo "grow; a cost paid per session rises with them. Watches and scan"
echo "caches are the sharpest signals — they are countable rather than"
echo "sampled, and neither is absorbed by the page cache."
