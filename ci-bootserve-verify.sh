#!/usr/bin/env bash
# ci-bootserve-verify.sh: stormboot-serve with the Linux kernel as the node
# (#474). Unprivileged: the kernel is a guest.
#
# `stormboot-serve` serves one image file here, for one named host, as the
# owner's laptop does for a node on the bench. A claim is made over its
# boothost API as stormbootx makes it. Then this machine's own kernel boots
# in QEMU (KVM when /dev/kvm is usable) from an initramfs holding busybox,
# nvme-cli and nvme-tcp, and does what the node's initramfs does with the
# claim reply:
#
#   nvme discover  as a host not named       -> nothing listed
#   nvme connect   to the host's subsystem as another host -> refused
#   nvme discover  as the host               -> exactly its subsystem
#   nvme connect   as the host               -> one namespace, the image's
#                  size; every byte reads as the image (sha256); a write
#                  reads back, flushes, and a Deallocate reads as zeros
# Then, here: the image file is unchanged byte for byte, the claim's overlay
# holds the host's writes (`GET /api/v1/boothost`), and a stop removes it.
#
# Needs: cargo, qemu-system-x86_64, /boot/vmlinuz-$(uname -r) and its
# modules, a static busybox, nvme-cli, curl.  sc-build 'bash ci-bootserve-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
NVME=$(command -v nvme || true)
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-bootserve.XXXXXX")
PORT=$((20000 + RANDOM % 20000))
HTTP=$((PORT + 1))
HOST=server8
SRV_PID=""
FAILS=0

say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
ok()   { echo "  ok    $*"; }
cleanup() {
    [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null && wait "$SRV_PID" 2>/dev/null
    rm -rf "$W"
}
trap cleanup EXIT

for need in "$KERNEL" "$BUSYBOX" "$NVME"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
command -v qemu-system-x86_64 >/dev/null || { echo "SKIP: no qemu-system-x86_64"; exit 2; }

say "build stormboot-serve"
cargo build --release --locked -p stormblock-nvmeof --bin stormboot-serve 2>&1 | tail -2
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/release/stormboot-serve"
[ -x "$BIN" ] || { echo "FAIL: no binary"; exit 1; }

say "an image: 48 MiB of known bytes and a ragged tail (an ISO's length)"
head -c $((48 * 1024 * 1024 + 2048)) /dev/urandom > "$W/release.img"
IMG_SUM=$(sha256sum < "$W/release.img" | cut -d' ' -f1)
# What the node reads: the file rounded up to whole 4 KiB blocks, zeros after.
cp "$W/release.img" "$W/padded"; truncate -s $(((48 * 1024 * 1024 + 2048 + 4095) / 4096 * 4096)) "$W/padded"
NS_SUM=$(sha256sum < "$W/padded" | cut -d' ' -f1)
NS_BYTES=$(stat -c %s "$W/padded")

say "stormboot-serve on 127.0.0.1:$PORT (NVMe/TCP) and :$HTTP (boothost)"
RUST_LOG=info "$BIN" --image "$W/release.img" --host "$HOST" --listen 127.0.0.1 \
    --nvme-port "$PORT" --http-port "$HTTP" --advertise 10.0.2.2 --scratch "$W/overlays" \
    > "$W/serve.log" 2>&1 &
SRV_PID=$!
for _ in $(seq 1 100); do curl -sf "http://127.0.0.1:$HTTP/api/v1/health" >/dev/null 2>&1 && break; sleep 0.1; done
curl -sf "http://127.0.0.1:$HTTP/api/v1/health" >/dev/null || { echo "FAIL: did not start"; cat "$W/serve.log"; exit 1; }
head -12 "$W/serve.log"

INTENT=$(curl -sf "http://127.0.0.1:$HTTP/api/v1/synonyms/boothost/$HOST/intent")
echo "$INTENT" | grep -q '"intent":"install"' && ok "intent: $INTENT" || fail "intent: $INTENT"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$HTTP/api/v1/synonyms/boothost/stranger/claim" -d '{}' -H 'Content-Type: application/json')
[ "$CODE" = 404 ] && ok "a host not named gets no claim (404)" || fail "a stranger's claim: $CODE"
CLAIM=$(curl -sf -X POST "http://127.0.0.1:$HTTP/api/v1/synonyms/boothost/$HOST/claim" \
    -H 'Content-Type: application/json' -d '{"mac":"0c:c4:7a:06:f9:6d","agent":{"name":"ci"}}')
echo "  claim: $CLAIM"
field() { echo "$CLAIM" | sed -n "s/.*\"$1\":\"\{0,1\}\([^\",}]*\)\"\{0,1\}.*/\1/p" | head -1; }
SUB=$(field nqn); NSID=$(field nsid); HNQN=$(echo "$CLAIM" | sed -n 's/.*"host_nqns":\["\([^"]*\)".*/\1/p')
case "$CLAIM" in '{"attach":{'*) ok "attach is the reply's first key (stormbootx reads the first match)" ;; *) fail "attach is not first" ;; esac
[ "$(field address)" = 10.0.2.2 ] && [ "$(field port)" = "$PORT" ] && [ -n "$SUB" ] && [ -n "$NSID" ] && [ -n "$HNQN" ] \
    && ok "the claim names 10.0.2.2:$PORT, $SUB nsid $NSID, host $HNQN" || { fail "the claim reply"; exit 1; }

say "guest initramfs: busybox, nvme-cli, nvme-tcp"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,etc/nvme,lib/mods}
cp "$BUSYBOX" "$I/bin/busybox"
for a in $("$BUSYBOX" --list); do [ -e "$I/bin/$a" ] || ln -s busybox "$I/bin/$a"; done
cp "$NVME" "$I/bin/nvme"
ldd "$NVME" | grep -o '/[^ ]*' | while read -r lib; do mkdir -p "$I$(dirname "$lib")"; cp -L "$lib" "$I$lib"; done
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
SUB=$SUB
NSID=$NSID
HNQN=$HNQN
NS_SUM=$NS_SUM
NS_BYTES=$NS_BYTES
OTHER=nqn.2026-09.lo.storm:host-server7
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
n=$(nvme discover $T --hostnqn "$OTHER" 2>&1 | grep -c '^subnqn:')
[ "$n" = 0 ] && r stranger-discovers-nothing PASS || r stranger-discovers-nothing "FAIL ($n)"
nvme connect $T -n "$SUB" --hostnqn "$OTHER" >/tmp/o 2>&1 \
    && r other-host-refused FAIL || r other-host-refused "PASS ($(head -1 /tmp/o))"
subs=$(nvme discover $T --hostnqn "$HNQN" 2>&1 | grep '^subnqn:' | sed 's/^subnqn: *//')
[ "$subs" = "$SUB" ] && r host-discovers-its-own PASS || r host-discovers-its-own "FAIL ($subs)"
if nvme connect $T -n "$SUB" --hostnqn "$HNQN" >/tmp/o 2>&1; then
    sleep 2
    d=""
    for b in /sys/block/nvme*n*; do case $(basename $b) in nvme*c*n*) ;; *) [ "$(cat $b/nsid)" = "$NSID" ] && d=$(basename $b) ;; esac; done
    [ -n "$d" ] && r one-namespace "PASS (/dev/$d)" || { r one-namespace FAIL; d=missing; }
    bytes=$(( $(cat /sys/block/$d/size) * 512 ))
    [ "$bytes" = "$NS_BYTES" ] && r size "PASS ($bytes)" || r size "FAIL ($bytes, want $NS_BYTES)"
    [ "$(cat /sys/block/$d/queue/logical_block_size)" = 4096 ] && r block-4096 PASS || r block-4096 FAIL
    sum=$(dd if=/dev/$d bs=1M iflag=direct 2>/dev/null | sha256sum | cut -d' ' -f1)
    [ "$sum" = "$NS_SUM" ] && r reads-the-image PASS || r reads-the-image "FAIL ($sum)"
    dd if=/dev/urandom of=/tmp/pat bs=4096 count=300 2>/dev/null
    dd if=/tmp/pat of=/dev/$d bs=4096 seek=1000 count=300 oflag=direct 2>/dev/null
    dd if=/dev/$d of=/tmp/back bs=4096 skip=1000 count=300 iflag=direct 2>/dev/null
    cmp -s /tmp/pat /tmp/back && r write-reads-back PASS || r write-reads-back FAIL
    nvme flush /dev/$d >/dev/null 2>&1 && r flush PASS || r flush FAIL
    if nvme dsm /dev/$d --ad -s 2000 -b 8 >/dev/null 2>&1; then
        z=$(dd if=/dev/$d bs=4096 skip=2000 count=8 iflag=direct 2>/dev/null | tr -d '\0' | wc -c)
        [ "$z" = 0 ] && r deallocate-reads-zeros PASS || r deallocate-reads-zeros "FAIL ($z non-zero bytes)"
    else
        r deallocate-reads-zeros "FAIL (nvme dsm)"
    fi
    nvme disconnect -n "$SUB" >/dev/null 2>&1
else
    r host-connects "FAIL ($(head -2 /tmp/o))"
fi
echo "GUEST dmesg (nvme):"; dmesg | grep -i nvme | tail -15
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
tr -d '\r' < "$W/guest.log" | grep -E '^(RESULT|GUEST)|nvme' | tail -40
results=$(tr -d '\r' < "$W/guest.log" | grep -c '^RESULT ')
bad=$(tr -d '\r' < "$W/guest.log" | grep '^RESULT ' | grep -vc ' PASS')
[ "$results" -ge 10 ] || fail "the guest reported $results results, expected 10"
[ "$bad" = 0 ] || fail "$bad guest check(s) failed"

say "this side"
[ "$(sha256sum < "$W/release.img" | cut -d' ' -f1)" = "$IMG_SUM" ] && ok "the image file is unchanged" || fail "the image file changed"
LIST=$(curl -sf "http://127.0.0.1:$HTTP/api/v1/boothost")
echo "  $LIST"
WB=$(echo "$LIST" | sed -n 's/.*"written_blocks":\([0-9]*\).*/\1/p' | head -1)
[ "${WB:-0}" -ge 300 ] && ok "the host's overlay holds its writes ($WB blocks)" || fail "overlay written_blocks: ${WB:-none}"
ls "$W/overlays"/*.overlay >/dev/null 2>&1 && ok "an overlay file while served" || fail "no overlay file"
kill -INT "$SRV_PID"; wait "$SRV_PID" 2>/dev/null; SRV_PID=""
ls "$W/overlays"/*.overlay >/dev/null 2>&1 && fail "an overlay outlived the stop" || ok "a stop removes the overlays"
grep -E "refused|claim|connected" "$W/serve.log" | sed 's/^.*\(INFO\|WARN\)/\1/' | tail -8

if [ "$FAILS" = 0 ]; then echo "ALL PASS ($results guest checks)"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
