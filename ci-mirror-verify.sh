#!/usr/bin/env bash
# ci-mirror-verify.sh — a live mirror of a mounted volume to a peer engine
# (#295, stormstorage#44) on a real kernel. Two engines run as root in a QEMU
# guest, each on its own virtio disk:
#
#   S  serves the volume over ublk; an ext2 filesystem on it is mounted and
#      written to (files appended, synced) the whole time
#   T  a fresh volume of the same size, attached over NVMe/TCP for S's host
#
#   synced    POST S/volumes/{id}/mirror {target}: the copy runs under the
#             writer and reaches `synced`; the writer never sees an error
#   switch    writer stopped, the filesystem unmounted (the guest paused),
#             DELETE S/volumes/{id}/mirror: flushed and dropped
#   bytes     T's volume, attached over ublk on T, equals S's device byte
#             for byte
#   mounts    T's copy mounts, and every file reads as written on S
#
# Needs: cargo, qemu-system-x86_64, /boot/vmlinuz-$(uname -r) and its
# ublk_drv and ext4, a static busybox, curl. Run through sc-build:
#   sc-build 'bash ci-mirror-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
CURL=$(command -v curl || true)
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-mirror.XXXXXX")
FAILS=0
say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
trap 'rm -rf "$W"' EXIT

for need in "$KERNEL" "$BUSYBOX" "$CURL"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
command -v qemu-system-x86_64 >/dev/null || { echo "SKIP: no qemu-system-x86_64"; exit 2; }

say "build"
cargo build --release --locked --bin stormblock 2>&1 | tail -2
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/release/stormblock"
[ -x "$BIN" ] || { echo "FAIL: no binary"; exit 1; }

say "guest initramfs: busybox, stormblock, curl, ublk_drv, ext4"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,etc,lib/mods,mnt,mnt2}
cp "$BUSYBOX" "$I/bin/busybox"
for a in sh mount umount insmod ip sleep cat echo ls grep dd cmp poweroff dmesg head tail wc sed \
         awk cut tr kill seq mkdir rm timeout mke2fs md5sum sync touch; do
    ln -sf busybox "$I/bin/$a"
done
for b in "$BIN" "$CURL"; do
    cp "$b" "$I/bin/$(basename "$b")"
    ldd "$b" | grep -o '/[^ ]*' | while read -r lib; do
        mkdir -p "$I$(dirname "$lib")"; cp -L "$lib" "$I$lib"
    done
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
ip link set lo up
r() { echo "RESULT $1 $2"; }
echo "GUEST kernel $(cat /proc/sys/kernel/osrelease)"
[ -e /dev/ublk-control ] || { r ublk_drv FAIL; poweroff -f; }
S=http://127.0.0.1:9101/api/v1
T=http://127.0.0.1:9102/api/v1
api() { curl -s -m 120 -H 'Authorization: Bearer t' -H 'Content-Type: application/json' "$@"; }
idof() { sed -n 's/.*"id":"\([0-9a-f-]*\)".*/\1/p' | head -1; }
hint() { sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p'; }
logs() { sed 's/^/LOG S /' /run/s.log | tail -15; sed 's/^/LOG T /' /run/t.log | tail -15; }
bail() { r "$1" "FAIL ($2)"; logs; poweroff -f; }

for n in s t; do
    mkdir -p /run/$n
    port=9101; [ $n = t ] && port=9102
    cat > /run/$n.toml <<EOT
[management]
api_token = "t"
admin_token = "t"
listen_addr = "127.0.0.1:$port"
data_dir = "/run/$n"
node_name = "node$n"
discovery_disabled = true

[fabric]
host_nqn = "nqn.2026-09.lo.storm:host-node$n"
EOT
    RUST_LOG=stormblock=info stormblock --config /run/$n.toml --data-dir /run/$n --no-iscsi > /run/$n.log 2>&1 &
done
for i in $(seq 1 100); do api -f $S/health >/dev/null 2>&1 && api -f $T/health >/dev/null 2>&1 && break; sleep 0.2; done
api -f -X POST $S/slabs -d '{"device_path":"/dev/vda","role":"data"}' >/dev/null || bail s-slab slab
api -f -X POST $T/slabs -d '{"device_path":"/dev/vdb","role":"data"}' >/dev/null || bail t-slab slab
api -f -X PUT $T/forge -d '{"listen_addr":"127.0.0.1:4420","nqn":"nqn.2026-10.ci:nodet"}' >/dev/null || bail t-forge forge

# S: the volume in use, over ublk, an ext2 filesystem on it with files.
v=$(api -X POST $S/volumes -d '{"name":"vm-disk","size":"192M"}' | idof)
dev=$(api -X POST $S/volumes/$v/attach -d '{"transport":"ublk"}' | hint)
[ -b "$dev" ] || bail s-attach "'$dev'"
mke2fs -q -b 4096 $dev >/dev/null 2>&1 || bail mkfs mke2fs
mount -t ext2 $dev /mnt || { dmesg | tail -5 | sed "s/^/LOG dmesg: /"; bail mount "mount $dev"; }
for i in $(seq 1 30); do dd if=/dev/urandom of=/mnt/before$i bs=1M count=1 2>/dev/null; done
sync
echo "GUEST S: volume $v on $dev, 30 MiB of files"

# T: a fresh volume of the same size, attached for S's host over NVMe/TCP.
t=$(api -X POST $T/volumes -d '{"name":"vm-disk-copy","size":"192M"}' | idof)
at=$(api -X POST $T/volumes/$t/attach -d '{"transport":"nvme_tcp","host_nqn":"nqn.2026-09.lo.storm:host-nodes","holder":"mirror"}')
nqn=$(echo "$at" | sed -n 's/.*"nqn":"\([^"]*\)".*/\1/p')
nsid=$(echo "$at" | sed -n 's/.*"nsid":\([0-9]*\).*/\1/p')
[ -n "$nqn" ] && [ -n "$nsid" ] || bail t-attach "$at"
uri="nvme-tcp://127.0.0.1:4420/$nqn?nsid=$nsid"
echo "GUEST T: volume $t as $uri"

# The writer: files appended and synced until told to stop.
( i=0
  while [ ! -e /tmp/stop ]; do
      i=$((i + 1))
      dd if=/dev/urandom of=/mnt/during$i bs=256k count=2 2>/dev/null || touch /tmp/werr
      [ $((i % 4)) = 0 ] && { sync || touch /tmp/werr; }
  done
  echo $i > /tmp/wcount ) &
w=$!
sleep 1
m=$(api -X POST $S/volumes/$v/mirror -d "{\"target\":\"$uri\"}")
echo "GUEST mirror: $m"
case "$m" in *'"state":"copying"'*|*'"state":"synced"'*) ;; *) bail mirror-start "$m" ;; esac
state=
for i in $(seq 1 600); do
    g=$(api $S/volumes/$v)
    state=$(echo "$g" | sed -n 's/.*"mirror":{[^}]*"state":"\([a-z]*\)".*/\1/p')
    [ "$state" = synced ] || [ "$state" = failed ] && break
    sleep 0.2
done
echo "GUEST mirror at $state: $(echo "$g" | sed -n 's/.*\("mirror":{[^}]*}\).*/\1/p')"
[ "$state" = synced ] && r synced PASS || r synced "FAIL ($state)"
sleep 2
touch /tmp/stop; wait $w
echo "GUEST writer: $(cat /tmp/wcount) files during the mirror"
[ -e /tmp/werr ] && r writer-clean FAIL || r writer-clean PASS
md5sum /mnt/* 2>/dev/null | sed 's#/mnt/##' | sort > /tmp/sums-s
umount /mnt || bail umount umount

# The switch-over.
d=$(api -X DELETE $S/volumes/$v/mirror)
echo "GUEST stop: $d"
case "$d" in *'"stopped":true'*'"flushed":true'*|*'"flushed":true'*'"stopped":true'*) r switch PASS ;; *) r switch "FAIL ($d)" ;; esac

# T's copy on T's own ublk device: the same bytes, and it mounts.
api -X DELETE $T/volumes/$t/attach >/dev/null
tdev=$(api -X POST $T/volumes/$t/attach -d '{"transport":"ublk"}' | hint)
[ -b "$tdev" ] || bail t-ublk "'$tdev'"
cmp $dev $tdev >/tmp/cmp 2>&1 && r bytes PASS || r bytes "FAIL ($(head -1 /tmp/cmp))"
mount -t ext2 -o ro $tdev /mnt2 || bail mount-t "mount $tdev"
md5sum /mnt2/* 2>/dev/null | sed 's#/mnt2/##' | sort > /tmp/sums-t
n=$(wc -l < /tmp/sums-s)
cmp /tmp/sums-s /tmp/sums-t >/dev/null && [ "$n" -gt 30 ] && r mounts PASS || r mounts "FAIL ($n files on S, $(wc -l < /tmp/sums-t) on T)"
umount /mnt2
grep -iE "panic|FATAL" /run/s.log /run/t.log | head -3 | sed 's/^/LOG /'
echo "GUEST done"
poweroff -f
EOF
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"
truncate -s 512M "$W/s.img" "$W/t.img"

say "boot $KERNEL in QEMU"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 900 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 2048 -smp 4 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -drive file="$W/s.img",format=raw,if=virtio,cache=none \
    -drive file="$W/t.img",format=raw,if=virtio,cache=none \
    -append "console=ttyS0 panic=-1 loglevel=4" > "$W/guest.log" 2>&1
G=$(tr -d '\r' < "$W/guest.log")
echo "$G" | grep -E '^(RESULT|GUEST|LOG)|panick'
for m in synced writer-clean switch bytes mounts; do
    echo "$G" | grep -q "^RESULT $m PASS" || fail "$m"
done
if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
