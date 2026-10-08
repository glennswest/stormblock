# Data classes: system, partner, customer

**Status: a review (#356, 2026-10-08).** It collects what the design says about
the three data classes, puts it next to what the code does today, and lists
what is designed but not built. The build of the class split is a separate
issue; the open questions are at the end.

The terms are the design's: **system** (ours), **partner** (vendor,
integrator) and **customer** (user). The owner's 2026-10-08 words "oem /
customer" are not used; "oem" is the system class.

## 1. Where the design came from

| when | where | what it said |
|---|---|---|
| 2026-07-21 | stormcos#17 (closed) | One **crate** = the read-only volumes of a release: OS + system containers. Install artifact = upgrade artifact. Node state (`var-*`, `containers-*`) is **not** in the crate. |
| 2026-07-21 | stormcos#20 (closed) | Four crates, each with its own **owner, signer and lifecycle**: **System** (us; pivoted on a stormcos release), **Integrator** (a partner; added and updated apart from System), **Customer** (the end customer; customer-managed, longest-lived), **Updates** (deltas over a base; short-lived). The stack order was left to decide ("System lowest, Customer/Updates highest?"), with precedence and signing together to stop a customer image shadowing a system one. |
| 2026-08-19 | stormcos#24 (closed) | "crate" renamed "pallet": the bundle is a pallet. |
| 2026-08-23 | f94effa, `docs/pallets.md` §2.5–2.8 | The pallet kinds `vendor` (8) and `user` (9) are added. `vendor` is alternated A/B like `system`. `user` is one pallet, never replaced wholesale. A pallet's slab is a **hard allocation boundary**, so a customer CoW can never spill into the system pallet and be lost with an upgrade. |
| 2026-09-30 | stormcos#17, #20 closed | Superseded: "grouped content is pallets (system1, kube1, data1, and partner/customer pallets in the design)". |
| 2026-10-06 | #311 (owner, the first rule for installs) | An install touches the system drive's **system half** only. The data half and every data volume are adopted, never recreated, and never wiped as a fallback. |
| 2026-10-06 | #312 (owner) | Node release/reset = **one overwrite of the extents user volumes actually use**, then freed. Built as `DELETE …?scrub=used` (#313). |
| 2026-10-08 | #349 (owner) | Old system volumes are disposable; partner/user apps and data are not. |
| 2026-10-08 | stormcos#456 (owner) | The node's config and history (**app-system-data**) are kept across installs. Every item belongs to us or to the customer. A **demote/reset** clears the customer's and keeps ours; a **factory wipe** is a separate, explicit step. The classes were separate **partitions**, each holding its own app data, managed through stormconsole (stormpump's apps) and CRDs. |
| 2026-10-08 | stormcos#448 (owner: later) | An `App` definition (images by digest, objects, data volumes with a reuse/replace/migrate policy). Partner apps come from a signed partner catalog on the forge. |

The design has three classes. stormcos#20's fourth crate, **Updates**, has no
counterpart in the pallet design: no pallet kind or note carries it forward.

## 2. The classes, as designed

| | **system** (ours) | **partner** (vendor, integrator) | **customer** (user) |
|---|---|---|---|
| pallet kinds | `boot`, `kernel`, `system`, `kube`, `runtime`, `data` (shipped, sealed) | `vendor` | `user` |
| partition | `system1` / `system2` (A/B), `kernel1`, `kube1`, `data1` | two pallets, A/B (named like `system1`/`system2`; the design names none) | one `user` pallet |
| what it holds | the platform: root, kernel, control plane, our services' images | software a partner layers on the platform | pulled and pushed images, customer data, customer apps |
| its app data | our services' state, and the node's record of itself (§4) | the partner apps' volumes | PVCs, the customer apps' volumes, `config/apps/` (what to launch, #448) |
| owner, signer | us; stormcentral's supply chain | the partner; a signed catalog per partner (stormcos#20, #448) | the customer; nothing signed today |
| replaced by | a stormcos release (A/B pallet) | the partner's own release (A/B pallet), on its own cadence | never wholesale |

Signing is designed per pallet: one signature over `manifest_digest` covers a
pallet's whole combination of members (`docs/pallets.md` §1, §2.9). It is not
implemented. The format reserves the field, so adding it changes no layout.

## 3. What each event does to each class

| event | system | partner | customer |
|---|---|---|---|
| **install** (a new release over a running node) | release content replaced; the node's own data kept | kept | kept |
| **upgrade of one class** | system A/B | partner A/B | — |
| **demote / reset** | kept: the node stays debuggable, and its hardware record follows it | cleared | cleared |
| **factory wipe** | cleared (an explicit, separate step) | cleared | cleared |

"Cleared" means the class's volumes are deleted with `scrub=used`. Each slot
they wrote is overwritten once, discarded on flash, then freed (#312, #313,
`docs/erase.md`). Free space is not touched.

**The install row is the design's.** pallets.md §2.8 says `system` is replaced
wholesale and `user` must be left untouched; stormcos#17 says node state is not
in the release. **The demote/reset and factory-wipe rows are not in
stormblock's design.** They are the owner's rules of 2026-10-06 and 2026-10-08
(#312, stormcos#456).

## 4. Where app-system-data fits

app-system-data (stormcos#456, built as the `system-data` volume in #355) is
**system class by owner and customer-like by lifetime.** It is ours, and a
demote keeps it. But the design's system pallet is replaced on every install,
and `system-data` must survive installs.

So the class axis (who owns it) and the lifetime axis (replaced by a release,
or kept) are separate, and every class has both halves:

| | release content (replaced by its owner's release) | kept data (survives installs) |
|---|---|---|
| system | `system1`/`system2`, `kernel1`, `kube1`; today the system slab | `system-data`, our services' state; today the data slab |
| partner | `vendor1`/`vendor2` | the partner apps' volumes |
| customer | — (the customer has no release) | `user`: images, PVCs, apps |

**`system-data` belongs in the system class's kept partition:** a data slab of
its own (or the system class's share of one, per §6), never the system pallet.
That is where #355 puts it today, in `stormblock-data`, and today that slab is
also every other class's kept data (§5).

## 5. What is built, and how it differs

The node disk today (`image::local::lay_node_slabs`, #285, #311):

```
boot area     ESP + boot pallets               (local boot, #123)
stormblock    slab, role system                the release's goldens and clones
stormblock-data  slab, role data               everything kept
stormblock-bulk  slab, role data, 8 MiB extents  (format 2, a data half ≥ 256 GiB, #156)
```

| design | built | how they differ |
|---|---|---|
| three classes, each its own partition | **two halves**: slab role `system` or `data` (`SlabRole`), plus the volume `origin` mark `release` / `node` / `unmarked` (#349) | partner and customer data share `stormblock-data` with `system-data`, our services' state (`fastetcd-data`, `stormcert-data`, `registry-data`, `kubelet-data` …) and the PVC blanks. Nothing on disk says which class a volume belongs to |
| `vendor`, `user` pallets | enum values in `crates/pallet-format` (f94effa); **nothing writes or reads them** | no partner or customer pallet exists anywhere |
| a pallet's own slab, a hard boundary (§2.6) | volumes share slab extents (`docs/composed-disks.md`); **the slab role is the boundary** | the boundary is two-way (system/data), not per class. A volume with no role used to land in the system half and vanish on install (#317, stormblock-registry#104); #349 now carries the node's volumes across |
| containers, L1/L2 map, per-container A/B (§2.6–2.8, #59) | not built | |
| A/B system pallets | the system half is re-laid in place by an install (#311) or staged beside N as `<name>@<v>` (#122, `docs/staging.md`) | one generation kept, not two partitions |
| `data` = sealed shipped data (§2.5) | stormcos's `data1` pallet ships the sealed blanks of the data volumes (`stormcert-data`, `stormblock-state`, …); the kept data lives in the data slab as their clones, not in a pallet | §2.5's wording is right for `data1`; writable data was never a pallet (#59: PVCs are CoW clones on the data slab) |
| per-class signers | no pallet signing (#45/#49 in stormcos); sbregistry verifies image signatures on arrival (registry #53) | |
| demote / reset | the primitive only: `DELETE /api/v1/volumes/{id}?scrub=used` (#313) and `GET /api/v1/erasures` | no operation picks a class to clear, because nothing knows the classes |
| factory wipe | an install with `force` lays both halves fresh; it does not scrub | |
| per-volume release policy | `/etc/stormblock/data-volumes`: keep, replace or migrate per data volume (#122) | system class only; the App policy for partner and customer apps is stormcos#448, later |

## 6. Open questions, for the build issue

Each one changes what is built, so they are for the owner.

1. **Partitions.** The data half today is one slab, `stormblock-data` (plus
   `stormblock-bulk`). Should it become one slab per class: `stormblock-data`
   (system: `system-data` and our services' state), `stormblock-partner`, and
   `stormblock-customer`, with customer last because only the last partition
   grows in place (`grow_data_half`)? Bulk goes per class or customer only. The
   alternative is a class mark on every volume within one shared slab. That
   needs less space planning, but a reset then filters volume by volume, which
   is what the owner's caution on stormcos#456 argued against.
2. **Our services' state on a demote.** `fastetcd-data` holds every
   customer object in the cluster, `registry-data` indexes pushed customer
   images, and `kubelet-data` holds pods' state. They are ours by owner but
   carry the customer's content. On a demote, are they kept (system), cleared,
   or re-cloned from their release goldens? The recommendation is that the node's
   record (`system-data`, `stormcert-data`, `stormdrive-data`) is kept, and the
   cluster's and workloads' state is cleared with the customer class.
3. **Factory wipe.** Is it every class scrubbed (`scrub=used`), then a fresh
   lay, while the release is kept? Or the disk handed back blank?
4. **Existing nodes.** Their `stormblock-data` holds everything. On the first
   boot of an engine with the split, should the node-origin volumes that are
   not ours (PVC clones, pushed images, `img-…` goldens) move to the
   customer slab? A partner volume can't be told from a customer one today, so
   the first boot could only file them as customer.
5. **Who says which class.** Should a volume's class be set by the creator (an
   import or registry push says `customer`; a partner catalog install says
   `partner`), with no default? Or should anything not from a release default
   to customer?
