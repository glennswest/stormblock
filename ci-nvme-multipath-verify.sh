#!/usr/bin/env bash
# ci-nvme-multipath-verify.sh — NVMe/TCP over two NICs on one subnet (#90).
# Unprivileged on dev: two QEMU guests, A (the target) and B (the engine's
# initiator), joined by two separate links (QEMU socket netdevs), each NIC on
# the same /24 with arp_ignore=1 / arp_announce=2, as the issue's node has.
#
#   A  the engine with [storage] interfaces = ["eth0", "eth1"]: forge's
#      NVMe/TCP target on 0.0.0.0:4420 listens on 10.20.0.1 and 10.20.0.11
#   B  attaches a volume for its host NQN through A's API: the reply's
#      `addresses` names both; then, with STORMBLOCK_STORAGE_INTERFACE=eth0,eth1,
#      formats a slab on the two-path URI and imports a 48 MB golden into it
#      (`stormblock clone`): tx_bytes on BOTH of B's NICs, and rx_bytes on
#      both of A's, rise by megabytes — the check the owner asked for on #90
#      (per-interface counters, not the total)
#   failover  eth1 down on B: a second import completes over eth0 alone
#
# Needs: cargo, qemu-system-x86_64, /boot/vmlinuz-$(uname -r) and its
# virtio_net module, a static busybox and curl. Run on dev:
#   sc-build 'sh ci-nvme-multipath-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
CURL=$(command -v curl || true)
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-mp.XXXXXX")
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
for f in /proc/sys/net/ipv4/conf/all /proc/sys/net/ipv4/conf/eth0 /proc/sys/net/ipv4/conf/eth1; do
    echo 1 > $f/arp_ignore; echo 2 > $f/arp_announce
done
if [ "$ROLE" = a ]; then A0=10.20.0.1; A1=10.20.0.11; else A0=10.20.0.2; A1=10.20.0.12; fi
ip addr add $A0/24 dev eth0; ip addr add $A1/24 dev eth1
ip link set eth0 up; ip link set eth1 up
echo "GUEST $ROLE eth0 $A0 eth1 $A1 ($(cat /sys/class/net/eth0/address), $(cat /sys/class/net/eth1/address))"
ctr() { cat /sys/class/net/$1/statistics/$2; }
api() { curl -sf -m 60 -H 'Authorization: Bearer t' -H 'Content-Type: application/json' "$@"; }

if [ "$ROLE" = a ]; then
    mkdir -p /run/sb
    cat > /run/sb.toml <<EOT
[management]
api_token = "t"
admin_token = "t"
listen_addr = "0.0.0.0:9090"
data_dir = "/run/sb"
node_name = "a"
discovery_disabled = true
[storage]
interfaces = ["eth0", "eth1"]
wait_secs = 10
EOT
    RUST_LOG=stormblock=info stormblock --config /run/sb.toml --data-dir /run/sb --no-iscsi > /run/e.log 2>&1 &
    for i in $(seq 1 100); do curl -s -m 2 http://127.0.0.1:9090/api/v1/health >/dev/null && break; sleep 0.2; done
    truncate -s 1G /run/d.img
    api -X POST http://127.0.0.1:9090/api/v1/slabs -d '{"device_path":"/run/d.img","role":"data"}' >/dev/null || echo "LOG slab failed"
    api -X PUT http://127.0.0.1:9090/api/v1/forge -d '{"listen_addr":"0.0.0.0:4420","nqn":"nqn.2026-10.ci:a"}' >/dev/null || echo "LOG forge failed"
    sleep 1
    echo "LOG A listening: $(awk '$4=="0A"{print $2}' /proc/net/tcp | tr '\n' ' ')"
    r0=$(ctr eth0 rx_bytes); r1=$(ctr eth1 rx_bytes)
    echo "GUEST A ready"
    # Until B names a volume "stop".
    for i in $(seq 1 450); do
        api http://127.0.0.1:9090/api/v1/volumes 2>/dev/null | grep -q '"name":"stop"' && break
        sleep 1
    done
    echo "A-COUNTERS eth0_rx=$(( $(ctr eth0 rx_bytes) - r0 )) eth1_rx=$(( $(ctr eth1 rx_bytes) - r1 ))"
    echo "LOG A eth1 tx $(ctr eth1 tx_bytes) rx $(ctr eth1 rx_bytes); neigh: $(ip neigh | tr '\n' ';')"
    echo "LOG A rp_filter all=$(cat /proc/sys/net/ipv4/conf/all/rp_filter) eth0=$(cat /proc/sys/net/ipv4/conf/eth0/rp_filter) eth1=$(cat /proc/sys/net/ipv4/conf/eth1/rp_filter); routes: $(ip route | tr '\n' ';')"
    grep -E "data path on|also listening|listening on" /run/e.log | sed 's/^/LOG A: /' | head -6
    poweroff -f
fi

# --- B ---
for i in $(seq 1 150); do curl -s -m 2 http://10.20.0.1:9090/api/v1/health >/dev/null && break; sleep 1; done
id=$(api -X POST http://10.20.0.1:9090/api/v1/volumes -d '{"name":"mp","size":"768M"}' | sed -n 's/.*"id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
att=$(api -X POST http://10.20.0.1:9090/api/v1/volumes/$id/attach -d '{"transport":"nvme_tcp","host_nqn":"nqn.2026-10.ci:b"}')
echo "LOG attach: $att"
case "$att" in *'"10.20.0.1"'*'"10.20.0.11"'*|*'"10.20.0.11"'*'"10.20.0.1"'*) r advertised PASS ;; *) r advertised FAIL ;; esac
nqn=$(echo "$att" | sed -n 's/.*"nqn":"\([^"]*\)".*/\1/p')
nsid=$(echo "$att" | sed -n 's/.*"nsid":\([0-9]*\).*/\1/p')
URI="nvme-tcp://10.20.0.1:4420,10.20.0.11:4420/$nqn?nsid=$nsid&hostnqn=nqn.2026-10.ci:b"
echo "LOG uri: $URI"
export STORMBLOCK_STORAGE_INTERFACE=eth0,eth1 RUST_LOG=stormblock=info
sb() { stormblock "$@" > /tmp/out 2>&1; rc=$?; grep -E "Error|cannot bind|path .* (down|failed)|imported|cloned" /tmp/out | sed 's/^/LOG   /' | head -8; return $rc; }

mkdir -p /tmp/r1; dd if=/dev/urandom of=/tmp/r1/big bs=1M count=48 2>/dev/null
tar -cf /tmp/r1.tar -C /tmp/r1 .
sb golden --out /tmp/one.img --size 96M --tar /tmp/r1.tar || r golden FAIL
sb slab format "$URI" --role data && r format PASS || r format FAIL
t0=$(ctr eth0 tx_bytes); t1=$(ctr eth1 tx_bytes)
sb clone --slab "$URI" --golden /tmp/one.img --name w1 && r import PASS || r import FAIL
d0=$(( $(ctr eth0 tx_bytes) - t0 )); d1=$(( $(ctr eth1 tx_bytes) - t1 ))
echo "B-COUNTERS eth0_tx=$d0 eth1_tx=$d1"
echo "LOG B eth1 rx $(ctr eth1 rx_bytes); neigh: $(ip neigh | tr '\n' ';'); rp_filter all=$(cat /proc/sys/net/ipv4/conf/all/rp_filter) eth1=$(cat /proc/sys/net/ipv4/conf/eth1/rp_filter)"
[ "$d0" -gt 8000000 ] && [ "$d1" -gt 8000000 ] && r both-nics PASS || r both-nics "FAIL (eth0 $d0, eth1 $d1)"
grep -q "cannot bind" /tmp/out && r bound-to-device "FAIL (not bound)" || r bound-to-device PASS

# failover: B's eth1 down, a second import over eth0 alone
ip link set eth1 down
mkdir -p /tmp/r2; dd if=/dev/urandom of=/tmp/r2/big bs=1M count=24 2>/dev/null
tar -cf /tmp/r2.tar -C /tmp/r2 .
sb golden --out /tmp/two.img --size 64M --tar /tmp/r2.tar
t0=$(ctr eth0 tx_bytes); t1=$(ctr eth1 tx_bytes)
sb clone --slab "$URI" --golden /tmp/two.img --name w2 && r failover PASS || r failover FAIL
echo "B-COUNTERS-FAILOVER eth0_tx=$(( $(ctr eth0 tx_bytes) - t0 )) eth1_tx=$(( $(ctr eth1 tx_bytes) - t1 ))"
ip link set eth1 up

api -X POST http://10.20.0.1:9090/api/v1/volumes -d '{"name":"stop","size":"1M"}' >/dev/null
echo "GUEST B done"
poweroff -f
EOF
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"

ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
P0=$((20000 + RANDOM % 20000)); P1=$((P0 + 1))
guest() { # role netdev-kind
    timeout 600 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 1536 -smp 2 \
        -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
        -append "console=ttyS0 panic=-1 loglevel=4 role=$1" \
        -netdev socket,id=n0,$2=127.0.0.1:$P0 -device virtio-net-pci,netdev=n0,mac=52:54:00:20:$3:00,addr=0x10 \
        -netdev socket,id=n1,$2=127.0.0.1:$P1 -device virtio-net-pci,netdev=n1,mac=52:54:00:20:$3:01,addr=0x11
}
say "boot A (target) and B (initiator), two links"
guest a listen 0a > "$W/a.log" 2>&1 &
QA=$!
sleep 3
guest b connect 0b > "$W/b.log" 2>&1
wait $QA
for g in a b; do tr -d '\r' < "$W/$g.log" | grep -E '^(RESULT|GUEST|LOG|WATCHDOG|A-COUNTERS|B-COUNTERS)|panick' | sed "s/^/[$g] /"; done

for m in advertised format import both-nics bound-to-device failover; do
    tr -d '\r' < "$W/b.log" | grep -q "^RESULT $m PASS" || fail "$m"
done
A=$(tr -d '\r' < "$W/a.log" | sed -n 's/^A-COUNTERS eth0_rx=\([0-9]*\) eth1_rx=\([0-9]*\).*/\1 \2/p')
set -- $A
if [ "${1:-0}" -gt 8000000 ] && [ "${2:-0}" -gt 8000000 ]; then echo "A received on both NICs: $1 / $2"; else fail "A rx on both NICs ($A)"; fi
if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
