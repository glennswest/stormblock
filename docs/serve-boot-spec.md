# StormBlock `serve-boot` — Boot File Server from iSCSI

## Problem

Boot files (vmlinuz, initramfs) for iSCSI-booted machines are currently staged manually to an HTTP server. This creates two copies of the truth — the files in `/boot` on the iSCSI disk and the staged copies. Updates require re-running the staging process.

## Solution

A `serve-boot` mode for StormBlock that connects to the iSCSI target, mounts the boot partition read-only, and serves its contents over HTTP. The PXE/iPXE infrastructure fetches boot files directly from the iSCSI disk — single source of truth.

## Boot Flow

```
                         ┌─────────────────────┐
                         │  stormblock          │
                         │  serve-boot          │
                         │                      │
                         │  iSCSI ──► slab      │
                         │  ublk  ──► /boot     │
                         │  HTTP  :8080/boot/*  │
                         └──────────┬───────────┘
                                    │
              ┌─────────────────────┼──────────────────────┐
              │                     │                      │
         ┌────▼────┐          ┌─────▼─────┐          ┌─────▼─────┐
         │ server2 │          │ server3   │          │ serverN   │
         │ iPXE    │          │ iPXE      │          │ iPXE      │
         └─────────┘          └───────────┘          └───────────┘

Each server:
  1. iPXE fetches vmlinuz + initramfs from stormblock serve-boot HTTP
  2. Kernel boots with rd.stormblock.* cmdline params
  3. Initramfs runs its own stormblock instance → connects to iSCSI → ublk → mount root
  4. switch_root to Fedora
```

## CLI

```bash
stormblock serve-boot \
    --portal 192.168.10.1 \
    --port 3260 \
    --iqn iqn.2000-02.com.mikrotik:fedora-boot \
    --layout esp:256M,boot:512M,root:7G,swap:1G,home:rest \
    --listen 0.0.0.0:8080 \
    --boot-partition 1
```

| Flag | Default | Description |
|------|---------|-------------|
| `--portal` | (required) | iSCSI target IP |
| `--port` | 3260 | iSCSI target port |
| `--iqn` | (required) | iSCSI target IQN |
| `--layout` | (required) | Partition layout string |
| `--listen` | 0.0.0.0:8080 | HTTP listen address |
| `--boot-partition` | 1 | Partition index for /boot (0-indexed from layout) |

## HTTP Endpoints

```
GET /boot/vmlinuz                    → /boot/vmlinuz-<latest>
GET /boot/initramfs.img              → /boot/stormblock-initramfs.img
GET /boot/                           → file listing (JSON)
GET /boot/<filename>                 → any file from /boot

GET /boot.ipxe                       → generated iPXE script (see below)
GET /boot.ipxe?host=server2          → iPXE script with per-host overrides

GET /health                          → { "status": "ok", "iscsi": "connected", "boot_mounted": true }
```

### Generated iPXE Script (`/boot.ipxe`)

The `serve-boot` process knows all the iSCSI parameters. It generates the iPXE script dynamically — no manual script authoring needed:

```ipxe
#!ipxe
kernel http://${next-server}:8080/boot/vmlinuz \
    rd.stormblock.portal=192.168.10.1 \
    rd.stormblock.iqn=iqn.2000-02.com.mikrotik:fedora-boot \
    rd.stormblock.layout=esp:256M,boot:512M,root:7G,swap:1G,home:rest \
    rd.stormblock.port=3260 \
    console=ttyS0,115200 console=ttyS1,115200 console=tty0

initrd http://${next-server}:8080/boot/initramfs.img

boot
```

## Deployment on mkube

### Option A: Run as a container

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

Requires: ublk_drv loaded on host, /dev/ublk-control passed through.

### Option B: Run as a process on mkube host

```bash
stormblock serve-boot \
    --portal 192.168.10.1 \
    --iqn iqn.2000-02.com.mikrotik:file--raid1-images-kube-gt-lo-raid1-disks-fedora-boot-raw \
    --layout esp:256M,boot:512M,root:7G,swap:1G,home:rest \
    --listen 0.0.0.0:8080 &
```

Simpler. No container overhead. Needs ublk_drv and iSCSI network access.

### Option C: No ublk — read blocks directly (future)

StormBlock already has `ThinVolumeHandle` which implements `BlockDevice` (read/write at offset). The boot partition could be read at the block level without ublk/mount — stormblock reads ext4 directory entries and inodes directly to serve individual files over HTTP.

This eliminates the ublk and mount dependency entirely. The trade-off is implementing a minimal read-only ext4 reader (~300 lines for superblock + inode + extent tree + directory parsing).

## BMH/iPXE Integration

### DHCP Configuration

Point the DHCP `next-server` and `boot-file` at the `serve-boot` instance:

```toml
# microdns DHCP config for g10
[[dhcp.v4.pools]]
next_server = "192.168.10.200"       # host running stormblock serve-boot
boot_file = "http://192.168.10.200:8080/boot.ipxe"
```

Or per-host via DHCP reservation:

```toml
[[dhcp.v4.reservations]]
mac = "ac:1f:6b:8b:11:5d"           # server2
ip = "192.168.10.11"
hostname = "server2"
next_server = "192.168.10.200"
boot_file = "http://192.168.10.200:8080/boot.ipxe"
```

### mkube BMH Boot Config

For the kexec-from-CoreOS pattern (existing infrastructure), point kexec at the `serve-boot` HTTP:

```
HTTP=http://192.168.10.200:8080
curl -o /tmp/vmlinuz $HTTP/boot/vmlinuz
curl -o /tmp/initramfs.img $HTTP/boot/initramfs.img
kexec -l /tmp/vmlinuz --initrd=/tmp/initramfs.img --append="<cmdline>"
kexec -e
```

The cmdline params can also be fetched from the serve-boot endpoint:

```
curl -s http://192.168.10.200:8080/boot.ipxe   # has all params baked in
```

### Direct iPXE Chain (if mkube supports iPXE script boot)

If a BMH can be pointed at a raw iPXE script URL instead of Ignition:

```bash
curl -s -X PATCH http://192.168.200.2:8082/api/v1/baremetalhosts/server2 \
  -H 'Content-Type: application/json' \
  -d '{"spec": {
    "image": "stormblock-boot",
    "bootConfigRef": "stormblock-boot",
    "online": true
  }}'
```

Where the `stormblock-boot` image chains to `http://192.168.10.200:8080/boot.ipxe`.

## Multiple Disks / Multiple Hosts

Each iSCSI disk gets its own `serve-boot` instance (or a single instance serves multiple disks on different HTTP paths):

```bash
# Future: multi-disk serve-boot
stormblock serve-boot \
    --disk server2:192.168.10.1:iqn...:esp:256M,boot:512M,root:7G,swap:1G,home:rest \
    --disk server3:192.168.10.1:iqn...:boot:512M,root:8G,swap:1G \
    --listen 0.0.0.0:8080
```

```
GET /server2/boot/vmlinuz
GET /server2/boot.ipxe
GET /server3/boot/vmlinuz
GET /server3/boot.ipxe
```

## Current iSCSI Disk Details

| Field | Value |
|-------|-------|
| Portal | 192.168.10.1:3260 |
| IQN | iqn.2000-02.com.mikrotik:file--raid1-images-kube-gt-lo-raid1-disks-fedora-boot-raw |
| Layout | esp:256M,boot:512M,root:7G,swap:1G,home:rest |
| Boot partition | index 1 (512 MB ext4, mounted at /boot) |
| Kernel | vmlinuz-7.0.0-0.rc5.260325gbbeb83d3182ab.44.fc45.x86_64 (18M) |
| Initramfs | stormblock-initramfs.img (4.4M) |
| Root password | changeme |
