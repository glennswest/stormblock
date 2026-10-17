#!/usr/bin/env bash
# ci-flow-nics-verify.sh — NVMe/TCP by every NIC that is up (#450).
# Unprivileged on a build VM: two QEMU guests on one L2 segment (B's two NICs
# and its uplink to A are ports of one QEMU hub: a switch), every address on
# one /24 with arp_ignore=1 / arp_announce=2.
#
#   A  forge's target with ONE NIC and ONE address, 10.20.0.1:4420, as forge
#   B  two NICs, 10.20.0.2 (eth0) and 10.20.0.12 (eth1), and NO storage
#      configuration: formats a slab on A's single-address URI and imports a
#      48 MB golden into it (`stormblock clone`). tx_bytes on BOTH of B's NICs
#      rise by megabytes: a path by each NIC, chosen by the engine alone
#   skip      STORMBLOCK_PATH_SKIP=eth1: a second import leaves by eth0 only
#   failover  eth1 down on B mid-import: the import completes over eth0
#
# Needs: cargo, qemu-system-x86_64, /boot/vmlinuz-$(uname -r) and its
# virtio_net module, a static busybox and curl. Run through sc-build:
#   sc-build 'sh ci-flow-nics-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
CURL=$(command -v curl || true)
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-nics.XXXXXX")
FAILS=0

say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
trap 'rm -rf "$W"; [ -n "${QA:-}" ] && kill $QA 2>/dev/null' EXIT

for need in "$KERNEL" "$BUSYBOX" "$CURL"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
command -v qemu-system-x86_64 >/dev/null || { echo "SKIP: no qemu-system-x86_64"; exit 2; }

say "build"
cargo build --release --locked 2>&1 | tail -2
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/release/stormblock"
[ -x "$BIN" ] || { echo "FAIL: no binary"; exit 1; }

# One initramfs for both guests; the kernel command line says which it is.
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,etc,lib/mods}
cp "$BUSYBOX" "$I/bin/busybox"
for a in sh mount insmod ip sleep cat echo ls grep dd poweroff head tail sed awk cut tr kill \
         seq mkdir rm truncate tar sync timeout expr; do
    ln -sf busybox "$I/bin/$a"
done
for b in "$BIN" "$CURL"; do
    cp "$b" "$I/bin/$(basename "$b")"
    ldd "$b" | grep -o '/[^ ]*' | while read -r lib; do
        mkdir -p "$I$(dirname "$lib")"; cp -L "$lib" "$I$lib"
    done
done
: > "$I/lib/mods/order"
modprobe -S "$KVER" --show-depends virtio_net 2>/dev/null | awk '$1=="insmod"{print $2}' \
| awk '!seen[$0]++' | while read -r ko; do
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
ROLE=$(sed -n 's/.*role=\([ab]\).*/\1/p' /proc/cmdline)
r() { echo "RESULT $1 $2"; }
( sleep 500; echo "WATCHDOG fired ($ROLE)"; poweroff -f ) &
ip link set lo up
echo 1 > /proc/sys/net/ipv4/conf/all/arp_ignore; echo 2 > /proc/sys/net/ipv4/conf/all/arp_announce
ctr() { cat /sys/class/net/$1/statistics/$2; }
api() { curl -sf -m 60 -H 'Authorization: Bearer t' -H 'Content-Type: application/json' "$@"; }

if [ "$ROLE" = a ]; then
    ip addr add 10.20.0.1/24 dev eth0; ip link set eth0 up
    echo "GUEST A eth0 10.20.0.1 (one NIC)"
    mkdir -p /run/sb
    cat > /run/sb.toml <<EOT
[management]
api_token = "t"
admin_token = "t"
listen_addr = "0.0.0.0:9090"
data_dir = "/run/sb"
node_name = "a"
discovery_disabled = true
EOT
    RUST_LOG=stormblock=info stormblock --config /run/sb.toml --data-dir /run/sb --no-iscsi > /run/e.log 2>&1 &
    for i in $(seq 1 100); do curl -s -m 2 http://127.0.0.1:9090/api/v1/health >/dev/null && break; sleep 0.2; done
    truncate -s 1G /run/d.img
    api -X POST http://127.0.0.1:9090/api/v1/slabs -d '{"device_path":"/run/d.img","role":"data"}' >/dev/null || echo "LOG slab failed"
    api -X PUT http://127.0.0.1:9090/api/v1/forge -d '{"listen_addr":"10.20.0.1:4420","nqn":"nqn.2026-10.ci:a"}' >/dev/null || echo "LOG forge failed"
    echo "GUEST A ready"
    for i in $(seq 1 450); do
        api http://127.0.0.1:9090/api/v1/volumes 2>/dev/null | grep -q '"name":"stop"' && break
        sleep 1
    done
    poweroff -f
fi

# --- B: two NICs, nothing configured ---
ip addr add 10.20.0.2/24 dev eth0; ip addr add 10.20.0.12/24 dev eth1
ip link set eth0 up; ip link set eth1 up
echo 1 > /proc/sys/net/ipv4/conf/eth0/arp_ignore; echo 1 > /proc/sys/net/ipv4/conf/eth1/arp_ignore
echo "GUEST B eth0 10.20.0.2 eth1 10.20.0.12"
for i in $(seq 1 150); do curl -s -m 2 http://10.20.0.1:9090/api/v1/health >/dev/null && break; sleep 1; done
id=$(api -X POST http://10.20.0.1:9090/api/v1/volumes -d '{"name":"nics","size":"768M"}' | sed -n 's/.*"id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
att=$(api -X POST http://10.20.0.1:9090/api/v1/volumes/$id/attach -d '{"transport":"nvme_tcp","host_nqn":"nqn.2026-10.ci:b"}')
nqn=$(echo "$att" | sed -n 's/.*"nqn":"\([^"]*\)".*/\1/p')
nsid=$(echo "$att" | sed -n 's/.*"nsid":\([0-9]*\).*/\1/p')
URI="nvme-tcp://10.20.0.1:4420/$nqn?nsid=$nsid&hostnqn=nqn.2026-10.ci:b"
echo "LOG uri (one address): $URI"
case "$URI" in *,*) r one-address "FAIL ($URI)" ;; *) r one-address PASS ;; esac
export RUST_LOG=stormblock=info
unset STORMBLOCK_STORAGE_INTERFACE
sb() { stormblock "$@" > /tmp/out 2>&1; rc=$?; grep -E "Error|cannot bind|paths|path .* (down|failed)|imported|cloned" /tmp/out | sed 's/^/LOG   /' | head -8; return $rc; }
img() { mkdir -p /tmp/$1; dd if=/dev/urandom of=/tmp/$1/big bs=1M count=$2 2>/dev/null; tar -cf /tmp/$1.tar -C /tmp/$1 .; sb golden --out /tmp/$1.img --size $3 --tar /tmp/$1.tar; }

img g1 48 96M
sb slab format "$URI" --role data && r format PASS || r format FAIL
t0=$(ctr eth0 tx_bytes); t1=$(ctr eth1 tx_bytes)
sb clone --slab "$URI" --golden /tmp/g1.img --name w1 && r import PASS || r import FAIL
d0=$(( $(ctr eth0 tx_bytes) - t0 )); d1=$(( $(ctr eth1 tx_bytes) - t1 ))
echo "B-COUNTERS eth0_tx=$d0 eth1_tx=$d1"
grep -q "2 paths" /tmp/out && r two-paths PASS || r two-paths "FAIL (no '2 paths' line)"
[ "$d0" -gt 8000000 ] && [ "$d1" -gt 8000000 ] && r both-nics PASS || r both-nics "FAIL (eth0 $d0, eth1 $d1)"
grep -q "cannot bind" /tmp/out && r bound-to-device "FAIL (not bound)" || r bound-to-device PASS

# The opt-out: eth1 named, the import leaves by eth0 alone.
img g2 24 64M
t0=$(ctr eth0 tx_bytes); t1=$(ctr eth1 tx_bytes)
STORMBLOCK_PATH_SKIP=eth1 sb clone --slab "$URI" --golden /tmp/g2.img --name w2 || r skip "FAIL (import)"
d0=$(( $(ctr eth0 tx_bytes) - t0 )); d1=$(( $(ctr eth1 tx_bytes) - t1 ))
echo "B-COUNTERS-SKIP eth0_tx=$d0 eth1_tx=$d1"
[ "$d0" -gt 8000000 ] && [ "$d1" -lt 1000000 ] && r skip PASS || r skip "FAIL (eth0 $d0, eth1 $d1)"

# eth1's link goes down mid-import: the import completes over eth0.
img g3 48 96M
( sleep 2; ip link set eth1 down; echo "LOG eth1 down" ) &
t0=$(ctr eth0 tx_bytes)
sb clone --slab "$URI" --golden /tmp/g3.img --name w3 && r failover PASS || r failover FAIL
echo "B-COUNTERS-FAILOVER eth0_tx=$(( $(ctr eth0 tx_bytes) - t0 ))"
ip link set eth1 up

api -X POST http://10.20.0.1:9090/api/v1/volumes -d '{"name":"stop","size":"1M"}' >/dev/null
echo "GUEST B done"
poweroff -f
EOF
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"

ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
P0=$((20000 + RANDOM % 20000))
run() {
    timeout 600 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 1536 -smp 2 \
        -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" "$@"
}
say "boot A (one NIC) and B (two NICs on one hub with the uplink to A)"
run -append "console=ttyS0 panic=-1 loglevel=4 role=a" \
    -netdev socket,id=n0,listen=127.0.0.1:$P0 -device virtio-net-pci,netdev=n0,mac=52:54:00:21:0a:00 \
    > "$W/a.log" 2>&1 &
QA=$!
sleep 3
run -append "console=ttyS0 panic=-1 loglevel=4 role=b" \
    -netdev socket,id=up,connect=127.0.0.1:$P0 \
    -netdev hubport,id=hup,hubid=0,netdev=up \
    -netdev hubport,id=h0,hubid=0 -device virtio-net-pci,netdev=h0,mac=52:54:00:21:0b:00,addr=0x10 \
    -netdev hubport,id=h1,hubid=0 -device virtio-net-pci,netdev=h1,mac=52:54:00:21:0b:01,addr=0x11 \
    > "$W/b.log" 2>&1
wait $QA
for g in a b; do tr -d '\r' < "$W/$g.log" | grep -E '^(RESULT|GUEST|LOG|WATCHDOG|B-COUNTERS)|panick|qemu' | sed "s/^/[$g] /"; done

for m in one-address format import two-paths both-nics bound-to-device skip failover; do
    tr -d '\r' < "$W/b.log" | grep -q "^RESULT $m PASS" || fail "$m"
done
if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
