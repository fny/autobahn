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
# Fan-out matters here because ten sessions over one alpha share a single
# observation and a single scan cache, but keep an ancestor each. A crash
# tears all of that down mid-write at once.
#
# Churn matters because the ancestor is a checkpoint plus a journal: a
# trial that never writes enough to compact never crashes during the
# checkpoint-then-clear sequence, which is the one interleaving where a
# stale journal could be replayed onto a checkpoint that already holds it.
#
# Usage: crash.sh [trials] [betas]
set -u
TRIALS=${1:-40}
BETAS=${2:-1}
AB=${AB:-/home/ubuntu/Workspace/autobahn/target/release/autobahn}
BM=${BM:-/home/ubuntu/Workspace/autobahn/bench/harness/target/release/benchmark}
PASS=0; FAIL=0; LOST=0
mkdir -p /tmp/crashtest && rm -rf /tmp/crashtest/*

for trial in $(seq 1 "$TRIALS"); do
  W=/tmp/crashtest/t$trial
  mkdir -p "$W/src" "$W/state"
  for b in $(seq 1 "$BETAS"); do mkdir -p "$W/dst$b"; done
  # A small tree with a file we will revert, plus filler to make cycles real.
  for i in $(seq 1 200); do printf 'v1-%s' "$i" > "$W/src/f$i.txt"; done
  printf 'ORIGINAL' > "$W/src/target.txt"
  printf 'DOOMED'   > "$W/src/doomed.txt"

  {
    echo "[groups.crash]"
    echo "alpha = \"$W/src\""
    echo 'mode = "two-way-safe"'
    echo "interval = 2"
    printf 'betas = ['
    for b in $(seq 1 "$BETAS"); do
      printf '"%s/dst%s"' "$W" "$b"
      [ "$b" -lt "$BETAS" ] && printf ', '
    done
    printf ']\n'
  } > "$W/ab.toml"

  # Converge once so both sides and the ancestor agree.
  timeout 60 "$AB" up --config "$W/ab.toml" --state-root "$W/state" --once > "$W/first.log" 2>&1

  before_target=$(cat "$W/dst1/target.txt" 2>/dev/null || echo MISSING)
  [ "$before_target" != "ORIGINAL" ] && { echo "trial $trial: setup did not converge"; FAIL=$((FAIL+1)); continue; }

  # Now make two changes that a stale ancestor would mishandle: a revert to
  # earlier content, and a deletion. Then start a watching session and kill
  # it mid-flight.
  printf 'REVERTED' > "$W/src/target.txt"
  rm -f "$W/src/doomed.txt"
  # Enough churn to drive the ancestor past a compaction, so some trials
  # crash during the checkpoint-then-clear sequence rather than only
  # during an append.
  for i in $(seq 1 200); do printf 'v2-%s-%s' "$i" "$RANDOM" > "$W/src/f$i.txt"; done

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
  src_target=$(cat "$W/src/target.txt" 2>/dev/null || echo MISSING)

  ok=1; why=""
  after_target=MISSING; doomed_back=NO
  for b in $(seq 1 "$BETAS"); do
    dst_m=$("$BM" manifest full "$W/dst$b" 2>/dev/null)
    [ "$src_m" != "$dst_m" ] && { ok=0; why="$why diverged(dst$b);"; }
    [ "$(cat "$W/dst$b/target.txt" 2>/dev/null || echo MISSING)" != "REVERTED" ] \
      && after_target=BAD
    [ -e "$W/dst$b/doomed.txt" ] && doomed_back=YES
  done
  [ "$after_target" != BAD ] && after_target=REVERTED
  # The revert must survive on BOTH sides. A stale ancestor overwrites it
  # with the peer's older content, which is the silent loss we are hunting.
  [ "$src_target" != "REVERTED" ] && { ok=0; why="$why revert-lost-on-source($src_target);"; LOST=$((LOST+1)); }
  [ "$after_target" != "REVERTED" ] && { ok=0; why="$why revert-not-propagated($after_target);"; }
  [ "$doomed_back" = "YES" ] && { ok=0; why="$why deletion-resurrected;"; }
  # The ancestor must still load. A journal left torn or stale by the kill
  # that cannot be read at all is the failure this whole file exists for,
  # and a session that refuses to start would otherwise show up only as a
  # divergence with no explanation.
  if grep -qiE "ancestor|journal" "$W/second.log" 2>/dev/null; then
    ok=0; why="$why ancestor-error($(grep -iE 'ancestor|journal' "$W/second.log" | head -1));"
  fi

  if [ $ok = 1 ]; then PASS=$((PASS+1)); rm -rf "$W"; else
    FAIL=$((FAIL+1)); echo "trial $trial FAILED:$why (kept at $W)"
  fi
done
echo "crash trials: $PASS passed, $FAIL failed, $LOST with lost reverts"
[ $FAIL = 0 ]
