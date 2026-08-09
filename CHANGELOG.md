# Changelog

## [Unreleased]

## [v7.1.0] — 2026-08-09

### 2026-08-09
- **perf:** `V1State::save()` rewrote the entire control-plane state as pretty JSON on every mutation, making each operation O(total volumes) — measured at ~0.017 ms per existing volume, extrapolating to ~17 ms per clone at 1000 volumes and ~85 ms at 5000, which is the scale the registry model targets (#32). It now diffs against a cached copy of what is on disk and appends only the changed entries, rewriting the full snapshot once every 512 records. Call sites are unchanged.
- **perf:** clone latency is now flat in the number of existing volumes (0.64 / 0.62 / 0.69 ms at ~21 / ~42 / ~63 volumes, previously 0.81 / 1.09 / 1.51), and clone-and-attach p50 fell from 3.14 ms to **1.46 ms** — inside the 1–2 ms budget #4 asks for. Every operation roughly halved: clone 1.65 → 0.80 ms, attach 1.48 → 0.67 ms, delete 1.52 → 0.68 ms.
- **note:** durability is deliberately unchanged — the journal append is flushed and synced before `save()` returns, exactly as the full rewrite was. The alternative debounce approach would have traded away a property the CSI contract depends on. An append failure falls back to a full rewrite rather than dropping the change. Records are whole-entity upserts, which makes replay idempotent and compaction crash-safe: the snapshot is written first and the journal dropped second, so a crash in between re-applies entries the snapshot already holds. A torn final record stops replay and keeps everything before it.
- **test:** `ci-clone-attach-bench.sh` — clone/attach/reset/delete p50 and p99, plus latency against volume count.


## [v7.0.0] — 2026-08-09

### Breaking
- **BREAKING:** the Global Extent Map and Slab Registry moved from `Mutex` to `RwLock`, changing public signatures: `AppState::new`, the `AppState.gem` / `AppState.slab_registry` fields, `VolumeManager::gem()` / `registry()`, and `ThinVolumeHandle::new` now take `Arc<tokio::sync::RwLock<_>>`. Callers holding these types must switch `.lock().await` to `.read().await` or `.write().await`. Binary consumers are unaffected; only code linking stormblock as a library needs updating.

### 2026-08-09
- **perf:** GEM and SlabRegistry were each behind a `Mutex` shared by *every* volume, so an extent lookup on one volume blocked I/O on all of them. Both are now `RwLock`, so the read-dominated hot path (extent lookup, slab resolution — done per 4 KiB chunk) runs concurrently.
- **perf:** `ThinVolumeHandle::write` held the per-volume lock for the whole call, serialising every write to a volume regardless of which extent it touched. The lock is now taken only when the mapping actually changes (allocation or COW), and those paths re-read the extent under it — two writers can both observe "unmapped" before either allocates, and without that re-check the second would allocate a duplicate slot and discard the first writer's data. Steady-state writes to an exclusively-owned extent take no volume lock at all.
- **fix:** `--reactor-cores` did nothing. Both targets accepted a `&ReactorPool` and ignored it (the parameter was named `_reactor`), while `main` built a pool from the flag, never referenced it, and dropped it at shutdown — so every connection ran on the ambient runtime and the log showed a configured pool immediately followed by a throwaway single-core one. Connections now dispatch onto one shared pool, kept alive for the process lifetime. Verified on a 4-core host: `Reactor pool started: 4 cores, pin=true` / `Target connections dispatch across 4 reactor core(s)`, with the throwaway pool gone.
- **test:** concurrent first-writes to a single extent allocate exactly one slot and leave it untorn (this fails without the re-check), and concurrent writes to distinct extents all land correctly.


## [v6.6.0] — 2026-08-09

### 2026-08-09
- **fix:** `FileDevice::discard` was a no-op, so freeing a slot reclaimed it inside the slab while the backing store kept every byte it had ever written — measured live, allocation went 72 → 116 MB and never came down (#28). Regular files are now punched with `fallocate(PUNCH_HOLE|KEEP_SIZE)` (KEEP_SIZE preserves the apparent length so slab offsets stay valid) and block devices get `BLKDISCARD`. A device that cannot discard is treated as success rather than an error — the range is still logically free.
- **fix:** both reclaim routes now reach the device: `Slab::free()` and `dec_ref_batch()` discard the freed slots, coalescing contiguous runs into one call. Fixing only one would have left dropped clones leaking on the other. (#28)
- **fix:** NVMe-oF Set Features acked without filling completion DW0, so the host read a grant of zero — 0-based for "one queue" — producing `creating 1 I/O queues` and one core's worth of completions for the whole namespace. FID 0x07 now grants `min(requested, max_io_queues - 1)` and returns `(ncqa << 16) | nsqa`; Get Features reports the maximum. Verified live: a 4-core host went from `creating 1 I/O queues` to `creating 4 I/O queues`. (#27)
- **feat:** `GET /api/v1/sessions` — active iSCSI sessions with TSIH, ISID, initiator/target name, discovery flag and connection count, plus `stormblock_iscsi_sessions_total` / `_active` gauges. `active` excludes discovery sessions, which never address a LUN, so it is the number to check before withdrawing an export; counting them would make an idle target look busy. Consumers previously had to guess with a drain timer and could pull a LUN out from under a live mount. (#29)
- **fix:** the full-feature connection is now registered on its session, so the reported connection count reflects reality instead of always being zero (#29)


## [v6.5.1] — 2026-08-09

### 2026-08-09
Both fixes below were found by pointing a real `open-iscsi` initiator at the
target for the first time (Fedora 43, kernel 7.1.4). Neither could have been
caught by the existing tests: our own initiator issues one command at a time,
and the external iSCSI suite exercises *our initiator* against LIO rather than
our target.
- **fix:** iSCSI discovery never worked — the target echoed the declarative `SessionType` key back in the login response, and open-iscsi aborts the login on an unexpected key (`couldn't recognize text SessionType=Discovery`). `SessionType` is initiator-to-target only (RFC 7143 §12.21); it is now recorded as `SessionParams::discovery_session` instead of reflected. RouterOS never hit this because it is configured with an explicit IQN and skips discovery.
- **fix:** any iSCSI write larger than the immediate-data limit could kill the session. `receive_data_via_r2t` assumed the next PDU after an R2T belonged to that transfer; an initiator with several commands in flight interleaves them, so another command arriving mid-write was treated as a protocol error and the connection was dropped (`expected Data-Out PDU`, then a reconnect every 2s). Interleaved PDUs are now parked in a queue drained by the full-feature loop — mirroring what the NVMe-oF side already did for H2CData — and NOP-Out is answered inline so a long write cannot delay a keepalive past its timeout.
- **test:** `ci-iscsi-reclaim.sh` — end-to-end proof for #25 over a real initiator: ext4 → fill → delete → `fstrim`, watching the engine's own slab accounting. Verified on Fedora 43: `discard_max_bytes` is non-zero (so VPD 0xB2 advertising works), and allocation went 0 → 360 MB → 56 MB with 304 MB reclaimed.
- **test:** `ci-nvmeof-hotadd.sh` — Linux suite plus live hot-add against a real kernel `nvme_tcp` initiator; verified a hot-added namespace appears on an already-connected controller with no reconnect, and is withdrawn on detach.

## [v6.5.0] — 2026-08-08

### 2026-08-08 (reset primitive)
- **feat:** `POST /v1/volumes/{id}/reset` — discard a clone's divergence and return it to its source's contents without recreating the volume. Delete-and-reclone costs two reference updates for every extent in the golden image; reset touches only the extents the clone actually wrote, so a container restart scales with what that container changed rather than with the image it started from. Returns `freed_extents` / `restored_extents` / `shared_extents`. The volume keeps its id and attachment record. (#4)
- **feat:** `VolumeManager::reset_volume` and `snapshot::reset_to_source`; new references are taken before old ones are released, so an interruption leaks a reference rather than freeing live data. `GlobalExtentMap::inc_extent_ref` bumps the share count of a single extent — the GEM refcount is what makes a write copy instead of landing in place, so re-sharing must bump both sides or the next container write would scribble onto the golden image. (#4)
- **fix:** reset is refused with 409 while the volume is attached (contents cannot change under a live host) and for a volume that was not created from a source. `VolumeRec` now records `source_local`.

## [v6.4.0] — 2026-08-08

### 2026-08-08 (later)
- **feat:** NVMe-oF hot-add — a host connects once and later attaches cost an async event plus a rescan, with no Connect and no new TCP session per container. `add_namespace_dynamic`/`remove_namespace` raise a namespace-change event; the admin queue gets its own loop that selects over the socket and that event stream, holding the host's Asynchronous Event Requests and completing one the moment a namespace changes. Adds the Changed Namespace List log page (LID 0x04, cleared on read, with the `0xFFFFFFFF` rescan-everything sentinel when a connection falls behind) and advertises OAES bit 8, without which a host never arms for the event. (#4, #26)
- **BREAKING:** `AttachInfo::NvmeTcp` previously advertised a per-volume NQN (`nqn.2026-01.io.stormblock:<volume_id>`) that the target rejected at Connect — it only ever answered to its configured subsystem NQN, so the nvme-tcp attach path could not have worked as shipped. It now returns the real subsystem NQN plus a per-volume `nsid`. A per-volume NQN would also force a Connect per container, which hot-add exists to avoid. Consumers must read `nsid` to pick the right namespace. (#26)
- **feat:** `/v1` attach hot-adds the volume as a namespace and reports its NSID; detach withdraws it once no node holds the volume. Attach is idempotent — a replay reuses the namespace instead of leaking one. (#4)
- **fix:** `/v1` `delete_volume` tore down the ublk export but never released the NVMe namespace, so deleting a COW image left a namespace pointing at freed slots — hit constantly by the delete-and-reclone container restart cycle. Released before the backing volume goes away.
- **perf:** COW clone and delete batch their refcount persistence. Each `inc_ref`/`dec_ref` was a read-modify-write of a whole sector, so clone and delete cost two round trips per extent and scaled with image size; delete also rewrote the header once per freed slot. Slot entries are 64 bytes against 512/4096-byte sectors, so entries are now grouped by sector, a fully-covered sector skips its read, and the header is written once. Measured: 256 slots go from 256 writes + 256 reads to 4 writes and 0 reads. Matters most for VM images. (#4)

## [v6.3.0] — 2026-08-08

### 2026-08-08
- **fix:** iSCSI UNMAP/discard never reclaimed thin allocation, so usage only ever grew (#25). Two causes: the target never advertised thin provisioning — VPD page 0xB2 (Logical Block Provisioning) was absent and unlisted in the supported-pages page, so Linux left `discard_max_bytes` at 0 and issued no UNMAP at all — and even a well-behaved UNMAP would have failed, because only `WRITE_10`/`WRITE_16` collected a data-out payload, leaving `handle_unmap` with an empty parameter list. VPD 0xB2 now reports LBPU/LBPWS/LBPWS10/LBPRZ and thin provisioning type; READ CAPACITY(16) gains LBPRZ; data-out collection is driven by `is_data_out_command()` covering UNMAP, WRITE SAME(10/16) and MAINTENANCE OUT.
- **feat:** WRITE SAME(10/16) — with the UNMAP bit and an all-zero pattern it deallocates, otherwise it writes the pattern in bounded chunks (#25)
- **feat:** `BlockDevice::discard_granularity()` (default: block size; thin volumes report their slot size) drives the optimal unmap granularity and UGAVALID alignment in the Block Limits VPD page, so initiators align discards to something that actually frees space (#25)
- **feat:** `/metrics` samples slab capacity/allocated/free per slab and in total at scrape time, making thin growth and reclaim observable (#25)
- **feat:** `LunBacking::Volume { volume_id }` — thin/COW volumes export directly as iSCSI LUNs, resolved through the same handle `attach` uses (#22)
- **feat:** the LUN table is persisted to `<data_dir>/luns.json` (temp file + rename) and re-opened at startup, so API-created exports survive a restart; an unresolvable backing is skipped rather than fatal (#22)
- **feat:** `POST /api/v1/exports` now wires the export into the running target and returns `active` with the assigned `lun_id` (iSCSI) or `nsid` (NVMe-oF) instead of parking in `pending_restart`; DELETE tears it down on the target (#24, #26)
- **feat:** NVMe-oF namespaces can be added and removed at runtime (`add_namespace_dynamic`/`remove_namespace`/`list_namespaces`), replacing an `Arc<HashMap>` that panicked via `Arc::get_mut` once the target was shared (#26, #24)
- **feat:** `management.advertised_addr` config (also `$STORMBLOCK_ADVERTISED_ADDR`) — `/v1` attach info and the NVMe-oF discovery log page report a routable address instead of falling back to `127.0.0.1` (#26)
- **perf:** the iSCSI I/O path no longer allocates a `Vec` of every LUN ID per SCSI command — only REPORT LUNS gathers the list — and `AppState::lun_entries` becomes a `HashMap` keyed by LUN ID, so lookups are O(1) at thousands of LUNs (#24)
- **fix:** REPORT LUNS follows SPC-4 — LUN LIST LENGTH reports the full list size even when truncated to the allocation length, SELECT REPORT 0x00/0x02 return the list and 0x01 returns empty, reserved values and an under-sized allocation length are an illegal request; LUN encoding is peripheral below 256 and flat-space above, verified to 2000 LUNs (#24)
- **fix:** MAINTENANCE OUT (SET TARGET PORT GROUPS) is no longer refused on a readonly LUN — readonly rejection now uses `modifies_media()` rather than the data-out predicate (#25)
- **fix:** `DELETE /api/v1/luns/{id}` now succeeds for a LUN wired in from config at startup
- **feat:** `POST /api/v1/luns` may omit `lun_id` to be assigned the next free number; LUN numbers are handed out in one place shared with the export path, so the two cannot collide (#24)

### 2026-08-07
- **fix:** iSCSI target sequence numbers were at the wrong BHS offsets in every target→initiator PDU (StatSN written to the ExpCmdSN slot, ExpCmdSN to the DataSN slot) and login responses carried no StatSN/ExpCmdSN/MaxCmdSN at all — a spec-compliant initiator (RouterOS) saw `MaxCmdSN=0`, a closed command window, and could never issue a SCSI command, reconnecting every ~10s (#23). Login responses now carry correct sequence numbers seeded from the request CmdSN (immediate-aware), and the full-feature connection continues those counters.
- **fix:** iSCSI login operational stage (CSG=1) now captures `InitiatorName`/`TargetName`/`SessionType` — initiators that skip the security stage (RouterOS) no longer log in with an empty initiator name; CHAP-required targets reject the security-stage bypass (#23)
- **feat:** iSCSI full-feature-phase Text Request handling (`SendTargets` discovery reply with TargetName + TargetAddress) (#23)
- **feat:** per-PDU debug tracing in the iSCSI full-feature phase (opcode, ITT, CmdSN, CDB opcode, LUN, response status) — enable with `RUST_LOG=stormblock=debug`; unknown LUN and unsupported opcodes now log at warn (#23)
- **fix:** REPORT LUNS encoded LUN numbers into the wrong byte (`[lun, 0]` instead of peripheral `[0, lun]`), so reported non-zero LUNs could never be addressed back; LUN field decoding now masks the SAM-5 address-method bits (#23)
- **fix:** aarch64 build broken by hardcoded `*mut i8` cast in `gethostname` calls (`src/stormfs.rs`, `src/cluster/mod.rs`) — `c_char` is unsigned on aarch64/arm; now casts to `*mut libc::c_char` for portability (#21)

### 2026-07-19
- **feat:** ublk transport for the CSI `/v1` attach path — when `[management] ublk_transport = true` and a volume is attached on the node that holds its master, the engine exports the backing device as a local `/dev/ublkbN` and returns `AttachInfo::Ublk { device_hint }` instead of NVMe-oF/TCP coordinates, giving the CSI node a local device with no network round trip. Falls back transparently to nvme-tcp when ublk is unavailable (non-Linux, `ublk_drv` not loaded) — probed once at startup. Exports are torn down on detach/delete; the CSI node never disconnects a ublk device itself. Closes the ublk half of the attach contract (the `Ublk` variant existed in the wire type but was never produced). New `src/mgmt/ublk_export.rs`; policy `should_offer_ublk` and export bookkeeping are unit-tested off-Linux, the kernel export path is verified on dev.g8.lo.

### 2026-04-03
- **feat:** Dynamic iSCSI LUN management REST API (`POST/GET/DELETE /api/v1/luns`)
- **feat:** Readonly LUN support — `readonly` flag on LUN creation prevents SCSI writes (WRITE PROTECTED sense)
- **feat:** iSCSI target starts unconditionally — LUNs can be added at runtime via REST API, even with no initial device
- **feat:** `[[luns]]` TOML config section for declarative LUN provisioning at startup
- **feat:** `REPORT_LUNS` SCSI command now reports actual active LUN IDs (was hardcoded to LUN 0)
- **refactor:** iSCSI LUN map changed from `Arc<HashMap>` to `Arc<RwLock<HashMap>>` for runtime mutability
- **refactor:** `IscsiTarget` stored in `AppState` for REST API access
- **refactor:** `open_one_drive()` made public for runtime device opening

### 2026-03-26
- **feat:** `--ublk` flag on `boot-iscsi` CLI — exports each partition as `/dev/ublkbN` via UblkServer (Linux 6.0+)
- **feat:** `scripts/build-stormblock-initramfs.sh` — LinuxBoot initramfs builder (busybox + stormblock + ublk_drv + /init script)
- **feat:** `install-fedora-iscsi.sh` — 8-phase mkube CI job: provision iSCSI disk, format filesystems, install Fedora via dnf --installroot, configure for LinuxBoot-style boot
- **feat:** `systemd/stormblock-ublk.service` — safety net for post-switch_root (restarts stormblock if initramfs process dies)
- **fix:** ublk `UblkCtrlCmd` struct layout — match kernel UAPI `ublksrv_ctrl_cmd` exactly (32 bytes, len@6, addr@8)
- **fix:** ublk ioctl-encoded command numbers (`UBLK_U_CMD_*`) — required by kernel 6.1+ (was sending raw 0x04, now 0xC0207504)
- **fix:** ublk `queue_id` must be `-1` (0xFFFF) for ADD_DEV — kernel validates this field
- **fix:** ublk mknod fallback for `/dev/ublkcN` in containers (read major:minor from sysfs)
- **fix:** ublk submit FETCH_REQ before START_DEV — kernel requires all queues registered first (use Barrier for sync)
- **fix:** ublk START_DEV requires PID in `data[0]` — kernel validates `ublksrv_pid > 0`
- **fix:** ublk mknod `/dev/ublkbN` block devices in containers (sysfs major:minor fallback)
- **fix:** ublk orphan cleanup — STOP+DEL stale devices before ADD_DEV, request specific dev_id
- **fix:** ublk WRITE_ZEROES (op 5) handler — treat as discard for thin volumes
- **fix:** `install-fedora-iscsi.sh` — use `dnf5 group install` syntax (rawhide has dnf5, not dnf4)
- **fix:** `install-fedora-iscsi.sh` — `--use-host-config` for installroot repo access, explicit package list (no Minimal Install group), `tsflags=noscripts` for container scriptlet failures, vmlinuz copy from lib/modules, busybox install for initramfs build
- **chore:** Full 8-phase Fedora iSCSI install verified: 163 packages installed, vmlinuz (18M) + initramfs (4.4M) staged, all verification checks passed

### 2026-03-25
- **feat:** `IscsiDevice` — production iSCSI initiator implementing `BlockDevice` trait (login, READ/WRITE(10), READ CAPACITY, UNMAP, NOP-Out keepalive)
- **feat:** `DriveType::Iscsi` variant for iSCSI-backed block devices
- **feat:** `boot_iscsi` module — iSCSI boot disk orchestrator with multi-volume partitioned layout
- **feat:** `BootDiskLayout::parse()` — layout string parsing (e.g., `esp:256M,boot:512M,root:6G,swap:1G,home:rest`)
- **feat:** `IscsiBootManager::provision()` — connect to iSCSI, format slab, create ThinVolumes per partition
- **feat:** CLI `boot-iscsi` subcommand — provision partitioned boot disk on remote iSCSI target
- **feat:** CLI `migrate-boot` subcommand — migrate boot volumes from iSCSI slab to local disk via placement engine
- **test:** 11 boot-from-iSCSI integration tests (layout parsing, provisioning, slab migration)
- **test:** 10 iSCSI hardware integration tests (`tests/iscsi_blockdev.rs`) — IscsiDevice connect/read/write/flush, slab format+allocate+reopen, ThinVolume I/O, multi-volume isolation, live migration between disks
- **chore:** `boot-iscsi-test.sh` — CI script for mkube job runner (7 phases: build, test, IscsiDevice, slab+volume, migration, CLI, clippy)
- **fix:** iSCSI NOP-In handling — distinguish solicited (flush response) vs unsolicited (target ping) per RFC 7143
- **fix:** Slab slot table alignment for 512-byte sector devices — read-modify-write for sub-block slot entries
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
- **fix:** iSCSI initiator — track `ExpStatSN` from response PDUs (RFC 7143 §11.6.1); stale ExpStatSN caused target CmdSN window stall after ~128 commands, hanging large migrations
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
