# Staging the next release on a running node

Issue #122, for stormupdate#1. Decided by the owner on 2026-10-06, on #122:

- **1(b):** a release may mark each data volume keep, replace or migrate.
- **2(A):** stormupdate re-points `boothost/<tag>` to N+1 before the reboot, and an install still wipes.

An update is made from the running system, never by an install at boot.

## The sequence (stormupdate)

```text
POST /api/v1/releases/{N+1}/stage {source, current: N}   → 202, a job
GET  /api/v1/releases/{N+1}/stage                         → running … complete (plan, migrations)
     run each migration's hook (stormupdate's; the engine runs nothing of the release's)
POST /api/v1/releases/{N+1}/activate                      → renames, boot pallet raised
     re-point boothost/<tag> to N+1 on the appliance
     drain, reboot
```

**Rollback**, when the node fails its gate:

```text
POST /api/v1/releases/rollback
```

Then re-point `boothost/<tag>` back to N and reboot.

Every verb but the GETs is destructive (#274). It needs the admin token, or a Kubernetes bearer whose SubjectAccessReview allows `update` (or `delete`, for a DELETE) on `storage.storm.io` `releases`.

`GET /api/v1/releases/generations` shows `current`, `staged` and `previous`. They are kept in `<data_dir>/release-generations.json`.

## What stage does

`source` is N+1's published image. That is `http://<appliance>/api/v1/releases/{v}/image.img`, read with `Range` GETs, so only what the slabs map crosses the wire and nothing on the appliance changes. A path also works.

The image's slabs are opened in a volume manager of their own. Nothing is copied that the node already has.

| image volume | becomes |
|---|---|
| a golden (sealed) | `<name>@<v>`, **under the release's own id**, copied unsealed and sealed when whole |
| a clone whose parent is staged or already here | `<name>@<v>`, a copy-on-write clone of that parent, plus the extents it has of its own |
| a volume whose id the node already has | shared: nothing copied, nothing renamed |
| an unsealed volume, policy **keep**, which the node has by name | kept: the node's volume stays, and N+1's is not staged |
| anything else (replace, migrate, or a volume N+1 adds) | `<name>@<v>`, copied |

N+1's boot pallet is copied into the disk's boot area **just below** the pallet the disk boots now. The ESP is not touched. The disk is the slab path the engine was started with that carries a node layout, or `disk` in the request.

A stage replaces anything staged before it, whole or not. It also deletes the generation before the current one, keeping one previous generation (the owner's rule). A volume something else is still cloned from is kept: for example N's data golden, while the node's kept volume is its clone.

### The release's policy

The policy is read from the release's root volume (`root`, default `stormpump`), at `/etc/stormblock/data-volumes`. Each line names one volume:

```text
# volume       policy    [hook: a path in the release's root filesystem]
kubelet-data   keep
fastetcd-data  migrate   /usr/libexec/stormcos/migrate-fastetcd
pod-logs       replace
```

- **No file, or a volume not listed:** replace in the system half, keep in the data half.
- **A word the engine does not know:** the stage fails. A policy half-understood is not applied.
- **A golden:** always staged whatever its policy, because the goldens are what `slab holds` compares.
- **migrate:** the volume is listed in the generation's `migrations` as `{volume, hook, staged}`. stormupdate runs the hook with both volumes present by name, the node's `<volume>` and the release's `<volume>@<v>`, before it activates.

## What activate does

For each staged volume:
- the node's volume of the same name becomes `<name>@<N>`;
- the staged volume takes the plain name.

Then N+1's boot pallet goes on top. The renames are checked before any is made, and undone if one fails.

`current` (in the stage or the activate request) names N when the engine does not know it yet. A node that has only ever been installed has no generation record.

## Why the boot after it keeps the disk

A boot whose claim is another release than the disk holds is an install: it lays the system half again and keeps the data half (#311, which replaced #261's wipe). `slab holds <disk> <claimed image>` decides, and it compares the image's goldens by id with the disk's sealed volumes. A staged update is still the way to change release without losing the system half's own volumes.

- **After activate:** N+1's goldens are on the disk under their own ids, and stormupdate has re-pointed the assignment to N+1. The boot claims N+1, the disk holds it, and the node boots its disk with N+1's names.
- **After a rollback:** N's goldens are still there, so a claim of N is held too (#265).
- **During a stage cut short:** the goldens are unsealed until whole. `slab holds` counts only sealed ones, so a half-staged release never reads as held.

## Not done here

- Running the migration hooks. That is stormupdate's job, by the owner's split.
- Writing `/etc/stormblock/data-volumes`. That is the release's job (stormcos).
- Staging on a node with no laid disk. The volumes stage, the boot pallet does not, and it is logged.
- A release whose data volume shares an id with the node's, staged as replace. It is shared instead, because one id is one volume.
