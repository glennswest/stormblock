# Metadata format v2: the one format change (#158, with #157 and #156)

**Status (2026-10-05):** stages A and B built; C, D, E to come. Built in
stages behind a format gate (`[metadata] format = 2`,
`$STORMBLOCK_METADATA_FORMAT=2`): the engine writes v1 until v2 is complete, so
nothing half-done ships, and the format changes **once**.

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

## As built (stages A and B)

- **Slab header v2** (`drive/slab.rs`): version 2 at byte 8 (an engine before
  #158 refuses the slab, rather than reading the v2 region as an empty v1 one
  and writing over it); `table_capacity` u64 at 128..136; the checksum at
  124..128 covers 0..124 and 128..256. v1 is unchanged and refuses a table
  past 4 Gi slots.
- **The store** (`volume/metav2.rs`), over a region of `size` bytes: two 4 KiB
  superblocks, a log of `size/8` (16 pages to 64 MiB), the rest pages.
  - Superblock: magic `SMV2SUPR`, generation, root page, log start and its
    first sequence, the layout, a random nonce, CRC.
  - Pages: leaves `(key, value)`; internal nodes of up to 124 children, each
    with the lowest key it may hold, and a level (1: children are leaves). Keys
    are 25 bytes, `volume (16) | kind (1) | index (8, big-endian)`; kinds are
    0 the document (extent size, arrays), 1 a volume header in chunks of at
    most 1536 bytes (the last shorter, so a reader knows where it ends), 2 an
    extent, 3 a parity group. A header is the volume's record without extents
    or parity, prefixed with its record version (9).
  - Log record: magic, the store's nonce, sequence, length, kind (ops or
    wrap), CRC, then the ops (`Put`, `Del`, `DropVolume`), padded to pages. A
    record that does not fit before the end of the ring is preceded by a wrap
    record. Replay stops at the first record with the wrong nonce or
    sequence, or a CRC that fails.
  - Checkpoint: when the log passes half, or 256 Ki entries wait. Pages are
    allocated from a bitmap built at open by walking the internal pages (the
    leaves are named by their parents); pages a checkpoint stops naming are
    freed only after its superblock is flushed.
- **Persist** (`volume/persist_v2.rs`): the GEM records what changed
  (`gem::Changes`: extents, parity groups, whole volumes) while a v2 store
  takes them. Each sink (a v2 metadata slab, `metadata.v2` in the data
  directory) is handed: the document entry if it changed, the headers whose
  bytes changed, the changed extents and stripes, whole volumes for those
  new to it or changed as a whole (a clone's source: every share count moved),
  `DropVolume` for those that left it. Which volumes a sink carries is
  `per_slab_metadata`'s rule, with each volume's slabs kept between persists.
  The first persist after a start, after a failed write, or after the GEM was
  not recording writes the sink whole (one checkpoint).
- **Order.** Batches are applied in the order their records were taken (a
  ticket per persist), never skipped for a newer one; a batch taken before a
  failure is dropped (the failure makes the next one whole), and a persist
  that stops before writing marks its sinks the same way.
- **Read.** `metav2::read_slab` reads either format; every reader of a slab's
  record goes through it. `Slab::read_metadata` on a v2 slab is an error,
  never "nothing here".

Measured (`examples/persist_cost`, dev, release, 4 KiB slots, 100 000
extents in one volume): one extent changed costs **4 096 bytes and 2.7 ms**
per persist in v2, against **2 738 435 bytes and 9.4 ms** in v1. The first
persist (the whole store) is 83 ms in v2 against 16 ms in v1. The full test
suite passes with the gate on (every slab and the data directory in v2) as
well as off; the power-cut test runs its 300 cuts in both formats.

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
