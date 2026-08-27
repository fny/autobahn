#!/bin/bash
# The decisive property, tested without needing separate filesystems: three
# observers, one of them frozen with SIGSTOP so it can never answer. If the
# agent finished an edit on the first acknowledgement it would report fast
# samples. Waiting for every destination means every edit must be censored.
set -u
BM=/home/ubuntu/Workspace/autobahn/bench/harness/target/release/benchmark
W=$(mktemp -d); PIDS=()
cleanup() { for p in "${PIDS[@]:-}"; do kill -CONT "$p" 2>/dev/null; kill "$p" 2>/dev/null; done
            kill $(jobs -p) 2>/dev/null; rm -rf "$W"; }
trap cleanup EXIT
mkdir -p "$W/src" "$W/dst"
for d in $(seq 0 29); do
  mkdir -p "$W/src/dir$d"
  for f in $(seq 0 79); do head -c $((300 + RANDOM % 6000)) /dev/urandom > "$W/src/dir$d/f$f.bin"; done
done
cp -r "$W/src/." "$W/dst/"
"$BM" partitions "$W/src" "$W/parts.json" > /dev/null
for i in 1 2 3; do "$BM" observer $((19980+i)) > /dev/null 2>&1 & PIDS+=($!); done
sleep 1
# Copy source to the shared destination, so observers 1 and 2 can confirm.
python3 - "$W" <<'PY' &
import os, shutil, sys, time
w=sys.argv[1]
while True:
    for root,_,names in os.walk(f"{w}/src"):
        rel=os.path.relpath(root,f"{w}/src")
        t=os.path.join(f"{w}/dst",rel) if rel!="." else f"{w}/dst"
        os.makedirs(t,exist_ok=True)
        for n in names:
            s=os.path.join(root,n); d=os.path.join(t,n)
            try:
                if not os.path.exists(d) or os.stat(s).st_mtime_ns!=os.stat(d).st_mtime_ns:
                    shutil.copy2(s,d)
            except OSError: pass
    time.sleep(0.05)
PY
sleep 2
echo "--- 3 observers; the third is frozen 12s in, past warmup ---"
( sleep 12; kill -STOP "${PIDS[2]}" ) &
timeout 400 "$BM" agents --root "$W/src" --peer-root "$W/dst" \
  --observer "127.0.0.1:19981,127.0.0.1:19982,127.0.0.1:19983" \
  --partitions "$W/parts.json" --side a --agents 10 --seconds 25 \
  --label freeze --nonce 555 > "$W/r.json" 2>"$W/r.err"
python3 - "$W/r.json" <<'PYEOF'
import json,sys
r=json.load(open(sys.argv[1]))
print(" destinations:", r["destinations"], " samples:", r["samples"], " censored:", r["censored"])
print(" p50:", r["p50_ms"])
# Edits issued in the gap between warmup ending and the freeze complete
# legitimately, so the property is that everything after the freeze stops
# completing: censoring must dominate, and no percentile may be finite.
ok = r["censored"] > 3 * r["samples"] and r["p50_ms"] is None
print(" VERDICT:", "correct — once a destination goes silent, edits stop completing" if ok
      else "WRONG — edits kept completing without every destination confirming")
PYEOF
