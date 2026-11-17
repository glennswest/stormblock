#!/bin/bash
# Build this repo's initramfs for a Fedora kernel the build host does not run
# (#630: a kernel move, 7.3.3-200.fc44 -> 7.3.3-300.fc45), from Fedora's own
# signed packages, and show what it carries of the boot path.
#
#   sc-build 'bash ci-kernel-initramfs.sh 7.3.3-300.fc45'
#
# The release's initramfs is built by stormcentral (`component initramfs`)
# from the release kernel's modules image; this is the same script against the
# same kernel, run where a session can read every line of it: a module the
# image needs that the new kernel dropped or renamed fails here, by name
# (`boot_path_missing`, the dependency closure), not as a node with no root.
#
# Needs: cargo with the musl target, dnf, rpm2cpio, cpio, depmod, rpmkeys, and
# Fedora's key for the kernel's release (fedora-gpg-keys). No root.
set -euo pipefail

NVR=${1:?usage: ci-kernel-initramfs.sh <kernel NVR, e.g. 7.3.3-300.fc45>}
ARCH=x86_64
case "$NVR" in *."$ARCH") KVER=$NVR ;; *) KVER=$NVR.$ARCH ;; esac
VER=${KVER%.$ARCH}                       # 7.3.3-300.fc45
FC=$(printf '%s' "$VER" | sed -n 's/.*\.fc\([0-9][0-9]*\)$/\1/p')
[ -n "$FC" ] || { echo "ERROR: $NVR names no Fedora release"; exit 1; }
KEY=/etc/pki/rpm-gpg/RPM-GPG-KEY-fedora-$FC-$ARCH
[ -r "$KEY" ] || { echo "ERROR: no Fedora $FC key at $KEY (fedora-gpg-keys)"; exit 1; }

W=$(mktemp -d)
trap 'chmod -R u+rwX "$W" 2>/dev/null; rm -rf "$W"' EXIT
mkdir -p "$W/rpm" "$W/root" "$W/keys" "$W/dnf"

echo "== the stormblock binary (musl)"
cargo build --locked --release --target x86_64-unknown-linux-musl --bin stormblock 2>&1 | tail -2
BIN="${CARGO_TARGET_DIR:-target}/x86_64-unknown-linux-musl/release/stormblock"

echo "== Fedora $FC's kernel $KVER"
PKGS="kernel-core-$VER kernel-modules-core-$VER kernel-modules-$VER"
# shellcheck disable=SC2086
if ! ( cd "$W/rpm" && dnf -q --releasever="$FC" --forcearch="$ARCH" \
        --setopt=cachedir="$W/dnf/cache" --setopt=persistdir="$W/dnf/persist" \
        --setopt=logdir="$W/dnf/log" --setopt=countme=0 \
        --enablerepo=updates-testing download $PKGS ) > "$W/dnf.out" 2>&1; then
    echo "   not in Fedora $FC's repos ($(tail -1 "$W/dnf.out")): from Koji"
    rm -f "$W"/rpm/*.rpm
    v=${VER%%-*}; r=${VER#*-}
    for p in kernel-core kernel-modules-core kernel-modules; do
        curl -fsSL -o "$W/rpm/$p-$VER.$ARCH.rpm" \
            "https://kojipkgs.fedoraproject.org/packages/kernel/$v/$r/$ARCH/$p-$VER.$ARCH.rpm"
    done
fi
rpmkeys --dbpath "$W/keys" --import "$KEY"
if rpmkeys --dbpath "$W/keys" -K "$W"/rpm/*.rpm | grep -v 'digests signatures OK' | grep .; then
    echo "ERROR: a package above is not signed by Fedora $FC's key"
    exit 1
fi
for f in "$W"/rpm/*.rpm; do
    printf '   %s  %s\n' "$(sha256sum "$f" | cut -c1-16)" "${f##*/}"
    ( cd "$W/root" && rpm2cpio "$f" | cpio -idm --quiet --no-absolute-filenames )
done
# Fedora's packages put modules under usr/lib/modules; the script reads
# $MODROOT/lib/modules, and modules.dep is made at install time: here.
MODROOT=$W/root
[ -d "$MODROOT/lib/modules/$KVER" ] || MODROOT=$W/root/usr
[ -d "$MODROOT/lib/modules/$KVER" ] || { echo "ERROR: no lib/modules/$KVER in the packages"; exit 1; }
depmod -b "$MODROOT" "$KVER"

echo "== the boot path in $KVER"
for m in block/ublk_drv nvme/host/nvme-tcp nvme/host/nvme nvme/host/nvme-fabrics nvme/host/nvme-core \
         scsi/sd_mod scsi/mpt3sas/mpt3sas virtio/virtio_pci block/virtio_blk \
         acpi/button input/evdev gpu/drm/tiny/simpledrm; do
    f=$(ls "$MODROOT/lib/modules/$KVER/kernel/drivers/$m".ko* 2>/dev/null | head -1 || true)
    if [ -n "$f" ]; then echo "   $m: module"
    elif grep -q "kernel/drivers/$m.ko" "$MODROOT/lib/modules/$KVER/modules.builtin" 2>/dev/null; then echo "   $m: built in"
    else echo "   $m: ABSENT"; fi
done

echo "== the initramfs for $KVER"
rc=0
MODROOT=$MODROOT bash scripts/build-stormblock-initramfs.sh "$BIN" "$KVER" "$W/initramfs.img" > "$W/build.out" 2>&1 || rc=$?
grep -E '^  (kernel|modules|boot path|arch):|ERROR|MISSING|WARN' "$W/build.out" || true
if [ "$rc" != 0 ] || [ ! -s "$W/initramfs.img" ]; then
    tail -20 "$W/build.out"
    echo "ERROR: the initramfs for $KVER failed (exit $rc)"
    exit 1
fi
echo "== built for $KVER: $(du -h "$W/initramfs.img" | cut -f1), sha256 $(sha256sum "$W/initramfs.img" | cut -c1-16)"
