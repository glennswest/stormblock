# Volume-level redundancy

*2026-08-28.* Redundancy is a property of a **volume**, not of a drive.

`src/raid/` is drive-level: a `RaidArray` mirrors or stripes whole member
devices, a slab sits on top, and every volume on that slab gets the same
protection. That is one answer per node, and it is not the model. What a node
needs is a **mix**: `app-data-1` as a two-way mirror, a database volume as
4+1 parity, a golden image mirrored so every clone of it is mirrored too,
StormFS scratch with no protection at all — all on the same drives. System
and kernel pallets are mirrored as pallets (see below); data is protected per
volume. This is what zeroboot installs onto. Drive-level `RaidArray` stays as
a leg transport (`nvme-tcp://`, `iscsi://` members) and for whole-device use.

## The model

A thin volume is a map from virtual extents to slab slots. Redundancy adds
legs to that map:

| policy | what an extent maps to | distinct domains per extent |
|---|---|---|
| `none` | one slot | 1 |
| `mirror:N` | N slots, each a full copy | N |
| `raid5:D+1` | one slot; every D consecutive extents form a **stripe** with one parity slot (P) | D+1 |
| `raid6:D+2` | as above with P and Q | D+2 |

Every leg of an extent — and every member and parity leg of a stripe — lands
on a distinct **failure domain**. That is a boundary, not a preference: an
allocation that cannot be spread is refused, and a create that the node could
never satisfy is refused up front (HTTP 409). Nothing silently spills onto a
drive that already holds another leg.

`raid10` is accepted as a spelling of `mirror:2`. Striping for throughput is
what organic placement already does — each extent picks its own slabs — so
there is nothing for a "10" to add.

### Bigger than a drive

A volume is thin and its extents are placed one at a time, so a volume larger
than any drive is the ordinary case: a 10 TB `mirror:2` across six 4 TB drives
needs no arrangement, each 1 MB extent picks two drives with free space. The
only capacity a policy is bounded by is total free space ÷ overhead
(`mirror:2` = 2×, `raid5:4+1` = 1.25×), and the only shape it needs is
*enough distinct domains*.

## Failure domains

A domain is an ordered chain of `rung=value` labels from widest blast radius
to narrowest:

```
site / building / room / row / rack / node / hba / shelf / bay / drive
```

Two slabs are *the same domain at rung R* when their chains agree through R.
A policy names the rung it spreads at (`mirror:2@shelf`); the default is
`drive`. A slab's domain is its device's identity (`drive=<serial>`, or the
path for a file) under whatever labels the drive was registered with:

```bash
# stormdrive (#70) resolves shelf/bay/hba from SES and sysfs and registers
# the drive with them; every slab on it inherits the chain.
curl -X POST :8080/api/v1/drives -d '{"path":"/dev/sdb","labels":{"shelf":"NA1234","bay":"7","hba":"host3"}}'
curl -X PUT  :8080/api/v1/drives/<id>/labels -d '{"labels":{"rack":"r2"}}'
curl        :8080/api/v1/drives/<id>/slabs
```

Empty chains are *unknown*, and unknown is treated as shared: a policy that
asks for separate drives is never satisfied by two slabs nobody can tell
apart. A slab that has been removed from the registry has no domain and
constrains nothing — that is what lets a resync place onto the survivors.

## What happens on I/O

**Mirror.** A write goes to every trusted leg concurrently and is acknowledged
when all of them have it. A read takes one leg (rotated per extent, so mirrors
share the load) and falls through to the next on error. A first write to an
extent zero-fills the rest of the slot on every leg, so legs are identical
from the first byte and unwritten bytes read as zero from any of them.

**Parity.** A write is a read-modify-write under a per-stripe lock: old data,
new data, `P ^= old ^ new`, `Q ^= g^i·(old ^ new)`. A member whose slot cannot
be read is reconstructed from the rest of the stripe — one loss with P, two
with P and Q — and a write to such a member rebuilds it onto a fresh domain on
the way through. Unallocated members are zeros, which is what lets a stripe
grow a member at a time. The RAID-5 write hole is the same as md without a
journal: a crash between the data write and the parity write leaves that
stripe's parity stale until `resync?verify=true` recomputes it.

**Copy-on-write.** A clone inherits its source's policy: shared extents are
already replicated, and a write copies the extent to a fresh set of legs. For
a parity volume the stripe's parity group is shared too and is copied — with a
full recompute — the first time a write in that stripe diverges.

**Failed set.** A leg whose write (or read) fails puts its slab into the
volume's failed set, persisted with the volume: skipped for every read and
write from then on, so a stale leg is never served. The volume reports
`degraded`. New allocations avoid it.

## Health and resync

```
GET  /api/v1/volumes/{id}          → "redundancy": "mirror:2", "health": "degraded", "physical_bytes": …
GET  /api/v1/volumes/{id}/health   → legs expected / missing, unreadable extents, failed slabs
POST /api/v1/volumes/{id}/resync   → rebuild every missing leg onto a fresh domain; clear the failed set
POST /api/v1/volumes/{id}/resync?verify=true   → also recompute and rewrite every stripe's parity
PUT  /api/v1/volumes/{id}/redundancy {"redundancy":"mirror:3"}
```

`health` is `healthy` (every leg the policy asks for is on a trusted slab),
`degraded` (something is missing but everything is readable) or `failed` (an
extent has no readable leg, or a stripe has lost more than its parity covers).

`resync` is the one repair verb: a replaced drive, a slab that was marked
failed, a policy raised from `mirror:2` to `mirror:3`, or a plain volume that
was just told to be a mirror all resolve the same way — rebuild what is
missing from what is left, add what the policy wants, drop what it no longer
does, and forgive any failed slab nothing references any more. It works on
shared slots too: a leg rebuilt for a golden is rebuilt for every clone that
shares it, in one sweep.

Changing a policy is applied in place only between `none` and `mirror:N`.
Converting to or from parity would re-stripe every extent — that is a move,
not a setting, and is refused with 400.

## Creating volumes

```bash
curl -X POST :8080/api/v1/volumes -d '{"name":"app-data-1","size":"100G","redundancy":"mirror:2"}'
curl -X POST :8080/api/v1/volumes -d '{"name":"db","size":"2T","redundancy":"raid5:4+1@shelf"}'
curl -X POST :8080/api/v1/fstemplates -d '{"name":"golden","size":"4G","redundancy":"mirror:2", ...}'
```

With a policy `array_id` is not needed — the volume's extents pick their own
slabs. `[[volumes]]` in `stormblock.toml` and the `--volume name:size:policy`
CLI flag take the same spelling. Spellings: `none`, `mirror` (2), `mirror:N`,
`raid1`, `raid10`, `raid5:D+1`, `raid5:M` (M members, one parity),
`raid6:D+2`, `raid6:M`, any with `@rung`.

## What is recorded where

- **Volume metadata V4** (`volumes.dat`): each extent's legs, each stripe's
  parity group (with its width), the policy, the failed set. A V3 file loads
  as unreplicated volumes.
- **Slab slot tables**: every leg records the `(volume, extent)` it belongs
  to — parity slots record a tagged index (`PARITY_TAG | leg << 56 | stripe`)
  — and its generation, which a copy-on-write raises. So `rebuild_from_slabs`
  recovers legs and parity groups on its own, and on restore the record wins
  except where the slot table is provably newer (a higher-generation slot for
  the same extent: a copy-on-write the record never saw).
- **GEM reverse index**: every leg, data or parity, maps back to its owner,
  so GC, evacuation and drain see all of them. `evacuate_slab` moves the leg
  that is on the slab and keeps the others where they are.

## Pallet-level mirror (#56)

A pallet is a sealed, self-contained partition, so its mirror is simpler than
a volume's: the same pallet on N drives, each leg a complete, independently
bootable candidate. Firmware needs no notion of a mirror — its boot order is
the failover. `PublishSpec.copies` places the extra legs on drives that hold
none (by failure domain); a copy lands at priority 0 and takes the source's
attributes only once it has verified, so a half-copied leg is invisible to
the ladder rather than a candidate that fails. `GET /api/v1/pallets/status`
groups legs by name and version with `copies_wanted` and `degraded`;
`POST /api/v1/pallets/resync` refills a lost leg; `PUT /api/v1/pallets/mirrors`
records how many drives a name should be on (`<data_dir>/pallet_mirrors.json`
— the drives carry no record of it).

## Not in this cut

- StormFS chunk/versioned volumes stay `none` — StormFS replicates above.
- Converting to or from parity (a restripe).
- A write-intent journal for the parity write hole; `resync?verify=true` is
  the recovery.
- `POST /api/v1/drives/{id}/drain` and the health inbound (#70 items 3–4);
  `rebalance` by failure domain (#71 item 3).
