#!/bin/bash
# End-to-end test of the measurement plumbing, entirely local: a synthetic
# corpus, the real binary's partitions/observer/agents/floor/manifest/
# sampler, and toysync.py as a subject whose behavior is known. Run after
# any harness change; the assertions below are the harness's contract.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
BINARY="$HERE/harness/target/release/benchmark"
WORK="$(mktemp -d)"
trap 'kill $(jobs -p) 2>/dev/null; rm -rf "$WORK"' EXIT

echo "== build =="
(cd "$HERE/harness" && cargo build --release --quiet)

echo "== synthetic corpus =="
python3 - "$WORK" <<'EOF'
import os, random, sys
work = sys.argv[1]
rng = random.Random(1)
for d in range(30):
    directory = f"{work}/src/dir{d:02d}"
    os.makedirs(directory)
    for f in range(80):
        with open(f"{directory}/file{f}.txt", "wb") as handle:
            handle.write(rng.randbytes(rng.randrange(300, 8000)))
# Content that must be excluded from every summary:
os.makedirs(f"{work}/src/.git"); open(f"{work}/src/.git/junk", "w").write("x")
os.makedirs(f"{work}/src/out"); open(f"{work}/src/out/artifact", "w").write("x")
open(f"{work}/src/dir00/leftover.bench-tmp", "w").write("x")
EOF

echo "== manifest excludes ignored content and temporaries =="
CHEAP=$("$BINARY" manifest cheap "$WORK/src")
COUNT=$(echo "$CHEAP" | cut -d' ' -f1)
[ "$COUNT" = "2400" ] || { echo "FAIL: expected 2400 files, got $COUNT"; exit 1; }

echo "== manifest full is deterministic and content-sensitive =="
FULL1=$("$BINARY" manifest full "$WORK/src")
FULL2=$("$BINARY" manifest full "$WORK/src")
[ "$FULL1" = "$FULL2" ] || { echo "FAIL: full manifest not deterministic"; exit 1; }
printf 'tweak' >> "$WORK/src/dir00/file0.txt"
FULL3=$("$BINARY" manifest full "$WORK/src")
[ "$FULL1" != "$FULL3" ] || { echo "FAIL: full manifest missed a content change"; exit 1; }

echo "== partitions generate, verify, and are reproducible =="
"$BINARY" partitions "$WORK/src" "$WORK/partitions.json" > /dev/null
"$BINARY" verify-partitions "$WORK/partitions.json" > /dev/null
"$BINARY" partitions "$WORK/src" "$WORK/partitions2.json" > /dev/null
cmp -s "$WORK/partitions.json" "$WORK/partitions2.json" \
  || { echo "FAIL: partitions not reproducible"; exit 1; }
python3 - "$WORK/partitions.json" <<'EOF'
import json, sys
p = json.load(open(sys.argv[1]))
a = p["sides"]["a"]
# The measured set must be identical at every agent count.
sets = [tuple(a[str(n)]["measured"]) for n in (1, 10, 100)]
assert len(set(sets)) == 1, "measured set varies with agent count"
assert len(sets[0]) == 40, f"measured set is {len(sets[0])} files"
assert len(a["100"]["background"]) == 99
EOF

echo "== observer + floor =="
mkdir -p "$WORK/dst"
"$BINARY" observer 19911 > "$WORK/observer.log" 2>&1 &
sleep 0.5
FLOOR=$("$BINARY" floor --observer 127.0.0.1:19911 --dest-root "$WORK/dst")
echo "  floor: $FLOOR"
python3 - "$FLOOR" <<'EOF'
import json, sys
floor = json.loads(sys.argv[1])
assert floor["samples"] == 50, floor
assert floor["p50_ms"] < 50, f"floor p50 {floor['p50_ms']}ms is implausibly high locally"
EOF

echo "== agents measure a known subject =="
cp -r "$WORK/src" "$WORK/src-run"
rm -rf "$WORK/dst" && cp -r "$WORK/src" "$WORK/dst"
python3 "$HERE/toysync.py" "$WORK/src-run" "$WORK/dst" &
REPORT=$("$BINARY" agents \
  --root "$WORK/src-run" --peer-root "$WORK/dst" \
  --observer 127.0.0.1:19911 --partitions "$WORK/partitions.json" \
  --side a --agents 10 --seconds 20 --label smoke)
echo "  agents: $(echo "$REPORT" | python3 -c 'import json,sys; r=json.load(sys.stdin); print({k: r[k] for k in ("samples","warmup_samples","censored","p50_ms","p90_ms")})')"
python3 - "$REPORT" <<'EOF'
import json, sys
report = json.loads(sys.argv[1])
assert report["censored"] == 0, report
assert report["samples"] >= 5, report
# toysync copies every 200ms: p50 must sit in the [floor, interval+slack]
# band. Outside it, the harness is measuring something other than the
# subject.
assert 20 <= report["p50_ms"] <= 600, report["p50_ms"]
assert report["warmup_samples"] >= 1, "warmup exclusion did not engage"
EOF

echo "== censoring engages when nothing propagates =="
# No sync tool at all: every announced edit must come back censored, and
# the run must complete rather than hang or crash. A short deadline via the
# observer is simulated by pointing at a directory nothing writes to.
rm -rf "$WORK/void" && mkdir -p "$WORK/void"
CENSORED=$(timeout 400 "$BINARY" agents \
  --root "$WORK/src-run" --peer-root "$WORK/void" \
  --observer 127.0.0.1:19911 --partitions "$WORK/partitions.json" \
  --side b --agents 1 --seconds 8 --label censored)
python3 - "$CENSORED" <<'EOF'
import json, sys
report = json.loads(sys.argv[1])
assert report["samples"] == 0, report
assert report["censored"] >= 1, report
assert report["censored_over_ms"] == 120000, report
EOF

echo "== sampler attributes by pattern and window =="
sleep 120 & SLEEPER=$!
"$BINARY" sampler "sleep 120" "$WORK/rss.log" & SAMPLER=$!
sleep 3; kill $SAMPLER 2>/dev/null; kill $SLEEPER 2>/dev/null
LINES=$(wc -l < "$WORK/rss.log")
[ "$LINES" -ge 2 ] || { echo "FAIL: sampler produced $LINES lines"; exit 1; }
awk '{ if ($4 < 1) { print "FAIL: sampler lost its target"; exit 1 } }' "$WORK/rss.log"

echo "ALL SMOKE CHECKS PASSED"
