#!/usr/bin/env bash
# ci-fd-boot-verify.sh — a boot-sized set of ublk exports against the fd
# limit (#522). server8 (X9, 12.58) ran out of file descriptors in the first
# second of its install: the initramfs engine started 85 boot exports at once,
# ~14 fds each at four queues, against the 1024 its PID 1 handed down; 26
# volumes never appeared and nothing reached the console.
#
# In a QEMU guest (root, ublk_drv, four vCPUs, STORMBLOCK_UBLK_QUEUES=4):
#   raised      the engine started under `prlimit --nofile=1024:1024` (server8's
#               limit, hard included) runs with 65536 (soft and hard), said in
#               its log and in health `fds.limit`
#   all-served  90 volumes attached over ublk: every one a block device that
#               reads, no EMFILE anywhere in the log
#   per-export  the fds they hold, measured, at most the engine's own
#               estimate (`ublk::fds_for`: 8 + 2 × queues = 16)
#   refused     the running engine's limit cut to what is open + 300 (the
#               reserve, 256, plus room for ~2): the next attaches are refused
#               whole and said to the caller and in the log ("file descriptors
#               needed"), never half-made
#               (no "eventfd failed", no "io_uring create failed"), and the 90
#               still read
#
# Needs: cargo, qemu-system-x86_64, /boot/vmlinuz-$(uname -r) and ublk_drv,
# a static busybox, curl and prlimit (copied in with their libraries).
#   sc-build 'sh ci-fd-boot-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
CURL=$(command -v curl || true)
PRLIMIT=$(command -v prlimit || true)
N=${N:-90}
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-fd-boot.XXXXXX")
FAILS=0
say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
trap 'rm -rf "$W"' EXIT

for need in "$KERNEL" "$BUSYBOX" "$CURL" "$PRLIMIT"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
command -v qemu-system-x86_64 >/dev/null || { echo "SKIP: no qemu-system-x86_64"; exit 2; }

say "build"
cargo build --release --locked --bin stormblock 2>&1 | tail -2
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/release/stormblock"
[ -x "$BIN" ] || { echo "FAIL: no binary"; exit 1; }

say "guest initramfs: busybox, stormblock, curl, prlimit, ublk_drv"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,etc,lib/mods}
cp "$BUSYBOX" "$I/bin/busybox"
for a in sh mount insmod ip sleep cat echo ls grep dd poweroff head tail wc sed awk seq mkdir timeout; do
    ln -sf busybox "$I/bin/$a"
done
for b in "$BIN" "$CURL" "$PRLIMIT"; do
    cp "$b" "$I/bin/$(basename "$b")"
    ldd "$b" | grep -o '/[^ ]*' | while read -r lib; do
        mkdir -p "$I$(dirname "$lib")"; cp -L "$lib" "$I$lib"
    done
done
: > "$I/lib/mods/order"
modprobe -S "$KVER" --show-depends ublk_drv 2>/dev/null | awk '$1=="insmod"{print $2}' | while read -r ko; do
    base=$(basename "$ko" | sed 's/\.xz$//; s/\.zst$//')
    case "$ko" in
        *.xz) xz -dc "$ko" > "$I/lib/mods/$base" ;;
        *.zst) zstd -dcq "$ko" > "$I/lib/mods/$base" ;;
        *) cp "$ko" "$I/lib/mods/$base" ;;
    esac
    echo "$base" >> "$I/lib/mods/order"
done
cat > "$I/init" <<EOF
#!/bin/sh
N=$N
EOF
cat >> "$I/init" <<'EOF'
export PATH=/bin
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
mount -t tmpfs run /run; mount -t tmpfs tmp /tmp
for m in $(cat /lib/mods/order); do insmod /lib/mods/$m 2>&1 | sed "s/^/LOG insmod $m: /"; done
ip link set lo up
r() { echo "RESULT $1 $2"; }
echo "GUEST kernel $(cat /proc/sys/kernel/osrelease), $(grep -c ^processor /proc/cpuinfo) CPUs"
[ -e /dev/ublk-control ] || { r ublk_drv FAIL; poweroff -f; }
api() { curl -sf -m 120 -H 'Authorization: Bearer t' -H 'Content-Type: application/json' "$@"; }
port=9095
mkdir -p /run/sb
cat > /run/sb.toml <<EOT
[management]
api_token = "t"
admin_token = "t"
listen_addr = "127.0.0.1:$port"
data_dir = "/run/sb"
node_name = "ci-fd"
discovery_disabled = true
EOT
export STORMBLOCK_UBLK_QUEUES=4
prlimit --nofile=1024:1024 stormblock --config /run/sb.toml --data-dir /run/sb --no-iscsi > /run/engine.log 2>&1 &
pid=$!
for i in $(seq 1 100); do api http://127.0.0.1:$port/api/v1/health >/dev/null 2>&1 && break; sleep 0.2; done
lim=$(awk '/Max open files/ {print $4 "/" $5}' /proc/$pid/limits)
echo "LOG engine's limit (soft/hard): $lim"
h=$(curl -s -m 5 http://127.0.0.1:$port/api/v1/health)
case "$lim:$h" in 65536/65536:*'"limit":65536'*) r raised PASS ;; *) r raised "FAIL ($lim)" ;; esac
grep -q "raised from 1024/1024 to 65536" /run/engine.log && r raised-said PASS || r raised-said FAIL

out=$(curl -s -m 120 -w ' HTTP%{http_code}' -H 'Authorization: Bearer t' -H 'Content-Type: application/json' \
    -X POST http://127.0.0.1:$port/api/v1/slabs -d '{"device_path":"/dev/vda","role":"data"}')
case "$out" in *HTTP2??) ;; *) r slab "FAIL ($out)"; tail -8 /run/engine.log | sed 's/^/LOG /'; poweroff -f ;; esac

f0=$(ls /proc/$pid/fd | wc -l)
ok=0 bad=
for n in $(seq 1 $N); do
    v=$(api -X POST http://127.0.0.1:$port/api/v1/volumes -d "{\"name\":\"boot$n\",\"size\":\"16M\"}" \
        | sed -n 's/.*"id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
    d=$(api -X POST http://127.0.0.1:$port/api/v1/volumes/$v/attach -d '{"transport":"ublk"}' \
        | sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p')
    echo "$d" >> /tmp/devs
    if [ -b "$d" ] && dd if=$d of=/dev/null bs=4096 count=1 iflag=direct 2>/dev/null; then ok=$((ok + 1)); else bad="$bad $n"; fi
done
f1=$(ls /proc/$pid/fd | wc -l)
per=$(( (f1 - f0 + N - 1) / N ))  # rounded up
echo "GUEST $ok of $N exports served; engine fds $f0 -> $f1 ($((f1 - f0)) for $N, ~$per an export)"
[ "$ok" = "$N" ] && r all-served PASS || r all-served "FAIL (not served:$bad)"
[ $((f1 - f0)) -le $((N * 16)) ] && r per-export PASS || r per-export "FAIL ($((f1 - f0)) > $N x 16)"
grep -E "os error 24|No file descriptors" /run/engine.log | head -3 | sed 's/^/LOG emfile: /'
grep -qE "os error 24|No file descriptors" /run/engine.log && r no-emfile FAIL || r no-emfile PASS

# The limit cut to what is open + 300: the reserve (256) and ~3 more exports.
open=$(ls /proc/$pid/fd | wc -l)
prlimit --pid $pid --nofile=$((open + 300)):$((open + 300))
echo "LOG limit cut to $((open + 300)) with $open open"
refused=0 served=0 said=0
for n in $(seq 1 10); do
    v=$(api -X POST http://127.0.0.1:$port/api/v1/volumes -d "{\"name\":\"more$n\",\"size\":\"16M\"}" \
        | sed -n 's/.*"id":"\([0-9a-f-]*\)".*/\1/p' | head -1)
    c=$(curl -s -m 60 -o /tmp/a -w '%{http_code}' -H 'Authorization: Bearer t' -H 'Content-Type: application/json' \
        -X POST http://127.0.0.1:$port/api/v1/volumes/$v/attach -d '{"transport":"ublk"}')
    grep -q "file descriptors needed" /tmp/a && said=1
    case "$c" in 200) served=$((served + 1)) ;; *) refused=$((refused + 1)); [ "$refused" = 1 ] && echo "LOG first refusal: $c $(head -c 300 /tmp/a)" ;; esac
done
echo "GUEST under the cut limit: $served served, $refused refused"
grep "file descriptors needed" /run/engine.log | head -1 | sed 's/^/LOG said: /'
[ "$refused" -ge 5 ] && [ "$said" = 1 ] && grep -q "file descriptors needed" /run/engine.log && r refused PASS || r refused "FAIL ($served served, $refused refused)"
grep -qE "eventfd failed|io_uring create failed|os error 24" /run/engine.log && r refused-whole FAIL || r refused-whole PASS
still=0
for d in $(cat /tmp/devs); do dd if=$d of=/dev/null bs=4096 count=1 iflag=direct 2>/dev/null && still=$((still + 1)); done
[ "$still" = "$N" ] && r still-served PASS || r still-served "FAIL ($still of $N)"
grep -E "ERROR" /run/engine.log | head -5 | sed 's/^/LOG engine: /'
echo "GUEST done"
poweroff -f
EOF
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"
truncate -s 4G "$W/a.img"

say "boot $KERNEL in QEMU"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 900 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 3072 -smp 4 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 panic=-1 loglevel=4" \
    -drive file="$W/a.img",if=virtio,format=raw > "$W/guest.log" 2>&1
tr -d '\r' < "$W/guest.log" | grep -E '^(RESULT|GUEST|LOG)|panick'
for m in raised raised-said all-served per-export no-emfile refused refused-whole still-served; do
    tr -d '\r' < "$W/guest.log" | grep -q "^RESULT $m PASS" || fail "$m"
done
if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
