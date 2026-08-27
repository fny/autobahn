#!/bin/bash
# Real filesystem semantics.
#
# probes.rs measures executability preservation, case sensitivity and
# Unicode behavior instead of assuming them (src/scan/probes.rs). Those
# paths are normally exercised against ext4, which preserves everything and
# so takes none of the interesting branches. A FAT image takes all of them:
# no executable bit, case-insensitive names.
set -u
AB=${AB:-/home/ubuntu/Workspace/autobahn/target/release/autobahn}
BM=${BM:-/home/ubuntu/Workspace/autobahn/bench/harness/target/release/benchmark}
IMG=$(mktemp -u /tmp/fatXXXXXX.img)
MNT=$(mktemp -d)
W=$(mktemp -d)
cleanup() { sudo umount "$MNT" 2>/dev/null; rm -rf "$IMG" "$MNT" "$W"; }
trap cleanup EXIT

dd if=/dev/zero of="$IMG" bs=1M count=128 status=none
mkfs.vfat -F 32 "$IMG" > /dev/null
sudo mount -o loop,uid=$(id -u),gid=$(id -g) "$IMG" "$MNT"
echo "mounted FAT32 at $MNT"

mkdir -p "$W/src" "$W/state" "$MNT/dst"
printf '#!/bin/sh\necho hi\n' > "$W/src/script.sh"; chmod 755 "$W/src/script.sh"
printf 'plain content'        > "$W/src/plain.txt"
mkdir -p "$W/src/nested/deep"; printf 'deep'  > "$W/src/nested/deep/file.txt"
printf 'accented'             > "$W/src/$(printf 'caf\xc3\xa9').txt"

cat > "$W/ab.toml" <<TOML
[groups.fat]
alpha = "$W/src"
mode = "one-way-safe"
interval = 2
betas = ["$MNT/dst"]
TOML

echo "--- first pass onto a filesystem that stores no executable bit ---"
timeout 120 "$AB" up --config "$W/ab.toml" --state-root "$W/state" --once > "$W/ab.log" 2>&1
rc=$?
echo "exit: $rc"
echo "files landed: $(find "$MNT/dst" -type f 2>/dev/null | wc -l) of 4"
ls -l "$MNT/dst" 2>/dev/null | head

echo "--- second pass must be a no-op, not a permanent diff ---"
# The executable bit cannot round-trip through FAT. If autobahn treated the
# missing bit as a change, every cycle would re-apply it forever.
timeout 120 "$AB" up --config "$W/ab.toml" --state-root "$W/state" --once > "$W/ab2.log" 2>&1
grep -c "change(s)" "$W/ab2.log" > /dev/null 2>&1
echo "second pass report:"; grep "synchronized" "$W/ab2.log" | tail -2

echo "--- a case collision the destination cannot represent ---"
printf 'lower' > "$W/src/Collide.txt"
printf 'upper' > "$W/src/COLLIDE.txt"
timeout 120 "$AB" up --config "$W/ab.toml" --state-root "$W/state" --once > "$W/ab3.log" 2>&1
echo "exit: $?"
echo "reported problems:"; grep -iE "problem|refus|collis|error" "$W/ab3.log" | head -3
echo "destination now holds: $(ls "$MNT/dst" | grep -ci collide) collide-ish name(s)"
