#!/bin/bash
# git-sync.sh — is keeping two git checkouts in sync, .git included, feasible?
#
# Two clones of a small repository on this machine, synchronized two-way by
# autobahn (the second over ssh to localhost, so the agent path is real),
# with the parts of .git that are machine-local ignored. A scripted
# sequence of git operations then runs on either side — commits, a fetch,
# branches, a checkout, a gc — and after each one both repositories are
# fsck'd and their `git status` compared. The result is a table of what
# happened: halts, conflicts by path, fsck failures, status disagreement.
#
#   bench/git-sync.sh [autobahn-binary]
set -u
BIN="$(realpath "${1:-$(cd "$(dirname "$0")/.." && pwd)/target/release/autobahn}")"
W="${GIT_SYNC_WORK:-/tmp/autobahn-git-sync}"
rm -rf "$W"; mkdir -p "$W"
A="$W/a"; B="$W/b"; ST="$W/state"; LOG="$W/autobahn.log"
mkdir -p "$ST"

# An origin, and a clone that becomes side A. Side B starts empty and is
# filled by the first synchronization — the shape of "bring my checkout
# to the other machine".
git init -q --bare "$W/origin.git"
git clone -q "$W/origin.git" "$A" 2>/dev/null
( cd "$A" && git config user.email t@t && git config user.name t
  for i in $(seq 1 20); do echo "line $i" > "f$i.txt"; done
  git add . && git commit -qm "initial" && git push -q origin HEAD:main )
mkdir -p "$B"

cat > "$W/config.toml" <<TOML
[groups.repo]
alpha = "$A"
mode = "two-way-conflict"
interval = 2
betas = ["localhost:$B"]
agent_command = "ssh localhost $BIN agent"
# What is machine-local in .git, and what is transient.
ignores = [".git/index", ".git/*.lock", ".git/**/*.lock", ".git/logs", ".git/gc.pid", ".git/FETCH_HEAD", ".git/ORIG_HEAD", ".git/COMMIT_EDITMSG"]
TOML
"$BIN" watch --debug --config "$W/config.toml" --state-root "$ST" > "$LOG" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null' EXIT

BM="$(cd "$(dirname "$0")" && pwd)/harness/target/release/benchmark"
# The working trees agree, and so do HEAD and every ref: what the sync is
# supposed to carry. (.git/index and the other ignored files differ by
# design, so the whole tree cannot simply be compared.)
sight() {
    ( cd "$1" && { find . -path ./.git -prune -o -type f -print0 | sort -z | xargs -0 md5sum; git show-ref 2>/dev/null; git rev-parse HEAD 2>/dev/null; cat .git/HEAD 2>/dev/null; } | md5sum )
}
converged() { [ "$(sight "$A")" = "$(sight "$B")" ]; }
settle() { for _ in $(seq 1 40); do converged && return 0; sleep 0.5; done; return 1; }

step() {
    local name="$1"; shift
    ( "$@" ) > "$W/step.out" 2>&1
    local ok="converged"; settle || ok="NOT converged"
    local fa fb; fa=$(cd "$A" && git fsck --no-progress 2>&1 | grep -vc "^$" ); fb=$(cd "$B" && git fsck --no-progress 2>&1 | grep -vc "^$")
    local sa sb; sa=$(cd "$A" && git status --porcelain=v1 2>/dev/null | sort | md5sum | cut -c1-8); sb=$(cd "$B" && git status --porcelain=v1 2>/dev/null | sort | md5sum | cut -c1-8)
    local ha hb; ha=$(cd "$A" && git rev-parse --short HEAD 2>/dev/null); hb=$(cd "$B" && git rev-parse --short HEAD 2>/dev/null)
    local conflicts halts; conflicts=$(grep -oE "[0-9]+ conflict" "$LOG" | awk '{s+=$1} END {print s+0}'); halts=$(grep -ci "halt" "$LOG")
    printf "%-28s %-14s fsck a/b %s/%s  HEAD a/b %s/%s  status %s  conflicts-so-far %s halts %s\n" "$name" "$ok" "$fa" "$fb" "$ha" "$hb" "$([ "$sa" = "$sb" ] && echo same || echo DIFFER)" "$conflicts" "$halts"
}

echo "binary: $BIN"; echo
step "first sync" true
step "commit on a" bash -c "cd '$A' && echo more >> f1.txt && git commit -qam 'edit f1'"
step "status on b" bash -c "cd '$B' && git status > /dev/null"
step "commit on b" bash -c "cd '$B' && git config user.email t@t && git config user.name t && echo b-side >> f2.txt && git commit -qam 'edit f2 on b'"
step "log on a sees b's commit" bash -c "cd '$A' && git log --oneline | grep -q 'edit f2 on b'"
step "branch+checkout on a" bash -c "cd '$A' && git checkout -qb feature && echo feat > f3.txt && git commit -qam feat"
step "checkout main on a" bash -c "cd '$A' && git checkout -q main"
step "gc on b" bash -c "cd '$B' && git gc -q"
step "status on a after gc" bash -c "cd '$A' && git status > /dev/null"
step "push from a" bash -c "cd '$A' && git push -q origin main"
step "fetch on b" bash -c "cd '$B' && git fetch -q origin"
step "reset on b" bash -c "cd '$B' && git reset -q --mixed HEAD"
echo
echo "cycles that reported conflicts or blocked paths:"
grep -E "cycle finished" "$LOG" | grep -vE " 0 conflict\(s\), 0 blocked" | head -8
echo; echo "cycle count: $(grep -c 'cycle finished' "$LOG"); errors: $(grep -ci 'error' "$LOG")"
echo "work dir kept at $W"
