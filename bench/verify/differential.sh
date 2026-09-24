#!/bin/bash
# Differential test: autobahn and mutagen implement the same reconciliation
# semantics (autobahn is a port), so the same scenario must produce the same
# tree on the far side. Where they disagree, one of them is wrong.
#
# Each scenario builds a source tree, an initial destination state, runs one
# tool to convergence, and records the resulting destination manifest.
set -euo pipefail
AB=${AB:-/home/ubuntu/Workspace/autobahn/target/release/autobahn}
BM=${BM:-/home/ubuntu/Workspace/autobahn/bench/harness/target/release/benchmark}
MU=${MU:-/home/ubuntu/Workspace/mutagen-bench/bin-stock/mutagen}
OUT=~/differential

# Fail at once on a tool that is missing or does not run: every scenario
# would otherwise record a failure that says nothing about semantics.
"$AB" --version > /dev/null 2>&1 || { echo "autobahn at $AB does not run" >&2; exit 1; }
"$MU" version > /dev/null 2>&1 || { echo "mutagen at $MU does not run" >&2; exit 1; }
[ -x "$BM" ] || { echo "no benchmark harness at $BM" >&2; exit 1; }
mkdir -p "$OUT"; rm -f "$OUT"/*.txt "$OUT"/result.jsonl

build_scenario() {   # $1 = scenario dir
  local d=$1; rm -rf "$d"; mkdir -p "$d/src" "$d/dst"
  case "$2" in
    plain)        printf 'one' > "$d/src/a.txt"; printf 'two' > "$d/src/b.txt";;
    nested)       mkdir -p "$d/src/x/y/z"; printf 'deep' > "$d/src/x/y/z/f.txt";;
    empty_dirs)   mkdir -p "$d/src/empty" "$d/src/also/empty";;
    exec_bits)    printf '#!/bin/sh\n' > "$d/src/run.sh"; chmod 755 "$d/src/run.sh";
                  printf 'plain' > "$d/src/plain.txt";;
    beta_extra)   printf 'src' > "$d/src/shared.txt"; printf 'only-on-beta' > "$d/dst/extra.txt";;
    beta_newer)   printf 'from-alpha' > "$d/src/c.txt"; printf 'from-beta' > "$d/dst/c.txt";;
    unicode)      printf 'nfc' > "$d/src/$(printf 'caf\xc3\xa9').txt";;
    big_name)     printf 'x' > "$d/src/$(python3 -c 'print("n"*200)').txt";;
    many_small)   mkdir -p "$d/src/many"; for i in $(seq 1 300); do printf "%s" "$i" > "$d/src/many/f$i"; done;;
    replace_type) mkdir -p "$d/src/thing"; printf 'inside' > "$d/src/thing/inner";
                  printf 'was-a-file' > "$d/dst/thing";;
  esac
}

run_autobahn() {   # $1 = scenario dir
  cat > "$1/ab.toml" <<TOML
[groups.diff]
alpha = "$1/src"
mode = "two-way-conflict"
interval = 2
betas = ["$1/dst"]
TOML
  # Its own state root per scenario: nothing carries over between them,
  # and the real ~/.autobahn is never touched.
  local rc=0
  timeout 120 "$AB" sync --config "$1/ab.toml" --state-root "$1/state" > "$1/ab.log" 2>&1 || rc=$?
  echo "$rc"
}

run_mutagen() {    # $1 = scenario dir
  rm -rf ~/.mutagen ~/.mutagen-dev
  "$MU" daemon start > /dev/null 2>&1 || true
  "$MU" sync create --name=diff --sync-mode=two-way-safe \
    "$1/src" "$1/dst" > "$1/mu.log" 2>&1 || { echo 1; return; }
  local status
  for _ in $(seq 1 60); do
    status=$("$MU" sync list 2>/dev/null | grep -c "Watching for changes" || true)
    [ "$status" -ge 1 ] && break
    sleep 2
  done
  sleep 3
  "$MU" sync terminate diff > /dev/null 2>&1 || true
  "$MU" daemon stop > /dev/null 2>&1 || true
  echo 0
}

for scenario in plain nested empty_dirs exec_bits beta_extra beta_newer unicode big_name many_small replace_type; do
  for tool in autobahn mutagen; do
    d=~/diffwork/$scenario-$tool
    build_scenario "$d" "$scenario"
    if [ "$tool" = autobahn ]; then rc=$(run_autobahn "$d"); else rc=$(run_mutagen "$d"); fi
    manifest=$("$BM" manifest full "$d/dst" 2>/dev/null || echo "<manifest failed>")
    python3 - "$scenario" "$tool" "$rc" "$manifest" >> "$OUT/result.jsonl" <<'PY'
import json,sys
print(json.dumps({"scenario":sys.argv[1],"tool":sys.argv[2],"rc":sys.argv[3],
                  "manifest":" ".join(sys.argv[4:])}))
PY
  done
done
echo "DIFFERENTIAL COMPLETE"
