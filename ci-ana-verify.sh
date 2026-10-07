#!/usr/bin/env bash
# ci-ana-verify.sh — live migration of a disk as the Linux kernel sees it
# (#83). Unprivileged: the kernel is a guest.
#
# Two engines run here, on dev, A and B: one volume (the same id, so the
# same NGUID) served from each on the same subsystem NQN at NSID 1, with
# controller-ID ranges that do not overlap. The host's own kernel boots in
# QEMU with native NVMe multipath and connects to both:
#
#   one multipath head, two paths            → A optimized, B inaccessible
#   write P1 through the head, read it back  → P1 (A's data)
#   A → inaccessible, B → optimized (API)    → ANA change notice; the paths'
#                                              ana_state follow; a read through
#                                              the same head now returns B's
#                                              data (zeros): the I/O moved, with
#                                              no unmount and no reconnect
#   write P2 through the head; flip back     → reads P1 again (A's)
#   a /v1 leg attached for the guest (epoch 1), written; fence 1 → 2
#                                            → the guest's next write fails
#
# A and B hold different bytes on purpose: which bytes come back is what
# says which node served the read. (Keeping both copies the same is the
# mirror's job — stormstorage's heads, #179 — not this check's.)
#
# Needs: cargo, qemu-system-x86_64, /boot/vmlinuz-$(uname -r) and its modules,
# a static busybox, nvme-cli (copied into the guest with its libraries), curl.
# Run on dev through sc-build:  sc-build 'sh ci-ana-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
NVME=$(command -v nvme || true)
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-ana.XXXXXX")
BASE=$((20000 + RANDOM % 20000))
PORT_A=$BASE; MGMT_A=$((BASE + 1)); PORT_B=$((BASE + 2)); MGMT_B=$((BASE + 3))
TOKEN=$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')
SHARED="nqn.2026-10.lo.storm:ci-ana"
H1="nqn.2014-08.org.nvmexpress:uuid:11111111-2222-3333-4444-555555555583"
PIDS=""
FAILS=0

say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
cleanup() {
    for p in $PIDS; do kill "$p" 2>/dev/null; wait "$p" 2>/dev/null; done
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

# engine NAME PORT MGMT CNTLID_MIN CNTLID_MAX
engine() {
    local n=$1 port=$2 mgmt=$3 lo=$4 hi=$5
    mkdir -p "$W/$n/data"
    truncate -s 512M "$W/$n/d1.img"
    cat > "$W/$n/stormblock.toml" <<EOF
[management]
api_token = "$TOKEN"
admin_token = "$TOKEN"
listen_addr = "127.0.0.1:$mgmt"
data_dir = "$W/$n/data"
node_name = "ci-ana-$n"
advertised_addr = "10.0.2.2"
discovery_disabled = true
nvme_cntlid_range = [$lo, $hi]

[nvmeof]
export_drives = false
allowed_hosts = ["$H1"]
EOF
    RUST_LOG=stormblock=info "$BIN" --config "$W/$n/stormblock.toml" \
        --device "$W/$n/d1.img" --data-dir "$W/$n/data" --no-iscsi \
        --nvmeof-addr "127.0.0.1:$port" --nvmeof-nqn "$SHARED" \
        > "$W/$n/engine.log" 2>&1 &
    PIDS="$PIDS $!"
}
api() { curl -sf -m 120 -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "$@"; }
field() { sed -n "s/.*\"$1\":\"\{0,1\}\([^\",}]*\)\"\{0,1\}.*/\1/p" | head -1; }

say "engines: A on :$PORT_A (cntlid 1-999), B on :$PORT_B (cntlid 1000-1999)"
engine a "$PORT_A" "$MGMT_A" 1 999
engine b "$PORT_B" "$MGMT_B" 1000 1999
for m in $MGMT_A $MGMT_B; do
    for _ in $(seq 1 100); do api "http://127.0.0.1:$m/api/v1/health" >/dev/null 2>&1 && break; sleep 0.2; done
    api "http://127.0.0.1:$m/api/v1/health" >/dev/null || { echo "FAIL: engine on $m did not start"; tail -30 "$W"/*/engine.log; exit 1; }
done
for n in a b; do
    m=$MGMT_A; [ $n = b ] && m=$MGMT_B
    r=$(curl -s -m 180 -w ' HTTP%{http_code}' -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
        -X POST "http://127.0.0.1:$m/api/v1/slabs" -d "{\"device_path\":\"$W/$n/d1.img\",\"role\":\"data\"}")
    case "$r" in *HTTP2??) ;; *) fail "slab on $n: $r" ;; esac
done

# One volume, one id, on both.
VOL=$(api -X POST "http://127.0.0.1:$MGMT_A/api/v1/volumes" -d '{"name":"vm-disk","size":"64M"}' | field id)
[ -n "$VOL" ] || { echo "FAIL: create on A"; tail -20 "$W/a/engine.log"; exit 1; }
VB=$(api -X POST "http://127.0.0.1:$MGMT_B/api/v1/volumes" -d "{\"name\":\"vm-disk\",\"size\":\"64M\",\"id\":\"$VOL\"}" | field id)
[ "$VB" = "$VOL" ] && echo "ok: B created the volume under A's id $VOL" || fail "B's copy has id '$VB', not $VOL"
code=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    -X POST "http://127.0.0.1:$MGMT_B/api/v1/volumes" -d "{\"name\":\"again\",\"size\":\"64M\",\"id\":\"$VOL\"}")
[ "$code" = 400 ] && echo "ok: a second volume with that id is refused" || fail "duplicate id gave $code"

NS_A=$(api -X POST "http://127.0.0.1:$MGMT_A/api/v1/volumes/$VOL/attach" -d '{"transport":"nvme-tcp"}' | field nsid)
NS_B=$(api -X POST "http://127.0.0.1:$MGMT_B/api/v1/volumes/$VOL/attach" -d '{"transport":"nvme-tcp"}' | field nsid)
[ -n "$NS_A" ] && [ "$NS_A" = "$NS_B" ] && echo "ok: NSID $NS_A on both" || { echo "FAIL: NSIDs '$NS_A' '$NS_B'"; exit 1; }
api -X PUT "http://127.0.0.1:$MGMT_B/api/v1/volumes/$VOL/ana" -d '{"state":"inaccessible"}' >/dev/null \
    || fail "set B inaccessible"

# A /v1 leg on A, attached for the guest (its own subsystem), at epoch 1.
LEG=$(api -X POST "http://127.0.0.1:$MGMT_A/v1/volumes" \
    -d '{"name":"leg-0","size_bytes":67108864,"replica_tier":{"slaves":0}}' | field id)
RL=$(api -X POST "http://127.0.0.1:$MGMT_A/v1/volumes/$LEG/attach" \
    -d "{\"node\":\"ci-ana-a\",\"mode\":\"read_write\",\"transport\":\"nvme_tcp\",\"host_nqn\":\"$H1\",\"epoch\":1}")
LEG_SUB=$(echo "$RL" | field nqn); LEG_NS=$(echo "$RL" | field nsid)
[ -n "$LEG_SUB" ] && [ -n "$LEG_NS" ] || { echo "FAIL: leg attach: $RL"; exit 1; }

say "guest initramfs: busybox, nvme-cli, nvme-tcp"
I="$W/initrd"
mkdir -p "$I"/{bin,sbin,dev,proc,sys,run,tmp,etc/nvme,lib/mods}
cp "$BUSYBOX" "$I/bin/busybox"
for a in sh mount insmod ip sleep cat echo ls grep dd cmp poweroff dmesg head tail wc sed basename cut tr sort od; do
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
PORT_A=$PORT_A
PORT_B=$PORT_B
SHARED=$SHARED
H1=$H1
LEG_SUB=$LEG_SUB
LEG_NS=$LEG_NS
EOF
cat > "$I/init" <<'EOF'
#!/bin/sh
export PATH=/bin
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
. /env
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>/dev/null; done
ip link set lo up; ip link set eth0 up
ip addr add 10.0.2.15/24 dev eth0; ip route add default via 10.0.2.2
r() { echo "RESULT $1 $2"; }
echo "GUEST kernel $(cat /proc/sys/kernel/osrelease) multipath=$(cat /sys/module/nvme_core/parameters/multipath 2>/dev/null)"
[ -e /dev/nvme-fabrics ] || { r modules FAIL; poweroff -f; }

# The ANA state of the path through the engine on port $1.
state_via() {
    for c in /sys/class/nvme/nvme*; do
        grep -q "trsvcid=$1" "$c/address" 2>/dev/null || continue
        for p in "$c"/nvme*c*n*; do cat "$p/ana_state" 2>/dev/null && return; done
    done
    echo none
}
# Wait until the path via $1 reads $2.
wait_state() {
    for _ in $(seq 1 100); do [ "$(state_via "$1")" = "$2" ] && return 0; sleep 0.2; done
    return 1
}
sync_point() { echo "SYNC $1"; while [ ! -e "/tmp/go-$1" ]; do
    # The host answers by flipping states; the guest sees it in sysfs.
    case "$1" in
        flip1) [ "$(state_via $PORT_B)" = optimized ] && break ;;
        flip2) [ "$(state_via $PORT_A)" = optimized ] && break ;;
        fence) break ;;
    esac; sleep 0.2; done; }

nvme connect -t tcp -a 10.0.2.2 -s $PORT_A -n "$SHARED" --hostnqn "$H1" >/tmp/o 2>&1 || r connect-a "FAIL ($(head -1 /tmp/o))"
nvme connect -t tcp -a 10.0.2.2 -s $PORT_B -n "$SHARED" --hostnqn "$H1" >/tmp/o 2>&1 || r connect-b "FAIL ($(head -1 /tmp/o))"
sleep 2
heads=$(ls -d /sys/block/nvme*n* 2>/dev/null | grep -v 'c[0-9]*n' | wc -l)
H=$(ls -d /sys/block/nvme*n* 2>/dev/null | grep -v 'c[0-9]*n' | head -1 | xargs basename 2>/dev/null)
paths=$(ls -d /sys/class/nvme/nvme*/nvme*c*n* 2>/dev/null | wc -l)
echo "GUEST head /dev/$H, $heads head(s), $paths path(s)"
[ "$heads" = 1 ] && [ "$paths" = 2 ] && r one-head-two-paths PASS || r one-head-two-paths "FAIL ($heads heads, $paths paths)"
a=$(state_via $PORT_A); b=$(state_via $PORT_B)
[ "$a" = optimized ] && [ "$b" = inaccessible ] && r initial-states PASS || r initial-states "FAIL (A $a, B $b)"

dd if=/dev/urandom of=/tmp/p1 bs=4096 count=16 2>/dev/null
dd if=/tmp/p1 of=/dev/$H bs=4096 count=16 oflag=direct 2>/dev/null
dd if=/dev/$H of=/tmp/back bs=4096 count=16 iflag=direct 2>/dev/null
cmp -s /tmp/p1 /tmp/back && r write-read-via-a PASS || r write-read-via-a FAIL

sync_point flip1
wait_state $PORT_B optimized && wait_state $PORT_A inaccessible \
    && r states-followed-the-move PASS || r states-followed-the-move "FAIL (A $(state_via $PORT_A), B $(state_via $PORT_B))"
dd if=/dev/zero of=/tmp/zero bs=4096 count=16 2>/dev/null
dd if=/dev/$H of=/tmp/back bs=4096 count=16 iflag=direct 2>/dev/null
cmp -s /tmp/zero /tmp/back && r read-now-served-by-b PASS || r read-now-served-by-b "FAIL ($(od -An -tx1 -N8 /tmp/back))"
dd if=/dev/urandom of=/tmp/p2 bs=4096 count=16 2>/dev/null
dd if=/tmp/p2 of=/dev/$H bs=4096 count=16 oflag=direct 2>/dev/null && r write-via-b PASS || r write-via-b FAIL

sync_point flip2
wait_state $PORT_A optimized && r states-followed-back PASS || r states-followed-back "FAIL (A $(state_via $PORT_A))"
dd if=/dev/$H of=/tmp/back bs=4096 count=16 iflag=direct 2>/dev/null
cmp -s /tmp/p1 /tmp/back && r read-back-on-a-is-p1 PASS || r read-back-on-a-is-p1 FAIL
dmesg | grep -i 'ana' | tail -5

# The leg: connected as the head, written, then fenced away.
nvme connect -t tcp -a 10.0.2.2 -s $PORT_A -n "$LEG_SUB" --hostnqn "$H1" >/tmp/o 2>&1 || r leg-connect "FAIL ($(head -1 /tmp/o))"
sleep 2
L=""
for c in /sys/class/nvme/nvme*; do
    [ "$(cat $c/subsysnqn 2>/dev/null)" = "$LEG_SUB" ] || continue
    for p in /sys/block/nvme*n*; do
        [ "$(cat $p/nsid 2>/dev/null)" = "$LEG_NS" ] || continue
        grep -q "$LEG_SUB" /sys/block/$(basename $p)/device/subsysnqn 2>/dev/null && L=$(basename $p)
    done
done
[ -n "$L" ] || L=$(for p in /sys/block/nvme*n*; do b=$(basename $p); [ "$b" = "$H" ] || echo $b; done | grep -v 'c[0-9]*n' | head -1)
echo "GUEST leg /dev/$L"
dd if=/tmp/p1 of=/dev/$L bs=4096 count=4 oflag=direct 2>/dev/null && r leg-write-before-fence PASS || r leg-write-before-fence FAIL
sync_point fence
ok=0
for _ in $(seq 1 50); do
    dd if=/tmp/p1 of=/dev/$L bs=4096 count=1 oflag=direct 2>/dev/null || { ok=1; break; }
    sleep 0.2
done
[ $ok = 1 ] && r leg-write-after-fence-fails PASS || r leg-write-after-fence-fails FAIL
echo "GUEST dmesg (nvme):"
dmesg | grep -i nvme | tail -25
echo "GUEST done"
poweroff -f
EOF
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"

say "boot $KERNEL in QEMU"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 400 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 1024 -smp 2 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 panic=-1 loglevel=4 nvme_core.multipath=Y" \
    -netdev user,id=n0 -device virtio-net-pci,netdev=n0 > "$W/guest.log" 2>&1 &
QPID=$!
PIDS="$PIDS $QPID"

# The host's side of each sync point.
done_flip1=0; done_flip2=0; done_fence=0
while kill -0 "$QPID" 2>/dev/null; do
    log=$(tr -d '\r' < "$W/guest.log")
    if [ $done_flip1 = 0 ] && echo "$log" | grep -q '^SYNC flip1'; then
        say "move: A inaccessible, B optimized"
        api -X PUT "http://127.0.0.1:$MGMT_A/api/v1/volumes/$VOL/ana" -d '{"state":"inaccessible"}' >/dev/null || fail "flip A"
        api -X PUT "http://127.0.0.1:$MGMT_B/api/v1/volumes/$VOL/ana" -d '{"state":"optimized"}' >/dev/null || fail "flip B"
        done_flip1=1
    fi
    if [ $done_flip2 = 0 ] && echo "$log" | grep -q '^SYNC flip2'; then
        say "move back: B inaccessible, A optimized"
        api -X PUT "http://127.0.0.1:$MGMT_B/api/v1/volumes/$VOL/ana" -d '{"state":"inaccessible"}' >/dev/null || fail "flip B back"
        api -X PUT "http://127.0.0.1:$MGMT_A/api/v1/volumes/$VOL/ana" -d '{"state":"optimized"}' >/dev/null || fail "flip A back"
        done_flip2=1
    fi
    if [ $done_fence = 0 ] && echo "$log" | grep -q '^SYNC fence'; then
        say "fence the leg 1 → 2"
        F=$(api -X POST "http://127.0.0.1:$MGMT_A/v1/volumes/$LEG/fence" -d '{"expected_epoch":1}')
        echo "fence: $F"
        echo "$F" | grep -q '"revoked":1' || fail "fence revoked nothing: $F"
        code=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
            -X POST "http://127.0.0.1:$MGMT_A/v1/volumes/$LEG/attach" \
            -d "{\"node\":\"ci-ana-a\",\"mode\":\"read_write\",\"transport\":\"nvme_tcp\",\"host_nqn\":\"$H1\",\"epoch\":1}")
        [ "$code" = 412 ] && echo "ok: a stale reattach is refused (412)" || fail "stale reattach gave $code"
        done_fence=1
    fi
    sleep 0.3
done
wait "$QPID" 2>/dev/null
tr -d '\r' < "$W/guest.log" | grep -E '^(RESULT|GUEST|SYNC)|nvme|ANA|ana' | tail -80

results=$(tr -d '\r' < "$W/guest.log" | grep -c '^RESULT ')
bad=$(tr -d '\r' < "$W/guest.log" | grep '^RESULT ' | grep -vc ' PASS')
[ "$results" -ge 11 ] || fail "the guest reported $results results, expected 11"
[ "$bad" = 0 ] || fail "$bad guest check(s) failed"

say "engines' side"
grep -hE "ANA|revoked|controller [0-9]+ connected" "$W"/a/engine.log "$W"/b/engine.log | sed 's/^.*\(WARN\|INFO\)/\1/' | tail -16

if [ "$FAILS" = 0 ]; then echo "ALL PASS ($results guest checks)"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
