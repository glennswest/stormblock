# StormBlock

**Pure Rust Enterprise Block Storage Engine**

StormBlock turns raw physical drives — NVMe SSDs, SAS SSDs, SAS HDDs — into network-accessible logical volumes over NVMe-oF/TCP and iSCSI. It is the block-layer foundation of the Storm ecosystem.

## Architecture

```
Initiator (StormFS, iSCSI, NVMe-oF client)
         │
    NVMe-oF/TCP (:4420) or iSCSI (:3260)
         │
         ▼
┌─────────────────────────────┐
│       StormBlock            │
│  ┌───────────────────────┐  │
│  │  Target Protocols     │  │
│  │  NVMe-oF/TCP + iSCSI │  │
│  ├───────────────────────┤  │
│  │  Volume Manager       │  │
│  │  Thin + COW Snapshots │  │
│  ├───────────────────────┤  │
│  │  RAID Engine          │  │
│  │  1/5/6/10 + SIMD      │  │
│  ├───────────────────────┤  │
│  │  Drive Layer          │  │
│  │  NVMe (VFIO) + SAS   │  │
│  └───────────────────────┘  │
└─────────────────────────────┘
         │
    NVMe (VFIO userspace) + SAS (io_uring)
         │
    Physical Drives
```

## Key Features

- **Pure Rust** — No SPDK, no FFI to C libraries. Single static binary (~12MB).
- **NVMe userspace driver** — VFIO-based, per-core queue pairs, MMIO polling. No kernel block layer in the NVMe path.
- **SAS via io_uring** — Kernel SAS drivers (mpt3sas) with O_DIRECT and registered buffers.
- **Software RAID** — RAID 1/5/6/10 with AVX2/AVX-512/NEON SIMD parity computation.
- **Thin provisioning** — Extent-based allocator, volumes grow on write.
- **COW snapshots** — Instant snapshots via extent map cloning with reference counting.
- **NVMe-oF/TCP target** — io_uring zero-copy send, per-core reactor model.
- **iSCSI target** — RFC 7143, CHAP authentication, MPIO/ALUA.
- **Cluster replication** — Raft consensus (openraft), synchronous or asynchronous.
- **REST API** — axum-based management (drives, arrays, volumes, exports).

## Hardware Targets

| Tier | Media | Interface | Network |
|------|-------|-----------|---------|
| Tier 0 | NVMe E1.S / E3.S / U.2 | VFIO userspace | 200GbE |
| Tier 1 | SAS SSD | io_uring (HBA330) | 25-100GbE |
| Tier 2 | SAS HDD (JBOD) | io_uring (ARM64 head unit) | 25GbE |

## Building

```bash
# x86_64
cargo build --release --target x86_64-unknown-linux-musl

# ARM64 (JBOD head units)
cargo build --release --target aarch64-unknown-linux-musl --features "arm64,iscsi,nvmeof"
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

## Storm Ecosystem

| Component | Role | Language |
|-----------|------|----------|
| **StormBlock** | Block storage engine | Rust |
| [StormFS](https://github.com/glennswest/stormfs) | Distributed filesystem | Rust |
| [StormForce](https://github.com/glennswest/stormforce) | Event streaming (Kafka replacement) | Rust |
| [StormOS](https://github.com/glennswest/stormos) | Infrastructure OS | Go |

## License

TBD
