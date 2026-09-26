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
`drive`. A slab's domain is its **drive's** identity under whatever labels
the drive was registered with: `drive=<serial>`, else its WWN, else its path.
The drive is read from sysfs (serial, model, WWN; a partition resolves to its
disk), so **two slabs on one drive are one domain whatever their offsets**.
Before #136 a slab in a partition got `drive=file+<offset>`, which made two
partitions of one spindle look like two drives to a mirror. The serial comes
first because it names the physical device. NVMe namespaces share their
controller's serial and fail with it (NVMe-oF namespaces from one StormBlock
target all report `SB010A` on dev), and they are told apart by WWN in each
slab's `drive`.

```bash
# Every call but the boot claim carries the node's token (docs/auth.md):
# -H "Authorization: Bearer $(cat /var/lib/stormblock/api_token)"
# stormdrive (#70) resolves shelf/bay/hba from SES and sysfs and registers
# the drive with them; every slab on it inherits the chain.
curl -X POST :9090/api/v1/drives -d '{"path":"/dev/sdb","labels":{"shelf":"NA1234","bay":"7","hba":"host3"}}'
curl -X PUT  :9090/api/v1/drives/<id>/labels -d '{"labels":{"rack":"r2"}}'
curl        :9090/api/v1/drives/<id>/slabs
```

Empty chains are *unknown*, and unknown is treated as shared: a policy that
asks for separate drives is never satisfied by two slabs nobody can tell
apart. A slab that has been removed from the registry has no domain and
constrains nothing — that is what lets a resync place onto the survivors.

A slab a volume **has stopped trusting** keeps its drive out, and only its
drive: it is compared at the `drive` rung, never at the policy's. One failed
drive in a shelf of 160 does not rule out the other 159 for a
`mirror:2@shelf` leg — for a two-shelf mirror, the rest of that shelf is the
only place the replacement can go. Likewise the member being replaced holds
no domain in its stripe. Until #146 both were counted at the policy's rung, so
an `@shelf` volume could not be rebuilt at all after a failure (a drain moved
its legs, which is why no test had noticed).

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
`margin` is how many more member losses the least protected extent (or
stripe) can take: `1` for a healthy `mirror:2` or `raid5`, `0` once one of its
members is gone — and for an unreplicated volume, always. The rebuild queue
orders by it.

`resync` is the one repair verb: a replaced drive, a slab that was marked
failed, a policy raised from `mirror:2` to `mirror:3`, or a plain volume that
was just told to be a mirror all resolve the same way — rebuild what is
missing from what is left, add what the policy wants, drop what it no longer
does, and forgive any failed slab nothing references any more. It works on
shared slots too: a leg rebuilt for a golden is rebuilt for every clone that
shares it, in one sweep.

A resync runs while the volume serves I/O. An extent only this volume maps is
copied and **published under that extent's lock**, so a write cannot land
between the copy and the switch and leave the new leg stale — which it could
until #146, when every new leg was published at the end. A shared extent is
never written in place (a write copies it away first), so those are published
together at the end, and any that stopped being shared in the meantime is done
again under its lock. The slots rebuilt legs replace are freed only after the
map that stopped naming them is on disk.

Changing a policy is applied in place only between `none` and `mirror:N`.
Converting to or from parity would re-stripe every extent — that is a move,
not a setting, and is refused with 400.

## Where a volume lives (#136)

`GET /api/v1/volumes/{id}` carries `placement`; the listing carries it with
`?placement=true` (it walks the volume's extent map, so it is opt-in there):

```json
"placement": {
  "slabs": [
    { "id": "9ea6…", "role": "system", "tier": "hot", "domain": "drive=WD-WX11D28JFS6T",
      "drive": { "serial": "WD-WX11D28JFS6T", "wwn": "naa.50014ee2b5d3a1f0",
                 "model": "WDC WD20EFRX-68E", "path": "/dev/sda" },
      "node": "C2NR0Q2", "state": "ok", "legs": 740, "shared_legs": 12,
      "parity_legs": 0, "bytes": 775946240 }
  ],
  "drives": [ { "drive": { … }, "node": "C2NR0Q2", "slabs": 2, "legs": 740, "bytes": 775946240 } ],
  "legs": { "policy": "mirror:2", "health": "healthy", "extents": 370,
            "expected": 740, "missing": 0, "unreadable": 0, "failed_slabs": [] },
  "rebuild": "none",
  "arrays": [ { "id": "…", "level": "raid1",
                "members": [ { "index": 0, "state": "active", "drive": { … }, "node": "…" } ] } ]
}
```

- **Slab `state`**: `ok`; `failed` means this volume stopped trusting it,
  and a resync rebuilds its legs elsewhere; `quarantined` means a health report
  took it out of placement; `draining` comes with `drain {moved, remaining,
  failed}`; `missing` means a leg names a slab this node no longer has.
- **`node`** is this node, or the host a fabric drive (`nvme-tcp://`,
  `iscsi://`) is served from. A leg on a RAID-1 member attached over NVMe/TCP
  names the node it lives on.
- **`rebuild`** is `needed` while legs are missing, and `queued` or
  `running` while the rebuild queue holds the volume; its progress is at
  `/api/v1/rebuilds` (below). A drive-level RAID rebuild's progress is not
  kept (#69).
- `array_id` on the volume is set when all of it is on one drive-level array.

The listing also carries `generation`, bumped whenever the node's volume
metadata is persisted: a volume created, deleted, cloned, re-placed or
re-legged. A mirror remembers it and asks `?since=N`, or sends
`If-None-Match: "N"`, and gets a `304` when nothing has changed. Slabs name
their drive the same way (`GET /api/v1/slabs` → `drive`), which is how "look at
a drive and know how much is left" joins to stormdrive.

## Creating volumes

```bash
curl -X POST :9090/api/v1/volumes -d '{"name":"app-data-1","size":"100G","redundancy":"mirror:2"}'
curl -X POST :9090/api/v1/volumes -d '{"name":"db","size":"2T","redundancy":"raid5:4+1@shelf"}'
curl -X POST :9090/api/v1/fstemplates -d '{"name":"golden","size":"4G","redundancy":"mirror:2", ...}'
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

## Retiring a drive: health, quarantine, drain

The drive plane (stormdrive, #70) talks to the engine in two verbs.

```
POST /api/v1/drives/{id}/health  {"state":"failing","reason":"SMART: 12 pending sectors"}
POST /api/v1/drives/{id}/drain
GET  /api/v1/drives/{id}/drain          → {"state":"running","moved":…,"remaining":…}
DELETE /api/v1/drives/{id}/drain        → cancel; what moved stays moved
```

**Health.** `degraded`, `failing`, `failed` or `missing` **quarantines** every
slab on the drive — no new allocation lands there, existing data stays
readable and writable in place — and puts the slab in the **failed set of
every redundant volume with a leg on it**, so those volumes stop reading and
writing that leg immediately. An unreplicated volume's only copy is left
alone: distrusting it would make the data unreadable, not safer. `healthy`
lifts the quarantine; the volumes' failed sets clear on their next `resync`.
Every redundant volume it touched is then **rebuilt automatically** (next
section). `failed` and `missing` (or `"drain": true`) also drain the drive —
after that rebuild finishes, since both would otherwise move the same legs.
The rebuild takes the redundant volumes off it from their surviving members;
the drain then takes what has no redundancy, from the drive itself if it still
answers. The response says `"rebuild": <job>` and `"drain_after_rebuild"`.

**Drain** moves every leg off every slab on the drive — data and parity —
one extent at a time, taking the map and registry locks per extent and
yielding between, so I/O keeps flowing and a cancel lands within one extent.
Each move keeps the leg off the domains of its extent's other legs, and a
slot shared by a golden and its clones is followed by every map that names
it. A leg that fails to move is skipped and listed rather than stalling the
rest. Terminal states: `empty` — nothing of any volume remains, safe to pull;
`stuck` — some legs could not move (the drive stays quarantined);
`cancelled`. A drive whose slab holds the volume metadata is refused; move the
metadata first.

## Rebalance by failure domain

`RebalanceStrategy::ByFailureDomain { rung }` (#71 item 3) runs two passes:
first it separates legs of one extent — or members and parity of one stripe —
that share a domain at the rung, which happens to data placed before the
labels existed; then it evens out allocation across the domains at that
rung, which is what a node does after a shelf is added. Both passes are
per-leg moves that respect the extent's other legs.

## The parity write hole

A parity write is two writes, and a crash between them leaves the stripe's
parity stale. With a data directory, a parity volume keeps a **dirty-stripe
log** (`<data_dir>/stripes-<volume>.log`): a stripe is marked before its
read-modify-write (one fsync the first time it goes dirty since the last
flush — not per write), and `flush` clears the log, since after a flush
nothing is mid-write from the consumer's point of view. On restart the
stripes left in the log are recomputed and only those. Without a data
directory there is no log and `resync?verify=true` remains the recovery.

## Restripe

```
POST /api/v1/volumes/{id}/restripe {"redundancy":"raid5:4+1"}
```

Converting to or from parity rebuilds the placement: every extent is copied
into a scratch placement under the new policy, the volume takes that map in
one swap, and the old slots are released. It holds the volume's mapping lock
throughout and is refused while the volume is exported or attached — it is
offline by design. `none`/`mirror` to `mirror` does not need it: set the
policy and `resync`.

## Rebuilding after a failure (#146)

There is no drive-level array to rebuild. What a failed drive leaves behind is
**a set of volumes each missing a member**, and each rebuilds onto drives of
its own choosing. Their extents were spread over the pool when they were
written, so the rebuilds read from and write to many drives at once: a
drive's worth of data comes back from the whole pool, not from one spare.

A health report of `degraded`, `failing`, `failed` or `missing` starts it; so
can an operator:

```
GET    /api/v1/rebuilds              → settings, queued, running, bytes_copied, items (the jobs, newest first), count
GET    /api/v1/rebuilds/{job}        → one job: every volume, its margin, state, legs, bytes, errors
POST   /api/v1/rebuilds {"volumes":["pvc-a","…"]}   → these (by id or name); {} = every volume not healthy
DELETE /api/v1/rebuilds/{job}        → stop it; what was rebuilt stays rebuilt
PUT    /api/v1/rebuilds/settings {"parallel":8,"extents_in_flight":8,"max_bytes_per_sec":524288000}
```

- **One queue for the node, most endangered first.** Ordered by `margin`: a
  `mirror:2` that lost a member (margin 0) goes before a `mirror:3` that lost
  one (margin 1), whichever failure queued them.
- **Parallel twice over.** `parallel` volumes at once (default 4), and
  `extents_in_flight` extents of each at once (default 4), so one large volume
  is not copied a slot at a time.
- **Throttled as a whole.** `max_bytes_per_sec` is one budget shared by every
  rebuild on the node (default 0, no limit): ten rebuilds take no more of the
  drives than one would. A rebuild pays for an extent before it locks it, so
  a rebuild waiting for its budget never holds a write up.
- **Durable as it goes.** Every 4096 rebuilt legs the map is persisted and the
  slots it stopped naming are freed; a crash loses at most that much progress.
- **A second failure** that hits a volume mid-rebuild queues it again for
  when the current pass ends — that pass may already be past what it took.
- A job ends `done` when every volume is healthy, and `partial`, with the
  reasons, when one is not (typically: too few domains left to hold its
  policy). Unreplicated volumes are never queued: there is nothing to rebuild
  them from.
- A manual `resync`, or `spec.resync`, of a volume the queue holds is refused
  (409) or left to the queue; a drain of a drive whose rebuild is running is
  refused until it finishes.

`[rebuild]` in `stormblock.toml` sets the starting values (`automatic`,
`parallel`, `extents_in_flight`, `max_bytes_per_sec`); the settings endpoint
changes them live and is not persisted.

**Measured** on dev (`examples/rebuild_rate`, 8 slabs on files of one virtual
disk, 16 `mirror:2` volumes, one slab failed, 64 MiB to rebuild, median of 3
interleaved runs): on the page cache — the engine alone — 1.8 GB/s one volume
at a time and 3.1 GB/s eight at once; with O_DIRECT, 126 MiB/s one at a time
and ~200 MiB/s in parallel, which is that one virtual disk's ceiling rather
than eight drives'. Run-to-run spread is wide (O_DIRECT 98–290 MiB/s) on a
shared host, so read those as scaling, not as a drive's rate. What a 256 TB
drive takes on real hardware is the test stormcos#92 (emulated drives) and a
shelf are for.

## Not in this cut

- StormFS chunk/versioned volumes stay `none` — StormFS replicates above.
- Erasure coding wider than two parity members (`k+m` with m > 2), and a
  periodic scrub that compares mirror legs and parity on a schedule (#159,
  #160). `resync?verify=true` is the manual parity check.
- A restripe of a volume with live writers (it is offline).

## Whole-disk goldens: VM images, cloud images, ISOs

A golden is a volume; nothing in the copy-on-write path assumes a
filesystem. A whole VM disk — GPT or MBR, partitions, whatever is inside
them — is stored as raw bytes and cloned like anything else. Redundancy
applies per clone.

```
POST /api/v1/volumes/import {"name":"ubuntu-24.04","url":"https://cloud-images.ubuntu.com/…/noble-server-cloudimg-amd64.img","redundancy":"mirror:2"}
GET  /api/v1/volumes/import/{id}      → downloading | writing | sealing | done | failed, bytes walked / written
POST /api/v1/volumes/{golden}/clone   {"name":"vm-7"} → a disk with its own GPT GUID / MBR signature
POST /api/v1/volumes/{clone}/attach   → the VM's disk, over ublk or NVMe-TCP
```

Formats, detected by magic: raw (an ISO is raw), qcow2 (zero and
zlib-compressed clusters; no backing chain — flatten first), VMDK
(monolithicSparse, streamOptimized as an OVA/OVF export carries, and the
descriptor + flat extent), and the VMDK inside an OVA. Only the clusters
the image carries are written, so a 2 GB cloud image with 600 MB used
costs 600 MB once and each VM pays what it writes. `vhd`/`vhdx` are
recognised and refused; convert with `qemu-img convert -O qcow2`.

**What is inside is found and read** (#147). After the bytes are written and
before the golden is sealed, the import finds every XFS and ext2/3/4
filesystem the image carries: the volume itself, or each GPT partition. It
opens each with the userspace reader for its kind (`fio-xfs`, `fio-ext4`),
names the OS from `/etc/os-release`, and walks the whole tree (for XFS, every
v5 checksum on the way). The status lists them:

```json
"filesystems": [
  { "partition": 3, "kind": "xfs", "label": "BOOT", "walked": { "entries": 370, "directories": 8, "files": 360 } },
  { "partition": 4, "kind": "xfs", "label": "rocky", "os": "Rocky Linux 9.8 (Blue Onyx)",
    "walked": { "entries": 37473, "directories": 5101, "files": 28941 } }
]
```

A filesystem that is recognised and does not read fails the import: an
image nothing will boot should not become a golden every VM clones.
`"verify": false` keeps it anyway, still listing what was found, without the
walk. Measured on dev: the Rocky 9 GenericCloud image (646 MB qcow2, 1.2 GB
written) imports, walks and seals in 19 s. `xfs_repair -n` passes on that root
partition, and `xfs_db` counts 34,306 inodes in use against the walk's 37,473
entries (hard links are entries of one inode). A whole volume that is itself
XFS is recorded as `fs.kind: xfs`.

What the engine stamps on a clone is the **disk** identity — GPT disk GUID
(both headers, CRCs redone) or MBR signature — because that is what a
host derives `PARTUUID` from, and two clones with one identity on one host
collide the way two ext4 clones with one UUID do. What lives inside the
partitions (filesystem UUIDs, `machine-id`, SIDs, hostname) is the guest's
to change on first boot — cloud-init, sysprep — exactly as with any cloned
VM disk. An ISO has no identity to stamp and is cloned as-is.
