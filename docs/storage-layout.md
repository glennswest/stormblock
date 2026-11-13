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

## The lay: `image storage-lay`

After the format, every drive taken gets a fresh GPT: a boot area, a
`stormblock` system partition on each system leg, and a `stormblock-raid`
data region, every region one size on every drive (the smallest drive's).
With more than one drive the data regions become one stormraid set
(`stormraid create --role node-data --parity P --force`), served at
`/dev/ublkb64`; the node's data half is laid on it, and its system half on
the first leg. Same rule as the format: only over drives this boot formatted,
with the wipe word, on a fresh install, or over drives that carry nothing.

## Every boot: the set served

Before the probe looks for the node's halves, `/init` serves every
`stormblock-raid` partition carrying a stormraid superblock as the set at
`/dev/ublkb64` (`stormraid serve --id 64`). A member missing opens it
degraded (`NODE-DATA SET DEGRADED`, the slot named); a set that does not open
is said and the boot goes on without it.

## The system half on every leg

Each of the first `system_mirror` drives carries a full copy of the system
volumes. `/init` names the legs other than the one the node boots from to
`boot-local` as `--system-leg DEV` (on the install, the rest of
`STORAGE_SYSTEM`; on every later boot, each served member's drive that
carries a system slab). `boot-local` opens a leg's slabs beside the system
half's, only when every slab on it is a system slab (anything else is said
and left alone), keeps them out of the flow-over's quarantine, and records
them in the handover record (`system_legs`). After the flow-over, and at each
start of a node whose record names legs, every system volume is set to
`mirror:N` at `drive` across the legs and resynced
(`pair::legs_up_unlocked`). A leg missing is said:
`WARNING: SYSTEM LEGS DEGRADED`, and the system runs on the legs it has.

Each leg is then made bootable like the first: its ESP and boot pallets
copied from the leg the node booted from (and from that leg's sources), right
after the first leg's own local boot on the install, and again at each start
of a node whose record names legs, so a leg replaced since gets them back
(`System legs: <drive> boots on its own`). Any leg then starts the node.

## The layout, reported (step 8)

`/init` writes `/run/stormblock/storage-layout.json` on the boot that laid the
drives (`"from": "install"`) and on every boot that serves the node-data set
(`"from": "boot"`), and keeps it in the boot record in system-data
(`storage_layout`): an install's record is its result. A node laid without
`storage:` has none.

```json
{
  "from": "install",
  "drives": [{"device": "/dev/sda", "format": "formatted", "protection": "none", "method": "auto"}],
  "asked": {"block_size": 4096, "protection": "type1", "parity": 1, "spares": 0},
  "regions": {"data_bytes": 0, "system_bytes": 0},
  "system_legs": ["/dev/sda", "/dev/sdb", "/dev/sdc", "/dev/sdd"],
  "set": {"device": "/dev/ublkb64", "state": "optimal", "parity": 1, "slots": 4,
          "spares": 0, "members": ["/dev/sda3", "..."], "missing": []}
}
```

`format` is `formatted` (with the `protection` and `method` used) or
`already` (the drive was at the format); null on a later boot. `set.state` is
`optimal`, `degraded` (a member missing: `missing` quotes stormraid's line) or
`not-served`; `"set": null` with one drive or `data_raid: none`.

The engine serves it at `GET /api/v1/storage/layout` (token; 404 when there is
none) and in health as `storage_layout`, with what it sees now laid over it:
each drive's `logical_block` and `integrity` from the kernel (a format that
did not take shows here), `legs_present` / `legs_missing` (the legs with a
local system slab open; left out until the first slab report) and `state`:
`ok`, or `degraded` when the set is not whole or a leg is missing. Health's
own `status` does not follow it.
