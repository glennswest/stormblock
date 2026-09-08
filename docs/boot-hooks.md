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
| 0 | `ZB_ACTION=boot-local` — boot from this slab | `ZB_SLAB` (required), `ZB_SLAB_ID`, `ZB_VOLUME`, `ZB_DRIVE` |
| 2 | `ZB_ACTION=ask-appliance` — do not boot locally | `ZB_REASON` |
| 1 | `ZB_ACTION=error` — the hook could not tell | `ZB_REASON` |

The `ZB_` prefix is the contract as `zeroboot` already emits it; the mechanism
is not zeroboot's, and anything can drop a hook in.

- `ZB_SLAB` may be a device path or a fabric URI (`nvme-tcp://…`). A path that
  is not on this machine is not believed — see below.
- `ZB_VOLUME` names the boot volume, and is used only when the command line
  did not name one: the hook answers *where*, an operator answers *which*.
- Anything else the hook prints is ignored.

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

## Installing one

Either the hook's own installer writes it into the image, or the build does:

```bash
BOOT_HOOKS="/path/to/zeroboot" ./scripts/build-stormblock-initramfs.sh
```

**A dynamically linked hook is refused at build time.** There is no loader in
this initramfs, so a glibc build fails at boot as `not found` — on a file that
is plainly there, with the executable bit set, which is as misleading as an
error gets. Static musl, or a shell script.
