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

**Musl static build** produces an 8.8 MB statically linked, stripped PIE binary (x86_64). Uses rustls-tls (no OpenSSL dependency). Requires `musl-tools` and `musl-dev` packages on the build host. Build and test on Linux: `root@dev.g8.lo:/root/stormblock` (or `gwest@dev.g8.lo` — shared dev host).

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
All phases (0–7) and all roadmap items are implemented. 312 unit/integration tests pass on macOS; 3 external iSCSI tests pass against real LIO Target via mkube job runner. Musl static release build produces an 11 MB stripped PIE binary (x86_64). The drive layer has four backends: SAS (io_uring, Linux), NVMe (VFIO with hugepage DMA and full init), iSCSI (TCP initiator, any target), and FileDevice (tokio, portable). SMART health monitoring via sysfs with REST endpoint. RAID 1/5/6/10 with SIMD parity, write-intent journal, background rebuild, and dynamic add_member/remove_member for RAID 1. Volume manager with thin provisioning, COW snapshots, extent allocator, and on-disk metadata persistence (`--data-dir` for restart recovery). Slab extent store (organic data placement with 1 MB slots per device, tier-indexed registry, GEM) and ublk server for kernel block device export (Linux 6.0+, io_uring URING_CMD). Boot volume manager with templates, COW clones, and direct Linux boot (kernel cmdline + initramfs config for ublk root). Live migration orchestrator for remote → local disk via RAID 1. Target protocols: iSCSI (RFC 7143, CHAP auth, full SCSI command set, multi-connection sessions, R2T/Data-Out, ALUA multipath) and NVMe-oF/TCP (fabric connect, admin + I/O commands, discovery, io_uring zero-copy send). Per-core reactor pool with CPU pinning on Linux. Management REST API with axum (drives, arrays, volumes, exports, slabs, metrics) with optional TLS via rustls. StormFS registration for volume announcement to metadata cluster. Cluster scaling via openraft 0.9 with HTTP/HTTPS Raft RPCs (TLS via rustls, shares management cert/key), node discovery, heartbeat health monitoring, sync/async volume replication, and volume migration — all behind `#[cfg(feature = "cluster")]`. Placement engine with snapshot-fenced cold copies, extent-level migration (migrate_extent, evacuate_slab, rebalance with EvenDistribution/TierAffinity strategies), storage topology classification (tier/locality), and slab-based migration orchestration. Slab extent store — organic data placement with fixed-size 1 MB slots per device, tier-indexed slab registry, and Global Extent Map (GEM) for cross-slab extent tracking with reverse index and COW snapshot cloning. Volume layer (Phase 2) rewritten: ThinVolume is config-only, ThinVolumeHandle routes I/O through GEM + SlabRegistry, allocate-on-write and COW via slab slot allocation. Shared ring IPC — io_uring-style zero-copy shared-memory block I/O between StormFS and StormBlock via Unix socket + memfd + eventfd. Boot-from-iSCSI: connect to remote iSCSI target as a BlockDevice, format as slab, create multi-volume partitioned disk layout (ESP/boot/root/swap/home), export each partition as ublk device, live-migrate to local disk. Integration tests exercise the full stack. Container images via Dockerfile for deployment under StormBase.

Build host: dev.g8.lo (login `root` or `gwest`) — the shared dev box for compile/build/test. For special runtime testing that needs its own machine (not plain compiles), spin up a VM with terragrunt — see the sister projects for examples. DNS: 192.168.1.252, 192.168.1.154 (dns.gw.lo).

---

## TODO — Implementation Roadmap

### Layered goldens — engine items (2026-08-19)

Master checklist lives in **stormblock-registry/CLAUDE.md**, "Layered goldens
— the plan". The engine owns two of its items:

- [x] **`FROM` for templates** — `TemplateSpec.parent` makes a template's raw
      volume a CoW clone of a parent's sealed snapshot instead of a blank, so
      a runtime several images share is stored once. New `awaiting_seed`
      state; a fresh filesystem UUID is stamped at creation, because two
      children must not both claim the parent's identity and under
      `metadata_csum` that UUID seeds every checksum in the filesystem.
- [ ] **Volume groups** — a "system" group for goldens and a "data" group for
      PVCs, so the system disk can be replaced wholesale without touching
      state. **A group has to be a hard allocation boundary, not a
      preference:** `PlacementPolicy` is tier-based with fallback today, so a
      golden would silently spill onto the data disk when the system disk
      fills — leaving a half-migrated system scattered across the disk you
      were about to replace. Size the system group for **two generations**; a
      rebase transiently holds both, since old blocks stay refcounted until
      the old goldens are deleted.
- [ ] **Failure-domain topology.** `placement/topology.rs` models
      `StorageTier` (Hot/Warm/Cool/Cold) and `Locality` — *how fast and how
      far*. It has no notion of *what fails together*: no chassis, rack, row,
      floor, building or site. Those are orthogonal — two drives can both be
      Hot and local and share a power supply — and the second is what
      placement needs to keep a volume's only copies out of one blast radius.
      **Density makes this urgent rather than theoretical.** A MikroTik node
      carries ~16 drives; a 4U 160-bay NVMe server (Supermicro
      ASG-4116S-NU160R, FMS 2026) carries 160+. At that point a *node* is
      already a failure domain worth reasoning about internally, and a rack of
      them is 4,000+ drives. stormblock has to know its own drives first, then
      where those drives are.
      Ties directly to volume groups: a group is "a set of slabs", and with
      topology it becomes "a set of slabs constrained by failure domain" —
      which makes "replace the system disk" and "survive losing a rack" the
      same mechanism.
- [x] **Raw import** — `POST /mk/v1/volumes/{id}/raw`, sparse-aware. Landed
      in stormblockmk 2026-08-19 and proven end to end; **belongs down here**
      with the rest of layer 2 (see below).
- [ ] **Promote the serving layer out of stormblockmk** — see
      [docs/layering.md](docs/layering.md). Measured: 4,865 lines in
      stormblockmk, of which the RouterOS-specific part is 11 mentions in
      config defaults and startup composition. The wiring table, reconciler,
      readiness, reaper, tar/raw import, trim and live-session detection are
      deployment-agnostic — a stormos profile wants ~3,700 of those lines and
      can only fork them today. Includes fixing the export-durability gap the
      split exposes: the engine keeps its export table in memory only and the
      profile persists it, which is a correctness requirement living in the
      wrong layer. **Design constraint from the notes:** layer 2 serves
      *volumes*, and must not assume the thing attaching is a container —
      VMs and micro-VMs are the easier case, since a clone already *is* a
      block device.


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

### /v1 CSI Contract API — DONE (issues #3, #8, #9, #10; API layer of #5/#6/#7)
- [x] `src/mgmt/api/v1.rs` — full /v1 surface per stormblock-csi docs/stormblock-api.md (MockEngine is the executable spec): volumes (name-idempotent create, COW clone via `source`, expand, attach/detach with mode gating), snapshots + group snapshots, placement/prestage, fence/promote (epoch CAS), bounded dual-attach, node capacity/topology, `{code,message,current_epoch?}` error envelope (404/409/412/507), optional bearer auth (`management.api_token`)
- [x] `VolumeManager::create_volume_any` (array-free create) + `create_snapshots_atomic` (GEM+registry locks held across all members = single consistency fence for VolumeGroupSnapshot)
- [x] Empty-volume snapshot/delete fix in `src/volume/snapshot.rs` (never-written volumes have no GEM map)
- [x] Config: `management.api_token`, `management.node_name`, `management.topology`; /v1 state persisted to `<data_dir>/v1_state.json`
- [x] 11 HTTP-level integration tests (`tests/integration_v1_api.rs`) ported from the MockEngine spec + engine-backed COW divergence
- [ ] Engine data path for #5/#6/#7 (cross-node replication, epoch-carrying writes, resync) — control-plane state only for now

### boot-local + storage-role systemd unit — DONE (issues #11, #12)
- [x] CLI `boot-local` — attach existing slab(s) non-destructively (`open_backing_device`, no reformat), restore volumes.dat, resolve boot volume by UUID/name/boot.toml, export as /dev/ublkb0 (+ optional image-store as ublkb1), `--local-disk` zeroboot flow-over (per-extent lock cycling so root I/O keeps flowing)
- [x] initramfs `/init` local-slab path: `rd.stormblock.slab=` / baked boot.toml, erofs root, no network needed
- [x] `systemd/stormblock-target.service` — storage-role target server (config from /etc/stormblock/stormblock.toml); SIGTERM now shuts down gracefully like Ctrl+C
- [x] 3 CLI integration tests (`tests/integration_boot_local.rs`)

### LinuxBoot-style Fedora on iSCSI — DONE
- [x] `--ublk` flag on `boot-iscsi` CLI — UblkServer per partition (Linux 6.0+)
- [x] `scripts/build-stormblock-initramfs.sh` — minimal initramfs (busybox + stormblock + ublk_drv + /init)
- [x] `install-fedora-iscsi.sh` — 8-phase mkube CI job (provision, format, install Fedora, configure, verify)
- [x] `systemd/stormblock-ublk.service` — post-switch_root safety net

### Registry-scale export path — DONE (issues #22, #24, #25, #26)

Driven by the stormblock-registry / stormblockmk design: a CoW clone per
container instance means thousands of volumes, each needing an export, each
reclaiming space when dropped.

**#22 — thin/CoW volumes exportable as iSCSI LUNs**
- [x] `LunBacking::Volume { volume_id }` — resolve via `VolumeManager::get_volume`
- [x] Persist LUN↔backing mappings to `<data_dir>/luns.json`, restore on startup

**#24 — scale to 1000s of LUNs**
- [x] `AppState::lun_entries`: `Vec<LunEntry>` → `HashMap<u64, LunEntry>` (O(1) lookup)
- [x] Drop the per-SCSI-command `list_luns()` Vec allocation (only REPORT LUNS needs it)
- [x] REPORT LUNS: full LUN LIST LENGTH when truncated, SELECT REPORT handling, >255 LUNs
- [x] `/api/v1/exports` reports the assigned LUN/NSID and goes active; auto-assign on create
- [x] Scale test at 1000 LUNs (attach + dense sorted numbering) and REPORT LUNS at 2000

**#25 — UNMAP/discard → GEM/slab reclaim**
- [x] VPD 0xB2 Logical Block Provisioning (LBPU/LBPWS/LBPRZ, thin) — without it Linux never issues discards (root cause of monotonic growth)
- [x] Data-out collection for UNMAP/WRITE SAME — second root cause: UNMAP's parameter list was never read
- [x] VPD 0xB0: optimal unmap granularity + alignment from `BlockDevice::discard_granularity()`
- [x] WRITE SAME(16)/(10) with UNMAP bit
- [x] Reclaim reporting: slab allocated/free gauges sampled on `/metrics`

**#26 — NVMe-oF dynamic namespaces + advertised address**
- [x] `add_namespace_dynamic(&self)` / `remove_namespace(&self)` — interior mutability like iSCSI
- [x] `management.advertised_addr` config; AttachInfo + discovery log page stop reporting 127.0.0.1

Not covered (would need a live initiator on rose1 to verify): NVMe-oF
namespace-scale benchmarks, and steady-state memory profiling at 1000 LUNs.

### Preformatted filesystem templates — DONE (issue #38)

*mkfs once, clone forever.* Moved into core from the mk profile and
stormblock-registry, and made generic: the consumer list is RouterOS
containers, StormOS, Proxmox VMs, microVMs and x86 hosts, only one of which
is RouterOS. A profile should carry platform *choices*, not
platform-independent capabilities.

- [x] `src/fs/ext4.rs` — the seam onto
      [`mkfs-ext4`](https://github.com/glennswest/mkfs.ext4.rs), a from-scratch
      async `mke2fs`/`e2fsck` in pure Rust: `VolumeDevice` (thin volumes format
      in place, zeroing is a discard), `format`, `read_layout`, `check` /
      `repair`, `stamp_uuid` / `stamp_label`. Replaced the hand-rolled writer
      that shipped first.
- [x] `src/fs/template.rs` — create → format → seal → clone lifecycle over
      the `VolumeManager`, persisted to `<data_dir>/fstemplates.json`
- [x] Seal guard: every flag a consumer acts on (`VALID_FS`, `ERROR_FS`,
      `RECOVER`, `ORPHAN_FS`) **plus a real fsck** — a superblock that says it
      is clean is a claim, not evidence (stormblock-registry#10)
- [x] Every clone fsck'd before hand-off, and discarded if it does not check
      out; `POST /api/v1/volumes/{id}/fsck` (`?repair=true`) for any volume,
      since RouterOS has neither an fsck nor a clean unmount
- [x] Clone-time UUID stamping (stormblockmk#12) — the piece that can only
      live here, since every consumer clones *through* the engine
- [x] Per-template features in `mke2fs` vocabulary: kind (`ext2`/`ext3`/`ext4`),
      `journal` tri-state, and an `-O` list. The default is what
      `mke2fs -t ext4` writes — journal, `flex_bg`, `64bit`, `metadata_csum`,
      `metadata_csum_seed` — which is also what RouterOS's own `format-drive`
      produces (#39)
- [x] Nothing holds a lock across a format, a check or a stamp: templates
      build and clones mint concurrently
- [x] `/api/v1/fstemplates` (create/list/get/seal/clone/delete) and
      `from_template` on `POST /api/v1/volumes`
- [x] `ci-fstemplate-verify.sh` — e2fsck + real mount through an iSCSI
      initiator, on a real kernel. **Passing** on dev.g8.lo (Fedora 6.17.1):
      four clones attached at once, `blkid`, `e2fsck -fn`, mount rw, write,
      unmount, check again, no kernel complaint
- [x] The volume reports its logical sector size, so blocks are never smaller
      than the sectors underneath them (#40) — the failure that passed fsck
      and refused to mount

Deliberately **not** here: writing image *content* into a filesystem (tar,
whiteouts, hashing, image config) stays with the consumer that owns the
content — stormblock-registry keeps its full ext4 writer for that.

Seeding *content* into a template before sealing (skeleton rootfs, kernel
cmdline, `boot.toml`) is done — `src/fs/files.rs` writes files into a volume
the engine already holds through
[`fio-ext4`](https://github.com/glennswest/fio.ext4.rs), with no mount, no loop
device and no attach. fio.ext4.rs#1 is resolved: the crate declares its own
`mkfs-ext4` by git rather than by sibling path, so it can be taken as a git
dependency. Both pins track **v1.2.0** and must move together, so cargo
resolves one copy of `mkfs-ext4` and the two crates agree on the `BlockDevice`
trait.

Not done: `boot_iscsi.rs` cloning its ESP and root from templates rather than
constructing them each time.

**Confirmed on RouterOS (2026-08-13, #39 closed).** A clone of a v8.2.0
template attached over NVMe-TCP takes writes: `/file add` succeeds, and the
disk table corroborates it rather than the return code alone — free space
234 438 656 → 234 434 560 (one 4 KiB block) and free inodes 65 524 → 65 523.
The geometry shows the new profile: 65 536 inodes against the old 16 384, and
~32 MB less free space on the same 256 MiB volume, which is the journal. The
clone carried a fresh UUID with its checksums still valid, so
`metadata_csum_seed` held the stamp to one superblock write rather than the
structural rewrite that was the risk. Verified against stormblockmk v0.7.0
with six templates, 64m–10240m, each built in 0.06–0.81 s.

---

## Session 2026-08-18 — ext4 crate releases (v9.2.0 → v9.2.1)

`mkfs-ext4` v1.3.0 and `fio-ext4` v1.3.0, then `fio-ext4` v1.3.1. Two things
worth not re-deriving:

- **Both pins move together.** `fio-ext4` pins `mkfs-ext4` by tag itself, and
  two different tags are two cargo source ids — cargo then resolves two copies
  and the `BlockDevice` trait from one does not satisfy the other. Release
  order is `mkfs.ext4.rs` → `fio.ext4.rs` → here. Check what tag the `fio-ext4`
  tag you are taking depends on: v1.3.1 pins `mkfs-ext4` v1.3.0, which is why
  only one pin moved for this release.
- **`fio-ext4` v1.3.1 is a correctness fix that our own checking could not
  see.** An extent leaf's checksum went at the end of the block rather than at
  `EXT4_EXTENT_TAIL_OFFSET`; the offsets coincide at 1 KiB and 4 KiB and differ
  at 2 KiB, 8 KiB and 32 KiB. A template built on 2 KiB blocks passed our
  `fsck` and its content digest and was still refused by the kernel with EIO.
  The lesson for template work: our reader agreeing with our writer proves
  nothing — `e2fsck` 1.47.3 and a real mount on `dev.g8.lo` are the check that
  counts.

---

## Session 2026-08-18 — issue sweep (v8.2.1 → v9.2.0)

Nine issues closed. What each one turned out to be, so the next session does
not re-derive it:

- **#46** (32 MiB template clone fails verify) — not a size-specific defect.
  The same copy-on-write short-copy fixed in v8.2.1 (`8dc3134`), seen from the
  clone side: at that geometry the root directory's data block lands in the
  second half of the first 4 MiB slot, and a copy that stopped at 2 MiB left
  the inode table intact with the directory blocks reading as zeros. Regression
  test runs at 4 MiB and 8 MiB slots because every earlier test used the 1 MiB
  default, under tokio's cap.
- **#47** (template volumes leaked) — three leaks: the scratch volume outlived
  a successful create, a create that failed at seal left both halves, and
  `DELETE` kept the volumes by default. **v8.3.0 changed the DELETE default**;
  `?purge=false` restores the old behaviour.
- **#48** (failed discard reported as discarded) — `let _ = delete_volume(…)`
  in three places. Now retries once and returns `TemplateError::Leaked` with
  the volume id, which is the only handle that exists since a clone carries the
  *caller's* name. The orphan sweep's in-use set is a **required argument**, so
  it cannot be forgotten.
- **#41** (sequential heartbeat) — fixed by concurrency *and* by giving each
  probe its own deadline: the cluster HTTP client's timeout is 10 s, ten
  heartbeat intervals, which is what let one wedged peer swallow a round.
- **#19** (ublk never learns a new size) — the real find was that
  `UBLK_U_CMD_GET_FEATURES` is `_IOR`, not `_IOWR`. Encoded wrongly it errored,
  and for a *feature query* an error is indistinguishable from "no such
  feature", so `UBLK_F_UPDATE_SIZE` was never negotiated on a kernel that has
  it. **Only the on-metal test could have found this.** `resize_volume` is now
  grow-only; `shrink_volume` is the explicit door.
- **#18** (pool growth on pressure) — sources are configured, never discovered.
  A `directory` source only creates new files and is also how to grow into the
  free tail of the node's own disk. A `device` source already carrying a slab
  is **adopted with its data**, not reformatted.
- **#20** (volume move) — offline, filesystem-level, two calls. The copy pipes
  `pack_tar` into `unpack_tar` over a bounded channel driven by one `join!`:
  no scratch file, fixed memory, and tar is what preserves hard links and
  xattrs (SELinux labels). Restartable, **not** resumable mid-copy.
- **#35 / #34** — `qos_class` taxonomy pinned; CSI wire fixtures vendored into
  `contract/` and round-tripped. They passed as copied: the two sides already
  agreed. Note the stale-epoch *message wording* differs between the fixture
  and what the engine emits — only `current_epoch` is contractual.
- **#31** (iSCSI MC/S) — the whole job was moving **CmdSN to the session** and
  leaving StatSN on the connection (RFC 7143 §4.2.2.1). Also fixed a live bug:
  any connection closing used to tear down the whole session.

### Still open, and why

All of them are blocked on something outside this repo:

- **#44** (StormKV for the GEM), **#42** (SWIM gossip) — need work in StormKV
  first; #42 wants `stormkv-gossip` extracted into a shared crate.
- **#36 / #30** — need a box with a real NVMe namespace. See below.
- **#17**, **#16** — other repos.
- **#15**, **#7**, **#6**, **#5**, **#3**, **#2** — need the multi-node lab.
  The `/v1` API layer for #5/#6/#7 is done; only the engine data path is not.

### #36: the 5.4x inversion does not reproduce, but this rig cannot say more

`examples/qd_sweep.rs` measures 4K random write against a `ThinVolume`
directly — no iSCSI or NVMe-oF, since the transport is not what #30 is about.

**Read the harness's own noise floor before believing any number from it.**
The first two single-pass runs on dev.g8.lo disagreed with each other: one put
the warm peak at QD32 with no inversion, the next at QD1 with a 0.85x
"inversion". That is run-to-run variance, not a depth effect, and reporting
either would have been reporting noise. The harness now samples each depth
`--repeats` times with the passes **interleaved** (so host drift cannot land on
one depth) and reports the median with the spread beside it, calling the result
`INCONCLUSIVE` when the depth effect is inside the spread.

What is established:

- **No 5.4x inversion.** Every depth on this rig sits within a narrow band of
  every other; an effect that size would be unmissable.
- **CPU-seconds per million I/Os is flat (~22–26) across all depths, in every
  run.** That is the stable number and the discriminating one: lock contention
  makes the per-operation cost *rise* with depth. It does not.

What is **not** established: the backing is a QEMU virtual disk, not an NVMe
namespace, and the ~10–14k IOPS ceiling is probably that disk. So this says
nothing about the engine on real NVMe, and #36 stays open for exactly that.
#30 has lost its main justification and should be re-argued on its own merits
rather than treated as confirmed.

---

## Pallets — engine support (2026-08-19, #51/#52)

A **pallet** is a GPT partition holding a named, versioned, self-contained set
of sealed member images plus the manifest that describes them. On-disk format
is specified in `stormuefi/docs/PALLET-SPEC.md` v1; the reader is built and
OVMF-verified in `stormuefi-map`. stormblock owns the **producer** side.

The engine had no notion of one: a drive carried a slab and nothing else, so
there was effectively a single implicit grouping. What the model needs is
**many pallets, several per drive, spread over several drives**, discovered by
scanning rather than configured.

- [x] `src/pallet/format.rs` — v1 writer + reader, byte-compatible with
      `stormuefi-map`. Content first, header last, so a torn publish leaves a
      pallet that fails its own CRC rather than one that lies.
- [x] `src/pallet/gpt.rs` — GPT read/write, protective MBR, primary + backup.
      Activation is an **attribute write** (bits 48–63), never a data write.
      Allocation is first-fit in free space, 1 MiB aligned, and refuses to
      alias — firmware does not publish a handle for an overlapping entry.
- [x] `src/pallet/store.rs` — discovery across every opened drive; selection
      order (priority desc, version desc) with the spec's candidate rule.
- [x] `src/pallet/manager.rs` — the lifecycle library (#52): compose, publish,
      verify, activate, mark-successful, roll back, prune with keep-N-1.
- [x] `/api/v1/pallets` + `stormblock pallet` CLI.
- [x] `docs/pallets.md`.
- [x] `src/pallet/select.rs` — the read-only half, as pure functions plus a
      `PalletBrowser` that cannot write. This is what a firmware or initramfs
      consumer holds, and what stormuefi mirrors.
- [x] A pallet **kind** (boot/system/kernel/kube/app/runtime/data) and a
      version label, in the superblock's reserved area, zero meaning
      "unspecified". Priority orders only pallets of the same kind.
- [x] Moves: a whole pallet between drives keeping its identity, and one member
      between pallets as a new version of each.
- [x] Whole-drive pallets (one pallet per device, no GPT) stay discoverable and
      `adopt_whole_drive` migrates them onto a partitioned drive.
- [x] `convert_drive(src, dest)` — the whole-drive operation: copy every pallet,
      verify each at the destination, remove from the source, optionally hand
      the source a fresh table. Refuses to wipe a source that is still the only
      copy of something that failed to convert.

### Image building (2026-08-19)

`stormblock image build` assembles disk images and ISOs out of pallets —
`src/image/`, [docs/images.md](docs/images.md), verified by
`ci-image-verify.sh`. An image file is a drive, so the builder drives the
ordinary `PalletManager` rather than reimplementing publishing.

**Two things external tools found that ours could not**, and the reason
`ci-image-verify.sh` exists:

- `mtools` showed `BOOTX64.EFI` stored as `BOOTX6~1`: the FAT name sanitiser
  replaced the dot before splitting the extension, so every plain 8.3 name
  became a long one.
- `xorriso` showed the El Torito boot image saturating its 16-bit sector count.
  **FAT32's 65,525-cluster floor (~33 MiB) sits just above El Torito's 32 MiB
  ceiling** — the two do not overlap, which is why `fat.rs` writes FAT16 too.
  An ESP for an ISO must be ≤ 32 MiB.

**Learned, and worth not re-deriving:** the GPT LBA size is *not* the device's
block size. A file device reports 4096 because that is the I/O size it prefers;
an image assembled as a file needs 512, which is what every tool and firmware
assumes. A 4Kn table on an image is one our own reader accepts and `fdisk`
cannot find — so the check that counts is an external one, byte by byte.

Not covered here, and still open on #51: volume-level sealed/read-only attach
refusal, and per-leg physical offsets for a read-only consumer that must not
reconstruct RAID.
