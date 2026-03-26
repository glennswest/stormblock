#!/bin/bash
# install-fedora-iscsi.sh — Install bootable Fedora on iSCSI via StormBlock
#
# mkube CI job script. Provisions an iSCSI-backed partitioned disk using
# StormBlock's slab extent store, formats filesystems, installs Fedora via
# dnf --installroot, configures for LinuxBoot-style boot (no GRUB, no dracut).
#
# 8 phases:
#   1. Build StormBlock
#   2. Provision iSCSI + ublk devices
#   3. Format filesystems
#   4. Install Fedora
#   5. Configure for StormBlock boot
#   6. Build stormblock-initramfs
#   7. Verify
#   8. Cleanup
#
# Usage (in mkube job):
#   ./install-fedora-iscsi.sh [portal] [iqn]
#
# Defaults:
#   portal = 192.168.10.1
#   iqn    = iqn.2000-02.com.mikrotik:fedora-boot

set -euo pipefail

PORTAL="${1:-192.168.10.1}"
IQN="${2:-iqn.2000-02.com.mikrotik:fedora-boot}"
PORT="${3:-3260}"
LAYOUT="esp:256M,boot:512M,root:7G,swap:1G,home:rest"
FEDORA_RELEASE="${FEDORA_RELEASE:-41}"
MNT="/mnt"
STORMBLOCK_PID=""

cleanup() {
    echo "=== Phase 8: Cleanup ==="

    # Unmount filesystems
    umount -R "$MNT" 2>/dev/null || true

    # Stop stormblock
    if [ -n "$STORMBLOCK_PID" ] && kill -0 "$STORMBLOCK_PID" 2>/dev/null; then
        echo "Stopping StormBlock (PID $STORMBLOCK_PID)..."
        kill "$STORMBLOCK_PID" 2>/dev/null
        wait "$STORMBLOCK_PID" 2>/dev/null || true
    fi

    echo "Cleanup complete."
}
trap cleanup EXIT

echo "============================================"
echo " StormBlock Fedora iSCSI Boot Installer"
echo "============================================"
echo "Portal:  $PORTAL:$PORT"
echo "IQN:     $IQN"
echo "Layout:  $LAYOUT"
echo "Fedora:  $FEDORA_RELEASE"
echo ""

# ============================================================
# Phase 1: Build StormBlock
# ============================================================
echo "=== Phase 1: Build StormBlock ==="

cd /build
if [ ! -f target/release/stormblock ]; then
    echo "Building stormblock..."
    cargo build --release 2>&1
    echo "Build complete: $(du -h target/release/stormblock | cut -f1)"
else
    echo "Using existing build: $(du -h target/release/stormblock | cut -f1)"
fi

STORMBLOCK_BIN="$(pwd)/target/release/stormblock"

# ============================================================
# Phase 2: Provision iSCSI + ublk
# ============================================================
echo ""
echo "=== Phase 2: Provision iSCSI + ublk ==="

# Load ublk driver
if ! lsmod | grep -q ublk_drv; then
    modprobe ublk_drv 2>/dev/null || {
        echo "modprobe failed — installing kernel modules for host kernel..."
        KVER=$(uname -r)
        dnf install -y "kernel-modules-$KVER" 2>&1 | tail -5 || \
            dnf install -y kernel-modules 2>&1 | tail -5 || true
        depmod -a 2>/dev/null || true
        modprobe ublk_drv || {
            # Try direct insmod as last resort
            UBLK_KO=$(find /lib/modules/ -name 'ublk_drv.ko*' 2>/dev/null | head -1)
            if [ -n "$UBLK_KO" ]; then
                insmod "$UBLK_KO" || { echo "FATAL: Failed to load ublk_drv module"; exit 1; }
            else
                echo "FATAL: ublk_drv.ko not found anywhere"; exit 1
            fi
        }
    }
fi
echo "ublk_drv module loaded"

# Start stormblock in background
echo "Starting StormBlock boot-iscsi with ublk export..."
"$STORMBLOCK_BIN" boot-iscsi \
    --portal "$PORTAL" --port "$PORT" \
    --iqn "$IQN" --layout "$LAYOUT" --ublk &
STORMBLOCK_PID=$!

# Wait for all 5 ublk devices
echo "Waiting for ublk devices..."
TIMEOUT=60
for dev_idx in 0 1 2 3 4; do
    while [ ! -b "/dev/ublkb${dev_idx}" ] && [ $TIMEOUT -gt 0 ]; do
        sleep 1
        TIMEOUT=$((TIMEOUT - 1))
    done
    if [ ! -b "/dev/ublkb${dev_idx}" ]; then
        echo "FATAL: /dev/ublkb${dev_idx} not found after timeout"
        exit 1
    fi
    echo "  /dev/ublkb${dev_idx} ready"
done
echo "All ublk devices ready."

# ============================================================
# Phase 3: Format filesystems
# ============================================================
echo ""
echo "=== Phase 3: Format filesystems ==="

echo "Formatting ESP (vfat)..."
mkfs.vfat -F 32 -n ESP /dev/ublkb0

echo "Formatting boot (ext4)..."
mkfs.ext4 -L boot -q /dev/ublkb1

echo "Formatting root (ext4)..."
mkfs.ext4 -L root -q /dev/ublkb2

echo "Formatting swap..."
mkswap -L swap /dev/ublkb3

echo "Formatting home (ext4)..."
mkfs.ext4 -L home -q /dev/ublkb4

echo "All filesystems formatted."

# ============================================================
# Phase 4: Install Fedora
# ============================================================
echo ""
echo "=== Phase 4: Install Fedora ==="

# Mount all under /mnt
mkdir -p "$MNT"
mount /dev/ublkb2 "$MNT"
mkdir -p "$MNT"/{boot,home}
mount /dev/ublkb1 "$MNT/boot"
mkdir -p "$MNT/boot/efi"
mount /dev/ublkb0 "$MNT/boot/efi"
mount /dev/ublkb4 "$MNT/home"
echo "Filesystems mounted at $MNT"

# Install Fedora minimal
echo "Installing Fedora $FEDORA_RELEASE (this takes a few minutes)..."
dnf --installroot="$MNT" --releasever="$FEDORA_RELEASE" -y \
    --setopt=install_weak_deps=False \
    groupinstall "Minimal Install" 2>&1 | tail -5

# Install additional packages
echo "Installing additional packages..."
dnf --installroot="$MNT" --releasever="$FEDORA_RELEASE" -y \
    --setopt=install_weak_deps=False \
    install \
    kernel \
    systemd \
    NetworkManager \
    passwd \
    rootfiles \
    vim-minimal \
    less \
    iproute \
    iputils \
    2>&1 | tail -5

# Copy stormblock binary
echo "Installing stormblock binary..."
mkdir -p "$MNT/usr/sbin"
cp "$STORMBLOCK_BIN" "$MNT/usr/sbin/stormblock"
chmod 755 "$MNT/usr/sbin/stormblock"

echo "Fedora installation complete."

# ============================================================
# Phase 5: Configure for StormBlock boot
# ============================================================
echo ""
echo "=== Phase 5: Configure for StormBlock boot ==="

# Write /etc/fstab with ublk device entries
cat > "$MNT/etc/fstab" << 'FSTAB'
# StormBlock ublk block devices (iSCSI-backed)
# Device         Mount       Type   Options        Dump Pass
/dev/ublkb2      /           ext4   defaults       0    1
/dev/ublkb1      /boot       ext4   defaults       0    2
/dev/ublkb0      /boot/efi   vfat   umask=0077     0    2
/dev/ublkb3      swap        swap   defaults       0    0
/dev/ublkb4      /home       ext4   defaults       0    2
FSTAB
echo "Wrote /etc/fstab"

# Write boot environment file
mkdir -p "$MNT/etc/stormblock"
cat > "$MNT/etc/stormblock/boot.env" << BOOTENV
PORTAL=$PORTAL
IQN=$IQN
LAYOUT=$LAYOUT
PORT=$PORT
BOOTENV
echo "Wrote /etc/stormblock/boot.env"

# Install systemd service
mkdir -p "$MNT/etc/systemd/system"
cp "$(dirname "$0")/systemd/stormblock-ublk.service" \
    "$MNT/etc/systemd/system/stormblock-ublk.service" 2>/dev/null \
    || cp /build/systemd/stormblock-ublk.service \
    "$MNT/etc/systemd/system/stormblock-ublk.service"
# Enable the service
mkdir -p "$MNT/etc/systemd/system/multi-user.target.wants"
ln -sf /etc/systemd/system/stormblock-ublk.service \
    "$MNT/etc/systemd/system/multi-user.target.wants/stormblock-ublk.service"
echo "Installed stormblock-ublk.service (enabled)"

# Set root password (changeme)
echo "root:changeme" | chroot "$MNT" chpasswd 2>/dev/null || \
    echo "root:changeme" | chpasswd -R "$MNT" 2>/dev/null || \
    echo "WARNING: Could not set root password"

# Set hostname
echo "stormblock-boot" > "$MNT/etc/hostname"

# Disable selinux for first boot (relabel takes forever on ublk)
if [ -f "$MNT/etc/selinux/config" ]; then
    sed -i 's/^SELINUX=.*/SELINUX=disabled/' "$MNT/etc/selinux/config"
    echo "SELinux disabled"
fi

# Enable serial console for headless boot
mkdir -p "$MNT/etc/systemd/system/getty.target.wants"
ln -sf /usr/lib/systemd/system/serial-getty@.service \
    "$MNT/etc/systemd/system/getty.target.wants/serial-getty@ttyS0.service" 2>/dev/null || true

echo "System configuration complete."

# ============================================================
# Phase 6: Build stormblock-initramfs
# ============================================================
echo ""
echo "=== Phase 6: Build stormblock-initramfs ==="

# Find the installed kernel version
KVER=$(ls "$MNT/lib/modules/" | sort -V | tail -1)
VMLINUZ=$(find "$MNT/boot" -name "vmlinuz-*" | sort -V | tail -1)

if [ -z "$KVER" ]; then
    echo "WARNING: No kernel modules found, using host kernel version"
    KVER=$(uname -r)
fi

if [ -z "$VMLINUZ" ]; then
    echo "WARNING: No vmlinuz found in $MNT/boot"
    VMLINUZ=""
fi

echo "Kernel version: $KVER"
echo "vmlinuz: $VMLINUZ"

# Build the initramfs
if [ -f /build/scripts/build-stormblock-initramfs.sh ]; then
    bash /build/scripts/build-stormblock-initramfs.sh \
        "$STORMBLOCK_BIN" "$KVER" /tmp/stormblock-initramfs.img
else
    echo "WARNING: build-stormblock-initramfs.sh not found, skipping initramfs build"
fi

# Stage boot files for TFTP/HTTP serving
STAGE="/tmp/stormblock-boot"
mkdir -p "$STAGE"
if [ -n "$VMLINUZ" ]; then
    cp "$VMLINUZ" "$STAGE/vmlinuz"
    echo "Staged: $STAGE/vmlinuz ($(du -h "$STAGE/vmlinuz" | cut -f1))"
fi
if [ -f /tmp/stormblock-initramfs.img ]; then
    cp /tmp/stormblock-initramfs.img "$STAGE/stormblock-initramfs.img"
    echo "Staged: $STAGE/stormblock-initramfs.img ($(du -h "$STAGE/stormblock-initramfs.img" | cut -f1))"
fi

# Write iPXE script
cat > "$STAGE/boot.ipxe" << IPXE
#!ipxe
# StormBlock LinuxBoot iPXE script
# Serve vmlinuz and initramfs via HTTP, then kexec into them.

kernel http://\${next-server}/stormblock-boot/vmlinuz \\
    rd.stormblock.portal=$PORTAL \\
    rd.stormblock.iqn=$IQN \\
    rd.stormblock.layout=$LAYOUT \\
    rd.stormblock.port=$PORT \\
    console=ttyS0,115200 console=tty0

initrd http://\${next-server}/stormblock-boot/stormblock-initramfs.img

boot
IPXE
echo "Staged: $STAGE/boot.ipxe"

echo "Boot files staged in $STAGE"

# ============================================================
# Phase 7: Verify
# ============================================================
echo ""
echo "=== Phase 7: Verify ==="

ERRORS=0

# Check key files exist
for f in \
    "$MNT/usr/sbin/stormblock" \
    "$MNT/etc/fstab" \
    "$MNT/etc/stormblock/boot.env" \
    "$MNT/etc/systemd/system/stormblock-ublk.service" \
    "$MNT/etc/hostname"; do
    if [ -f "$f" ]; then
        echo "  OK: $f"
    else
        echo "  FAIL: $f not found"
        ERRORS=$((ERRORS + 1))
    fi
done

# Check stormblock binary is executable
if [ -x "$MNT/usr/sbin/stormblock" ]; then
    echo "  OK: stormblock is executable"
else
    echo "  FAIL: stormblock is not executable"
    ERRORS=$((ERRORS + 1))
fi

# Check vmlinuz exists
if [ -n "$VMLINUZ" ] && [ -f "$VMLINUZ" ]; then
    echo "  OK: vmlinuz exists ($(du -h "$VMLINUZ" | cut -f1))"
else
    echo "  WARN: vmlinuz not found"
fi

# Check initramfs
if [ -f /tmp/stormblock-initramfs.img ]; then
    echo "  OK: stormblock-initramfs.img ($(du -h /tmp/stormblock-initramfs.img | cut -f1))"
else
    echo "  WARN: stormblock-initramfs.img not built"
fi

# Check fstab has correct entries
if grep -q ublkb2 "$MNT/etc/fstab"; then
    echo "  OK: fstab has ublk root entry"
else
    echo "  FAIL: fstab missing ublk root entry"
    ERRORS=$((ERRORS + 1))
fi

# Check systemd service is enabled
if [ -L "$MNT/etc/systemd/system/multi-user.target.wants/stormblock-ublk.service" ]; then
    echo "  OK: stormblock-ublk.service enabled"
else
    echo "  FAIL: stormblock-ublk.service not enabled"
    ERRORS=$((ERRORS + 1))
fi

# Verify stormblock runs inside chroot
if chroot "$MNT" /usr/sbin/stormblock --version 2>/dev/null; then
    echo "  OK: stormblock runs in chroot"
else
    echo "  WARN: stormblock --version failed in chroot (may need static build)"
fi

echo ""
if [ $ERRORS -eq 0 ]; then
    echo "All checks passed."
else
    echo "$ERRORS check(s) FAILED."
fi

echo ""
echo "============================================"
echo " Installation Complete"
echo "============================================"
echo ""
echo "To boot this system via iPXE, serve the files from $STAGE via HTTP"
echo "and use the following iPXE script:"
echo ""
echo "  kernel http://server/stormblock-boot/vmlinuz \\"
echo "    rd.stormblock.portal=$PORTAL \\"
echo "    rd.stormblock.iqn=$IQN \\"
echo "    rd.stormblock.layout=$LAYOUT"
echo "  initrd http://server/stormblock-boot/stormblock-initramfs.img"
echo "  boot"
echo ""

# Phase 8 runs via trap
