# Metadata format v2: the one format change (#158, with #157 and #156)

**Status (2026-10-06):** built, stages A–E, and run at scale on emulated
1 PiB and 256 TiB drives (below). Format 2 is the default for new
slabs and data directories; `[metadata] format = 1` (or
`$STORMBLOCK_METADATA_FORMAT=1`) keeps format 1 and migrates nothing. Built in
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

As decided on #158 (the master's recommendation, accepted: "old slabs migrate
on first open"), and as built:

- An engine with v2 reads **both** formats. A slab is v1 or v2 as a whole.
- New slabs are v2 once the gate's default is 2 (stage E).
- A serving engine migrates each v1 **metadata** slab in place at its first
  persist once 2 is the default (`VolumeManager::upgrade_slabs`); by hand,
  `stormblock slab upgrade <slab>` or `POST /api/v1/slabs/{id}/upgrade`
  (destructive, #274). Inspection commands (`slab volumes`, `slab holds`,
  `image inspect`) never migrate.
- The order makes a cut safe (`Slab::upgrade_to_v2`):
  1. the slab's record is written into **both** v1 copies, so the copy in the
     region's second half is current;
  2. the v2 store is written into the region's **first half only**, flushed;
  3. the slab header, version 2, last, flushed.

  A cut before 3 leaves a v1 slab whose second copy is its record; after it, a
  v2 slab. A region too small to hold the store in half of it stays v1 (said
  in the log) and is written as v1.
- A slab with no metadata region keeps a v1 header: it holds no record, and
  v1 reads it fine below 4 Gi slots.
- Rollback: an engine before #158 refuses a v2 slab. A stormcos install lays
  fresh slabs (install = wipe, #261), so nodes come to v2 by installing; a
  long-lived node or the forge migrates in place and cannot go back to an
  older engine without a reinstall (forge: a VM snapshot, as for v20).

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
    or parity, with the volume's extent size (#156), prefixed with the header
    version (10).
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

## As built (stage C)

- **A map is resident or cold** (`gem.rs`). A cold map keeps a summary
  (extent count, exclusive/shared counts, the slabs it is on) and nothing
  else. Every accessor that would read or change a cold map panics, and so
  does every walk of all maps while one is cold: a missing map would read as
  "no extents", which serves zeros, allocates over the volume's data and lets
  GC free its slots. Loud is the safe failure.
- **Loading** (`gem::ensure_resident`, `persist_v2::StorePager`): from a
  store that holds the volume; every entry point of a volume's handle (I/O,
  resize, health, resync, relocate) loads first, as does a delete or clone
  (`prefetch_volume`). A listing reads the summary and loads nothing.
- **Eviction** (`VolumeManager::evict_idle`, `[metadata] cache_mb`, a task
  every 30 s under the manager's lock): least recently used first, only a map
  that no one outside the manager holds a handle to (nothing attached,
  served, mid-I/O), that nothing holds, with no change since the last persist,
  when every metadata store is v2, every record taken has been written and
  no store is due a whole write. Checked under the GEM's write lock, which a
  persist needs to take its records. A share count a clone gives back to a
  cold golden is not applied to it (too high costs a copy, never data).
- **Walks**: GC reads cold maps from their stores one at a time into a live
  set of one bit a slot per slab, and collects nothing if any cold map could
  not be read; it never puts them back in memory. A flow-over, a drain, a
  resync and the install's data seed pin every map in memory for their run
  (`gem::pin_resident`): nothing is evicted while a pin is held. A whole
  write of a store, a v1 record, or the last v2 store going away load every
  map first.
- **Checked** by running the whole suite with every slab in v2 and
  `$STORMBLOCK_METADATA_CACHE_MB=0` (every eligible map evicted after every
  persist): 920/921 on dev, the one being the qcow2 import's 5 s deadline on
  a loaded box (#173's class; it passes alone and in its module).
- Measured (`examples/map_cache`, 64 goldens × 2 000 extents laid in order):
  a map costs 12.4 B an extent resident, all of it freed when it goes cold;
  a 2 000-extent map loads in 0.4 ms.

## As built (stage D, #156)

- **A volume's extent size** is fixed at creation and kept with it:
  `ThinVolume`'s slot size, `VolumeRecord.extent_size` (never written by
  bincode, so v1 records are unchanged), the v2 header (`Header.extent_size`).
  A v1 record has one size a document: it decodes as every volume at the
  document's size, and refuses to encode a volume of another size.
- **Pools are role × tier × size.** Every picker filters by slot size
  (`SlabRegistry::size_ok`, `best_slab_for_tier_in_role(…, size)`,
  `distinct_domains_with_space_in_role(…, size)`), and so do placement's own
  (drain, rebalance, evacuation, flow-over destinations) and StormFS chunks.
  A move into a slab of another slot size is refused.
- **Creation** (`VolumeManager::choose_extent_size`): what was asked
  (`CreateOptions::extent_size`, `POST /api/v1/volumes {extent_size}`,
  `/v1 {extent_size_bytes}` = a StorageClass `extentSize`, stormblock-csi#37);
  else 8 MiB for a volume of 64 GiB or more where the role has an 8 MiB pool;
  else the node's default; else the smallest size the role has. A pinned
  volume takes its slab's. A clone takes its source's; a composition its
  members' (one size, or refused). Sizes other than the default only where
  every metadata store is v2 (or 2 is the default).
- **Adoption and restore.** A slab of another slot size is another pool where
  the records can say each volume's size, and refused (as before) where they
  cannot. A restored volume's size must equal the slot size of every slab it
  has a leg on, or it is refused: the incident that refusal came from (1 MiB
  extents addressed in 4 MiB slots) is checked per volume now.
- **The install** (`lay_node_slabs`, `LocalLayout::bulk`, on with format 2):
  a data half of 256 GiB or more is a 1 MiB data slab (a quarter of it, at
  least 64 GiB) and an 8 MiB bulk slab (`stormblock-bulk`, typed as a data
  slab, last, the one that grows); smaller, one data slab. A reinstall of the
  system half keeps both; discovery finds the bulk slab as it finds any slab.
  The quarter is a default, not a decision recorded on #156.

## As built (stage E)

- **The default is format 2** (44fc8e3): new slabs and data directories are
  v2; `[metadata] format = 1` keeps v1. Migration is as described under
  "Migration and rollback".
- **Run at scale** on dev (2026-10-06), on emulated drives (#208), 1 MiB
  extents, metadata v2 on every slab
  (`integration_metadata_v2::a_v2_node_on_petabyte_drives_keeps_its_volumes_across_restarts`,
  ignored for its minutes; `--run-ignored only -E 'test(petabyte_drives)'`):
  - one 1 PiB drive and two of 256 TiB, one of them laid as v1 by an earlier
    engine and migrated at the first persist;
  - an 8 PiB thin volume with extents at 0, 1 PiB, past 2^32 and at its last
    extent; a mirror across two drives; a hundred goldens, a clone and its
    copy-on-write;
  - maps evicted and loaded back from the store; two restarts from the disks
    alone (a fresh manager, `Slab::open` + `restore`), every byte checked, a
    new volume each round.

  | | |
  |---|---|
  | format, 1 PiB v2 slab (1.07 Gi slots, 24 GiB region) | 1.3 s |
  | format, 256 TiB slab (v1 or v2) | 0.3 s |
  | restart: open the three slabs and restore | 39 s |

  The restart is the slot tables (64 B a slot, 1.6 Gi slots read in one pass
  each, one slab after another); the store adds nothing measurable. That is
  #307's subject (160 × 256 TiB take 22 min), not the metadata format.
- **Slot indexes past 2^32**:
  `drive::slab::tests::a_v2_slab_past_four_gi_slots_addresses_every_slot`
  (ignored): a v2 slab of 4.49 Gi 4 KiB slots (17 TiB) formats in 5.4 s, a slot
  at index 2^32 + 6 is written, and after a reopen (58 s) its entry, owner and
  data are read back.

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
