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
| `STORAGE_UNREADY` | drives taken that answer with no readable format (`ready=0`): formatted first, then planned again |
| `STORAGE_SMALLEST` | the drive that sets the data region's size (ready drives only) |
| `STORAGE_SAID` | the plan in words, for the console |
| `STORAGE_WIPE` | the install-config's wipe word (`storage.wipe`, #614), said as where it came from; empty without it |
| `STORAGE_ON_DRIVE_FAILURE` | `stop` (the default) or `continue`: `storage.on_drive_failure`, what a drive that cannot be laid does to the install (#625) |
| `STORAGE_RUNTIME` | `1` with `storage.runtime: true`: a fresh install over blank drives is the running node's (#623, below); empty otherwise |
| `STORAGE_STOP` | refused: no drive is taken, the install stops naming why |

It refuses by name a drive that `carries` something (another node's
install, a foreign partition table) unless `drives:` lists it, an external
drive (USB, removable, another shelf) not listed, and `system_drive:` given
with `storage:`.

## Drives with no readable format (#595)

A drive an interrupted format left behind answers on the HBA and reads
nothing: NOT READY or MEDIUM FORMAT CORRUPTED, so sd gives it no block device,
or one of 0 bytes (forge1's three SAS drives after the 2026-11-07 format). The
inventory lists every disk-type SCSI device, these too, with its WWID (VPD
0x83, which an unformatted drive still answers), as `NOT READY: no readable
format`, and `"ready": false` in `local-disk.json`. The `sg` module is loaded
so such a drive has a node (`/dev/sgN`).

The plan takes them (`ready=0`, no size), and `/init` formats them **before**
the rest of the install decides anything: they hold nothing readable. Each is
found again afterwards by its WWID (sd makes a new disk of it), and the plan
runs again with every drive's size. The kernel is asked to read each one
again, in the same boot (#611): a disk it has is rescanned (its new block
size and PI), a SCSI device sd made no disk of is bound to sd, and with no
SCSI device at all every SCSI host is scanned. That happens at once and every
10 s for up to 60 s (`STORAGE_FIND_SECS`), each time said on the console
(`storage: after 0s the kernel asked to read naa.… again: sd bound to 0:0:1:0`). This happens on an install (no node-data
set served yet) or with the wipe word; a node whose set is served leaves such
a drive alone and says so in a WARNING (#593).

## The format

Every drive taken is formatted to the block size and protection **at once**,
one `stormdrive format` each in the background, its progress on the console
prefixed with the drive's name. Every drive is formatted (owner, #595: "It
need to format all the drives") **but one the kernel already reads at the
asked format** (owner, #614: "skip the format on drives already at 4096 +
PI"): its logical block size and `protection_type` (what sd read with READ
CAPACITY 16) are the block size and PI asked, so it gets no FORMAT UNIT and
the console says `sda: already 4096 + PI type1 (READ CAPACITY 16), not
formatted again (#614)`. It is still laid afresh with the rest (what it holds
is lost to the lay, so the same go-ahead applies), read back with its
`FORMATTED:` line, and recorded as `already` in the layout report. forge1's
10 TB sda took 14 h 25 m to reach 4096 + PI type 1. `auto` is the fast
format (FFMT), and if the drive refuses it, a full FORMAT UNIT to the same
block size and PI. There is no other fallback (owner, #595: "4k with pi"):
a drive that refuses the block size or the PI fails the install loudly with
the sense data, recorded in the drive's `.fmt` and the install result, and
nothing is laid. Never 512 + PI, never 4096 without PI.

After the format, the kernel must read each drive at the block size with
`protection_type` equal to the PI asked (READ CAPACITY 16 as sd saw it); the
console says `FORMATTED: <dev> reads 4096-byte blocks, PROT_EN=1 P_TYPE 1`,
and anything else is fatal.

A format loses every byte on the drive, so it runs only:

- on a fresh install (nothing kept), or
- over drives that hold nothing at all, or
- with the wipe word: `storage.wipe` in that install's install-config
  (#614), or `rd.stormblock.assimilate=force` on its command line. An
  install the appliance asks for (the install ticket) is not the wipe word:
  it keeps the data half (#311), and on forge1 12.86 its force taken for the
  wipe word formatted a 10 TB drive it had just said it kept (#609).

### The wipe word in the install-config (#614)

The command line is the release pallet's, the same for every host that boots
the release, so it cannot carry one install's go-ahead. The install-config
can: it is per install, staged by stormcentral, and `storage:` is read from
it. The operator's tool (stormcentral's `testhost install --force`, which
already means "reinstall whatever its disk holds" and demands `--why`)
writes it for that install:

```yaml
storage:
  drives: all
  wipe: all            # or true; false, none or absent: what the drives hold is kept
  # or, saying who asked and why (shown on the console):
  # wipe: {by: "testhost install --force by <who>", why: "<why>"}
```

`/init` takes it as `STORAGE_FORCE`, exactly as `rd.stormblock.assimilate=force`:
every drive `storage:` takes is formatted and laid whatever it holds, the
plan takes drives that carry something under `drives: all`, and is made
again with the drives that have no readable format even when this node's set
is served. The console says where the word came from and names what is lost:

```
  storage: wipe asked by the install-config (testhost install --force by <who>: <why>): every drive storage: takes is formatted and laid, whatever it holds (#614)
  storage: wipe asked by the install-config (…): /dev/sda(stormblock slabs) laid afresh at 4096 + PI type1, what it holds is lost (#614)
```

Nothing else changes: an install without the word still keeps data and
STOPs as below, `rd.stormblock.assimilate=off` and recovery still lay
nothing, and a boot that is no install (#598) never reads `storage:` at all,
so a wipe word left in forge's staged config does not touch a reboot. It is
still the operator tool's job to write it for one install only: the next
install the appliance asks for with the word still there would wipe too.

A keep-data reinstall, or an ordinary boot, over a drive holding stormblock
slabs or a foreign table stops, names the drives and what they hold, and
writes nothing (STORAGE-LAYOUT.md: a layout change is a destructive verb,
never automatic).

**`storage:` is applied on an install only** (#598). forge stages the host's
install-config on every claim, so an ordinary boot (a reboot, a power cut)
has the section too. `/init` plans and lays drives only when the appliance
asked for an install (`/run/stormblock/install.json`), when the probe rules
the disk installed over, or when the boot media hands down its own
install-config. Any other boot says `storage: … this boot is no install: the
drives are booted as laid, none is selected` and boots what the install
laid: the set, the legs, or with no set (one drive, `data_raid: none`) the
plain data region, which `storage serve` names to `slab partner` beside the
set (`storage: /dev/sda3 is this node's data region, no set`). On an install,
a drive of the served set or carrying this node's plain data region is this
node's own and is never refused as "carries stormblock slabs".

**A keep-data reinstall keeps the layout it finds** (STORAGE-LAYOUT.md: "a
keep-data reinstall never relays a drive"). When the set this boot serves is
made of exactly the drives `storage:` takes, and each is already at the asked
block size, `/init` says `KEEP: … nothing is formatted or laid`, skips the
format and the lay, and the install writes its system half onto the kept
legs; the report says `"from": "reinstall"`. Anything else (a drive outside
the set, a member not taken, another block size) stops as above, naming what
differs, unless the wipe word is given; the wipe word relays even this node's
own layout.

**An install over the served set is a keep-data install, whatever the
install ticket says** (#689). The node-data set is the data half's partner,
and the probe makes it what the install is over; the set is no drive
`storage:` names, so `/init` used to say `this node is on /dev/ublkb65535
now; storage: does not take it`, forget it, find no data slab on the sd
drives, and take the install for a fresh one, which formats and lays with no
look at what the drives hold. forge1 lost its whole data half that way on
every release install from 2026-11-17 to 12.96 (a new set each time). Now
`/init` says `this node's data half is on /dev/ublkb65535, the node-data set
over …: an install keeps it`; on `KEEP` boot-local is handed the set as the
data half (`--local-disk`) and the first leg as the system drive, never
forced. And a set this boot serves is never formatted, let go or laid without
the wipe word, whatever the install's intent: a fresh install over it stops
naming the drives, the set still served.

## The lay: `image storage-lay`

After the format, every drive taken gets a fresh GPT: a boot area, a
`stormblock` system partition on each system leg, and a `stormblock-raid`
data region, every region one size on every drive (the smallest drive's).
With more than one drive the data regions become one stormraid set
(`stormraid create --role node-data --parity P --pi auto --force`), served at
`/dev/ublkb65535`; the node's data half is laid on it, and its system half on
the first leg. The id is far above any the engine numbers (#605): boot-local
numbers the release's volumes 0, 1, 2, … in the order `/init` mounts them, and
12.84's 81 volumes reached the old 64. boot-local refuses to start, naming the
device, when an id it would create is already live.

**The set's PI is said, never silent** (#647). `--pi auto` lays `type1` when
every member has PI and the kernel holds the host's integrity profile (DIX).
An mpt3sas HBA offers DIF only, by design (`prot_capabilities` 7). On one the
set is `off`, while every block is still PI-guarded from the card to the
platter: the card inserts and strips the tags, and the drive checks them. One
line says which case it is, on the boot that lays the set and on every boot
that serves it (stormraid's `serve` and `status` don't say the set's PI, so a
serving boot decides from the facts; a DIF-only host or a member without PI
can't hold a type1 set):
- `storage: node-data PI type1: every member 4096 + PI type 1 with the host's integrity profile`;
- `storage: node-data PI off: drives type1, card-inserted (DIF, no host DIX on mpt3sas); set checksums by stormraid` (forge1);
- `storage: node-data PI off: sdc (naa.…) has no PI` when a member lacks it.

The route to a type1 set is a DIX-capable HBA (stormcos#1016). Same rule as the format: only over drives this boot formatted,
with the wipe word, on a fresh install, or over drives that carry nothing.

### A drive that cannot be laid (#625)

Every drive is laid at once, and one that fails does not undo the others
(forge1 12.87: sdb, a NetApp X378 at 4096 + PI type 1, took no write at LBA
0, and three laid drives were thrown away with the shell drop). The engine
explains the drive in one block on the console, asked through SG_IO on its
own node after the write failed (an example, for a drive whose format is corrupt):

```
Storage: /dev/sdb: NOT LAID: clearing the front of the drive: I/O error (os error 5)
Storage: /dev/sdb:   asked through sdb: TEST UNIT READY good; REQUEST SENSE good
Storage: /dev/sdb:   format: 2441609216 blocks of 4096 + PI type 1; write protect: off
Storage: /dev/sdb:   READ(16) LBA 0, no PI: good; WRITE(16) LBA 0, no PI: MEDIUM ERROR asc/ascq 0x31/0x00
Storage: /dev/sdb:   so: the drive's format is corrupt (did not complete): it needs a FORMAT UNIT again (#595, #621)
Storage: /dev/sdb:   NETAPP X378_HLBRE10TA07 NA02 serial …: write uncorrected 0, read uncorrected 0, non-medium 0, grown defects 0, SMART 0x00/0x00
Storage: /dev/sdb:   kernel: sd 0:0:3:0: [sdb] tag#9 Sense Key : Medium Error [current]
```

A drive that **left the SAS link** on the write (forge1's X378 on its
second attempt: the kernel removed its device mid-lay) is said so first:
`so: the drive left the SAS link on the write: the kernel removed /dev/sda
(SAS 0x5000cca26a819e69)`. Its SAS address is read before the lay, while
sysfs still has it. The block then gives the card's own row for it (slot,
link, state, present, Linux device, or that the card no longer lists it),
asks the drive through the card by that address (the probe below works
with no Linux device), and matches the kernel's lines by the address too
(`removing handle(0x000b), sas_addr(0x5000cca26a819e69)` never names the
disk).

The write at LBA 0 is one block of zeros with WRPROTECT 0 (no protection
information sent; the drive makes its own), so it tells a drive that refuses
writes from a path that does not carry them: when it succeeds, the failure is
in what the kernel's writes carry (the PI the HBA adds) or the HBA. The
`so:` line reads the sense: a corrupt format (ASC 0x31), a format still
running, write protect or DATA PROTECT, a PI guard / application / reference
tag check, a medium or hardware error. The `kernel:` lines are the last of
`/dev/kmsg` about the drive (`[sdb]`, `dev sdb,`) and the card's `log_info`.
On every install (one `storage:` lays, or one an install ticket asks for:
a single drive, laid later by boot-local) the console loglevel is 7, so the
kernel's own sense lines are on the console too (`install: console loglevel
7 (was 4): the kernel's messages, SCSI sense included, on the console until
switch_root`). It is put back just before switch_root. An ordinary boot is
untouched.

The engine prints `STORAGE_LAID` (the drives laid), `STORAGE_LAY_FAILED` and
`STORAGE_LAY_WHY` (each failure with its `so:` line), and exits non-zero only
when no drive was laid. Each failed drive is `failed` in the install record
(#616). Then `storage.on_drive_failure` decides:

```yaml
storage:
  drives: all
  on_drive_failure: continue   # stop (the default): the install ends with why
```

- `stop` (the default): `STOP:` names the drive, the drives laid and this
  setting, then the FATAL with the drive's `so:` line.
- `continue`: the install goes on without it while the plan still holds:
  the failed drive's leg dropped (at least one leg must be left), a spare
  taking its place in the set (the spares count goes down first), and the set
  at its parity (`parity + 1` data members at least). Otherwise it stops as
  `stop` does, saying which of these failed. The console says
  `WARNING: storage: /dev/sdb left out (storage.on_drive_failure: continue)`
  and the set it makes (`2 data + 1 parity`).

## The install done by the running node (`storage.runtime`, #623)

```yaml
storage:
  drives: all
  runtime: true      # false (the default): /init formats and lays, as above
```

With `runtime: true`, `/init` lays nothing on a **fresh install** when it
runs from an image an appliance or the media gave it (`SLAB` is a URL), the
node's data half is not served from the drives, no wipe word is given, and
every drive taken holds nothing (no stormblock slab, no signature). It says
`RUNTIME INSTALL: storage.runtime …`, lets go of the set if it served one, leaves
`/run/stormblock/runtime-install` (the drives, a line), and boots the claimed
image with every local drive left alone (`ASSIMILATE=off`, said as
`DISKLESS, as storage.runtime …`). When any of these does not hold, it says
`storage.runtime: not this boot, <why>` and goes on exactly as without it.

The node's engine (`adopt-ublk`) sees the file once it is up and runs the
install once a boot (`mgmt::node_install`), with the console, the boot screen
and the install page up the whole time:

1. it reads the words `/init` kept (`install-intent.yaml`, no wipe word) and
   plans over the drives it sees now, as `/init` plans; a drive taken that
   holds anything stops it (`stage install failed`, nothing written);
2. **format**: #621's format jobs on each SAS drive not at the format, waited
   for; a drive on no card must already be at the block size;
3. the plan again over the drives as they are after the format, the same
   ones by serial;
4. **lay**: what `image storage-lay` does, every drive at once;
   `storage.on_drive_failure` as above;
5. **set**: `stormraid create --role node-data` and `serve --id 65535`, as
   `/init` makes it, so the next boot finds it;
6. **system half and legs**: the system half on the first leg and the data
   half on the set, in the live engine, the other legs' slabs opened, the
   handover record written, then the flow-over. Once it is done the disk
   boots on its own and the install is reported `laid`, as `/init`'s is.

Each step is in the install record (`format`, `lay`, `set`, `system legs`,
`flow-over`), which the NodeInstall object, the page and the screen show. An
engine restart does not start it again: a flow-over it began resumes from the
handover record. A keep-data reinstall, and an install with the wipe word,
are `/init`'s, as above (option 1 on #623). The node keeps running from the
local disk after the flow-over; it is not rebooted for you.

## Every boot: the set served

Before the probe looks for the node's halves, `/init` serves every
`stormblock-raid` partition carrying a stormraid superblock as the set at
`/dev/ublkb65535` (`stormraid serve --id 65535`). A member missing opens it
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
  "drives": [{"device": "/dev/sda", "wwid": "naa.5000cca26a819e68", "format": "formatted",
              "protection": "type1", "method": "auto"}],
  "asked": {"block_size": 4096, "protection": "type1", "parity": 1, "spares": 0},
  "regions": {"data_bytes": 0, "system_bytes": 0},
  "system_legs": ["/dev/sda", "/dev/sdb", "/dev/sdc", "/dev/sdd"],
  "set": {"device": "/dev/ublkb65535", "state": "optimal", "parity": 1, "slots": 4,
          "spares": 0, "members": ["/dev/sda3", "..."], "missing": []}
}
```

`format` is `formatted` (with the `protection` and `method` used) or
`already` (the drive was at the format). `wwid` is the drive's, as sysfs had it
at the time. The install's report is also kept in system-data as
`config/storage-layout.install.json`, and every later boot puts it back beside
its own as `/run/stormblock/storage-layout.install.json`, so the format is
not lost when the node reboots (see below). `set.state` is
`optimal`, `degraded` (a member missing: `missing` quotes stormraid's line) or
`not-served`; `"set": null` with one drive or `data_raid: none`.

The engine serves it at `GET /api/v1/storage/layout` (token; 404 when there is
none) and in health as `storage_layout`, with what it sees now laid over it.
For each drive:
- `logical_block` from the kernel (a format that did not take shows here).
- `pi_type`: the P_TYPE the drive is formatted at (sd's `protection_type`:
  0 none, 1 type 1).
- `integrity`: the kernel's DIX profile. It is `none` on an HBA that
  registers none, however the drive is formatted (forge1's HBA330 reads
  `integrity: none` with `pi_type: 1`).
- `serial`, `wwid`, `controller` and `bay`, as stormcentral#1318 checks
  drives by serial and bay. `bay` is the enclosure slot, else the mpt card's
  slot for that disk (`bay_from`: `enclosure` or `card`, #611).
- On a boot's report, `format`, `protection` and `method` from the kept
  install report, matched by WWID, then by device name (`format_from:
  install`), and its `asked` and `regions` when the boot's report has none.

Then `legs_present` / `legs_missing` (the legs with a
local system slab open; left out until the first slab report) and `state`:
`ok`, or `degraded` when the set is not whole or a leg is missing. Health's
own `status` does not follow it.

## Drives that arrive later: hot plug (#593)

Owner: "You need to watch for hot plug even after boot." No udev runs on
stormcos, so drives are watched through the kernel's own uevents
(`NETLINK_KOBJECT_UEVENT`, `src/drive/uevent.rs`): whole block disks, and the
`sg` node of a disk with no block device (a drive with no readable format,
#595). Partitions, loop, ram, ublk, nbd, dm, md and optical devices are not
drives.

**In the initramfs.** `/init` starts `stormblock hotplug watch --out
/run/stormblock/hotplug.log --say` right after the storage inventory and stops
it before switch_root. Each drive added or removed is said on the console
with its serial, controller and bay (`storage: drive added: /dev/sde (serial
…) on 0000:03:00.0, bay 4, 10000 GB`) and logged one line each:

```
add /dev/sde serial=S5 wwid=naa.… controller=0000:03:00.0 bay=4 bytes=10000831348736 ready=1 at=1794626959
```

Just before the lay writes anything (`storage arrivals`), a drive that arrived
since the inventory makes the engine plan again over every drive. A drive the
new plan takes is formatted like the rest (the same go-ahead: the wipe word,
a fresh install, or a drive that holds nothing) and laid with them:
`storage: /dev/sde taken into the layout: …`. Drives still arriving during that
format are planned in the next round, up to `STORAGE_ARRIVAL_ROUNDS` (3).
After that they are left to the running node. A new plan that stops, or one
that takes a drive holding something without the wipe word, is not used: the
layout goes on with the drives it had, and the arrival is said with why.

The install's report carries `arrived` (each with `device`, `serial`, `wwid`,
`controller`, `bay`, `at` and whether it was `laid`) and `not_laid` (drives
the inventory saw that the layout does not take), so the inventory and the
layout never differ in silence. A drive that arrived and was not laid is said
on the console as the running node's.

**On the running node.** The engine watches the same uevents for as long as
it runs (`src/mgmt/hotplug.rs`; read from sysfs every 30 s when netlink
cannot be read; `STORMBLOCK_NO_HOTPLUG` turns it off). Every add and remove
is logged at WARN with the device, serial, WWID, controller and bay, so it
reaches the console and the journal. On a node laid by `storage:`, a drive
that is not in the layout, carries no slab this engine has open, and holds
nothing (its first and last MiB zeros) or answers with no readable format is
a **candidate**, said once (`… is blank and not in this node's layout:
offered as replacement, system_leg`). Each is offered as:

- `replacement` when the node-data set misses a member (first);
- `spare` when the set is whole;
- `system_leg`, a system mirror leg.

`GET /api/v1/storage/layout` and health's `storage_layout` carry
`drive_events` (the last 64, newest last), `candidates` (device, serial,
WWID, controller, bay, bytes, `holds`: `blank` or `not_ready`, `offered`) and
`drives_watched` (`uevents` or `sysfs`). Nothing is formatted or joined by
the engine: a drive that holds anything is never a candidate, and taking one
is a person's call (#154 decision 3: no auto format).

## The card's rows (#611)

An mpt2sas/mpt3sas card is asked for its drives through its own API
(`/dev/mpt3ctl`, MPI2 config pages; `src/drive/mpt.rs`), and each row is then
matched to Linux by SAS address (`docs/boot-hooks.md`, the card's own rows).

**At install.** The rows go into `local-disk.json` as `card_rows`. A drive the
card lists and the kernel never made a SCSI device of is formatted through
the card's own SCSI pass-through, at the plan's block size and PI, when the
config says `drives: all` and the install may format (an install, or the
wipe word). The steps are MODE SELECT with the drive's own descriptor, then
FORMAT UNIT with FMTDATA, IMMED and FMTPINFO, a full format, followed by
REQUEST SENSE / TEST UNIT READY. The kernel is then asked to look again, and
the plan runs again over the drives that came up, all in one boot. With
named drives, such a drive is a WARNING and is not taken.

```
  storage: 0x5000cca268077301 listed by the card with no SCSI device - formatted through the card to 4096 + PI type1 (a full FORMAT UNIT), then found again (#611)
  storage: 0x5000cca268077301: FORMAT UNIT to 4096 with PI type 1 started (IMMED) through the card (mpt3ctl ioc 0), handle 0x000c
  storage: 0x5000cca268077301: formatting 41.3%, 95m so far
```

**After a card format, Linux is waited for (#620).** forge1 12.87's three
drives took 850 min of FORMAT UNIT, and the install gave up on them 9 s
later. Now, while the card lists a formatted drive with no SCSI device, the
drive is asked through the card every 10 s (`stormblock hba ready --sas …
--kick`). It gets TEST UNIT READY, START UNIT when it answers NOT READY
04/02, and READ CAPACITY 16. Linux is asked to find it: its host is
scanned and its phy's link reset (`/sys/class/sas_phy/phy-<host>:<phy>`).
The console gets a line a minute with what the drive answered, and the
install record a `formatting` beat. After 10 min (`STORAGE_FIND_SECS`, 600)
a drive still without a Linux device is said with the card's rows and the
kernel's lines about it, and the way out: a power cycle (the formats are
kept, so the next install formats nothing, #614). The install goes on
waiting; a finished format is never dropped to a shell.

```
  storage: 0x5000cca268077301: TEST UNIT READY good; READ CAPACITY 16 2441609216 blocks of 4096 + PI type 1; no Linux device yet; its host scanned, phy-1:6 link reset - waiting for Linux to find it (3 min) (#620)
  storage: every drive the card formatted has a Linux device again, after 4 min (#620)
```

**On a running node.** The engine reads the cards again every 30 s and on
every drive event, matches the rows to Linux again, and logs each change
at WARN: a row appearing, a slot emptied, a state, link, access or device
changing (`storage: the card's drive 0x… (slot 2): state: no target -> not
ready (#611)`). The API:

- `GET /api/v1/storage/hba`: `rows`, `read_at`, `changes` (the last 64),
  `errors`.
- `GET /api/v1/storage/hba/{sas}/details`: the drive's own answers, through
  its sg node or the card: INQUIRY, the VPD pages (serial, designators and
  WWID, rotation rate, form factor, transfer limits, thin provisioning),
  READ CAPACITY 16 (block size, PI), a format's progress, temperature, the
  informational exceptions (a SMART trip), the self-test log, start-stop
  cycles, read/write uncorrected and non-medium errors, the grown defect
  count, and the caching page. It is cached with `read_at`; `?fresh=1` reads
  it again, and an action or a change on the row drops the cache.
- `POST /api/v1/storage/hba/{sas}/action` (admin token, #274): `spin_up`,
  `spin_down`, `self_test`, `self_test_extended`, or `format` with
  `block_size` (512 or 4096), `pi` (`none` or `type1`) and `confirm` = the
  SAS address. A format is followed to its end; then the kernel is asked to
  look again (a rescan, or a delete and host scan when its disk read
  0 bytes) and the rows are read again.
- `POST /api/v1/storage/hba/{sas}/write-test` (admin token, #627) with
  `confirm` = the SAS address: the write-test matrix at the format the drive
  has. One block at LBA 0 and one at the last LBA are written with
  WRPROTECT 0 (the drive makes the PI), then WRPROTECT 1 (the host's PI:
  the T10-DIF guard, application tag 0, the LBA as reference tag). Each is
  read back with RDPROTECT 0 (the data as written?) and RDPROTECT 1 (the
  data and the PI as expected?). After each step it reads TEST UNIT READY
  and the card's link for the drive (rate, state, present). Mode pages
  0x03/0x08/0x0A/0x19/0x1A/0x1C and log pages 0x0E/0x15 are read before and
  after, and what changed is said. The run stops when the drive no longer
  answers (a transport error: the link gone). **Two blocks are
  overwritten**, and a drive holding a slab this engine has open is refused
  (409). Every line goes to the log at WARN, and `GET …/write-test` answers
  the last run. On a shell: `stormblock hba write-test --sas 0x… --confirm
  0x… [--json]`.
- `…/details` carries `mode_pages` too: each page's current values
  (`hex`), its changeable values (`changeable`: the bits MODE SELECT may set,
  #636) and what its bits say (`said`): caching WCE/RCD, control
  DPICZ/D_SENSE/QAM/QERR/SWP/ATO/TAS, the power-condition timers,
  informational exceptions.
- …and `vpd` (#636): every VPD page the drive lists in page 0x00, each read
  whole (up to 4 KiB), as `hex`. The standard ones are said: 0x00, 0x80,
  0x83, **0x86** (extended INQUIRY: ACTIVATE_MICROCODE, SPT, **GRD_CHK
  APP_CHK REF_CHK**, the task-attribute bits, WU/CRD/NV/V_SUP), 0x8F/0x90/0x91
  (named, raw), 0xB0, 0xB1, 0xB2, 0xB7 (named, raw). A vendor page (0xC0 and
  up: Sun/HGST 0xC0/0xC1, NetApp 0xD1–0xD3) is raw with its printable
  `strings`; its layout is the vendor's, so nothing in it is interpreted or
  trusted beyond display.
- `stormblock hba pages [--compare] [--json] <sas>…`: those pages from a
  shell, through sg or the card. With several drives they are side by side:
  a page the same on every drive is one line (`= on every drive` and what it
  says); one that differs gives each drive's bytes in rows of 16, the bytes
  that differ from the first drive's in brackets.
  ```
  vpd 0x86  DIFFERS
    0x5000cca26801b649 (HGST H7210A521SUN010T B3Y1): extended INQUIRY: ACTIVATE_MICROCODE 1 SPT 1 GRD_CHK 1 APP_CHK 0 REF_CHK 1 …
    0x5000cca26a819e69 (NETAPP X378_HLBRE10TA07 NA02) 0000: 00 86 00 3c [0c] …
  mode 0x0a  = on every drive: control: DPICZ 0 D_SENSE 0 QAM 0 QERR 0 SWP 0 ATO 0 TAS 0
  ```
  (an example of the shape, not forge1's answers). The Drive object carries
  the same pages through stormdrive.

### The SAS link, both sides, every minute (#642)

forge1's bay 3 "failed writes" for a week while its own phy counters said
why: log page 0x18 had loss of DWORD sync at 73 million and climbing about
500 a minute idle at 12 Gbit, the invalid DWORD and disparity counters
saturated, and the HBA's side at 0. At 6 Gbit the counter froze and every
protected write and read was good. Nobody read the counter.

- **Sampled every minute** by the card's thread: for every drive the card
  lists, the drive's side (log page 0x18 through its sg node or the card:
  each phy's negotiated rate, attached address, invalid DWORDs, running
  disparity errors, loss of DWORD sync, phy reset problems) and the host's
  side (the HBA phy facing it, `/sys/class/sas_phy/phy-<host>:<phy>`, and its
  `maximum_linkrate`). Not yet: an expander's phy (SMP REPORT PHY ERROR LOG).
- **The rate per minute** is what counts. The totals are history:
  saturated at 2^32 − 1 on an old shelf drive, and reset by a re-plug. A
  saturated or reset counter gives no rate for that pair.
- **The level:** `settling` (one sample), `ok` (nothing moving),
  `degraded` (any counter moving), `failing` (loss of sync or phy resets
  moving in two samples running). A change is logged at WARN:
  `storage: drive 0x5000cca26a819e69 (bay 3): link ok -> failing at 12
  Gbit: drive phy 0 loss of sync 500/min (#642)`.
- **A sample not read is stale** (#721): the drive's log page fails, or the
  HBA phy that faced the drive is no longer listed. Nothing measured
  changes: the last valid counters, rates and level are kept (`stale: true`,
  `checked_at`, `errors`), said once at WARN (`link not measured, failing
  kept: …`), and the next valid sample is measured against the last valid
  one. A link never read stays `settling`. A drive no HBA phy ever faced
  has an empty host side, read.
- **Where:** each row of `GET /api/v1/storage/hba` carries `link` (both
  sides' phys, their rates, the host phy and its maximum, the level and
  since when). `/metrics` has `storm_drive_link_invalid_dwords_total`,
  `storm_drive_link_disparity_errors_total`, `storm_drive_link_loss_of_sync_total`,
  `storm_drive_link_phy_resets_total` (counters) and `storm_drive_link_rate_mbps`
  (gauge), labelled `node, drive, bay, shelf, side=drive|host, phy`, and
  `storm_drive_link_level` (-1 settling, 0 ok, 1 degraded, 2 failing),
  `storm_drive_link_stale` (1 while the last sample was not read).
  Health (open, no token) has `drive_links`: each drive's level, since when,
  `stale`,
  the drive side's loss-of-sync rate and its rate in Mbit/s. The
  trend and the alert rules are ironprom's (ironprom#49), the Drive object's
  `status.link` and its condition stormdrive's, the console's view
  stormconsole's (#339).
- **The write test and the lay read it before and after.** `hba write-test`
  ends with `link (the drive's side): phy 0 at 12000 Mbit/s: loss of sync
  +300 during the test` and the host side's line. A drive the lay could not
  lay (#625) has `link (the drive's side): … during the lay` in its block,
  ahead of any reading of the PI path.
- **The link-rate policy:** `linkRate: 6` on a `storage.drives` entry (`{bay:
  3, linkRate: 6}`; 1.5, 3, 6, 12 or 22.5) holds the HBA phy facing that drive
  at that rate. `/init` sets it before anything is formatted or laid
  (`stormblock hba linkrate --dev /dev/sdb --max 6`: `maximum_linkrate`, then
  `link_reset`) and writes `/run/stormblock/link-policy`. The node's engine
  keeps that in its data directory (the state volume) and applies it again at
  every start, since a sysfs write does not persist. Until the engine starts,
  a later boot runs the link at what it negotiates. It is the owner's word
  for a marginal drive until it is replaced, never set automatically; the
  level still watches it. `--max hw` gives the phy back its hardware maximum.

### Drive error and PI-check counters, every minute (#648)

Owner: "we also need raid counters and keep track of pi rewrites? Ie bit
rot". stormraid#80 counts what the set finds and repairs. This is the
drive's own side, sampled by the card's thread beside its link, once a
minute, for every drive the card lists (not one a format job is formatting).

- **What is read:**
  - the drive's error counter log pages (only those page 0x00 lists): 0x02
    write, 0x03 read, 0x05 verify (corrected without delay, corrected with
    possible delays, total rewrites/rereads, total corrected, total
    uncorrected) and 0x06 non-medium errors;
  - 0x2F, the last informational exception (`exception`, ASC/ASCQ). The page
    holds the last one, not a count;
  - READ DEFECT DATA (12), the grown list's length (count only);
  - a SATA drive (INQUIRY vendor `ATA`): SMART READ DATA through ATA
    PASS-THROUGH (16), raw 5 (reallocated), 197 (pending), 198 (offline
    uncorrectable);
  - the kernel's sense lines for the drive (`/dev/kmsg`, matched by the
    row's H:C:T:L, only records not seen before): `Sense Key` + `Add. Sense`
    of each failed command. ASC 0x10 ASCQ 1/2/3 is a PI guard, application
    tag or reference tag check. When the sense key is ILLEGAL REQUEST it is
    the card's (mpt3sas makes an IOC EEDP error that,
    `_scsih_eedp_error_handling`): `eedp_guard`, `eedp_app_tag`,
    `eedp_ref_tag`. Otherwise the drive raised it: `pi_guard`, `pi_app_tag`,
    `pi_ref_tag`. Medium, recovered and hardware errors are counted too. The
    drive keeps no count of these itself; these count since the engine
    started (and what the kernel's buffer held then).
- **The rates:** per minute between samples, by kind; defects per day (a 24 h
  window of the grown list, and SATA's pending).
- **The level:** `settling` (one sample); `ok` (nothing that matters moving:
  corrected-on-the-fly ECC never counts); `degraded` (a retry-corrected,
  non-medium, recovered or hardware error, a medium error or PI check
  failure, a defect grown in the last day); `failing` (an uncorrected error;
  a PI check failure or medium error in 2 samples running; 10 defects grown
  in a day). A change is logged at WARN: `storage: drive 0x… (bay 3): errors
  ok -> degraded: pi_guard 1.0/min (#648)`.
- **Where:** each row of `GET /api/v1/storage/hba` carries `errors` (the
  drive's counters, the sense counts, `per_min`, `defects_per_day`, the
  level, since when, `why`). Health (open) has `drive_errors`: each drive's
  level, since when, `pi_checks`, `eedp_checks`, `uncorrected`,
  `medium_errors`, `grown_defects`, `defects_per_day`, `why`. `/metrics` has
  `storm_drive_errors_total{node,drive,bay,shelf,kind}` (counters; kinds
  `pi_guard`, `pi_app_tag`, `pi_ref_tag`, `eedp_guard`, `eedp_app_tag`,
  `eedp_ref_tag`, `medium_error`, `recovered_error`, `hardware_error`,
  `{read,write,verify}_{corrected_fast,corrected_delayed,corrected,uncorrected}`,
  `read_rereads`, `write_rewrites`, `verify_rereads`, `non_medium`; a kind the
  drive keeps no page for is left out, the sense kinds are always there),
  `storm_drive_defects{…,kind}` (gauge: `grown`, and on SATA `reallocated`,
  `pending`, `offline_uncorrectable`) and `storm_drive_errors_level` (-1
  settling, 0 ok, 1 degraded, 2 failing). The Drive object's
  `status.health` is stormdrive's (stormdrive#149), the alerts ironprom's
  (ironprom#53), the console's view stormconsole's (#339).

### A drive the layout does not take says why (#672)

server8's 2 TB SATA HDD sat laid with nothing and said nowhere. Every boot
that has a local disk now says each whole drive its layout leaves, with what
the drive is and why. Laid as the owner asked (both halves on the HDD, rule
4 in `docs/boot-hooks.md`), server8 says:

```
not taken: sda (Patriot P210 512GB, 512 GB, SATA SSD, 512-byte sectors): left for the fast tier: the node is on the rotating /dev/sdb; stormdrive's DrivePolicy enrolls this drive as a `fast` data slab, which only a volume or claim that asks for `fast` uses, and its Drive object says what it decided (#672)
not taken: sdc (Virtual Disk, 0 GB, USB): a USB drive: an install never takes one (#672)
```

Before that, laid on the SSD (24.22.0), the HDD was said as `not taken: sdb
(ST2000DM008-2UB1, 2000 GB, SATA HDD, 512e): the install lays the node on
/dev/sda; a data drive beyond them is enrolled by stormdrive's DrivePolicy
after the boot, …`.

What it is: SATA/SSD/HDD, the kernel's zoned model (`zoned host-managed`;
drive-managed SMR reads `none`, so stormdrive's ATA IDENTIFY says that one),
and 512e, 4Kn or the sector size. Why: USB, removable, no media, or a data
drive beyond the node's layout. Such a drive is stormdrive's to enroll,
through `POST /api/v1/slabs {device_path, role, tier}`, at its block size
with no format and no PI (a SATA 512e drive is taken as is). On a blade
that is the SSD beside the HDD, as `{role: "data", tier: "fast"}`
(stormdrive#156, stormcos#1039). The engine's survey (health's
`local_disk.drives`) carries `rotational`, `zoned`, `logical_block` and
`physical_block` for each drive.

### The read test, each drive against its class (#652)

forge1's three HGST 10 TB drives at 12 Gbit read at 64–103 MB/s with `dd
iflag=direct`, while the HITACHI 10 TB beside them read 238 MB/s at 6 Gbit.
All four were 7200 rpm, at 4096 + PI type 1, with clean link counters.

- **What it reads, read only, every drive at once:**
  - sequential 1 MiB and 4 MiB reads from 10 % into the drive (the same
    place on each, so the same zone), two ways:
    - the kernel's block device with O_DIRECT. This is what the OS mirror
      and the set read through: sd's DIF path, the card checking and
      stripping the PI.
    - sg `READ(16)` with `RDPROTECT 0`. The drive checks its own PI and
      sends none, and the card does no EEDP.

    The two side by side say whether the DIF read path is the cost.
  - random 4 KiB reads, one at a time, over the whole drive.
  - the mode pages 0x08 caching, 0x0A control (QAM), 0x1A power condition
    (idle and standby timers) and 0x1C informational exceptions, said, and
    log page 0x15's background scan (whether a medium scan is running, how
    many, how far).
- **The level:** each drive's best sequential rate against the best of its
  class (same rotation rate and size, rounded to the TB). `ok` is 80 % of
  that or more, `slow` is below 80 %, `very_slow` below 50 %. A drive alone
  in its class is `ok`.
- **The lines** start `read-test <sas> (<dev>, <vendor> <product> <rev>,
  <class>, link <rate>)`, a line a step, then the level: `very_slow: 103
  MB/s is 43 % of the best of its class (238 MB/s)`.
- **Where:**
  - `stormblock hba read-test [SAS…] [--dev-name /dev/sda…] [--all] [--mib
    1024] [--offset-pct 10] [--secs 10] [--out FILE] [--json]` on the node;
  - `POST /api/v1/storage/hba/read-test` (`{sas: [], bytes, offset_pct,
    random_secs}`; every present drive by default; the node token, since it
    only reads). Its lines are logged at WARN and reach the console. `GET`
    gives the last results, which are also each card row's `read_test`.
  - **/init, on an install with `storage:`:** after the link-rate policy
    and before anything is formatted, while the drives are quiet, the drives
    taken are tested at 256 MiB a pass and 5 s of random reads. The lines go
    on the console and the results to `/run/stormblock/read-test.json`,
    which the engine takes into the rows at its start.
    `rd.stormblock.readtest=off` skips it.

  It loads the drives while it runs, a minute or two at the defaults. A
  fix it points to (a mode page) is made through the Drive object
  (stormdrive#146), never by hand.

### The drive's own link rate, mode page 0x19/1 (#646)

forge1 bay 3's replacement (a HITACHI with EMC firmware) negotiated 6 Gbit
on a 12 Gbit HBA phy. The cause was the drive, not the HBA. Its phy control
and discover subpage (mode page 0x19 subpage 1) has, in each phy's
descriptor, byte 32 = programmed minimum | hardware minimum and byte 33 =
programmed maximum | hardware maximum. Its byte 33 was `0xab`: programmed
6 Gbit, hardware 12. The HBA's `maximum_linkrate` (#642) cannot raise that.

- **Read:** every minute with the link (`scsi::phy_rates`). The sample
  carries `drive_programmed_max` and `drive_hardware_max` (Mbit/s, the
  lowest of the drive's phys) in each row's `link` and in health's
  `drive_links`. A drive that programs less than its hardware is logged at
  WARN once: `storage: drive 0x… (bay 3): the drive programs 6 Gbit
  (hardware 12)`. The level's line says it too. The write test and the lay
  each say it: `link: the drive programs 6 Gbit (hardware 12): …linkRate
  raises it (#646)`.
- **Set:** `linkRate` (and `stormblock hba linkrate --max N|hw`) works on
  both sides:
  1. When the drive programs less than `N`, its programmed maximum is raised
     to `N`, capped at its hardware maximum (`hw`: the hardware maximum).
     This is MODE SELECT(10) with the page as the drive gave it, only the
     programmed maximum nibble changed on every phy, SP = 1 when the page is
     savable (current and saved). Otherwise it is current only, set again on
     every boot by the engine's kept policy. It is read back.
  2. Then the HBA phy's maximum is set, and the link is reset.
  3. The rate negotiated is said (`bay 3 (0x…): negotiated 12 Gbit after the
     reset; drive programmed 12 (hardware 12); host phy-1:2 maximum 12.0
     Gbit`).
  4. The counters on both sides are watched for `--watch` seconds (60 by
     default; the install's `/init` waits that minute) and what moved is
     said.

  A drive that programs at least the asked rate is left alone: holding a
  link lower is the HBA's maximum. A programmed maximum the drive does not
  mark changeable is refused by name, and a refused MODE SELECT is said with
  its sense. The HBA side is set either way.

### The runtime format (#621)

The owner, 2026-11-15: "the new 'format' needs to be implemented in running
system too. As we will need it for netapp shelves as well". The install's
card-path format is a job on the running node, with no reboot. It runs on
one drive (`POST /api/v1/storage/hba/{sas}/format`, `confirm` = its SAS
address) or every present drive of an enclosure (`POST
/api/v1/storage/enclosures/{id}/format`, `confirm` = the enclosure id). A
drive this engine has a slab open on is refused, or on a shelf left out and
named. Nothing formats by itself. `action: format` on `…/hba/{sas}/action`
runs the same job at the format asked, with no fallback.

Each drive, on a thread of its own (`src/mgmt/format_job.rs`):
1. **Skipped** when already at the format (#614).
2. **Formatted:** MODE SELECT, then FORMAT UNIT (IMMED), through its sg node
   or the card, followed every 30 s. The row's `format_job` carries the
   state, `pct`, `eta_secs` and `elapsed_secs`; a line a minute goes on the
   console. Twenty unread progress answers in a row end it as `failed`.
3. **Rescanned:** the kernel is asked to look again by SAS address, every
   minute up to 10 minutes, until Linux has the disk at its new size. Hotplug
   (#593) then offers it as a `replacement`, `spare` or `system_leg`.
4. **Write-tested** (#627's matrix; `test: false` skips it). A drive whose
   firmware refuses writes at the format is formatted again at the next
   format in `fallback`, and that is said. The default asks 4096 + PI type 1
   and falls back to 4096 without PI; `512` can be listed after it.
5. **`ready`** at the format it ended at, or **`failed`** with why.

**A shelf with NetApp firmware** (a row's `vendor` `NETAPP`; forge1's X378
took 4096 + PI and then refused its first write) has one drive formatted and
tested first, the **canary** (`canary` on by default for such a shelf). The
rest wait (`waiting`) and then start together, at the format the canary
ended at. A canary that fails formats nothing else.

**The rows** carry each drive's `vendor`, `product`, `revision` (the
firmware, to compare a shelf's drives), `block_size` and `pi_type`, read
each minute except while a format runs. They also carry `offered`:
`details`, `write_test`, and `format` for a drive that is NOT READY or not
at 4096 + PI type 1, unless a job runs on it. `GET /api/v1/storage/formats`
lists every job; health's `formats` lists the running and failed ones in
brief.

**Drive firmware:** `POST /api/v1/storage/hba/{sas}/firmware?confirm=<sas>`
(the body is the image) or `stormblock hba firmware --sas … --file …
--confirm …`. It runs READ BUFFER (the descriptor: the offset boundary and
buffer size), then WRITE BUFFER mode 0x0e (download with offsets, save,
defer activate) in chunks of at most 64 KiB, then mode 0x0f (activate); or
mode 0x07 (`mode=0x07`). INQUIRY's revision is read before and after. A
drive in use or being formatted is refused. Use only the drive's own vendor
image: a NetApp X378 takes NetApp's `.LOD` disk packages, and the drive
refuses an image it does not take.

The Drive CR's `format` action, its status and the console's bars are
stormdrive's and stormconsole's, reading these rows and jobs.

### NetApp-firmware drives (#627)

forge1's bay 3 is a NETAPP `X378_HLBRE10TA07` fw `NA02` (an HGST He10 with
NetApp firmware), formatted through the card to 4096 + PI type 1. It reads
back PROT_EN=1 P_TYPE 1, but its first write through the kernel's sd path
left the SAS link, twice (#625). The matrix above is what answers it, on
the running node. Its results and the policy for a drive whose INQUIRY vendor
is `NETAPP` go here once it has been run; until then nothing in the install
treats NetApp firmware differently.

On the command line, `stormblock hba view|pending|count|json|details|act`
does the same.

Not read yet: ATA SMART for SATA drives behind the card, NVMe logs, SES slot
status and the locate LED, and SANITIZE.

