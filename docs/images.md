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

[slab]                          # the mutable end
size = "rest"
tier = "hot"
```

**Order on disk is the order of the sections** — ESP, pallets as listed, raw
partitions, slab — because allocation is first-fit out of measured free runs
and the builder fills them in that sequence. An image is often read by
something that was told where to look, so being predictable matters.

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
| `[slab]` | `4C9A7B2E-…` stormblock slab | a formatted slab, ready for volumes |

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

**Look inside anything**

```bash
stormblock image inspect stormcos.iso
stormblock pallet --drive stormcos.iso list
```
