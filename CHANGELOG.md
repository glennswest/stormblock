# Changelog

## [Unreleased]

### 2026-03-21
- **feat:** Shared io_uring-style ring buffer IPC — zero-copy shared-memory block I/O between StormFS and StormBlock via Unix socket + memfd + eventfd (`src/drive/uring_channel.rs`, `src/drive/uring_server.rs`)

### 2026-03-20
- **feat:** Container extent store — organic data placement with fixed-size 1 MB slots per device (`src/drive/container.rs`)
- **feat:** Container registry — tier-indexed container lookup with best-fit allocation (`src/drive/container_registry.rs`)
- **feat:** Global Extent Map (GEM) — cross-container extent tracking with reverse index, COW snapshot cloning, rebuild-from-containers recovery (`src/volume/gem.rs`)

### 2026-03-19
- **feat:** ublk server — exports BlockDevice as `/dev/ublkbN` via io_uring URING_CMD (replaces NBD)
- **feat:** Direct Linux boot — kernel cmdline and initramfs config generation (replaces iPXE scripts)
- **refactor:** Replace `stormblock nbd` CLI subcommand with `stormblock ublk`
- **refactor:** Migration orchestrator docs updated for ublk (NBD → ublk)
- **BREAKING:** NBD server removed (`src/drive/nbd.rs` deleted, `pub mod nbd` removed)
- **feat:** Placement engine with snapshot-fenced cold copies (`src/placement/`) — extent-level data replication across storage domains
- **feat:** Storage topology types — `StorageTier` (Hot/Warm/Cool/Cold), `Locality` (Local/Remote), `StorageDevice` wrapper
- **feat:** `ColdCopy` — snapshot-fenced replica with per-extent sync bitmap (bitvec), incremental update via `snapshot_diff()`
- **feat:** `PlacementEngine` — cold copy lifecycle management, device registry, async replication with rate limiting

## [v6.0.0] — 2026-03-19

### Added
- **DiskPool**: On-disk pool format with header, VDrive table, first-fit allocator (1 MB alignment), CRC32C checksums, free-space management
- **VDrive**: Offset-translating BlockDevice wrapper over parent device region, with bounds checking
- **NBD server**: Newstyle fixed negotiation protocol, exports any BlockDevice to kernel via `/dev/nbdN` (read/write/disc/flush/trim)
- **RAID 1 dynamic members**: `add_member()` spawns background rebuild, `remove_member()` validates minimum active count — enables live migration
- **DriveType::VDrive**: New variant for virtual drives backed by pool regions
- **Pool REST API**: `GET/POST/DELETE /api/v1/pools` and `/api/v1/pools/{id}/vdrives` for pool and VDrive management
- **RAID member API**: `POST /api/v1/arrays/{id}/members` and `DELETE /api/v1/arrays/{id}/members/{uuid}` for dynamic member management
- **Boot volume manager**: Template creation, per-machine COW snapshot provisioning, iPXE script generation for iSCSI sanboot
- **Migration orchestrator**: Live migrate from iSCSI to local disk via RAID 1 add/rebuild/remove — system never notices
- **CLI subcommands**: `stormblock pool format/list/vdrives/create-vdrive`, `stormblock nbd`, `stormblock migrate`
- **PoolConfig and BootConfig** in configuration parsing
- Pools tracking in AppState for runtime pool management
- 18 new tests (pool header roundtrip, VDrive offset translation, NBD handshake/IO, boot manager, migration)

### Changed
- RAID `members` field refactored from `Vec<MemberInfo>` to `std::sync::RwLock<Vec<MemberInfo>>` for concurrent access
- RAID `capacity` field changed to `AtomicU64` for thread-safe dynamic updates
- All RAID async I/O methods extract `Arc<dyn BlockDevice>` before `.await` (RwLock safety pattern)

## [v5.1.0] — 2026-03-09

### Added
- TLS for cluster RPCs — Raft, heartbeat, and join use HTTPS when `cluster.tls_enabled = true`
- Async replication retry with exponential backoff — retry queue (max 10K entries), up to 8 retries per request, 100ms–30s backoff, Prometheus metrics for retry success/failure/exhausted/dropped
- Fuzz testing for PDU parsers — 6 cargo-fuzz targets covering iSCSI BHS, iSCSI PDU read, iSCSI text params, NVMe-oF common header, NVMe-oF PDU read, NVMe-oF connect data
- StormBase ISO build script (`scripts/build-stormbase-iso.sh`)

### Fixed
- All compiler warnings (unused imports, dead code, unused variables)
- All 55 clippy warnings (Copy vs clone, redundant closures, derive Default, div_ceil, etc.)
- `.gitignore` now covers `target/` everywhere (was only `/target`)

### Changed
- Dockerfile: Alpine 3.21 runtime with storage tools (nvme-cli, smartmontools, fio, iproute2, util-linux, lsblk, e2fsprogs, xfsprogs, jq, ca-certificates)
- Dockerfile: stormblock binary installed to `/usr/bin/stormblock`
- TLS service error type for hyper-util compatibility
- IoUring type annotation for Linux build

## [v5.0.0] — 2026-02-23

### Added
- TLS support for management API via rustls (cert/key config in stormblock.toml)
- Drive health monitoring — SMART data via sysfs with REST endpoint (`GET /api/v1/drives/{id}/smart`)
- iSCSI multi-connection sessions and R2T/Data-Out for large write commands
- NVMe-oF io_uring zero-copy send for C2H data PDUs (Linux, 16KB+ threshold)
- SCSI ALUA (Asymmetric Logical Unit Access) for multipath I/O — REPORT/SET TARGET PORT GROUPS
- VFIO hugepage DMA allocator (MAP_HUGETLB with fallback) and IOVA lookup via /proc/self/pagemap
- NVMe VFIO driver init — open container/group/device, map BAR0, admin queue pair, controller enable
- StormFS registration stub — periodic volume announcement to StormFS metadata cluster

## [v4.0.0] — 2026-02-23

### Added
- Journal recovery and background scrub/verify for RAID engine
- Volume resize (grow/shrink) support with REST API endpoint
- HTMX + Askama web UI for storage management

## [v3.2.0] — 2026-02-19

### Added
- HTMX + Askama web UI for storage management (dashboard, drives, arrays, volumes, exports)

### Changed
- Switch reqwest to rustls-tls for fully static musl builds (no OpenSSL dependency)

### Fixed
- Fix ioctl calls to use `libc::Ioctl` for musl compatibility

## [v3.1.0] — 2026-02-19

### Added
- On-disk metadata persistence for volume state recovery (`--data-dir` flag)
- Binary envelope format with atomic writes and CRC32C checksums
- Restart recovery for extent allocator, thin volumes, and snapshots

## [v3.0.0] — 2026-02-19

### Added
- End-to-end integration tests (FileDevice → RAID 1 → ThinVolume → iSCSI/NVMe-oF target → TCP client)
- Crash recovery tests (journal persist/recovery, superblock validation, extent allocator consistency)
- RAID degraded mode tests (RAID 1 + RAID 5 with failed members)
- Management REST API tests (drives, arrays, volumes, exports, metrics endpoints)
- Volume lifecycle tests (create, snapshot COW, delete, multi-extent writes)
- Criterion micro-benchmarks (parity throughput, extent allocation, PDU parsing)
- fio macro-benchmark scripts (iSCSI + NVMe-oF, 4K random + sequential)
- Container images via Dockerfile for x86_64 and aarch64

### Breaking
- Major version bump for stabilized test/benchmark infrastructure

## [v2.0.0] — 2026-02-19

### Added
- **Phase 3 — Volume manager:** thin provisioning, COW snapshots, extent allocator with free-space bitmap, discard/TRIM handling, snapshot diff for incremental backup
- **Phase 4 — Target protocols:** iSCSI target (RFC 7143, CHAP MD5 auth, full SCSI command set including INQUIRY, READ/WRITE 10/16, READ_CAPACITY, MODE_SENSE, UNMAP, REPORT_LUNS, VPD pages), NVMe-oF/TCP target (fabric connect, discovery subsystem, admin + I/O commands, PDU parsing), per-core reactor pool with CPU pinning
- **Phase 5 — Management plane:** REST API via axum (drives, arrays, volumes, exports endpoints), TOML config parsing with validation, Prometheus metrics endpoint
- **Phase 6 — Cluster scaling:** Raft consensus via openraft 0.9, node discovery and membership, health heartbeat, synchronous and asynchronous replication, volume migration/rebalance, online node addition — all behind `#[cfg(feature = "cluster")]`

### Breaking
- Major version bump for new network protocol subsystems and cluster architecture

## [v1.0.0] — 2026-02-19

### Added
- **Phase 1 — Drive layer:** `BlockDevice` trait (async read/write/flush/discard), page-aligned DMA buffer allocator, SAS backend via io_uring (O_DIRECT, SSD/HDD detection, sysfs metadata), NVMe struct definitions (stub — needs bare metal), FileDevice portable fallback (tokio file I/O for MikroTik/dev/testing), drive enumeration and auto-detection
- **Phase 2 — RAID engine:** RAID 1 (mirror with read balancing), RAID 5 (XOR parity), RAID 6 (dual parity with GF(2^8) multiplication), RAID 10 (striped mirrors), SIMD parity compute (AVX2 x86_64, NEON aarch64, scalar fallback), write-intent bitmap journal with recovery, background rebuild with rate limiting, on-disk superblock format
- CLI entry point with `--device` flag, Ctrl+C graceful shutdown

## [v0.1.0] — 2026-02-17

### Added
- Initial project structure and module layout
- Specification document (`docs/stormblock-spec.md`)
- Source stubs for all planned modules
- Cargo.toml with dependency declarations (openraft 0.9, tokio, axum, io-uring, etc.)
