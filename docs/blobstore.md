# The blob store (#732)

A content-addressed store in the engine: write-once objects keyed by their
sha256, put, read (with Range), listed and deleted by digest. It is what
stormblock-registry keeps its OCI blobs in (stormblock-registry#241), in
place of the growable ext4 `registry-data` volume: nothing to grow or fill,
and the space comes from the node's slab pool. Goldens are not in it; they
stay sealed filesystems.

## The surface

On the management port (`:9090`), with the node's token, like the rest of
`/api/v1`. A blob's delete and an upload's abort take the node's token too:
the registry's garbage collection deletes blobs routinely, as a CSI
controller deletes volumes, and a blob deleted is put again by the next push
of it. Everything else under `/api/v1/blobs` that deletes stays the admin's.

A digest is `sha256:<64 lower-case hex>` (the bare hex is taken too).

| request | answer |
|---|---|
| `PUT /api/v1/blobs/sha256:<hex>` (body: the blob) | `201` made, `200` held already (nothing written, the body read and dropped), `400` the bytes hash to something else (`want`, `got`; nothing kept), `507` the pool is full (nothing kept). With a `Content-Length` larger than the pool has free, the `507` comes before a byte is read. `{digest, size, outcome}` and `Docker-Content-Digest` |
| `GET /api/v1/blobs/sha256:<hex>` | the bytes; `Range: bytes=a-b`, `a-`, `-n` answer `206` with `Content-Range`, past the end `416`; `ETag` and `Docker-Content-Digest` are the digest. `404` not held |
| `HEAD /api/v1/blobs/sha256:<hex>` | the stat: `200` with `Content-Length`, or `404` |
| `DELETE /api/v1/blobs/sha256:<hex>` | `200 {deleted}`: `true` if it was held, `false` if not (not an error: a retried delete succeeds). A read already in flight finishes; the space goes back when it ends |
| `GET /api/v1/blobs?after=<digest>&limit=<n>` | `{blobs: [{digest, size}], next}`, in digest order, 1000 a page by default (at most 10,000); `next` is the `after` of the next page, `null` at the end |
| `POST /api/v1/blobs/uploads` | `201 {id, length: 0, location}`: an upload session |
| `GET /api/v1/blobs/uploads` | every session: `{id, length, touched}` |
| `GET /api/v1/blobs/uploads/{id}` | `{id, length}` and `Range: 0-<length-1>`; `404` no such session |
| `PATCH /api/v1/blobs/uploads/{id}` (body: a chunk) | appended at `?offset=`, else at `Content-Range: <from>-<to>`, else at the session's end. An offset that is not the session's length: `416` with `length` (and `Range`). `202 {id, length}` once the chunk is durable |
| `PUT /api/v1/blobs/uploads/{id}?digest=sha256:<hex>` (body: a last chunk, or none) | verified against the digest and sealed under it: `201 {digest, size, outcome, location}`. A mismatch is `400` and **keeps** the session (the client may abort it). A digest held already ends the session as `existed` and keeps none of its bytes |
| `DELETE /api/v1/blobs/uploads/{id}` | `200 {aborted}`; its space goes back |
| `GET /api/v1/blobstore` | `exists`, `blobs`, `bytes` (the blobs' own), `sessions`, `session_bytes`, `packs`, `pack_live_bytes`, `slots_in_use`, `slot_size`, `volume`, `volume_capacity`, **`used_bytes`** (what the store's volume holds on the pool), **`pool_free_bytes`** (what the pool has free for it: the registry holds a sync with "waiting for space" on it), `journal_epoch`, `journal_blocks`, `pending_frees`, `scrub` |
| `POST /api/v1/blobstore/scrub` | `202 {started, scrub}`: every blob read and hashed against its digest, on a task of its own |
| `GET /api/v1/blobstore/scrub` | `{running, started_at, finished_at, checked, bytes, failed: [{digest, error}], skipped}` |

A session survives an engine restart: its length and bytes are journaled
with each append. One untouched for 7 days is aborted. Its hash is kept as
the chunks arrive; after a restart the commit reads the session back to hash
it.

The store is made the first time something is put (or a session opened),
and opened again from its volume when the engine starts and is first asked.
Reads, lists and stats with no store answer as an empty store (`404` for a
blob).

## A blob as the source of something else (#733)

Three engine operations take a blob from the store by digest instead of a
file path, and read it **in place**: nothing is staged or copied first, so a
5 GB ISO costs no temporary space.

| operation | the blob |
|---|---|
| pallet publish (`POST /api/v1/pallets`) and recompose (`…/{id}/recompose`, `add`) | a member `{"name", "role", "kind"?, "source": "blob", "digest": "sha256:<hex>"}` |
| volume import (`POST /api/v1/volumes/import`) | `"blob": "sha256:<hex>"` in place of `file` or `url`; raw (an ISO, a disk image: `"format": "iso"`), qcow2, a sparse or streamOptimized VMDK, or an OVA. A descriptor VMDK names extent files beside it and is refused from a blob |
| image build (`POST /api/v1/images/build`) | a pallet member's `blob = "sha256:<hex>"` in place of `file` or `text` (one of the three) |

The blob is looked up **before** the work starts: a digest the store does not
hold answers `404` naming it (and nothing is made: no pallet, no import job,
no image file); a malformed digest `400`. Each operation then **holds** the
blob until it ends: a `DELETE` of it meanwhile answers as usual and takes the
digest out of the index at once, but its space goes back only when the
publish, the import or the build lets go, as with a `GET` in flight.

Only a build the engine runs reads the store: `stormblock image build` on
the command line has none, and a spec member naming a blob fails there.

## A sealed volume as the source (#737)

The registry keeps each blob as a raw **sealed volume** holding its exact
bytes, its length recorded beside it (stormblock-registry#244, #248). A
pallet publish already takes such a volume as a member (`"source":
"volume", "volume_id", "byte_len"`); the import and the image build take it
the same way, read in place, so the registry never writes a blob to a file:

| operation | the volume |
|---|---|
| volume import (`POST /api/v1/volumes/import`) | `"volume": "<id or name>", "byte_len": <n>` in place of `file`, `url` or `blob`; detection, conversion, verification and sealing as for a file |
| image build (`POST /api/v1/images/build`) | a pallet member's `volume = "<id or name>"`, `byte_len = <n>` (one of `file`, `text`, `blob`, `volume`); the ESP's `from_volume`, `byte_len` (one of `from_dir`, `from_image`, `from_volume`) |

`byte_len` is how many bytes from the volume's start are the content; absent,
the whole volume (its size, padding and all). The volume is looked up before
anything is made: not there `404`, **not sealed `409`** (a source read in
place must not change under the read), secret `409`, a `byte_len` past its
end `400`, `byte_len` without a volume `400`. The volume is never written.
While the import or the build runs it holds a serve hold on the volume
(#267), so a `DELETE` of it is refused (`409`, naming the import or the
build); an unseal meanwhile fails the read instead of letting it finish on
other bytes. `stormblock image build` on the command line has no volumes.

## Where the bytes go

One thin volume, `stormblock-blobs` in the default namespace, placed in the
node's data slabs where it has them (identity and state, which no install
formats). It is made at 1 GiB and grows by a quarter (at least 256 MiB) as
the store's address space fills: a thin volume's size is what the pool
counts as promised (#152), so it follows what the store uses rather than
claiming the pool. Its slot (extent) size is the store's unit, `S`.

- **Slot 0** holds two superblocks (blocks 0 and 1, written alternately by
  generation, crc32c each).
- **The journal** is a chain of segments (at least 4 MiB of contiguous
  slots each). It is written in batches of whole 4 KiB blocks, each with a
  header (the store's id, the epoch, a sequence number, the payload's length
  and crc32c). A batch is never rewritten. Replay stops at the first block
  that is not the next batch of the current epoch. A segment ends in a
  `Next` record naming the one after it.
- **A blob's body** is whole slots of its own, in order (a streamed put asks
  for the slot after the last, so a large blob is mostly one run).
- **Its tail**, the bytes past its last whole slot (all of a blob smaller
  than a slot), is in a **pack**: a slot shared by tails, cut in 4 KiB
  blocks. A 1 KB config takes one block, not a slot; 256 of them share a
  1 MiB slot.
- **An upload session** writes into slots of its own. Its commit copies the
  last partial slot's bytes into a pack and makes the whole slots the
  blob's, without reading or copying them.

The records are `Put` (digest, size, slot runs, tail), `Delete`,
`SessionOpen`, `SessionAppend` (new length, new slots), `SessionEnd` and
`Next`. A **checkpoint** (when an epoch's journal passes 4096 blocks, or
twice what the live set takes) writes the live set as a fresh chain under
the next epoch, then the superblock naming it; the old chain's slots are
then free.

## Durability

Data first, then the record that names it. A put's bytes are flushed before
its `Put` is journaled and flushed; an append's before its `SessionAppend`.
A crash in between leaves space nothing names. Opening the store replays the
journal and **discards every mapped slot of the volume that nothing names**.
That also covers a delete journaled whose discard had not happened.

A journal write or flush that fails leaves it unknown whether the batch
reached the disk. The store then stops: nothing more is journaled or given
back, and requests answer `503`. Once no request holds it, it is opened
again from its volume (replay, sweep).

## Space given back

A delete is journaled, then the digest leaves the index at once. Its slots
go back (by discard, so the pool has them) when the last read of it in
flight ends. A pack goes back when no tail in it lives. A closed pack whose
live blocks fall under a quarter of it is **repacked**: its live tails are
copied to the open pack and journaled, and the old pack goes back once the
reads of the old copies end. Packs sparse at open are repacked then.

## Integrity

`POST /api/v1/blobstore/scrub` reads every blob and compares its sha256 with
its digest; failures are listed and logged at ERROR. The journal and
superblocks carry crc32c. The data's own redundancy is the volume's, which
is the pool's.

## Not done

- The store does not read the volume's blocks ahead of a reader: a pull is
  served 1 MiB at a time, one I/O in flight.
- Puts are committed one journal batch (one 4 KiB block and one flush) each;
  there is no group commit across concurrent puts.
- One store a node, not one per namespace or tenant.
