# The storage layout (#573)

stormcos's STORAGE-LAYOUT.md (stormcos#902) asks that an install lay **every
selected drive alike**: an ESP and the boot pallets on each, the system region
mirrored on all of them, and the data slab on a stormraid set over their data
regions. stormblock's `/init` and engine do it **only when the claim's
install-config has a `storage:` section** (option A, the master on #573).
Without one, the rules of #478, #350 and #402 apply unchanged.

This file grows with the work. The plan is the `## #573` section of
[work-plan.md](work-plan.md).

## The tools in the image

The layout uses two tools from other repos:

- `stormraid` creates and assembles the node's data set (`--role node-data`,
  stormraid's `docs/node-data.md`).
- `stormdrive` formats the drives (`stormdrive format`, stormdrive#136).

The image carries them only when the build is given them:

```bash
STORMBLOCK_INITRAMFS_TOOLS="stormraid=/path/stormraid stormdrive=/path/stormdrive" \
  ./scripts/build-stormblock-initramfs.sh <stormblock> <kver> <out>
```

Each lands at `/usr/sbin/<name>`. The build refuses a name other than these
two, a file that is not an ELF, a binary for another machine than the image's
stormblock, and a dynamically linked one (there is no loader in the
initramfs). stormcentral's `component initramfs` passes them
(stormcentral#1335). An image built without them boots as before, and an
install whose config has `storage:` stops and says the tools are missing.

## The plan: `image storage-plan`

`/init` hands the engine the install-config and one `--drive` per drive it
sees (`dev`, `serial`, `wwn`, `model`, `shelf`, `position`, `bay`, `bytes`,
`removable`, `usb`, `carries`, `running`). The engine reads only `storage:`
from the file and prints shell `/init` evaluates:

| line | means |
|---|---|
| `STORAGE_NONE=1` | the config has no `storage:`; nothing changes |
| `STORAGE_DRIVES`, `STORAGE_SYSTEM` | the drives taken, and those with a system leg |
| `STORAGE_BLOCK_SIZE`, `STORAGE_PROTECTION`, `STORAGE_METHOD` | the format |
| `STORAGE_RAID`, `STORAGE_PARITY`, `STORAGE_SPARES` | the data set |
| `STORAGE_SMALLEST` | the drive that sets the data region's size |
| `STORAGE_SAID` | the plan in words, for the console |
| `STORAGE_STOP` | refused: no drive is taken, the install stops naming why |

It refuses by name a drive that `carries` something (another node's
install, a foreign partition table) unless `drives:` lists it, an external
drive (USB, removable, another shelf) not listed, and `system_drive:` given
with `storage:`.

## The format

Every drive taken is formatted to the block size and protection **at once**,
one `stormdrive format` each in the background, its progress on the console
prefixed with the drive's name. A drive already at the format (`--method
none` exits 4) is left. A drive or HBA that refuses PI type 1 (exit 3) is
formatted again with `none`, and that is said. Any other failure is fatal: the
install result says `failed` and nothing is laid.

A format loses every byte on the drive, so it runs only:

- on a fresh install (nothing kept), or
- over drives that hold nothing at all, or
- with the wipe word: `rd.stormblock.assimilate=force` on that install's
  command line (or stormcentral's fresh-slab install).

A keep-data reinstall, or an ordinary boot, over a drive holding stormblock
slabs or a foreign table stops, names the drives and what they hold, and
writes nothing (STORAGE-LAYOUT.md: a layout change is a destructive verb,
never automatic).

Until the per-drive layout lands (increments C–H in the work plan), the
system and data are laid on the first drive as before, and the other drives
are left formatted and blank.
