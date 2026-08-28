# StormBlock Development Guide

## Project Overview
Pure Rust enterprise block storage engine. Turns raw NVMe/SAS drives into network-accessible volumes over NVMe-oF/TCP and iSCSI. Part of the Storm ecosystem (StormBlock, StormFS, StormForce, StormOS).

## Design Principle: Single-node first, scale-out later
StormBlock must be fully functional as a **standalone single-node** storage engine — no cluster requirement. A single node handles its own drives, RAID, volumes, and exports independently. Clustering (replication, Raft) is layered on top and strictly optional. New nodes can be added to an existing deployment at any time without disrupting running nodes.

## Build on dev, never on this Mac

**Every `cargo build`, `cargo test`, `cargo check` and every image build runs on
`root@dev.g8.lo`.** Not on the workstation, not "just to check quickly".

The workstation is macOS and the target is Linux, so the two builds do not
compile the same code. `libc` is a Linux-only dependency here; `io_uring`,
`ublk`, `/dev/kmsg`, `mlockall` and the whole storage path are behind
`cfg(target_os = "linux")`. A macOS build therefore *skips* the code most
likely to be wrong, and it passes while the node's build fails — and the
reverse, where a change that only breaks macOS is pushed because the node built
fine. Both have happened here in one session.

The numbers say it plainly: `cargo test` runs **258** tests on the Mac and
**303** on dev. The 45 that only exist on Linux are the ones covering the parts
that touch hardware.

The workflow is therefore:

```
commit  →  push  →  pull on dev  →  build and test on dev
```

and never a build whose result was not produced on the machine the code runs
on. Editing on the Mac is fine; believing it is not.

## Build
```bash
# Full node (x86_64 — VFIO, io_uring, all features)
cargo build --release --target x86_64-unknown-linux-musl

# ARM64 JBOD head unit
cargo build --release --target aarch64-unknown-linux-musl --features "arm64,iscsi,nvmeof"

# MikroTik RouterOS appliance (NVMe-TCP only — no VFIO, no io_uring, no StormFS)
cargo build --release --target aarch64-unknown-linux-musl --no-default-features --features "mikrotik,nvmeof"

# The embedded management UI is off by default since v12.2.0 (#79):
# stormview is the UI. Add --features ui for the old pages.
```

**NVMe-TCP, not iSCSI.** What StormBlock serves on RouterOS is containers,
PVCs and sbregistry, and those are 100% NVMe because **iSCSI is slow**.
Sharing an iSCSI disk and PXE-booting a bare-metal host are **mkube's**,
already working and unchanged by anything here — the engine does not need
iSCSI to do its own job on this platform. Measured, aarch64 release, since
"the binary must be small" is a real constraint:

| profile | bytes |
|---|---|
| `mikrotik,nvmeof` | 11,034,016 |
| `mikrotik,iscsi` | 11,398,192 |
| `mikrotik,iscsi,nvmeof` | 11,663,136 |

NVMe alone is the smallest of the three — 629 KB below carrying both — so the
fast transport is also the cheap one. Add `iscsi` only for a node that must
serve an iSCSI LUN or run `boot-iscsi` itself.

The profile leaves out `stormfs-data` too: a node with 256 MB is not a StormFS
data node, and a mounted surface invites being called.

**Musl static build** produces an 8.8 MB statically linked, stripped PIE binary (x86_64). Uses rustls-tls (no OpenSSL dependency). Requires `musl-tools` and `musl-dev` packages on the build host. Build and test on Linux: `root@dev.g8.lo:/root/stormblock` (or `gwest@dev.g8.lo` — shared dev host).

## Target Platforms

| Platform | Arch | Drive I/O | Targets | Notes |
|----------|------|-----------|---------|-------|
| Full node (Tier 0) | x86_64 | VFIO NVMe + io_uring SAS | NVMe-oF/TCP + iSCSI | Bare metal, buildroot image |
| ARM64 JBOD (Tier 2) | aarch64 | io_uring SAS | NVMe-oF/TCP + iSCSI | SAS shelf head unit |
| MikroTik RouterOS | arm64/x86 | tokio file I/O (no VFIO, no io_uring) | NVMe-oF/TCP | Container on RouterOS 7+, USB/SATA attached storage, small footprint. iSCSI sharing and PXE boot are mkube's. |

**MikroTik considerations:**
- Runs as a container on RouterOS 7+ (or CHR VM)
- No PCIe passthrough — no VFIO, drives are `/dev/sdX` block devices
- No io_uring on RouterOS kernel — fall back to tokio `AsyncFd` / `spawn_blocking` with O_DIRECT
- Memory constrained (256MB–1GB typical) — no hugepage DMA allocator
- **NVMe-TCP is the transport.** RouterOS 7 speaks it (`/disk add
  type=nvme-tcp ...`), and containers, PVCs and sbregistry all run on it
  because iSCSI is slow. An earlier version of this table said "iSCSI target
  only, NVMe-oF unlikely on these networks" — that was wrong, and a RouterOS
  node was confirmed taking writes over NVMe-TCP on 2026-08-13 (#39).
- **iSCSI sharing and PXE boot are mkube's, not the engine's.** They already
  work and nothing here changes them, so the engine profile does not carry
  iSCSI to support them. That division stands until NVMe boot over iPXE is
  *demonstrated* rather than assumed — nobody has proved it yet, and the boot
  path is not the place to find out by guessing.
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
- [x] **Volume groups — answered by a data pallet** (2026-08-28). The group is
      a pallet of `PalletKind::Data`, named `data1`, `data2` and so on beside
      `system1` and `kernel1`. A pallet is a GPT partition, so it is the hard
      allocation boundary this entry said was required, rather than a
      preference a tier-based policy can fall back out of. The system drive
      carries one; a drive that is not a system drive is mostly these. The
      property wanted was that the system disk can be replaced wholesale
      without touching state, and it follows: a new system pallet is published
      and activated beside a data pallet that was never the same partition.
      Sizing for two generations still applies to the *system* pallet, since a
      rebase transiently holds both.

- [ ] **Placing a claim among many data pallets.** Discovery already supports
      any arrangement — `PalletStore::scan` walks every drive and appends
      everything it finds, so several data pallets on one drive and across
      several drives need no configuration. **Selection is what does not fit.**
      `select()` is `max_by_key` over priority then version: a *ladder*, which
      is the right question for boot, kernel and system, where exactly one
      wins. Data pallets are a **pool**. Nothing selects a data pallet; a claim
      is *placed into* one, and the inputs are free capacity and failure
      domain, neither of which the ladder consults. This wants a new call
      beside `select` rather than a change to it — `select` is correct for what
      it answers, and firmware links the read-only half of that code.
      Blocked on the failure-domain work below for the second input; free
      capacity alone is enough to start.
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

---

## Session 2026-08-20 — StormFS data-path surface (#49, #50) — DONE

Worked the issue list in reverse, in the lane the pallet work (#51–#60) was
not in. #50 is the newer number but sits on top of #49 by its own account
("base chunk lifecycle is the immediate blocker"), so #49 landed first.

- [x] `src/volume/chunk.rs` — chunk lifecycle (#49). A chunk is a run of whole
      slab slots inside one volume, addressed `(volume, offset, len)`.
      `allocate` is **eager and tier-scoped**: StormFS owns which tier,
      StormBlock owns where on it, so slots come from `best_slab_for_tier` and
      the GEM mapping is recorded now rather than left to allocate-on-write,
      which would place the chunk by the *volume's* policy and could fail on
      space this call reported as free. Deallocate is idempotent by
      construction. Trim is the same call with one bit changed: both free the
      slots, only deallocate returns the address range.
- [x] `src/volume/versioned.rs` — the three primitives (#50). CAS and atomic
      multi-block write are **one mechanism**; pins are the existing COW
      retention exposed.
- [x] `src/mgmt/api/stormfs.rs` + spec §9.1.1/§9.1.2, 18 HTTP tests.

### The journal the plan called for was not needed, and that is the finding

The plan above said a swap needs a commit journal in `<data_dir>` to roll a
partial swap forward, on the reasoning that the slab slot table is what
recovery reads. That was wrong about which record is authoritative. **The
durable record of an extent map is the volume metadata file**, written whole
and atomically with a checksum by `MetadataStore` — `rebuild_from_slabs` is
the fallback for when there is no such file, and the existing COW path already
leaves duplicate slot claims that only that fallback would ever see. So a
commit is untearable across a crash for free: the map is written in one piece.
Slots are still re-pointed (`Slab::reassign_slot`) so the fallback agrees, but
nothing hinges on it.

What *does* need care is that versions live in a second file. **The write
order is load-bearing**: versions first, then the map. A crash between them
leaves the version ahead of the map, so a stale writer is told to re-read and
finds the old data — it retries, which costs nothing. The other order leaves
the version behind a map that has already moved, and that writer would commit
over committed data. Versions must be monotonic, not gapless.

Also worth not re-deriving: a `409` from `commit` carries `current_version`,
so the writer never needs a second round trip to ask what it missed — the
same shape as `current_epoch` in the `/v1` surface (#35).

Not done: nothing here has met a real StormFS client. The primitives are
tested against the engine and over HTTP, but the two sides have not been run
against each other, which is the check that counts — the same lesson as the
ext4 work, where our reader agreeing with our writer proved nothing.

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

### Standing clones (2026-08-19, #55)

A sealed template holds one pre-minted clone; `claim` takes it and replenishes
behind the caller. The engine owns it because the engine owns templates,
snapshots and volumes — so the invariant holds without anyone asking, and
stormboot's fast path works before the registry is up.

Three things that are cheap now and expensive later, all from the interim
version in sbregistry:

- **Keyed by the template**, never by a name or tag. A template is derived from
  the manifest digest, so a moved tag needs no detection or repair — the new
  manifest is a new template with its own standing clone.
- **One, not a pool.** A second only helps when two starts of the same template
  collide, and these nodes are memory constrained.
- **Never handed out twice.** `claim` takes the field under the store lock;
  the loser of a race mints its own. Two containers on one writable filesystem
  is the worst outcome available here, and it is silent.

sbregistry's interim implementation (`standing` on `CloneRec`,
`POST /v1/clones/claim`) should be **removed**, not kept in parallel.

**Check and fix are separate verbs.** `GET /api/v1/fstemplates/standby` reports
which templates would make a start wait; `POST` on the same path mints what is
missing. A supervisor must be able to ask whether a node is warm without the
asking making it true. Both are idempotent — safe on every start.

**A take is a take**: an ordinary clone tops the template back up too, not only
a claim. A standby mint is flagged (`CloneSpec.standby`) so it does not count
against `clones` — that number means "how many went somewhere" — and does not
trigger a top-up of itself, which is what would make minting recursive.

### The format's read side is a crate (2026-08-19, #53)

`crates/pallet-format` — `no_std`, no allocation, no async, no I/O, **no write
path**. Firmware links it, so it must stay small enough to read in one sitting
and structurally unable to write. stormblock keeps emission and writes at the
offsets `pallet_format::layout` defines.

The rule this encodes: **one on-disk format may have only one reader.** Two
hand-maintained readers in two repos drift, and the drift fails as *the node
does not boot*. Emission needs no such sharing because there is only ever one
writer.

Check it with `cargo check -p stormblock-pallet-format --target
x86_64-unknown-uefi --no-default-features --features verify` — the `no_std`
claim is verified, not asserted. Its tests work from hand-built bytes on
purpose: a decoder tested only against its own encoder proves nothing.

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

---

## Session 2026-08-26 — NVMe-TCP initiator (#73) — DONE

The engine can now *attach* NVMe-TCP, not just serve it.
`drive/nvmeof_dev.rs` is an initiator `BlockDevice`;
`nvme-tcp://host:port/<nqn>?nsid=N` (and `iscsi://host:port/iqn`) are
accepted everywhere a device path is — `[[drives]]`, `POST /api/v1/drives`,
RAID `add_member`. Proven on dev end to end: engine A attached engine B's
namespace through the API and created a RAID-1 across a local drive and the
remote leg, both members active. This is the cross-node RAID-leg transport
stormstorage's DistVolume model orchestrates.

Worth not re-deriving:

- **Reuse the test initiator's framing.** `tests/common/nvmeof_initiator.rs`
  already carried the initiator direction, proven against this target *and*
  the Linux kernel (the FCTYPE-at-byte-4 lesson lives there). The device is
  that wire logic productionized: admin conn (QID 0) identifies at open and
  is dropped; one I/O conn (QID 1) behind a Mutex, cleared on error so the
  next op reconnects — a bounced remote degrades to per-op errors RAID can
  see (#69 is where those errors should start flipping member state).
- **`DeviceId.uuid` is uuid5 of the attach URI** — stable across reopens.
  Do this for every new backend; #65 is the cost of not doing it.
- **libc's ioctl request type moves.** c_ulong on glibc, c_int on musl, and
  it has changed across libc releases — keep raw u32 consts and cast
  `as _` at the call site (`nvme_smart` broke exactly this way when the
  lockfile advanced).
- **The TOML `[iscsi]`/`[nvmeof]` sections are dead** — targets read the
  CLI values with clap defaults (#75). Two engines on one host need
  `--iscsi-addr/--nvmeof-addr/--nvmeof-nqn` until that's fixed.
- The NVMe-oF target only starts when an `export_device` exists at boot
  (`main.rs:1128`); dynamic namespaces exist (#26) but no boot device means
  no listener at all.

---

## Volume-level redundancy — the RAID the design actually needs (2026-08-28)

**Correction of record.** `src/raid/` is drive-level: `RaidArray` mirrors or
stripes whole member devices and a slab sits on top, so every volume on that
slab gets the same protection and a node can have exactly one answer. That is
not the model. **Redundancy is a property of a volume**, realised by placing
that volume's extents across N distinct physical drives, and a node carries a
mix — `app-data-1` as a two-way mirror, another volume as 4+1 parity, a
golden's clones inheriting the golden's policy — side by side on the same
drives. System and kernel pallets are mirrored as pallets (#56); data is
mirrored or parity-protected per volume. This is what zeroboot installs onto,
so it is the blocking item; drive-level `RaidArray` stays as a leg transport
and for whole-device use, nothing more.

### Design

- **`FailureDomain`** (`placement/domain.rs`): an ordered chain of
  `rung=value` — `site/building/room/row/rack/node/hba/shelf/bay/drive` (#72's
  vocabulary, as a chain not a flat map, so #71 is not built twice). A slab
  carries one; by default it is `drive=<device serial|uuid>`, and a drive
  registered with `labels` (#70 item 1) extends it. Two slabs are *the same
  domain at rung R* when their chains agree through R.
- **`RedundancyPolicy`** (`volume/redundancy.rs`): `none` | `mirror:N` |
  `raid5:D+1` | `raid6:D+2`, plus the rung to spread at (default `drive`).
  Spelled `mirror`, `mirror:3`, `raid1`, `raid5:4+1`, `raid6:4+2`, `raid10`
  (= mirror on organic placement, since striping is what slabs already do).
- **A hard boundary, not a preference.** Every leg of an extent — and every
  data member and parity leg of a stripe — lands on a distinct domain at the
  policy's rung, or the allocation fails. Creation is refused up front when
  the node cannot satisfy the policy at all.
- **GEM**: `ExtentLocation` gains `mirrors: Vec<Leg>`; `legs()` is primary
  plus mirrors. Parity volumes keep one data leg per extent and a per-volume
  `ParityGroup` per stripe (stripe = `data` consecutive virtual extents; P and
  Q legs, own ref count, so a clone shares parity until a COW moves it). The
  reverse index covers every leg, so GC and evacuation see them. Parity slots
  record `PARITY_TAG | leg << 56 | stripe` as their virtual extent so
  `rebuild_from_slabs` can tell them apart. Metadata **V4**.
- **I/O**: mirror writes go to every leg and ack when all healthy legs have
  it; reads pick a leg and fall through on error. Parity writes are
  read-modify-write under a per-stripe lock; a lost data slot is
  reconstructed from the stripe. A leg whose write fails puts its slab in
  the volume's **failed set** (persisted): skipped for reads and writes,
  volume reports *degraded*, until `resync` rebuilds every leg that was on
  it onto a fresh domain and clears it. That is also how `none → mirror:2`
  and `mirror:2 → mirror:3` are applied: set the policy, resync. The RAID-5
  write hole is the same as md without a journal; `resync?verify=true`
  recomputes parity.
- **Clones inherit** the source's policy: shared extents are already
  replicated, and every COW re-replicates.
- **Surface**: `redundancy` + `spread` on `POST /api/v1/volumes`, on
  `TemplateSpec`, on `[[volumes]]`; `redundancy` and `health` on every volume
  response; `PUT /api/v1/volumes/{id}/redundancy`; `POST
  /api/v1/volumes/{id}/resync`. Slabs carry `domain`; drives accept `labels`
  and `uuid` on `POST /api/v1/drives` and list their slabs (#70 items 1–2).
- **Out of this cut**: chunk/versioned (StormFS) volumes stay `none` — StormFS
  replicates above; converting to or from parity (a restripe); drain over
  HTTP (#70 item 3) and the health inbound (#70 item 4).

### Work plan — DONE (v10.0.0)

- [x] `placement/domain.rs` + registry domain tracking + domain-aware best-slab
- [x] `volume/redundancy.rs`
- [x] GEM: legs, parity groups, reverse index, rebuild
- [x] metadata V4 (V3 shape kept, converted on load)
- [x] every consumer of a location frees/shares *all* legs
- [x] thin.rs: mirror + parity paths, failed set, health
- [x] VolumeManager: create options, inherit, persist/restore, resync, set policy
- [x] HTTP + template + config surface; drives `labels`/`uuid`, `/drives/{id}/slabs`
- [x] tests: mirror across two slabs, degrade, resync; parity 2+1 reconstruct;
      clone COW keeps policy; insufficient domains refused; V3 → V4 load;
      RAID-6 two-member loss; restart; set+resync
- [x] docs/redundancy.md, CHANGELOG, README; build + test on dev
- [x] pallets: `copies` on publish, legs reported in status, resync (#56)

### Worth not re-deriving

- **A removed slab has no domain, and an empty domain must constrain
  nothing.** The first resync test failed with "no slab apart from 2
  domains" because the *lost* slab's empty chain was in the exclusion set
  and `same_at` treats unknown as shared. Right for a candidate (never
  place on a slab you cannot tell apart), wrong for an exclusion.
- **The reverse index has owners.** `insert` used to drop the reverse
  entries of the old location unconditionally — so a clone COWing an
  extent took the *source's* slot out of the index. Every removal now
  checks ownership. This was pre-existing and would have made evacuation
  miss shared slots.
- **Restore precedence.** "Slot table wins" only worked by iteration order:
  two slots for one extent (a COW's old and new) both had generation 1.
  `allocate_gen` records the COW generation, and restore takes the record
  unless the slot table is provably newer.
- **Lock order.** Redundant writes take the extent/stripe shard *before* the
  volume lock; `discard` therefore must not take the volume lock for a
  redundant volume. Parity never takes the volume lock at all.
- **`sync_refs` after `dec_ref`.** The GEM's `ref_count` on the *owner* is
  otherwise never lowered when a clone diverges, so the owner COWs for
  nobody forever; for parity that also meant the source's group never
  went back to in-place RMW.

### Follow-on — the pieces left out of v10.0.0 (2026-08-28) — DONE (v11.0.0)

- [x] **Drain over HTTP** (#70 item 3): `POST /api/v1/drives/{id}/drain` → a
      background task moving every leg off every slab on that device, one
      extent at a time (locks per extent, so I/O keeps flowing), progress at
      `GET …/drain`, terminal `empty` = safe to remove. Slabs being drained
      (or quarantined) take no new allocations.
- [x] **Health inbound** (#70 item 4): `POST /api/v1/drives/{id}/health` with
      a stormdrive report → quarantine the drive's slabs for placement and
      put them in the failed set of every *redundant* volume with a leg
      there (an unreplicated volume's only copy stays readable); `failed`
      orders a drain.
- [x] **Rebalance by failure domain** (#71 item 3): fix legs that collide at
      a rung (placed before labels existed) and even out allocation across
      domains after a shelf is added.
- [x] **Topology as a chain** (#72 item 1 remainder): `[management].topology`
      feeds the node rungs of every slab's domain and /v1 reports the chain.
- [x] **Dirty-stripe log** for the parity write hole: mark a stripe before
      its read-modify-write, clear lazily, verify only the dirty stripes on
      restart.
- [x] **Restripe**: change a policy to or from parity by copying into a new
      placement and swapping the map; refused while exported.

### #76 — a template is a volume that has been sealed (2026-08-28) — DONE (v12.0.0)

Lineage, sealing and filesystem identity move onto the **volume**:

- `VolumeRecord` (metadata **V5**) gains `parent`, `sealed` and `fs`
  (kind, journal, features, 64bit, metadata_csum, csum_seed, label, uuid).
  `create_snapshot` records the parent and inherits `fs`.
- A sealed volume refuses writes, discards and shrinks — sealing is a state
  transition, not a snapshot into a second object. `fs::template::seal`
  seals the raw volume **in place**: one template is one volume, so the
  `-raw` half that leaked (#47) no longer exists.
- **Cloning always stamps.** `fs::clone_volume(vm, source, spec)` is the one
  clone: snapshot, fresh filesystem UUID when the source carries a
  filesystem, fsck, lineage recorded. `clone_template`, the volume snapshot
  API and the /v1 `source: volume` path all go through it.
- `POST /api/v1/volumes/{id}/seal`, `POST …/{id}/clone`, `GET …/{id}/lineage`;
  `parent`, `sealed`, `fs` on every volume response; `from_template` on
  `POST /api/v1/volumes` also accepts a sealed volume by id or name — the
  blank-ext4-built-into-the-image case that was in neither namespace.
- The `FsTemplate` store stays as the HTTP view (name, standing clone,
  clone count); everything that is a property of the filesystem or of
  lineage is read from and written to the volume record. Persisted
  templates are adopted at startup: their sealed volumes are marked sealed
  and given their `fs`.
- **#78, same split one layer up:** attach lived only on `/v1`, whose
  volume registry is a second store, so a clone made through `/api/v1`
  had no path to a block device. `POST /api/v1/volumes/{id}/attach` is
  the volume-level door (same `AttachInfo`, same ublk/NVMe machinery,
  no epochs or fencing — that stays `/v1`'s contract). The natural end
  of this line is `/v1` becoming a view over engine volumes rather than
  its own map; not done.

### Follow-on 2 (2026-08-28): #77, #72, #79 — DONE (v12.2.0)

- [x] **#77** — `stormblock image build` seals every golden it lays down
      (and records its `fs`), so a blank arrives cloneable and the claim
      path asserts instead of repairing.
- [x] **#72** — the discovery beacon carries the node's topology chain, so
      `/v1/nodes/capacity` reports `topology_chain` for peers too.
- [x] **#79** — dependency cut (335 → 212 default, 262 → 186 RouterOS; `hyper`/`hyper-util` stay direct — they cost nothing beside axum): one hyper-based HTTP client instead of
      `reqwest`; `/metrics` rendered on the axum route instead of
      `metrics-exporter-prometheus`; no direct `hyper`/`hyper-util`; `ui`
      off by default; parse-only TOML. Measure before and after.

### Kubernetes-shaped resources, served by the engine (2026-08-28, #80)

Every component serves its own resources (Glenn: "the kube resources should
be in each component"). stormblock: `/apis/storage.storm.io/v1/{volumes,
slabs,drives,nodes}` — `apiVersion/kind/metadata/spec/status`, API
discovery at `/apis` and `/apis/storage.storm.io/v1`, `?watch=1` as a
newline-delimited event stream, writes on `Volume.spec` (redundancy,
sealed, resync) and `Drive.spec` (labels, drain) only. `metadata.name` is
the uuid — engine names are not unique — with the human name in
`spec.name` and `metadata.labels["storm.io/name"]`; get accepts either.
Read-mostly projections of the same state the REST API serves: no second
store.

- [x] `src/mgmt/api/kube.rs` + tests (v12.3.0); stormdrive 0.6.0 serves `drives`/`enclosures`
