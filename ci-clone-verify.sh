#!/usr/bin/env bash
# ci-clone-verify.sh — the offline golden round trip (#87), as tinyanvil runs
# it, with the built binary and a real kernel. Unprivileged on dev: everything
# runs as root inside a QEMU guest (ublk and mount need it).
#
#   golden   `stormblock golden --tar` builds base.img (an ext4 golden)
#   format   `slab format` makes a fresh slab file
#   clone    `clone --golden base.img --name work1` imports it once and clones
#   dedup    a second `clone` of the same image imports nothing
#   mount    `attach --volume work1 --mount` hands the clone to the kernel:
#            the golden's file is there, a new one is written, Ctrl+C releases
#   out      `golden --from-volume work1 --seal` writes new.img and seals work1
#   check    new.img cloned into a second fresh slab carries both files; the
#            golden's other clone (work2), mounted read-only, does not have the
#            new file and has its own filesystem UUID
#
# Needs: cargo, qemu-system-x86_64, /boot/vmlinuz-$(uname -r) and its modules
# (ublk_drv, ext4), a static busybox. Run on dev:
#   sc-build 'sh ci-clone-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-clone.XXXXXX")
FAILS=0

say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
trap 'rm -rf "$W"' EXIT

for need in "$KERNEL" "$BUSYBOX"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
command -v qemu-system-x86_64 >/dev/null || { echo "SKIP: no qemu-system-x86_64"; exit 2; }

say "build"
cargo build --release --locked 2>&1 | tail -2
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/release/stormblock"
[ -x "$BIN" ] || { echo "FAIL: no binary"; exit 1; }

say "guest initramfs: busybox, stormblock, ublk_drv, ext4"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,etc,mnt,lib/mods}
cp "$BUSYBOX" "$I/bin/busybox"
for a in sh mount umount insmod sleep cat echo ls grep poweroff head tail sed awk cut tr kill \
         seq mkdir rm truncate tar sync blkid timeout; do
    ln -sf busybox "$I/bin/$a"
done
cp "$BIN" "$I/bin/stormblock"
ldd "$BIN" | grep -o '/[^ ]*' | while read -r lib; do
    mkdir -p "$I$(dirname "$lib")"; cp -L "$lib" "$I$lib"
done
: > "$I/lib/mods/order"
for m in ublk_drv ext4; do
    modprobe -S "$KVER" --show-depends "$m" 2>/dev/null | awk '$1=="insmod"{print $2}'
done | awk '!seen[$0]++' | while read -r ko; do
    base=$(basename "$ko" | sed 's/\.xz$//; s/\.zst$//')
    case "$ko" in
        *.xz) xz -dc "$ko" > "$I/lib/mods/$base" ;;
        *.zst) zstd -dcq "$ko" > "$I/lib/mods/$base" ;;
        *) cp "$ko" "$I/lib/mods/$base" ;;
    esac
    echo "$base" >> "$I/lib/mods/order"
done
cat > "$I/init" <<'EOF'
#!/bin/sh
export PATH=/bin
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
mount -t tmpfs run /run; mount -t tmpfs tmp /tmp
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>&1 | sed "s/^/LOG insmod $m: /"; done
r() { echo "RESULT $1 $2"; }
echo "GUEST kernel $(cat /proc/sys/kernel/osrelease)"
[ -e /dev/ublk-control ] || { r ublk_drv FAIL; poweroff -f; }
( sleep ${WATCHDOG:-400}; echo "WATCHDOG fired"; poweroff -f ) &
export RUST_LOG=warn
sb() { stormblock "$@" > /tmp/out 2>&1; rc=$?; sed 's/^/LOG   /' /tmp/out | tail -6; return $rc; }

# golden: an ext4 image built from a tar, no mount
mkdir -p /tmp/root/etc /tmp/root/usr/bin
echo "NAME=base" > /tmp/root/etc/os-release
tar -cf /tmp/base.tar -C /tmp/root .
sb golden --out /run/base.img --size 64M --tar /tmp/base.tar && r golden PASS || r golden FAIL

# format: a fresh slab file
truncate -s 1G /run/work.slab
sb slab format /run/work.slab --role data && r format PASS || r format FAIL

# clone, and the same image again
if sb clone --slab /run/work.slab --golden /run/base.img --name work1 && grep -q "imported from" /tmp/out; then
    r clone PASS; else r clone FAIL; fi
if sb clone --slab /run/work.slab --golden /run/base.img --name work2 && grep -q "already in the slab" /tmp/out; then
    r dedup PASS; else r dedup FAIL; fi
sb clone --slab /run/work.slab --golden /run/base.img --name work1 && r name-refused "FAIL (a second work1)" \
    || { grep -q "already holds a volume named work1" /tmp/out && r name-refused PASS || r name-refused FAIL; }

# mount: the clone in the kernel, written as a build would
stormblock attach --slab /run/work.slab --volume work1 --mount /mnt > /tmp/attach.log 2>&1 &
ap=$!
for i in $(seq 1 100); do [ -e /mnt/work1/etc/os-release ] && break; sleep 0.2; done
if grep -q "NAME=base" /mnt/work1/etc/os-release 2>/dev/null; then r mounted PASS; else
    r mounted FAIL; sed 's/^/LOG attach: /' /tmp/attach.log | tail -8; fi
U1=$(blkid -s UUID -o value $(awk '$2=="/mnt/work1"{print $1}' /proc/mounts) 2>/dev/null)
echo "installed" > /mnt/work1/usr/bin/tool
sync
kill -INT $ap; wait $ap; echo "LOG attach exited $?"
grep -q "/mnt/work1" /proc/mounts && r released "FAIL (still mounted)" || r released PASS

# out: back to a golden image, work1 sealed in the slab
sb golden --slab /run/work.slab --from-volume work1 --out /run/new.img --seal && grep -q "sealed in the slab" /tmp/out \
    && r out PASS || r out FAIL

# check: new.img into a second fresh slab, both files there
truncate -s 1G /run/check.slab
sb slab format /run/check.slab --role data
sb clone --slab /run/check.slab --golden /run/new.img --name check || r check-clone FAIL
sb slab cat --slab /run/check.slab --volume check --out /tmp/tool /usr/bin/tool
grep -q installed /tmp/tool 2>/dev/null && r check-new-file PASS || r check-new-file FAIL
sb slab cat --slab /run/check.slab --volume check --out /tmp/osr /etc/os-release
grep -q NAME=base /tmp/osr 2>/dev/null && r check-base-file PASS || r check-base-file FAIL
# work1, sealed, clones again by name
sb clone --slab /run/work.slab --golden work1 --name work3 && r reclone PASS || r reclone FAIL

# the golden's other clone, read-only: untouched, its own identity
stormblock attach --slab /run/work.slab --volume work2 --mount /mnt --ro > /tmp/attach2.log 2>&1 &
ap=$!
for i in $(seq 1 100); do [ -e /mnt/work2/etc/os-release ] && break; sleep 0.2; done
[ -e /mnt/work2/etc/os-release ] && [ ! -e /mnt/work2/usr/bin/tool ] && r golden-untouched PASS || r golden-untouched FAIL
U2=$(blkid -s UUID -o value $(awk '$2=="/mnt/work2"{print $1}' /proc/mounts) 2>/dev/null)
echo "LOG uuids work1=$U1 work2=$U2"
[ -n "$U1" ] && [ -n "$U2" ] && [ "$U1" != "$U2" ] && r own-uuid PASS || r own-uuid FAIL
kill -INT $ap; wait $ap
echo "GUEST done"
poweroff -f
EOF
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"

say "boot $KERNEL in QEMU"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 600 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 3072 -smp 4 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 panic=-1 loglevel=4" -nic none > "$W/guest.log" 2>&1
tr -d '\r' < "$W/guest.log" | grep -E '^(RESULT|GUEST|LOG|WATCHDOG)|panick'

for m in golden format clone dedup name-refused mounted released out check-new-file check-base-file \
         reclone golden-untouched own-uuid; do
    tr -d '\r' < "$W/guest.log" | grep -q "^RESULT $m PASS" || fail "$m"
done
if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
