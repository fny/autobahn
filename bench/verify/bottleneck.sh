#!/bin/bash
# Why does a cold sync move only ~10 MB/s when the link does gigabits?
#
# Four measurements against the same corpus, each isolating one candidate.
set -u
C=sub50k
SRC=$HOME/corpus/$C
BM=$HOME/bench/benchmark
AB=$HOME/autobahn
BYTES=$(du -sb "$SRC" | cut -f1)
FILES=$(find "$SRC" -type f | wc -l)
MB=$(awk -v b="$BYTES" 'BEGIN{printf "%.0f", b/1048576}')
echo "corpus: $FILES files, ${MB} MB"
echo

rate() { awk -v m="$MB" -v s="$1" 'BEGIN{printf "%.1f MB/s", m/s}'; }

# 1. Raw ssh stream: the ceiling for anything that goes over this transport.
ssh -n dest 'rm -rf ~/dest/probe; mkdir -p ~/dest/probe' 2>/dev/null
t0=$(date +%s.%N)
dd if=/dev/zero bs=1M count=$MB 2>/dev/null | ssh dest 'cat > /dev/null'
t1=$(date +%s.%N)
s=$(awk -v a=$t0 -v b=$t1 'BEGIN{printf "%.1f", b-a}')
echo "1. raw ssh stream (${MB}MB of zeros):        ${s}s  $(rate $s)"

# 2. tar over ssh: a well-tuned tool moving this exact tree, files and all.
ssh -n dest 'rm -rf ~/dest/tartest; mkdir -p ~/dest/tartest' 2>/dev/null
ssh -n dest 'sync; echo 3 | sudo tee /proc/sys/vm/drop_caches' >/dev/null 2>&1
t0=$(date +%s.%N)
tar cf - -C "$HOME/corpus" "$C" | ssh dest 'tar xf - -C ~/dest/tartest'
t1=$(date +%s.%N)
s=$(awk -v a=$t0 -v b=$t1 'BEGIN{printf "%.1f", b-a}')
echo "2. tar over ssh (same tree):                ${s}s  $(rate $s)"

# 3. Local untar on the destination: no network at all, pure file creation.
ssh -n dest "rm -rf ~/dest/localtest; mkdir -p ~/dest/localtest; sync; \
  echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null 2>&1; \
  s=\$(date +%s.%N); tar cf - -C ~/corpus $C | tar xf - -C ~/dest/localtest; \
  e=\$(date +%s.%N); awk -v a=\$s -v b=\$e 'BEGIN{printf \"%.1f\", b-a}'" > /tmp/local.t 2>/dev/null
s=$(cat /tmp/local.t)
echo "3. local untar on destination (no network): ${s}s  $(rate $s)"

# 4. autobahn.
ssh -n dest "pkill -x 'autobahn(-linux-x86_64)?' 2>/dev/null; sleep 0.5; \
  rm -rf ~/dest/$C ~/.autobahn ~/.autobahn-dev; mkdir -p ~/dest/$C; sync" >/dev/null 2>&1
pkill -x autobahn 2>/dev/null
rm -rf ~/.autobahn ~/.autobahn-dev ~/state; mkdir -p ~/state
sync; echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null 2>&1
ssh -n dest 'sync; echo 3 | sudo tee /proc/sys/vm/drop_caches' >/dev/null 2>&1
expected=$("$BM" manifest cheap "$SRC")
printf '[groups.g]\nalpha = "%s"\nmode = "two-way-safe"\ninterval = 5\nbetas = ["dest:%s/dest/%s"]\n' \
  "$SRC" "$HOME" "$C" > ~/bn.toml
t0=$(date +%s.%N)
setsid "$AB" up --config ~/bn.toml --state-root ~/state > ~/bn.log 2>&1 &
for _ in $(seq 1 300); do
  [ "$(ssh -n dest "$BM manifest cheap ~/dest/$C" 2>/dev/null)" = "$expected" ] && break
  sleep 1
done
t1=$(date +%s.%N)
pkill -x autobahn 2>/dev/null
s=$(awk -v a=$t0 -v b=$t1 'BEGIN{printf "%.1f", b-a}')
echo "4. autobahn cold sync:                      ${s}s  $(rate $s)"
echo
echo "1 is the transport ceiling. 3 is the destination's filesystem ceiling."
echo "2 is what a mature tool achieves against both. 4 is us."
