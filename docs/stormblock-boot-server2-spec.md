# StormBlock iPXE Boot — server2.g10.lo

## Target Host

| Field | Value |
|-------|-------|
| Hostname | server2.g10.lo |
| IP | 192.168.10.11 |
| Boot MAC | ac:1f:6b:8b:11:5d |
| BMC/IPMI | 192.168.11.11 (g11, ADMIN/ADMIN) |
| Hardware | Supermicro SYS-5037MR-H8TRF |
| CPU | Xeon E5-2651 v2 (12 cores) |
| RAM | 62.8 GB |
| Disk | ST2000DM008 1.8T (local, unused for iSCSI boot) |

## iSCSI Target (already provisioned)

| Field | Value |
|-------|-------|
| Portal | 192.168.10.1:3260 |
| IQN | iqn.2000-02.com.mikrotik:file--raid1-images-kube-gt-lo-raid1-disks-fedora-boot-raw |
| Disk size | 10 GB |
| Layout | esp:256M,boot:512M,root:7G,swap:1G,home:rest |

Fedora is already installed on this iSCSI disk (163 packages, kernel 7.0.0-rc5).

## Boot Files

These were built by the `install-fedora-iscsi.sh` job and need to be served via HTTP:

| File | Size | Description |
|------|------|-------------|
| `vmlinuz` | 18M | Linux kernel 7.0.0-0.rc5 |
| `stormblock-initramfs.img` | 4.4M | Initramfs (stormblock 11M + busybox 1.4M + /init) |
| `boot.ipxe` | ~0.5K | iPXE script (optional, for direct iPXE chain) |

## Boot Flow

```
IPMI power on → PXE/iPXE → HTTP fetch vmlinuz + initramfs → kernel boots →
/init runs → network up → stormblock boot-iscsi --ublk → ublk devices appear →
mount root (/dev/ublkb2) → switch_root → Fedora systemd
```

## Setup Steps

### 1. Stage boot files on HTTP server

The boot files need to be accessible over HTTP from the g10 network. The mkube HTTP server at `192.168.10.200` (or `192.168.200.2`) is the natural place.

**Option A: Use mkube static file serving**

Copy the files from the build job output to mkube's static serving directory:

```
/stormblock-boot/vmlinuz
/stormblock-boot/stormblock-initramfs.img
```

Accessible at: `http://192.168.10.200/stormblock-boot/vmlinuz` (etc.)

**Option B: Run a build job that copies them**

Submit a job that builds stormblock and stages the files to a persistent location that mkube serves.

### 2. Create mkube boot config: `stormblock-boot`

Use the kexec-from-CoreOS pattern (same as `fedora-rawhide-install`). This boots CoreOS live, downloads vmlinuz + initramfs, then kexec into the StormBlock kernel.

```bash
curl -s -X POST http://192.168.200.2:8082/api/v1/bootconfigs \
  -H 'Content-Type: application/json' \
  -d '{
    "apiVersion": "v1",
    "kind": "BootConfig",
    "metadata": {"name": "stormblock-boot"},
    "spec": {
      "format": "ignition",
      "description": "StormBlock LinuxBoot — iSCSI root via ublk",
      "data": {
        "config.ign": "{\"ignition\":{\"version\":\"3.4.0\"},\"passwd\":{\"users\":[{\"name\":\"core\",\"sshAuthorizedKeys\":[\"ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQDWUsb0I159v27vSBuOOyQMX54iD2zuKZOOy+e5GRCJ3yONNr3Mkdyng67BNfsnvlf8kpgSi0yiaVGeXKSjkrY9YPHe0wkVW0UHZ9uZqYqgVdEzSG3Z0NNkrd/zp3jCztPad+q6iWb1R0iFlK7/h8NihOky9HXOustrtDwnvTgONwJnluxQp1zl86deKP0W9xx3Ky/Jobr3dbfOhJVK3qzF6OL6KaNjpT+hDYjh1OISzrx1jWLxFvZ4r7X2wbRhcNRyD5sTrxcs3z5Xdz/KRT0UhIj47CF4Heoiqtl/aQ5kdjpRqlmC2spJ9WZinsqbb6HhZ1i8Yd2ZycDQZF+S8n1n gwest@Glenns-MacBook-Pro.local\"]}]},\"systemd\":{\"units\":[{\"name\":\"serial-getty@ttyS0.service\",\"enabled\":true},{\"name\":\"serial-getty@ttyS1.service\",\"enabled\":true},{\"name\":\"stormblock-kexec.service\",\"enabled\":true,\"contents\":\"[Unit]\\nDescription=Kexec into StormBlock LinuxBoot kernel\\nAfter=network-online.target\\nWants=network-online.target\\n\\n[Service]\\nType=oneshot\\nExecStartPre=/usr/bin/sleep 10\\nExecStart=/bin/bash -c '"'"'set -ex; HTTP=http://192.168.10.200/stormblock-boot; curl -L -o /tmp/vmlinuz $HTTP/vmlinuz; curl -L -o /tmp/initramfs.img $HTTP/stormblock-initramfs.img; kexec -l /tmp/vmlinuz --initrd=/tmp/initramfs.img --append=\"rd.stormblock.portal=192.168.10.1 rd.stormblock.iqn=iqn.2000-02.com.mikrotik:file--raid1-images-kube-gt-lo-raid1-disks-fedora-boot-raw rd.stormblock.layout=esp:256M,boot:512M,root:7G,swap:1G,home:rest rd.stormblock.port=3260 console=ttyS0,115200 console=ttyS1,115200 console=tty0\"; kexec -e'"'"'\\nStandardOutput=journal+console\\nStandardError=journal+console\\nTimeoutStartSec=300\\n\\n[Install]\\nWantedBy=multi-user.target\\n\"}]}}"
      }
    }
  }'
```

### 3. Register `stormblock-boot` as an available image

If mkube requires explicit image registration (check if `stormblock-boot` appears in availableImages after creating the boot config):

```bash
# This may happen automatically. If not, update BMH image list:
curl -s -X PATCH http://192.168.200.2:8082/api/v1/baremetalhosts/server2 \
  -H 'Content-Type: application/json' \
  -d '{"spec": {"image": "stormblock-boot", "bootConfigRef": "stormblock-boot"}}'
```

### 4. Set server2 to boot StormBlock and power on

```bash
# Set image to stormblock-boot
curl -s -X PATCH http://192.168.200.2:8082/api/v1/baremetalhosts/server2 \
  -H 'Content-Type: application/json' \
  -d '{"spec": {"image": "stormblock-boot", "bootConfigRef": "stormblock-boot", "online": true}}'
```

Or via two steps:

```bash
# Set image
curl -s -X PATCH http://192.168.200.2:8082/api/v1/baremetalhosts/server2 \
  -H 'Content-Type: application/json' \
  -d '{"spec": {"image": "stormblock-boot", "bootConfigRef": "stormblock-boot"}}'

# Power on
curl -s -X POST http://192.168.200.2:8082/api/v1/baremetalhosts/server2/power \
  -H 'Content-Type: application/json' \
  -d '{"action": "on"}'
```

### 5. Monitor boot via serial console

```bash
# IPMI serial-over-LAN
ipmitool -I lanplus -H 192.168.11.11 -U ADMIN -P ADMIN sol activate
```

Expected boot sequence:
1. IPMI power on
2. PXE boot → iPXE → CoreOS live
3. CoreOS starts `stormblock-kexec.service`
4. Downloads vmlinuz (18M) + initramfs (4.4M) from HTTP
5. kexec into StormBlock kernel
6. `/init` runs: network up, stormblock boot-iscsi --ublk
7. ublk devices appear (/dev/ublkb0-4)
8. Mount root, switch_root to Fedora
9. systemd starts, SSH available at 192.168.10.11

## Kernel Command Line

```
rd.stormblock.portal=192.168.10.1
rd.stormblock.iqn=iqn.2000-02.com.mikrotik:file--raid1-images-kube-gt-lo-raid1-disks-fedora-boot-raw
rd.stormblock.layout=esp:256M,boot:512M,root:7G,swap:1G,home:rest
rd.stormblock.port=3260
console=ttyS0,115200
console=ttyS1,115200
console=tty0
```

## Credentials

| Service | User | Password |
|---------|------|----------|
| IPMI | ADMIN | ADMIN |
| Fedora root | root | changeme |

## Alternative: Direct iPXE (no CoreOS intermediate)

If mkube supports raw iPXE scripts instead of Ignition boot configs, this is simpler:

```ipxe
#!ipxe
kernel http://192.168.10.200/stormblock-boot/vmlinuz \
    rd.stormblock.portal=192.168.10.1 \
    rd.stormblock.iqn=iqn.2000-02.com.mikrotik:file--raid1-images-kube-gt-lo-raid1-disks-fedora-boot-raw \
    rd.stormblock.layout=esp:256M,boot:512M,root:7G,swap:1G,home:rest \
    rd.stormblock.port=3260 \
    console=ttyS0,115200 console=ttyS1,115200 console=tty0

initrd http://192.168.10.200/stormblock-boot/stormblock-initramfs.img

boot
```

This skips the CoreOS intermediate step entirely — iPXE loads the StormBlock kernel directly.

## Troubleshooting

| Issue | Fix |
|-------|-----|
| No network in initramfs | Check `ip=` cmdline param, or verify DHCP on g10 |
| ublk devices don't appear | Check `modprobe ublk_drv` on host kernel, needs 6.0+ |
| iSCSI connection refused | Verify portal 192.168.10.1:3260 is reachable from server2 |
| switch_root fails | Check /dev/ublkb2 exists, root filesystem intact |
| kexec fails in CoreOS | Ensure vmlinuz/initramfs URLs are reachable |
