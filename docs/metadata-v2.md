# Metadata format v2: the one format change (#158, with #157 and #156)

**Status:** design and work plan (2026-10-05). Built in stages behind a format
gate: the engine keeps writing v1 until v2 is complete, so nothing half-done
ships, and the format changes **once**.

Owner decisions:
- #158 (2026-10-01): one format change, bundling the paged extent index with
  u64 slot and extent indexes, a format version and in-place migration.
- #157 (2026-10-05, **B**): the change log ships inside that change, not in a
  region format of its own.
- #156 (2026-10-01, and on 2026-10-05 "both", with 8 MiB rather than 64 MiB):
  extent size is per pool, fixed at creation: 1 MiB hot, 8 MiB bulk.

Measured starting point, from #155 and #208, at 1 MiB slots:
- a free slot: 0.6 B;
- an extent: 25 B resident;
- the free map: 128 MiB per PiB;
- a persist rewrites every volume's whole record into each metadata slab's
  region, two alternating copies.

## What changes on disk

| | v1 (today) | v2 |
|---|---|---|
| slab header | `STRMSLAB` v1; `table_capacity` u32 at 120..124 | v2: `table_capacity` u64; the slot index is u64 everywhere |
| slot table entry | 64 B: state, volume, vext u64, refs u32, generation u64 | unchanged (the index is the entry's position, not a field) |
| metadata region | two alternating full copies of the volumes document (`STRMVMET` v1) | a superblock pair, a page area holding a copy-on-write B-tree, and a change log |
| volumes document | bincode V9: one `extent_size`; `slot_idx` u32 | B-tree records: a volume header (with its own `extent_size`, #156); extents keyed (volume, vext) with u64 slots |
| data directory copy | `volumes.dat`, the whole document | the same paged structure in a file (`metadata.v2`) |

### The metadata region, v2

The metadata region is divided into three parts:

- **Superblocks A and B**, 4 KiB each, written alternately. Each carries:
  - the generation;
  - the root page of the volume tree;
  - the page allocator's root;
  - where the log starts and ends;
  - the checkpoint generation;
  - a CRC.

  The newer one that checks is the record.
- **The page area**, of 4 KiB pages. Pages are copy-on-write: a checkpoint
  writes new pages and then a superblock that names them, so a cut leaves the
  previous checkpoint whole. It holds:
  - one B-tree keyed `(volume id, kind, index)`, where kind is: volume
    header, data extent, or parity group;
  - the freed-page list.
- **The log**, a ring of records, each with its own length, generation and
  CRC. A record says one of:
  - upsert or remove a volume header;
  - upsert or remove an extent;
  - upsert or remove a parity group.

  A persist appends the records for what changed since the last persist and
  flushes once: O(changes), the same durability point as today's one write
  (#171's ordering holds; a persist still follows the slab syncs).

**Checkpoint (background).** When the log passes a threshold, the changes are
applied into the tree as new pages, and the device is flushed. Then the next
superblock names the new root and moves the log's start past what was
applied. Then the replaced pages are freed.

**Recovery.** Take the newer valid superblock, open its tree, and replay the
log records whose generation is newer than its checkpoint, stopping at the
first record that fails its CRC (a torn tail).

**The versions-before-map order** of `volume/versioned.rs` (#50) holds: the
versions file is written before the log append that names the new map.

### u64 slot and extent indexes

`Leg.slot_idx`, `ExtentLocation.slot_idx`, slab, slot table and free-map
indexes become u64 in memory. That is stage A, with no format change: v1
writes them as u32 and refuses a slab past 4 Gi slots. v2 stores u64.

### Paged in memory

The extent map in memory becomes a cache of the on-disk tree:

- a volume's map is loaded when the volume is opened or attached;
- an idle, clean map is evicted under memory pressure (the node's
  `[metadata] cache_mb`);
- what is resident is the working set: the attached volumes and the ones
  being cloned, moved or rebuilt.

A golden that nothing is cloning costs nothing resident. The slot side is
already bounded since #155: a page cache plus the free map.

### Extent size per pool (#156)

- The volume header records its own `extent_size`.
- A pool is role × tier × extent size, and a volume is placed in one pool.
- `VolumeManager` stops having one slot size, and a slab of any size is
  adopted.
- Bulk (8 MiB) volumes: 64 GiB and over, or a StorageClass `extentSize`.
- The install lays a 1 MiB data slab and an 8 MiB bulk slab, the bulk one
  taking the rest of the drive.

### Migration and rollback

- An engine with v2 reads **both** formats. A slab is v1 or v2 as a whole.
- New slabs are v2 once the gate is on.
- An existing v1 slab is migrated in place, explicitly, slab by slab:
  `stormblock slab upgrade <slab>`, or `POST /api/v1/slabs/{id}/upgrade`.
  The engine writes the v2 tree from its v1 records, flushes, and then
  rewrites the header to v2 as the last step.
- A cut before the header write leaves a v1 slab. A slab not yet migrated
  stays readable by an older engine, which is the rollback.
- A stormcos install lays fresh v2 slabs (install = wipe, #261), so nodes come
  to v2 by installing. The forge and long-lived nodes migrate explicitly.

## Stages

Each stage is tested on dev and leaves the engine shippable.

| stage | what | format written |
|---|---|---|
| A | u64 slot/extent indexes in memory; v1 encodes u32 and refuses a slab beyond 4 Gi slots | v1 |
| B | v2 reader and writer: slab header v2, the metadata region (superblocks, COW B-tree, log, checkpoint, recovery), `metadata.v2` in the data directory; persist appends changes (#157); under the gate (`[metadata] format = 2`, tests) | v1 by default |
| C | the extent map in memory as a cache of the tree: load on open/attach, evict idle clean maps under `cache_mb` | v1 by default |
| D | extent size per volume and per pool (#156); the install's bulk slab | v1 by default |
| E | migration (`slab upgrade`), the gate's default flipped to v2 for new slabs, scale runs on emulated 1 PiB drives (#208), docs | **v2** |

Not here: free-extent trees per allocation group. The free map is 16 MiB per
PiB at 8 MiB extents and 128 MiB at 1 MiB, which is not the limit yet.
