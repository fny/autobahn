#!/bin/bash
# Crash consistency.
#
# The most dangerous property in autobahn is that a lost ancestor write turns
# a deliberate revert into silent data loss (src/session/mod.rs:419). That
# justification is prose; nothing kills the process and checks it.
#
# Each trial: write known content, start a session, kill it with SIGKILL at a
# random point during a cycle, restart, let it converge, then assert the two
# invariants that matter.
#
#   1. Convergence — the trees agree once the dust settles.
#   2. No resurrection — content deleted before the crash stays deleted, and
#      content written before the crash is not replaced by an older version.
#
# Usage: crash.sh [trials] [--remote]
set -u
TRIALS=${1:-40}
AB=${AB:-/home/ubuntu/Workspace/autobahn/target/release/autobahn}
BM=${BM:-/home/ubuntu/Workspace/autobahn/bench/harness/target/release/benchmark}
PASS=0; FAIL=0; LOST=0
mkdir -p /tmp/crashtest && rm -rf /tmp/crashtest/*

for trial in $(seq 1 "$TRIALS"); do
  W=/tmp/crashtest/t$trial
  mkdir -p "$W/src" "$W/dst" "$W/state"
  # A small tree with a file we will revert, plus filler to make cycles real.
  for i in $(seq 1 200); do printf 'v1-%s' "$i" > "$W/src/f$i.txt"; done
  printf 'ORIGINAL' > "$W/src/target.txt"
  printf 'DOOMED'   > "$W/src/doomed.txt"

  cat > "$W/ab.toml" <<TOML
[groups.crash]
alpha = "$W/src"
mode = "two-way-safe"
interval = 2
betas = ["$W/dst"]
TOML

  # Converge once so both sides and the ancestor agree.
  timeout 60 "$AB" up --config "$W/ab.toml" --state-root "$W/state" --once > "$W/first.log" 2>&1

  before_target=$(cat "$W/dst/target.txt" 2>/dev/null || echo MISSING)
  [ "$before_target" != "ORIGINAL" ] && { echo "trial $trial: setup did not converge"; FAIL=$((FAIL+1)); continue; }

  # Now make two changes that a stale ancestor would mishandle: a revert to
  # earlier content, and a deletion. Then start a watching session and kill
  # it mid-flight.
  printf 'REVERTED' > "$W/src/target.txt"
  rm -f "$W/src/doomed.txt"

  setsid "$AB" up --config "$W/ab.toml" --state-root "$W/state" > "$W/watch.log" 2>&1 &
  UP=$!
  # Kill at a random point inside the window where a cycle is likely running.
  sleep "0.$(( RANDOM % 9 + 1 ))"
  kill -9 $UP 2>/dev/null
  pkill -9 -x autobahn 2>/dev/null
  sleep 0.3

  # Restart and let it settle.
  timeout 90 "$AB" up --config "$W/ab.toml" --state-root "$W/state" --once > "$W/second.log" 2>&1
  timeout 90 "$AB" up --config "$W/ab.toml" --state-root "$W/state" --once >> "$W/second.log" 2>&1

  src_m=$("$BM" manifest full "$W/src" 2>/dev/null)
  dst_m=$("$BM" manifest full "$W/dst" 2>/dev/null)
  after_target=$(cat "$W/dst/target.txt" 2>/dev/null || echo MISSING)
  doomed_back=$([ -e "$W/dst/doomed.txt" ] && echo YES || echo NO)
  src_target=$(cat "$W/src/target.txt" 2>/dev/null || echo MISSING)

  ok=1; why=""
  [ "$src_m" != "$dst_m" ] && { ok=0; why="$why diverged;"; }
  # The revert must survive on BOTH sides. A stale ancestor overwrites it
  # with the peer's older content, which is the silent loss we are hunting.
  [ "$src_target" != "REVERTED" ] && { ok=0; why="$why revert-lost-on-source($src_target);"; LOST=$((LOST+1)); }
  [ "$after_target" != "REVERTED" ] && { ok=0; why="$why revert-not-propagated($after_target);"; }
  [ "$doomed_back" = "YES" ] && { ok=0; why="$why deletion-resurrected;"; }

  if [ $ok = 1 ]; then PASS=$((PASS+1)); rm -rf "$W"; else
    FAIL=$((FAIL+1)); echo "trial $trial FAILED:$why (kept at $W)"
  fi
done
echo "crash trials: $PASS passed, $FAIL failed, $LOST with lost reverts"
[ $FAIL = 0 ]
