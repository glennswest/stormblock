---
marp: true
title: stormblock
description: The block storage engine of the Storm stack — its purpose and what it does today
paginate: true
size: 16:9
style: |
  section { font-size: 23px; }
  h1 { font-size: 40px; }
  h2 { font-size: 31px; }
  pre, code { font-size: 16px; }
  table { font-size: 18px; }
---

<!-- Render: npx @marp-team/marp-cli@4 docs/presentation.md -o out/presentation.html
     (add --pdf for PDF). Written 2026-09-26 for v19.1.2 (#132), after the docs
     were rewritten from the code (#131). Every claim here is checkable against
     the code; README.md gives the file for each. -->

# stormblock

**The block storage engine of the Storm stack**

Drives in; thin, copy-on-write, per-volume-redundant volumes out — as local
block devices (ublk), over NVMe/TCP and over iSCSI. It also builds and boots
the disks stormcos nodes run from.

v19.1.2 · `glennswest/stormblock` · Rust, one static binary

---

## The problem, in one slide

A stormcos node needs storage that is **fast to hand out** and **safe to lose a
drive under**, on anything from a RouterOS box to a 160-drive shelf:

- a container, VM or PVC wants its own writable disk **now** — not after an
  mkfs or a copy;
- every node boots the same image, yet each must be its own machine;
- a drive fails, and the volumes on it must come back without anyone asking.

stormblock answers all three with one idea: **a volume is a map of 1 MiB slots,
and a clone is a copy of the map.** Cloning a sealed golden is a snapshot plus
one superblock write — no mkfs, no copy.

---

## Where it sits in stormcos

stormblock depends on no other stormcos component. From stormcentral's
relationships graph, these depend on it:

| group | component | what it takes from stormblock |
|---|---|---|
| product | **stormcos** | ships it, its goldens and the initramfs that boots from it |
| node | **stormpump**, **stormvm** | root and volume devices; VM disks |
| storage | **stormblock-registry** | turns pushed images into goldens through its API |
| storage | **stormdrive** | registers, labels and reports on the drives below it |
| storage | **stormstorage**, **stormblock-csi** | distributed volumes and CSI over `/v1` |
| boot | **stormbootx**, **stormuefi** | the boot claim and NVMe/TCP attach; the pallet format |
| tooling | buildbox2 | the build environment, run as a golden |

rustkube's kubelet uses it too: it **is** the built-in PVC driver's storage.

---

## How it works

```
 drives · files · nvme-tcp:// · iscsi://               drive layer (O_DIRECT)
                     │
   slabs ── 1 MiB slots · role system|data · tier · failure domain
                     │
   global extent map ── volume → extent → legs (mirror / parity, per volume)
                     │
   thin volumes ── CoW clones · sealed goldens · ext4 / XFS templates
                     │
   ublk /dev/ublkbN │ NVMe-oF/TCP (shared + per-volume) │ iSCSI (shared + portals)
                     │
   management API :9090 ── /api/v1 · /v1 · /serve/v1 · /apis/storage.storm.io/v1
```

Slabs carry their own volume records, so a drive can move to another engine
and be adopted with its volumes. Clones share slots by refcount; a write copies
only the slot it touches.

---

## What it does today: storage

- **Drives** open at runtime, `O_DIRECT` (io_uring, or the blocking pool where
  there is none), with identity (serial, WWN) and labels (`shelf`, `bay`) from
  stormdrive; drained and reported failing over HTTP.
- **Slabs** in two roles: `system` (goldens, replaced by an install) and
  `data` (identity and state, never formatted by an install).
- **Thin volumes** that give space back on UNMAP / TRIM; **clones** that cost
  nothing until written; **sealed** goldens that take no writes.
- **Redundancy per volume** — `mirror:N`, `raid5`, `raid6`, spread at a
  failure-domain rung (`mirror:2@shelf`) — and **automatic rebuild** when a
  drive fails: most endangered volume first, several at once, one byte budget.
- **Drive-level RAID 1** across NVMe/TCP legs as a *dedicated* array a
  volume can be pinned to (how stormstorage builds a distributed volume).

---

## What it does today: filesystems, images, boot

- **Templates**: ext4 (`mkfs-ext4`) and XFS (`mkfs-xfs`) formatted in-process,
  checked, sealed; every clone gets a fresh filesystem UUID.
- **PVCs on stormcos**: the built-in `stormblock` class — the kubelet clones
  the sealed `pvc-ext4j-<MiB>m` blank of the claim's size class and attaches
  it over ublk. No CSI; CSI (`/v1`) is for third-party drivers.
- **Import**: raw, qcow2, VMDK, OVA and ISO images become sealed goldens, and
  the filesystems inside them are found and read (a Rocky 9 image: 19 s).
- **Images and pallets**: `image build` lays GPT disks and ISOs out of
  pallets — sealed, versioned boot sets stormuefi selects; `compose` builds a
  disk as a map over shared goldens, writing nothing.
- **Boot**: a machine claims `boothost/<tag>` (the one request needing no
  token), gets a clone of its own golden over NVMe/TCP, and `boot-local` flows
  it onto the local disk.

---

## Interfaces

| | |
|---|---|
| **API** | `:9090`. `/api/v1` (drives, arrays, slabs, volumes, templates, synonyms, pallets, images, rebuilds, …), `/v1` (the CSI contract), `/serve/v1` (exports, readiness), `/apis/storage.storm.io/v1` (kube-shaped, `?watch=1`) |
| **Data ports** | NVMe/TCP `4420`, iSCSI `3260`, per-export portals `3261–3388`, discovery UDP `7447` |
| **Auth** | a bearer token on everything, minted at first start; open: `/api/v1/health`, the `/serve/v1` probes, the boot claim |
| **Health** | `GET /api/v1/health` (public, no I/O); `GET /serve/v1/ready` — 200 only when an attach would work now |
| **Metrics** | `GET /metrics` (token): slab and drive gauges refreshed at scrape, API counters, pool, rebuild, iSCSI, serving |
| **CLI** | the daemon, plus `slab`, `image`, `pallet`, `golden`, `attach`, `boot-claim`, `boot-local`, `adopt-ublk`, `must-gather` |
| **Config** | `/etc/stormblock/stormblock.toml`: `[management]`, `[[drives]]`, `[serve]`, `[rebuild]`, `[gc]`, `[pressure]`, … every key and default in the README |

---

## How it ships and runs

- A **`special`** component in stormcentral: a bare binary plus
  `/etc/stormblock/stormblock.toml`, staged with `stormcentral component
  stage`, built `--locked` for musl by stormcos's `build-goldens.sh`.
- Three goldens: **`stormblock`** (the binary, `system1`),
  **`stormblock-data`** and **`stormblock-state`** (blanks, `data1`).
- **Boot**: the initramfs runs `boot-claim` + `boot-local`, and the engine it
  starts exports root as `/dev/ublkb0`.
- **Running**: stormpump's `00-stormblock` starts
  `adopt-ublk --api 0.0.0.0:9090 --data-dir /run/stormblock/engine`, which
  takes over those devices without them disappearing and restores its state
  from `stormblock-state`.
- **Update**: a new golden in a new release; the node re-claims or re-lays its
  system half, and the data half — identity and state — is kept.
- **Build**: `sc-build` on dev, never root; ~870 tests.

---

## Proven, and how

| what | proven by |
|---|---|
| a drive fails; its volumes rebuild on their own | `tests/integration_multidrive.rs` over HTTP; 1.8 → 3.1 GB/s at 1 → 8 volumes (`examples/rebuild_rate`) |
| a write during a rebuild reaches the new leg | `a_write_during_a_resync_reaches_the_rebuilt_leg` |
| XFS blanks and claims pass `xfs_repair -n`; a Rocky 9 image imports | `ci-xfs-verify.sh` on dev |
| a leg exported over `nvme_tcp` is a working namespace | `nvme_tcp_is_given_even_where_ublk_would_be` |
| a composed disk boots in OVMF | `ci-compose-disk-verify.sh` |
| the API is closed but the boot claim is open | `ci-auth-verify.sh` |

---

## Planned, not built

- **Scale to 40 PB a node**: compact resident metadata, extent size per class,
  incremental and paged metadata, 64-bit indexes (#155–#158).
- **Scrub** on a schedule (#160, needs a decision: no checksums yet) and
  **erasure coding** wider than P+Q (#159).
- **Claim policy**: redundancy / spread / tier from the StorageClass (#151),
  overcommit admission (#152), drive affinity (#153), new drives joining the
  pool (#154).
- **NVMe VFIO driver** — a stub today (#167: build or delete).
- Wiring the drive-level RAID extras: on-disk journal, scrub, reassembly (#168).

---

## Status and the issues that matter

**v19.1.2**, full suite green on dev apart from two image tests (#120).

- **The golden is held.** Since v17 the API is closed by default, and the
  engine's clients have to present a token first (#107; stormcentral#30,
  stormcos#89, and one issue per client).
- **Security**: CHAP in the config file is ignored, so a CHAP-configured
  iSCSI target runs open (#164); the optional `ui` pages bypass the token
  (#166).
- **Correctness**: `boot-iscsi` formats its target every run, and a unit runs
  it every boot (#162); `--data-dir` reaches only the volume manager (#163);
  config sections parsed and ignored (#165).
- **Decisions waiting**: VFIO (#167), the StormFS registration target (#170),
  scrub without checksums (#160).

The README is the reference; `docs/` has the design behind each part.
