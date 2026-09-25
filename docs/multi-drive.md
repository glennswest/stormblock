# Multi-drive: pools, placement and failure domains

**Status:** design (#142, 2026-09-25). Written against what the engine does
today — proved by `tests/integration_multidrive.rs` — and split into work.
Companion to [redundancy.md](redundancy.md), which specifies per-volume
redundancy, and [data-placement.md](data-placement.md).

The owner's frame:

> "we need to figure out multi-drive soon."
>
> Redundancy is **per volume** — members of a volume on different drives —
> **never a RAID across drives**. The multi-drive design places volume members
> by failure domain (drive < shelf of 160 < rack) and rebuilds per volume
> (#146).

And the scale it has to hold: 160-drive shelves of 256 TB, stacked per rack
(stormcos#93), on a fleet of mixed hardware where most nodes today have one
or two drives.

---

## 1. The model

```
node
 ├─ drive  (identity: serial / WWN — #140; where: shelf, bay, hba — stormdrive)
 │   └─ slab   one per role the drive serves (data; + system on the system drive)
 ├─ drive
 │   └─ slab
 └─ …
pool  = every slab of one role and tier on the node   (implicit, not a thing you make)
volume = extents; each extent has N legs (mirror) or a stripe (parity);
         every leg of an extent on a distinct domain at the volume's rung
```

* **A drive is the unit of hardware.** It is known by the identity stormdrive
  uses: serial, then WWN (#136/#140), never its device name or an offset.
  Every slab on it takes that identity as its `drive=` rung, so **one disk is
  one failure domain** however it is partitioned.
* **A slab is the unit of storage.** A drive carries one slab per role (the
  system drive: a system slab and a data slab; every other drive: a data
  slab). A slab is a flat array of slots (1 MiB by default).
* **A pool is implicit:** every slab of one *role* (system / data) and *tier*
  (hot, warm, cool, cold) on the node. There is no "create pool" step and no
  named pool. Adding a drive's slab grows its pool, and draining one shrinks
  it. What an operator asks of a pool is capacity, pressure and tier. That is
  `GET /api/v1/slabs/pool` (a total and a per-tier breakdown) and `/metrics`.
* **No RAID across drives.** The drive-level `RaidArray` stays for what it
  already serves, whole-device legs such as a remote RAID-1 leg over
  NVMe/TCP, but it is **not** how a pool is made or how a volume is protected.
  A pool is never an array. A volume is protected by where its own members
  are.

## 2. Placement

### What happens today (proved)

| volume | how each new extent is placed |
|---|---|
| `redundancy: none` | the **most-free** slab of its role, preferring its tier. A volume's extents **spread across every drive in the pool** (16 extents over 4 drives landed on all 4). |
| `mirror:N[@rung]` | each leg on the most-free slab whose domain differs, at the volume's rung, from the extent's other legs; refused (`InsufficientDomains`) at create when the node cannot promise N distinct domains |
| `raid5:D+1`, `raid6:D+2` | data legs and P/Q legs likewise, every member of a stripe on its own domain |

A volume's placement is visible per slab and per drive (`placement` on
`GET /api/v1/volumes/{id}`, #136). The console's Volumes view reads it.

### Failure domains

A slab's domain is a chain, widest first:

```
site / building / room / row / rack / node / hba / shelf / bay / drive
```

* `drive=` comes from the drive's own identity (#140). Two slabs on one drive
  share it.
* `shelf`, `bay` and `hba` come from **stormdrive**, which registers each drive
  as `POST /api/v1/drives {path, labels}` and relabels it as it learns more
  (`PUT …/labels`). Every slab on the drive follows.
* `rack`, `row`, `room` and `site` come from the node's `[management].topology`.
* A volume's policy names the rung its members are kept apart at:
  `mirror:2@shelf`, `raid6:8+2@rack`. The default is `drive`.

**The rule is a hard boundary.** Two legs of one extent never share a domain
at the volume's rung — on create, on every write that allocates, on resync,
and (fixed with this design) **when a drain moves a leg**. The drain kept legs
apart only at `drive`, so a `mirror:2@shelf` leg drained off a failed drive
could land beside its sibling in the other shelf. The multi-drive test found
it: the volume read *healthy* with both copies of an extent in shelf B.
`drain` now moves with the volume's own rung and never moves a leg into a slab
of the other role (#88).

### How claims spread — proposed

The two kinds of volume want opposite things:

* **A redundant volume should spread.** Its extents are on many drives, so
  when one drive fails, the volumes with a member there are many, each missing
  a little, and **each rebuilds onto different drives in parallel**. That is
  the whole point at 160 drives a shelf (#146). This is what the engine does
  today.
* **A non-redundant volume should not.** Spreading it over every drive in the
  pool means it is lost when **any** of those drives fails: on a 4-drive node,
  four times the loss rate of keeping it on one drive, and nothing gained but
  bandwidth. **Proposed:** a `none` volume keeps **drive affinity**. Its
  extents go to the drive its first extent went to, while that drive has
  room, and spill to the next most-free drive only when it does not.

*(Owner decision 1: drive affinity for non-redundant volumes. Proposed, not
built.)*

## 3. Policy: what a claim asks for

A claim's placement is the volume's policy, and it should come from the
StorageClass. **Nothing carries it today:**

* stormblock-csi reads `qosClass`, `bandwidthClass`, `encrypted` and
  `replicaSlaves`, and `/v1` volume create has no redundancy field.
* rustkube-node reads no StorageClass parameters for the built-in driver. It
  mints size-class blanks with no redundancy, so every PVC is `none`.
* The engine already accepts `redundancy` on `POST /api/v1/volumes` and
  `POST /api/v1/fstemplates`, and **a clone inherits its golden's policy**.

**Proposed parameters** (the same names on both drivers):

| parameter | values | default |
|---|---|---|
| `redundancy` | `none`, `mirror`, `mirror:3`, `raid5:D+1`, `raid6:D+2` | proposed `none` (see below) |
| `spread` | `drive`, `shelf`, `rack`, … (any rung) | `drive` |
| `tier` | `hot`, `warm`, `cool`, `cold` | `hot` |

For the built-in driver, a size-class blank is per `(size, fs, redundancy,
spread)`, named for all four (`pvc-ext4j-1048576m-mirror2-shelf`), and a claim
clones the blank for its class and policy, so it inherits the policy with no
second step.

*(Owner decision 2: the default for a claim that names no policy: `none` as
today, or `mirror` on a node that can hold two drives' worth.)*

## 4. A drive's life

| event | today | wanted |
|---|---|---|
| **added** | `POST /api/v1/drives` opens and labels it; a slab is a second, manual call (`POST /api/v1/slabs`, or `…/adopt` for one that already carries one). Boot adopts the slabs it finds. | A new blank drive gets a data slab by policy (the same `assimilate` policy the boot uses: `off`, `blank`, `any`), and the pool grows by it. A drive carrying a slab is adopted with its data, never reformatted. Then the pool is **rebalanced** onto it (below). *(Owner decision 3: automatic, or on an operator's word.)* |
| **drained** | `POST /api/v1/drives/{id}/drain`: quarantine, then move every leg off, one extent at a time, with I/O flowing. Ends `empty`, `stuck` or `cancelled`. Now respects each volume's rung and role. | Rate-limited against live I/O. A drive is **removed** only when it is `empty` (`DELETE /api/v1/drives/{id}`). |
| **failing / failed** | `POST /api/v1/drives/{id}/health` (from stormdrive): quarantine, and every redundant volume with a leg there stops trusting it (degraded). `failed` also starts a drain. **Rebuild is manual:** `POST /api/v1/volumes/{id}/resync`, one volume per call, synchronous, holding the volume manager for the whole run. | **Automatic, background, per-volume rebuild (#146):** the volumes with a member on the drive, most-endangered first (least redundancy left), several at once onto different drives, throttled against live I/O. |
| **gone** (pulled, not answering) | reported `missing`: as failed | as failed |

The test drives all of the "today" column: four drives in two shelves; a
failed drive is quarantined, the mirror reads degraded, the drain empties the
drive, a resync makes the mirror healthy with one leg per shelf, both volumes
read back unchanged, and a fifth drive in a third shelf grows the pool and
makes `mirror:3@shelf` placeable.

### Rebalance

`PlacementEngine::rebalance` (even distribution, tier affinity, and by failure
domain) exists and is tested, but **nothing calls it**: no API, no timer. After
a drive is added, the pool stays lopsided until writes happen to land on the
new drive. Wanted: `POST /api/v1/slabs/rebalance` (dry-run first), and a gentle
automatic pass after a drive is added.

## 5. Capacity and overcommit

Thin volumes promise more than the pool holds. That is their purpose, and
nothing bounds it today. The only check is `/v1` create's `free_bytes >=
size` against free space at that moment. A write that finds no slot fails
back to the host (NVMe `CAPACITY_EXCEEDED`, SCSI `SPACE ALLOCATION FAILED`).

**Proposed** (stormdrive#13 holds the per-drive half):

* **Per drive:** `overcommit: false` (the default for data drives) or `true`
  with a ratio, set through stormdrive and the console.
* **Per pool:** *promisable* = Σ over its slabs of (capacity × the drive's
  ratio, or × 1). *Committed* = Σ of the virtual size of the volumes placed
  there, where a clone's shared extents count once, at the golden.
* **Enforced when a claim binds** (a volume create, a template clone, a `/v1`
  create). A claim that would pass *promisable* is refused **with the reason**,
  and rustkube-node publishes the pool's headroom to the scheduler
  (rustkube-node#62).
* Reported on `GET /api/v1/slabs/pool`, per drive on `/api/v1/drives`, and on
  `/metrics`: committed, written and free.

## 6. What the console shows

* **Drives:** identity (serial, WWN, model), where it is (shelf, bay, hba),
  health, its slabs and how full each is, overcommit setting, and a drain in
  progress with its progress.
* **Pools:** per role and tier, total, written, committed, free, headroom
  under overcommit, and pressure.
* **Volumes:** where each lives (#136 `placement`): drives, shelves, legs per
  drive, health, and whether a rebuild is owed.

## 7. What is proved, what was fixed, what is next

**Proved** (`tests/integration_multidrive.rs`, over the HTTP API, four file
drives in two shelves): slabs name shelf and drive; a `none` volume spreads
across all four; `mirror:2@shelf` puts one leg of each extent in each shelf;
`mirror:3@shelf` is refused with two shelves; a failed drive is quarantined and
drained while a mirror reads degraded; a resync restores one leg per shelf,
healthy; data reads back unchanged; a fifth drive grows the pool and makes
three shelves available.

**Fixed with this design:** a drain now moves a leg with the volume's own
spread rung (it used `drive`, which broke `@shelf`), and a move never crosses
the system/data boundary.

**Needs a machine with several real drives** to prove on hardware; everything
above ran on files.

**Work** (issues filed from this document):

| | where |
|---|---|
| per-volume background rebuild: automatic on failure, parallel, prioritised, throttled | stormblock #146 |
| redundancy / spread / tier as StorageClass parameters, end to end | stormblock #151, rustkube-node #71, stormblock-csi #21 |
| overcommit per drive → pool admission and headroom | stormblock #152, stormdrive #13, rustkube-node #62 |
| drive affinity for non-redundant volumes *(decision 1)* | stormblock #153 |
| a new drive: slab by policy, then rebalance onto it *(decision 3)*; `POST /slabs/rebalance`; drain rate limit | stormblock #154 |
| Drives and Pools pages | stormconsole #29 (and #32 at 160 drives) |
| drive-level RAID failure states — only for whole-device legs now | stormblock #69 |
| replicas on other servers (a different axis: across nodes) | rustkube-node #68 |

**Decisions for the owner:** (1) drive affinity for non-redundant volumes;
(2) the default policy for a claim that names none; (3) whether a new drive
joins the pool automatically.
