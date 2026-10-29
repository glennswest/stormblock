#!/usr/bin/env bash
# ci-finder-verify.sh — finding the appliance by multicast (#104) on a real
# kernel. A QEMU guest with a virtio NIC, the engine as root in it:
#
#   off     forge mode off: `stormblock discover` hears nothing (exit 1)
#   answer  forge mode on (PUT /api/v1/forge): the query on 239.255.42.99:7448
#           is answered with this engine's URL, the address the reply left
#           from (nothing advertised): http://10.0.2.15:9098
#   health  that URL answers /api/v1/health, as /init asks it
#
# Needs: cargo, qemu-system-x86_64, /boot/vmlinuz-$(uname -r) and its modules
# (virtio_net), a static busybox and curl.
# Run on a build VM:  sc-build 'bash ci-finder-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
CURL=$(command -v curl || true)
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-finder.XXXXXX")
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

say "guest initramfs: busybox, stormblock, curl, virtio_net"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,etc,lib/mods}
cp "$BUSYBOX" "$I/bin/busybox"
for a in sh mount insmod ip sleep cat echo ls grep sed awk cut tr kill seq mkdir rm poweroff dmesg head tail wc; do
    ln -sf busybox "$I/bin/$a"
done
for b in "$BIN" "$CURL"; do
    cp "$b" "$I/bin/$(basename "$b")"
    ldd "$b" | grep -o '/[^ ]*' | while read -r lib; do
        mkdir -p "$I$(dirname "$lib")"; cp -L "$lib" "$I$lib"
    done
done
: > "$I/lib/mods/order"
modprobe -S "$KVER" --show-depends virtio_net 2>/dev/null | awk '$1=="insmod"{print $2}' | while read -r ko; do
    base=$(basename "$ko" | sed 's/\.xz$//; s/\.zst$//')
    case "$ko" in
        *.xz) xz -dc "$ko" > "$I/lib/mods/$base" ;;
        *.zst) zstd -dcq "$ko" > "$I/lib/mods/$base" ;;
        *) cp "$ko" "$I/lib/mods/$base" ;;
    esac
    echo "$base" >> "$I/lib/mods/order"
done

cat > "$I/init" <<'GUEST'
#!/bin/sh
export PATH=/bin
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
mount -t tmpfs run /run; mount -t tmpfs tmp /tmp
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>&1 | sed "s/^/LOG insmod $m: /"; done
r() { echo "RESULT $1 $2"; }
ip link set lo up
for i in $(seq 1 50); do [ -e /sys/class/net/eth0 ] && break; sleep 0.1; done
ip link set eth0 up
ip addr add 10.0.2.15/24 dev eth0
ip route add default via 10.0.2.2
ip route add 224.0.0.0/4 dev eth0
echo "LOG eth0: $(ip -o link show eth0 | grep -o '<[^>]*>')"
mkdir -p /run/sb
cat > /run/sb.toml <<EOT
[management]
api_token = "t"
admin_token = "t"
listen_addr = "0.0.0.0:9098"
data_dir = "/run/sb"
node_name = "ci-forge"
discovery_disabled = true
EOT
RUST_LOG=stormblock=info stormblock --config /run/sb.toml --data-dir /run/sb --no-iscsi > /run/engine.log 2>&1 &
pid=$!
for i in $(seq 1 100); do curl -sf -m 2 http://127.0.0.1:9098/api/v1/health >/dev/null 2>&1 && break; sleep 0.2; done
out=$(stormblock discover --wait-ms 1500); rc=$?
echo "LOG forge off: rc $rc, '$out'"
[ $rc -eq 1 ] && [ -z "$out" ] && r off PASS || r off "FAIL (rc $rc, '$out')"
st=$(curl -s -m 10 -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer t' -H 'Content-Type: application/json' \
    -X PUT http://127.0.0.1:9098/api/v1/forge -d '{"listen_addr":"0.0.0.0:4420","nqn":"nqn.2026-08.lo.storm:ci-forge"}')
echo "LOG forge on: PUT $st"
out=$(stormblock discover --wait-ms 1500); rc=$?
echo "LOG forge on: rc $rc, '$out'"
[ $rc -eq 0 ] && [ "$out" = "http://10.0.2.15:9098" ] && r answer PASS || r answer "FAIL (rc $rc, '$out')"
[ -n "$out" ] && curl -sf -m 5 "$out/api/v1/health" > /dev/null && r health PASS || r health FAIL
grep "finder" /run/engine.log | head -3 | sed 's/^/LOG engine: /'
kill $pid
echo "GUEST done"
poweroff -f
GUEST
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"

say "boot $KERNEL in QEMU"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 300 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 1024 -smp 2 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -nic user,model=virtio-net-pci \
    -append "console=ttyS0 panic=-1 loglevel=4" > "$W/guest.log" 2>&1
G=$(tr -d '\r' < "$W/guest.log")
echo "$G" | grep -E '^(RESULT|LOG|GUEST)'
for m in off answer health; do
    echo "$G" | grep -q "^RESULT $m PASS" || fail "$m"
done
if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
