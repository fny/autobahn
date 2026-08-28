#!/bin/bash
# Correctness under sharing: ten sessions over one alpha must each deliver
# every edit to their own destination. A shared observation that dropped a
# change for one session, or served a stale scan, shows up here as a
# destination that does not match the source.
set -u
AB=/home/ubuntu/Workspace/autobahn/target/release/autobahn
BM=/home/ubuntu/Workspace/autobahn/bench/harness/target/release/benchmark
W=$(mktemp -d); trap 'pkill -x autobahn 2>/dev/null; rm -rf "$W"' EXIT
mkdir -p "$W/src"
for d in $(seq 1 40); do
  mkdir -p "$W/src/dir$d"
  for f in $(seq 1 25); do printf 'v0-%s-%s' "$d" "$f" > "$W/src/dir$d/f$f.txt"; done
done

{ echo "[groups.fan]"; echo "alpha = \"$W/src\""; echo 'mode = "two-way-safe"'
  echo "interval = 2"; printf 'betas = ['
  for b in $(seq 1 10); do mkdir -p "$W/dst$b"; printf '"%s/dst%s"' "$W" "$b"
    [ "$b" -lt 10 ] && printf ', '; done; printf ']\n'; } > "$W/ab.toml"

setsid "$AB" up --config "$W/ab.toml" --state-root "$W/state" > "$W/ab.log" 2>&1 &
sleep 20

echo "--- 5 rounds of edits, checking all 10 destinations each time ---"
fail=0
for round in 1 2 3 4 5; do
  for d in $(seq 1 40); do
    printf 'round-%s-%s' "$round" "$d" > "$W/src/dir$d/f1.txt"
  done
  # Give it time to converge everywhere.
  for _ in $(seq 1 40); do
    src=$("$BM" manifest full "$W/src")
    agree=0
    for b in $(seq 1 10); do
      [ "$("$BM" manifest full "$W/dst$b" 2>/dev/null)" = "$src" ] && agree=$((agree+1))
    done
    [ "$agree" = 10 ] && break
    sleep 1
  done
  echo "  round $round: $agree of 10 destinations match"
  [ "$agree" != 10 ] && fail=1
done

echo "--- errors reported ---"
grep -ciE "error|fail" "$W/ab.log" || true
[ "$fail" = 0 ] && echo "VERDICT: every edit reached every destination" \
                || echo "VERDICT: FAILED — a destination diverged"
exit $fail
