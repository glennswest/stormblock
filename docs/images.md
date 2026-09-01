# Building images

**Status: implemented, 2026-08-19** (stormblock v9.5.0). Companion to
[docs/pallets.md](pallets.md), which specifies what a pallet is.

> A disk image is a GPT plus a concatenation of pallets.

That line from [docs/pallets.md](pallets.md) is the whole design. Everything inside a pallet
is addressed relative to its partition, so assembling an image is *appending
bytes and adding a GPT entry* — no offsets to rewrite, no signatures to redo.
The builder is therefore small: **an image file is a drive** to this engine, so
it opens the file and drives the ordinary `PalletManager`. Publishing into an
image is the same operation as publishing onto a disk, verified the same way,
by the same code.

```
stormblock image build --spec image.toml --out disk.qcow2
stormblock image inspect disk.img
stormblock image convert --in disk.img --out disk.iso --format iso
stormblock image formats
```

The CLI is glue: the builder is `crate::image`, a library, and **`/api/v1/images`
is the same glue over HTTP** (§8). Anything that can reach a node's management
API can build an image; nothing about this is shell-only.

---

## 1. The spec

One TOML document. Sizes are human strings (`512M`, `8G`); exactly one
partition may say `rest`, and only when the image has an explicit `size`.
Paths are relative to the spec file.

```toml
name = "stormcos"
size = "8G"

[esp]
size  = "64M"
label = "EFI"
from_dir = "build/esp"          # or from_image = "esp.img"

[[pallet]]
name          = "stormcos-boot"
kind          = "boot"
version_label = "6.12.0-200.fc41"
priority      = 15              # selection order; higher wins
members = [
  { name = "kernel",    role = "kernel",    kind = "kernel",     file = "build/vmlinuz" },
  { name = "initramfs", role = "initramfs", kind = "initramfs",  file = "build/initramfs.img" },
  { name = "cmdline",   role = "cmdline",   kind = "bootconfig", text = "root=ublk0 ro" },
]

[[pallet]]                      # the fallback, copied in byte for byte
from_image = "previous.img"
id         = "3a20096f-312c-41fd-a860-4259a34953ed"   # omit to take them all

[[partition]]                   # anything else
name = "firmware"
type = "linux"                  # esp | linux | swap | basic | slab | pallet | <GUID>
from_file = "build/fw.bin"

[data_slab]                     # identity and state — never reformatted
size = "64M"

  [[data_slab.golden]]          # the blank every `-data` volume clones
  name  = "stormcert"
  file  = "build/goldens/blank64.img"
  clone = "stormcert-data"

[slab]                          # the mutable end
size = "rest"
tier = "hot"

  [[slab.golden]]               # what the node runs
  name  = "stormpump"
  from  = "pallet:system1/stormpump"    # or file = "build/goldens/root.img"
  clone = "stormpump"           # the first clone; defaults to `name`
```

**Order on disk is the order of the sections** — ESP, pallets as listed, raw
partitions, data slab, slab — because allocation is first-fit out of measured
free runs and the builder fills them in that sequence. The data slab comes
before the system slab deliberately: the system slab is the half that says
`rest` and the half an image replaces, so putting it last means growing it
across a release does not move the partition holding the node's identity. An image is often read by
something that was told where to look, so being predictable matters.

**Block size, on a 4Kn target.** Images are written with 512-byte LBAs,
because that is what every tool and every firmware assumes of an image file
(see [docs/pallets.md](pallets.md) §2.4). stormblock discovers the LBA size on
read by probing where each candidate puts the header; **firmware does not** —
UEFI parses the GPT using the media's own block size. So a 512-LBA image
written to a 4Kn drive puts the header at byte 512, firmware looks at byte
4096, and the disk does not boot. Set `block_size = 4096` in the spec when the
target drive is 4Kn. `image build` prints which one it wrote, and says so
whenever a bootable image is built at 512.

**Sizing.** Omit the image's `size` and it is computed from the contents; every
estimate is an upper bound, because an image that turns out roomier than it
needed costs nothing on a sparse file, and one that turns out too small costs
the whole build. Declare a size that does not fit and the build is **refused**
rather than truncated.

**Importing.** `from_image` copies a pallet out of another image or a drive,
byte for byte — the manifest, the extents and (when it exists) the signature
travel unchanged. This is the shape every upgrade image has: compose the new
boot pallet, import the previous one as the fallback.

**Verification is not optional.** Every pallet is checked *inside the image*
after it lands, against the digests its own manifest records. A build whose
pallet does not verify fails; it does not warn.

## 2. What the builder writes

| Section | Type GUID | Contents |
|---|---|---|
| `[esp]` | `C12A7328-…` EFI System | FAT16 or FAT32, built from a directory, copied from an image, or left formatted and empty |
| `[[pallet]]` | `A324B90E-…` stormcos pallet | a sealed, versioned, verified pallet |
| `[[partition]]` | as named | a file, verbatim |
| `[data_slab]` | `7D3E5A91-…` stormblock data slab | the same, for identity and state — a partition no install may reformat |
| `[slab]` | `4C9A7B2E-…` stormblock slab | a formatted slab holding the goldens, a first clone of each, and its own volume metadata |

## 2a1. Two slabs: one an install replaces, one it must not (#88)

The mutable end of a node is two partitions, not one.

The **system slab** holds the goldens and the clones the node runs from.
Replacing it wholesale *is* the install: a new image lands new goldens and new
first clones, and nothing of value was in there that the image did not put
there.

The **data slab** holds what the node minted and nothing else can mint again.
Tier-0 (`/data/stormcert`) is the case that matters: the node CA private key,
the apiserver serving cert, and the **ServiceAccount token signing key**. A
re-minted signing key invalidates every ServiceAccount token in the cluster
and a re-minted CA invalidates everything it issued — and the node comes up
looking healthy, which is what makes the failure expensive.

Before the split, both sat in one slab, so anything that reformatted the slab
to replace the goldens took identity with it. Two places did: `image build`
formats unconditionally, and `boot-local --local-disk` formats whatever device
it is handed, which on a reinstall is the disk the previous install was on.

The split is a **partition boundary**, the same argument the `data1` pallet
already makes for blank templates — a hard allocation boundary rather than a
preference something can fall out of. It is recorded twice, because the two
records answer different questions:

- the **GPT type GUID** (`7D3E5A91-6C24-4B8F-A05D-2E9147BC6F38`), so a whole
  drive can be judged without opening any partition on it;
- a **role byte in the slab header**, which is what a whole-drive slab with no
  partition table has instead. A slab formatted before roles existed reads as
  `system`, which is what all of them are.

Three things follow, and all three are enforced rather than documented:

1. **Each slab carries its own `volumes.dat`.** The data slab's record of
   itself has to survive the system slab being replaced, so it cannot live in
   the system slab. A boot given both (`--slab … --slab …`) reads each slab's
   own copy and merges them; a slab with no copy of its own is the older
   single-document arrangement and still works.
2. **The role is a hard allocation boundary.** A system volume never takes a
   slot in a data slab and a data volume never takes one in a system slab —
   otherwise the split leaks one copy-on-write extent at a time, which is the
   same loss, slower. Clones inherit the role from their source.
3. **Nothing on an install path formats a data slab.** `boot-local
   --local-disk` refuses a target that carries one, asking the device rather
   than trusting the path; a flow-over never migrates a data slab's extents
   onto the system disk; `stormblock slab format` and `POST /api/v1/slabs`
   refuse unless the caller says `--role data` / `"role": "data"` in that same
   request.

`[data_slab]` takes the same fields as `[slab]`, and the same
`[[data_slab.golden]]` entries. It needs an explicit `size` — it is allocated
before the system slab, so it cannot take the `rest`, and an identity
partition is not something to size by guess. **Put the blank that `-data`
volumes clone from in the data slab**: a clone shares its golden's slots, so a
blank in the system slab would leave the clone's unwritten extents pointing at
the half an install replaces.

## 2a. The slab is where the node actually lives

An image whose slab is empty *contains* a node. An image whose slab holds the
goldens and a clone of each **is** one. The difference is one boot: a pallet is
sealed and read-only, so nothing can run out of one, and a slab that was
formatted and left empty has no volume for `root=/dev/ublkb0` to be (#62).

Each `[[slab.golden]]` does two things:

1. **Lands the golden.** Content comes from a file, or from
   `pallet:<pallet>/<member>` — a member of a pallet this image already
   carries, read through its extent map, with nothing staged in between. Zero
   runs are skipped a slab slot at a time: a filesystem image is mostly holes,
   and writing them would cost a slot per hole and lose the thin provisioning
   the slab exists to have.
2. **Takes the first clone.** The clone is a copy-on-write clone sharing every
   extent with the golden, so it costs nothing until it is written, and it is
   read-write. **The clone is what the node runs from; the golden is never
   used directly.** That is also the whole upgrade: publish `system2` beside
   `system1`, take a first clone, and the previous clone is still there
   because nothing was overwritten.

| field | default | |
|---|---|---|
| `name` | — | base name |
| `from` / `file` | — | content: a path, or `pallet:<pallet>/<member>` |
| `clone` | `name` | the clone's volume name — what `stormblock.volume=` resolves to |
| `golden_name` | `<name>.golden` | the golden's volume name |
| `size` | content, rounded up to a slot | the volume's virtual size |

Two volumes may not end up with the same name: a boot resolves a volume *by
name*, so a collision is refused at build time rather than discovered at 3am.

### volumes.dat lives in the slab

There is no filesystem in an image to keep a volume record in, and the volume
that record describes is the one that has to be exported before there is one.
So the slab carries its own: a region between the header and the slot table,
written as **two copies alternating by generation**, each with its own
checksum, so a write that did not finish leaves the previous one readable.

```
stormblock slab header (4 KiB)
volumes.dat copy A | copy B      <- meta_size, both copies
slot table
data: slots of slot_size
```

Sized automatically from the slab unless `[slab] meta_size` says otherwise;
`meta_size = "0"` formats a slab that keeps no record of itself, which is
right wherever a data directory holds one instead. A slab formatted before
this existed reads as *no region* — the offset and size live in bytes the v1
header left reserved, and every other offset in that header is already
absolute, so old slabs open unchanged and new ones open on older code.

`boot-local` reads it when no `--meta` is given, and writes back to it, so
this boots with no metadata directory anywhere:

```
root=/dev/ublkb0 rd.stormblock.slab=/dev/sda4 stormblock.volume=stormpump
```

`stormblock image inspect` prints what a slab says it holds.

## 3. FAT16 or FAT32, and why both

Firmware needs FAT — that is what the ESP is for — so the builder writes it
itself, with real VFAT long names because `loader/entries/stormcos-6.12.0.conf`
does not fit 8.3 and a boot loader that cannot read its own config is not a
floor to build on. A name that *is* 8.3 is stored plainly, with no long-name
entries at all.

The width is chosen by size, and the two do not overlap by accident:

- **FAT32** is only legally FAT32 above 65,525 clusters, which puts a floor of
  roughly **33 MiB** on the volume.
- **El Torito** counts a boot image in 512-byte sectors in a *16-bit* field,
  which puts a ceiling of **32 MiB** on an ESP an optical boot can describe in
  full.

Without FAT16 there is no ESP size that satisfies both, and every ISO would
ship a filesystem firmware could only half-see. So: **≤ 32 MiB for an ISO**,
larger and FAT32 for a disk image. An oversized ESP in an ISO warns at build
time rather than being discovered at a boot prompt.

Timestamps are fixed and directory entries are sorted, so building the same
tree twice produces the same bytes.

## 4. Formats

| Format | What it is | Sparse |
|---|---|---|
| `raw` | a plain disk image | on any filesystem with holes |
| `qcow2` | QEMU / KVM / Proxmox, v3, 64 KiB clusters | yes — all-zero clusters are skipped |
| `vhd` | Hyper-V and Azure, **fixed** (raw plus a 512-byte footer) | no, by definition |
| `vmdk` | VMware, monolithic sparse, single file | yes — all-zero grains are skipped |
| `iso` | ISO9660 + El Torito, with the partitions appended and a GPT over them | n/a |

Every one except the ISO is a conversion of the finished raw image, so there is
one builder and one layout: a qcow2 and a VHD of the same build are the same
bytes described differently. A 320 MB image with an empty slab converts to
about 10 MB of qcow2 or VMDK.

Fixed VHD rather than dynamic because Azure requires it, and because a format
whose entire content is "the raw image" cannot be wrong about it.

## 5. The ISO is the same image seen twice

```text
sector 0..15   system area — protective MBR + primary GPT (32 KiB)
sector 16      primary volume descriptor
sector 17      boot record volume descriptor (El Torito)
sector 18      terminator
sector 19      boot catalog — one UEFI entry, no emulation
sector 20/21   path tables
sector 22      root directory
…              each partition, 1 MiB aligned, as an ISO file *and* a GPT entry
end            backup GPT
```

The ISO9660 filesystem at the front is what an optical reader and firmware's El
Torito path understand. The GPT written into the 32 KiB system area describes
**the same bytes** as partitions, so writing the file to a USB stick gives a
disk whose ESP and pallets are exactly where the disk build put them. The
pallets inside an ISO verify through the ordinary pallet tooling — which is the
point of everything inside them being partition-relative:

```bash
stormblock pallet --drive disk.iso verify all
```

The boot catalog is **EFI-only on purpose**. There is no BIOS loader in these
images, so a bootable x86 entry would send a BIOS machine into nothing, and a
non-bootable placeholder is an El Torito image with no size — which `xorriso`
reports as a hidden image. One honest entry beats two, one of which is a lie.

**The slab is left out by default.** It is the mutable end of a disk and starts
empty, so carrying it turns a 35 MB image into a 320 MB one made mostly of
zeros. An installer formats one on the target disk. A live image that genuinely
ships content in its slab passes `--include-slab`.

## 6. Verifying a build

`tests/integration_image.rs` proves this code agrees with itself, which — as
the ext4 work taught us once already — proves nothing about a consumer.
**`ci-image-verify.sh` is the check that counts.** It hands the output to tools
that are not ours:

- `mtools` reads the ESP and extracts files, compared byte for byte against the
  sources — including a long-named one;
- an independent Python parser walks each container format's own metadata
  (qcow2 L1/L2 *and* its refcount blocks, the VHD footer checksum, the VMDK
  grain directory) and **rebuilds the raw image**, comparing digests;
- `xorriso` reads the ISO, its El Torito catalog and its system area;
- `fdisk` reads the GPT;
- and `stormblock pallet verify` runs against the ISO itself.

Both bugs worth having found here were found this way and could not have been
found any other: an 8.3 name mangled into `BOOTX6~1`, and an El Torito entry
whose size field had silently saturated.

## 7. Recipes

**A bootable USB/disk image for a VM host**

```bash
stormblock image build --spec image.toml --out stormcos.qcow2
```

**An installer ISO, ESP sized to stay inside El Torito**

```toml
[esp]
size = "24M"
from_dir = "build/esp"
```
```bash
stormblock image build --spec iso.toml --out stormcos.iso --format iso
```

**An upgrade image that keeps the previous boot pallet as its fallback**

```toml
[[pallet]]
name = "stormcos-boot"
kind = "boot"
version = 2
priority = 15
members = [ ... ]

[[pallet]]
from_image = "stormcos-v1.img"
```

**A node that boots itself: goldens in the slab, a clone of each**

```toml
[[pallet]]
name = "kernel1"
kind = "kernel"
priority = 15
members = [
  { name = "kernel",    role = "kernel",    kind = "kernel",     file = "build/vmlinuz" },
  { name = "initramfs", role = "initramfs", kind = "initramfs",  file = "build/initramfs.img" },
  { name = "cmdline",   role = "cmdline",   kind = "bootconfig",
    text = "root=/dev/ublkb0 rd.stormblock.slab=/dev/sda4 stormblock.volume=stormpump" },
]

[[pallet]]
name = "system1"
kind = "system"
members = [ { name = "stormpump", role = "rootfs", file = "build/stormpump.img" } ]

[slab]
size = "rest"

  [[slab.golden]]
  name = "stormpump"
  from = "pallet:system1/stormpump"

  [[slab.golden]]
  name = "stormpump-config"
  file = "build/goldens/stormpump-config.img"
```

The initramfs is a member like any other — it is what runs `boot-local`, so an
image without one boots to firmware and stops. Build it with
`scripts/build-stormblock-initramfs.sh`, which bakes in the same static
`stormblock` binary.

**Look inside anything**

```bash
stormblock image inspect stormcos.iso
stormblock pallet --drive stormcos.iso list
```

## 8. Over REST

`/api/v1/images`, served by any profile that merges `mgmt::api::router`. It
belongs to the engine for the reason [docs/layering.md](layering.md) gives:
assembling an image is *mechanism*, not a deployment choice, so a second
profile inherits it rather than forking it.

| Method | Path | |
|---|---|---|
| POST | `/api/v1/images/build` | the spec plus `out`, `format`, `keep_raw`, `include_slab` |
| POST | `/api/v1/images/convert` | `{in, out, format, include_slab}` |
| POST | `/api/v1/images/inspect` | `{path}` → the GPT and the pallets in it |
| GET | `/api/v1/images/formats` | what can be written |

The spec arrives in whichever form the caller has: `spec` as JSON (`ImageSpec`
is `Deserialize`, so it is the same document the TOML describes), `spec_toml`
as text, or `spec_path` pointing at a file on the node.

**Paths are resolved, not `chdir`-ed.** The CLI changes directory so a spec's
relative paths resolve against the spec file — correct for a process that then
exits. A daemon cannot: the working directory is process-global, so one build
would move the ground under every other request in flight. Relative paths are
resolved against `base_dir` (or the spec file's directory) and refused **by
name** when there is nothing to resolve them against, because a path that
silently resolved against wherever the daemon happens to be would build an
image out of files nobody named.

**A build holds its connection**, which is minutes for a real image. That is
the same bargain the CLI makes, and it keeps "did it verify" in the same answer
as "did it build"; a job id would separate them.

```bash
curl -sS -X POST http://node:9090/api/v1/images/build \
  -H 'Authorization: Bearer '"$TOKEN" -H 'Content-Type: application/json' \
  -d '{"spec_path":"/build/image.toml","out":"/data/images/stormcos.iso","format":"iso"}'
```

The consumer this exists for is a registry: stormblock-registry composes the
spec from content-addressed references — every member resolved to a real path —
and posts it here. See its `docs/pallets.md`.
