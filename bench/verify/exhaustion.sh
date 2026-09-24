#!/bin/bash
# Resource exhaustion.
#
# autobahn's watcher gives up and demands a full scan when the kernel drops
# events or the record overflows (src/endpoint/local.rs:184). The contract is
# that watch failures cost latency, never correctness. This induces the
# failure for real rather than asserting it.
set -euo pipefail
AB=${AB:-/home/ubuntu/Workspace/autobahn/target/release/autobahn}
BM=${BM:-/home/ubuntu/Workspace/autobahn/bench/harness/target/release/benchmark}

# Fail at once on a binary that is missing or does not run.
"$AB" --version > /dev/null 2>&1 || { echo "autobahn at $AB does not run" >&2; exit 1; }
[ -x "$BM" ] || { echo "no benchmark harness at $BM" >&2; exit 1; }

W=$(mktemp -d); mkdir -p "$W/src" "$W/dst" "$W/state"
UP=""
# setsid makes the supervisor a process group of its own, so signals go to
# that group and never to another autobahn on the machine.
trap '[ -n "$UP" ] && kill -- "-$UP" 2>/dev/null; rm -rf "$W"' EXIT

echo "system inotify limits: watches=$(cat /proc/sys/fs/inotify/max_user_watches) queue=$(cat /proc/sys/fs/inotify/max_queued_events)"

# A tree with many directories, so a low watch limit is genuinely exceeded.
for d in $(seq 1 400); do
  mkdir -p "$W/src/dir$d"
  for f in $(seq 1 5); do printf 'seed-%s-%s' "$d" "$f" > "$W/src/dir$d/f$f.txt"; done
done
echo "tree: $(find "$W/src" -type d | wc -l) directories, $(find "$W/src" -type f | wc -l) files"

cat > "$W/ab.toml" <<TOML
[groups.exhaust]
alpha = "$W/src"
mode = "two-way-conflict"
interval = 3
betas = ["$W/dst"]
TOML

setsid "$AB" watch --config "$W/ab.toml" --state-root "$W/state" > "$W/ab.log" 2>&1 &
UP=$!
sleep 1
if ! kill -0 "$UP" 2>/dev/null; then
  UP=""
  echo "autobahn did not start; the end of its log:" >&2
  tail -n 20 "$W/ab.log" >&2
  exit 1
fi
sleep 11

echo "--- forced overflow: stop the process so it cannot drain its watcher ---"
# SIGSTOP means events accumulate with nothing consuming them, so the
# record passes MAXIMUM_PENDING_PATHS (8192) and give_up() must fire. On
# resume the next scan has to read the tree rather than trust the record.
kill -STOP -- "-$UP"
for d in $(seq 1 400); do
  for f in $(seq 1 5); do printf 'stopped-%s-%s' "$d" "$f" > "$W/src/dir$d/f$f.txt"; done
done
for d in $(seq 1 400); do
  for f in $(seq 6 12); do printf 'new-%s-%s' "$d" "$f" > "$W/src/dir$d/n$f.txt"; done
done
echo "generated $(find "$W/src" -type f | wc -l) files worth of events while stopped"
kill -CONT -- "-$UP"
sleep 5

echo "--- storm: rewriting every file at once, far past the 8192-path record ---"
for round in 1 2 3; do
  for d in $(seq 1 400); do
    for f in $(seq 1 5); do printf 'round-%s-%s-%s' "$round" "$d" "$f" > "$W/src/dir$d/f$f.txt"; done
  done
done
echo "storm done, waiting for convergence"

converged=no
for _ in $(seq 1 60); do
  s=$("$BM" manifest full "$W/src" 2>/dev/null)
  t=$("$BM" manifest full "$W/dst" 2>/dev/null)
  [ "$s" = "$t" ] && { converged=yes; break; }
  sleep 2
done
kill -- "-$UP" 2>/dev/null || true; UP=""

echo "converged: $converged"
echo "--- errors reported by autobahn ---"
grep -icE "error|fail" "$W/ab.log" || true
grep -iE "error|fail" "$W/ab.log" | head -3 || true
[ "$converged" = yes ]
