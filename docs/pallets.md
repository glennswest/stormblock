# Pallets

**Status: implemented, 2026-08-19.** Producer side of the format specified in
[`stormuefi/docs/PALLET-SPEC.md`](https://github.com/glennswest/stormuefi/blob/main/docs/PALLET-SPEC.md)
v1, whose reference reader is `stormuefi-map` — `no_std`, allocation-free and
OVMF-verified. Engine issues: [#51](https://github.com/glennswest/stormblock/issues/51)
(primitives), [#52](https://github.com/glennswest/stormblock/issues/52)
(lifecycle).

> A **pallet** is a GPT partition containing a named, versioned, self-contained
> set of sealed member images and the manifest that describes them.

---

## 1. Why pallets exist

**A pallet is the unit of replacement, and the unit that gets signed.**

Because a pallet names its members by digest, one signature covers the
*combination*. A signed kernel paired with a different signed initramfs is a
different manifest and will not verify. Signing each image separately cannot
give that, and the pairing is exactly what a boot has to get right.

**Replacement granularity is the other half.** The failure mode worth naming is
the OpenShift one: everything is a container, so a z-stream bump rewrites every
container whether or not anything in it changed. A pallet moves a *set* — the
kernel and its modules, an application and the containers it needs, a body of
data — and leaves every other set untouched. What did not change is not
rewritten, and therefore cannot break.

**Kernels are the sharp case.** A kernel should be hard to change, not easy: it
is sealed, read-only, digest-verified, and an upgrade is a new pallet rather
than an edit. A tampered or broken upgrade is refused before it boots, and the
previous pallet is still on the disk *by construction* — nothing overwrote it,
because publishing never writes over anything in use.

**Data gets the same treatment.** `kind = data` is a first-class pallet: a
sealed, versioned, verifiable set of content that ships and rolls back the same
way code does.

---

## 2. Anatomy

### 2.1 Why a partition

1. **Firmware can see it.** A real, non-overlapping GPT partition gets a handle
   and a filesystem binding from firmware with no driver. Overlapping entries do
   *not* get a handle — benched in `stormcos/docs/BOOT.md` — so the allocator
   here refuses to alias rather than trusting arithmetic.
2. **It is relocatable.** Everything inside a pallet is addressed relative to
   the partition start, so a pallet is byte-for-byte copyable to another disk,
   image or ISO. Assembling a bootable image is *concatenating pallets and
   writing a GPT*.
3. **GPT already carries the state.** Priority, tries and the successful bit
   live in attribute bits, so boot selection is readable before any filesystem
   exists — and **activation is an attribute write**, never a data write.

### 2.2 GPT entry

| Field | Value |
|---|---|
| `PartitionTypeGUID` | `A324B90E-CED9-4019-B338-7A5B98E1B7D2` — a stormcos pallet |
| `UniquePartitionGUID` | the pallet's identity; **stable across copies and moves**, and the handle every API call takes |
| `PartitionName` | the pallet name, e.g. `stormcos-boot`; must match the superblock |
| `Attributes` | bit 0 required-partition; bits 48–63 below |

| Bits | Field | Meaning |
|---|---|---|
| 48–51 | `priority` | 0 = never boot; higher wins |
| 52–55 | `tries_left` | decremented per attempt; 0 with `successful=0` means skip |
| 56 | `successful` | confirmed good |
| 57 | `sealed` | never relocate, reuse or GC these extents |
| 58 | `read_only` | never attach any member writably |

Priority is four bits, so activation **renumbers** rather than climbs: the
target takes 15 and its competitors are pushed down below it in their existing
order.

### 2.3 Partition contents

All offsets are relative to the first byte of the partition.

```
+0            superblock (4096 B)
+4096         member table  (member_count × 128 B)
              extent table  (extent_count × 32 B)
              ... padding to member_data_offset (4 KiB aligned) ...
+data         member content
```

**Superblock (4096 B)** — as v1 specifies, plus two fields stormblock writes in
the reserved area:

| Off | Size | Field |
|---|---|---|
| 0 | 8 | magic `STORMPAL` |
| 8 | 4 | `version` = 1 — a newer one is **refused with a diagnostic**, never guessed at |
| 12 | 4 | `superblock_len` = 4096 |
| 16 | 4 | `block_size` — the media's sector size (see §2.4) |
| 20 | 4 | `member_size` = 128 |
| 24 | 4 | `member_count` |
| 28 | 4 | `extent_size` = 32 |
| 32 | 4 | `extent_count` |
| 36 | 8 | `pallet_version` — monotonic; ties broken by GPT `priority` |
| 44 | 8 | `member_data_offset` |
| 52 | 40 | `name` — UTF-8, matches the GPT `PartitionName` |
| 92 | 32 | `manifest_digest` — SHA-256 over the member + extent tables |
| 124 | 4 | `members_crc` (CRC-32/IEEE) |
| 128 | 4 | `extents_crc` |
| 132 | 4 | `superblock_crc` — computed with this field zeroed |
| 136 | 8 | `flags` — mirrors GPT bits 57–58 |
| **144** | **4** | **`kind`** — stormblock extension, see §2.5 |
| **148** | **32** | **`version_label`** — stormblock extension, UTF-8 |
| 180 | 3916 | reserved, zero |

Both extension fields are defined so that **zero means "unspecified"**: a pallet
written before they existed reads back as `PalletKind::Unspecified` with an
empty label, and a reader that ignores them is still correct. They are covered
by `superblock_crc` (which spans 136..4096) and *not* by `manifest_digest`,
which covers only the two tables — so they are not signed content, and neither
are the mirrored flags.

**Member (128 B)**: `name` (40), `role` (16), `kind` u32, `flags` u32 (bit 0
sealed, 1 read-only, 2 digest-present), `byte_len` u64, `extent_first` u32,
`extent_count` u32, `digest` (32), reserved (16).

**Extent (32 B)**: `logical_block` u64, `partition_block` u64 (**partition
relative**), `block_count` u64, `flags` u32, reserved.

The writer emits one extent per member and skips the extent entirely for a
zero-length one. The reader handles many, so a sparse layout is a change in one
function and nowhere else.

### 2.4 Block sizes — the one that bites

Three sizes are in play and they are not the same number:

| | What it is | What stormblock uses |
|---|---|---|
| GPT LBA size | the unit the partition table counts in | 512 for a file-backed device, the device's own for a real drive |
| pallet `block_size` | the unit the extent table counts in | **the same as the GPT LBA size** |
| content alignment | where member content starts | 4 KiB, always |

A file device reports a 4096-byte block size because that is the I/O size it
*prefers*, not because it has 4 KiB sectors — and a file is how disk images and
ISOs are assembled, where every tool and every firmware assumes 512-byte LBAs.
Writing the table in 4096 produced one this code read back happily and `fdisk`
could not find at all. A real 4Kn drive reports 4096 because it *is* 4Kn, and
there 4096 is the right answer; so the rule is "the smallest unit the media
actually addresses", and on read the size is **discovered** by looking for a
header where each candidate would put it.

Making the pallet's `block_size` follow the GPT LBA size means an extent's
`partition_block` is a plain sector offset from the partition start, with no
unit conversion for a pre-kernel reader to get wrong. Content is nonetheless
placed on 4 KiB boundaries, so 512-byte LBAs cost nothing in alignment.

### 2.5 Kinds

`kind` is the discriminator a consumer selects on before it looks at anything
inside. A node carries several pallets at once and they are not
interchangeable: stormuefi wants the boot pallet, stormpump wants the ones
holding application containers, and "what kernel is this node on" wants neither.

| Value | `kind` | What it holds |
|---|---|---|
| 0 | `unspecified` | did not say — written before the field existed |
| 1 | `boot` | kernel + initramfs + cmdline: what firmware selects between |
| 2 | `system` | the platform itself: root image and what makes a node a node |
| 3 | `kernel` | a kernel and its modules, versioned apart from the boot pallet that pairs it |
| 4 | `kube` | control-plane and node components |
| 5 | `app` | an application: the containers one workload needs |
| 6 | `runtime` | dependencies shared between applications |
| 7 | `data` | data or configuration shipped as a sealed set |

**Priority only orders pallets that compete with each other.** A `kube` pallet
does not outrank a `boot` pallet by carrying a bigger number — it is not in the
same race. Renumbering on activation is per kind, and a consumer must filter by
kind before comparing priority. `unspecified` is treated as a candidate for any
kind, so a pallet written before the field existed is never stranded.

### 2.6 Trust chain

```
GPT entry            type + name + attributes   (integrity: GPT CRCs)
  └── superblock     manifest_digest            (integrity: superblock CRC)
       └── member table + extent table          (covered by manifest_digest)
            └── each member: name, role, digest (covered by manifest_digest)
                 └── content                    (verified against member digest)
```

Members are verified against the **manifest's** digest and never against any
other index. Checking against an unsigned map would pass for whoever rewrote
that map to point at other content. A member failing fails the whole pallet:
partial acceptance would defeat combination signing.

Signing itself is not implemented here. The format reserves `manifest_digest`
as the quantity a signature covers, so adding it changes no layout.

---

## 3. Multi-pallet: several on one drive

A drive holds as many pallets as fit, and it normally holds more than one — an
upgrade is a *new* partition beside the running one, so any drive that has ever
been upgraded carries at least two.

- **Allocation comes out of measured free runs.** `Gpt::free_runs` walks the
  existing partitions in disk order and returns the gaps; `Gpt::allocate` takes
  the first gap big enough, 1 MiB aligned. A freed run is reused.
- **Aliasing is refused, not documented.** `Gpt::insert` rejects any range that
  touches an existing partition. Firmware does not publish a handle for an
  overlapping entry and the failure is the firmware's, so it is not portable
  even where it appears to work.
- **Both GPT copies are written on every mutation**, so a header lost to a bad
  block is recoverable from the other end of the disk. `Gpt::read` prefers the
  primary and falls back to the backup, reporting which it used.
- **Sizing.** A pallet is laid into a partition that may be larger than it
  needs; `size_bytes` sets that, and sparse costs nothing to a consumer that
  only does block reads. `used_bytes` is what is actually occupied, and it is
  what a copy transfers.

A single drive with a boot ladder and an app pallet:

```
LBA 34         GPT entries end
2048           pallet stormcos-boot v3   pri=15 tries=3           (the upgrade)
12288          pallet stormcos-boot v2   pri=14 successful=1      (the fallback)
22528          pallet kube v7            pri=1                    (not in that race)
...            slab / everything mutable
```

## 4. Multi-drive: several drives, one node

Nothing is configured. `PalletStore` is handed drives and finds what is on them
by reading each GPT and each superblock. That is the only arrangement that
survives the cases that matter — a disk moved between nodes, a pallet copied
onto a spare, an image assembled elsewhere and written whole. A configured list
would have to be right about all of them.

- A **file is a drive**: same GPT, same partitions, same code. That is what makes
  an image built on a workstation and a disk in a node the same object.
- **Identity is the partition GUID**, which is stable across a byte-for-byte
  copy — so a pallet keeps its name when it moves to another drive.
- A drive with no usable GPT is **skipped, not fatal**: a node's other drives
  are not its business.
- **Selection spans drives.** The ladder is per kind across every drive, not per
  disk.

## 5. Two libraries, on purpose

### 5.1 Read-only — `pallet::select`

The surface a boot-time consumer holds: stormuefi in firmware, an initramfs, a
recovery shell. The policy is **pure functions over plain data** — no I/O, no
device, no async, nothing borrowed from the writer — so the same rule can be
lifted into a `no_std` consumer rather than reimplemented and left to drift.

```rust
pub struct Candidate { id, kind, version, attributes, readable }

order(&[Candidate], kind)            -> Vec<Candidate>   // priority desc, then version desc
select(&[Candidate], kind)           -> Option<Candidate>
fallback_after(&[Candidate], id, kind) -> Option<Candidate>
chain(&[Candidate], kind)            -> Vec<Candidate>   // the whole fallback order
```

The rule, verbatim from the spec:

```text
candidates = pallets with priority > 0, ordered by (priority desc, version desc)
for p in candidates:
    if p.successful == 0 and p.tries_left == 0: skip
    if not verify(p): continue          # fall back
    use p
```

`PalletBrowser` feeds those from real drives and **cannot write** — there is no
mutating method on it, by construction rather than by discipline:

| Call | Answers |
|---|---|
| `list()` / `list_drive(i)` | every pallet, everywhere / on one drive |
| `select(kind)` | the one a consumer would use right now |
| `fallback_after(id, kind)` | what to use instead if that one is bad |
| `chain(kind)` | the whole order it would be walked in |
| `open(loc)` / `view(loc)` | the manifest / a byte window, no content read |
| `verify(id)` | manifest, then every member against the manifest's digest |
| `select_verified(kind)` | walk the chain, return the first that passes and why each earlier one was rejected |

A read-only consumer *decides*; it does not record. Firmware falls back by
simply trying the next one. The running system makes a decision stick with
`PalletManager::activate`.

### 5.2 Full — `pallet::PalletManager`

| Call | Does |
|---|---|
| `init_gpt(drive, force)` | write a fresh GPT so a drive can carry pallets |
| `list()` / `get(id)` / `status(kind)` | what is here; what is active, available, failed and why |
| `publish(PublishSpec)` | lay new sealed content beside what is running, verify it where it landed, optionally activate |
| `recompose(id, RecomposeSpec)` | republish as a new version with members added or dropped |
| `verify(id)` | full check in spec order, per-member verdicts |
| `activate(id)` | attribute write: target to top priority, competitors renumbered below |
| `mark_successful(id)` | confirmed good; stops spending tries |
| `rollback(kind)` | take the active one out of the running, select the next down |
| `set_read_only(id, v, force)` / `set_sealed(id, v)` | GPT attribute **and** superblock mirror |
| `copy_pallet(id, drive)` / `move_pallet(id, drive)` | between drives, verified at the destination |
| `copy_member` / `move_member` / `member_spec` | move a container, a kernel, anything, between pallets |
| `adopt_whole_drive(from, to)` | migrate a pre-subdivision whole-drive pallet |
| `convert_drive(from, to, ConvertOptions)` | a whole drive onto another: copy, verify, remove, optionally reinitialise the source (§7) |
| `delete(id, force)` / `prune(name, keep)` | remove entries, keeping N-1 |

Two invariants shape all of it:

- **Nothing in use is ever rewritten.** Publishing allocates a new partition.
  Recomposing publishes a new *version*. Fallback works by construction.
- **Activation is an attribute write.** No content moves, and there is no window
  where the node has nothing to boot.

## 6. Recipes

**Publish an upgrade and select it**

```bash
stormblock pallet --drive /dev/nvme0n1 publish \
  --name stormcos-boot --kind boot --label 6.12.0-200.fc41 \
  --member kernel:kernel:kernel:/build/vmlinuz \
  --member initramfs:initramfs:initramfs:/build/initramfs.img \
  --member cmdline:cmdline:bootconfig:/build/cmdline \
  --activate
```

The publish verifies before it returns, so a bad build fails here rather than at
the next boot.

**Confirm it after it boots, or roll back**

```bash
stormblock pallet --drive /dev/nvme0n1 successful <id>
stormblock pallet --drive /dev/nvme0n1 rollback --kind boot
```

Rollback restores nothing. It selects the previous pallet, whose content was
never overwritten.

**See what would boot, and what would happen next**

```bash
stormblock pallet --drive /dev/nvme0n1 status --kind boot
stormblock pallet --drive /dev/nvme0n1 chain  --kind boot
stormblock pallet --drive /dev/nvme0n1 verify all
```

**Move a whole pallet to another drive**

```bash
stormblock pallet --drive /dev/nvme0n1 --drive /dev/nvme1n1 move <id> --to /dev/nvme1n1
```

Copy, verify at the destination, adopt the source's identity, drop the source —
in that order, so no interruption leaves two disks claiming to be the same
pallet.

**Move one member between pallets**

```bash
stormblock pallet --drive /dev/nvme0n1 move-member <src-id> etcd --into <dst-id>
```

Both sides come back as *new versions*: the member cannot be added to a sealed
pallet in place, which is the point of sealing. The originals stay until pruned.

**Retention**

```bash
stormblock pallet --drive /dev/nvme0n1 prune stormcos-boot --keep 3
```

`keep` is floored at 2 whatever is asked — the active pallet and the one
fallback depends on. Refcounting is pallet-aware in exactly this sense: an older
pallet still pins its members.

## 7. Converting a drive

One call for the operation a drive replacement actually is: **everything on the
source becomes partitioned pallets on the destination.**

```bash
stormblock pallet --drive old.img --drive /dev/nvme1n1 \
  convert --from old.img --to /dev/nvme1n1 --reinit-source
```

It covers both shapes a source can be in, without the caller having to know
which it is looking at:

- a **whole-drive pallet** from before drives were subdivided, which cannot be
  partitioned in place because the table wants the bytes its superblock is in;
- an **already-partitioned drive** carrying several pallets — evacuation, the
  same thing you do to a disk you are about to pull.

What it guarantees:

- **Copy, verify, then remove.** Nothing leaves the source until its copy has
  been read back at the destination and checked against the manifest's digests.
- **Identities survive.** A partitioned source's pallets keep their partition
  GUIDs, so every reference to them still resolves. (A whole-drive pallet has no
  real GUID to keep — its identity is derived from the device path — so it
  arrives with a fresh one.)
- **A pallet that will not parse is skipped and reported**, never copied:
  copying it would only spread the damage.
- **`--reinit-source` is refused while anything failed**, because that is
  exactly the case where the source is still the only copy of it. Without it,
  a converted whole-drive source still holds its pallet — there is no GPT entry
  to remove — and the report says so rather than leaving you to notice.
- The destination gets a GPT written if it has none; an existing table is never
  overwritten.

`--keep-source` copies instead of moving, for seeding a second drive rather than
emptying the first.

## 8. Migrating from whole-drive pallets

The earlier arrangement put one pallet on a whole device: superblock at byte
zero, no partition table. Such a device is still **discovered**
(`PalletLocation::is_whole_drive()`), with its sealed and read-only bits read
from the superblock mirror, because a pallet nobody can find is the same as a
pallet that is gone.

It can be read, verified and its flags changed. It **cannot** take part in the
priority ladder — there is no GPT entry to carry priority, tries or the
successful bit — and it is left out when activation renumbers.

Subdividing its own drive in place is refused: the table wants the very bytes
the superblock is in. The migration is a copy.

```bash
stormblock pallet --drive old.img --drive /dev/nvme0n1 adopt --from old.img --to /dev/nvme0n1
stormblock pallet --drive old.img init-gpt old.img --force   # now it can carry many
```

`adopt` is the single-pallet primitive; `convert` (§7) is the same move with the
reinitialisation, the reporting and the refusals around it, and is what you
usually want.

## 9. REST

Base `/api/v1/pallets`. The store is rebuilt from the node's open drives on
every request rather than cached, so nothing here can disagree with the disk.

| Method | Path | |
|---|---|---|
| GET | `/` `?kind=` | list every pallet on every drive |
| GET | `/status` `?kind=` | active, available, failed with reasons |
| GET | `/chain` `?kind=` | the order a consumer would try |
| GET | `/{id}` | one pallet and its members |
| POST | `/` | publish (members from a volume, a file, or inline base64) |
| POST | `/{id}/verify` | full check, per-member verdicts |
| POST | `/{id}/activate` · `/{id}/successful` · `/rollback` | selection |
| POST | `/{id}/read-only` · `/{id}/sealed` | `{"value":bool,"force":bool}` |
| POST | `/{id}/copy` · `/{id}/move` | `{"drive":"<path or index>"}` |
| POST | `/{id}/recompose` | add/remove members as a new version |
| POST | `/{id}/members/{name}/copy` · `/move` | `{"into":"<pallet id>"}` |
| DELETE | `/{id}` `?force=` | remove the GPT entry |
| POST | `/convert` | `{"from","to","keep_source","reinit_source"}` — a whole drive onto another |
| POST | `/prune` · `/gpt` · `/adopt` | retention, init a table, migrate a whole-drive pallet |

A member sourced as `{"source":"volume","volume_id":"…"}` is read straight out
of the GEM — the golden a pallet ships is a sealed clone, published by being
read out of the engine with nothing staged in between.

## 10. What is guaranteed, and what is not yet

Guaranteed, and asserted by tests:

- The layout is pinned **by offset**, not by round-tripping through our own
  reader — a reader that runs in firmware cannot be updated in lockstep.
- The manifest digest covers the combination: different content, or the same
  members in a different order, is a different manifest.
- A torn publish leaves a partition that fails its own CRC and is reported
  unreadable, never a manifest describing content that is not there.
- A tampered upgrade is refused and the previous pallet is selected.
- Activation writes no content, on any pallet.
- A move preserves identity and verifies at the destination before the source
  goes, and a whole-drive conversion never wipes a source that is still the only
  copy of something.
- The GPT is standard: validated against `fdisk` and byte-by-byte, not only
  against this code.

Not done here:

- **Signing.** The format reserves the signed quantity; key handling is undecided.
- **Volume-level sealed / read-only attach refusal** (#51 item 2) — a pallet
  member is read-only by attribute, but the engine's volume attach path does not
  yet refuse a writable attach of a sealed volume.
- **Per-leg physical offsets** (#51 item 3) — so a read-only consumer can take
  one good RAID leg and stop, instead of reconstructing.
- Multi-extent (sparse) members: the reader handles them, the writer emits one
  extent per member.
