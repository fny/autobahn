#!/bin/bash
# Cold sync against fan-out width, with the destinations on their own machines.
#
# The question: when one source feeds N destinations, does the source pay N
# times, or does it share the work?
#
# Local tests cannot answer this. Every "destination" runs in the same process
# on the same host, so the destinations' own cost lands on the source and the
# curve that comes out is an artifact of the test, not a property of the tool.
# This script needs real destinations, which is why it runs on EC2.
#
# Run on the source. Destinations must be reachable as dest1..destN.
# Usage: coldfan.sh [corpus] [max-width] [repeats]
set -u

CORPUS=${1:-sub50k}
MAX=${2:-2}
REPEATS=${3:-3}
AB=${AB:-$HOME/autobahn}
BM=${BM:-$HOME/bench/benchmark}
TIMEOUT=${TIMEOUT:-900}
POLL=${POLL:-3}

for b in $(seq 1 "$MAX"); do
  ssh -o ConnectTimeout=5 -n "dest$b" true 2>/dev/null || {
    echo "dest$b unreachable" >&2; exit 1; }
done

echo "corpus: $CORPUS ($(find "$HOME/corpus/$CORPUS" -type f | wc -l) files)"
echo "widths: 1..$MAX, $REPEATS repeats each"
echo

# Computed once, outside the timed region: digesting the corpus reads every
# byte of it, so doing this after dropping caches would warm the very cache
# the measurement is trying to start cold.
expected=$("$BM" manifest full "$HOME/corpus/$CORPUS")
expected_cheap=$("$BM" manifest cheap "$HOME/corpus/$CORPUS")

printf '%6s %7s %11s %11s %11s\n' width repeat 'elapsed s' 'src cpu s' 'cpu/dest s'
printf '%6s %7s %11s %11s %11s\n' ------ ------- ----------- ----------- -----------

for width in $(seq 1 "$MAX"); do
  for repeat in $(seq 1 "$REPEATS"); do
    # Reset every destination, not just the ones this width uses, so each run
    # starts from the same state regardless of what ran before it.
    pkill -x autobahn 2>/dev/null
    for b in $(seq 1 "$MAX"); do
      # Match the process NAME exactly, never the full command line: -f scans
      # arguments, and the rm below names ~/.autobahn, so a -f pattern matches
      # this very shell and kills it before the rm runs.
      ssh -n "dest$b" "pkill -x 'autobahn(-linux-x86_64)?' 2>/dev/null; sleep 0.5; \
        rm -rf ~/dest/$CORPUS ~/.autobahn ~/.autobahn-dev; \
        mkdir -p ~/dest/$CORPUS; sync" > /dev/null 2>&1
    done
    rm -rf "$HOME/.autobahn" "$HOME/.autobahn-dev" "$HOME/state"
    mkdir -p "$HOME/state"
    # The reset is the one step whose silent failure would invalidate every
    # number below it: a destination that kept its files reports an instant
    # "cold" sync. Prove it is empty before starting the clock.
    for b in $(seq 1 "$width"); do
      remaining=$(ssh -n "dest$b" "find ~/dest/$CORPUS -type f 2>/dev/null | wc -l")
      if [ "$remaining" != 0 ]; then
        echo "ABORT: dest$b still holds $remaining files after reset" >&2
        exit 1
      fi
    done
    sync; echo 3 | sudo tee /proc/sys/vm/drop_caches > /dev/null 2>&1
    for b in $(seq 1 "$MAX"); do
      ssh -n "dest$b" "sync; echo 3 | sudo tee /proc/sys/vm/drop_caches" > /dev/null 2>&1
    done

    {
      echo "[groups.fan]"
      echo "alpha = \"$HOME/corpus/$CORPUS\""
      echo 'mode = "two-way-safe"'
      echo "interval = 5"
      printf 'betas = ['
      for b in $(seq 1 "$width"); do
        printf '"dest%s:%s/dest/%s"' "$b" "$HOME" "$CORPUS"
        [ "$b" -lt "$width" ] && printf ', '
      done
      printf ']\n'
    } > "$HOME/fan.toml"

    start=$(date +%s.%N)
    setsid "$AB" up --config "$HOME/fan.toml" --state-root "$HOME/state" \
      > "$HOME/fan-$width-$repeat.log" 2>&1 &
    pid=""
    for _ in $(seq 1 30); do
      pid=$(pgrep -x autobahn | head -1); [ -n "$pid" ] && break; sleep 0.2
    done
    [ -z "$pid" ] && { printf '%6s %7s %11s\n' "$width" "$repeat" DIED; continue; }
    j0=$(awk '{print $14+$15}' "/proc/$pid/stat" 2>/dev/null || echo 0)

    # Poll with the cheap manifest (names, sizes, modes) and only once every
    # POLL seconds. Digesting file contents on each destination every second
    # would load the destinations continuously and distort the thing being
    # measured; the cheap walk is the stopping signal, and the content digest
    # runs once afterwards as the arbiter of whether the bytes really arrived.
    # Probe the destinations concurrently, not one after another. Serial
    # probing costs width x round-trip per cycle, so at width 4 the polling
    # alone would keep the destinations busy continuously and would also
    # smear the convergence timestamp across several seconds.
    probes="$HOME/.coldfan-probe"
    converged=no
    for _ in $(seq 1 $((TIMEOUT / POLL))); do
      rm -rf "$probes"; mkdir -p "$probes"
      # Wait on the probe PIDs specifically. A bare `wait` also waits for the
      # autobahn daemon started with & above, which never exits.
      probe_pids=""
      for b in $(seq 1 "$width"); do
        (
          [ "$(ssh -n "dest$b" "$BM manifest cheap ~/dest/$CORPUS" 2>/dev/null)" \
            = "$expected_cheap" ] && : > "$probes/$b"
        ) &
        probe_pids="$probe_pids $!"
      done
      # shellcheck disable=SC2086
      wait $probe_pids
      agree=$(find "$probes" -type f | wc -l)
      [ "$agree" = "$width" ] && { converged=yes; break; }
      kill -0 "$pid" 2>/dev/null || break
      sleep "$POLL"
    done
    end=$(date +%s.%N)
    j1=$(awk '{print $14+$15}' "/proc/$pid/stat" 2>/dev/null || echo "$j0")
    pkill -x autobahn 2>/dev/null

    if [ "$converged" != yes ]; then
      printf '%6s %7s %11s\n' "$width" "$repeat" "NO-CONVERGE"
      continue
    fi
    # Now that the clock has stopped, confirm the contents, not just the shape.
    for b in $(seq 1 "$width"); do
      if [ "$(ssh -n "dest$b" "$BM manifest full ~/dest/$CORPUS" 2>/dev/null)" \
           != "$expected" ]; then
        printf '%6s %7s %11s (dest%s)\n' "$width" "$repeat" "DIGEST-MISMATCH" "$b"
        converged=bad
      fi
    done
    [ "$converged" = bad ] && continue
    elapsed=$(awk -v a="$start" -v b="$end" 'BEGIN{printf "%.1f", b-a}')
    cpu=$(awk -v a="$j0" -v b="$j1" 'BEGIN{printf "%.1f", (b-a)/100}')
    per=$(awk -v c="$cpu" -v w="$width" 'BEGIN{printf "%.1f", c/w}')
    printf '%6s %7s %11s %11s %11s\n' "$width" "$repeat" "$elapsed" "$cpu" "$per"
  done
done

echo
echo "src cpu is the autobahn process only. It excludes the ssh children that"
echo "encrypt each stream, whose cost is genuinely per-destination either way."
echo "If cpu/dest falls as width grows, the source shares work across sessions."
echo "If it stays flat, each destination costs the source a full pass."
