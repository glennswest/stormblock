#!/usr/bin/env bash
# ci-xfs-mount-verify.sh — an engine-made XFS blank and two restamped claims,
# mounted by a real kernel (#225). Unprivileged: the kernel is a guest.
#
# ci-xfs-verify.sh judges the engine's XFS with xfsprogs on files; nothing
# mounted one. A claim is restamped by the engine (a new sb_uuid, META_UUID
# set, the blank's UUID kept as meta_uuid), and the ext4 precedent (fio-ext4
# v1.3.1) is a filesystem that passed our checks and failed to mount with EIO.
#
# The engine runs here on dev: an XFS template (mkfs-xfs) is created and
# sealed, two claims are minted from it, and all three are attached to one
# host over NVMe/TCP (the blank read-only). Dev's own kernel then boots in
# QEMU from an initramfs with busybox, nvme-cli, xfsprogs and the nvme-tcp and
# xfs modules, and:
#   * mounts the blank (ro) and both claims (rw) at once: the kernel refuses
#     two XFS filesystems with one UUID, so this is the restamp's real test;
#   * writes a file to each claim, syncs, unmounts, remounts one and reads its
#     file back;
#   * runs xfs_repair -n on all three, finds no XFS error in dmesg, and reads
#     the UUIDs: three distinct, each claim's meta_uuid the blank's.
#
# Needs: cargo, qemu-system-x86_64, /boot/vmlinuz-$(uname -r) and its modules,
# a static busybox, nvme-cli, xfsprogs (copied into the guest with their
# libraries), curl, python3. Run on dev through sc-build:
#   sc-build 'bash ci-xfs-mount-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
NVME=$(command -v nvme || true)
XFS_REPAIR=$(command -v xfs_repair || true)
XFS_DB=$(command -v xfs_db || true)
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-xfs-mount.XXXXXX")
PORT=$((20000 + RANDOM % 20000))
MGMT=$((PORT + 1))
TOKEN=$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')
SHARED="nqn.2026-10.lo.storm:ci-xfs-shared"
H1="nqn.2014-08.org.nvmexpress:uuid:22222222-3333-4444-5555-666666666601"
SB_PID=""
FAILS=0

say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
cleanup() {
    [ -n "$SB_PID" ] && kill "$SB_PID" 2>/dev/null && wait "$SB_PID" 2>/dev/null
    rm -rf "$W"
}
trap cleanup EXIT

for need in "$KERNEL" "$BUSYBOX" "$NVME" "$XFS_REPAIR" "$XFS_DB"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
command -v qemu-system-x86_64 >/dev/null || { echo "SKIP: no qemu-system-x86_64"; exit 2; }

say "build"
cargo build --release --locked 2>&1 | tail -2
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/release/stormblock"
[ -x "$BIN" ] || { echo "FAIL: no binary"; exit 1; }

say "engine on 127.0.0.1:$PORT (NVMe/TCP) and :$MGMT (API)"
mkdir -p "$W/data"
truncate -s 4G "$W/d1.img"
cat > "$W/stormblock.toml" <<EOF
[management]
api_token = "$TOKEN"
# The slab format and the seal are destructive verbs (#274).
admin_token = "$TOKEN"
listen_addr = "127.0.0.1:$MGMT"
data_dir = "$W/data"
node_name = "ci-xfs-mount"
advertised_addr = "10.0.2.2"
discovery_disabled = true

[nvmeof]
export_drives = false
EOF
RUST_LOG=stormblock=info "$BIN" --config "$W/stormblock.toml" \
    --device "$W/d1.img" --data-dir "$W/data" --no-iscsi \
    --nvmeof-addr "127.0.0.1:$PORT" --nvmeof-nqn "$SHARED" \
    > "$W/engine.log" 2>&1 &
SB_PID=$!
api() { curl -sf -m 300 -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "$@"; }
j() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1]))" "$1"; }
for _ in $(seq 1 100); do
    api "http://127.0.0.1:$MGMT/api/v1/health" >/dev/null 2>&1 && break
    sleep 0.2
done
api "http://127.0.0.1:$MGMT/api/v1/health" >/dev/null || { echo "FAIL: engine did not start"; tail -30 "$W/engine.log"; exit 1; }
api -X POST "http://127.0.0.1:$MGMT/api/v1/slabs" -d "{\"device_path\":\"$W/d1.img\",\"role\":\"data\"}" >/dev/null \
    || { echo "FAIL: slab"; tail -10 "$W/engine.log"; exit 1; }

say "an XFS blank (mkfs-xfs, sealed) and two claims of it"
T=$(api -X POST "http://127.0.0.1:$MGMT/api/v1/fstemplates" \
    -d '{"name":"ci-xfs-blank","size":"512M","fs":"xfs","label":"blank"}') \
    || { echo "FAIL: template"; tail -20 "$W/engine.log"; exit 1; }
T=$(echo "$T" | j 'json.dumps(d.get("template", d))')
TID=$(echo "$T" | j 'd["id"]')
G=$(echo "$T" | j 'd.get("sealed_volume_id") or d.get("volume_id")')
GU=$(echo "$T" | j 'd.get("fs_uuid") or ""')
claim() {
    api -X POST "http://127.0.0.1:$MGMT/api/v1/fstemplates/$TID/clone" -d "{\"name\":\"$1\"}"
}
CA=$(claim ci-xfs-claim-a) && CB=$(claim ci-xfs-claim-b) || { echo "FAIL: claims"; tail -20 "$W/engine.log"; exit 1; }
A=$(echo "$CA" | j 'd["volume_id"]'); AU=$(echo "$CA" | j 'd.get("fs_uuid") or ""')
B=$(echo "$CB" | j 'd["volume_id"]'); BU=$(echo "$CB" | j 'd.get("fs_uuid") or ""')
echo "  blank $G (uuid $GU), claim a $A (uuid $AU), claim b $B (uuid $BU)"
[ -n "$G" ] && [ -n "$A" ] && [ -n "$B" ] || { echo "FAIL: ids: $T $CA $CB"; exit 1; }

say "attach all three to one host over NVMe/TCP (the blank read-only)"
RG=$(api -X POST "http://127.0.0.1:$MGMT/api/v1/exports" \
    -d "{\"volume_id\":\"$G\",\"protocol\":\"nvmeof\",\"host_nqn\":\"$H1\"}") || fail "export the blank"
RA=$(api -X POST "http://127.0.0.1:$MGMT/api/v1/volumes/$A/attach" -d "{\"transport\":\"nvme-tcp\",\"host_nqn\":\"$H1\"}") || fail "attach a"
RB=$(api -X POST "http://127.0.0.1:$MGMT/api/v1/volumes/$B/attach" -d "{\"transport\":\"nvme-tcp\",\"host_nqn\":\"$H1\"}") || fail "attach b"
SUB=$(echo "$RA" | j 'd["nqn"]')
NS_G=$(echo "$RG" | j 'd["nsid"]'); NS_A=$(echo "$RA" | j 'd["nsid"]'); NS_B=$(echo "$RB" | j 'd["nsid"]')
echo "  subsystem $SUB: blank nsid $NS_G, a nsid $NS_A, b nsid $NS_B"

say "guest initramfs: busybox, nvme-cli, xfsprogs, nvme-tcp, xfs"
I="$W/initrd"
mkdir -p "$I"/{bin,sbin,dev,proc,sys,run,tmp,mnt/g,mnt/a,mnt/b,etc/nvme,lib/mods}
cp "$BUSYBOX" "$I/bin/busybox"
for a in $("$BUSYBOX" --list); do [ -e "$I/bin/$a" ] || ln -s busybox "$I/bin/$a"; done
for t in "$NVME" "$XFS_REPAIR" "$XFS_DB"; do
    cp "$t" "$I/bin/"
    ldd "$t" | grep -o '/[^ ]*' | while read -r lib; do
        mkdir -p "$I$(dirname "$lib")"; cp -L "$lib" "$I$lib"
    done
done
: > "$I/lib/mods/order"
for m in virtio_net nvme-tcp xfs; do
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
echo "  modules: $(tr '\n' ' ' < "$I/lib/mods/order")"
cat > "$I/env" <<EOF
PORT=$PORT
SUB=$SUB
H1=$H1
NS_G=$NS_G
NS_A=$NS_A
NS_B=$NS_B
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
echo "GUEST kernel $(cat /proc/sys/kernel/osrelease)"
grep -q xfs /proc/filesystems || { r xfs-module FAIL; poweroff -f; }
nvme connect -t tcp -a 10.0.2.2 -s "$PORT" -n "$SUB" --hostnqn "$H1" >/tmp/o 2>&1 || { r connect "FAIL ($(head -1 /tmp/o))"; poweroff -f; }
sleep 2
# The namespace's block device, not a multipath path node (nvme0c0n1: hidden,
# no /dev entry) — native NVMe multipath lists both in /sys/block.
dev_of() { for b in /sys/block/nvme*n*; do case "${b##*/}" in nvme*c*n*) continue ;; esac; [ "$(cat $b/nsid 2>/dev/null)" = "$1" ] && basename $b && return; done; }
g=$(dev_of "$NS_G"); a=$(dev_of "$NS_A"); b=$(dev_of "$NS_B")
echo "GUEST blank /dev/$g, claim a /dev/$a, claim b /dev/$b"
uuid() { xfs_db -r -c "sb 0" -c "p uuid" "/dev/$1" 2>/dev/null | awk '{print $3}'; }
meta() { xfs_db -r -c "sb 0" -c "p meta_uuid" "/dev/$1" 2>/dev/null | awk '{print $3}'; }
echo "GUEST uuids: blank $(uuid $g), a $(uuid $a) (meta $(meta $a)), b $(uuid $b) (meta $(meta $b))"
echo "UUIDS $(uuid $g) $(uuid $a) $(uuid $b) $(meta $a) $(meta $b)"

# All three mounted at once: the kernel refuses a second XFS with a UUID it
# already has mounted, so this is what the restamp is for.
mount -t xfs -o ro "/dev/$g" /mnt/g 2>/tmp/eg && r mount-blank-ro PASS || r mount-blank-ro "FAIL ($(cat /tmp/eg))"
mount -t xfs "/dev/$a" /mnt/a 2>/tmp/ea && r mount-claim-a PASS || r mount-claim-a "FAIL ($(cat /tmp/ea))"
mount -t xfs "/dev/$b" /mnt/b 2>/tmp/eb && r mount-claim-b PASS || r mount-claim-b "FAIL ($(cat /tmp/eb))"
[ "$(grep -c ' xfs ' /proc/mounts)" = 3 ] && r three-mounted-at-once PASS || r three-mounted-at-once "FAIL ($(grep ' xfs ' /proc/mounts | wc -l))"
dd if=/dev/urandom of=/tmp/pat bs=1M count=8 2>/dev/null
cp /tmp/pat /mnt/a/file && cp /tmp/pat /mnt/b/file && mkdir -p /mnt/a/dir/deeper && echo hello > /mnt/a/dir/deeper/x \
    && sync && r write-both PASS || r write-both FAIL
touch /mnt/g/nope 2>/dev/null && r blank-refuses-writes FAIL || r blank-refuses-writes PASS
umount /mnt/a && umount /mnt/b && umount /mnt/g && r unmount-all PASS || r unmount-all FAIL
mount -t xfs "/dev/$a" /mnt/a && cmp -s /tmp/pat /mnt/a/file && [ "$(cat /mnt/a/dir/deeper/x)" = hello ] \
    && r remount-reads-back PASS || r remount-reads-back FAIL
umount /mnt/a
for d in "$g" "$a" "$b"; do
    xfs_repair -n "/dev/$d" >/tmp/rep 2>&1 && r "xfs-repair-$d" PASS || r "xfs-repair-$d" "FAIL ($(tail -3 /tmp/rep | tr '\n' ' '))"
done
bad=$(dmesg | grep -iE 'XFS.*(corrupt|error|metadata I/O|unmount and run xfs_repair)' | head -3)
[ -z "$bad" ] && r no-xfs-errors-in-dmesg PASS || r no-xfs-errors-in-dmesg "FAIL ($bad)"
echo "GUEST dmesg (xfs):"
dmesg | grep -i xfs | tail -12
nvme disconnect -n "$SUB" >/dev/null 2>&1
echo "GUEST done"
poweroff -f
EOF
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"

say "boot $KERNEL in QEMU"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 400 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 1536 -smp 2 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 panic=-1 loglevel=4" \
    -netdev user,id=n0 -device virtio-net-pci,netdev=n0 > "$W/guest.log" 2>&1
tr -d '\r' < "$W/guest.log" > "$W/g.txt"
grep -E '^(RESULT|GUEST)|XFS' "$W/g.txt" | tail -50
grep -q '^GUEST done' "$W/g.txt" || { tail -30 "$W/g.txt"; fail "the guest did not finish"; }

read -r ug ua ub ma mb < <(sed -n 's/^UUIDS //p' "$W/g.txt")
if [ -n "${ug:-}" ] && [ "$ug" != "$ua" ] && [ "$ug" != "$ub" ] && [ "$ua" != "$ub" ]; then
    echo "  ok    three distinct UUIDs"
else
    fail "UUIDs not distinct: blank '$ug' a '$ua' b '$ub'"
fi
[ "${ma:-}" = "$ug" ] && [ "${mb:-}" = "$ug" ] && echo "  ok    each claim's meta_uuid is the blank's (xfs_admin -U's shape)" \
    || fail "meta_uuid a '$ma' b '$mb', want the blank's '$ug'"
[ -z "$AU" ] || [ "$AU" = "$ua" ] && echo "  ok    claim a's UUID is the one the engine reported" || fail "engine said $AU, the disk says $ua"

results=$(grep -c '^RESULT ' "$W/g.txt")
bad=$(grep '^RESULT ' "$W/g.txt" | grep -vc ' PASS')
[ "$results" -ge 12 ] || fail "the guest reported $results results, expected 12"
[ "$bad" = 0 ] || fail "$bad guest check(s) failed"
if [ "$FAILS" = 0 ]; then echo "ALL PASS ($results guest checks)"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
