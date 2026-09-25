# Allocation metadata at 40 PB a node

**Status:** design (#145, 2026-09-25), with measurements and the first fix.
Scale target (the owner): 160 × 256 TB drives per 4U node (Supermicro
ASG-4116S-NU160R class), ~41 PB a node, ~400 PB a rack, and 1 PB drives
coming (stormcos#93). Companion: [multi-drive.md](multi-drive.md).

## 1. What it costs today, measured

`examples/metadata_footprint.rs` formats a slab, allocates every slot, fills
the global extent map (GEM) with one extent per slot, and reads resident memory
after each step. On dev, with 1 M and 4 M slots, the two runs agree:

| what is resident | per slot / extent | why |
|---|---|---|
| a slab, **every slot free** | **~41 B** a slot | `Slab.slots: Vec<Slot>`, 40 B for every slot whether used or not, and the free bitmap |
| the slot allocated, slab side | **~70 B** more | `extent_index: HashMap<(VolumeId, u64), u32>`, a second copy of what the slot says |
| the extent, volume side | **~215 B** | GEM forward `BTreeMap<u64, ExtentLocation>` (56 B plus node overhead) and reverse `HashMap<(SlabId, u32), (VolumeId, u64)>` |

Extrapolated at 1 MiB slots:

| | resident |
|---|---|
| one 256 TB drive, empty | **~10 GB** |
| one 256 TB drive, full | **~80 GB** |
| a 160-drive node, full | **~12.8 TB** |
| per PB, full | **~313 GB** |

The issue estimated ~12 GB a full drive from the index alone. The real
figure is about seven times that, and a third of it is paid **before anything
is written**.

### Three more walls

1. **Allocation scanned the bitmap from slot 0 every time**
   (`BitVec::first_one()`). The cost grew with fill, from 25 µs a slot empty to
   168 µs at 90% of 4 M slots, and would be milliseconds on a 244 M-slot drive.
   **Fixed** (below).
2. **Metadata is persisted as one document.** `VolumeManager::persist` encodes
   every volume and every extent and writes the whole record, to the data
   directory and to each metadata slab, on every persist. That happens on every
   mint, every drain step and every metadata change. Its size grows with
   everything allocated: at ~40 B an extent it is ~38 GB per PB at 1 MiB,
   rewritten whole each time. At PB scale this is the first wall to be hit, and
   it is independent of RAM.
3. **Slot indexes are `u32`**, in memory and on disk (the GEM, the slot table,
   the reverse index). A slab caps at 4 Gi slots: **4 PiB at 1 MiB**. That is
   enough for one 1 PB drive, and a wall for larger slabs or smaller slots.

## 2. The budget

**Proposed: resident allocation metadata ≤ 1 GiB per PiB of written
capacity**, and no per-slot cost for capacity that is not written. A full
41 PB node then holds ~40 GiB of allocation metadata, which fits beside
everything else a node runs.

Two levers reach it, and neither alone does:

| extent size | extents per PiB | bytes each at 1 GiB/PiB | what that means |
|---|---|---|---|
| 1 MiB | 1 G | 1 | impossible resident: must be paged |
| 16 MiB | 64 M | 16 | tight, compact entries |
| **64 MiB** | **16 M** | **64** | comfortable with a compact entry |
| 1 GiB | 1 M | 1024 | anything fits |

So: **bulk capacity uses large extents**, and **small extents are paged**.

## 3. The design

### 3.1 Extent size by class

Slot size is already per slab (`SlabFormat::new(slot_size, …)`). The volume
manager, however, uses one slot size for everything. **Proposed:**
* the extent size is a property of the **pool**: role × tier × extent size;
* a volume is placed in one pool and its extents are that pool's size;
* a drive may carry more than one slab to serve more than one class.

Proposed classes: **1 MiB** for hot, small, randomly written volumes (container
roots, PVCs of the small size classes, VM system disks) and **64 MiB** for bulk
(large PVCs, object and backup data, media), with 1 GiB available for archive.
A StorageClass (or the size class a claim rounds to) picks the class.

*(Owner decision A: the classes and which tier/size uses which.)*

### 3.2 One owner record per extent, compact

Today every allocated slot is described three times in memory: `Slab.slots`,
`Slab.extent_index`, and the GEM (forward and reverse). **Proposed, with no
on-disk change:**
* the slab keeps **only its free map** resident (1 bit a slot, with the
  per-chunk summary). The per-slot owner record lives in the on-disk slot
  table, which is already authoritative and already written on every
  allocation. Nothing needs 40 B per free slot in memory;
* the GEM keeps **one compact forward entry** per extent:
  * a slab *ordinal* (u16, into a table of slab ids) instead of a 16-byte uuid;
  * a u64 slot;
  * a packed refcount and generation;
  * mirrors out of line, since most extents have none;

  That is ~24–32 B, in a sorted vector or run-length **extent runs** per
  volume (contiguous virtual extents on contiguous slots, one entry), not a
  B-tree node per extent;
* the **reverse index is not resident**. Drain, evacuation and GC ask "what is
  on this slab", and the slab's own on-disk slot table answers that by a
  sequential scan, at the rare moment it is asked.

Measured target for this step: **~30 B per extent, 0 B per free slot**.
Together with 64 MiB bulk extents that is ~0.5 GiB per PiB, under budget.

### 3.3 Paged, incremental metadata

For 1 MiB classes at scale, and for persistence at any scale:
* **An extent map on disk** per volume, as a B-tree in the metadata region of
  the slab (or a metadata slab), read through a **bounded cache**. What is
  resident is the working set, not the whole map.
* **Incremental persistence:** an append-only log of changes (allocate, free,
  re-point, seal, lineage), fsync'd as today's single write is, and
  checkpointed into the B-tree in the background. This replaces rewriting the
  whole document. Recovery replays the log over the last checkpoint.
* **Free space as extent-range trees per allocation group** once a bitmap per
  slab stops being small. It is 30 MB for a 256 TB drive at 1 MiB, 0.5 MB at
  64 MiB, so it can stay a bitmap for now.

### 3.4 64-bit indexes, one format change

Slot and extent indexes become `u64` in memory and on disk. The on-disk change
is made **once**, with the paged index (3.3): a new slab and metadata format
version, read by the new engine alongside the old, and migrated in place slab
by slab, with the old format still readable for rollback. A single format change is cheaper than several, and
the paged index rewrites the same records anyway.

*(Owner decision B: bundle the 64-bit change with the paged-index format
change, rather than ship it first on its own.)*

## 4. What was done now

**First-fit through a per-chunk free summary** (`drive/freemap.rs`). The
bitmap is split into 64 Ki-slot chunks, each with a free count. The first
free slot is the first chunk with a count, then the first bit inside it. It
gives the same answer as before (a randomised test compares it with a scan), with no format
change. Measured on 4 M slots: allocation is **flat at ~27 µs a slot from
empty to 90% full**, where it had grown from 25 to 168 µs. The remaining
27 µs is the slot-table write.

## 5. Work

| | issue |
|---|---|
| resident compaction: no per-free-slot record, one compact GEM entry per extent, no resident reverse index *(no format change)* | stormblock #155 |
| extent size by pool class *(decision A)* | stormblock #156 |
| incremental persistence: a change log and checkpoints instead of rewriting the whole document | stormblock #157 |
| paged on-disk extent index with a bounded cache; 64-bit indexes; format version and migration *(decision B)* | stormblock #158 |
| emulated 256 TB / 1 PB drives to test the budget at every level | stormcos #92 |

Each is measured by `examples/metadata_footprint.rs`. The budget is a test:
resident bytes per written PiB, and zero per free slot.
