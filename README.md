# StormBlock

**Pure Rust Enterprise Block Storage Engine**

StormBlock turns raw physical drives — NVMe SSDs, SAS SSDs, SAS HDDs — into network-accessible logical volumes over NVMe-oF/TCP and iSCSI. It is the block-layer foundation of the Storm ecosystem.

## Architecture

```
Initiator (StormFS, iSCSI, NVMe-oF client)
         │
    NVMe-oF/TCP (:4420) or iSCSI (:3260)
    Shared Ring IPC (Unix socket + memfd)
         │
         ▼
┌──────────────────────────────────┐
│          StormBlock              │
│  ┌────────────────────────────┐  │
│  │  Target Protocols          │  │
│  │  NVMe-oF/TCP + iSCSI      │  │
│  │  Shared Ring IPC           │  │
│  ├────────────────────────────┤  │
│  │  Volume Manager            │  │
│  │  Thin + COW Snapshots      │  │
│  │  Global Extent Map (GEM)   │  │
│  ├────────────────────────────┤  │
│  │  Placement Engine          │  │
│  │  Cold copies + tiered data │  │
│  ├────────────────────────────┤  │
│  │  Slab Extent Store         │  │
│  │  1 MB slots, multi-device  │  │
│  ├────────────────────────────┤  │
│  │  RAID Engine               │  │
│  │  1/5/6/10 + SIMD           │  │
│  ├────────────────────────────┤  │
│  │  Drive Layer               │  │
│  │  NVMe (VFIO) + SAS + ublk │  │
│  └────────────────────────────┘  │
└──────────────────────────────────┘
         │
    NVMe (VFIO userspace) + SAS (io_uring)
    ublk (io_uring URING_CMD)
         │
    Physical Drives
```

## Key Features

- **Pure Rust** — No SPDK, no FFI to C libraries. Single static binary (~11 MB musl).
- **NVMe userspace driver** — VFIO-based, per-core queue pairs, MMIO polling. No kernel block layer in the NVMe path.
- **SAS via io_uring** — Kernel SAS drivers (mpt3sas) with O_DIRECT and registered buffers.
- **ublk server** — Exports volumes as `/dev/ublkbN` via io_uring URING_CMD (Linux 6.0+).
- **Software RAID** — RAID 1/5/6/10 with AVX2/AVX-512/NEON SIMD parity computation.
- **Slab extent store** — Organic data placement with fixed-size 1 MB slots per device. Volumes spread across any device on any tier.
- **Global Extent Map (GEM)** — Cross-slab extent tracking with reverse index, COW snapshot cloning, and rebuild-from-slabs recovery.
- **Thin provisioning** — Extent-based allocator, volumes grow on write, and shrink again on discard: the targets advertise thin provisioning (SCSI VPD 0xB2, NVMe DSM) so initiators issue UNMAP/TRIM, which frees slab slots back to the pool.
- **COW snapshots** — Instant snapshots via extent map cloning with reference counting; clone and delete persist refcounts a sector at a time, so latency tracks sectors touched rather than image size.
- **Filesystem templates** — mkfs once, clone forever. The engine formats its own volumes through [`mkfs-ext4`](https://github.com/glennswest/mkfs.ext4.rs) (a from-scratch async mke2fs/e2fsck in pure Rust), seals a template as a snapshot, and every consumer gets a COW clone with a freshly stamped filesystem UUID instead of running mkfs. Formats run concurrently and every clone is fsck'd before hand-off.
- **Placement engine** — Snapshot-fenced cold copies, tiered data placement (Hot/Warm/Cool/Cold), extent-level replication.
- **Shared ring IPC** — io_uring-style zero-copy shared-memory block I/O between StormFS and StormBlock via Unix socket + memfd + eventfd.
- **NVMe-oF/TCP target** — io_uring zero-copy send, per-core reactor model, and hot-add: a host connects once and later attaches arrive as an async event plus a rescan, with no Connect per volume.
- **iSCSI target** — RFC 7143, CHAP authentication, MPIO/ALUA. Thin volumes export directly as LUNs, added and removed at runtime, and scale to thousands per target.
- **Cluster replication** — Raft consensus (openraft), synchronous or asynchronous, TLS-secured RPCs.
- **REST API** — axum-based management (drives, arrays, volumes, exports, slabs, filesystem templates) with optional TLS.
- **Direct Linux boot** — Kernel cmdline and initramfs config for ublk root volumes.
- **312 tests** — Unit, integration, crash recovery, degraded RAID, volume lifecycle, thin reclaim, LUN scale, PDU fuzz testing.

## Data Placement Model

StormBlock uses an **organic, cellular storage model**. Each physical device is formatted as a Slab — a flat array of 1 MB slots. Any volume can allocate slots in any slab on any device. A volume's data starts as a single 1 MB chunk and grows/shrinks/spreads across devices as needed.

```
Volume Z (virtual_size: 100 GB)
  ├── extent 0  ──→  Slab A (local NVMe, Hot), slot 42
  ├── extent 1  ──→  Slab A (local NVMe, Hot), slot 43
  ├── extent 2  ──→  Slab B (remote SAS, Warm), slot 7
  └── extent 3  ──→  Slab A (local NVMe, Hot), slot 100

Slab A (NVMe, tier=Hot, 10K slots)
  ├── slot 42: Volume Z, extent 0
  ├── slot 43: Volume Z, extent 1
  ├── slot 100: Volume Z, extent 3
  └── slot 200: Volume Y, extent 5
```

The **Global Extent Map (GEM)** tracks all extent→slot mappings and is reconstructable from slab slot tables on recovery.

## Hardware Targets

| Tier | Media | Interface | Network |
|------|-------|-----------|---------|
| Tier 0 | NVMe E1.S / E3.S / U.2 | VFIO userspace | 200GbE |
| Tier 1 | SAS SSD | io_uring (HBA330) | 25-100GbE |
| Tier 2 | SAS HDD (JBOD) | io_uring (ARM64 head unit) | 25GbE |
| MikroTik | USB/SATA (RouterOS) | tokio file I/O | 1-10GbE |

## Building

```bash
# Full node (x86_64 — VFIO, io_uring, all features)
cargo build --release --target x86_64-unknown-linux-musl

# ARM64 (JBOD head units)
cargo build --release --target aarch64-unknown-linux-musl --features "arm64,iscsi,nvmeof"

# MikroTik RouterOS (lightweight — no VFIO, no io_uring, iSCSI only)
cargo build --release --target aarch64-unknown-linux-musl --no-default-features --features "mikrotik,iscsi"

# Run tests
cargo test
```

## Configuration

```toml
# stormblock.toml
[system]
hostname = "stormblock-nvme-1"
management_port = 8443

[topology]
site = "nashville"
rack = "rack-a"
tier = "tier0"

[network]
nvmeof_bind = "0.0.0.0:4420"
iscsi_bind = "0.0.0.0:3260"

[io]
io_cores = "2-15"
nvme_queue_depth = 256
uring_sqpoll = true

[management]
listen_addr = "0.0.0.0:9090"
# Where API-created LUNs and volume metadata are persisted, so exports
# come back after a restart.
data_dir = "/var/lib/stormblock"
# Address remote consumers should dial for this node's targets. Target
# listen addresses are usually wildcards, which tell a caller nothing;
# without this, attach info and NVMe-oF discovery fall back to loopback.
advertised_addr = "192.168.200.21"
```

See [stormblock-spec.md](docs/stormblock-spec.md) for the full specification.

### Exporting a volume

A thin/COW volume can be served directly, with no restart. The export
reports the LUN (iSCSI) or namespace ID (NVMe-oF) the initiator must
address:

```bash
# Export a volume — returns {"status":"active","lun_id":0,...}
curl -X POST http://node:9090/api/v1/exports \
  -H 'Content-Type: application/json' \
  -d '{"volume_id":"<uuid>","protocol":"iscsi"}'

# Or attach a LUN directly, letting the next free number be assigned
curl -X POST http://node:9090/api/v1/luns \
  -H 'Content-Type: application/json' \
  -d '{"backing":{"type":"volume","volume_id":"<uuid>"}}'
```

Thin allocation and reclaim are visible on `/metrics` via
`stormblock_slab_allocated_bytes` and `stormblock_slab_free_bytes`.

### Preformatted filesystem templates — mkfs once, clone forever

Formatting a filesystem is the expensive part of provisioning a volume: a
256 MiB ext4 laid down over the network takes ~20 s, while cloning a sealed
template is effectively instant and starts at near-zero allocation. So format
once, seal, and clone:

```bash
# Format + seal in one call — the engine writes the filesystem locally.
# The default is what `mke2fs -t ext4` produces.
curl -X POST http://node:9090/api/v1/fstemplates \
  -H 'Content-Type: application/json' \
  -d '{"name":"ext4-256m","size":"256M","label":"storm"}'

# A variant for a consumer that predates some of it, told apart by name.
# `features` is an `mke2fs -O` list; `journal` and `fs` (ext2/ext3/ext4) are
# the other two knobs.
curl -X POST http://node:9090/api/v1/fstemplates \
  -H 'Content-Type: application/json' \
  -d '{"name":"ext4-plain-256m","size":"256M","journal":false,
       "features":"^64bit,^metadata_csum"}'

# Clone one per consumer — a snapshot plus a fresh filesystem UUID
curl -X POST http://node:9090/api/v1/volumes \
  -H 'Content-Type: application/json' \
  -d '{"name":"pvc-1","from_template":"ext4-256m"}'

# Check any volume's filesystem; ?repair=true corrects what it can
curl -X POST http://node:9090/api/v1/volumes/<uuid>/fsck
```

**Every feature is a per-template choice**, expressed in `mke2fs` terms rather
than re-invented as flags: a filesystem kind (`ext2`/`ext3`/`ext4`), a journal
switch, and an `-O` list. For the journal:
RouterOS cannot replay a journal, so one that ever goes dirty there leaves the
filesystem read-only permanently, while a Linux host or VM wants the crash
consistency.

**Every clone is stamped with its own filesystem UUID.** Without that, two
clones of one template collide on mount-by-UUID and in the blkid cache the
moment both are attached to one host. It happens here because every consumer
clones *through* the engine — a UUID stamped in a layer above would miss the
clones that layer never touches. The default profile carries
`metadata_csum_seed`, so checksums are seeded from the superblock rather than
the UUID and the stamp stays a single write; a filesystem with `metadata_csum`
and no seed has one pinned from its current UUID first, as `tune2fs -U` does.

**Every clone is checked before it is handed out**, and a clone that does not
pass is discarded rather than handed over (`"verify": false` to skip). The same
check is available for any volume at `POST /api/v1/volumes/{id}/fsck` —
RouterOS has no fsck and cannot cleanly unmount a network disk, so a volume it
leaves dirty has nowhere else to be repaired.

Sealing runs a real fsck and refuses a filesystem a consumer could not mount
read-write — `VALID_FS` clear, `ERROR_FS` set, journal replay pending, orphan
cleanup pending, or anything the check turns up. A template that seals dirty
fails much later, inside a container, as `Read-only file system`. Pass `?force=true` to override, and
`{"format": false}` at create time to have an initiator lay the filesystem down
over an export instead, then `POST /api/v1/fstemplates/{id}/seal`.

Formats do not queue. No lock is held across a format, a check or a stamp, and
the formatter takes `&self` so one format fans out across block groups —
provisioning many templates at once costs about what one does, not the sum.

### Clone-per-consumer, reset on restart

Untar a golden image **once**, then clone it per consumer — a clone copies
no data, it shares the source's extents and diverges copy-on-write. When a
consumer restarts, reset it instead of deleting and re-cloning: the volume
keeps its id and only the extents it actually wrote are touched, so the cost
tracks divergence rather than image size.

```bash
# Clone the golden image for a new container instance
curl -X POST http://node:9090/v1/volumes \
  -H 'Content-Type: application/json' \
  -d '{"name":"container-1","size_bytes":536870912,
       "source":{"kind":"volume","id":"<golden-uuid>"}}'

# On restart: squash divergence, back to the golden image
# → {"freed_extents":3,"restored_extents":3,"shared_extents":47}
curl -X POST http://node:9090/v1/volumes/<clone-id>/reset
```

Reset is refused while the volume is attached, since its contents cannot
change under a live host.

## Module Structure

```
src/drive/       BlockDevice trait, NVMe/SAS/FileDevice, Slab extent store, ublk, ring IPC
src/raid/        RAID 1/5/6/10, SIMD parity, write journal, rebuild, scrub
src/volume/      Thin provisioning, COW snapshots, GEM, extent allocator, metadata
src/fs/          filesystem templates: format/check via mkfs-ext4, seal guard, UUID stamp
src/placement/   Cold copies, storage topology, tiered replication
src/target/      NVMe-oF/TCP + iSCSI target protocols, per-core reactor
src/mgmt/        REST API (axum), TOML config, Prometheus metrics, web UI
src/cluster/     Raft consensus, replication, migration (optional feature)
src/boot.rs      Boot volume manager: templates, COW clones, direct Linux boot
src/migrate.rs   Live migration: remote → local via RAID 1
src/stormfs.rs   StormFS registration: volume announcement to metadata cluster
```

## Storm Ecosystem

| Component | Role | Language |
|-----------|------|----------|
| **StormBlock** | Block storage engine | Rust |
| [StormFS](https://github.com/glennswest/stormfs) | Distributed filesystem | Rust |
| [StormForce](https://github.com/glennswest/stormforce) | Event streaming (Kafka replacement) | Rust |
| [StormOS](https://github.com/glennswest/stormos) | Infrastructure OS | Go |

## License

TBD
