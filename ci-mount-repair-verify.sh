#!/usr/bin/env bash
# A volume the kernel refuses, repaired by the shipped initramfs (#532).
#
# server8's 10 kept `-data` volumes: `orphan file block N: bad magic`, every
# mount of every boot. The in-process tests cannot see what counts here: that
# the real kernel refuses these bytes and takes them after the repair, and that
# e2fsck agrees. So:
#
#   1. files: mkfs.ext4 -O orphan_file at 64 MiB and 4 GiB, the orphan file's
#      blocks zeroed from block 0 and from block 217 as on server8; e2fsck sees
#      it; `stormblock ext4-repair`; e2fsck -fn clean;
#   2. a node disk with a clone of a 64 MiB filesystem missing the 1 MiB extent
#      its orphan file is in (what server8's numbers point at), the shipped
#      initramfs on the build VM's kernel in QEMU: the kernel's `bad magic`, the
#      FINDING, `mounted: ... (after a repair)`, a clean r/w mount;
#   3. a filesystem the repair cannot save (its metadata overwritten): FATAL
#      naming it, the other volume mounted;
#   4. rd.stormblock.mount-repair=off: refused, nothing repaired, said.
#
# Needs: cargo (musl target), qemu-system-x86_64, /boot/vmlinuz-$(uname -r), its
# modules, busybox, e2fsprogs. Unprivileged.
# Run through sc-build:  sc-build 'bash ci-mount-repair-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-mrep.XXXXXX")
trap 'rm -rf "${W:?}"' EXIT
FAILS=0
say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
ok()   { echo "  ok    $*"; }

[ -r "$KERNEL" ] || { echo "SKIP: no $KERNEL"; exit 2; }
export PATH="$PATH:/usr/sbin:/sbin"

say "build the engine (musl) and the initramfs"
cargo build --release --locked --target x86_64-unknown-linux-musl --bin stormblock 2>&1 | tail -2
TDIR=$(cargo metadata --format-version 1 --no-deps 2>/dev/null \
    | sed 's/.*"target_directory":"\([^"]*\)".*/\1/')
BIN="${TDIR:-$ROOT/target}/x86_64-unknown-linux-musl/release/stormblock"
[ -x "$BIN" ] || { echo "FAIL: no engine at $BIN"; exit 1; }
bash scripts/build-stormblock-initramfs.sh "$BIN" "$KVER" "$W/initrd.img" > "$W/initrd.log" 2>&1 \
    || { tail -20 "$W/initrd.log"; echo "FAIL: the initramfs did not build"; exit 1; }

# The orphan file's inode: mkfs puts it in the superblock, and debugfs's `stats`
# names it. Its blocks, one "logical physical" a line, from debugfs's `bmap`.
orphan_inum() {
    debugfs -R stats "$1" 2>/dev/null | awk -F: '/[Oo]rphan file inode/ { gsub(/ /, "", $2); print $2; exit }'
}
# damage IMAGE FROM: zero the orphan file's blocks from logical FROM on.
damage() {
    local img=$1 from=$2 inum n=0 lb=$2 phys first=""
    inum=$(orphan_inum "$img")
    [ -n "$inum" ] || { fail "$img: no orphan file inode"; return 1; }
    while :; do
        phys=$(debugfs -R "bmap <$inum> $lb" "$img" 2>/dev/null | tail -1 | tr -d ' ')
        case "$phys" in ''|0|*[!0-9]*) break ;; esac
        first=${first:-$phys}
        dd if=/dev/zero of="$img" bs=4096 seek="$phys" count=1 conv=notrunc status=none
        n=$((n + 1)); lb=$((lb + 1))
    done
    [ "$n" -gt 0 ] || { fail "$img: no orphan file block to zero from $from"; return 1; }
    echo "  $(basename "$img"): inode $inum, $n orphan file block(s) zeroed from block $from, at $first ($((first % 256)) into a 1 MiB extent)"
}

say "1: files, e2fsck on both sides"
for spec in "64M 0" "4G 217"; do
    set -- $spec
    f="$W/f$1.img"
    truncate -s "$1" "$f"
    mkfs.ext4 -q -F -b 4096 -O orphan_file "$f" || fail "mkfs.ext4 $1"
    cp --sparse=always "$f" "$f.good"
    damage "$f" "$2"
    if e2fsck -fn "$f" > "$W/before" 2>&1; then
        fail "$1: e2fsck did not see the zeroed blocks"; cat "$W/before"
    else
        ok "$1: e2fsck sees it: $(grep -im1 orphan "$W/before")"
    fi
    if "$BIN" ext4-repair "$f" > "$W/rep" 2>&1; then ok "$1: repaired"; else fail "$1: repair exit $?"; fi
    sed 's/^/    /' "$W/rep" | head -8
    if grep -q "FINDING: .*orphan file block" "$W/rep"; then ok "$1: the finding"; else fail "$1: no FINDING"; fi
    if e2fsck -fn "$f" > "$W/after" 2>&1; then ok "$1: e2fsck -fn clean"; else fail "$1: e2fsck after"; cat "$W/after"; fi
    if "$BIN" ext4-repair "$f" > "$W/rep2" 2>&1 && ! grep -q FINDING "$W/rep2"; then
        ok "$1: a second run finds nothing"
    else
        fail "$1: a second run"; cat "$W/rep2"
    fi
done

# What server8's numbers point at: block 217 of a 4 GiB orphan file is exactly on
# a 1 MiB boundary, so what read wrong there is a whole extent of the volume, not
# some blocks of a file. The same, with everything else that extent holds.
say "1b: a whole 1 MiB extent of zeros (extent 8 at 64 MiB, 35 at 4 GiB)"
for spec in "64M 8" "4G 35"; do
    set -- $spec
    f="$W/x$1.img"
    cp --sparse=always "$W/f$1.img.good" "$f"
    dd if=/dev/zero of="$f" bs=1M seek="$2" count=1 conv=notrunc status=none
    if e2fsck -fn "$f" > "$W/before" 2>&1; then
        fail "$1: e2fsck did not see the lost extent"
    else
        ok "$1: e2fsck sees it: $(grep -im1 orphan "$W/before")"
    fi
    if "$BIN" ext4-repair "$f" > "$W/rep" 2>&1; then ok "$1: repaired"; else fail "$1: repair exit $?"; fi
    grep -v ' INFO ' "$W/rep" | sed 's/^/    /' | head -12
    if e2fsck -fn "$f" > "$W/after" 2>&1; then ok "$1: e2fsck -fn clean"; else fail "$1: e2fsck after"; head -30 "$W/after"; fi
done

"$BIN" ext4-repair "$W/no-such-file" > /dev/null 2>&1
if [ $? = 2 ] && [ ! -e "$W/no-such-file" ]; then ok "no such file: exit 2, nothing made"; else fail "a missing file"; fi
head -c 1M /dev/urandom > "$W/noise"
"$BIN" ext4-repair "$W/noise" > "$W/rep" 2>&1
if [ $? = 2 ]; then ok "not ext4: exit 2"; else fail "not ext4"; cat "$W/rep"; fi

say "lay node disks: a damaged and a good volume; a wrecked and a good one"
mkdir -p "$W/root/etc" "$W/root/d/hurt" "$W/root/d/good"
printf 'NAME=stormcos\nVERSION_ID=11.79\n' > "$W/root/etc/os-release"
mkfs.ext4 -q -b 4096 -d "$W/root" "$W/rootfs.img" 32M || { echo "FAIL: mkfs.ext4 -d"; exit 1; }
cp --sparse=always "$W/f64M.img.good" "$W/good.img"
cp --sparse=always "$W/f64M.img.good" "$W/hurt.img"
dd if=/dev/zero of="$W/hurt.img" bs=1M seek=8 count=1 conv=notrunc status=none
cp --sparse=always "$W/f64M.img.good" "$W/wreck.img"
# Past the superblock: the group descriptors and the inode table overwritten.
# The superblock stays, so it is still ext4 and the repair has to say no.
dd if=/dev/urandom of="$W/wreck.img" bs=4096 seek=1 count=600 conv=notrunc status=none

lay() { # image.img hurt-file
    cat > "$W/$1.toml" <<IMG
name = "mrep"
size = "768M"

[data_slab]
size = "256M"
  [[data_slab.golden]]
  name  = "good.golden"
  file  = "$W/good.img"
  clone = "good"
  [[data_slab.golden]]
  name  = "hurt.golden"
  file  = "$2"
  clone = "hurt"

[slab]
size = "rest"
  [[slab.golden]]
  name  = "stormpump.golden"
  file  = "$W/rootfs.img"
  clone = "stormpump"
IMG
    (cd "$W" && "$BIN" image build --spec "$1.toml" --out "$1.img" > "$1.log" 2>&1)
}
lay disk "$W/hurt.img" || { tail -20 "$W/disk.log"; echo "FAIL: image build"; exit 1; }
if ! lay wrecked "$W/wreck.img"; then
    fail "the wrecked disk did not build: the golden's seal checks it"
    tail -5 "$W/wrecked.log" | sed 's/^/    | /'
fi

ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
boot() { # name disk extra-cmdline
    local name=$1 log="$W/$1.log"
    cp --sparse=always "$W/$2.img" "$W/$name.disk"
    qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 1024 -smp 2 \
        -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.img" \
        -drive "file=$W/$name.disk,format=raw,if=virtio" -nic none \
        -append "console=ttyS0 panic=-1 loglevel=4 root=/dev/ublkb0 rd.stormblock.slab=/dev/vda stormblock.volume=stormpump $3" \
        > "$log" 2>&1 < /dev/null &
    local q=$! t=0
    while [ $t -lt 300 ]; do
        sleep 2; t=$((t + 2))
        if grep -qE 'Dropping to shell|volume\(s\) mounted in' "$log" 2>/dev/null; then sleep 4; break; fi
        kill -0 $q 2>/dev/null || break
    done
    kill $q 2>/dev/null; wait $q 2>/dev/null
    tr -d '\r' < "$log" > "$W/$name.txt"
    echo "  ($name: ${t}s)"
}
has()    { grep -qE -- "$2" "$W/$1.txt"; }
expect() { if has "$1" "$2"; then ok "$1: $3"; else fail "$1: $3 - no '$2'"; tail -40 "$W/$1.txt" | sed 's/^/    | /'; fi; }
refuse() { if has "$1" "$2"; then fail "$1: $3"; grep -E -- "$2" "$W/$1.txt" | head -5 | sed 's/^/    | /'; else ok "$1: $3"; fi; }

MOUNTS="rd.stormblock.mount=good:/d/good,hurt:/d/hurt"

say "2: the kernel refuses it, the repair, the kernel takes it"
boot A disk "$MOUNTS"
expect A "orphan file block 0: bad magic" "the kernel's own refusal (server8's line)"
expect A "FINDING: /dev/ublkb[0-9]+: 32 of the 32 orphan file block" "the finding"
expect A "mounted: /dev/ublkb[0-9]+ -> /d/hurt \(after a repair\)" "mounted after the repair"
expect A "mounted: /dev/ublkb[0-9]+ -> /d/good$" "the good volume mounted, unrepaired"
refuse A "^FATAL: .*not mounted" "no FATAL"
refuse A "StormBlock starting" "the repair's INFO lines stay off the console"
expect A "32 all zeros" "says what the blocks were"

if [ -s "$W/wrecked.img" ]; then
    say "3: past repair: loud"
    boot B wrecked "$MOUNTS"
    expect B "would not mount at /d/hurt" "the warning"
    expect B "^FATAL: 1 volume\(s\) of the mount list are not mounted" "the FATAL"
    expect B "/d/hurt .*could not be repaired" "named, with why"
    expect B "mounted: /dev/ublkb[0-9]+ -> /d/good$" "the good volume still mounted"
fi

say "4: off by the command line"
boot C disk "$MOUNTS rd.stormblock.mount-repair=off"
expect C "orphan file block 0: bad magic" "refused"
refuse C "FINDING: /dev/ublkb" "no repair"
expect C "repair is off" "says why"

if [ "$FAILS" = 0 ]; then echo "ALL PASS"; else echo "FAILURES: $FAILS"; exit 1; fi
