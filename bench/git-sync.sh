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
#
# It works in a fresh private directory (mktemp -d), removed when it ends;
# GIT_SYNC_KEEP=1 keeps it for a look afterwards.
set -euo pipefail
BIN="$(realpath "${1:-$(cd "$(dirname "$0")/.." && pwd)/target/release/autobahn}")"
"$BIN" --version > /dev/null 2>&1 || { echo "autobahn at $BIN does not run" >&2; exit 1; }
# Private and fresh: a fixed path under /tmp could have been made by
# someone else first, and the configuration written here names the
# command that runs the agent.
W="$(mktemp -d "${TMPDIR:-/tmp}/autobahn-git-sync.XXXXXX")"
PID=""
finish() {
    if [ -n "$PID" ]; then kill "$PID" 2>/dev/null || true; wait "$PID" 2>/dev/null || true; fi
    if [ "${GIT_SYNC_KEEP:-}" = 1 ]; then echo "work dir kept at $W"; else rm -rf "$W"; fi
}
trap finish EXIT
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
sleep 1
if ! kill -0 "$PID" 2>/dev/null; then
    PID=""
    echo "autobahn watch did not start; the end of its log:" >&2
    tail -n 20 "$LOG" >&2
    exit 1
fi

# The working trees agree, and so do HEAD and every ref: what the sync is
# supposed to carry. (.git/index and the other ignored files differ by
# design, so the whole tree cannot simply be compared.)
sight() {
    ( cd "$1" && { find . -path ./.git -prune -o -type f -print0 | sort -z | xargs -0 md5sum; git show-ref 2>/dev/null || true; git rev-parse HEAD 2>/dev/null || true; cat .git/HEAD 2>/dev/null || true; } | md5sum )
}
converged() { [ "$(sight "$A")" = "$(sight "$B")" ]; }
settle() { for _ in $(seq 1 40); do converged && return 0; sleep 0.5; done; return 1; }

step() {
    local name="$1"; shift
    # A step's own failure is part of what the table reports, not a
    # reason to stop: the checks below say what it left behind.
    ( "$@" ) > "$W/step.out" 2>&1 || true
    local ok="converged"; settle || ok="NOT converged"
    local fa fb; fa=$(git -C "$A" fsck --no-progress 2>&1 | grep -vc "^$" || true); fb=$(git -C "$B" fsck --no-progress 2>&1 | grep -vc "^$" || true)
    local sa sb; sa=$(cd "$A" && git status --porcelain=v1 2>/dev/null | sort | md5sum | cut -c1-8); sb=$(cd "$B" && git status --porcelain=v1 2>/dev/null | sort | md5sum | cut -c1-8)
    local ha hb; ha=$(git -C "$A" rev-parse --short HEAD 2>/dev/null || true); hb=$(git -C "$B" rev-parse --short HEAD 2>/dev/null || true)
    local conflicts halts; conflicts=$({ grep -oE "[0-9]+ conflict" "$LOG" || true; } | awk '{s+=$1} END {print s+0}'); halts=$(grep -ci "halt" "$LOG" || true)
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
{ grep -E "cycle finished" "$LOG" || true; } | { grep -vE " 0 conflict\(s\), 0 blocked" || true; } | head -8
echo; echo "cycle count: $(grep -c 'cycle finished' "$LOG" || true); errors: $(grep -ci 'error' "$LOG" || true)"
