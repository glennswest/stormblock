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
