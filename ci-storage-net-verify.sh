#!/usr/bin/env bash
# ci-storage-net-verify.sh — the data path on its own network (#85).
# Unprivileged on dev: the engine runs as root inside a QEMU guest that has a
# storage network of its own — a VLAN (id 100) on a dummy link, 10.99.0.1/24,
# MTU 9000 — beside loopback, which plays the LAN (the control channel).
#
#   bound      with [storage] interface = the VLAN, the iSCSI target and the
#              shared NVMe/TCP target (forge, PUT /api/v1/forge with a
#              0.0.0.0:4420 listen) listen on 10.99.0.1 only: nothing on the
#              wildcard, nothing on loopback
#   advertised an NVMe/TCP attach reply names 10.99.0.1
#   health     data_path {state up, address 10.99.0.1, mtu 9000} and the
#              control interface; no mtu_mismatch
#   down       the VLAN's link down: health (on the control channel, still
#              answering) says data_path down, status ok; up again: up
#   mtu        a second engine told the network is 9000 on a 1500 link says
#              mtu_mismatch
#   no address a second engine whose storage interface does not exist: its
#              targets listen on loopback only (never the wildcard) and
#              health says missing
#
# Needs: cargo, qemu-system-x86_64, /boot/vmlinuz-$(uname -r) and its modules
# (dummy, 8021q), a static busybox and curl. Run on dev:
#   sc-build 'sh ci-storage-net-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
CURL=$(command -v curl || true)
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-storage-net.XXXXXX")
FAILS=0

say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
trap 'rm -rf "$W"' EXIT

for need in "$KERNEL" "$BUSYBOX" "$CURL"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
command -v qemu-system-x86_64 >/dev/null || { echo "SKIP: no qemu-system-x86_64"; exit 2; }

say "build"
cargo build --release --locked 2>&1 | tail -2
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/release/stormblock"
[ -x "$BIN" ] || { echo "FAIL: no binary"; exit 1; }

say "guest initramfs: busybox, stormblock, curl, dummy, 8021q"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,etc,lib/mods}
cp "$BUSYBOX" "$I/bin/busybox"
for a in sh mount insmod ip sleep cat echo ls grep dd poweroff head tail wc sed \
         awk cut tr kill seq mkdir rm timeout pidof truncate; do
    ln -sf busybox "$I/bin/$a"
done
for b in "$BIN" "$CURL"; do
    cp "$b" "$I/bin/$(basename "$b")"
    ldd "$b" | grep -o '/[^ ]*' | while read -r lib; do
        mkdir -p "$I$(dirname "$lib")"; cp -L "$lib" "$I$lib"
    done
done
: > "$I/lib/mods/order"
for m in dummy 8021q; do
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
( sleep ${WATCHDOG:-300}; echo "WATCHDOG fired"; sed 's/^/LOG engine: /' /run/e1.log | tail -20; poweroff -f ) &

# The storage network: a VLAN on a dummy link (the dummy alone when this
# busybox cannot make a VLAN).
[ -d /sys/class/net/dummy0 ] || ip link add dummy0 type dummy 2>&1 | sed 's/^/LOG /'
ip link set dummy0 mtu 9000 up
SIF=dummy0.100
if ! ip link add link dummy0 name $SIF type vlan id 100 2>/tmp/vlan.err; then
    echo "LOG no VLAN ($(cat /tmp/vlan.err)): the storage network is dummy0 itself"
    SIF=dummy0
fi
ip link set $SIF mtu 9000 up
ip addr add 10.99.0.1/24 dev $SIF
echo "GUEST storage network $SIF: $(ip -o addr show dev $SIF | awk '{print $4}' | head -1), mtu $(cat /sys/class/net/$SIF/mtu), $(cat /sys/class/net/$SIF/operstate)"

api() { port=$1; shift; curl -sf -m 60 -H 'Authorization: Bearer t' -H 'Content-Type: application/json' "$@" | sed "s/^//"; }
health() { curl -s -m 5 http://127.0.0.1:$1/api/v1/health; }
# Listening sockets as host:port in hex, /proc/net/tcp's form (state 0A).
listening() { awk '$4=="0A"{print $2}' /proc/net/tcp; }

engine() { # name port toml-extra
    mkdir -p /run/$1
    cat > /run/$1.toml <<EOT
[management]
api_token = "t"
admin_token = "t"
listen_addr = "127.0.0.1:$2"
data_dir = "/run/$1"
node_name = "$1"
discovery_disabled = true
interface = "lo"
$3
EOT
}

# --- engine 1: the storage network on $SIF ---
engine e1 9090 "
[storage]
interface = \"$SIF\"
mtu = 9000
wait_secs = 5
[iscsi]
listen_addr = \"0.0.0.0:3260\"
"
RUST_LOG=stormblock=info stormblock --config /run/e1.toml --data-dir /run/e1 > /run/e1.log 2>&1 &
e1=$!
for i in $(seq 1 100); do health 9090 >/dev/null 2>&1 && break; sleep 0.2; done
truncate -s 512M /run/d1.img
out=$(curl -s -m 60 -w ' HTTP%{http_code}' -H 'Authorization: Bearer t' -H 'Content-Type: application/json' \
    -X POST http://127.0.0.1:9090/api/v1/slabs -d '{"device_path":"/run/d1.img","role":"data"}')
case "$out" in *HTTP2??) ;; *) echo "LOG slab: $out" ;; esac
out=$(curl -s -m 60 -w ' HTTP%{http_code}' -H 'Authorization: Bearer t' -H 'Content-Type: application/json' \
    -X PUT http://127.0.0.1:9090/api/v1/forge -d '{"listen_addr":"0.0.0.0:4420","nqn":"nqn.2026-10.ci:forge"}')
echo "LOG forge: $out"
sleep 1
L=$(listening); echo "LOG listening: $(echo $L)"
# 10.99.0.1 = 0100630A; 4420 = 1144, 3260 = 0CBC
echo "$L" | grep -q '^0100630A:1144$' && r nvme-bound PASS || r nvme-bound "FAIL (not on 10.99.0.1:4420)"
echo "$L" | grep -q '^0100630A:0CBC$' && r iscsi-bound PASS || r iscsi-bound "FAIL (not on 10.99.0.1:3260)"
echo "$L" | grep -Eq '^(00000000|0100007F):(1144|0CBC)$' && r not-elsewhere "FAIL (a data port on the wildcard or loopback)" || r not-elsewhere PASS
echo "$L" | grep -q '^0100007F:2382$' && r control-bound PASS || r control-bound "FAIL (no API on 127.0.0.1:9090)"

id=$(api 9090 -X POST http://127.0.0.1:9090/api/v1/volumes -d '{"name":"v1","size":"64M"}' \
    | sed -n 's/.*"id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
att=$(api 9090 -X POST http://127.0.0.1:9090/api/v1/volumes/$id/attach \
    -d '{"transport":"nvme_tcp","host_nqn":"nqn.2026-10.ci:host"}')
echo "LOG attach: $att"
case "$att" in *'"10.99.0.1"'*) r advertised PASS ;; *) r advertised "FAIL (attach does not name 10.99.0.1)" ;; esac
case "$att" in *'127.0.0.1'*) r advertised-not-lan "FAIL (names loopback)" ;; *) r advertised-not-lan PASS ;; esac

h=$(health 9090); echo "LOG health: $h"
case "$h" in *'"data_path":{'*"\"interface\":\"$SIF\""*'"state":"up"'*'"address":"10.99.0.1"'*'"mtu":9000'*) r health-up PASS ;;
    *) r health-up "FAIL" ;; esac
case "$h" in *mtu_mismatch*) r health-mtu "FAIL (mismatch reported)" ;; *) r health-mtu PASS ;; esac
case "$h" in *'"control":{"interface":"lo"'*) r health-control PASS ;; *) r health-control FAIL ;; esac

ip link set $SIF down
sleep 1
h=$(health 9090); echo "LOG health, link down: $h"
case "$h" in *'"status":"ok"'*'"data_path":{'*'"state":"down"'*) r data-down PASS ;; *) r data-down FAIL ;; esac
ip link set $SIF up
sleep 1
h=$(health 9090)
case "$h" in *'"data_path":{'*'"state":"up"'*) r data-back PASS ;; *) r data-back "FAIL ($h)" ;; esac
grep -E "data path on|initiators are told" /run/e1.log | sed 's/^/LOG e1: /' | head -3

# --- engine 2: told the network is 9000 on a 1500 link ---
ip link add dummy1 type dummy 2>/dev/null || true
ip link set dummy1 mtu 1500 up; ip addr add 10.98.0.1/24 dev dummy1
engine e2 9091 "
[storage]
interface = \"dummy1\"
mtu = 9000
wait_secs = 2
"
RUST_LOG=stormblock=info stormblock --config /run/e2.toml --data-dir /run/e2 --no-iscsi > /run/e2.log 2>&1 &
e2=$!
for i in $(seq 1 100); do health 9091 >/dev/null 2>&1 && break; sleep 0.2; done
h=$(health 9091); echo "LOG e2 health: $h"
case "$h" in *'"mtu":1500'*'"expected_mtu":9000'*'"mtu_mismatch":true'*) r mtu-mismatch PASS ;; *) r mtu-mismatch FAIL ;; esac
grep -q "large transfers will stall" /run/e2.log && r mtu-logged PASS || r mtu-logged FAIL
kill -TERM $e2; wait $e2

# --- engine 3: a storage interface that does not exist ---
engine e3 9092 "
[storage]
interface = \"nosuch0\"
wait_secs = 2
"
RUST_LOG=stormblock=info stormblock --config /run/e3.toml --data-dir /run/e3 --no-iscsi > /run/e3.log 2>&1 &
e3=$!
for i in $(seq 1 100); do health 9092 >/dev/null 2>&1 && break; sleep 0.2; done
api 9092 -X PUT http://127.0.0.1:9092/api/v1/forge -d '{"listen_addr":"0.0.0.0:4421","nqn":"nqn.2026-10.ci:f3"}' >/dev/null
sleep 1
L=$(listening)
# 4421 = 1145
echo "$L" | grep -q '^0100007F:1145$' && r noaddr-loopback PASS || r noaddr-loopback "FAIL ($(echo $L))"
echo "$L" | grep -q '^00000000:1145$' && r noaddr-not-wildcard "FAIL (on the wildcard)" || r noaddr-not-wildcard PASS
h=$(health 9092); echo "LOG e3 health: $h"
case "$h" in *'"status":"ok"'*'"state":"missing"'*) r noaddr-health PASS ;; *) r noaddr-health FAIL ;; esac
grep -q "listen on 127.0.0.1 only" /run/e3.log && r noaddr-logged PASS || r noaddr-logged FAIL
kill -TERM $e3; wait $e3

kill -0 $e1 2>/dev/null && r engine-alive PASS || r engine-alive FAIL
kill -TERM $e1; wait $e1
grep -hE "ERROR|panicked" /run/e1.log | sed 's/^/LOG e1: /' | head -5
echo "GUEST done"
poweroff -f
EOF
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"

say "boot $KERNEL in QEMU"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 600 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 2048 -smp 4 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 panic=-1 loglevel=4" -nic none > "$W/guest.log" 2>&1
tr -d '\r' < "$W/guest.log" | grep -E '^(RESULT|GUEST|LOG|WATCHDOG)|panick'

for m in nvme-bound iscsi-bound not-elsewhere control-bound advertised advertised-not-lan \
         health-up health-mtu health-control data-down data-back \
         mtu-mismatch mtu-logged noaddr-loopback noaddr-not-wildcard noaddr-health noaddr-logged engine-alive; do
    tr -d '\r' < "$W/guest.log" | grep -q "^RESULT $m PASS" || fail "$m"
done
if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
