#!/bin/bash
# Gate 1: does anyone wait for the ancestor tail?
# Gate 2: do the three reconcile inputs share storage in a real session?
#
# The cycle transitions beta before it persists the ancestor, so the tail
# does not delay an isolated save. It occupies the worker, though, so a
# second save landing inside that window waits for it. Comparing an
# isolated save against the second of two saves 100ms apart measures
# exactly that, and nothing else.
set -u
C=${1:-chromium}
N=${2:-10}
AB=$HOME/autobahn
BM=$HOME/bench/benchmark
SRC=$HOME/corpus/$C

echo "corpus $C: $(find "$SRC" -type f | wc -l) files"

# Start converged: a chromium cold sync would take many minutes and is not
# what either gate is about.
ssh -n dest "pkill -x 'autobahn(-linux-x86_64)?' 2>/dev/null; sleep 1; \
  rm -rf ~/dest/$C ~/.autobahn ~/.autobahn-dev ~/poll.sh; mkdir -p ~/dest; \
  cp -a ~/corpus/$C ~/dest/$C" >/dev/null 2>&1
pkill -x autobahn 2>/dev/null
pkill -f 'poll.sh' 2>/dev/null
rm -rf ~/.autobahn ~/.autobahn-dev ~/state ~/arrivals.txt; mkdir -p ~/state
rm -rf "$SRC/probe"; mkdir -p "$SRC/probe"
ssh -n dest "rm -rf ~/dest/$C/probe" >/dev/null 2>&1

printf '[groups.g]\nalpha = "%s"\nmode = "two-way-safe"\ninterval = 5\nbetas = ["dest:%s/dest/%s"]\n' \
  "$SRC" "$HOME" "$C" > ~/gates.toml
AUTOBAHN_SHARING_PROBE=1 setsid "$AB" up --config ~/gates.toml --state-root ~/state \
  > ~/gates.log 2>&1 &

# A pre-seeded destination matches immediately, so a manifest comparison
# proves nothing about whether the session is running yet. Wait for the
# session itself: one completed reconcile, then a canary edit that actually
# lands on beta. The first cycle over 505k entries scans both sides, walks
# the whole tree, and writes the initial ancestor, which takes minutes.
for _ in $(seq 1 600); do
  grep -q '\[sharing\]' ~/gates.log && break
  pgrep -x autobahn >/dev/null || { echo "autobahn died:"; tail -5 ~/gates.log; exit 1; }
  sleep 2
done
grep -q '\[sharing\]' ~/gates.log || { echo "no cycle in 20 minutes"; tail -5 ~/gates.log; exit 1; }
echo "first cycle done after $(grep -c '\[sharing\]' ~/gates.log) reconcile(s)"

date +%s.%N > "$SRC/probe/canary"
for _ in $(seq 1 300); do
  ssh -n dest "test -e ~/dest/$C/probe/canary" 2>/dev/null && break
  sleep 2
done
ssh -n dest "test -e ~/dest/$C/probe/canary" 2>/dev/null \
  || { echo "canary never arrived; pipeline is not working"; tail -5 ~/gates.log; exit 1; }
echo "canary delivered; measuring"

# The poller stamps arrival with the destination's own clock; the file
# carries the source's send time. Both hosts use the Amazon time source.
ssh dest "cat > ~/poll.sh" <<'POLL'
#!/bin/bash
seen=""
end=$(( $(date +%s) + 400 ))
while [ "$(date +%s)" -lt "$end" ]; do
  for f in ~/dest/$1/probe/*; do
    [ -e "$f" ] || continue
    case " $seen " in *" $f "*) continue;; esac
    sent=$(cat "$f" 2>/dev/null); [ -z "$sent" ] && continue
    now=$(date +%s.%N)
    awk -v n="$(basename "$f")" -v a="$sent" -v b="$now" \
      'BEGIN{printf "%s %.1f\n", n, (b-a)*1000}'
    seen="$seen $f"
  done
done
POLL
ssh -n dest 'chmod +x ~/poll.sh'
ssh -n dest "~/poll.sh $C" > ~/arrivals.txt 2>/dev/null &
poller=$!
sleep 2

# Isolated saves: spaced well beyond the tail, so each cycle starts idle.
for i in $(seq 1 "$N"); do
  date +%s.%N > "$SRC/probe/i$i"
  sleep 5
done
# Paired saves: the second lands 100ms after the first, inside the window
# the first cycle's ancestor tail occupies.
for i in $(seq 1 "$N"); do
  date +%s.%N > "$SRC/probe/a$i"
  sleep 0.1
  date +%s.%N > "$SRC/probe/b$i"
  sleep 7
done
sleep 10
kill $poller 2>/dev/null
ssh -n dest 'pkill -f poll.sh' 2>/dev/null
pkill -x autobahn 2>/dev/null

stat() {
  grep "^$1" ~/arrivals.txt | awk '{print $2}' | sort -n > /tmp/s.$1
  local n; n=$(wc -l < /tmp/s.$1)
  [ "$n" -lt 2 ] && { echo "$1: only $n samples"; return; }
  awk -v n="$n" -v label="$2" '{v[NR]=$1}
    END{printf "%-28s n=%-3d min %7.1f  p50 %7.1f  max %7.1f\n",
        label, n, v[1], v[int(n*0.5)+1], v[n]}' /tmp/s.$1
}
echo
stat i "isolated save"
stat b "second of two (100ms apart)"
echo
echo "=== gate 2: sharing on real reconcile inputs ==="
grep '\[sharing\]' ~/gates.log | tail -5
echo "distinct sharing lines: $(grep -c '\[sharing\]' ~/gates.log)"
