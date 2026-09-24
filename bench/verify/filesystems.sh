#!/bin/bash
# Real filesystem semantics.
#
# probes.rs measures executability preservation, case sensitivity and
# Unicode behavior instead of assuming them (src/scan/probes.rs). Those
# paths are normally exercised against ext4, which preserves everything and
# so takes none of the interesting branches. A FAT image takes all of them:
# no executable bit, case-insensitive names.
set -euo pipefail
AB=${AB:-/home/ubuntu/Workspace/autobahn/target/release/autobahn}
BM=${BM:-/home/ubuntu/Workspace/autobahn/bench/harness/target/release/benchmark}

# Fail at once on a binary that is missing or does not run, before
# anything is mounted.
"$AB" --version > /dev/null 2>&1 || { echo "autobahn at $AB does not run" >&2; exit 1; }

W=$(mktemp -d)
IMG="$W/fat.img"
MNT=$(mktemp -d)
cleanup() { sudo umount "$MNT" 2>/dev/null || true; rm -rf "$MNT" "$W"; }
trap cleanup EXIT

dd if=/dev/zero of="$IMG" bs=1M count=128 status=none
mkfs.vfat -F 32 "$IMG" > /dev/null
sudo mount -o "loop,uid=$(id -u),gid=$(id -g)" "$IMG" "$MNT"
echo "mounted FAT32 at $MNT"

mkdir -p "$W/src" "$W/state" "$MNT/dst"
printf '#!/bin/sh\necho hi\n' > "$W/src/script.sh"; chmod 755 "$W/src/script.sh"
printf 'plain content'        > "$W/src/plain.txt"
mkdir -p "$W/src/nested/deep"; printf 'deep'  > "$W/src/nested/deep/file.txt"
printf 'accented'             > "$W/src/$(printf 'caf\xc3\xa9').txt"

cat > "$W/ab.toml" <<TOML
[groups.fat]
alpha = "$W/src"
mode = "one-way-conflict"
interval = 2
betas = ["$MNT/dst"]
TOML

# The first two passes have nothing to refuse: a failure there is a broken
# subject, and the report after it would describe nothing.
must_pass() {  # rc log
  [ "$1" = 0 ] && return 0
  echo "autobahn sync failed ($1); the end of $2:" >&2
  tail -n 20 "$2" >&2
  exit 1
}

echo "--- first pass onto a filesystem that stores no executable bit ---"
rc=0
timeout 120 "$AB" sync --config "$W/ab.toml" --state-root "$W/state" > "$W/ab.log" 2>&1 || rc=$?
echo "exit: $rc"
must_pass "$rc" "$W/ab.log"
echo "files landed: $(find "$MNT/dst" -type f 2>/dev/null | wc -l) of 4"
# shellcheck disable=SC2012  # a listing for the reader, not a parse
ls -l "$MNT/dst" 2>/dev/null | head || true

echo "--- second pass must be a no-op, not a permanent diff ---"
# The executable bit cannot round-trip through FAT. If autobahn treated the
# missing bit as a change, every cycle would re-apply it forever.
rc=0
timeout 120 "$AB" sync --config "$W/ab.toml" --state-root "$W/state" > "$W/ab2.log" 2>&1 || rc=$?
echo "exit: $rc"
must_pass "$rc" "$W/ab2.log"
echo "second pass report:"; grep "synchronized" "$W/ab2.log" | tail -2 || true

echo "--- a case collision the destination cannot represent ---"
printf 'lower' > "$W/src/Collide.txt"
printf 'upper' > "$W/src/COLLIDE.txt"
rc=0
timeout 120 "$AB" sync --config "$W/ab.toml" --state-root "$W/state" > "$W/ab3.log" 2>&1 || rc=$?
echo "exit: $rc"
echo "reported problems:"; grep -iE "problem|refus|collis|error" "$W/ab3.log" | head -3 || true
echo "destination now holds: $(find "$MNT/dst" -maxdepth 1 -iname '*collide*' | wc -l) collide-ish name(s)"
