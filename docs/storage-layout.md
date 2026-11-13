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
