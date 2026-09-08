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
| `any` (default) | any drive that is not already a stormblock slab |
| `blank` | a drive with no slab and no partition table |
| `off` | nothing |
| `force` | a drive even when it is one of ours, destroying the identity on it |

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
| this node's own layout — a data half and a system half | **updated**: the system half is formatted afresh for the new goldens, the data half is opened and kept, and the node boots normally with the identity it already had. This is what an install *is*, and it needs no `force`. |
| the same, already holding what this boot carries | **nothing at all.** The system half's own record is read offline and compared by volume id; if it already holds everything this boot would copy, it is left exactly as it is and the node boots from it. An update that has nothing to update must not reformat a working half and re-copy the same bytes. |
| a lone data slab | refused. That is an install abandoned part-way, and it is indistinguishable from a live node's identity — `--local-disk-force` is the deliberate act for a drive whose identity is spent. |
| a lone system slab | left alone; the policy is about drives that are nobody's. |

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

- exit 0 with no `ZB_SLAB`, or a `ZB_SLAB` that is not on this machine;
- exit 0 with an `ZB_ACTION` that says something other than `boot-local`;
- any other exit status.

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
