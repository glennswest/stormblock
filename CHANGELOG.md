# Changelog

## [Unreleased]

### 2026-03-25
- **feat:** `IscsiDevice` — production iSCSI initiator implementing `BlockDevice` trait (login, READ/WRITE(10), READ CAPACITY, UNMAP, NOP-Out keepalive)
- **feat:** `DriveType::Iscsi` variant for iSCSI-backed block devices
- **feat:** `boot_iscsi` module — iSCSI boot disk orchestrator with multi-volume partitioned layout
- **feat:** `BootDiskLayout::parse()` — layout string parsing (e.g., `esp:256M,boot:512M,root:6G,swap:1G,home:rest`)
- **feat:** `IscsiBootManager::provision()` — connect to iSCSI, format slab, create ThinVolumes per partition
- **feat:** CLI `boot-iscsi` subcommand — provision partitioned boot disk on remote iSCSI target
- **feat:** CLI `migrate-boot` subcommand — migrate boot volumes from iSCSI slab to local disk via placement engine
- **test:** 11 boot-from-iSCSI integration tests (layout parsing, provisioning, slab migration)
- **chore:** `boot-iscsi-test.sh` — CI script for mkube job runner (5 phases: build, test, iSCSI slab, clippy, migration)
- **refactor:** Phase 4 API cleanup — replace DiskPool/VDrive with Slab REST API
- **BREAKING:** REST endpoint `/api/v1/pools` removed, replaced by `/api/v1/slabs` (list, get, format, delete, list slots)
- **BREAKING:** CLI subcommand `pool` removed, replaced by `slab` (format, list, info)
- **BREAKING:** `DriveType::VDrive` variant removed from public API
- **refactor:** AppState now holds `Arc<Mutex<SlabRegistry>>` + `Arc<Mutex<GlobalExtentMap>>` instead of `RwLock<HashMap<Uuid, DiskPool>>`
- **refactor:** `migrate_to_local()` simplified — no longer creates DiskPool/VDrive, directly uses RAID 1 add/rebuild/remove
- **chore:** Deleted dead code: `pool.rs` (714 lines), `vdrive.rs` (198 lines), `container.rs`, `container_registry.rs`
- **chore:** Removed `PoolConfig`, `VDriveConfig` from config parser
- **feat:** Placement engine Phase 3 — extent-level migration, slab evacuation, and rebalancing
- **feat:** `migrate_extent()` — move a single extent between slabs with data integrity, GEM update, and ref count management
- **feat:** `evacuate_slab()` — move all extents off a slab for device removal/maintenance
- **feat:** `rebalance()` — redistribute extents across slabs via EvenDistribution or TierAffinity strategy
- **feat:** `migrate_to_slab()` — format destination device as slab, register, and evacuate source slab
- **feat:** `slab_extents()` helper on GlobalExtentMap — collect all extents on a given slab via reverse index
- **feat:** `PlacementError` enum and result types for placement operations
- **feat:** `ci-test.sh` — comprehensive CI orchestrator for mkube job runner (5-phase: build, test+clippy, single-disk iSCSI, multi-disk iSCSI, release build)
- **test:** Multi-disk iSCSI tests — 3 disks (test1 10GB, stormblock-test2 5GB, stormblock-test3 5GB) exercised via job runner
- **fix:** iSCSI initiator — pad SCSI WRITE(10) data to block_size boundary (fixes CHECK CONDITION on 512-byte sector disks)
- **chore:** Dedicated 5GB iSCSI test disks (`boot-iscsi-src`, `boot-iscsi-dst`) for CI isolation
- **fix:** Resolve all compiler warnings and clippy lints for clean `clippy -- -D warnings` on Linux

### 2026-03-24
- **fix:** iSCSI initiator — strict two-phase login (Security→Operational→FullFeature) for LIO Target compatibility
- **fix:** iSCSI initiator — same ITT across all login PDUs per RFC 7143
- **fix:** iSCSI initiator — TSIH propagation from Phase 1 to Phase 2
- **fix:** iSCSI initiator — unique ISID per connection (atomic counter) to prevent session collisions
- **fix:** iSCSI initiator — ExpStatSN+1 after login for full-feature phase
- **fix:** iSCSI initiator — use target's ExpCmdSN from login response for SCSI command sequencing
- **fix:** iSCSI initiator — remove Immediate flag from SCSI write commands (LIO resets on Immediate writes)
- **fix:** iSCSI initiator — NOP-In handling in read loop
- **fix:** iSCSI initiator — use actual block_size from READ CAPACITY instead of hardcoded 4096
- **feat:** Containerfile.iscsi-test — pre-built iSCSI test container for fast iteration
- **feat:** run-iscsi-test.sh — unified runner for pre-built container or cargo build fallback
- **test:** All 3 external iSCSI tests pass against real LIO Target (discovery, write/read/verify, multi-block I/O)

### 2026-03-21
- **feat:** Shared io_uring-style ring buffer IPC — zero-copy shared-memory block I/O between StormFS and StormBlock via Unix socket + memfd + eventfd (`src/drive/uring_channel.rs`, `src/drive/uring_server.rs`)
- **refactor:** Rename Container → Slab throughout codebase — `container.rs` → `slab.rs`, `container_registry.rs` → `slab_registry.rs`, `ContainerId` → `SlabId`, magic `STRMCONT` → `STRMSLAB`
- **fix:** COW bug in Slab.free() — only remove from extent_index if it still points to the slot being freed (prevents index corruption after COW allocation)
- **feat:** Rewrite volume layer to use GEM + SlabRegistry (Phase 2) — ThinVolume is now config-only, all extent tracking via Global Extent Map, I/O routes through Slab slots, allocate-on-write and COW via slab slot allocation, VolumeManager formats Slabs internally from RAID arrays
- **refactor:** ThinVolumeHandle holds Arc<Mutex<GEM>> + Arc<Mutex<SlabRegistry>> instead of embedded extent_map + allocator
- **refactor:** snapshot_diff() now takes (&GlobalExtentMap, VolumeId, VolumeId) — compares slab slot mappings across volumes
- **refactor:** VolumeManager.create_volume() keeps backward-compatible array_id parameter, internally maps to slab preference

### 2026-03-20
- **feat:** Slab extent store — organic data placement with fixed-size 1 MB slots per device (`src/drive/slab.rs`)
- **feat:** Slab registry — tier-indexed slab lookup with best-fit allocation (`src/drive/slab_registry.rs`)
- **feat:** Global Extent Map (GEM) — cross-slab extent tracking with reverse index, COW snapshot cloning, rebuild-from-slabs recovery (`src/volume/gem.rs`)

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
