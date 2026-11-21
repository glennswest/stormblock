#!/bin/bash
# A release ISO read off a block device by `stormdist://` (#442), in QEMU.
#
# /init's `rd.stormblock.iso=auto` names /dev/sr0, and a USB stick is a
# /dev/sdX: the ISO is read through a block device, never a file. stormdist's
# reader (mkfs-iso v0.1.0) sized a block device by `metadata().len()`, which
# is 0, so every read off a CD or a stick failed (mkfs.iso.rs#35; v0.1.1
# seeks to the end). This makes a release ISO with stormdist's own `make`
# (the flavour's disk: a laid system drive with the volume `pod-logs` in its
# slab), boots the build host's kernel with the ISO attached twice, as a CD
# (/dev/sr0) and as a raw read-only disk (/dev/vda, the stick's stand-in),
# and asks the static stormblock in the guest, on each device:
#
#   slab volumes stormdist:///dev/<d>?flavour=cilium&key=…   volume pod-logs
#
# the disk's GPT, its system slab and the volume's record, every chunk
# checked by its sha256 on the way. The control is the same script on a commit that pins
# mkfs-iso v0.1.0: both devices fail to open.
#
#   sc-build 'bash ci-isodev-blockdev-verify.sh'
#
# Needs: cargo with the musl target, qemu-system-x86_64,
# /boot/vmlinuz-$(uname -r) and its modules, busybox. No root.
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
[ -r "$KERNEL" ] || { echo "ERROR: cannot read $KERNEL"; exit 1; }
BB=$(command -v busybox.musl.static || command -v busybox) || { echo "ERROR: no busybox"; exit 1; }
QEMU=$(command -v qemu-system-x86_64) || { echo "ERROR: no qemu-system-x86_64"; exit 1; }
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-isodev.XXXXXX")
trap '[ -n "${QPID:-}" ] && kill "$QPID" 2>/dev/null; rm -rf "$W"' EXIT

echo "== the release ISO (stormdist make, in the it binary)"
mkdir -p "$W/out"
STORMBLOCK_ISO_OUT="$W/out" cargo test --locked --test it -- --ignored --exact \
    integration_isodev::writes_the_guests_release_iso 2>&1 | tail -3
[ -s "$W/out/12.99.iso" ] || { echo "ERROR: no ISO was made"; exit 1; }
ISO="$W/out/12.99.iso"
echo "  $(stat -c %s "$ISO") bytes, the disk's sha256 $(cat "$W/out/disk.sha256")"

echo "== the stormblock binary (musl, static)"
cargo build --locked --target x86_64-unknown-linux-musl --bin stormblock 2>&1 | tail -1
BIN="${CARGO_TARGET_DIR:-target}/x86_64-unknown-linux-musl/debug/stormblock"
[ -x "$BIN" ] || { echo "ERROR: no $BIN"; exit 1; }

R=$W/root
mkdir -p "$R"/{bin,sbin,proc,sys,dev,tmp,lib/mods}
cp "$BB" "$R/bin/busybox"
for c in sh mount insmod sleep cat echo ls poweroff; do ln -s busybox "$R/bin/$c"; done
cp "$BIN" "$R/sbin/stormblock"
cp "$W/out/release-key.pub" "$R/release-key.pub"
: > "$R/lib/mods/order"
for m in ata_piix sr_mod virtio_pci virtio_blk; do
    modprobe -S "$KVER" --show-depends "$m" 2>/dev/null | awk '$1=="insmod"{print $2}'
done | awk '!seen[$0]++' | while read -r ko; do
    base=$(basename "$ko" | sed 's/\.xz$//; s/\.zst$//')
    case "$ko" in
        *.xz) xz -dc "$ko" > "$R/lib/mods/$base" ;;
        *.zst) zstd -dcq "$ko" > "$R/lib/mods/$base" ;;
        *) cp "$ko" "$R/lib/mods/$base" ;;
    esac
    echo "$base" >> "$R/lib/mods/order"
done
echo "  modules: $(tr '\n' ' ' < "$R/lib/mods/order")(the rest built in)"
cat > "$R/init" <<'EOF'
#!/bin/sh
export PATH=/bin:/sbin
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>/dev/null; done
for _ in 1 2 3 4 5 6 7 8 9 10; do [ -b /dev/sr0 ] && [ -b /dev/vda ] && break; sleep 1; done
for d in sr0 vda; do
    echo "GUEST $d: $(cat /sys/block/$d/size 2>/dev/null || echo none) sectors"
    u="stormdist:///dev/$d?flavour=cilium&key=/release-key.pub"
    echo "GUEST-BEGIN $d volumes"
    stormblock slab volumes "$u" 2>&1
    echo "GUEST-END $d volumes rc=$?"
done
echo GUEST-DONE
poweroff -f
EOF
chmod +x "$R/init"
( cd "$R" && find . | cpio -o -H newc --quiet | gzip -1 ) > "$W/initrd.gz"

ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
echo "== QEMU ($ACCEL): $KERNEL, the ISO as a CD and as a raw disk"
timeout 300 "$QEMU" -accel "$ACCEL" -m 1024 -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 panic=-1 quiet" -display none -no-reboot \
    -drive "file=$ISO,media=cdrom,if=ide,readonly=on" \
    -drive "file=$ISO,if=virtio,format=raw,readonly=on" \
    -serial "file:$W/serial.log" &
QPID=$!
wait "$QPID"
QPID=""
grep -a "^GUEST\|stormdist\|slab\|volume\|ERROR\|rror" "$W/serial.log" | grep -v '^\[' || true
grep -aq GUEST-DONE "$W/serial.log" || { tail -40 "$W/serial.log"; echo "ERROR: the guest did not finish"; exit 1; }

FAILS=0
for d in sr0 vda; do
    vol=$(awk -v d="$d" '$0 ~ "^GUEST-BEGIN "d" volumes" {on=1; next} $0 ~ "^GUEST-END "d" volumes" {on=0} on' "$W/serial.log")
    if echo "$vol" | grep -q "volume pod-logs"; then
        echo "PASS: /dev/$d: the flavour's disk read off the ISO, its slab and pod-logs found"
    else
        echo "FAIL: /dev/$d: pod-logs not found through stormdist:///dev/$d"
        FAILS=$((FAILS + 1))
    fi
done
[ "$FAILS" -eq 0 ] || { echo "== $FAILS device(s) failed"; exit 1; }
echo "== a release ISO on a CD and on a raw disk reads through stormdist://"
