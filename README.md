# StormBlock

**Pure Rust Enterprise Block Storage Engine**

StormBlock turns raw physical drives — NVMe SSDs, SAS SSDs, SAS HDDs — into network-accessible logical volumes over NVMe-oF/TCP and iSCSI. It is the block-layer foundation of the Storm ecosystem.

> **Build on `root@dev.g8.lo`, never on a Mac.** The workstation is macOS and
> the target is Linux: `libc`, `io_uring`, `ublk`, `/dev/kmsg` and the whole
> storage path are behind `cfg(target_os = "linux")`, so a macOS build skips
> exactly the code most likely to be wrong. `cargo test` runs 258 tests there
> and 303 on dev. Commit, push, pull on dev, build there.

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

**A template is one volume, and deleting it takes that volume with it.** The
scratch volume a template is formatted on is dropped the moment it is sealed —
the sealed snapshot holds its own refcounted extents and does not depend on the
volume it was taken from — and `DELETE /api/v1/fstemplates/{id}` purges what is
left unless asked not to (`?purge=false`). A create that fails anywhere, at
format, at seed or at seal, leaves nothing behind, so retrying a name does not
cost two more volumes each time. For a node that already accumulated debris,
`GET /api/v1/fstemplates/orphans` lists volumes named like a template's that no
template claims **and nothing on this node is serving**, and `DELETE` on the
same path reclaims them; clones are named by their consumer, so they are never
in that set.

Formats do not queue. No lock is held across a format, a check or a stamp, and
the formatter takes `&self` so one format fans out across block groups.
Measured on a Fedora 6.17.1 host: one 256 MiB template formats and seals in
**50 ms**, four concurrently in **79 ms** total; a clone costs 54–86 ms
including its verification fsck.

Verified against real consumers rather than only against itself: clones
exported over iSCSI and attached with open-iscsi are read by `blkid`, pass
`e2fsck -fn`, mount read-write four at a time, take writes, unmount and check
clean again (`ci-fstemplate-verify.sh`); and a clone attached to RouterOS over
NVMe-TCP takes writes, with the disk table's free-block and free-inode counts
moving to match.

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

### Multiple connections per iSCSI session (MC/S)

An iSCSI session can carry several TCP connections, so an iSCSI-only consumer
is not limited to one stream. The target offers up to `max_connections`
(default 4) and negotiation takes the **lower** of that and what the initiator
asks for — an initiator that wants one connection still gets one, so raising
the cap cannot change what an existing consumer sees.

```toml
[iscsi]
max_connections = 4
```

A login carrying a non-zero TSIH adds a connection to that session rather than
starting a new one, and the ISID has to match as well: the two identify the
session together, and a TSIH on its own is guessable. One connection closing
now removes **that connection**, not the session — the session ends when its
last connection does.

The part that had to be right first: **CmdSN belongs to the session, StatSN to
the connection** (RFC 7143 §4.2.2.1). One shared command window is handed to
every connection that joins, so an initiator with two paths is told one
consistent thing about its own flow control. Tracking it per connection — which
is what single-connection code could get away with — would have each connection
advertising a different window for the same session.

`GET /api/v1/sessions` reports the per-session connection count. NVMe-oF reaches
parallelism through queue count instead, so this matters for consumers that are
iSCSI-only.

### Moving a volume — re-home, or shrink

Growing is a resize (above). Shrinking is not, and cannot be: the extents past
the new end are freed immediately and **xfs cannot shrink into that**, so
`resize` refuses it. The only safe form of "make this smaller" is to build a
new, smaller filesystem and copy the contents across — which is what a move is,
and why the copy is at the filesystem level rather than the block level. A
block-level clone would faithfully reproduce the size being escaped.

```bash
# Copy and verify. Nothing is destroyed — all three volumes exist afterwards.
curl -X POST http://node:9090/api/v1/moves \
  -H 'Content-Type: application/json' \
  -d '{"volume_id":"<uuid>","target_name":"var-small","target_size":"24G"}'

# → {"move":{"state":"ready_to_commit","verified":true,
#             "target_volume_id":"…","rollback_snapshot_id":"…"}}

# Repoint whatever used the source at the target, then:
curl -X POST http://node:9090/api/v1/moves/<move-id>/commit   # source goes
curl -X POST http://node:9090/api/v1/moves/<move-id>/abort    # target goes instead
```

**Two calls, because the ones people skip when they script this are the ones
that matter.** The first snapshots the source (copy-on-write, so it costs
metadata and doubles as the way back), creates the target, formats it to match,
streams the contents across and *fsck's the result* — and stops. The source is
untouched. Only `commit` deletes it, and only after the caller has moved its
consumer over, which is the one thing the engine cannot know.

The copy is streamed straight from one filesystem into the other with no scratch
file and no whole-archive buffer, so a 64 GiB volume holding 2 GiB moves 2 GiB.
It goes through tar rather than a hand-rolled tree walk, which is what preserves
modes, ownership, timestamps, symlinks, hard links, device nodes and extended
attributes — SELinux labels among them, without which a rootfs stops booting.
Both ends count what crossed independently and a mismatch in any category fails
the move.

A move is **offline by contract**: an exported or attached volume is refused,
because anything written during the copy would not be in the target. It is
*restartable* rather than resumable — an interrupted move is discarded and
re-run, which costs time and never data.

### Growing the pool on disk pressure

Thin volumes overcommit, so a node can run out of **physical** space while every
volume still reports free virtual space — invisible until writes start failing,
and confusing when they do. The pool watches its own utilisation and adds a slab
when it crosses a high-water mark:

```toml
[pressure]
enabled = true
high_water_pct = 80          # add capacity at or above this
check_interval_secs = 60
min_slab_bytes = 1073741824  # smallest slab worth adding
max_slabs = 64               # backstop against a bad source list

# Where capacity may come from, claimed in order. Nothing is ever discovered.
[[pressure.sources]]
kind = "directory"           # creates a new backing file — never overwrites
path = "/var/lib/stormblock/grow"
slab_bytes = 8589934592

[[pressure.sources]]
kind = "device"              # claimed whole; adopted if it already holds a slab
path = "/dev/sdb2"
```

**Grow on pressure, never preallocate** — preallocating to the virtual size
gives back everything thin provisioning saved. The pool grows one slab at a
time, when it is actually needed. A slab is added rather than enlarged because a
slab's data region starts past a slot table sized at format time; growing one in
place would move every byte of data.

Sources are configured and never discovered: formatting the wrong device is
unrecoverable, and "it had no filesystem on it" is not consent. A `directory`
source only ever creates new files, which is also how to grow into the unused
tail of the node's own disk — mount the spare space and point it there. A
`device` source that already carries a readable slab is **adopted with its
data**, not reformatted, so a source claimed before a reboot comes back intact.

```bash
# How full is this node, and what is the watcher doing about it?
curl -s http://node:9090/api/v1/slabs/pool
```

Pressure with every source claimed is logged at error and reported as
`sources_exhausted` — the pool is under pressure and the engine is out of ways
to answer it, which is not a state to discover late. `stormblock_pool_used_pct`,
`stormblock_pool_free_bytes` and `stormblock_pool_slabs_added_total` carry the
same story to Prometheus.

### Growing a volume online

A volume grows in place, and the block device grows with it. `POST
/v1/volumes/{id}/expand` moves the volume's virtual size and then tells the
kernel, via `UBLK_U_CMD_UPDATE_SIZE`, so `/dev/ublkbN` reports the new capacity
and `xfs_growfs` has somewhere to grow into. **No quiesce**: resizing has no
consistency point to capture, and stalling a live `/var` to make it bigger
turns a day-2 operation into an outage. The capability is negotiated at device
creation and degrades to "the volume grew, the device did not" — said loudly —
on a kernel older than 6.12.

Shrinking is not the same operation and is not offered as one. A smaller size
comes back `409`: the extents past the new end would be freed immediately, and
xfs cannot shrink at all, so a shrink of a mounted volume destroys live data
with nothing to undo it. `VolumeManager::shrink_volume` exists for a caller
that means it. Moving a volume onto a smaller one, with its data, is a copy —
a different operation.

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
