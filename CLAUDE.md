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
- `src/drive/` — BlockDevice trait: NVMe via VFIO (`nvme.rs`), SAS via io_uring (`sas.rs`), DMA buffers (`dma.rs`)
- `src/raid/` — Software RAID 1/5/6/10: SIMD parity (`parity.rs`), write journal (`journal.rs`), rebuild (`rebuild.rs`)
- `src/volume/` — Thin provisioning (`thin.rs`), extent allocator (`extent.rs`), COW snapshots (`snapshot.rs`)
- `src/target/` — NVMe-oF/TCP :4420 (`nvmeof.rs`), iSCSI :3260 (`iscsi.rs`), per-core reactor (`reactor.rs`)
- `src/mgmt/` — REST API via axum (`api.rs`), TOML config parsing (`config.rs`)
- `src/main.rs` — CLI entry point, startup sequence (currently just scaffolding)

## Current State
Phase 1 (drive layer) is implemented. The drive layer has three backends: SAS (io_uring, Linux), NVMe (VFIO, stub only), and FileDevice (tokio, portable). Builds and tests pass on macOS and Linux (devx.gw.lo). Remaining modules (raid, volume, target, mgmt) are still scaffolding.

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
- [ ] `dma.rs` — Hugepage-backed slab allocator for VFIO (future, needs bare metal)
- [x] `nvme.rs` — Struct definitions (NvmeDevice, IoQueuePair, SQ/CQ entries, registers)
- [ ] `nvme.rs` — VFIO init, BAR0 mapping, queue pairs (needs bare metal hardware)
- [x] `sas.rs` — Open /dev/sdX with O_DIRECT, detect SSD/HDD, read serial/model from sysfs
- [x] `sas.rs` — io_uring read/write/flush/discard
- [x] `filedev.rs` — NEW: Portable tokio file I/O fallback (MikroTik, dev, testing)
- [x] `mod.rs` — Drive enumeration: auto-detect block device vs file, open appropriate backend
- [x] `main.rs` — Wired up drive init with `--device` CLI flag
- [ ] Drive health monitoring (SMART via NVMe admin commands / SG_IO)

### Phase 2: RAID engine (`src/raid/`) — DONE
- [x] RAID superblock format (on-disk metadata: member drives, layout, state)
- [x] RAID 1 (mirror) — read balancing, write duplication
- [x] RAID 5 — stripe layout, XOR parity compute
- [x] RAID 6 — dual parity (P + Q, GF(2^8) multiplication)
- [x] RAID 10 — striped mirrors
- [x] `parity.rs` — SIMD XOR: AVX2 (x86_64), NEON (aarch64), scalar fallback
- [x] `parity.rs` — GF multiply for RAID 6 Q syndrome (AVX2 shuffle, NEON vtbl)
- [x] `journal.rs` — Write-intent bitmap: mark dirty stripes before write, clear after
- [ ] `journal.rs` — Journal recovery on startup (partial stripe detection)
- [x] `rebuild.rs` — Background rebuild: read surviving members, recompute parity/mirror
- [x] `rebuild.rs` — Rate limiting (don't starve foreground I/O)
- [ ] Scrub/verify (background read + parity check)

### Phase 3: Volume manager (`src/volume/`)
- [ ] On-disk metadata format (extent tree, volume table, snapshot DAG)
- [ ] `extent.rs` — Free-space bitmap, extent allocation (first-fit or best-fit)
- [ ] `extent.rs` — Extent deallocation, coalescing
- [ ] `thin.rs` — Thin volume: virtual-to-physical extent mapping
- [ ] `thin.rs` — On-demand allocation on first write (allocate-on-write)
- [ ] `thin.rs` — Discard/TRIM handling (return extents to free pool)
- [ ] `snapshot.rs` — COW snapshot creation (clone extent map, bump refcounts)
- [ ] `snapshot.rs` — Snapshot deletion (decrement refcounts, free unreferenced extents)
- [ ] `snapshot.rs` — Snapshot diff (for incremental backup)
- [ ] Volume resize (grow/shrink)

### Phase 4: Target protocols (`src/target/`)
- [ ] `reactor.rs` — Per-core event loop: epoll/io_uring for network + drive completions
- [ ] `reactor.rs` — Core affinity, CPU isolation integration
- [ ] `nvmeof.rs` — NVMe-oF/TCP PDU parsing (ICReq, ICResp, CapsuleCmd, CapsuleResp, C2HData, H2CData)
- [ ] `nvmeof.rs` — NVMe-oF discovery subsystem (log pages)
- [ ] `nvmeof.rs` — NVMe-oF I/O subsystem (connect, read, write, flush, dsm)
- [ ] `nvmeof.rs` — io_uring zero-copy send for C2H data
- [ ] `iscsi.rs` — iSCSI PDU parsing (login, text, SCSI command, data-out, data-in)
- [ ] `iscsi.rs` — iSCSI login negotiation (discovery, normal sessions)
- [ ] `iscsi.rs` — CHAP authentication
- [ ] `iscsi.rs` — SCSI command dispatch (READ_10/16, WRITE_10/16, INQUIRY, READ_CAPACITY, etc.)
- [ ] `iscsi.rs` — Multi-connection sessions, task management
- [ ] MPIO/ALUA support for multipath

### Phase 5: Management plane (`src/mgmt/`)
- [ ] `config.rs` — Parse `stormblock.toml` into typed config structs
- [ ] `config.rs` — Config validation (drive paths exist, ports not conflicting, etc.)
- [ ] `api.rs` — REST routes: `GET/POST /api/v1/drives` (enumerate, add)
- [ ] `api.rs` — REST routes: `GET/POST/DELETE /api/v1/arrays` (RAID create/delete/status)
- [ ] `api.rs` — REST routes: `GET/POST/DELETE /api/v1/volumes` (create/delete/resize/snapshot)
- [ ] `api.rs` — REST routes: `GET/POST/DELETE /api/v1/exports` (NVMe-oF/iSCSI target mappings)
- [ ] Prometheus metrics endpoint (`/metrics`)
- [ ] TLS for management API (rustls)

### Phase 6: Cluster scaling (optional — single-node must work without any of this)
- [ ] Node discovery: new node announces itself via REST to an existing node or seed list
- [ ] Cluster membership store: track known nodes, health, capacity (local JSON or embedded DB)
- [ ] `api.rs` — REST routes: `GET/POST/DELETE /api/v1/cluster/nodes` (list, join, remove)
- [ ] Node health heartbeat (periodic ping between peers, mark unreachable)
- [ ] Raft consensus via openraft (leader election, log replication) for metadata coordination
- [ ] Synchronous replication (write to N replicas before ack)
- [ ] Asynchronous replication (background catchup)
- [ ] Volume migration/rebalance: move volumes between nodes when capacity added
- [ ] Online node addition: join a running cluster, receive replicated volumes without downtime

### Phase 7: Integration & hardening
- [ ] End-to-end test: create array → create volume → export via iSCSI → mount on initiator
- [ ] Crash recovery testing (power-cut simulation)
- [ ] Performance benchmarks (fio via iSCSI/NVMe-oF, compare to kernel LIO)
- [ ] Buildroot image generation (kernel config, initramfs with stormblock binary)
- [ ] StormFS registration (announce volumes to StormFS metadata cluster)
