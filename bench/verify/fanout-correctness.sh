#!/bin/bash
# Correctness under sharing: ten sessions over one alpha must each deliver
# every edit to their own destination. A shared observation that dropped a
# change for one session, or served a stale scan, shows up here as a
# destination that does not match the source.
set -euo pipefail
AB=${AB:-/home/ubuntu/Workspace/autobahn/target/release/autobahn}
BM=${BM:-/home/ubuntu/Workspace/autobahn/bench/harness/target/release/benchmark}

# Fail at once on a binary that is missing or does not run.
"$AB" --version > /dev/null 2>&1 || { echo "autobahn at $AB does not run" >&2; exit 1; }
[ -x "$BM" ] || { echo "no benchmark harness at $BM" >&2; exit 1; }

W=$(mktemp -d); UP=""
# The supervisor's own process group (setsid), never every autobahn here.
trap '[ -n "$UP" ] && kill -- "-$UP" 2>/dev/null; rm -rf "$W"' EXIT
mkdir -p "$W/src"
for d in $(seq 1 40); do
  mkdir -p "$W/src/dir$d"
  for f in $(seq 1 25); do printf 'v0-%s-%s' "$d" "$f" > "$W/src/dir$d/f$f.txt"; done
done

{ echo "[groups.fan]"; echo "alpha = \"$W/src\""; echo 'mode = "two-way-conflict"'
  echo "interval = 2"; printf 'betas = ['
  for b in $(seq 1 10); do mkdir -p "$W/dst$b"; printf '"%s/dst%s"' "$W" "$b"
    if [ "$b" -lt 10 ]; then printf ', '; fi; done; printf ']\n'; } > "$W/ab.toml"

setsid "$AB" watch --config "$W/ab.toml" --state-root "$W/state" > "$W/ab.log" 2>&1 &
UP=$!
sleep 1
if ! kill -0 "$UP" 2>/dev/null; then
  UP=""
  echo "autobahn did not start; the end of its log:" >&2
  tail -n 20 "$W/ab.log" >&2
  exit 1
fi
sleep 19

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
  if [ "$agree" != 10 ]; then fail=1; fi
done

echo "--- errors reported ---"
grep -ciE "error|fail" "$W/ab.log" || true
[ "$fail" = 0 ] && echo "VERDICT: every edit reached every destination" \
                || echo "VERDICT: FAILED — a destination diverged"
exit $fail
