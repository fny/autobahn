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
# A handful of files above a megabyte, so patch mode has somewhere to
# land. Compressible content, since a random megabyte is neither typical
# of a source tree nor kind to the delta being measured.
os.makedirs(f"{work}/src/large")
for f in range(6):
    with open(f"{work}/src/large/blob{f}.bin", "wb") as handle:
        handle.write((b"the quick brown fox jumps over the lazy dog\n" * 40000)[:1_500_000])
# Content that must be excluded from every summary:
os.makedirs(f"{work}/src/.git"); open(f"{work}/src/.git/junk", "w").write("x")
os.makedirs(f"{work}/src/out"); open(f"{work}/src/out/artifact", "w").write("x")
open(f"{work}/src/dir00/leftover.bench-tmp", "w").write("x")
EOF

echo "== manifest excludes ignored content and temporaries =="
CHEAP=$("$BINARY" manifest cheap "$WORK/src")
COUNT=$(echo "$CHEAP" | cut -d' ' -f1)
[ "$COUNT" = "2406" ] || { echo "FAIL: expected 2406 files (2400 small + 6 large), got $COUNT"; exit 1; }

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
for side in ("a", "b"):
    s = p["sides"][side]
    # The measured set must be identical at every agent count, on both sides.
    sets = [tuple(s[str(n)]["measured"]) for n in (1, 10, 100)]
    assert len(set(sets)) == 1, f"side {side}: measured set varies with agent count"
    assert len(sets[0]) == 40, f"side {side}: measured set is {len(sets[0])} files"
    assert len(s["100"]["background"]) == 99
EOF

echo "== observer + floor =="
mkdir -p "$WORK/dst"
"$BINARY" observer 19911 > "$WORK/observer.log" 2>&1 &
sleep 0.5
FLOOR=$("$BINARY" floor --observer 127.0.0.1:19911 --dest-root "$WORK/dst" --nonce 7)
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
  --side a --agents 10 --seconds 20 --label smoke --nonce 11)
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

echo "== patch mode edits a region of a large file =="
# The point of patch mode: the destination keeps a base that differs from
# the new content in one place, which is the only case a delta algorithm
# can exploit. Every other cell replaces a file with fresh random bytes,
# where a tool that always sent everything would score identically.
rm -rf "$WORK/src-patch" "$WORK/dst-patch"
cp -r "$WORK/src" "$WORK/src-patch"; cp -r "$WORK/src" "$WORK/dst-patch"
"$BINARY" observer 19915 > "$WORK/observer-patch.log" 2>&1 &
sleep 0.5
BEFORE=$(find "$WORK/src-patch/large" -type f -printf '%s\n' | paste -sd+ | bc)
python3 "$HERE/toysync.py" "$WORK/src-patch" "$WORK/dst-patch" &
TOY=$!
REPORT=$("$BINARY" agents \
  --root "$WORK/src-patch" --peer-root "$WORK/dst-patch" \
  --observer 127.0.0.1:19915 --partitions "$WORK/partitions.json" \
  --side a --agents 1 --seconds 15 --label smoke-patch --nonce 21 --mode patch)
kill $TOY 2>/dev/null || true
AFTER=$(find "$WORK/src-patch/large" -type f -printf '%s\n' | paste -sd+ | bc)
python3 - "$REPORT" "$BEFORE" "$AFTER" <<'EOF'
import json, sys
report = json.loads(sys.argv[1])
before, after = int(sys.argv[2]), int(sys.argv[3])
assert report["censored"] == 0, report
assert report["samples"] >= 3, report
# A patch rewrites a region and leaves the rest: the files must still be
# large. A replace-mode edit would have shrunk them to at most 64 KiB.
assert after > before * 0.9, (before, after)
assert after / 6 > 1_000_000, "patched files should still be megabytes"
EOF
echo "  patch: $(echo "$REPORT" | python3 -c 'import json,sys; r=json.load(sys.stdin); print({k: r[k] for k in ("samples","censored","p50_ms")})')"

echo "== a fan-out edit waits for every destination =="
# Two observers watching the same destination stand in for two machines.
# The point is the plumbing: the agent must open a connection per
# destination, announce to all of them, and only finish an edit once every
# one has confirmed. A regression here would silently turn a fan-out
# measurement back into a single-destination one.
"$BINARY" observer 19912 > "$WORK/observer2.log" 2>&1 &
sleep 0.5
FANOUT=$("$BINARY" agents \
  --root "$WORK/src-run" --peer-root "$WORK/dst" \
  --observer 127.0.0.1:19911,127.0.0.1:19912 --partitions "$WORK/partitions.json" \
  --side a --agents 10 --seconds 20 --label fanout --nonce 17)
python3 - "$FANOUT" <<'EOF'
import json, sys
report = json.loads(sys.argv[1])
assert report["destinations"] == 2, report
assert report["samples"] >= 5, report
assert report["censored"] == 0, report
# Both destinations are the same directory here, so they confirm together
# and the straggle is near zero. The field must still be reported.
assert report["spread_p50_ms"] is not None, report
assert report["spread_p50_ms"] < 200, report
EOF
echo "  fan-out: $(echo "$FANOUT" | python3 -c 'import json,sys; r=json.load(sys.stdin); print({k: r[k] for k in ("destinations","samples","censored","p50_ms","spread_p50_ms")})')"

echo "== censoring engages when nothing propagates =="
# No sync tool at all: every announced edit must come back censored, and
# the run must complete rather than hang or crash. A short deadline via the
# observer is simulated by pointing at a directory nothing writes to.
rm -rf "$WORK/void" && mkdir -p "$WORK/void"
# The window must outlast the 10s warmup, since warmup edits are excluded
# from censoring by design; the drain then waits out the 120s deadline.
CENSORED=$(timeout 400 "$BINARY" agents \
  --root "$WORK/src-run" --peer-root "$WORK/void" \
  --observer 127.0.0.1:19911 --partitions "$WORK/partitions.json" \
  --side b --agents 1 --seconds 15 --label censored --nonce 13)
python3 - "$CENSORED" <<'EOF'
import json, sys
report = json.loads(sys.argv[1])
assert report["samples"] == 0, report
assert report["censored"] >= 1, report
assert report["censored_over_ms"] == 120000, report
# With every attempt censored, no percentile may report a finite number.
assert report["p50_ms"] is None, report
EOF

echo "== job.py end to end, locally, with toysync as the subject =="
JOBHOME="$WORK/jobhome"
mkdir -p "$JOBHOME/bench" "$JOBHOME/corpus" "$JOBHOME/dest"
cp "$BINARY" "$JOBHOME/bench/benchmark"
cp "$HERE/toysync.py" "$JOBHOME/bench/toysync.py"
cp -r "$WORK/src" "$JOBHOME/corpus/smoke"
# A pristine copy, as bake produces, so the source-restore path runs.
mkdir -p "$JOBHOME/corpus-pristine"
cp -r "$WORK/src" "$JOBHOME/corpus-pristine/smoke"
mkdir -p "$JOBHOME/corpus/smoke.bench"
"$JOBHOME/bench/benchmark" partitions "$JOBHOME/corpus/smoke"   "$JOBHOME/corpus/smoke.bench/partitions.json" > /dev/null

echo "== source restoration actually restores =="
# Corrupt a partition-listed file, restore, and demand the pristine bytes
# back — this fails if restore_sources() is a successful no-op.
BENCH_HOME="$JOBHOME" BENCH_LOCAL=1 python3 - "$HERE" "$JOBHOME" <<'EOF'
import importlib.util, json, os, sys
here, jobhome = sys.argv[1], sys.argv[2]
spec = importlib.util.spec_from_file_location("job", os.path.join(here, "job.py"))
job = importlib.util.module_from_spec(spec)
spec.loader.exec_module(job)
partitions = json.load(open(f"{jobhome}/corpus/smoke.bench/partitions.json"))
victim = partitions["sides"]["a"]["10"]["measured"][0]
path = f"{jobhome}/corpus/smoke/{victim}"
pristine = open(f"{jobhome}/corpus-pristine/smoke/{victim}", "rb").read()
with open(path, "wb") as handle:
    handle.write(b"CORRUPTED BY SMOKE TEST")
job.restore_sources(["smoke"])
restored = open(path, "rb").read()
assert restored == pristine, f"restore left {victim} corrupted"
print(f"  restored {victim} ({len(pristine)} bytes)")
EOF

echo "== a pre-seeded cell starts converged and skips cold sync =="
# Seeding fills the destination from the corpus already present, then
# verifies by digest. A cell that seeds must report cold sync as
# pre_seeded rather than as an impossibly fast measurement.
SEEDSPEC='{"run":"smoke-run","pair":"pair-0","job":"seed-job","repeat":0,
       "cell":{"name":"seeded","corpora":["smoke"],"agents":1,"bidirectional":false,
               "pre_seeded":true},
       "tools":["toysync"]}'
BENCH_HOME="$JOBHOME" BENCH_LOCAL=1 BENCH_OBSERVER_PORT=19911 BENCH_WORKLOAD_SECONDS=10 \
  python3 "$HERE/job.py" --spec "$SEEDSPEC" --output "$JOBHOME/seed.jsonl"
python3 - "$JOBHOME/seed.jsonl" <<'EOF'
import json, sys
records = [json.loads(l) for l in open(sys.argv[1])]
kinds = [r["measurement"] for r in records]
assert "seeded" in kinds, "the seed step never ran"
cold = next(r for r in records if r["measurement"] == "cold_sync")
for corpus, timing in cold["timings"].items():
    assert timing.get("pre_seeded"), f"{corpus} did not report as pre-seeded: {timing}"
    assert timing.get("verified"), timing
    # A seeded cell must not invent a duration.
    assert "digest_verified_s" not in timing, timing
complete = next(r for r in records if r["measurement"] == "job_complete")
assert complete["statuses"] == {"toysync": "ok"}, complete
EOF
echo "  seeded cell: cold sync reported as pre-seeded, job ok"

SPEC='{"run":"smoke-run","pair":"pair-0","job":"smoke-job","repeat":0,
       "cell":{"name":"smoke","corpora":["smoke"],"agents":10,"bidirectional":false},
       "tools":["toysync"]}'
BENCH_HOME="$JOBHOME" BENCH_LOCAL=1 BENCH_OBSERVER_PORT=19911   BENCH_WORKLOAD_SECONDS=20   python3 "$HERE/job.py" --spec "$SPEC" --output "$JOBHOME/results.jsonl"
python3 - "$JOBHOME/results.jsonl" <<'EOF'
import json, sys
records = [json.loads(l) for l in open(sys.argv[1])]
kinds = {r["measurement"] for r in records}
for expected in ("job_start", "floor", "cold_sync", "idle_window",
                 "workload", "reconvergence", "resources", "job_complete"):
    assert expected in kinds, f"missing {expected} record"
cold = next(r for r in records if r["measurement"] == "cold_sync")
assert all(t["verified"] for t in cold["timings"].values()), cold
workload = next(r for r in records if r["measurement"] == "workload")
assert workload.get("samples", 0) >= 3, workload
assert workload.get("censored", 1) == 0, workload
reconv = next(r for r in records if r["measurement"] == "reconvergence")
assert all(reconv["converged"].values()), reconv
complete = next(r for r in records if r["measurement"] == "job_complete")
assert complete["statuses"] == {"toysync": "ok"}, complete
resources = next(r for r in records if r["measurement"] == "resources")
assert set(resources["phases"]) >= {"cold_sync", "idle", "workload"}
assert "offset_s" in resources["clock_offset"]
EOF

echo "== aggregate over the job output =="
mkdir -p "$WORK/results"
cp "$JOBHOME/results.jsonl" "$WORK/results/pair-0-results.jsonl"
python3 "$HERE/aggregate.py" "$WORK/results" > "$WORK/aggregate.json"
python3 - "$WORK/aggregate.json" <<'EOF'
import json, sys
report = json.load(open(sys.argv[1]))
assert report["jobs"]["started"] == 1 and report["jobs"]["completed"] == 1
assert not report["jobs"]["started_but_unfinished"]
assert not report["tainted_runs"], report["tainted_runs"]
latency = report["latency"]["smoke/toysync/smoke:a-to-b"]
assert isinstance(latency["p50_ms"], (int, float)), latency
assert latency["pooled_samples"] >= 3
assert report["floor_p50_ms"] is not None
cold = report["cold_sync_s"]["smoke/toysync/smoke"]
assert cold["digest_verified"] is not None
EOF

echo "== sampler attributes by pattern and window =="
sleep 120 & SLEEPER=$!
"$BINARY" sampler "sleep 120" "$WORK/rss.log" & SAMPLER=$!
sleep 3; kill $SAMPLER 2>/dev/null; kill $SLEEPER 2>/dev/null
LINES=$(wc -l < "$WORK/rss.log")
[ "$LINES" -ge 2 ] || { echo "FAIL: sampler produced $LINES lines"; exit 1; }
awk '{ if ($4 < 1) { print "FAIL: sampler lost its target"; exit 1 } }' "$WORK/rss.log"

echo "ALL SMOKE CHECKS PASSED"
