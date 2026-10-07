# Boot hooks — letting something else decide where a node boots from

The initramfs `/init` this repo generates decides where a node boots from. Its
own answer is deliberately narrow: it asks whether **the one device the kernel
command line names** is a slab holding the volume the command line asks for.

That is the right question for the image the command line belongs to, and the
wrong one for a machine. A hook can answer the wider one (#109).

## The mechanism

Before the local-slab probe, `/init` runs every executable in
`/etc/stormblock/boot.d`, in name order, and then `/sbin/zeroboot` if it is
there. The first hook that returns a decision wins; the rest are not run. With
no hook installed, `/init` behaves exactly as it did before hooks existed —
the probe decides, and the appliance is the fallback.

Each hook is invoked as:

```sh
/etc/stormblock/boot.d/50-something boot
```

## The contract

stdout is `KEY='value'` lines. stderr and `/dev/kmsg` are the hook's own voice
and go straight to the console, which is where its progress belongs.

| exit | meaning | keys |
|---|---|---|
| 0 | `ZB_ACTION=boot-local` — boot from this slab | `ZB_SLAB` (required), `ZB_SLAB_ID`, `ZB_VOLUME`, `ZB_DRIVE`, `ZB_TAKEABLE` |
| 2 | `ZB_ACTION=ask-appliance` — do not boot locally | `ZB_REASON`, `ZB_TAKEABLE` |
| 1 | `ZB_ACTION=error` — the hook could not tell | `ZB_REASON` |

The `ZB_` prefix is the contract as `zeroboot` already emits it; the mechanism
is not zeroboot's, and anything can drop a hook in.

- `ZB_SLAB` may be a device path or a fabric URI (`nvme-tcp://…`). A path that
  is not on this machine is not believed — see below.
- `ZB_VOLUME` names the boot volume, and is used only when the command line
  did not name one: the hook answers *where*, an operator answers *which*.
- `ZB_TAKEABLE` names a drive this node may assimilate onto — see below. It
  travels with either decision, and in practice with `ask-appliance`: a node
  with nothing of its own boots from the appliance and takes a blank drive on
  the way, which is one boot rather than two.
- Anything else the hook prints is ignored.

## The drive to assimilate onto

`boot-local --local-disk <drive>` is the flow-over: it lays a data slab and a
system slab on a local drive and migrates the node's writes onto them in the
background, after root is up. `/init` picks that drive today from
`rd.stormblock.assimilate=`:

| policy | takes |
|---|---|
| `any` (default) | a **blank** drive (first and last MiB all zeros: no partition table, no stormraid/md/LVM/ZFS/filesystem signature, #273); also this node's own layout (updated in place, below) and a drive carrying only system slabs, which holds no identity (#118) |
| `blank` | a drive with no slab and no partition table |
| `off` | nothing |
| `force` | a drive even when it is one of ours, destroying the identity on it; a drive with a foreign signature only when it is the named one (below) |

Whatever the policy, three rules decide which drives are candidates at all
(#273: a NetApp shelf on the Dell, for stormraid):

1. **The named drive only.** When the drive `rd.stormblock.slab=` names (its
   disk, for a partition) is on this machine, it is the only drive the survey
   or an install may take; every other drive is left alone and the console
   says so. A machine without it (NVMe-only, while the line names
   `/dev/sda`) falls back to the scan under the next two rules.
2. **Never a shelf.** A drive behind a SAS expander, or attached to an SES
   enclosure (`/sys/block/X/device/enclosure_device:*`), is never taken by
   the scan, and the install's search for this node's data slab skips it.
   `rd.stormblock.allow-external=1` allows them, for a server whose own bays
   sit behind an expander or a SES backplane.
3. **Somebody else's is not blank.** A drive that is not a stormblock slab
   is taken only when its first and last MiB are zeros; a stormraid
   superblock is named on the console. `force` may still clear the named
   drive whatever it carries (an abandoned partition table, #236), never
   another.

An install the appliance asked for (the host's boot intent is `install`, #148;
`boot-claim` leaves `/run/stormblock/install.json`) makes the policy `force`,
whatever the cmdline named, except `off`, which still means no. See
`docs/auth.md` "Boot intent".

### An install without an intent (#236, a stopgap)

An appliance older than v20 (forge on 13.7, #235) serves no boot intent, and
stormbootx then claims on every boot, so "this kernel came over the network"
is true of a reboot as much as an install. Until intents can be used:

- **The local-slab probe asks about the release.** A local disk that can boot
  the node is no longer booted on sight when a boothost is known: `/init`
  claims the machine's image and asks `stormblock slab holds <disk> <image>`
  whether the disk already holds every golden (sealed volume, by id) of it,
  with the install finished. Held is a reboot: the disk boots, its data kept.
  The same release with its flow-over cut short (records that still place
  extents on a slab not on the disk; exit 3) boots the disk too: the engine
  finishes the move from the clone just claimed and compared
  (`STORMBLOCK_RESUME_SOURCE`), never a second claim, and claims as the name
  resolved here (`STORMBLOCK_BOOT_TAG`), never the SMBIOS serial worked out
  again (#171, #258, #259). Not held is an **install**:
  the claimed image boots and the disk is installed over. An install the
  appliance asked for (the ticket) installs whatever the disk holds. No image,
  or "cannot say" (exit 2), boots the disk as before.
- **An install keeps the data half; the same release = recovery** (#311;
  owner, 2026-10-06: "How can you wipe a production node data? what if it has
  100's or 1000's of drives and many more volumes?", superseding #261's
  install = wipe). Whenever a boot runs from the image it claimed and a local
  drive carries this node's data slab, `/init` asks `slab holds <disk>
  <claimed>`, with or without a boot intent:
  - exit 1, another release (or the probe already ruled the bootable disk an
    install, or the appliance asked for an install): **INSTALL** — the system
    half of that disk is laid again and its data half is kept, never
    `--local-disk-force`d. `boot-local` adopts the data half before it resolves
    what it mounts (`image::install`): every volume on it keeps its id and its
    name, except where the release says otherwise in its
    `/etc/stormblock/data-volumes` (#122): `keep` (the default) keeps the
    node's and drops the claim's fresh clone; `replace` and `migrate` set the
    node's aside as `<name>@<old release>` and take the release's (a
    migration is listed for stormupdate); a golden the release ships takes
    the name and the node's is set aside (its clones still name it by id). A
    data volume the release adds comes with it (stormcos#236's
    `kubelet-data`), moving onto the disk in the background. No other drive
    is read, formatted or wiped.
  - exit 0, the same release, or 3, the same release cut short (a power cut
    during the flow-over, #258/#259): **RECOVERY** — the disk is kept and its
    data half is not touched.
  - exit 2, cannot say: **LEFT ALONE** — every local drive is left alone and
    the boot runs from the appliance.
  **A failure never falls back to a wipe.** Before anything is written the
  engine checks that the data half's records read, that no data volume has an
  extent in the system half, and that the system half holds no unsealed
  volume the release does not bring back (one the node made there, which is
  application data); any of these stops the install, says why, and the node
  runs from the appliance with the disk untouched. A lone data slab is never
  forced either. With no data slab on any drive and no intent stated, a drive
  is laid fresh (forced): there is nothing to keep.
- **With no appliance there is no release check, and it is said (#294).**
  server8 installed 11.82 over an older disk and came up on nothing:
  - Forge missed the one 3-second health check made right after its mlx4
    link came up, so no boothost was known.
  - Both the release check and the probe were guarded by a known boothost,
    so both were skipped without a word.
  - `boot-local` then died on `volume 'kubelet-data' not found`. That error
    scrolled off above `FATAL: root device /dev/ublkb0 not found`.

  What `/init` does now:
  - **A named boothost is waited for.** A boothost the network names
    (`/run/stormblock-boothost`, from the DHCP root path) is asked again for
    up to `STORM_BOOTHOST_WAIT` seconds (90; on the kernel command line it
    reaches `/init` as an environment variable). With no named boothost, the
    candidates are asked once as before. Why there is none is kept and said
    wherever it matters.
  - **The local-slab probe runs with or without an appliance.** A disk it
    would have sent to the appliance — not a slab, no root volume, missing a
    volume the release's mount list names — stops the boot when there is no
    appliance. The message is a FATAL naming what is missing and the release
    on the disk (its root volume's `/etc/os-release`), and the disk is never
    handed to `boot-local`.
  - **A disk that can boot, with no appliance, boots.** The console says
    `RELEASE CHECK SKIPPED` and why: if this boot was meant to install
    another release, it has not.
  - **The engine's output goes to `/run/stormblock/engine.log`**, followed
    onto the console. Its last 25 lines are repeated after
    `FATAL: root device … not found`.
- `rd.stormblock.assimilate=off` still means no, to both.
- Not covered: reinstalling the **same** release fresh. Assign another
  release first, or use the `install` intent once the appliance serves them.

Once the appliance states intents, the marker is not written and the intent
decides (`install` = fresh; keeping data is #234's `upgrade`).

### A local root that does not come up (#244)

A disk that holds the release, by volume id, boots — and its root can still
fail: server1 on 11.56 got `erofs: cannot find valid erofs superblock` and
stopped at a shell, with the release one claim away. Now, **once per boot**,
when `/dev/ublkb0` never appears or the root will not mount, `/init` falls
back. It does so only when all of these hold:
- the root came from a local disk, not from a hook's decision or a claimed
  image;
- an appliance is known;
- the machine's name is not a guess (#249).

The fallback:
- says `ROOT FAILED: …` and `FALLING BACK: …` on every console;
- stops the engine (SIGTERM, which tears its ublk devices down, then
  SIGKILL after 15 s) and waits for the root device to go;
- boots the image claimed for this machine, claiming one if this boot has
  none yet;
- installs it over the disk (`INSTALL_OVER` = the disk, not the partition)
  the way a release the disk does not hold is installed, with
  `STORMBLOCK_RELAY_SYSTEM_HALF=1`. The engine then lays the system half
  again even though the ids say it is up to date, and adopts the data half
  as in any install (#311).

A volume the node and the claimed image share by id (the same release) keeps
the node's bytes; the claim's copy is dropped. A second failure, or a boot
that does not qualify, stops at the shell as before, and says why.

### Whose image: the name the firmware claimed on (#249)

`/init` claims as the machine **stormbootx claimed as**, not as whatever it
works out for itself. stormbootx names the machine (DHCP and reverse DNS on a
chassis whose blades share one SMBIOS serial, the engine's own host name when
the claim reply gives one) and, before it starts the loader, sets two
**volatile** EFI variables (attributes `BOOTSERVICE_ACCESS | RUNTIME_ACCESS`,
`0x6`, never non-volatile) under vendor GUID
`ab361f54-0166-44a4-a088-1ac22e98ab76`:

| variable | value (ASCII, no NUL) |
|---|---|
| `StormBootTag` | the name it claimed `boothost/<name>` on |
| `StormBootHostNqn` | the host NQN it attached as |
| `StormBootInstallConfig` | the boot media's `install-config.yaml`: `v1:<length>:<chunks>:<sha256>`, the bytes in `StormBootInstallConfig0..N-1` (768 each). Used only when length and digest match, every variable deleted once read, written to `/state/config/install-config.yaml` (0600) only when `/state` has none (#275, stormbootx#79) |

Linux reads them from efivarfs (`/sys/firmware/efi/efivars/<Name>-<guid>`,
four attribute bytes then the value; `/init` mounts efivarfs if nothing has).
A value outside `[A-Za-z0-9._:-]` is ignored. Nothing about the pallet's
command line or stormuefi changes.

Order: the firmware's variable, then `rd.stormblock.tag=`, then the SMBIOS
serial, then the SMBIOS UUID. A cmdline tag (or `rd.stormblock.hostnqn=`)
that differs from the firmware's is reported on the console and the
firmware's is used; so is an SMBIOS serial that differs.

**A guessed name never installs over a disk.** A name read from SMBIOS
because nothing handed one down may be another machine's — eight MicroCloud
blades report one chassis serial, and server8 booted 11.58 from stormbootx and
then laid its disk from server1's old synonym, 11.50. Under a guess:

- the local-slab probe boots the local disk instead of installing a release it
  does not hold, ticket or not;
- the survey takes only a **blank** drive (`blank`): no `force` from #236's
  no-intent marker or from an install ticket, and no drive carrying a slab;
- the guessed image still boots when there is nothing local to boot — there is
  nothing better — and the next boot under the right name installs.

`rd.stormblock.trust-smbios=1` lifts this for an image whose machines are
named by serial and booted without a loader that hands a name down.

**The default is to take one, because this image is an installer.** It was
`off`, which made the common case — one drive, netbooted to be installed — do
nothing and keep every write on the appliance until somebody knew to add a
kernel parameter. Nobody netboots an installer at a machine whose disk they
mean to keep; a node that must not touch its drive says `off`.

A drive carrying somebody's ext4, or a previous life's partition table, is not
a reason to stop either. Garbage cannot be interpreted safely, so
`lay_node_slabs` destroys the ends of the drive before it lays its table: a
fresh GPT leaves everything the old one described exactly where it was — an
ext4 backup superblock, an LVM label, an mdraid superblock at the tail, a
stale *backup* GPT whose header sits at a different offset because the old
table used a different LBA size — and each of those is read by something that
scans rather than asks. The alternative to installing over it is a setup API
and a remote UI to drive it, which is a great deal of machinery to decide
something the boot already decided.

A drive carrying one of *our* slabs is a different question, because what is
on it is not garbage. Three cases, and they get three answers:

| the drive | what happens |
|---|---|
| this node's own layout — a data half and a system half | **kept** when it holds the release claimed (recovery: the system half is re-laid only if it cannot boot on its own, the data half is opened and kept); when it holds another release, its **system half is laid again and its data half adopted** (#311: an install never wipes data). |
| the same, already holding what this boot carries, and able to boot on its own | **nothing at all** — except after a root that would not come up (#244, `STORMBLOCK_RELAY_SYSTEM_HALF`), when the system half is laid again. The system half's own record is read offline and compared by volume id; if it already holds everything this boot would copy, it is left exactly as it is and the node boots from it. An update that has nothing to update must not reformat a working half and re-copy the same bytes. |
| a lone data slab | refused. That is an install abandoned part-way, and it is indistinguishable from a live node's identity. `/init` never forces it (#311); `--local-disk-force` by hand is the deliberate act for a drive whose identity is spent. |
| a lone system slab | **taken** under `any` and `force` (no identity lives on it; fresh slabs are laid over it), left alone under `blank`. |

The data half holds this node's CA key and its ServiceAccount signing key, and
nothing can mint those again — which is why it is opened rather than assumed:
a data partition that will not open as a data slab stops the flow-over instead
of being guessed at.

Those are *fleet* statements, applied by a scan that can only ask `slab list`
whether a drive is one of ours. That is the right question for a policy and a
weak one for a drive: a foreign ext4, or the four partitions a second-hand
server carries from a previous life, answers "not a slab" and is taken. An
operator typing `--local-disk /dev/sda` has looked at the drive — which is the
premise that makes this safe, and it is gone the moment the path is chosen by
something other than a person.

A hook closes that gap by naming a drive it has actually examined. `zeroboot`
offers one only when it read the whole thing back as zero — no partition
table, no filesystem signature, no slab, nothing over the network, nothing
removable — which is strictly stronger than "carries no data slab", so nothing
a hook offers can trip `boot-local`'s own guard, and that guard stays the last
word.

Precedence, most specific first:

1. `rd.stormblock.assimilate=off` — an operator saying no, and it means no.
2. A policy *named on the kernel command line*, and the drive its scan chose.
3. The hook's offer — which beats the **default** scan, because the default is
   not an instruction and the hook looked harder: the scan can only ask
   `slab list` whether a drive is one of ours, and the hook read the drive.

`/init` refuses an offer that is not on this machine, and one that names the
drive this boot is reading from: offering that would hand the node its own
root to reformat. It never passes `--local-disk-force` for an offered drive —
force destroys whatever a drive carries, and a drive that had to be forced is
by definition not the blank one a hook offered.

### A hook is asked, never obeyed

`/init` refuses a decision it cannot act on, logs why, and moves to the next
hook — and with no hook left, the ordinary probe decides:

- exit 0 with no `ZB_SLAB`, or a local `ZB_SLAB` that is not on this machine
  (a URI, `*://*`, is not checked for existence);
- exit 0 with a `ZB_ACTION` other than `boot-local` or empty;
- any other exit status. Exit 2 means ask-appliance whatever `ZB_ACTION`
  says.

The failure this avoids is trading the appliance fallback — which works — for
a boot that commits and then drops to an initramfs shell.

### Nothing a hook prints is executed

The obvious reading of a `KEY='value'` contract is `eval "$(hook boot)"`.
`/init` does not do that, and neither should anything else that consumes one.
This is PID 1: `eval` there makes a stray log line on stdout a command run as
root before there is a system to run it on. The values are read out with `sed`
instead, so the worst a misbehaving hook can do is be ignored.

`tests/initramfs-boot-hook.sh` installs a hook that prints a command among its
assignments and checks it did not run.

## Why a hook, and not a better probe

Three things a hook can answer that `slab list <the device the cmdline names>`
cannot, all seen on hardware:

1. **The command line names one device; the slab may be on another.** The
   command line is a pallet member, identical on every machine that boots the
   image, so `rd.stormblock.slab=/dev/sda2` is a guess about enumeration
   order.
2. **A slab is not the same thing as a bootable disk.** One formatted and
   never filled answers `2047 slots, 2047 free` and boots nothing. A hook can
   check the ESP for a loader entry, and the kernel and initramfs it names —
   and `stormblock slab volumes <dev>` (#108) lets it check the boot volume is
   really in the slab, offline, without attaching anything.
3. **Whose disk is it.** Nothing in a slab superblock records an owner, so a
   disk moved between chassis is indistinguishable from one that was always
   there — and the hostname on it is the node CA's subject CN.

And one thing it can contribute rather than answer: **which drive is free**,
in `ZB_TAKEABLE`. The assimilation itself stays where it is — `boot-local`
implements it, and a hook has no business reimplementing that.

## Installing one

Either the hook's own installer writes it into the image, or the build does:

```bash
BOOT_HOOKS="/path/to/zeroboot" ./scripts/build-stormblock-initramfs.sh
```

**A dynamically linked hook is refused at build time.** There is no loader in
this initramfs, so a glibc build fails at boot as `not found` — on a file that
is plainly there, with the executable bit set, which is as misleading as an
error gets. Static musl, or a shell script.
