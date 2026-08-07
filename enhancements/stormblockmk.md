# stormblockmk — StormBlock packaging profile for RouterOS containers

**Status:** Proposed (2026-08-07)
**Driven by:** mkube `enhancements/stormblock-registry.md` (sbregistry — the
CoW image registry / mkube supervisor / PVC provisioner on rose1). Read that
spec first; this file covers only what StormBlock itself must provide.

## What sbregistry needs from StormBlock

A build/packaging *profile* (not a fork) that runs inside a RouterOS
container on rose1 as a stormd-supervised sibling process of sbregistry:

1. **File-backed slab store** as a first-class, default-configurable backing:
   slabs are large preallocated files on a mounted path (`/data/slabs`), no
   raw block devices — RouterOS containers have no /dev passthrough. CoW/thin
   semantics come from the GEM as usual; the underlying ext4 is opaque.
2. **Static musl aarch64 build**, scratch-image friendly (no shell, no /tmp
   assumptions, dirs created with MkdirAll, logs to a configurable dir on a
   mounted volume — see stormd#1 lessons).
3. **iSCSI target** bound on the container IP (gt bridge); NVMe-oF/TCP
   optional until RouterOS initiator support is validated.
4. **Shared-ring IPC** (unix socket + memfd) reachable by a sibling process
   in the same container (sbregistry is the only control-plane client):
   create/clone/delete thin volumes, snapshot, refcount/GC hooks, expose
   target/LUN identifiers for a volume.
5. **Crash-safety posture**: initiators (RouterOS itself) will reconnect
   after a restart; volume metadata must recover cleanly from power loss —
   pods' root filesystems ride on these LUNs.

## Explicitly out of scope here

OCI registry logic, golden-image build (mkfs/extract), mkube supervision,
webhook notifications, PVC storage-class integration — all sbregistry
(mkube-side spec). StormBlock stays a generic block engine.
