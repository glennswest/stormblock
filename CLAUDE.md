# StormBlock Development Guide

## Project Overview
Pure Rust enterprise block storage engine. Turns raw NVMe/SAS drives into network-accessible volumes over NVMe-oF/TCP and iSCSI. Part of the Storm ecosystem (StormBlock, StormFS, StormForce, StormOS).

## Design Principle: Single-node first, scale-out later
StormBlock must be fully functional as a **standalone single-node** storage engine — no cluster requirement. A single node handles its own drives, RAID, volumes, and exports independently. Clustering (replication, Raft) is layered on top and strictly optional. New nodes can be added to an existing deployment at any time without disrupting running nodes.

## Build
```bash
# Full node (x86_64 — VFIO, io_uring, all features)
cargo build --release --target x86_64-unknown-linux-musl

# ARM64 JBOD head unit
cargo build --release --target aarch64-unknown-linux-musl --features "arm64,iscsi,nvmeof"

# MikroTik RouterOS appliance (lightweight — no VFIO, no io_uring, iSCSI only)
cargo build --release --target aarch64-unknown-linux-musl --no-default-features --features "mikrotik,iscsi"
```

**Musl static build** produces an 8.8 MB statically linked, stripped PIE binary (x86_64). Uses rustls-tls (no OpenSSL dependency). Requires `musl-tools` and `musl-dev` packages on the build host. Build and test on Linux: `root@devx.gw.lo:/root/stormblock`.

## Target Platforms

| Platform | Arch | Drive I/O | Targets | Notes |
|----------|------|-----------|---------|-------|
| Full node (Tier 0) | x86_64 | VFIO NVMe + io_uring SAS | NVMe-oF/TCP + iSCSI | Bare metal, buildroot image |
| ARM64 JBOD (Tier 2) | aarch64 | io_uring SAS | NVMe-oF/TCP + iSCSI | SAS shelf head unit |
| MikroTik RouterOS | arm64/x86 | tokio file I/O (no VFIO, no io_uring) | iSCSI | Container on RouterOS 7+, USB/SATA attached storage, small footprint |

**MikroTik considerations:**
- Runs as a container on RouterOS 7+ (or CHR VM)
- No PCIe passthrough — no VFIO, drives are `/dev/sdX` block devices
- No io_uring on RouterOS kernel — fall back to tokio `AsyncFd` / `spawn_blocking` with O_DIRECT
- Memory constrained (256MB–1GB typical) — no hugepage DMA allocator
- iSCSI target only (NVMe-oF unlikely on these networks)
- RAID 1 (mirror) most relevant; RAID 5/6 may be too CPU-heavy on lower-end models
- Binary must be small — strip, LTO, minimal features

## Architecture (bottom-up)
- `src/drive/` — BlockDevice trait: NVMe via VFIO (`nvme.rs`), SAS via io_uring (`sas.rs`), iSCSI initiator (`iscsi_dev.rs`), DMA buffers (`dma.rs`), Slab extent store (`slab.rs`), Slab registry (`slab_registry.rs`), ublk server (`ublk.rs`, Linux-only), shared ring IPC (`uring_channel.rs`, `uring_server.rs`)
- `src/raid/` — Software RAID 1/5/6/10: SIMD parity (`parity.rs`), write journal (`journal.rs`), rebuild (`rebuild.rs`), dynamic add/remove members (RAID 1)
- `src/volume/` — Thin provisioning (`thin.rs`), extent allocator (`extent.rs`), COW snapshots (`snapshot.rs`), Global Extent Map (`gem.rs`)
- `src/target/` — NVMe-oF/TCP :4420 (`nvmeof/`), iSCSI :3260 (`iscsi/`), per-core reactor (`reactor.rs`)
- `src/mgmt/` — REST API via axum (`api/`), TOML config parsing (`config.rs`), Prometheus metrics, slab management (`api/slabs.rs`)
- `src/cluster/` — Optional multi-node: Raft consensus (`raft/`), membership (`membership.rs`), heartbeat (`heartbeat.rs`), replication (`replication.rs`), migration (`migration.rs`)
- `src/boot.rs` — Boot volume manager: templates, COW snapshots per machine, direct Linux boot (kernel cmdline + initramfs config)
- `src/migrate.rs` — Live migration orchestrator: RAID 1 add/rebuild/remove + slab-based extent migration
- `src/placement/` — Placement engine: cold copies, extent migration, slab evacuation, rebalancing (even distribution + tier affinity), storage topology
- `src/stormfs.rs` — StormFS registration: periodic volume announcement to metadata cluster
- `src/boot_iscsi.rs` — iSCSI boot disk orchestrator: multi-volume partitioned disk on iSCSI backing, layout parsing, provisioning
- `src/main.rs` — CLI entry point, drive → RAID → volume → target startup with subcommands (slab, ublk, migrate, boot-iscsi, migrate-boot)

## Current State
All phases (0–7) and all roadmap items are implemented. 246 unit/integration tests pass on macOS; 3 external iSCSI tests pass against real LIO Target via mkube job runner. Musl static release build produces an 11 MB stripped PIE binary (x86_64). The drive layer has four backends: SAS (io_uring, Linux), NVMe (VFIO with hugepage DMA and full init), iSCSI (TCP initiator, any target), and FileDevice (tokio, portable). SMART health monitoring via sysfs with REST endpoint. RAID 1/5/6/10 with SIMD parity, write-intent journal, background rebuild, and dynamic add_member/remove_member for RAID 1. Volume manager with thin provisioning, COW snapshots, extent allocator, and on-disk metadata persistence (`--data-dir` for restart recovery). Slab extent store (organic data placement with 1 MB slots per device, tier-indexed registry, GEM) and ublk server for kernel block device export (Linux 6.0+, io_uring URING_CMD). Boot volume manager with templates, COW clones, and direct Linux boot (kernel cmdline + initramfs config for ublk root). Live migration orchestrator for remote → local disk via RAID 1. Target protocols: iSCSI (RFC 7143, CHAP auth, full SCSI command set, multi-connection sessions, R2T/Data-Out, ALUA multipath) and NVMe-oF/TCP (fabric connect, admin + I/O commands, discovery, io_uring zero-copy send). Per-core reactor pool with CPU pinning on Linux. Management REST API with axum (drives, arrays, volumes, exports, slabs, metrics) with optional TLS via rustls. StormFS registration for volume announcement to metadata cluster. Cluster scaling via openraft 0.9 with HTTP/HTTPS Raft RPCs (TLS via rustls, shares management cert/key), node discovery, heartbeat health monitoring, sync/async volume replication, and volume migration — all behind `#[cfg(feature = "cluster")]`. Placement engine with snapshot-fenced cold copies, extent-level migration (migrate_extent, evacuate_slab, rebalance with EvenDistribution/TierAffinity strategies), storage topology classification (tier/locality), and slab-based migration orchestration. Slab extent store — organic data placement with fixed-size 1 MB slots per device, tier-indexed slab registry, and Global Extent Map (GEM) for cross-slab extent tracking with reverse index and COW snapshot cloning. Volume layer (Phase 2) rewritten: ThinVolume is config-only, ThinVolumeHandle routes I/O through GEM + SlabRegistry, allocate-on-write and COW via slab slot allocation. Shared ring IPC — io_uring-style zero-copy shared-memory block I/O between StormFS and StormBlock via Unix socket + memfd + eventfd. Boot-from-iSCSI: connect to remote iSCSI target as a BlockDevice, format as slab, create multi-volume partitioned disk layout (ESP/boot/root/swap/home), export each partition as ublk device, live-migrate to local disk. Integration tests exercise the full stack. Container images via Dockerfile for deployment under StormBase.

Build host: root@devx.gw.lo (192.168.1.53), CT 102 on pvex.gw.lo (192.168.1.160). 40GB /build disk for cargo target directory. DNS: 192.168.1.199, 192.168.1.154 (dns.gw.lo).

---

## TODO — Implementation Roadmap

### Phase 0: Build fixes (get it compiling) — DONE
- [x] Fix `openraft` version: 0.10 → 0.9
- [x] Add `anyhow` to dependencies
- [x] Make `io-uring` dependency Linux-only via `[target.'cfg(target_os = "linux")'.dependencies]`
- [x] Make `nix` dependency Linux-only
- [ ] Add `#[allow(unused)]` or `#[cfg]` gates so empty modules don't warn (not needed yet — no code to warn about)
- [x] Verify the full dependency set resolves and compiles (confirmed on macOS, Linux targets need cross-compiler)

### Phase 1: Drive layer (`src/drive/`) — DONE
- [x] Define `BlockDevice` trait (async read/write/flush/discard)
- [x] `dma.rs` — Page-aligned buffer allocator (DmaBuf with alloc/zeroed/pool)
- [x] `dma.rs` — Hugepage-backed slab allocator for VFIO
- [x] `nvme.rs` — Struct definitions (NvmeDevice, IoQueuePair, SQ/CQ entries, registers)
- [x] `nvme.rs` — VFIO init, BAR0 mapping, queue pairs
- [x] `sas.rs` — Open /dev/sdX with O_DIRECT, detect SSD/HDD, read serial/model from sysfs
- [x] `sas.rs` — io_uring read/write/flush/discard
- [x] `filedev.rs` — NEW: Portable tokio file I/O fallback (MikroTik, dev, testing)
- [x] `mod.rs` — Drive enumeration: auto-detect block device vs file, open appropriate backend
- [x] `main.rs` — Wired up drive init with `--device` CLI flag
- [x] Drive health monitoring (SMART via sysfs + REST endpoint)

### Phase 2: RAID engine (`src/raid/`) — DONE
- [x] RAID superblock format (on-disk metadata: member drives, layout, state)
- [x] RAID 1 (mirror) — read balancing, write duplication
- [x] RAID 5 — stripe layout, XOR parity compute
- [x] RAID 6 — dual parity (P + Q, GF(2^8) multiplication)
- [x] RAID 10 — striped mirrors
- [x] `parity.rs` — SIMD XOR: AVX2 (x86_64), NEON (aarch64), scalar fallback
- [x] `parity.rs` — GF multiply for RAID 6 Q syndrome (AVX2 shuffle, NEON vtbl)
- [x] `journal.rs` — Write-intent bitmap: mark dirty stripes before write, clear after
- [x] `journal.rs` — Journal recovery on startup (partial stripe detection)
- [x] `rebuild.rs` — Background rebuild: read surviving members, recompute parity/mirror
- [x] `rebuild.rs` — Rate limiting (don't starve foreground I/O)
- [x] Scrub/verify (background read + parity check)

### Phase 3: Volume manager (`src/volume/`) — DONE
- [x] On-disk metadata persistence (`metadata.rs` — binary envelope, atomic writes, CRC32C, restart recovery)
- [x] `extent.rs` — Free-space bitmap, extent allocation (first-fit or best-fit)
- [x] `extent.rs` — Extent deallocation, coalescing
- [x] `thin.rs` — Thin volume: virtual-to-physical extent mapping
- [x] `thin.rs` — On-demand allocation on first write (allocate-on-write)
- [x] `thin.rs` — Discard/TRIM handling (return extents to free pool)
- [x] `snapshot.rs` — COW snapshot creation (clone extent map, bump refcounts)
- [x] `snapshot.rs` — Snapshot deletion (decrement refcounts, free unreferenced extents)
- [x] `snapshot.rs` — Snapshot diff (for incremental backup)
- [x] Volume resize (grow/shrink)

### Phase 4: Target protocols (`src/target/`) — DONE
- [x] `reactor.rs` — Per-core single-threaded tokio runtimes, round-robin dispatch
- [x] `reactor.rs` — Core affinity via sched_setaffinity (Linux), no-op on macOS
- [x] `nvmeof/pdu.rs` — NVMe-oF/TCP PDU parsing (ICReq, ICResp, CapsuleCmd, CapsuleResp, C2HData, H2CData, R2T)
- [x] `nvmeof/discovery.rs` — NVMe-oF discovery subsystem (discovery log page)
- [x] `nvmeof/fabric.rs` — Fabric Connect, Property Get/Set, controller register emulation
- [x] `nvmeof/admin.rs` — Identify Controller/Namespace, Active NS List, Get Log Page
- [x] `nvmeof/io.rs` — NVMe I/O: Read, Write, Flush, Dataset Management (TRIM)
- [x] `nvmeof/mod.rs` — NVMe-oF target server (ICReq/ICResp handshake, command loop)
- [x] `nvmeof` — io_uring zero-copy send for C2H data
- [x] `iscsi/pdu.rs` — iSCSI PDU parsing (48-byte BHS, CRC32C digests, text params)
- [x] `iscsi/login.rs` — iSCSI login state machine (security + operational negotiation)
- [x] `iscsi/chap.rs` — CHAP MD5 authentication (constant-time verify)
- [x] `iscsi/scsi.rs` — SCSI command dispatch (INQUIRY, READ/WRITE 10/16, READ_CAPACITY, MODE_SENSE, UNMAP, REPORT_LUNS, VPD pages)
- [x] `iscsi/session.rs` — Session registry, TSIH allocation, CmdSN/StatSN tracking
- [x] `iscsi/mod.rs` — iSCSI target server (login phase, full-feature phase, Data-In chunking)
- [x] `main.rs` — CLI flags for target config, startup with Ctrl+C graceful shutdown
- [x] `iscsi` — Multi-connection sessions, R2T/Data-Out for large writes
- [x] MPIO/ALUA support for multipath

### Phase 5: Management plane (`src/mgmt/`) — DONE
- [x] `config.rs` — Parse `stormblock.toml` into typed config structs
- [x] `config.rs` — Config validation (drive paths exist, ports not conflicting, etc.)
- [x] `api/drives.rs` — REST routes: `GET /api/v1/drives` (enumerate)
- [x] `api/arrays.rs` — REST routes: `GET/POST/DELETE /api/v1/arrays` (RAID create/delete/status)
- [x] `api/volumes.rs` — REST routes: `GET/POST/DELETE /api/v1/volumes` (create/delete/snapshot)
- [x] `api/exports.rs` — REST routes: `GET/POST/DELETE /api/v1/exports` (NVMe-oF/iSCSI target mappings)
- [x] `metrics.rs` — Prometheus metrics endpoint (`/metrics`)
- [x] `mod.rs` — AppState, DriveInfo, ArrayInfo, ExportEntry, start_management_server()
- [x] `main.rs` — Config loading, CLI merge, AppState wiring, mgmt server spawn
- [x] TLS for management API (rustls)

### Phase 6: Cluster scaling (optional — single-node must work without any of this) — DONE
- [x] Node discovery: new node announces itself via REST to an existing node or seed list
- [x] Cluster membership store: track known nodes, health, capacity (local JSON or embedded DB)
- [x] `api/cluster.rs` — REST routes: `GET/POST/DELETE /api/v1/cluster/nodes` (list, join, remove)
- [x] Node health heartbeat (periodic ping between peers, mark unreachable)
- [x] Raft consensus via openraft (leader election, log replication) for metadata coordination
- [x] Synchronous replication (write to N replicas before ack)
- [x] Asynchronous replication (background catchup)
- [x] Volume migration/rebalance: move volumes between nodes when capacity added
- [x] Online node addition: join a running cluster, receive replicated volumes without downtime
- [x] TLS for cluster RPCs (Raft, heartbeat, join) via rustls — shares management API cert/key

### Phase 7: Integration & hardening — DONE
- [x] End-to-end test: FileDevice → RAID 1 → ThinVolume → iSCSI/NVMe-oF target → TCP initiator → read/write/verify
- [x] Crash recovery testing (journal persist/recovery, superblock validation, extent allocator consistency)
- [x] RAID degraded mode tests (RAID 1 + RAID 5 with failed members)
- [x] Management REST API tests (drives, arrays, volumes, exports, metrics endpoints)
- [x] Volume lifecycle tests (create, snapshot COW, delete, multi-extent writes)
- [x] Criterion micro-benchmarks (parity throughput, extent allocation, PDU parsing)
- [x] fio macro-benchmark scripts (iSCSI + NVMe-oF, 4K random + sequential)
- [x] Container images (Dockerfile x86_64 + aarch64, deployed via StormBase)
- [x] StormFS registration (announce volumes to StormFS metadata cluster)

### Container Extent Store — Organic Data Placement

Replaces rigid DiskPool/VDrive/ExtentAllocator with organic, cellular storage. Each device is a Container (flat array of 1 MB slots). Volumes spread across any device on any tier. GEM is the single source of truth for extent placement.

**Phase 1: Foundation (additive, non-breaking) — DONE**
- [x] `src/drive/container.rs` — Container extent store with on-disk format, slot table, free bitmap, CRC32C (~550 lines, 11 tests)
- [x] `src/drive/container_registry.rs` — Tier-indexed container lookup with best-fit allocation (~150 lines, 3 tests)
- [x] `src/volume/gem.rs` — Global Extent Map with forward+reverse index, COW snapshot cloning, rebuild-from-containers (~300 lines, 10 tests)
- [x] Module declarations in `src/drive/mod.rs` and `src/volume/mod.rs`

**Phase 2: Volume layer rewrite — DONE**
- [x] Rewrite `src/volume/thin.rs` — ThinVolume backed by GEM + SlabRegistry instead of array_id + ExtentAllocator
- [x] Add VolumePurpose (Partition, StormFS, ObjectStore, KeyValue, Boot) and PlacementPolicy
- [x] Rewrite `src/volume/snapshot.rs` — COW via GEM clone + slab inc_ref
- [x] Update `src/volume/mod.rs` — VolumeManager uses GEM + SlabRegistry
- [x] Update `src/volume/metadata.rs` — V2 format with slab refs
- [x] Update external references: boot.rs, mgmt/api/volumes.rs, mgmt/mod.rs, main.rs, placement/mod.rs, tests/

**Phase 3: Placement integration — DONE**
- [x] `src/placement/mod.rs` — PlacementError, migrate_extent(), evacuate_slab(), rebalance() (EvenDistribution + TierAffinity)
- [x] `src/volume/gem.rs` — slab_extents() helper for reverse-index slab queries
- [x] `src/migrate.rs` — Slab-based extent migration via migrate_to_slab() (alongside existing RAID-level migrate_to_local())
- [x] 6 new tests: migrate_extent, evacuate_slab, rebalance_even, rebalance_tier_affinity, placement_error_display, migrate_to_slab

**Phase 4: API + cleanup — DONE**
- [x] Deleted `pool.rs`, `vdrive.rs`, `container.rs`, `container_registry.rs`
- [x] Created `src/mgmt/api/slabs.rs` — Slab REST API (list, get, format, delete, list slots)
- [x] Updated AppState: `slab_registry` + `gem` (Arc<Mutex>) instead of `pools` (RwLock<HashMap>)
- [x] Replaced CLI `pool` subcommand with `slab` (format, list, info)
- [x] Removed `DriveType::VDrive`, `PoolConfig`, `VDriveConfig`
- [x] Simplified `migrate_to_local()` — uses RAID 1 directly, no DiskPool/VDrive
- [x] All tests pass (229), clean clippy

### External iSCSI Test Infrastructure — DONE
- [x] `tests/common/iscsi_initiator.rs` — Pure Rust iSCSI initiator (two-phase login, SCSI read/write/inquiry/capacity/logout)
- [x] `tests/external_iscsi.rs` — 3 integration tests against real LIO Target (discovery, write/read/verify, multi-block I/O)
- [x] `Containerfile.iscsi-test` — Pre-built test container for fast iteration via mkube job runner
- [x] `run-iscsi-test.sh` — Unified runner (pre-built binary or cargo build fallback)
- [x] `test-iscsi.sh` — Build script for mkube job submission
- [x] Verified against LIO Target at 192.168.10.1:3260 (MikroTik, 10 GB, 512-byte blocks)

### Shared Ring IPC — DONE
- [x] `src/drive/uring_channel.rs` — Ring buffer protocol, SQE/CQE types, shared memory layout
- [x] `src/drive/uring_server.rs` — Unix socket server, per-client memfd+eventfd, I/O dispatch

### Boot-from-iSCSI with Live Migration — DONE
- [x] `src/drive/iscsi_dev.rs` — Production iSCSI initiator BlockDevice (login, READ/WRITE(10), READ CAPACITY, UNMAP, NOP-Out)
- [x] `DriveType::Iscsi` variant added to drive layer
- [x] `src/boot_iscsi.rs` — Boot disk orchestrator (BootDiskLayout, IscsiBootManager, multi-volume provisioning)
- [x] CLI `boot-iscsi` subcommand — provision partitioned boot disk on remote iSCSI target
- [x] CLI `migrate-boot` subcommand — migrate boot volumes from iSCSI slab to local disk
- [x] 11 integration tests (layout parsing, provisioning on file slab, slab migration with data verification)
- [x] `boot-iscsi-test.sh` — CI script for mkube job runner
