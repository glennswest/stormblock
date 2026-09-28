#!/usr/bin/env bash
# ci-nvme-hosts-verify.sh — who may connect over NVMe/TCP, checked with the
# Linux kernel as the initiator (#210). Unprivileged: the kernel is a guest.
#
# The engine runs here, on dev, with volumes attached for named hosts. The
# host's own kernel then boots in QEMU (KVM when /dev/kvm is usable, TCG
# otherwise) from an initramfs holding busybox, nvme-cli and the nvme-tcp /
# nvme-auth modules, and runs what an operator on pve would run:
#
#   nvme discover  as a host given nothing        → no subsystem listed
#   nvme connect   to the shared subsystem        → refused
#   nvme discover  as H1                          → exactly H1's subsystem
#   nvme connect   to H1's subsystem as H2        → refused
#   nvme connect   to H1's subsystem as H1        → its two namespaces: a clone
#                  (read/write round trip) and a golden (read-only block device,
#                  a write fails)
#   nvme connect   to H2's subsystem as H2 without its secret, with another
#                  host's secret                  → refused
#                  with its own --dhchap-secret   → connected, authenticated
#
# Needs: cargo, qemu-system-x86_64, /boot/vmlinuz-$(uname -r) and its modules,
# a static busybox, nvme-cli (copied into the guest with its libraries), curl.
# Run on dev through sc-build:  sc-build 'sh ci-nvme-hosts-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
NVME=$(command -v nvme || true)
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-nvme-hosts.XXXXXX")
PORT=$((20000 + RANDOM % 20000))
MGMT=$((PORT + 1))
TOKEN=$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')
SHARED="nqn.2026-09.lo.storm:ci-shared"
H1="nqn.2014-08.org.nvmexpress:uuid:11111111-2222-3333-4444-555555555501"
H2="nqn.2014-08.org.nvmexpress:uuid:11111111-2222-3333-4444-555555555502"
H3="nqn.2014-08.org.nvmexpress:uuid:11111111-2222-3333-4444-555555555503"
H4="nqn.2014-08.org.nvmexpress:uuid:11111111-2222-3333-4444-555555555504"
SB_PID=""
FAILS=0

say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
cleanup() {
    [ -n "$SB_PID" ] && kill "$SB_PID" 2>/dev/null && wait "$SB_PID" 2>/dev/null
    rm -rf "$W"
}
trap cleanup EXIT

for need in "$KERNEL" "$BUSYBOX" "$NVME"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
command -v qemu-system-x86_64 >/dev/null || { echo "SKIP: no qemu-system-x86_64"; exit 2; }

say "build"
cargo build --release --locked 2>&1 | tail -2
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/release/stormblock"
[ -x "$BIN" ] || { echo "FAIL: no binary"; exit 1; }

say "engine on 127.0.0.1:$PORT (NVMe/TCP) and :$MGMT (API)"
mkdir -p "$W/data"
truncate -s 512M "$W/d1.img" "$W/d2.img"
cat > "$W/stormblock.toml" <<EOF
[management]
api_token = "$TOKEN"
listen_addr = "127.0.0.1:$MGMT"
data_dir = "$W/data"
node_name = "ci-nvme-hosts"
advertised_addr = "10.0.2.2"
discovery_disabled = true

[nvmeof]
export_drives = false
EOF
RUST_LOG=stormblock=info "$BIN" --config "$W/stormblock.toml" \
    --device "$W/d1.img" --device "$W/d2.img" \
    --data-dir "$W/data" --no-iscsi \
    --nvmeof-addr "127.0.0.1:$PORT" --nvmeof-nqn "$SHARED" \
    > "$W/engine.log" 2>&1 &
SB_PID=$!
api() { curl -sf -m 20 -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "$@"; }
for _ in $(seq 1 100); do
    api "http://127.0.0.1:$MGMT/api/v1/health" >/dev/null 2>&1 && break
    sleep 0.2
done
api "http://127.0.0.1:$MGMT/api/v1/health" >/dev/null || { echo "FAIL: engine did not start"; tail -30 "$W/engine.log"; exit 1; }

for d in d1 d2; do
    r=$(curl -s -m 180 -w ' HTTP%{http_code}' -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
        -X POST "http://127.0.0.1:$MGMT/api/v1/slabs" -d "{\"device_path\":\"$W/$d.img\",\"role\":\"data\"}")
    case "$r" in *HTTP2??) ;; *) fail "slab on $d: $r"; tail -5 "$W/engine.log" ;; esac
done

mkvol() {
    local r
    r=$(curl -s -m 20 -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
        -X POST "http://127.0.0.1:$MGMT/api/v1/volumes" -d "{\"name\":\"$1\",\"size\":\"64M\"}")
    echo "$r" | grep -o '"id":"[0-9a-f-]*"' | head -1 | cut -d'"' -f4
    echo "$r" | grep -q '"id":"' || echo "create $1: $r" >&2
}
field() { sed -n "s/.*\"$1\":\"\{0,1\}\([^\",}]*\)\"\{0,1\}.*/\1/p" | head -1; }

A=$(mkvol ci-clone-a)
B=$(mkvol ci-clone-b)
C=$(mkvol ci-clone-c)
G=$(mkvol ci-golden)
[ -n "$A" ] && [ -n "$B" ] && [ -n "$C" ] && [ -n "$G" ] || { echo "FAIL: volumes"; tail -20 "$W/engine.log"; exit 1; }
api -X POST "http://127.0.0.1:$MGMT/api/v1/volumes/$G/seal" -d '{"force":true}' >/dev/null \
    || fail "seal the golden"

# Refused before anything reaches a host: no host named; a golden unnamed.
code=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    -X POST "http://127.0.0.1:$MGMT/api/v1/volumes/$A/attach" -d '{"transport":"nvme-tcp"}')
[ "$code" = 400 ] && echo "ok: an attach naming no host is refused (400)" || fail "attach without host_nqn gave $code"

RA=$(api -X POST "http://127.0.0.1:$MGMT/api/v1/volumes/$A/attach" \
    -d "{\"transport\":\"nvme-tcp\",\"host_nqn\":\"$H1\"}")
SUB1=$(echo "$RA" | field nqn); NS_A=$(echo "$RA" | field nsid)
RG=$(api -X POST "http://127.0.0.1:$MGMT/api/v1/exports" \
    -d "{\"volume_id\":\"$G\",\"protocol\":\"nvmeof\",\"host_nqn\":\"$H1\"}")
NS_G=$(echo "$RG" | field nsid)
[ "$(echo "$RG" | field nqn)" = "$SUB1" ] || fail "the golden did not land in H1's subsystem: $RG"
RB=$(api -X POST "http://127.0.0.1:$MGMT/api/v1/volumes/$B/attach" \
    -d "{\"transport\":\"nvme-tcp\",\"host_nqn\":\"$H2\",\"dhchap\":true}")
SUB2=$(echo "$RB" | field nqn); S2=$(echo "$RB" | field dhchap_secret)
RC=$(api -X POST "http://127.0.0.1:$MGMT/api/v1/volumes/$C/attach" \
    -d "{\"transport\":\"nvme-tcp\",\"host_nqn\":\"$H4\",\"dhchap\":true}")
S4=$(echo "$RC" | field dhchap_secret)
echo "H1 subsystem $SUB1: clone nsid $NS_A, golden nsid $NS_G"
echo "H2 subsystem $SUB2: secret ${S2:0:14}…"
[ -n "$SUB1" ] && [ -n "$SUB2" ] && [ -n "$S2" ] && [ -n "$S4" ] || { echo "FAIL: attach replies: $RA $RB $RC"; exit 1; }

say "guest initramfs: busybox, nvme-cli, nvme-tcp"
I="$W/initrd"
mkdir -p "$I"/{bin,sbin,dev,proc,sys,run,tmp,etc/nvme,lib/mods}
cp "$BUSYBOX" "$I/bin/busybox"
for a in sh mount insmod ip sleep cat echo ls grep dd cmp poweroff dmesg head tail wc sed basename cut tr sort; do
    ln -sf busybox "$I/bin/$a"
done
cp "$NVME" "$I/bin/nvme"
ldd "$NVME" | grep -o '/[^ ]*' | while read -r lib; do
    mkdir -p "$I$(dirname "$lib")"; cp -L "$lib" "$I$lib"
done
: > "$I/lib/mods/order"
for m in virtio_net nvme-tcp; do
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
cat > "$I/env" <<EOF
PORT=$PORT
SHARED=$SHARED
SUB1=$SUB1
SUB2=$SUB2
NS_A=$NS_A
NS_G=$NS_G
H1=$H1
H2=$H2
H3=$H3
S2='$S2'
S4='$S4'
EOF
cat > "$I/init" <<'EOF'
#!/bin/sh
export PATH=/bin
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
. /env
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>/dev/null; done
ip link set lo up; ip link set eth0 up
ip addr add 10.0.2.15/24 dev eth0; ip route add default via 10.0.2.2
T="-t tcp -a 10.0.2.2 -s $PORT"
r() { echo "RESULT $1 $2"; }
echo "GUEST kernel $(cat /proc/sys/kernel/osrelease)"
[ -e /dev/nvme-fabrics ] || { r modules FAIL; poweroff -f; }

# 1. A host given nothing is told of nothing.
n=$(nvme discover $T --hostnqn "$H3" 2>&1 | grep -c '^subnqn:')
[ "$n" = 0 ] && r stranger-discovers-nothing PASS || r stranger-discovers-nothing "FAIL ($n)"
# 2. ...and cannot reach the shared subsystem.
nvme connect $T -n "$SHARED" --hostnqn "$H3" >/tmp/o 2>&1 \
    && r stranger-shared-refused FAIL || r stranger-shared-refused "PASS ($(head -1 /tmp/o))"
# 3. H1 is shown exactly its own subsystem.
nvme discover $T --hostnqn "$H1" > /tmp/d 2>&1
subs=$(grep '^subnqn:' /tmp/d | sed 's/^subnqn: *//')
[ "$subs" = "$SUB1" ] && r h1-discovers-its-own PASS || r h1-discovers-its-own "FAIL ($subs)"
# 4. H2 cannot reach H1's subsystem.
nvme connect $T -n "$SUB1" --hostnqn "$H2" >/tmp/o 2>&1 \
    && r h2-into-h1-refused FAIL || r h2-into-h1-refused "PASS ($(head -1 /tmp/o))"
# 5. H1 connects and finds its clone (rw) and the golden (ro), nothing else.
if nvme connect $T -n "$SUB1" --hostnqn "$H1" >/tmp/o 2>&1; then
    sleep 2
    dev_of() { for b in /sys/block/nvme*n*; do [ "$(cat $b/nsid 2>/dev/null)" = "$1" ] && basename $b && return; done; }
    count=$(for b in /sys/block/nvme*n*; do cat $b/nsid; done 2>/dev/null | wc -l)
    [ "$count" = 2 ] && r h1-sees-two-namespaces PASS || r h1-sees-two-namespaces "FAIL ($count)"
    a=$(dev_of "$NS_A"); g=$(dev_of "$NS_G")
    echo "GUEST clone nsid $NS_A = /dev/$a, golden nsid $NS_G = /dev/$g"
    if [ -z "$a" ] || [ -z "$g" ] || [ ! -b "/dev/$a" ] || [ ! -b "/dev/$g" ]; then
        r h1-block-devices "FAIL (clone '$a', golden '$g')"; a=missing; g=missing
    fi
    dd if=/dev/urandom of=/tmp/pat bs=4096 count=16 2>/dev/null
    dd if=/tmp/pat of=/dev/$a bs=4096 count=16 oflag=direct 2>/dev/null
    dd if=/dev/$a of=/tmp/back bs=4096 count=16 iflag=direct 2>/dev/null
    cmp -s /tmp/pat /tmp/back && r h1-clone-round-trip PASS || r h1-clone-round-trip FAIL
    [ "$(cat /sys/block/$g/ro)" = 1 ] && r golden-block-device-ro PASS || r golden-block-device-ro "FAIL ($g ro=$(cat /sys/block/$g/ro))"
    dd if=/tmp/pat of=/dev/$g bs=4096 count=1 oflag=direct 2>/dev/null \
        && r golden-write-refused FAIL || r golden-write-refused PASS
    dd if=/dev/$g of=/dev/null bs=4096 count=16 iflag=direct 2>/dev/null \
        && r golden-readable PASS || r golden-readable FAIL
    nvme disconnect -n "$SUB1" >/dev/null 2>&1
else
    r h1-connects "FAIL ($(head -2 /tmp/o))"
fi
# 6. H2 must prove its secret: none, or another host's, is refused.
nvme connect $T -n "$SUB2" --hostnqn "$H2" >/tmp/o 2>&1 \
    && { r h2-no-secret-refused FAIL; nvme disconnect -n "$SUB2" >/dev/null 2>&1; } \
    || r h2-no-secret-refused "PASS ($(head -1 /tmp/o))"
nvme connect $T -n "$SUB2" --hostnqn "$H2" --dhchap-secret "$S4" >/tmp/o 2>&1 \
    && { r h2-wrong-secret-refused FAIL; nvme disconnect -n "$SUB2" >/dev/null 2>&1; } \
    || r h2-wrong-secret-refused "PASS ($(head -1 /tmp/o))"
if nvme connect $T -n "$SUB2" --hostnqn "$H2" --dhchap-secret "$S2" >/tmp/o 2>&1; then
    sleep 2
    dmesg | grep -q 'authenticated with hash' && r h2-authenticated PASS || r h2-authenticated "FAIL (no auth line)"
    count=$(for b in /sys/block/nvme*n*; do cat $b/nsid; done 2>/dev/null | wc -l)
    [ "$count" = 1 ] && r h2-sees-its-one-namespace PASS || r h2-sees-its-one-namespace "FAIL ($count)"
    nvme disconnect -n "$SUB2" >/dev/null 2>&1
else
    r h2-authenticated "FAIL ($(head -2 /tmp/o))"
fi
echo "GUEST dmesg (nvme):"
dmesg | grep -i nvme | tail -30
echo "GUEST done"
poweroff -f
EOF
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"

say "boot $KERNEL in QEMU"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 300 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 1024 -smp 2 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 panic=-1 loglevel=4" \
    -netdev user,id=n0 -device virtio-net-pci,netdev=n0 > "$W/guest.log" 2>&1
tr -d '\r' < "$W/guest.log" | grep -E '^(RESULT|GUEST)|nvme' | tail -70

results=$(tr -d '\r' < "$W/guest.log" | grep -c '^RESULT ')
bad=$(tr -d '\r' < "$W/guest.log" | grep '^RESULT ' | grep -vc ' PASS')
[ "$results" -ge 12 ] || fail "the guest reported $results results, expected 12"
[ "$bad" = 0 ] || fail "$bad guest check(s) failed"

say "engine's side"
grep -E "refused|DH-HMAC-CHAP|admits" "$W/engine.log" | sed 's/^.*\(WARN\|INFO\)/\1/' | tail -12

if [ "$FAILS" = 0 ]; then echo "ALL PASS ($results guest checks)"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
