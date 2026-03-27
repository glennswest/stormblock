# LinuxBoot + StormBlock iSCSI — Firmware-Level Network Boot

## Overview

Replace the entire UEFI/iPXE/PXE stack with LinuxBoot firmware that has StormBlock built in. The server boots directly from SPI flash into a Linux kernel that connects to iSCSI, creates ublk block devices, and either:

- **Single-stage**: mounts root and `switch_root` directly (firmware kernel = production kernel)
- **Two-stage**: reads vmlinuz from `/boot` on the iSCSI disk, kexec into it (firmware kernel is a loader only)

No PXE. No iPXE. No DHCP boot chain. No GRUB. No dracut. Power on → Fedora login in seconds.

## Architecture

### Current Boot Chain (5 stages)

```
UEFI/BIOS → PXE ROM → DHCP → iPXE → HTTP (vmlinuz + initramfs)
  → kernel boots → /init → stormblock → iSCSI → ublk → mount → switch_root
  → Fedora systemd
```

~30-60 seconds. Depends on DHCP, HTTP server, iPXE firmware, PXE ROM.

### LinuxBoot Single-Stage (2 stages)

```
coreboot → Linux kernel + stormblock-initramfs (from SPI flash)
  → /init → stormblock → iSCSI → ublk → mount → switch_root
  → Fedora systemd
```

~5-10 seconds. No network boot infrastructure needed. The initramfs we already built (4.4M) works as-is.

### LinuxBoot Two-Stage (3 stages)

```
coreboot → loader kernel + stormblock-initramfs (from SPI flash)
  → /init → stormblock → iSCSI → ublk → mount /boot
  → kexec vmlinuz + production initramfs from /boot
  → /init → stormblock → iSCSI → ublk → mount root → switch_root
  → Fedora systemd
```

~10-15 seconds. Allows kernel updates from /boot without reflashing firmware. iSCSI reconnects after kexec (~1s penalty).

## Why Single-Stage Is Better

In single-stage, the firmware kernel IS the production kernel. No kexec, no reconnection, no second stormblock instance. This works because:

- The kernel on SPI flash can be the same kernel installed in /boot
- Kernel updates require a firmware reflash, but that's a `flashrom` command (< 1 minute)
- The stormblock initramfs is only 4.4M — fits easily in a 32M SPI flash alongside a compressed kernel
- No redundant iSCSI reconnection

Two-stage is useful when kernel updates must happen without physical access to the machine (remote kernel updates via /boot on iSCSI).

## Firmware Image Layout (SPI Flash)

Typical server SPI flash: 16-32 MB.

```
┌─────────────────────────────────────┐
│ coreboot (1-2 MB)                   │  ROM bootblock + ramstage
├─────────────────────────────────────┤
│ Linux kernel, compressed (8-10 MB)  │  bzImage, same as /boot/vmlinuz
├─────────────────────────────────────┤
│ stormblock-initramfs (4-5 MB)       │  busybox + stormblock + /init
├─────────────────────────────────────┤
│ NVRAM / config (64 KB)              │  iSCSI portal, IQN, layout
├─────────────────────────────────────┤
│ Fallback / recovery (remaining)     │  minimal shell for rescue
└─────────────────────────────────────┘
```

Total: ~15 MB. Fits in a 16 MB SPI flash with room to spare.

## Configuration Storage

The iSCSI parameters need to survive reboots without a network dependency.

### Option A: NVRAM (preferred)

Store in CMOS/NVRAM alongside coreboot settings:

```
stormblock.portal=192.168.10.1
stormblock.iqn=iqn.2000-02.com.mikrotik:fedora-boot
stormblock.layout=esp:256M,boot:512M,root:7G,swap:1G,home:rest
stormblock.port=3260
```

Read from `/init` via `nvramtool` or by reading coreboot tables from `/sys/firmware/coreboot/`.

### Option B: Embedded in initramfs

Bake the config into the initramfs at build time:

```
/etc/stormblock/boot.env
```

Simple, but requires rebuilding the initramfs to change targets. Fine for dedicated appliances.

### Option C: Kernel command line (in coreboot)

coreboot passes the command line to the payload kernel:

```
rd.stormblock.portal=192.168.10.1 rd.stormblock.iqn=... rd.stormblock.layout=...
```

The existing `/init` script already parses these. No changes needed.

## Hardware Requirements

### Supermicro SYS-5037MR-H8TRF (server2, server3, etc.)

| Component | Status | Notes |
|-----------|--------|-------|
| SPI flash | 16 MB | Standard W25Q128 or equivalent |
| coreboot support | Partial | X9 boards have community coreboot ports |
| BMC/IPMI | Separate | AST2300, not affected by SPI reflash |
| Network | Intel I210/I350 | Linux driver: igb, works in initramfs |
| SPI programmer | Needed | Pomona clip + flashrom for first flash |

### First Flash vs Updates

| Method | When | Tool |
|--------|------|------|
| External programmer | First flash (UEFI → coreboot) | Pomona clip + CH341A + `flashrom` |
| Internal flashrom | Subsequent updates | `flashrom -p internal -w firmware.rom` |
| BMC flash | If supported | IPMI firmware update channel |

## Implementation Phases

### Phase 1: Test with kexec (no firmware change)

Use the existing CoreOS + kexec approach but with the stormblock-initramfs. This validates the entire iSCSI → ublk → mount → switch_root flow on real hardware without touching firmware.

**This is what we have today.** The `stormblock-boot-server2-spec.md` covers this.

### Phase 2: Build coreboot + Linux payload

Build a coreboot ROM for the Supermicro X9 board with a Linux kernel + stormblock-initramfs as the payload.

```bash
# coreboot build (simplified)
make defconfig BOARD=supermicro/x9sri-3f
# Set payload to Linux
# Set Linux bzImage path
# Set initramfs path to stormblock-initramfs.img
# Set kernel cmdline with rd.stormblock.* params
make
```

Output: `build/coreboot.rom` (~15 MB)

### Phase 3: Flash one test server

```bash
# External flash (first time only)
flashrom -p ch341a_spi -w coreboot.rom

# Or if already running coreboot:
flashrom -p internal -w coreboot.rom
```

Power on. Should go from power-on to Fedora login in under 10 seconds.

### Phase 4: Remote update pipeline

Build a firmware update job in mkube:

```bash
# On the running server (after iSCSI boot):
curl -O http://192.168.10.200:8080/firmware/coreboot.rom
flashrom -p internal -w coreboot.rom
reboot
```

Or via BMC if the board supports IPMI firmware update.

### Phase 5: Multi-host fleet

Each server gets its own coreboot ROM with its specific iSCSI target config baked in (or reads from NVRAM). The `install-fedora-iscsi.sh` script is extended to also build the coreboot ROM.

## Comparison

| | PXE/iPXE | LinuxBoot Single | LinuxBoot Two-Stage |
|---|----------|-----------------|-------------------|
| Boot time | 30-60s | 5-10s | 10-15s |
| Network deps at boot | DHCP + HTTP | iSCSI only | iSCSI only |
| Firmware | UEFI (stock) | coreboot + Linux | coreboot + Linux |
| Kernel updates | HTTP server | Reflash firmware | Update /boot on iSCSI |
| Infrastructure | DHCP, HTTP, iPXE | None (self-contained) | None (self-contained) |
| First setup | Easy (no flash) | Requires SPI programmer | Requires SPI programmer |
| Recovery | PXE boot rescue | coreboot fallback payload | coreboot fallback payload |
| Binary in flash | None | stormblock (11M compressed) | stormblock (4.4M loader) |

## initramfs Reuse

The `stormblock-initramfs.img` (4.4M) we already build works unchanged for all three approaches:

1. **iPXE boot** — fetched over HTTP, kernel boots, /init runs stormblock
2. **LinuxBoot single-stage** — baked into SPI flash alongside kernel, /init runs stormblock
3. **LinuxBoot two-stage** — baked into SPI flash as loader, reads /boot, kexec, then same initramfs in /boot runs stormblock again

The `/init` script is identical in all cases. It parses `rd.stormblock.*` from the kernel cmdline, connects to iSCSI, creates ublk devices, mounts root, and switch_root.

## Open Questions for the Team

1. **Which X9 board variant exactly?** coreboot support varies by sub-model (X9SRI-3F, X9SRL-F, etc.)
2. **SPI flash size on these boards?** Need to verify 16 MB vs 8 MB
3. **Is internal flashrom possible from the running OS?** Some boards lock SPI write access in UEFI
4. **Do we want two-stage for remote kernel updates?** Or is reflashing acceptable for this fleet?
5. **NVRAM vs embedded config?** Per-host NVRAM is more flexible; embedded is simpler for identical machines
6. **Network driver in initramfs?** Intel igb is built into most kernels, but if modular, we need the .ko in the initramfs
