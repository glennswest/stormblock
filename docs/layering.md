# Layering — what belongs where, and why it matters for stormos

**Status:** design rationale, 2026-08-19, updated 2026-09-26 (#131). Written
before the serving layer moved into the engine; that move has happened, so the
"today" parts below now say where things landed. The model — three layers, a
runtime-neutral golden, flat maps that reference slabs by UUID — is current
and is cited from the code (`src/serve/mod.rs`, `src/mgmt/api/mod.rs`,
`src/mgmt/config.rs`).

## Where it started, and where it landed

The serving layer began in `stormblockmk`, a crate named for the RouterOS
profile: 4,865 lines of which the RouterOS-specific part was 11 mentions in
config defaults and startup. It now lives in the engine as `src/serve/` —
`api.rs`, `reconcile.rs`, `reap.rs`, `wiring.rs`, `ctx.rs`, `status.rs`,
`tarfs.rs`, `trim.rs`, `config.rs`, `netstat.rs` (≈3,900 lines) — mounted at
`/serve/v1`, with `/mk/v1` kept as a deprecated alias of the same routes. The
ext4 formatter is `src/fs/ext4.rs` over the `mkfs-ext4` crate.

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

Layer 2 used to sit inside layer 3; it is now in the engine.

## The durability gap, closed

`ctx.rs` once said *"the engine keeps [the export table] in memory only; mk owns
durability for it"*. Durability of export state is a correctness requirement,
not a policy, and it now lives in the engine: the wiring table is written
atomically (`src/serve/wiring.rs`) and API exports are restored at start
(`restore_exports`, `src/mgmt/api/exports.rs`).

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

## Content in and out

`tar` and `raw` — the content-writing mechanisms a second deployment would
have copied first — moved with layer 2: `/serve/v1/volumes/{id}/tar` and
`/serve/v1/volumes/{id}/raw`. A whole image also imports through
`POST /api/v1/volumes/import` (raw, qcow2, VMDK, OVA, ISO).

## Bootable formats — where they fit (notes 2026-08-19; status 2026-09-26)

In August a golden was only a bare filesystem. Whole-disk goldens have been
built since: `stormblock image build` lays GPT, ESP (FAT16/32 writer,
`src/image/fat.rs`) and pallets (`docs/images.md`), a disk can be composed out
of shared goldens with nothing written (`docs/composed-disks.md`), and images
are written as raw, qcow2, VHD, VMDK or ISO (`src/image/formats.rs`).

The ladder as it was reasoned, cheapest first:

1. **Micro-VM, direct kernel boot — nothing new needed.** A
   firecracker/cloud-hypervisor guest is handed a kernel, an initrd and a raw
   block device for root. A clone *is* that block device. No partition table,
   no bootloader, no ESP. This is why micro-VMs are the easy case: the format
   we already build is the format they want.
2. **Network boot.** Done by stormbootx (UEFI, NVMe/TCP attach of a claimed
   clone) chain-loading stormuefi, which boots a pallet off it. The iPXE and
   LinuxBoot designs of 2026-03 were not the path taken
   (`docs/history/`).
3. **VM with firmware boot.** Was the real gap; now `src/pallet/gpt.rs`,
   `src/image/fat.rs` and `image build`.
4. **Hypervisor container formats — qcow2, VMDK, VHD.** Written by
   `src/image/formats.rs`, read by `src/image/decode/` (qcow2, VMDK).
5. **ISO (El Torito)** — `src/image/iso.rs`.

### Two golden shapes, named

The distinction to keep straight, because it changes what a clone is:

- **Filesystem golden** — what is built today. Clone attaches as the root
  filesystem. Containers, micro-VMs, netboot.
- **Whole-disk golden** — partition table, ESP, rootfs partition. Clone
  attaches as a *disk* that firmware can boot.

Both are goldens, both clone by refcount, both import through
`/serve/v1/volumes/{id}/raw` or `POST /api/v1/volumes/import`. The difference is only what the builder lays down,
which is another reason bootability belongs in the **builder**, decided once
at build time, exactly like the golden itself.

### Consequence for the build tool

This was the argument for a build tool that owns "produce an artifact from an
image", with the target as a parameter. It became `stormblock image
build|convert|inspect|formats|lay-node|local-boot` and `/api/v1/images/*`;
`sbregistry build-image` posts its specs to the engine.

### What this replaces (2026-08-19)

stormcos is to be **pure Rust**, and this stack is what builds it — replacing
`stormcos_builder`, which today composes the node image in a disposable LXC
clone from harvested component releases and publishes qcow2/raw.zst. Notably
that builder shells out to almost nothing already (one `qemu-img`); the
non-Rust part is the LXC compose step, not the tooling around it.

So the target is: a stormcos image is a **whole-disk golden**, built by
`mkfs-ext4` + `fio-ext4` plus the GPT/ESP/bootloader piece (now built) — no LXC, no distro tooling, no host to be shared or to go stale. The same
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
`ExtentLocation { slab_id: SlabId(Uuid), slot_idx: u32, ref_count, generation,
mirrors }` — a slab named by UUID and a slot within it, plus the share count,
the copy-on-write generation and any mirror legs. `base → l2 → l3` means `l3`'s map already names
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
