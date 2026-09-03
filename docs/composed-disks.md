# Composed disks — a per-node disk is a chain of goldens

**Status: implemented, 2026-09-03** (stormblock v13.4.0). Companion to
[docs/pallets.md](pallets.md), which specifies what a pallet is, and
[docs/images.md](images.md), which builds disk *images* out of pallets.

> A disk image is a GPT plus a concatenation of pallets.

That line from the pallet spec was true and expensive: `image build` lays
every pallet down as **bytes**, per image, so a fleet of a hundred nodes is a
hundred copies of the same goldens — a stormcos image is 8.7 GB of pallets and
2.3 GB of slab for about 2.4 GB of distinct content. `POST /api/v1/volumes/compose`
(v13.3) showed the alternative — a volume that is a *list of* goldens sharing
their slots — but a raw composition has no partition table and nothing in it
knows it is on a disk. It is not something a node can boot.

A **composed disk** is the same idea carried all the way down. Everything on
a disk becomes a golden, and a disk is a chain of them:

```text
slot 0        1..            ..            ..              last
┌──────────┬─────────────┬──────────────┬───────────────┬──────────┐
│ GPT head │ ESP         │ boot pallet  │ system pallet │ GPT tail │
│ (golden) │ (golden)    │ (pallet vol) │ (pallet vol)  │ (golden) │
└──────────┴─────────────┴──────────────┴───────────────┴──────────┘
  shared     shared        shared         shared          shared
```

Nothing is written. What a disk costs is its extent map, and copy-on-write
covers the rest exactly as it does for a clone: a node whose boot ladder
decrements `tries`, or whose filesystem writes a journal, gets its own slot for
what it changed and keeps sharing everything else.

## 1. A pallet is a sealed volume

`POST /api/v1/volumes/compose/pallet` builds a pallet the ordinary way — the
same `PalletBuilder`, the same superblock, member table and extent table — with
one change in *placement*: every member's content starts on a **slab slot
boundary** (`PalletBuilder::content_align`). That is what lets a member that is
already a golden be **shared in by its extent map** rather than copied: an
extent map can only express slot-aligned offsets, and a member on one is a
`gather_into` away.

```json
POST /api/v1/volumes/compose/pallet
{
  "name": "boot-v1",
  "pallet": "boot",
  "kind": "boot",
  "version_label": "6.12.0-200.fc41",
  "members": [
    {"name": "kernel",    "role": "kernel",    "kind": "kernel",     "volume": "kernel.golden", "len": "17807720"},
    {"name": "initramfs", "role": "initramfs", "kind": "initramfs",  "volume": "initrd.golden"},
    {"name": "cmdline",   "role": "cmdline",   "kind": "bootconfig", "text": "root=PARTUUID=… ro"}
  ]
}
```

A member is either a **`volume`** — by id or name, usually a sealed golden,
shared — or **`text`** — written, in its own slot. The response is the volume
plus a `pallet` object saying what each member cost:

```json
{
  "id": "…", "name": "boot-v1", "sealed": true, "fs": {"kind": "pallet", "label": "boot", …},
  "pallet": {
    "pallet": "boot", "version": 1, "kind": "boot", "lba": 4096,
    "manifest_digest": "…",
    "members": [
      {"name": "kernel",    "offset": 1048576,  "len": 17807720, "span": 18874368, "shared": true,  "digest": "…"},
      {"name": "initramfs", "offset": 19922944, "len": 48398118, "span": 50331648, "shared": true,  "digest": "…"},
      {"name": "cmdline",   "offset": 70254592, "len": 18,       "span": 1048576,  "shared": false, "digest": "…"}
    ],
    "shared_bytes": 69206016,
    "written_bytes": 1052672
  }
}
```

Three things worth knowing:

- **`len` and span are different numbers.** `len` is what the manifest
  digests — give the file's length for a kernel imported from a 15.3 MB file
  into a 16 MiB golden, and the member digest is `sha256sum` of the file. The
  *span* is the golden's whole size rounded up to a slot, because sharing a
  golden brings every slot it owns, and the next member has to start after all
  of them (`MemberSpec::reserve`). A `len` left out is the golden's size, and
  a kernel or cpio with trailing zeros is still a kernel or a cpio.
- **The version follows.** Leave `version` out and it is one past the highest
  version any sealed pallet volume of that *pallet name* carries — read off
  each one's superblock, so there is no bookkeeping to get wrong. Cutting
  version 2 is the same call with a different kernel.
- **It is verified before it is sealed**, through `Pallet::read` and
  `verify_all` over the volume — the reader every consumer uses. A map that put
  a member one slot off would digest wrong here and nowhere sooner; a pallet
  that does not verify is deleted and the error returned. The sealed volume's
  `fs.kind` is `pallet`, with the pallet name as its label, which is how a disk
  can be composed out of it by name.

## 2. The GPT is two goldens

A GPT is a protective MBR, a primary header and an entry array at the front,
and the array and a backup header at the back. For a given **layout** — LBA
size, disk size, and the ordered partitions with their types, sizes, names and
attributes — those bytes are the same for every disk. So they are goldens:

- `gpt-<layout digest>.head.golden` — the first slot of the disk;
- `gpt-<layout digest>.tail.golden` — the last slot, with the backup header on
  the disk's last LBA.

Minted once when a layout is first seen, found by name after that, and never
written again. **Identity is a function of the layout too**: the disk GUID
and every partition GUID are derived from its digest, so every node composed
from one layout has the same `PARTUUID`s — which is what lets one kernel
command line say `root=PARTUUID=…` for the whole fleet.

A disk that must be unique to a host — two of them on one machine, say —
says `"fresh_guid": true`, and is stamped its own disk GUID. That costs the two
GPT slots, because the stamp lands on the disk's copy-on-write slots; the
shared goldens keep the layout's GUID.

## 3. A disk is `compose(head, partitions…, tail)`

```json
POST /api/v1/volumes/compose/disk
{
  "name": "node1.disk",
  "partitions": [
    {"volume": "esp.golden", "name": "EFI", "type": "esp"},
    {"volume": "boot-v1",    "priority": 5},
    {"volume": "system-v3"}
  ]
}
```

Each partition is a volume: a composed pallet, an ESP imported as a golden,
any sealed volume. Its span is the volume's size rounded up to a slot; they
are laid out in order after the head slot. `type` defaults to what the volume
*is* — `pallet` for a pallet volume, `linux` otherwise; say `esp` for an ESP.
For a pallet partition `priority` and `tries` become the GPT attribute bits
the boot ladder reads, `sealed`/`read-only`/`required` are set, and the
superblock's own flags already agree.

```json
{
  "id": "…", "name": "node1.disk", "fs": {"kind": "gpt", "uuid": "…", "features": "lba=4096"},
  "allocated_bytes": 0,
  "disk": {
    "lba": 4096, "size_bytes": 1073741824, "disk_guid": "…", "layout": "5b1c…",
    "head_golden": "…", "tail_golden": "…", "gpt_minted": true,
    "partitions": [
      {"index": 0, "name": "EFI",     "type_guid": "c12a7328-…", "start_bytes": 1048576,  "size_bytes": 16777216, "volume": "…", "partuuid": "…", "attributes": 1},
      {"index": 1, "name": "boot-v1", "type_guid": "a324b90e-…", "start_bytes": 17825792, "size_bytes": 71303168, "volume": "…", "partuuid": "…", "attributes": 1407374883553281}
    ],
    "shared_bytes": 90177536,
    "written_bytes": 0
  }
}
```

`written_bytes` is zero. The second node of the same layout reports
`gpt_minted: false`, the same `partuuid`s, and allocates nothing. The result
is read back through the map — `Gpt::read` finds both headers, each pallet
partition's manifest checks — before it is returned; the disk is recorded as
`fs.kind = gpt` with its GUID, and is left *unsealed*: it is a node's disk and
the node writes to it.

**The LBA size defaults to 4096.** A volume is presented at 4096-byte blocks
over NVMe/TCP and ublk (`ThinVolumeHandle::block_size`), and firmware parses
a GPT in the media's own block size — a 512-LBA table on a 4Kn namespace is
one this engine can read and the node cannot boot
([docs/pallets.md §2.4](pallets.md#24-block-sizes--the-one-that-bites)). A
pallet's extent table counts in the same unit, so `compose/pallet` takes the
same `lba`. A disk meant to be copied onto a 512-byte drive says `"lba": 512`
on both.

## 4. Cutting a new version

This is what the design is for. A new kernel:

1. `POST /api/v1/volumes/import {"name": "kernel-6.13.golden", "file": …}` —
   the only bytes written are the kernel's.
2. `POST /api/v1/volumes/compose/pallet` with the new kernel and the *same*
   initramfs golden — version 2 follows; one header slot is written; the
   initramfs is shared again.
3. `POST /api/v1/volumes/compose/disk` per node, with the new pallet and the
   same ESP and system pallet — nothing is written. If the pallet's size
   changed the layout, a new pair of GPT goldens is minted once, two slots.

The previous disks are untouched and still boot; the previous pallet is still
there for the ladder to fall back to. Demotion works on the same sharing:
`POST /api/v1/volumes/{id}/tier` on last month's disk copies what this month's
still uses and moves what it does not (see the retier report's `copied`, which
is the count that becomes non-zero once disks share their goldens).

## 5. What is checked, and by what

`src/volume/disk.rs` and `tests/integration_compose_disk.rs` prove the engine
agrees with itself: offsets, sharing, allocation counts, the version, the
read-back. As the ext4 and image work taught, that proves nothing about a
consumer, so **`ci-compose-disk-verify.sh` is the check that counts.** On
dev.g8.lo it runs the built binary, imports a real kernel, initramfs and an ESP
carrying shim and grub, composes the pallet and the disk, attaches the disk as
a ublk block device, and hands it to tools that are not ours:

- `fdisk` reads a GPT at 4096-byte sectors and finds both partitions;
- `blkid` calls the ESP vfat and the kernel mounts it;
- the kernel bytes read off the block device at the member's offset digest to
  `sha256sum` of the file;
- `stormblock pallet --drive /dev/ublkbN verify all` passes;
- QEMU with OVMF boots it as an NVMe namespace with 4096-byte LBAs: firmware
  finds the ESP, shim loads grub, grub reads the config on the ESP and lists
  `(hd0,gpt1)` and `(hd0,gpt2)` on the serial console.

## 6. Not done

- **No CLI subcommand.** Both operations are REST and library only.
- **`image build` still lays pallets as bytes.** An image spec could be
  pointed at composed pallets — that is the natural next step, and it would
  make an image file and a composed disk the same layout from the same code.
- **An ESP is imported as a golden**, built elsewhere (`mkfs.vfat` +
  `mcopy`, or `image build`'s FAT writer through the library). Building one
  from a directory over HTTP is not there.
- **No node has yet booted a full stormcos from a composed disk over
  NVMe/TCP.** The OVMF stage above proves firmware and a boot loader read the
  disk; it does not run a stormcos kernel to a shell.
- **Nested slabs are not shared.** A `[slab]` inside a composed disk would be
  a slab written into a volume — copied bytes — so a network-booted node's
  mutable state should be its own volumes on the storage node, not a slab in
  its disk.
