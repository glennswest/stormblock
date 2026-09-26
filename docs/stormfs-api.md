# StormFS data-path API

The block-layer surface StormFS calls to place its chunks and commit its
metadata without a fence. Served under `/api/v1/stormfs` by the management API
(default `:9090`), built with the `stormfs-data` feature (on by default, off in
the RouterOS profile). Like every route but the boot claim it needs the node's
bearer token (`docs/auth.md`). Implemented in `src/volume/chunk.rs`,
`src/volume/versioned.rs` and `src/mgmt/api/stormfs.rs`; versions persist in
`<data_dir>/stormfs.json`.

This text was §9.1.1–§9.1.2 of the original design spec (now
`docs/history/stormblock-spec.md`), the two sections of it that describe code
as it is; it moved here with #131.

## Chunks: allocate, deallocate, trim, extent map (#49)

StormFS keeps its namespace and chunk map in a KV store and puts **no process
in the data path**: a client holding a chunk map issues I/O straight to
StormBlock. Chunk lifecycle therefore has to be a StormBlock operation, and
that is what these routes are (#49). Implementation: `src/volume/chunk.rs`,
`src/volume/versioned.rs`, `src/mgmt/api/stormfs.rs`.

Note the path-parameter syntax: axum 0.8 spells it `{vol}`. The `:vol` above
in earlier revisions of this table was the axum 0.6 form.

A **chunk** is a run of whole slab slots inside one volume, addressed as the
same `(volume, offset, len)` the client then reads and writes over iSCSI or
NVMe-oF. Whole slots, because a slot is the unit the volume can reclaim — it
is already what `discard_granularity` reports.

### Allocate

```http
POST /api/v1/stormfs/allocate
{ "tier": "hot", "len": 4194304, "count": 8,
  "hint_volume": "<uuid>", "zero": true }

200 { "extents": [ { "volume": "<uuid>", "offset": 0, "len": 4194304 }, ... ],
      "tier": "hot", "count": 8, "slot_size": 1048576 }
```

Allocation is **eager and tier-scoped**. StormFS owns *policy* — which tier a
file belongs on — while StormBlock owns *placement*. So the slots are taken
from that tier and mapped now, rather than left to allocate-on-write, which
would place them by the *volume's* policy instead and could fail later on
space this call reported as available. A tier with no room is `507`, never a
quiet substitution of a slower one.

`count` is there because a 1 GiB write at a 4 MiB chunk size is 256
allocations, and one round trip each would put the round-trip count back in
the data path this design exists to keep out of it. The request is
all-or-nothing.

`hint_volume` is optional only once this node has already carved StormFS
chunks somewhere; otherwise it is a `400`. The engine will not pick a volume
for you — handing out another volume's address space is not recoverable.

`zero` defaults to `true` and costs a full write of each chunk. Set it
`false` for a chunk about to be overwritten end to end; a chunk allocated
that way reads back whatever the slot's previous tenant left, except where
the backing device zeroes on discard.

### Deallocate and trim

```http
POST /api/v1/stormfs/deallocate
{ "extents": [ { "volume": "<uuid>", "offset": 0, "len": 4194304 } ] }

200 { "freed": 1, "already_free": 0, "slots_freed": 4,
      "bytes_freed": 4194304, "failures": [] }
```

`trim` takes the same body and frees the same space, and differs in one bit:
deallocate returns the address range to the pool, trim keeps it spoken for.
So a chunk StormFS has punched a hole in cannot be handed to a second caller
while StormFS still maps it.

**Deallocate is idempotent.** The orphan sweeper can crash between freeing an
extent and dropping its queue entry, so it will re-free; freeing what is
already free is a success counted as `already_free`. Making it an error would
wedge the queue permanently on one crash. Deallocating chunks of a volume
that no longer exists is likewise a success — that is the sweeper arriving
late.

Only slots a range covers *completely* are freed, matching the volume's
discard granularity.

### Extent map

```http
GET /api/v1/stormfs/extent-map/{vol}

200 { "volume": "...", "slot_size": 1048576, "virtual_size": ...,
      "chunks": [[0, 4194304]], "owned_bytes": ..., "allocated_bytes": ...,
      "unmapped_owned_bytes": ..., "volume_version": 3, "pins": 0,
      "extents": [ { "offset": 0, "len": 1048576, "slab": "<uuid>",
                     "slot": 12, "ref_count": 1, "generation": 1,
                     "version": 3, "owned": true } ] }
```

For `fsck` reconciling StormFS's chunk maps against what the block layer
actually holds. `owned: false` marks an extent at an address the chunk
allocator never handed out, which is the discrepancy fsck is looking for;
`unmapped_owned_bytes` is the other direction — space spoken for with nothing
under it, either trimmed or allocated and not yet written.

## Fence-free commits and pins (#50)

Three further primitives (#50), which between them remove a journal and a
fencing protocol from StormFS.

```http
POST /api/v1/stormfs/commit
{ "volume": "<uuid>", "offset": 0, "len": 4194304,
  "expected_version": 3,
  "staged": { "volume": "<uuid>", "offset": 8388608 } }

200 { "version": 4, "extents_swapped": 4, "extents_released": 4,
      "failures": [] }
409 { "error": "...", "code": 409, "current_version": 5 }
```

**Versioned-map CAS** and **atomic multi-block write** are the same call. A
writer fills scratch extents wherever it likes, then asks for the target
range to be re-pointed at them. The swap moves extent *identity*, not bytes,
so it is cheap enough to run under one lock, validated in full before
anything is applied — the range either moves or it does not, which is the
atomic multi-block write, and why StormFS needs no journal of its own.
Gating it on a version is the CAS: a writer that stalled long enough to lose
its lease is harmless rather than dangerous, because its data went into
extents nobody points at and its swap fails the check. No fencing round trip,
and no correctness argument that depends on clocks.

A `409` carries `current_version`, so the writer retries without a second
round trip to ask what it missed. Omitting `staged` punches the range out
atomically — a truncate rather than a write. Refused: a staged range with a
gap in it (`400`, rather than replacing live data with zeros half-way
through), a range spanning two versions, and a commit into the range you
staged into.

**Pinned-version reads** are the engine's copy-on-write retention exposed:

```http
POST /api/v1/stormfs/pins        { "volume": "<uuid>" }
201 { "pin": "<uuid>", "volume": "...", "snapshot": "<uuid>", "version": 3 }

DELETE /api/v1/stormfs/pins/{id}
```

A pin is a snapshot, so a commit that supersedes an extent decrements its
reference rather than freeing it, and the pinned reader keeps the bytes it
started with. Read through the `snapshot` volume — it is an ordinary volume,
attached and read through the ordinary export path, which is what keeps the
rule that no process sits in the data path. It also makes tier migration
invisible: a reader pinned to an older version keeps reading the source
chunks until it releases, so allocate-copy-swap-free needs no reader
coordination. Releasing twice is a success. A pinned snapshot cannot be
deleted through the volume API.

**What makes a commit untearable is not a journal.** The durable record of an
extent map is the volume metadata file, written whole and atomically with a
checksum, so a crash finds the whole swap or none of it. Versions live in
`<data_dir>/stormfs.json`, and the write order is load-bearing: versions
first, then the map. A crash between them leaves the version *ahead* of the
map, so a stale writer is told to re-read and finds the old data — it
retries. The other order leaves the version behind a map that has already
moved, and that writer would commit over committed data. Versions must be
monotonic, not gapless, so burning one is free.
