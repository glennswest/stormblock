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
- **Thin provisioning** — Extent-based allocator, volumes grow on write.
- **COW snapshots** — Instant snapshots via extent map cloning with reference counting.
- **Placement engine** — Snapshot-fenced cold copies, tiered data placement (Hot/Warm/Cool/Cold), extent-level replication.
- **Shared ring IPC** — io_uring-style zero-copy shared-memory block I/O between StormFS and StormBlock via Unix socket + memfd + eventfd.
- **NVMe-oF/TCP target** — io_uring zero-copy send, per-core reactor model.
- **iSCSI target** — RFC 7143, CHAP authentication, MPIO/ALUA.
- **Cluster replication** — Raft consensus (openraft), synchronous or asynchronous, TLS-secured RPCs.
- **REST API** — axum-based management (drives, arrays, volumes, exports, slabs) with optional TLS.
- **Direct Linux boot** — Kernel cmdline and initramfs config for ublk root volumes.
- **242 tests** — Unit, integration, crash recovery, degraded RAID, volume lifecycle, PDU fuzz testing.

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
```

See [stormblock-spec.md](docs/stormblock-spec.md) for the full specification.

## Module Structure

```
src/drive/       BlockDevice trait, NVMe/SAS/FileDevice, Slab extent store, ublk, ring IPC
src/raid/        RAID 1/5/6/10, SIMD parity, write journal, rebuild, scrub
src/volume/      Thin provisioning, COW snapshots, GEM, extent allocator, metadata
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
