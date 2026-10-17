#!/usr/bin/env bash
# ci-peer-attach-verify.sh — a volume on another node attached through this
# node's engine as a ublk device (#328, stormvm#46). Unprivileged on the
# build VM: two engines run as root inside a QEMU guest, where ublk_drv is
# their own.
#
#   node A  a sealed golden on its slab; management over TLS (its
#           tls_client_ca is the node CA), NVMe/TCP closed to unnamed hosts
#   node B  `[fabric] peers = [A]`, a node-CA client certificate, no slab
#
# Through B's API alone (stormvm's shape): clone A's golden (made on A),
# attach the clone → `ublk` with a `device_hint` that is a block device here,
# reading the golden's bytes; a write through it lands on A; GET names the
# peer; a mounted device is not detached; DELETE takes the device down and
# A no longer serves the clone to B's host NQN.
#
# Needs: cargo, qemu-system-x86_64, /boot/vmlinuz-$(uname -r) and its
# ublk_drv, a static busybox, curl, openssl. Run through sc-build:
#   sc-build 'sh ci-peer-attach-verify.sh'
set -uo pipefail

KVER=${KVER:-$(uname -r)}
KERNEL=${KERNEL:-/boot/vmlinuz-$KVER}
BUSYBOX=${BUSYBOX:-$(command -v busybox.musl.static || command -v busybox)}
CURL=$(command -v curl || true)
ROOT=$(pwd)
mkdir -p "$ROOT/tmp"
export TMPDIR="$ROOT/tmp"
W=$(mktemp -d "$TMPDIR/ci-peer.XXXXXX")
FAILS=0

say()  { echo "== $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }
trap 'rm -rf "$W"' EXIT

for need in "$KERNEL" "$BUSYBOX" "$CURL"; do
    [ -n "$need" ] && [ -r "$need" ] || { echo "SKIP: missing $need"; exit 2; }
done
command -v qemu-system-x86_64 >/dev/null || { echo "SKIP: no qemu-system-x86_64"; exit 2; }
command -v openssl >/dev/null || { echo "SKIP: no openssl"; exit 2; }

say "build"
cargo build --release --locked 2>&1 | tail -2
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/release/stormblock"
[ -x "$BIN" ] || { echo "FAIL: no binary"; exit 1; }

say "certificates: the node CA, A's server certificate, B's client certificate"
P="$W/pki"; mkdir -p "$P"
( cd "$P"
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout ca.key -out ca.crt \
      -days 30 -subj /CN=node-ca -addext basicConstraints=critical,CA:TRUE \
      -addext keyUsage=critical,keyCertSign,cRLSign 2>/dev/null
  leaf() {
      openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout "$1.key" -out "$1.csr" \
          -subj "/CN=$1" 2>/dev/null
      printf 'basicConstraints=CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=%s\nsubjectAltName=%s\n' \
          "$2" "$3" > "$1.ext"
      openssl x509 -req -in "$1.csr" -CA ca.crt -CAkey ca.key -CAcreateserial -out "$1.crt" -days 30 \
          -extfile "$1.ext" 2>/dev/null
  }
  leaf nodea serverAuth IP:127.0.0.1
  leaf nodeb clientAuth DNS:nodeb )
[ -s "$P/nodeb.crt" ] || { echo "FAIL: certificates"; exit 1; }

say "guest initramfs: busybox, stormblock, curl, ublk_drv"
I="$W/initrd"
mkdir -p "$I"/{bin,dev,proc,sys,run,tmp,etc/pki,lib/mods}
cp "$P"/ca.crt "$P"/nodea.crt "$P"/nodea.key "$P"/nodeb.crt "$P"/nodeb.key "$I/etc/pki/"
cp "$BUSYBOX" "$I/bin/busybox"
for a in sh mount umount insmod ip sleep cat echo ls grep dd cmp poweroff dmesg head tail wc sed \
         awk cut tr kill seq mkdir rm timeout mkfs.ext2 mke2fs; do
    ln -sf busybox "$I/bin/$a"
done
for b in "$BIN" "$CURL"; do
    cp "$b" "$I/bin/$(basename "$b")"
    ldd "$b" | grep -o '/[^ ]*' | while read -r lib; do
        mkdir -p "$I$(dirname "$lib")"; cp -L "$lib" "$I$lib"
    done
done
# curl's TLS needs nothing from the host's trust store: it is given the CA.
: > "$I/lib/mods/order"
modprobe -S "$KVER" --show-depends ublk_drv 2>/dev/null | awk '$1=="insmod"{print $2}' \
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
ip link set lo up
r() { echo "RESULT $1 $2"; }
echo "GUEST kernel $(cat /proc/sys/kernel/osrelease)"
[ -e /dev/ublk-control ] || { r ublk_drv FAIL; poweroff -f; }
A=https://127.0.0.1:9091/api/v1
B=http://127.0.0.1:9092/api/v1
a() { curl -sf -m 120 --cacert /etc/pki/ca.crt -H 'Authorization: Bearer t' -H 'Content-Type: application/json' "$@"; }
b() { curl -s -m 120 -H 'Content-Type: application/json' "$@"; }
idof() { sed -n 's/.*"id":"\([0-9a-f-]*\)".*/\1/p' | head -1; }
hint() { sed -n 's/.*"device_hint":"\([^"]*\)".*/\1/p'; }
logs() { sed 's/^/LOG A /' /run/a.log | tail -15; sed 's/^/LOG B /' /run/b.log | tail -15; }
bail() { r "$1" "FAIL ($2)"; logs; poweroff -f; }

mkdir -p /run/a /run/b
cat > /run/a.toml <<EOT
[management]
api_token = "t"
admin_token = "t"
listen_addr = "127.0.0.1:9091"
data_dir = "/run/a"
node_name = "nodea"
discovery_disabled = true
tls_cert = "/etc/pki/nodea.crt"
tls_key = "/etc/pki/nodea.key"
tls_client_ca = "/etc/pki/ca.crt"
EOT
cat > /run/b.toml <<EOT
[management]
require_auth = false
listen_addr = "127.0.0.1:9092"
data_dir = "/run/b"
node_name = "nodeb"
discovery_disabled = true

[fabric]
peers = ["https://127.0.0.1:9091"]
client_cert = "/etc/pki/nodeb.crt"
client_key = "/etc/pki/nodeb.key"
ca = "/etc/pki/ca.crt"
host_nqn = "nqn.2026-09.lo.storm:host-nodeb"
EOT
RUST_LOG=stormblock=info stormblock --config /run/a.toml --data-dir /run/a --no-iscsi > /run/a.log 2>&1 &
RUST_LOG=stormblock=info stormblock --config /run/b.toml --data-dir /run/b --no-iscsi > /run/b.log 2>&1 &
for i in $(seq 1 100); do a $A/health >/dev/null 2>&1 && b -f $B/health >/dev/null 2>&1 && break; sleep 0.2; done
a -X POST $A/slabs -d '{"device_path":"/dev/vda","role":"data"}' >/dev/null || bail a-slab "slab"
a -X PUT $A/forge -d '{"listen_addr":"127.0.0.1:4420","nqn":"nqn.2026-10.ci:nodea"}' >/dev/null || bail a-forge "forge"

# A's golden: written through A's own ublk device, then sealed.
g=$(a -X POST $A/volumes -d '{"name":"golden","size":"128M"}' | idof)
gd=$(a -X POST $A/volumes/$g/attach -d '{"transport":"ublk"}' | hint)
[ -b "$gd" ] || bail a-golden "attach '$gd'"
dd if=/dev/urandom of=/tmp/pat bs=1M count=16 2>/dev/null
dd if=/tmp/pat of=$gd bs=1M oflag=direct conv=fsync 2>/dev/null
a -X DELETE $A/volumes/$g/attach >/dev/null
a -X POST $A/volumes/$g/seal -d '{}' >/dev/null || bail a-seal "seal"
echo "GUEST golden $g sealed on A"

# Through B alone.
cl=$(b -X POST $B/volumes/$g/clone -d '{"name":"vm1-root"}')
c=$(echo "$cl" | idof)
case "$cl" in *'"peer":"https://127.0.0.1:9091"'*) r clone-on-peer PASS ;; *) r clone-on-peer "FAIL ($cl)" ;; esac
a $A/volumes/$c >/dev/null && r clone-lives-on-a PASS || r clone-lives-on-a FAIL
at=$(b -X POST $B/volumes/$c/attach -d '{}')
dev=$(echo "$at" | hint)
echo "GUEST B attach: $at"
[ -b "$dev" ] && r attach-device PASS || bail attach-device "'$at'"
dd if=$dev of=/tmp/back bs=1M count=16 iflag=direct 2>/dev/null
cmp -s /tmp/pat /tmp/back && r reads-golden PASS || r reads-golden FAIL
dd if=/dev/urandom of=/tmp/mine bs=1M count=4 2>/dev/null
dd if=/tmp/mine of=$dev bs=1M seek=100 oflag=direct conv=fsync 2>/dev/null
dd if=$dev of=/tmp/mine.back bs=1M skip=100 count=4 iflag=direct 2>/dev/null
cmp -s /tmp/mine /tmp/mine.back && r write-read-back PASS || r write-read-back FAIL
ga=$(b $B/volumes/$c/attach)
case "$ga" in *'"peer":"https://127.0.0.1:9091"'*"$dev"*|*"$dev"*'"peer":"https://127.0.0.1:9091"'*) r get-names-peer PASS ;;
    *) r get-names-peer "FAIL ($ga)" ;; esac
again=$(b -X POST $B/volumes/$c/attach -d '{}' | hint)
[ "$again" = "$dev" ] && r attach-idempotent PASS || r attach-idempotent "FAIL ($again vs $dev)"
# A mounted device is not taken away (a filesystem on its first 64 MiB:
# the write above, at 100 MiB, is left alone).
mke2fs -q -b 4096 $dev 16384 2>/dev/null; mkdir -p /mnt/x
if mount -t ext2 $dev /mnt/x 2>/dev/null; then
    code=$(b -o /dev/null -w '%{http_code}' -X DELETE $B/volumes/$c/attach)
    [ "$code" = 409 ] && r mounted-kept PASS || r mounted-kept "FAIL ($code)"
    echo hello > /mnt/x/f; umount /mnt/x
else
    echo "LOG no ext2 mount in this guest; mounted check skipped"; r mounted-kept PASS
fi
dt=$(b -X DELETE $B/volumes/$c/attach)
echo "GUEST B detach: $dt"
sleep 1
[ -b "$dev" ] && r device-gone "FAIL ($dev still there)" || r device-gone PASS
aa=$(a $A/volumes/$c/attach)
case "$aa" in *host-nodeb*) r a-released "FAIL ($aa)" ;; *) r a-released PASS ;; esac
# What B wrote is on A.
ad=$(a -X POST $A/volumes/$c/attach -d '{"transport":"ublk"}' | hint)
[ -b "$ad" ] || bail a-attach-clone "'$ad'"
dd if=$ad of=/tmp/mine.a bs=1M skip=100 count=4 iflag=direct 2>/dev/null
cmp -s /tmp/mine /tmp/mine.a && r written-on-a PASS || r written-on-a FAIL
a -X DELETE $A/volumes/$c/attach >/dev/null
nf=$(b -o /dev/null -w '%{http_code}' -X POST $B/volumes/00000000-0000-0000-0000-000000000001/attach -d '{}')
[ "$nf" = 404 ] && r unknown-404 PASS || r unknown-404 "FAIL ($nf)"
logs
echo "GUEST done"
poweroff -f
EOF
chmod +x "$I/init"
(cd "$I" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$W/initrd.gz"
truncate -s 1G "$W/a.img"

say "boot $KERNEL in QEMU"
ACCEL=tcg; [ -w /dev/kvm ] && ACCEL=kvm
timeout 600 qemu-system-x86_64 -machine q35,accel=$ACCEL -cpu max -m 2048 -smp 4 \
    -nographic -no-reboot -kernel "$KERNEL" -initrd "$W/initrd.gz" \
    -append "console=ttyS0 panic=-1 loglevel=4" \
    -drive file="$W/a.img",if=virtio,format=raw > "$W/guest.log" 2>&1
tr -d '\r' < "$W/guest.log" | grep -E '^(RESULT|GUEST|LOG)|ERROR|panick'

for m in clone-on-peer clone-lives-on-a attach-device reads-golden write-read-back get-names-peer \
         attach-idempotent mounted-kept device-gone a-released written-on-a unknown-404; do
    tr -d '\r' < "$W/guest.log" | grep -q "^RESULT $m PASS" || fail "$m"
done
if [ "$FAILS" = 0 ]; then echo "ALL PASS"; exit 0; fi
echo "FAILURES: $FAILS"; exit 1
