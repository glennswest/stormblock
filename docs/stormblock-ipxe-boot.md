# StormBlock iPXE + HTTP Boot — Complete Spec

## What This Does

Boots bare metal servers from iSCSI-backed storage over the network. No local disk needed. The server's NIC PXE ROM loads iPXE, iPXE fetches a kernel + initramfs over HTTP, the initramfs runs StormBlock which connects to an iSCSI target, creates ublk block devices, mounts the root filesystem, and hands off to systemd.

## Network Diagram

```
                    ┌──────────────────────────────────────────────┐
                    │                 g10 network                  │
                    │              192.168.10.0/24                 │
                    │                                              │
  ┌─────────┐      │   ┌──────────────┐     ┌──────────────────┐  │
  │ MikroTik│      │   │ mkube        │     │ stormblock       │  │
  │ iSCSI   │◄─────┼───│ 192.168.10.200    │ serve-boot       │  │
  │ target  │      │   │              │     │ :8080            │  │
  │ .10.1   │      │   │ DHCP + TFTP  │     │                  │  │
  │ :3260   │◄─────┼───│ iPXE chain   │     │ iSCSI → mount    │  │
  └─────────┘      │   └──────┬───────┘     │ /boot → HTTP     │  │
                    │          │             └────────┬─────────┘  │
                    │          │                      │            │
                    │     ┌────▼──────────────────────▼────┐      │
                    │     │          server2               │      │
                    │     │       192.168.10.11            │      │
                    │     │    MAC ac:1f:6b:8b:11:5d       │      │
                    │     │                                │      │
                    │     │  1. PXE ROM → DHCP → iPXE      │      │
                    │     │  2. iPXE → HTTP GET vmlinuz    │      │
                    │     │  3. iPXE → HTTP GET initramfs  │      │
                    │     │  4. kernel boots               │      │
                    │     │  5. /init → stormblock         │      │
                    │     │  6. iSCSI → ublk → mount root  │      │
                    │     │  7. switch_root → Fedora       │      │
                    │     └────────────────────────────────┘      │
                    └──────────────────────────────────────────────┘
```

## Boot Sequence (timed)

| Step | Time | What Happens |
|------|------|--------------|
| 0 | 0s | IPMI power on |
| 1 | ~5s | POST, PXE ROM init |
| 2 | ~8s | DHCP lease from mkube (192.168.10.200) |
| 3 | ~9s | TFTP fetch `undionly.kpxe` (70K) → iPXE starts |
| 4 | ~10s | iPXE chains to `http://.../boot.ipxe` |
| 5 | ~12s | HTTP fetch vmlinuz (18M, ~1-2s on gigabit) |
| 6 | ~13s | HTTP fetch stormblock-initramfs.img (4.4M, <1s) |
| 7 | ~15s | Kernel boots, `/init` runs |
| 8 | ~16s | Network up (DHCP or static) |
| 9 | ~17s | StormBlock connects to iSCSI 192.168.10.1:3260 |
| 10 | ~18s | Slab opened, 5 thin volumes loaded |
| 11 | ~19s | 5 ublk devices created (/dev/ublkb0-4) |
| 12 | ~20s | mount root (/dev/ublkb2), boot, ESP, home, swap |
| 13 | ~21s | `switch_root` → `/sbin/init` (systemd) |
| 14 | ~25s | Fedora booted, SSH available |

**~25 seconds** from power-on to login. Compare to ~60-90s for traditional PXE + GRUB + dracut + iSCSI initiator.

## Components

### 1. iSCSI Target (MikroTik — already exists)

| Field | Value |
|-------|-------|
| Portal | 192.168.10.1:3260 |
| IQN | `iqn.2000-02.com.mikrotik:file--raid1-images-kube-gt-lo-raid1-disks-fedora-boot-raw` |
| Disk | 10 GB, 512-byte sectors |
| Contents | Fedora 41 (Rawhide), 163 packages, kernel 7.0.0-rc5 |

Partition layout on the iSCSI disk (StormBlock slab + thin volumes):

| Index | Name | Size | FS | Mount | ublk Device |
|-------|------|------|----|-------|-------------|
| 0 | esp | 256 MB | vfat | /boot/efi | /dev/ublkb0 |
| 1 | boot | 512 MB | ext4 | /boot | /dev/ublkb1 |
| 2 | root | 7 GB | ext4 | / | /dev/ublkb2 |
| 3 | swap | 1 GB | swap | swap | /dev/ublkb3 |
| 4 | home | ~1.2 GB | ext4 | /home | /dev/ublkb4 |

### 2. Boot Files (on HTTP server)

| File | Size | Source |
|------|------|--------|
| `vmlinuz` | 18M | `/boot/vmlinuz-7.0.0-*` on iSCSI boot partition |
| `stormblock-initramfs.img` | 4.4M | Built by `scripts/build-stormblock-initramfs.sh` |

The initramfs contains:

| Path | Size | Purpose |
|------|------|---------|
| `/init` | 3K | Shell script — the boot entry point |
| `/usr/sbin/stormblock` | 11M | iSCSI initiator + ublk server |
| `/bin/busybox` | 1.4M | Shell, mount, ip, insmod, switch_root, etc. |
| `/lib/modules/ublk_drv.ko*` | varies | Kernel module (if not built-in) |

### 3. iPXE Script

```ipxe
#!ipxe

kernel http://192.168.10.200:8080/boot/vmlinuz \
    rd.stormblock.portal=192.168.10.1 \
    rd.stormblock.iqn=iqn.2000-02.com.mikrotik:file--raid1-images-kube-gt-lo-raid1-disks-fedora-boot-raw \
    rd.stormblock.layout=esp:256M,boot:512M,root:7G,swap:1G,home:rest \
    rd.stormblock.port=3260 \
    console=ttyS0,115200 console=ttyS1,115200 console=tty0

initrd http://192.168.10.200:8080/boot/stormblock-initramfs.img

boot
```

### 4. Kernel Command Line Parameters

| Parameter | Value | Description |
|-----------|-------|-------------|
| `rd.stormblock.portal` | 192.168.10.1 | iSCSI target IP |
| `rd.stormblock.port` | 3260 | iSCSI target port |
| `rd.stormblock.iqn` | iqn.2000-02.com.mikrotik:... | iSCSI target name |
| `rd.stormblock.layout` | esp:256M,boot:512M,root:7G,swap:1G,home:rest | Partition layout |
| `console` | ttyS0,115200 / ttyS1,115200 / tty0 | Serial + VGA console |
| `ip` | (optional) | Static IP, e.g. `ip=192.168.10.11::192.168.10.1:255.255.255.0::eth0:none` |

### 5. Target Host: server2.g10.lo

| Field | Value |
|-------|-------|
| Hostname | server2.g10.lo |
| IP | 192.168.10.11 |
| Boot MAC | ac:1f:6b:8b:11:5d |
| BMC/IPMI | 192.168.11.11 (ADMIN/ADMIN) |
| Hardware | Supermicro SYS-5037MR-H8TRF |
| CPU | Xeon E5-2651 v2, 12 cores |
| RAM | 62.8 GB |
| Local disk | ST2000DM008 1.8T (unused — boots from iSCSI) |

## Setup — Step by Step

### Step 1: Run `stormblock serve-boot` on mkube

This connects to the iSCSI target, mounts the boot partition read-only, and serves files over HTTP.

```bash
stormblock serve-boot \
    --portal 192.168.10.1 \
    --port 3260 \
    --iqn iqn.2000-02.com.mikrotik:file--raid1-images-kube-gt-lo-raid1-disks-fedora-boot-raw \
    --layout esp:256M,boot:512M,root:7G,swap:1G,home:rest \
    --listen 0.0.0.0:8080
```

Or as a container on mkube:

```json
{
  "apiVersion": "v1",
  "kind": "Container",
  "metadata": {"name": "stormblock-boot-server"},
  "spec": {
    "image": "registry.gt.lo:5000/stormblock:latest",
    "command": [
      "/stormblock", "serve-boot",
      "--portal", "192.168.10.1",
      "--iqn", "iqn.2000-02.com.mikrotik:file--raid1-images-kube-gt-lo-raid1-disks-fedora-boot-raw",
      "--layout", "esp:256M,boot:512M,root:7G,swap:1G,home:rest",
      "--listen", "0.0.0.0:8080"
    ],
    "ports": [{"containerPort": 8080, "hostPort": 8080}],
    "network": "g10"
  }
}
```

Requires: `ublk_drv` loaded on host, `/dev/ublk-control` accessible.

**HTTP endpoints once running:**

```
GET /boot/vmlinuz              → serves vmlinuz from iSCSI boot partition
GET /boot/initramfs.img        → serves stormblock-initramfs.img
GET /boot.ipxe                 → generated iPXE script with all params
GET /boot/                     → directory listing (JSON)
GET /health                    → { "status": "ok", "iscsi": "connected" }
```

**Verify:**

```bash
curl -s http://192.168.10.200:8080/health
curl -sI http://192.168.10.200:8080/boot/vmlinuz   # should show Content-Length: ~18M
curl -s http://192.168.10.200:8080/boot.ipxe        # should show iPXE script
```

### Step 2: Configure DHCP for server2

Add a per-host DHCP reservation that points iPXE at the boot script.

**Option A: Per-host reservation in microdns config**

```toml
[[dhcp.v4.reservations]]
mac = "ac:1f:6b:8b:11:5d"
ip = "192.168.10.11"
hostname = "server2"
boot_file = "http://192.168.10.200:8080/boot.ipxe"
```

iPXE recognizes HTTP URLs in the DHCP `boot_file` field and fetches the script directly.

**Option B: Global iPXE chainload**

If all PXE-booting hosts should use StormBlock, set it globally in the DHCP pool:

```toml
[[dhcp.v4.pools]]
range_start = "192.168.10.10"
range_end = "192.168.10.210"
subnet = "192.168.10.0/24"
gateway = "192.168.10.1"
dns = ["192.168.1.199"]
domain = "g10.lo"
lease_time_secs = 600
next_server = "192.168.10.200"
boot_file = "http://192.168.10.200:8080/boot.ipxe"
```

**Option C: Two-stage DHCP (PXE ROM → iPXE → HTTP script)**

If the NIC PXE ROM doesn't support HTTP URLs in `boot_file`, use the existing two-stage approach:

1. PXE ROM fetches `undionly.kpxe` via TFTP from `next_server`
2. iPXE starts, re-does DHCP, gets the HTTP `boot_file` URL
3. iPXE fetches and executes the boot script

This is what mkube already does. The only change is the `boot_file` URL.

### Step 3: Create mkube BMH image `stormblock-boot`

Register as an available boot image in the BMH system:

```bash
# Create the boot config
curl -s -X POST http://192.168.200.2:8082/api/v1/bootconfigs \
  -H 'Content-Type: application/json' \
  -d '{
    "apiVersion": "v1",
    "kind": "BootConfig",
    "metadata": {"name": "stormblock-boot"},
    "spec": {
      "format": "ipxe",
      "description": "StormBlock iSCSI boot — Fedora on ublk over iSCSI",
      "data": {
        "script.ipxe": "#!ipxe\nkernel http://192.168.10.200:8080/boot/vmlinuz rd.stormblock.portal=192.168.10.1 rd.stormblock.iqn=iqn.2000-02.com.mikrotik:file--raid1-images-kube-gt-lo-raid1-disks-fedora-boot-raw rd.stormblock.layout=esp:256M,boot:512M,root:7G,swap:1G,home:rest rd.stormblock.port=3260 console=ttyS0,115200 console=ttyS1,115200 console=tty0\ninitrd http://192.168.10.200:8080/boot/stormblock-initramfs.img\nboot\n"
      }
    }
  }'
```

If mkube doesn't support `format: "ipxe"` natively, use the kexec-from-CoreOS pattern instead (Ignition config that boots CoreOS live, then kexec into the StormBlock kernel — see Appendix A).

### Step 4: Assign image to server2 and power on

```bash
# Set image and power on
curl -s -X PATCH http://192.168.200.2:8082/api/v1/baremetalhosts/server2 \
  -H 'Content-Type: application/json' \
  -d '{"spec": {"image": "stormblock-boot", "bootConfigRef": "stormblock-boot", "online": true}}'
```

### Step 5: Monitor via serial console

```bash
ipmitool -I lanplus -H 192.168.11.11 -U ADMIN -P ADMIN sol activate
```

Expected output:

```
StormBlock LinuxBoot init
  Portal: 192.168.10.1:3260
  IQN:    iqn.2000-02.com.mikrotik:file--raid1-images-...
  Layout: esp:256M,boot:512M,root:7G,swap:1G,home:rest
Network: 192.168.10.11/24
Starting StormBlock...
Root device ready: /dev/ublkb2
Mounting filesystems...
Switching to real root...

Fedora Linux 41 (Rawhide)
Kernel 7.0.0-0.rc5 on an x86_64

stormblock-boot login:
```

## Updating Boot Files

### Kernel Update

```bash
# SSH into the running server
ssh root@192.168.10.11

# Install new kernel (writes to /boot on ublk → iSCSI)
dnf5 update kernel

# Copy initramfs if needed
# The stormblock-initramfs.img is separate from dracut's initramfs
```

Since `serve-boot` mounts `/boot` from the iSCSI disk, the updated kernel is immediately available to iPXE on next reboot. No file copying needed.

### StormBlock Binary Update

```bash
# On the running server — update the binary
curl -o /usr/sbin/stormblock http://192.168.10.200:8080/stormblock-latest
chmod 755 /usr/sbin/stormblock

# Rebuild initramfs with new binary
# (run from build host or mkube job)
./scripts/build-stormblock-initramfs.sh /usr/sbin/stormblock $(uname -r)
cp /tmp/stormblock-initramfs.img /boot/stormblock-initramfs.img
```

## Multi-Host Scaling

### Same OS, Same iSCSI Disk (COW clones)

For multiple servers booting the same Fedora image, use StormBlock's COW snapshot feature. Each server gets a snapshot of the base volume — writes go to a private overlay, reads fall through to the shared base.

```bash
# Provision server3 as a COW clone of server2's disk
stormblock boot-iscsi \
    --portal 192.168.10.1 \
    --iqn iqn.2000-02.com.mikrotik:server3-boot \
    --clone-from iqn.2000-02.com.mikrotik:fedora-boot \
    --layout esp:256M,boot:512M,root:7G,swap:1G,home:rest
```

Each server gets its own iPXE config (different IQN in the kernel cmdline) but shares the same base image. Storage-efficient — only diffs are stored.

### Per-Host iPXE Scripts

The `serve-boot` HTTP endpoint supports per-host scripts:

```
GET /boot.ipxe?host=server2    → iPXE script with server2's IQN
GET /boot.ipxe?host=server3    → iPXE script with server3's IQN
```

DHCP can pass the hostname via option 12, and iPXE forwards it in the HTTP request.

## Credentials

| Service | User | Password |
|---------|------|----------|
| IPMI (server2) | ADMIN | ADMIN |
| Fedora root | root | changeme |
| iSCSI | (no auth) | (no auth) |

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| PXE ROM says "No boot filename" | DHCP not returning boot_file | Check microdns DHCP reservation for server2's MAC |
| iPXE says "Connection refused" on HTTP | serve-boot not running | Start stormblock serve-boot, verify with `curl /health` |
| iPXE says "File not found" | Boot files not on HTTP server | Check `curl -sI http://192.168.10.200:8080/boot/vmlinuz` |
| Kernel panic: no init found | Initramfs corrupt or missing /init | Rebuild with `scripts/build-stormblock-initramfs.sh` |
| "FATAL: Missing required kernel parameters" | iPXE script missing rd.stormblock.* | Check iPXE script has all 4 parameters |
| "No network interface found" | Kernel missing NIC driver | Add igb.ko to initramfs, or use built-in kernel |
| DHCP fails in initramfs | DHCP server not responding on g10 | Add `ip=192.168.10.11::192.168.10.1:255.255.255.0::eth0:none` to cmdline |
| "iSCSI connection refused" | Portal unreachable | Verify 192.168.10.1:3260 from server2's network |
| "root device /dev/ublkb2 not found" | ublk_drv not loaded | Add ublk_drv.ko to initramfs, or ensure kernel has it built-in |
| switch_root fails | Root filesystem empty or corrupt | Re-run `install-fedora-iscsi.sh` to reprovision |
| Slow boot (>60s at iPXE stage) | Falling back to TFTP | Ensure iPXE gets HTTP URL in boot_file, not TFTP path |

## Appendix A: Kexec-from-CoreOS Fallback

If mkube doesn't support direct iPXE script boot configs, use the existing CoreOS + kexec pattern. This boots CoreOS live via the standard PXE path, then a systemd service downloads vmlinuz + initramfs and kexec into them.

```json
{
  "apiVersion": "v1",
  "kind": "BootConfig",
  "metadata": {"name": "stormblock-boot"},
  "spec": {
    "format": "ignition",
    "description": "StormBlock iSCSI boot via CoreOS kexec",
    "data": {
      "config.ign": "<ignition JSON — see below>"
    }
  }
}
```

Ignition config (readable form):

```yaml
ignition:
  version: "3.4.0"
passwd:
  users:
    - name: core
      sshAuthorizedKeys:
        - "ssh-rsa AAAAB3Nza... gwest@Glenns-MacBook-Pro.local"
systemd:
  units:
    - name: serial-getty@ttyS0.service
      enabled: true
    - name: serial-getty@ttyS1.service
      enabled: true
    - name: stormblock-kexec.service
      enabled: true
      contents: |
        [Unit]
        Description=Kexec into StormBlock LinuxBoot kernel
        After=network-online.target
        Wants=network-online.target

        [Service]
        Type=oneshot
        ExecStartPre=/usr/bin/sleep 10
        ExecStart=/bin/bash -c '\
          set -ex; \
          HTTP=http://192.168.10.200:8080; \
          curl -L -o /tmp/vmlinuz $HTTP/boot/vmlinuz; \
          curl -L -o /tmp/initramfs.img $HTTP/boot/stormblock-initramfs.img; \
          CMDLINE="rd.stormblock.portal=192.168.10.1 \
            rd.stormblock.iqn=iqn.2000-02.com.mikrotik:file--raid1-images-kube-gt-lo-raid1-disks-fedora-boot-raw \
            rd.stormblock.layout=esp:256M,boot:512M,root:7G,swap:1G,home:rest \
            rd.stormblock.port=3260 \
            console=ttyS0,115200 console=ttyS1,115200 console=tty0"; \
          kexec -l /tmp/vmlinuz --initrd=/tmp/initramfs.img --append="$CMDLINE"; \
          kexec -e'
        StandardOutput=journal+console
        StandardError=journal+console
        TimeoutStartSec=300

        [Install]
        WantedBy=multi-user.target
```

This adds ~15-20 seconds (CoreOS boot + kexec) compared to direct iPXE, but works with the existing mkube infrastructure without changes.

## Appendix B: Files Reference

| File | Repo Path | Description |
|------|-----------|-------------|
| Install script | `install-fedora-iscsi.sh` | 8-phase Fedora provisioning on iSCSI |
| Initramfs builder | `scripts/build-stormblock-initramfs.sh` | Builds the 4.4M initramfs |
| systemd service | `systemd/stormblock-ublk.service` | Safety-net service for post-boot |
| LinuxBoot spec | `docs/linuxboot-iscsi-spec.md` | Future: coreboot firmware approach |
| This spec | `docs/stormblock-ipxe-boot.md` | You are here |
