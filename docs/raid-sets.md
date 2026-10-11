# RAID sets on a shelf, with hot spares

**Status:** built (#252, #168, 2026-10-02). The reference for what
`src/raid/` and `/api/v1/arrays`, `/api/v1/shelves`, `/api/v1/spares` do.

The owner's decision on #252 (2026-10-02): a shelf of drives (a NetApp
DS2246, 24 × 1 TB; the rows of 16 on a 160-bay box) is divided into
**several drive-level RAID sets**. Each set is its own failure domain. Hot
spares sit beside them, per shelf or global. Volumes are allocated onto the
sets. This reverses `docs/multi-drive.md`'s "never a RAID across drives" for
parity at shelf scale. The reason is per-volume parity's cost on our load: a
CoW clone's first write into a stripe reads the whole stripe and writes new P
and Q. On a set, a clone's CoW is one extent write and parity is handled
underneath. Per-volume `mirror` stays, for a volume that must survive losing
a whole set or a whole shelf.

## The shape

```
shelf ds1 (a pool name, not a stored object)
 ├─ set ds1-a   RAID-6 of 11   ─ one slab, data role, general pool
 ├─ set ds1-b   RAID-6 of 11   ─ one slab, data role, general pool
 └─ spares      2 drives, pool "ds1"
global spares   pool ""
```

- A **set** is a `RaidArray`: RAID 1, 5, 6 or 10 over whole drives, presented
  as one block device. The engine formats one slab on it: data role, with a
  metadata region, in the general pool (not dedicated). Volumes are placed
  onto it the way they are placed onto any drive.
- **Its failure domain** is `shelf=<pool>/set=<name>/drive=raid-<uuid>`, under
  the node's own rungs. `set` is a rung between `shelf` and `bay`. Two sets
  are always two domains, so `mirror:2@set` puts a volume's legs on two
  sets, and `mirror:2@shelf` on two shelves.
- A **shelf** is the name its sets and spares carry as their pool. It is
  written on every member's and every spare's superblock, so a restart groups
  them again. Nothing else stores it.

## On disk

Each member keeps its first MiB for the array:

| bytes | |
|---|---|
| 0 – 4 KiB | superblock (version 2): array uuid, member uuid, slot, level, stripe unit, data size, `events`, set name, pool, and the **slot table**: every slot's member uuid, state and rebuild position. CRC32C. |
| 64 KiB – 1 MiB | **write-intent bitmap**: one bit per chunk of member data (64 MiB, or larger so the map fits) |
| 1 MiB – | data |

A spare carries a spare superblock (level 0, no array) naming its pool.

Layout of the data: RAID-1 mirrors member offset = array offset (the format
stormstorage's legs already have). RAID-5/6 are left-symmetric. P rotates
from the last member backwards, Q (RAID-6) is on the member after P, and data
follows Q. RAID-10 is near-2: members 2k and 2k+1 mirror each other, and
units are striped across the pairs. RAID-6's Q is the real GF(2^8) syndrome
(generator 2, polynomial 0x11D). Any two lost strips of a stripe are
recovered: two data strips, data with P, data with Q, or P with Q.

## The rules the I/O keeps

- **One stripe is changed under its lock.** A small write is a
  read-modify-write: old data, old P and Q, and the change folded in. A write
  with a member missing is a reconstruct-write: the column is read, the
  missing strips are recovered, the new data is laid over, and parity is
  recomputed. Two writers never lose each other's parity update. A rebuild
  or resync never sees a stripe half written.
- **A write sets its bitmap bits on disk first**, on every member. Bits are
  cleared lazily, after the chunk has been idle and a flush has made it
  durable, and on a clean `close`.
- **A member is read only where it holds the data.** An active member is read
  anywhere. A rebuilding member is read only below how far its rebuild has
  got; above that it is treated as missing (#175). A write to a mirror range
  that straddles that point goes to the rebuilding member for the part below
  it, and the rebuild copies the rest after.
- **A member whose read, write or flush fails is failed.** The I/O carries on
  degraded, and the superblocks say so before the I/O is acknowledged.
  Otherwise a restart would trust the stale member again. The one exception:
  a member is **not** failed when that would lose data (the last copy of a
  mirror, a RAID-10 member whose partner is gone, a parity member beyond the
  level's tolerance). Then that I/O returns the error, and the set keeps
  serving everything else.

## A failure, a spare, a rebuild

1. A member fails: an I/O error, `POST /api/v1/drives/{id}/health
   {"state":"failed"|"missing"}` from stormdrive, or
   `POST /api/v1/arrays/{id}/members/{slot}/fail`. `events` moves and the
   survivors' superblocks record it.
2. The set's supervisor takes a spare: the smallest one that fits from the
   set's own pool, else from the global pool, never another shelf's. It
   writes a member superblock over the spare's and starts the rebuild. With
   no spare, the set stays `degraded` and says why in the log. It retries
   when a spare is added (`POST /api/v1/spares`) or when it is given a drive
   (`POST …/members/{slot}/replace`).
3. The rebuild walks the member data a few MiB at a time under the stripe
   locks, so live I/O keeps flowing. It reconstructs the slot's strips, P or
   Q included, and records how far it has got in the superblocks every 30 s.
   A restart resumes it from there. When it is done the member is `active`.
   Progress is `status.rebuild` on the array. Its rate can be capped:
   `PUT /api/v1/arrays/{id}/rebuild {"max_bytes_per_sec": …}`.

**A drive reported `failing`** (SMART predicting failure) is replaced by a
spare **while it still serves** (#256), so the set never runs degraded for
it:
- The slot's device becomes a tee. Reads come from the failing drive, and
  every write to member data goes to both drives.
- A copy walks the member under the same stripe locks writes take, reading
  the failing drive and writing the spare. Its progress is `status.rebuild`.
- The spare then takes the slot under a member uuid of its own. The
  superblocks are rewritten and the old drive's is wiped.
- If the failing drive stops answering a read mid-copy, it is failed and the
  spare takes the slot as an ordinary rebuild from where the copy was.
- A write error on the spare abandons the replacement, and the slot stays as
  it was.
- Nothing below the member data (superblock, bitmap) is written to the spare
  before the swap, so a stop mid-copy leaves it a spare again.
- `POST …/members/{slot}/replace {"drive_uuid"}` on an **active** slot does
  the same with a drive of the operator's choosing.

**A drive reported `missing`**, or gone at assembly, is failed as
**missing**, not as failed on I/O (#256):
- While a member is missing the set **keeps its write-intent bitmap**. It is
  not cleared when idle, and not zeroed at assembly, so it records every
  chunk written while the drive was gone.
- The slot record marks the member missing, with the set's `events` at the
  time.
- If the drive comes back, its superblock saw everything up to then, and no
  spare took the slot meanwhile, assembly **re-adds** it: only the chunks the
  bitmap names are rebuilt onto it from the others, and it serves again.
  Then the bitmap clears as usual.
- A drive that failed on I/O is not re-added. Neither is one whose
  superblock is older than when it went missing: it stays failed, and a
  spare or a replace rebuilds the slot in full.
- An older engine writes these record bytes as zeros, which only ever means
  a full rebuild.

The response to a drive health report carries `raid_member: {array, slot,
failed, replacing_with}`. The array's members carry the drive's identity (serial, WWN,
model, path) and its registration labels (`shelf=…/bay=…`), so the dead bay
can be found. Lighting its LED (SES) is stormdrive's job (stormdrive#44).

## Assembly

At startup the daemon reads the superblock of every configured drive before
it scans for slabs. In each array, the copy with the most `events` describes
it, and each slot is matched to the drive whose superblock carries that
member uuid. A slot whose drive is absent is failed. An array missing more
than its level tolerates is **not** assembled, and its drives are left
alone: nothing formats over them. The bitmap's dirty chunks (the union over
the members) are resynced: parity is recomputed from the data, and a mirror's
other legs are copied from the first one. Then the slab on the array is
adopted with its volumes. Members and spares are taken out of the plain-drive
slab scan, `--raid` and the raw namespaces.

Drives registered after startup (`POST /api/v1/drives`, an `nvme-tcp://` leg)
are assembled on request: `POST /api/v1/arrays/assemble` with no body (every
open drive not already in use) or with `{"drive_uuids": […]}`. The report
names the arrays assembled or refused, the spares taken back, the damaged
superblocks, and the **stale** drives. A stale drive was a member and has
since been replaced. It is left alone; `force` on a spare or create request
reuses it.

## API

| | |
|---|---|
| `POST /api/v1/shelves` | lay a shelf out: `{"name":"ds1","drive_uuids":[…bay order…],"level":"raid6","sets":2,"spares":2,"stripe_kb":64}`. The last `spares` drives become spares in pool `ds1`. The rest are dealt into `sets` sets of consecutive drives (`ds1-a`, `ds1-b`, …), as even as they go. A layout that leaves a set too small for its level is refused (400). A failure part-way undoes the sets already made. |
| `GET /api/v1/shelves`, `GET /api/v1/shelves/{name}` | the sets and spares of each pool, usable bytes, and the worst set's state |
| `POST /api/v1/arrays` | one array: `{"level", "drive_uuids", "stripe_kb", "name", "pool", "spares": […], "dedicated"}`. `dedicated` defaults to true (#150, a consumer's array); a set on a shelf is `false`. |
| `GET /api/v1/arrays[/{id}]` | `name`, `pool`, `status` (`clean` / `rebuilding` / `degraded` / `failed`, `failed`, `tolerated`, `dirty_chunks`, `rebuild`, `scrub`), `events`, `members` (slot, uuid, state, `drive`, `labels`, `rebuilt_bytes`), the slab and its `domain`, the volumes on it |
| `POST /api/v1/arrays/assemble` | see Assembly |
| `POST /api/v1/arrays/{id}/members/{slot}/fail` | fail a member (to pull its drive). 409 when it would lose data. |
| `POST /api/v1/arrays/{id}/members/{slot}/replace` | `{"drive_uuid"}`: put a drive into a failed slot and rebuild onto it, or replace an **active** slot while it serves (#256) |
| `POST /api/v1/arrays/{id}/members`, `DELETE …/members/{uuid}` | grow or shrink a RAID-1 (stormstorage's leg moves, `migrate_to_local`) |
| `POST / GET / DELETE /api/v1/arrays/{id}/scrub` | verify every stripe or mirror unit, and with `repair` (default) rewrite parity or the other legs where they disagree; `max_bytes_per_sec` caps it; progress and mismatch counts |
| `PUT /api/v1/arrays/{id}/rebuild` | `{"max_bytes_per_sec"}` |
| `DELETE /api/v1/arrays/{id}` | refused while a volume is on it. Otherwise it wipes the members' superblocks so the drives can be reused (`?keep_superblocks=true` leaves them). |
| `GET / POST /api/v1/spares`, `DELETE /api/v1/spares/{uuid}` | `{"drive_uuid","pool"}`; `""` is the global pool. Removing a spare wipes its superblock. |

A drive that is a member or a spare is refused, with 409, by another create, a
spare request, a replace or a RAID-1 add (#215). So is a drive holding a slab
the engine has registered, and a drive carrying the superblock of an array
this engine does not hold: assemble it, or pass `force` to overwrite it.
`DELETE /api/v1/drives/{id}` refuses a member or spare unless `force`.

`GET /api/v1/health` carries `raid`: the worst set's state, when the node has
any. Metrics: `stormblock_raid_state{array,name}` (0 clean, 1 rebuilding,
2 degraded, 3 failed), `stormblock_raid_failed_members`,
`stormblock_raid_rebuild_percent`.

## Not here

- **Bay identity and LEDs**: enclosure + slot from SES, and lighting a failed
  bay's LED, are stormdrive's. The engine shows the labels a drive was
  registered with.
- **Growing or reshaping a set**, adding a member to a parity set, or changing
  its level (#387). A set's width is fixed at creation.
- **`degraded`** on a drive is reported, not acted on (`failing` is replaced,
  `failed` and `missing` are failed: see above).
- **A member that comes back while the set is running** is re-added at the
  next assembly (`POST /api/v1/arrays/assemble`, or a restart), not on its
  own.
- **Discard** is passed down by RAID-1 only.
- **Version-1 superblocks** (before this, never reassembled) are not read. A
  drive carrying one counts as blank.
- **`[[arrays]]` in the config file** (#165) makes a named set at the
  daemon's start when no set of that name was assembled, only from drives
  that hold nothing (no slab, no set). No spares or pools from the file: those
  are the API's (`/api/v1/spares`).
