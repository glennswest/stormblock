# Layering — what belongs where, and why it matters for stormos

**Status:** notes, 2026-08-19. Written before the refactor so the refactor is
done with the destination in mind, not just the itch.

## The observation that started it

If you build a stormos base and want what stormblockmk does, you find almost
all of it trapped in a crate named for the RouterOS profile. Measured:

| module | lines | RouterOS-specific lines |
|---|---:|---:|
| `api.rs` | 1,185 | 0 |
| `reconcile.rs` | 619 | 0 (one explanatory comment) |
| `ext4.rs` | 486 | 0 |
| `reap.rs` | 479 | 0 |
| `wiring.rs` | 376 | 0 (one comment) |
| `config.rs` | 308 | defaults only |
| `ctx.rs` | 253 | a little |
| `status.rs` | 251 | 0 |
| `tarfs.rs` | 204 | 0 (one comment) |
| `trim.rs` | 110 | 0 |
| `netstat.rs` | 83 | 0 |
| `main.rs` | 511 | composition |

**4,865 lines, and the RouterOS-ness is 11 mentions in config defaults and
startup.** Every mention in the substantial modules is a `//!` comment
explaining *why* a decision was made — "RouterOS attaches one export", "an
initiator that cannot select a LUN" — not RouterOS logic.

So a second deployment wants ~3,700 of those lines and, today, can only fork
them or depend on a crate whose name is a lie about its contents.

## The three layers

1. **Engine — mechanism.** Drives, arrays, slabs, the GEM, thin volumes,
   snapshots, filesystem templates, the ext4 formatter, the iSCSI and NVMe-oF
   targets, the reactor pool. How storage *works*. Runs standalone.
2. **Serving — deployment-agnostic policy.** The wiring table (export →
   transport, port, NQN/IQN/LUN), the reconciler, ordered teardown and drain,
   readiness, reaping, tar in/out, trim, live-session detection, raw import.
   What it takes to *serve* volumes to something. None of this is a choice a
   deployment makes differently; it is the job.
3. **Profile — the deployment.** Advertise address, portal range, where the
   slab lives, auth, the `/data`-is-a-mountpoint guard, boot composition.
   This, and only this, is what makes a build "the RouterOS one" or "the
   stormos one".

Today layer 2 sits inside layer 3. That is the whole of the problem.

## A gap this exposes, which is not a naming problem

`ctx.rs`: *"Persist the engine's export table. The engine keeps it in memory
only; mk owns durability for it, atomically."*

Durability of export state is not a policy anyone could reasonably choose
differently — it is a correctness requirement, and it lives in the profile
because the engine has a hole. Every new consumer inherits the hole and
re-solves it. Move it down with the rest.

## Why this matters more than tidiness: the runtimes above it

The intent is several runtimes on one substrate — containers, Kubernetes, full
VMs, and two flavours of micro-VM. That changes how layer 2 must be designed,
so it is worth being explicit now:

- **Do not let layer 2 assume a container.** It serves *volumes*. What attaches
  to them — a container root, a VM disk, a micro-VM rootfs — is layer 3's
  business.
- **A VM is the easier case, not the harder one.** A VM wants a block device.
  A CoW clone *is* a block device. The container path needed the stub-root and
  `/payload` gymnastics only because a RouterOS container will not take a block
  device as its root. Nothing in the golden model is container-shaped; it was
  bent into a container shape at the edge.
- **The golden model is already runtime-neutral.** "A read-only filesystem
  built once, cloned per instance, shared by refcount" describes a container
  image, a VM template and a micro-VM rootfs equally well. `FROM` layering
  (2026-08-19) and chain-ID keying are properties of the *content*, not of OCI:
  an image whose layers happen to arrive as an OCI manifest is one source of a
  golden, not the definition of one.
- **Micro-VMs make `/raw` the primary path, not a convenience.** A
  firecracker-style guest boots a raw disk. Importing a pre-built image and
  cloning it is exactly that, with no unpack step anywhere.

The practical rule for the refactor: if a name in layer 2 says "image",
"container" or "pod", it is probably in the wrong layer or wrongly named.
Layer 2 should be expressible in volumes, exports, templates and clones.

## Shape to aim for

```
stormblock            engine + serving   (layers 1 and 2)
  └── stormblockmk    RouterOS profile   (layer 3, ~300–500 lines)
  └── stormos         stormos profile    (layer 3)
```

Promoting layer 2 into the engine rather than into a third crate, because the
engine already owns exports, LUNs, sessions and both targets — the wiring
table is the piece that makes those usable, so it completes the engine rather
than polluting it. A separate `stormblock-serve` stays the alternative if the
engine is to remain strictly mechanism-only as a rule; it costs one more crate
and one more boundary.

## Also on the wrong side today

`/mk/v1/volumes/{id}/tar` and `/mk/v1/volumes/{id}/raw` are content-writing
mechanisms sitting in the profile because the profile is their only caller.
They are the two things a second deployment would copy first. They move with
layer 2.

## Bootable formats — where they fit (notes, 2026-08-19)

A golden today is a bare filesystem: `mkfs-ext4` formats the whole device,
there is no partition table and nothing boots it. That is the right shape for
a container root and it is already the right shape for a micro-VM. It is not
the shape a firmware boot wants.

The ladder, cheapest first, with what exists:

1. **Micro-VM, direct kernel boot — nothing new needed.** A
   firecracker/cloud-hypervisor guest is handed a kernel, an initrd and a raw
   block device for root. A clone *is* that block device. No partition table,
   no bootloader, no ESP. This is why micro-VMs are the easy case: the format
   we already build is the format they want.
2. **Network boot — already specified.** `docs/stormblock-ipxe-boot.md` and
   `docs/linuxboot-iscsi-spec.md`. The root lives on a stormblock volume and
   firmware never reads a local disk, so again nothing on the image has to be
   made bootable.
3. **VM with firmware boot — this is the real gap.** Needs a whole-disk image:
   protective MBR + GPT, an ESP (FAT32) holding a bootloader, and the rootfs
   partition. **None of that code exists** in stormblock, mkfs-ext4 or
   fio-ext4 — searched. It is a GPT writer, a small FAT32 writer, and
   bootloader placement.
4. **Hypervisor container formats — qcow2, VMDK, VHD.** A wrapper around a raw
   image. `mkube/pkg/diskimg/` already has Go converters (`qcow2.go`,
   `vhd.go`, `vmdk.go`), currently *to* raw; the build tool needs the other
   direction, or to shell out.
5. **ISO (El Torito)** — separate wrapping, mostly install media rather than a
   runtime root.

### Two golden shapes, named

The distinction to keep straight, because it changes what a clone is:

- **Filesystem golden** — what is built today. Clone attaches as the root
  filesystem. Containers, micro-VMs, netboot.
- **Whole-disk golden** — partition table, ESP, rootfs partition. Clone
  attaches as a *disk* that firmware can boot.

Both are goldens, both clone by refcount, both import through
`/mk/v1/volumes/{id}/raw`. The difference is only what the builder lays down,
which is another reason bootability belongs in the **builder**, decided once
at build time, exactly like the golden itself.

### Consequence for the build tool

This is an argument for a `stormblock-build` that owns "produce an artifact
from an image", with the target as a parameter — filesystem golden, whole-disk
golden, and a format wrapper — rather than bootability being bolted onto
`sbregistry build-image`, which is named and shaped for OCI images.

### What this replaces (2026-08-19)

stormcos is to be **pure Rust**, and this stack is what builds it — replacing
`stormcos_builder`, which today composes the node image in a disposable LXC
clone from harvested component releases and publishes qcow2/raw.zst. Notably
that builder shells out to almost nothing already (one `qemu-img`); the
non-Rust part is the LXC compose step, not the tooling around it.

So the target is: a stormcos image is a **whole-disk golden**, built by
`mkfs-ext4` + `fio-ext4` plus the GPT/ESP/bootloader piece that does not exist
yet — no LXC, no distro tooling, no host to be shared or to go stale. The same
artifact then imports through `/raw` and clones per node like any other golden,
which also makes a node image rebuild a rebase rather than a re-compose.

**Per architecture, and this is not a detail.** A bootable image is
arch-specific all the way down: x86_64 wants BIOS and/or UEFI with its own
loader binaries, arm64 is UEFI-only in practice, and the ESP contents differ.
The builder takes the target arch as a parameter and the two are separate
artifacts — a `base-<chainid>` shared across architectures is a category error,
since the layers themselves differ.

Related: Kubernetes is already Rust here (`rustkube`), so the runtimes above
stormblock — containers, k8s, VMs, micro-VMs — can be one Rust stack rather
than a Rust storage layer under borrowed pieces.


## Layer references, and what it takes to move them

A layered golden is not a stack that gets composed at read time. Each level
owns a **complete extent map**, and every entry in it is an
`ExtentLocation { slab_id: SlabId(Uuid), slot_idx: u32 }` — a slab named by
UUID, and a slot within it. `base → l2 → l3` means `l3`'s map already names
every slot it needs, whichever level first wrote it.

Two things follow, and they pull in opposite directions.

**Depth is free to read.** There is no chain to walk, at any depth, so a
twelve-level stack reads exactly as fast as a one-level one. A runtime clone
of the deepest level is one map copy plus a refcount bump — measured at one
slot for a 9 MiB stack, the cost of the clone stamping its own filesystem
identity. Because that is so cheap, a clone can be made before it is wanted
and parked; a cold start becomes a map lookup rather than a build. Writes go
copy-on-write into fresh slab space and rewrite only that clone's map, so the
levels underneath are never written — the clone is disposable by
construction. All of this is asserted in
`fs::template::tests::a_clone_flattens_the_stack_and_writes_only_to_itself`.

The reason to squash is therefore **space, never latency**: each level carries
its own filesystem metadata, and each level's writes round up to a slot. Two
or three levels is a good trade; twelve is paying that tax twelve times for no
read benefit.

**But a map is only meaningful next to the slabs it names.** This is the real
cost of flattening, and it decides how a volume travels:

- **Moving a pallet is free.** Slabs are identified by UUID, not by node,
  device, or path. Move the slabs and every map that references them stays
  valid verbatim — nothing to rewrite, nothing to rebuild, and the layer
  structure comes along untouched. This is the case worth designing for,
  because it is the common one.
- **Moving a volume away from its slabs is a rebuild.** Cloning a container to
  a node that does not hold those slabs means materialising the content into
  the destination's own slabs and writing a fresh map. Here the flat map is an
  advantage rather than an obstacle: there is one map to read and one set of
  slots to pull, with no chain to resolve first. `volume::relocate` is this
  operation within a node; across nodes is the same shape with a network in
  the middle.

Local first. Off-system references — a map entry naming a slab held by another
node — are possible and are not ruled out by anything above, since the
reference is already a UUID rather than a local address. They are deliberately
not the starting point: a local reference cannot be broken by a partition, and
a design that works when every slot is reachable is the one worth having
before adding the case where some are not.
