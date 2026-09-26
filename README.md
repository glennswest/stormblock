# StormBlock

**The block storage engine of the Storm stack, in Rust.** It turns drives and
files into slabs of 1 MiB slots, carves thin copy-on-write volumes out of them
with per-volume redundancy, and serves those volumes as local block devices
(ublk), over NVMe-oF/TCP and over iSCSI. It also builds and boots the disks
stormcos nodes run from: sealed goldens, pallets, GPT disk images, and the
claim a machine makes for its boot image.

One binary, `stormblock` (v19.1.2). It is a daemon, an initramfs boot agent
and a set of offline tools, chosen by subcommand.

```
drives / files / nvme-tcp:// / iscsi://          (the drive layer)
        │
   slabs: 1 MiB slots, a role (system | data), a tier, a failure domain
        │
   global extent map: volume → extents → legs (mirror / parity per volume)
        │
   thin volumes: CoW clones, sealed goldens, filesystem templates (ext4, XFS)
        │
   ublk /dev/ublkbN · NVMe-oF/TCP (shared subsystem, per-volume subsystems)
   · iSCSI (shared target, per-export portals)
        │
   management API :9090 — /api/v1, /v1 (CSI contract), /serve/v1,
   /apis/storage.storm.io/v1, /metrics
```

## Where it runs

| where | how it is started | what it does there |
|---|---|---|
| **a stormcos node** | the stormpump boot unit `00-stormblock` runs `stormblock adopt-ublk --api 0.0.0.0:9090 --data-dir /run/stormblock/engine` | takes over the ublk devices the initramfs engine created (root and the mounted volumes) without them disappearing, restores its state from the `stormblock-state` volume, serves the API and the per-export portals (`/serve/v1`). No shared :3260/:4420 target, no discovery beacon, no cluster in this mode. |
| **the stormcos initramfs** | `/init` (built by `scripts/build-stormblock-initramfs.sh`) runs `boot-claim` then `boot-local`, or `boot-local` on a local slab | claims the machine's image from an appliance (`boothost/<tag>`), attaches it, exports root as `/dev/ublkb0`, and flows it over onto a local disk in the background (`--local-disk`). Boot hooks decide local vs appliance (`docs/boot-hooks.md`). |
| **an appliance (forge)** | `stormblock --config …` (the daemon) | serves goldens and host clones over NVMe-oF/TCP, answers boot claims, builds images and pallets. |
| **anywhere else** | the daemon, or a subcommand | a standalone storage node; `image`, `pallet`, `slab`, `golden`, `attach`, `must-gather` work offline on files and drives. |

## PVCs on stormcos

stormcos has a **built-in PVC driver**: the StorageClass `stormblock`
(provisioner `stormblock.storm.io`). A claim is rounded up to a size class, and
the kubelet (rustkube-node) CoW-clones the sealed, pre-formatted **blank** of
that class — `pvc-ext4j-<MiB>m`, a `/api/v1/fstemplates` template — through
`POST /api/v1/fstemplates/{id}/clone`, attaches the clone over ublk
(`POST /api/v1/volumes/{id}/attach`), and hands it to stormpump to mount. No
mkfs, no copy, no CSI; a missing blank is minted and sealed on first use. The
claim's volume is named `pvc-<namespace>-<claim>`.

CSI exists only for **third-party drivers**: `/v1` is the contract
stormblock-csi speaks (volumes, snapshots and group snapshots, attach, fence
and promote, dual-attach), and orchestrators such as stormstorage use it too.

## What it does

**Storage.**
- **Drives** are raw block devices opened `O_DIRECT` — an io_uring on a thread
  of its own, or `pread`/`pwrite` on the blocking pool where io_uring is
  unavailable (RouterOS) — plus `nvme-tcp://` and `iscsi://` initiators, and
  files (tests and development only). Drives open and close at runtime
  (`POST /api/v1/drives`), carry their identity (serial, WWN) and labels
  (`shelf`, `bay`, `hba` from stormdrive), and can be drained and reported
  failing over HTTP.
- **Slabs** are the unit of storage: 1 MiB slots, a slot table, and optionally
  the volume records themselves, so a slab is self-describing and can be
  adopted by another engine. Each has a **role** — `system` (goldens, replaced
  by an install) or `data` (identity and state, never formatted by an install)
  — a tier, and a failure domain (`site/…/rack/node/hba/shelf/bay/drive`).
- **Thin volumes** allocate on write and give space back on discard (iSCSI
  UNMAP / WRITE SAME, NVMe DSM). **Copy-on-write clones** share extents by
  refcount; a **sealed** volume takes no writes and is what clones come from.
- **Redundancy is per volume** — `none`, `mirror:N`, `raid5:D+1`, `raid6:D+2`,
  each with the failure-domain rung its legs are kept apart at
  (`mirror:2@shelf`). A failed drive's volumes are **rebuilt automatically**,
  most endangered first, several at once, under one byte budget
  (`/api/v1/rebuilds`). See `docs/redundancy.md`, `docs/multi-drive.md`.
- **Drive-level RAID 1/5/6/10** (`/api/v1/arrays`) exists for whole-device
  legs — a RAID 1 across NVMe/TCP legs is how stormstorage builds a
  distributed volume. An API-created array is *dedicated* by default, and a
  volume created with its `array_id` lives only on it.

**Filesystems.** Templates formatted in-process — ext2/3/4 with
[`mkfs-ext4`](https://github.com/glennswest/mkfs.ext4.rs), XFS with
[`mkfs-xfs`](https://github.com/glennswest/mkfs.xfs.rs) — sealed after a check,
and cloned with a fresh filesystem UUID each time. Files are written into ext4
volumes and read out of ext4 and XFS ones in userspace (`fio-ext4`,
`fio-xfs`), with no mount.

**Images and boot.** `import` turns a raw, qcow2, VMDK or OVA image (or an
ISO) into a sealed golden and reads the filesystems inside it. `image build`
lays GPT disks and ISOs out of **pallets** — sealed, versioned sets of boot
members that stormuefi selects at boot (`docs/pallets.md`, `docs/images.md`).
`compose` builds a bootable disk as a map over shared goldens with nothing
written (`docs/composed-disks.md`). **Synonyms** name volumes and re-point the
name at a new version; `boothost/<tag>` is how a machine claims its own boot
image, the one request that needs no token (`docs/auth.md`).

**Serving.** ublk devices for the local node; a shared NVMe-oF/TCP subsystem
with namespace hot-add, and per-volume subsystems; a shared iSCSI target
(CHAP, MC/S, ALUA, thousands of LUNs) and per-export portals. `/serve/v1`
(the serving layer: exports, readiness, tar in/out, raw import, trim) is
mounted by the engine whenever it has a data directory.

**Cluster (optional).** UDP-multicast node discovery, openraft membership,
heartbeats and volume replication behind the `cluster` feature; off unless
`[cluster] enabled = true`. Single-node is the design point: nothing needs a
cluster.

## Building

Build and test **on dev.g8.lo, never on a workstation**: the storage path
(`io_uring`, `ublk`, `O_DIRECT`, `/dev/kmsg`) is `cfg(target_os = "linux")`,
so another OS compiles a different, smaller program. From this repo's
sessions that means `sc-build` after `git push` — it fetches the pushed commit
onto dev as an unprivileged user, builds in a scratch directory and deletes it.
Nothing here needs root.

```bash
sc-build                                           # cargo build && cargo test
sc-build 'cargo build --release --locked'          # what a golden is built with
sc-build 'cargo test --locked --test integration_multidrive'
```

Tests that write files need `TMPDIR` inside the scratch tree
(`mkdir -p tmp && export TMPDIR=$PWD/tmp`). `Cargo.lock` is committed and every
golden is built `--locked`; `cargo update` is a commit of its own.

**Features** (`Cargo.toml`): `default = ["nvmeof", "iscsi", "cluster",
"stormfs-data"]`.

| feature | adds |
|---|---|
| `nvmeof` | the NVMe-oF/TCP target, `--nvmeof-*` flags, `[nvmeof]` |
| `iscsi` | the iSCSI target, `--iscsi-*`/`--chap-*` flags, `boot-iscsi`, `migrate-boot`, `[iscsi]`, `/api/v1/luns`, `/api/v1/sessions` |
| `cluster` | openraft membership, heartbeats, replication, `[cluster]`, `/api/v1/cluster`, `/raft/*` |
| `stormfs-data` | the StormFS data path, `/api/v1/stormfs` (`docs/stormfs-api.md`) |
| `ui` | the old embedded web UI at `/ui` (off since v12.2.0; stormview is the UI) |
| `arm64`, `mikrotik` | profile names only — no code is gated on them |

Profiles:

```bash
cargo build --release --locked --target x86_64-unknown-linux-musl                 # a full node
cargo build --release --locked --target aarch64-unknown-linux-musl \
    --no-default-features --features "mikrotik,nvmeof"                            # RouterOS (NVMe-TCP only)
```

The RouterOS profile serves containers, PVCs and sbregistry over NVMe-TCP;
iSCSI sharing and PXE boot on RouterOS are mkube's. `--no-default-features`
without `nvmeof` does not currently compile (#161).

## Running

With no subcommand, `stormblock` is the storage daemon. It loads
`/etc/stormblock/stormblock.toml` (defaults if the file is missing, an error if
it does not parse), then, in order:

1. starts the volume manager (with `--data-dir` or `[management] data_dir`,
   metadata survives a restart);
2. starts node discovery (unless `discovery_disabled`), the extent GC (`[gc]`)
   and the pool-pressure watcher (`[pressure]`, off by default);
3. opens the drives (`-d` or `[[drives]]`) and **adopts the slabs already on
   them**, their volumes included; with `--raid`, builds an array from them,
   and with `--volume` too, creates volumes on it; with neither, every drive
   becomes a raw NVMe namespace;
4. starts the cluster engine (`[cluster] enabled`) and StormFS registration
   (`[stormfs] enabled`);
5. starts the **iSCSI target** (unless `--no-iscsi`) and the **NVMe-oF/TCP
   target** — the latter only when there is something to export at startup
   (a drive, an array or a volume); with no drives there is no :4420 listener;
6. mounts `/serve/v1` if there is a data directory (`[serve] data_dir`, or
   `<management.data_dir>/serve`) and starts its reconciler;
7. starts the management API on `[management] listen_addr` (HTTPS with
   `tls_cert` + `tls_key`), resolving or minting its token first.

SIGINT or SIGTERM stops it: ublk exports are told to stop first, then the
metadata flush and the ublk teardown run together, bounded at about 13 s so a
unit's `TimeoutStopSec` (30 s in `systemd/stormblock-target.service`) is never
reached mid-teardown. `RUST_LOG` sets the log filter (default `info`); logs go
to stderr.

### Subcommands

| subcommand | what it does |
|---|---|
| `slab format\|grow\|list\|info\|volumes` | format a device as a slab (`--role system\|data`, `--tier`, `--metadata-bytes`), grow a node disk's data half, and read slabs offline — `volumes` lists what a slab says it holds without attaching it |
| `image build\|convert\|inspect\|formats\|lay-node\|local-boot` | build disk images and ISOs out of pallets from a TOML spec (`docs/images.md`); `lay-node` lays a node's disk layout (destroys the drive); `local-boot` copies an ESP and boot pallets onto an installed disk |
| `pallet …` (24 actions) | the pallet lifecycle on drives given with `--drive`: `init-gpt`, `list`, `info`, `status`, `chain`, `verify`, `publish`, `activate`, `successful`, `rollback`, `copy`, `move`, members, `read-only`, `sealed`, `delete`, `prune`, `convert`, `adopt` (`docs/pallets.md`) |
| `golden` | build an ext4 image from tar archives, with no mount or privilege (`--out --size --tar … [--read-only] [--whiteouts] [--fsck]`) — how stormcentral builds service goldens |
| `attach` | attach a slab offline and export (and optionally mount) volumes in it; with no `--volume`, list them |
| `boot-claim` | ask an appliance which image this machine boots (`--boothost URL --tag <service tag>`), print the attach URI |
| `boot-local` | attach local slabs non-destructively, export the boot volume as `/dev/ublkb0` (plus `--image-store`, `--writable`), optionally flow over to `--local-disk`; `--check` validates and exits |
| `adopt-ublk` | take over the ublk devices an earlier engine (the initramfs one) created; `--api` serves the management API too — what stormcos runs |
| `must-gather` | collect what is needed to debug a node into one directory, read-only |
| `boot-iscsi` | provision a partitioned disk on a remote iSCSI target and export it over ublk. **It formats the target every run** (#162): a first-install tool, not a boot path |
| `migrate-boot` | copy boot volumes from an iSCSI slab onto a local disk |
| `ublk`, `migrate` | stubs that print how to do it with a running engine |

`stormblock <subcommand> --help` lists each one's flags.

## Configuration

### Command line (daemon)

| flag | default | |
|---|---|---|
| `-c, --config` | `/etc/stormblock/stormblock.toml` | config file; parsed even when a subcommand runs |
| `-d, --device` | — | drives to open (repeatable); replaces `[[drives]]` |
| `--raid` | — | build an array from the drives: `1`/`raid1`/`mirror`, `5`, `6`, `10` |
| `--stripe-kb` | `64` | stripe size for RAID 5/6/10 |
| `--volume` | — | `name:size[:redundancy]` to create on the array (repeatable) |
| `--data-dir` | — | volume metadata directory. **Only the volume manager sees it**: `/serve/v1`, the token file, templates, synonyms and `/v1` state read `[management] data_dir` (#163) |
| `--iscsi-addr` | `0.0.0.0:3260` | iSCSI listen address (`iscsi`) |
| `--iscsi-target-name` | `iqn.2024.io.stormblock:default` | iSCSI target IQN (`iscsi`) |
| `--chap-user`, `--chap-secret` | — | CHAP for the iSCSI target; both or neither (`iscsi`) |
| `--no-iscsi` | off | do not start the iSCSI target (`iscsi`) |
| `--nvmeof-addr` | `0.0.0.0:4420` | NVMe-oF/TCP listen address (`nvmeof`) |
| `--nvmeof-nqn` | `nqn.2024.io.stormblock:default` | NVMe-oF subsystem NQN (`nvmeof`) |
| `--no-nvmeof` | off | do not start the NVMe-oF target (`nvmeof`) |
| `--reactor-cores` | `0` | per-core reactor threads for the targets; 0 = one per core |

**The target listen addresses, IQN, NQN and CHAP come from these flags only.**
`[iscsi] listen_addr/target_name/chap_*` and `[nvmeof] listen_addr/nqn` in the
file are overwritten by the flags' defaults (#75, #164) — in particular CHAP set
only in the file is **not applied**.

### Environment

| variable | used for | when unset |
|---|---|---|
| `RUST_LOG` | log filter | `info` |
| `STORMBLOCK_API_TOKEN` | the API token, after `[management] api_token`; also `boot-claim --token` | token file, else minted |
| `STORMBLOCK_ADMIN_TOKEN` | the admin token, after `[management] admin_token` | no admin tier |
| `STORMBLOCK_TOKEN_FILE` | where CLI tools look for a local engine's token, before `/etc/stormblock/api_token` and `/var/lib/stormblock/api_token` | those two |
| `STORMBLOCK_NODE`, `HOSTNAME` | node name, after `[management] node_name` | kernel hostname, else `localhost` |
| `STORMBLOCK_ADVERTISED_ADDR` | the address reported to consumers, after `[management] advertised_addr` | derived from the listen address or the default route |
| `STORMBLOCK_CLAIM_GRACE_SECS` | how long a superseded boot clone is kept | `600` |
| `STORMBLOCK_HOST_NQN` | host NQN the NVMe/TCP initiator connects as | `nqn.2024.io.stormblock:initiator` |
| `STORMBLOCK_ENGINE` | `image build --engine` (engine holding `volume:` goldens) | — |
| `STORMBLOCK_SEED_DATA`, `STORMBLOCK_NO_SEED_DATA` | whether `boot-local` flow-over seeds the data half | policy decides |

### The config file

Every section is optional; unknown keys are ignored silently. Sizes take
`K`/`M`/`G`/`T` (base 1024). `stormblock.example.toml` is a commented example.

**`[management]`**

| key | default | |
|---|---|---|
| `listen_addr` | `0.0.0.0:9090` | API address (an IP, not a hostname) |
| `tls_cert`, `tls_key` | — | HTTPS; both or neither |
| `data_dir` | — | durable state (see *Files* below); also the default serve directory and token-file location |
| `api_token` | — | bearer token for every request but the probes and the boot claim |
| `admin_token` | — | if set, destructive verbs need this one instead |
| `token_file` | `<data_dir>/api_token`, else `/etc/stormblock/api_token` | where a minted token is kept (mode 0600) |
| `require_auth` | unset = required | `false` opens the API deliberately (and says so every boot) |
| `node_name` | env, then hostname | this node's name in `/v1` |
| `topology` | `{}` | rungs above the node: `[management.topology] site = …, rack = …` |
| `advertised_addr` | derived | host (or host:port) consumers should dial |
| `discovery_disabled` | `false` | no UDP multicast beacon (239.255.42.99:7447) |
| `beacon_secs`, `peer_stale_secs` | `5`, `30` | discovery timing |
| `ublk_transport` | `true` | offer ublk for a local attach; per request, `"transport": "nvme_tcp"` asks for the network instead |

**`[[drives]]`** `path` — a device, partition, file or `nvme-tcp://`/`iscsi://` URI.

**`[iscsi]`** (`iscsi`) — `max_connections` (`4`, MC/S per session) is used;
`listen_addr`, `target_name`, `chap_user`, `chap_secret` are not (see above).

**`[nvmeof]`** (`nvmeof`) — `export_drives` (`true`: publish each drive as a raw
namespace; set `false` where the drives are the engine's pool) is used;
`listen_addr` and `nqn` are not (see above).

**`[[luns]]`** (`iscsi`) — `id`, `path`, `size` (creates or extends a file),
`readonly` (`false`): LUNs on the shared target at startup.

**`[serve]`** — the serving layer (`/serve/v1`)

| key | default | |
|---|---|---|
| `enabled` | `true` | mount `/serve/v1` |
| `data_dir` | `<management.data_dir>/serve` | wiring and export tables; with neither set, serving is skipped |
| `advertise_addr` | the management advertised address | what consumers attach to |
| `iscsi_enabled` | `false` | serve the legacy shared iSCSI target |
| `portal_base`, `portal_span` | `3261`, `128` | per-export portal ports (3261–3388) |
| `iqn`, `iqn_prefix` | `iqn.2026-08.lo.storm:shared`, `iqn.2026-08.lo.storm` | |
| `nqn`, `nqn_prefix` | `nqn.2026-08.lo.storm:shared`, `nqn.2026-08.lo.storm` | per-volume subsystems are `<nqn_prefix>:vol-<uuid>` |
| `drain_grace_secs` | `120` | a withdrawn export drains this long before its LUN is pulled |
| `reconcile_secs` | `2` | reconciler tick |
| `orphan_export_grace_secs` | `300` | an export naming a missing volume is withdrawn after this; 0 = never |
| `reap_secs`, `reap_apply`, `reap_min_age_secs`, `reap_max_per_pass` | `600`, `true`, `900`, `64` | template debris reaper |

**`[rebuild]`** — `automatic` (`true`), `parallel` (`4`), `extents_in_flight`
(`4`), `max_bytes_per_sec` (`0` = unlimited). Live-adjustable at
`PUT /api/v1/rebuilds/settings`.

**`[gc]`** — `enabled` (`true`), `interval_secs` (`600`), `confirm_passes`
(`true`), `max_reclaim_per_pass` (`4096`), `dry_run` (`false`): the collector
for slab slots no volume maps.

**`[pressure]`** — `enabled` (`false`), `high_water_pct` (`80.0`),
`check_interval_secs` (`60`), `min_slab_bytes` (1 GiB), `max_slabs` (`64`), and
`[[pressure.sources]]` of `kind = "device"` (`path`; adopted if it holds a
slab, **formatted** if not) or `kind = "directory"` (`path`, `slab_bytes`).

**`[cluster]`** (`cluster`) — `enabled` (`false`), `data_dir`
(`/var/lib/stormblock/raft`), `seed_nodes`, `heartbeat_interval_ms` (`1000`),
`heartbeat_timeout_ms` (`5000`), `tls_enabled` (`false`, needs management TLS),
`tls_ca_cert`. `replication_mode` and `replication_factor` are parsed and not
used.

**`[stormfs]`** — `enabled` (`false`), `metadata_url`, `heartbeat_secs`
(`30`), `advertise_addr`: announce this node's volumes to
`<metadata_url>/api/v1/storage/register` (served by stormstorage).

**Parsed and not acted on:** `[[arrays]]` and `[[volumes]]` (validated, so a
bad value still stops startup — use `--raid`/`--volume`, or the API),
`[reactor]` (use `--reactor-cores`) and `[boot]` (#165).

## Ports

| port | what | when |
|---|---|---|
| TCP 9090 | management API, `/metrics`, cluster and Raft RPCs | always (daemon, and `adopt-ublk --api`) |
| TCP 4420 | NVMe-oF/TCP shared subsystem and discovery | daemon, with something to export at startup |
| TCP 3260 | iSCSI shared target | daemon, unless `--no-iscsi` |
| TCP 3261–3388 | per-export portals and per-volume NVMe subsystems (`[serve] portal_base/span`) | when `/serve/v1` is mounted |
| UDP 7447, group 239.255.42.99 | node discovery beacon | daemon, unless `discovery_disabled` |

## Health, readiness and metrics

- `GET /api/v1/health` — public, no locks, no I/O:
  `{"status":"ok","service":"stormblock","version":…,"auth":"required"|"none"}`.
  A booting node asks this of every candidate address before it has a token.
- `GET /serve/v1/health` and `GET /serve/v1/ready` — public. `ready` is 200 only
  when an attach would work now (slab open, metadata restored, targets
  listening, exports wired), else 503 with the blockers.
- `GET /metrics` — Prometheus text, **needs the token**. Slab and drive gauges
  are refreshed at scrape time: `stormblock_slab_{capacity,allocated,free}_bytes{slab,tier}`
  and their `_total`s, `stormblock_drive_{capacity_bytes,healthy,media_errors,temperature_celsius,available_spare_pct,power_on_hours}{drive,serial}`;
  plus `stormblock_api_requests_total{endpoint,method}`, `stormblock_volumes_total`,
  `stormblock_pool_*`, `stormblock_iscsi_sessions_*`, `stormblock_cluster_*`,
  `stormblock_replication_*`, and the serving layer's `stormblockmk_*` gauges.

## The API

Everything is on the management port, behind one bearer-token check
(`docs/auth.md`). The token is required by default: the engine takes
`api_token`, `$STORMBLOCK_API_TOKEN` or the token file, and mints one into the
token file if there is none. Open without a token: `/api/v1/health`, the
`/serve/v1` (and legacy `/mk/v1`) `health` and `ready` probes, and
`POST /api/v1/synonyms/boothost/<tag>/claim`. With an `admin_token`, destructive
requests (any `DELETE`, `…/seal`, writing files or a tar into a volume, a
non-dry-run GC, `trim?apply`, `fsck?repair`) need it.

| surface | for |
|---|---|
| `/api/v1/drives`, `/arrays`, `/slabs`, `/rebuilds` | drives (open, label, drain, health), RAID arrays, slabs and the pool, GC, rebuild queue |
| `/api/v1/volumes` | volumes: create, clone, seal, access, redundancy, tier, restripe, attach, fsck, files, cidata, import, compose, placement |
| `/api/v1/fstemplates`, `/moves`, `/synonyms`, `/releases` | templates and blanks, offline moves, names and boot claims, published releases |
| `/api/v1/pallets`, `/images` | pallets on drives, image build/convert/inspect |
| `/api/v1/exports`, `/luns`, `/sessions`, `/discovery`, `/cluster` | engine exports, iSCSI LUNs and sessions, discovery, cluster |
| `/api/v1/stormfs` | the StormFS data path (`docs/stormfs-api.md`) |
| `/v1` | the CSI / orchestrator contract (`contract/`) |
| `/serve/v1` (and `/mk/v1`) | the serving layer: exports, readiness, tar, raw, trim |
| `/apis/storage.storm.io/v1` | Kubernetes-shaped `volumes`, `slabs`, `drives`, `nodes`, with `?watch=1` |

## Files

In the data directory (`[management] data_dir`; `adopt-ublk --data-dir`):
`volumes.dat` (+ `.bak`), `luns.json`, `exports.json`, `v1_state.json` (+
journal), `fstemplates.json`, `synonyms.json`, `releases.json`, `moves.json`,
`pallet_mirrors.json`, `stormfs.json`, `cluster_identity.json`, `api_token`
(0600) and `serve/wiring.json`. Each slab with a metadata region also carries
its own volumes' records. On stormcos, `adopt-ublk` restores these from, and
captures them back into, the `stormblock-state` volume.

Elsewhere: `/etc/stormblock/stormblock.toml`, `/etc/stormblock/boot.toml`
(`boot-local`), `/run/stormblock/handover.json` (the initramfs engine's record
for `adopt-ublk`), `/var/lib/stormblock/raft` (`[cluster] data_dir`).

## How it ships

stormblock is a **`special`** component in stormcentral (`components/stormcos.toml`):
a bare binary plus `/etc/stormblock/stormblock.toml`, not a stormd service. It
is staged with `stormcentral component stage`, which runs stormcos's
`deploy/build-goldens.sh`. That builds `cargo build --release --locked
--target x86_64-unknown-linux-musl` and lays three goldens:

- **`stormblock`** — read-only ext4, the binary at `/usr/bin/stormblock` and a
  config with `listen_addr = "0.0.0.0:9090"`; placed in `system1`;
- **`stormblock-data`** and **`stormblock-state`** — blanks in `data1`, the
  engine's `/data` and its persisted state.

The same build puts the binary in the initramfs (`/usr/sbin/stormblock`) and
the fedora golden, and every service golden is written by `stormblock golden`.
The container images (`Dockerfile`, `Dockerfile.aarch64`) and
`systemd/stormblock-target.service` are for running it outside stormcos.

## Using it

The examples below leave the token out for brevity. Every request except the
health probes and the boot claim needs it:

```bash
TOKEN=$(cat /var/lib/stormblock/api_token)      # <data_dir>/api_token
curl -H "Authorization: Bearer $TOKEN" http://node:9090/api/v1/volumes
```

### Exporting a volume

A thin/COW volume can be served directly, with no restart. The export
reports the LUN (iSCSI) or namespace ID (NVMe-oF) the initiator must
address:

```bash
# Export a volume — returns {"status":"active","lun_id":0,...}
curl -X POST http://node:9090/api/v1/exports \
  -H 'Content-Type: application/json' \
  -d '{"volume_id":"<uuid>","protocol":"iscsi"}'

# Or attach a LUN directly, letting the next free number be assigned
curl -X POST http://node:9090/api/v1/luns \
  -H 'Content-Type: application/json' \
  -d '{"backing":{"type":"volume","volume_id":"<uuid>"}}'
```

Thin allocation and reclaim are visible on `/metrics` via
`stormblock_slab_allocated_bytes` and `stormblock_slab_free_bytes`.

### Read-write, read-only, and sealed

Two different statements, both enforced, reported separately:

- **`access`** (`rw`/`ro`) is a *setting*, and it moves both ways over a
  volume's life — seeded read-write and published read-only, handed to a
  rescue guest read-only, opened again when it is that volume's turn to be
  written.
- **`sealed`** says what a volume *is*: the master copy clones descend from.
  A sealed volume is read-only whatever its access says, and unsealing does
  not silently make a read-only volume writable.

`writable` on every volume response is the answer to "would a write land":
not sealed **and** not read-only.

```bash
# Take it out of service for writes, and put it back
curl -X PUT http://node:9090/api/v1/volumes/<uuid>/access \
  -H 'Content-Type: application/json' -d '{"access":"ro"}'
curl -X PUT http://node:9090/api/v1/volumes/<uuid>/access \
  -H 'Content-Type: application/json' -d '{"access":"rw"}'

# The setting, and whether writes actually land
curl http://node:9090/api/v1/volumes/<uuid>/access
```

A refused write says which gate closed it rather than reading as a hardware
fault: NVMe answers *namespace is write protected*, iSCSI answers DATA
PROTECT. The same care applies to space — a thin volume with nowhere to put a
write answers *Capacity Exceeded* (ENOSPC at the initiator) rather than a
media error (#92).

### Synonyms — a stable name, re-pointed at a new version

A consumer refers to storage by a name it chose once (`fedora-43`,
`images/nginx`, `node-root`); what that name should mean changes when a new
golden is imported or a version is rolled back. A **synonym** is that binding,
kept apart from the volume — a volume is extents, a synonym is a pointer, so
dropping a name never touches data and deleting the data is refused while a
name still points at it.

```bash
# Name a volume (by id or by its own name), in a namespace
curl -X POST http://node:9090/api/v1/synonyms \
  -H 'Content-Type: application/json' \
  -d '{"namespace":"images","name":"fedora","volume":"fedora-43-x86_64","label":"43"}'

# Publish a new version of the same name — the version bumps
curl -X PUT http://node:9090/api/v1/synonyms/images/fedora \
  -H 'Content-Type: application/json' \
  -d '{"volume":"fedora-44-x86_64","label":"44"}'

# Undo it — a rollback goes *forward* in version, because a client that saw
# the bad publish has to see a change when it is undone
curl -X POST http://node:9090/api/v1/synonyms/images/fedora/rollback
```

**How a client knows whether it changed.** The name is stable, so the version
carries the change. A client remembers the `version` it resolved and asks in
one call:

```bash
# 200 with {"changed": false, …} — still what you hold
curl 'http://node:9090/api/v1/synonyms/images/fedora?since=7'

# The HTTP-native spelling: 304 Not Modified, or 200 with the new target
curl -H 'If-None-Match: "7"' http://node:9090/api/v1/synonyms/images/fedora
```

Resolution also returns what the target *is* right now (size, `sealed`,
`access`, `role`), so resolving a name and asking about the volume is one
round trip. A target may be a volume on this node or storage another node
serves (`"uri": "nvme-tcp://host:4420/<nqn>?nsid=1"`) — resolution says which,
so a caller learns it is being sent off-node rather than discovering it when
the I/O is slow. A synonym is usable wherever a volume is named by id or name;
the volume manager is asked first, so a synonym can never shadow a real
volume.

A synonym does not pin: it resolves to whatever it points at *now*. A consumer
that must not be moved under its feet records the `(version, target)` it
resolved and compares at its next start.

**Writes go to a clone, never to the golden.** A golden is the master copy and
is sealed, so what a consumer wants from a name is not the volume it resolves
to but a copy-on-write clone of it — its own filesystem identity, its own
divergence, costing nothing until written. `claim` is that in one call, and it
binds a name to the clone in the caller's own namespace, which is how one
golden ends up behind many consumers each holding a name of their own:

```bash
curl -X POST http://node:9090/api/v1/synonyms/images/fedora/claim \
  -H 'Content-Type: application/json' \
  -d '{"namespace":"tenant-a","name":"root"}'
```

Claiming again re-points that tenant's own name at its new clone. A claim of
an unsealed target is refused (`unsealed_ok=true` to override): a golden is
sealed, so an unsealed target is something still being written, and a clone of
it is a snapshot of a moving thing — the caller's consistency question, and
one they should have to ask out loud.

The same rule is enforced where it is cheapest to say so: a **read-write
attach of a sealed volume is refused**, before a guest boots onto storage that
will not take its writes, and the refusal names the way forward (clone it, or
attach `mode=ro`).

**A claim answers with somewhere to attach.** A volume id is not something
firmware can act on: a machine doing an NVMe/TCP boot would learn that a volume
exists and still have to ask where it is — a second request, from a client whose
whole state machine is "get an address, attach it, boot", with a window in
between where the claim is held and nothing is being served. So the claim
carries the tuple:

```json
{
  "claimed_from": {"synonym": "boothost/C2NR0Q2", "version": 1},
  "volume": {"id": "42e3fbba-…", "name": "boothost-C2NR0Q2"},
  "attach": {
    "protocol": "nvme-tcp",
    "address": "192.168.31.202", "port": 4420,
    "nqn": "nqn.2026-09.lo.g16:stormcos", "nsid": 3,
    "uri": "nvme-tcp://192.168.31.202:4420/nqn.2026-09.lo.g16:stormcos?nsid=3"
  }
}
```

An export that already exists is reused rather than a second one minted: **the
nsid is part of the address**, and issuing a fresh one for a volume that already
has an address changes it under whoever is holding the old one. The address
reported is the advertised one — a wildcard listen address tells a caller
nothing, and loopback is worse than nothing.

### Naming a machine's image by its service tag

Which image a machine boots is a fleet decision, and it belongs next to the
images rather than in the network. A synonym in a `boothost` namespace, keyed on
the machine's service tag, is that decision written down:

```bash
# This machine runs 10.22.
curl -X POST http://forge:9090/api/v1/synonyms \
  -d '{"namespace":"boothost","name":"C2NR0Q2","volume":"stormcos-sno-10.22","label":"10.22"}'

# At boot: one request, and the answer is bootable.
curl -X POST http://forge:9090/api/v1/synonyms/boothost/C2NR0Q2/claim
#   → a copy-on-write clone of the sealed golden, costing nothing until written
#   → and the nvme-tcp:// URI that reaches it

# Move that machine to a new image, or put it back.
curl -X PUT  http://forge:9090/api/v1/synonyms/boothost/C2NR0Q2 -d '{"volume":"stormcos-sno-10.23"}'
curl -X POST http://forge:9090/api/v1/synonyms/boothost/C2NR0Q2/rollback
```

**Why not DHCP.** DHCP can carry a pointer — a `root_path`, a boot file — and
it is the wrong home for one. A lease is not a source of truth; the mapping
would live in the network layer while the thing it names lives here, which is
two places to change and a way for them to disagree. It does not survive a
change of boot method, because firmware with an NVMe/TCP boot extension takes
its target from its own configuration rather than from a DHCP option. And a
DHCP option is one static string, so it cannot answer the same name with
different locations — which is what load balancing across appliances needs.
DHCP's job stays the one it is good at: handing the machine an IP.

A service tag is the right key because it is the machine, not a NIC: it
survives a network card being swapped, which a MAC does not.

### Where a volume is placed: `system` or `data`

A volume lives in one half of the node's mutable storage — `system` (goldens,
which an install replaces wholesale) or `data` (identity and state, which no
install path formats). Name it on `POST /api/v1/volumes` or on
`/api/v1/volumes/import` with `"role": "system"|"data"`; leave it out and the
node decides, which matters on a box whose drives carry *only* data slabs — a
registry node, whose content is meant to outlive a rebuild. There the default
was a system slab that does not exist, and every write failed at its first
allocation while reads went on working (#92, #93).

### Preformatted filesystem templates — mkfs once, clone forever

> **A template is a volume that has been sealed (#76, v12.0.0).** Lineage
> (`parent`), `sealed` and the filesystem record (`fs`) live on every
> volume. Sealing is a state — `POST /api/v1/volumes/{id}/seal` — not a
> second object; cloning is `POST /api/v1/volumes/{id}/clone` and always
> stamps a fresh filesystem UUID; `GET /api/v1/volumes/{id}/lineage` walks
> the ancestry; and `from_template` accepts any sealed volume by id or name.
> The `/api/v1/fstemplates` surface below is the same model with a name and
> a clone count kept beside it.
>
> **A template's format always finishes (#141, v18.3.0).** A create runs to
> the end even if its caller stops waiting, and a format an engine was
> stopped in the middle of is finished at the next start. `formatting: true`
> on a template means the engine is laying it down.
>
> **Nothing is minted ahead of a claim (#137, v18.0.0).** A mint is a
> sub-millisecond snapshot, one superblock write with its flush, and one
> metadata persist. A clone of a sealed blank is verified by reading its stamp
> back, not by an fsck of its own, because the blank was checked when it was
> sealed. So `claim` mints on demand and no volume on a node is one nobody
> asked for; `examples/claim_timing.rs` and `ci-claim-timing.sh` measure it.

### Volumes and images (#138)

Goldens stay volumes underneath; what a volume *is* and whether anything is
using it is on every entry of `GET /api/v1/volumes`, so a console can show a
Volumes view of what running containers and VMs use and an Images view of the
rest without naming conventions:

- **`kind`**: `volume` (anything unsealed), `golden`, `blank` (a template's
  sealed volume, or a blank an image shipped), `media` (a whole-disk image or
  ISO), `snapshot` (a `/v1` snapshot, i.e. a VolumeSnapshot) or `template` (a
  template's scratch volume).
- **`in_use`** and **`attachments`**: every way the engine is serving it right
  now. That covers ublk devices with the mount point, boot devices the engine
  adopted, NVMe namespaces on the shared subsystem, per-volume subsystems, the
  serving layer's wiring, exports and iSCSI LUNs. The delete guards use the
  same answer.
- **`consumer`**: the volume's owner (`PUT …/owner`: a PVC, a VMI, …), else the
  mount a ublk device carries.
- Filters: `?kind=volume|golden|blank|media|snapshot|template` (comma-separated),
  `?kind=image` for everything but `volume`, `?in_use=true|false`,
  `?unowned=true`. With no filter the listing is what it always was.

```
GET /api/v1/volumes?kind=volume&in_use=true   # the Volumes view
GET /api/v1/volumes?kind=image                # the Images view
```

Formatting a filesystem is the expensive part of provisioning a volume: a
256 MiB ext4 laid down over the network takes ~20 s, while cloning a sealed
template is effectively instant and starts at near-zero allocation. So format
once, seal, and clone:

```bash
# Format + seal in one call — the engine writes the filesystem locally.
# The default is what `mke2fs -t ext4` produces.
curl -X POST http://node:9090/api/v1/fstemplates \
  -H 'Content-Type: application/json' \
  -d '{"name":"ext4-256m","size":"256M","label":"storm"}'

# A variant for a consumer that predates some of it, told apart by name.
# `features` is an `mke2fs -O` list; `journal` and `fs` (ext2/ext3/ext4) are
# the other two knobs.
curl -X POST http://node:9090/api/v1/fstemplates \
  -H 'Content-Type: application/json' \
  -d '{"name":"ext4-plain-256m","size":"256M","journal":false,
       "features":"^64bit,^metadata_csum"}'

# Clone one per consumer — a snapshot plus a fresh filesystem UUID
curl -X POST http://node:9090/api/v1/volumes \
  -H 'Content-Type: application/json' \
  -d '{"name":"pvc-1","from_template":"ext4-256m"}'

# An XFS blank (the built-in PVC driver's blanks are ext4, pvc-ext4j-<MiB>m;
# XFS is for whoever asks for it)
curl -X POST http://node:9090/api/v1/fstemplates \
  -H 'Content-Type: application/json' \
  -d '{"name":"pvc-xfs-1t","size":"1T","fs":"xfs"}'

# Check any volume's filesystem; ?repair=true corrects what it can (ext only:
# an XFS volume is walked and reported, never repaired)
curl -X POST http://node:9090/api/v1/volumes/<uuid>/fsck
```

**Every feature is a per-template choice**, expressed in `mke2fs` terms rather
than re-invented as flags: a filesystem kind (`ext2`/`ext3`/`ext4`), a journal
switch, and an `-O` list. For the journal:
RouterOS cannot replay a journal, so one that ever goes dirty there leaves the
filesystem read-only permanently, while a Linux host or VM wants the crash
consistency.

**XFS as well as ext4** (#147): `"fs": "xfs"` formats with
[`mkfs-xfs`](https://github.com/glennswest/mkfs.xfs.rs), the same filesystem
`mkfs.xfs` 6.15 writes (v5, CRCs, finobt, rmapbt, reflink, bigtime), from 300 MB
up. The log it zeroes is a discard on a thin volume, so a 2 GiB XFS blank
costs 6 MiB. Sealing checks what can be checked without `xfs_repair`: the
superblock's CRC and flags, then the whole tree walked by
[`fio-xfs`](https://github.com/glennswest/fio.xfs.rs) with every v5 checksum
checked. The crate has no checker of its own yet. Not for XFS: `features`
(an `mke2fs` list), `journal: false` (XFS always has a log), and `seed`
(`fio-xfs` reads, it does not write yet). A claim or clone of an XFS blank
gets a new UUID the way `xfs_admin -U` gives one: `sb_uuid` changes, the old
UUID stays in `sb_meta_uuid` (the one the metadata blocks carry), and the
`META_UUID` feature is set, one sector per allocation group. That matters more
than for ext4, because the kernel refuses to mount two XFS filesystems with
one UUID at all. `ci-xfs-verify.sh` checks blanks and claims with
`xfs_repair -n`, `blkid` and `xfs_db` on dev.

**Every clone is stamped with its own filesystem UUID.** Without that, two
clones of one template collide on mount-by-UUID and in the blkid cache the
moment both are attached to one host. It happens here because every consumer
clones *through* the engine — a UUID stamped in a layer above would miss the
clones that layer never touches. The default profile carries
`metadata_csum_seed`, so checksums are seeded from the superblock rather than
the UUID and the stamp stays a single write; a filesystem with `metadata_csum`
and no seed has one pinned from its current UUID first, as `tune2fs -U` does.

**Every clone is checked before it is handed out**, and a clone that does not
pass is discarded rather than handed over (`"verify": false` to skip). The same
check is available for any volume at `POST /api/v1/volumes/{id}/fsck` —
RouterOS has no fsck and cannot cleanly unmount a network disk, so a volume it
leaves dirty has nowhere else to be repaired.

Sealing runs a real fsck and refuses a filesystem a consumer could not mount
read-write — `VALID_FS` clear, `ERROR_FS` set, journal replay pending, orphan
cleanup pending, or anything the check turns up. A template that seals dirty
fails much later, inside a container, as `Read-only file system`. Pass `?force=true` to override, and
`{"format": false}` at create time to have an initiator lay the filesystem down
over an export instead, then `POST /api/v1/fstemplates/{id}/seal`.

**A template is one volume, and deleting it takes that volume with it.** The
scratch volume a template is formatted on is dropped the moment it is sealed —
the sealed snapshot holds its own refcounted extents and does not depend on the
volume it was taken from — and `DELETE /api/v1/fstemplates/{id}` purges what is
left unless asked not to (`?purge=false`). A create that fails anywhere, at
format, at seed or at seal, leaves nothing behind, so retrying a name does not
cost two more volumes each time. For a node that already accumulated debris,
`GET /api/v1/fstemplates/orphans` lists volumes named like a template's that no
template claims **and nothing on this node is serving**, and `DELETE` on the
same path reclaims them; clones are named by their consumer, so they are never
in that set.

Formats do not queue. No lock is held across a format, a check or a stamp, and
the formatter takes `&self` so one format fans out across block groups.
Measured on a Fedora 6.17.1 host: one 256 MiB template formats and seals in
**50 ms**, four concurrently in **79 ms** total; a clone costs 54–86 ms
including its verification fsck.

Verified against real consumers rather than only against itself: clones
exported over iSCSI and attached with open-iscsi are read by `blkid`, pass
`e2fsck -fn`, mount read-write four at a time, take writes, unmount and check
clean again (`ci-fstemplate-verify.sh`); and a clone attached to RouterOS over
NVMe-TCP takes writes, with the disk table's free-block and free-inode counts
moving to match.

### Clone-per-consumer, reset on restart

Untar a golden image **once**, then clone it per consumer — a clone copies
no data, it shares the source's extents and diverges copy-on-write. When a
consumer restarts, reset it instead of deleting and re-cloning: the volume
keeps its id and only the extents it actually wrote are touched, so the cost
tracks divergence rather than image size.

```bash
# Clone the golden image for a new container instance
curl -X POST http://node:9090/v1/volumes \
  -H 'Content-Type: application/json' \
  -d '{"name":"container-1","size_bytes":536870912,
       "source":{"kind":"volume","id":"<golden-uuid>"}}'

# On restart: squash divergence, back to the golden image
# → {"freed_extents":3,"restored_extents":3,"shared_extents":47}
curl -X POST http://node:9090/v1/volumes/<clone-id>/reset
```

Reset is refused while the volume is attached, since its contents cannot
change under a live host.

### Multiple connections per iSCSI session (MC/S)

An iSCSI session can carry several TCP connections, so an iSCSI-only consumer
is not limited to one stream. The target offers up to `max_connections`
(default 4) and negotiation takes the **lower** of that and what the initiator
asks for — an initiator that wants one connection still gets one, so raising
the cap cannot change what an existing consumer sees.

```toml
[iscsi]
max_connections = 4
```

A login carrying a non-zero TSIH adds a connection to that session rather than
starting a new one, and the ISID has to match as well: the two identify the
session together, and a TSIH on its own is guessable. One connection closing
now removes **that connection**, not the session — the session ends when its
last connection does.

The part that had to be right first: **CmdSN belongs to the session, StatSN to
the connection** (RFC 7143 §4.2.2.1). One shared command window is handed to
every connection that joins, so an initiator with two paths is told one
consistent thing about its own flow control. Tracking it per connection — which
is what single-connection code could get away with — would have each connection
advertising a different window for the same session.

`GET /api/v1/sessions` reports the per-session connection count. NVMe-oF reaches
parallelism through queue count instead, so this matters for consumers that are
iSCSI-only.

### Moving a volume — re-home, or shrink

Growing is a resize (above). Shrinking is not, and cannot be: the extents past
the new end are freed immediately and **xfs cannot shrink into that**, so
`resize` refuses it. The only safe form of "make this smaller" is to build a
new, smaller filesystem and copy the contents across — which is what a move is,
and why the copy is at the filesystem level rather than the block level. A
block-level clone would faithfully reproduce the size being escaped.

```bash
# Copy and verify. Nothing is destroyed — all three volumes exist afterwards.
curl -X POST http://node:9090/api/v1/moves \
  -H 'Content-Type: application/json' \
  -d '{"volume_id":"<uuid>","target_name":"var-small","target_size":"24G"}'

# → {"move":{"state":"ready_to_commit","verified":true,
#             "target_volume_id":"…","rollback_snapshot_id":"…"}}

# Repoint whatever used the source at the target, then:
curl -X POST http://node:9090/api/v1/moves/<move-id>/commit   # source goes
curl -X POST http://node:9090/api/v1/moves/<move-id>/abort    # target goes instead
```

**Two calls, because the ones people skip when they script this are the ones
that matter.** The first snapshots the source (copy-on-write, so it costs
metadata and doubles as the way back), creates the target, formats it to match,
streams the contents across and *fsck's the result* — and stops. The source is
untouched. Only `commit` deletes it, and only after the caller has moved its
consumer over, which is the one thing the engine cannot know.

The copy is streamed straight from one filesystem into the other with no scratch
file and no whole-archive buffer, so a 64 GiB volume holding 2 GiB moves 2 GiB.
It goes through tar rather than a hand-rolled tree walk, which is what preserves
modes, ownership, timestamps, symlinks, hard links, device nodes and extended
attributes — SELinux labels among them, without which a rootfs stops booting.
Both ends count what crossed independently and a mismatch in any category fails
the move.

A move is **offline by contract**: an exported or attached volume is refused,
because anything written during the copy would not be in the target. It is
*restartable* rather than resumable — an interrupted move is discarded and
re-run, which costs time and never data.

### Growing the pool on disk pressure

Thin volumes overcommit, so a node can run out of **physical** space while every
volume still reports free virtual space — invisible until writes start failing,
and confusing when they do. The pool watches its own utilisation and adds a slab
when it crosses a high-water mark:

```toml
[pressure]
enabled = true
high_water_pct = 80          # add capacity at or above this
check_interval_secs = 60
min_slab_bytes = 1073741824  # smallest slab worth adding
max_slabs = 64               # backstop against a bad source list

# Where capacity may come from, claimed in order. Nothing is ever discovered.
[[pressure.sources]]
kind = "directory"           # creates a new backing file — never overwrites
path = "/var/lib/stormblock/grow"
slab_bytes = 8589934592

[[pressure.sources]]
kind = "device"              # claimed whole; adopted if it already holds a slab
path = "/dev/sdb2"
```

**Grow on pressure, never preallocate** — preallocating to the virtual size
gives back everything thin provisioning saved. The pool grows one slab at a
time, when it is actually needed. A slab is added rather than enlarged because a
slab's data region starts past a slot table sized at format time; growing one in
place would move every byte of data.

Sources are configured and never discovered: formatting the wrong device is
unrecoverable, and "it had no filesystem on it" is not consent. A `directory`
source only ever creates new files, which is also how to grow into the unused
tail of the node's own disk — mount the spare space and point it there. A
`device` source that already carries a readable slab is **adopted with its
data**, not reformatted, so a source claimed before a reboot comes back intact.

```bash
# How full is this node, and what is the watcher doing about it?
curl -s http://node:9090/api/v1/slabs/pool
```

Pressure with every source claimed is logged at error and reported as
`sources_exhausted` — the pool is under pressure and the engine is out of ways
to answer it, which is not a state to discover late. `stormblock_pool_used_pct`,
`stormblock_pool_free_bytes` and `stormblock_pool_slabs_added_total` carry the
same story to Prometheus.

### Growing a volume online

A volume grows in place, and the block device grows with it. `POST
/v1/volumes/{id}/expand` moves the volume's virtual size and then tells the
kernel, via `UBLK_U_CMD_UPDATE_SIZE`, so `/dev/ublkbN` reports the new capacity
and `xfs_growfs` has somewhere to grow into. **No quiesce**: resizing has no
consistency point to capture, and stalling a live `/var` to make it bigger
turns a day-2 operation into an outage. The capability is negotiated at device
creation and degrades to "the volume grew, the device did not" — said loudly —
on a kernel older than 6.12.

Shrinking is not the same operation and is not offered as one. A smaller size
comes back `409`: the extents past the new end would be freed immediately, and
xfs cannot shrink at all, so a shrink of a mounted volume destroys live data
with nothing to undo it. `VolumeManager::shrink_volume` exists for a caller
that means it. Moving a volume onto a smaller one, with its data, is a copy —
a different operation.

### Releases: available, or archived

A release names a volume; it never copies one, so the download streams out of
the image that was built. That also means a release can outlive its volume, and
until #106 nothing said which had: eight versions stood in the index with
manifests, digests and download links for bytes that had been reclaimed.

Two answers, in both directions:

- **A volume a published release names cannot be deleted.** It comes back
  `409`, naming the release, the same way a volume with a live export does —
  `DELETE /api/v1/releases/{version}` first, which withdraws the promise
  deliberately.
- **A release whose volume is gone reports `state: "archived"`**, and
  `GET /api/v1/releases/{version}/image.img` answers **410 Gone** rather than
  a 404 that reads as "no such version". Its manifest and notes still answer:
  the record of what a version contained is worth keeping after the bytes are
  not. The browser index drops the download link rather than offering one that
  cannot be taken.

The state is derived on every read from whether the volume resolves, never
stored — a stored flag would be a second copy of a fact the volume manager
already holds, wrong exactly when it mattered.

### Reading a slab without attaching it

`stormblock slab list` and `slab info` answer from a slab's header without a
daemon, a reactor or ublk. `slab volumes` does the same for what is *in* it:

```
$ stormblock slab volumes /dev/sda2
/dev/sda2: volume boot-cp-01 (2.1 GB, 540 slots, sealed) 88d5da3f-…
/dev/sda2: volume image-store-stormcos-0.1.0 (8.4 GB, 2100 slots) 3f2b…
```

The records are on the device, in the region the header's `meta_offset` and
`meta_size` name, and until #108 the only way to read them was to attach the
slab — which needs the kernel module and root, and makes the volume live. That
is the wrong thing to do while you are still deciding whether this is a disk to
touch at all: an initramfs asking "does this slab actually hold the volume the
loader entry names" must be able to ask without committing.

Read-only in the strict sense: the device is opened `O_RDONLY`, and a path that
does not exist is *not created* — the ordinary door creates what it cannot
find, which turned `slab volumes /dev/sdz` into a zero-byte `/dev/sdz`
reported as "not a slab".

Three answers, deliberately distinct, because a boot decision turns on which:

| output | means |
|---|---|
| `: volume <name> (…)` | positive evidence, greppable, in `slab list`'s shape |
| `slab <id> holds no volumes` | the slab can say, and says it is empty — formatted and never filled |
| `slab <id> keeps no volume metadata` | the slab cannot say; its records live wherever `rd.stormblock.meta=` points |

Every slab this engine formats reserves a region for that record — `slab
format`, `POST /api/v1/slabs` and the pool-growth path alike, sized from the
device. It used to be `data` slabs alone, on the reasoning that outliving
whatever formatted it is the point of that role; `image build` has always
given both roles one, so a disk formatted by hand and a disk the builder laid
down were not the same kind of thing, and the hand-formatted one could only be
read by attaching it. `--metadata-bytes 0` (or `metadata_bytes: 0`) formats a
slab that deliberately keeps no record of itself.

### An installed disk boots on its own

A flow-over lays more than the two slabs now. It also leaves a boot area at the
front of the drive. Once the goldens have moved, the engine copies the image's
ESP (stormuefi) and its boot pallets into that area, so a cold boot no longer
needs the network. The table and the ESP are written in the drive's own sector
size, which firmware requires. Release N sits at priority 14 with N-1 kept below
it, so an attached image (priority 15) still wins. See
[docs/images.md §2b](docs/images.md); `ci-local-boot-verify.sh` boots the
result under OVMF.

### Booting: who decides where

The initramfs decides where a node boots from, and `docs/boot-hooks.md`
describes how to take that decision over: any executable in
`/etc/stormblock/boot.d` is asked first, and `/init` honours `boot-local` or
`ask-appliance`. With no hook installed the built-in probe decides exactly as
before — and that probe now uses `slab volumes`, so it works on the partition
a loader entry names rather than only on a whole disk with a GPT.

### Stopping a node

Every step of shutdown is bounded, and that is a correctness property rather
than politeness. The engine flushes volume metadata (10 s, then it carries on —
each slab keeps its own copy of the record, which is what adoption reads) and
stops its ublk devices (10 s), the two together rather than in series.

The ublk half is the one that bites. An export's queue threads sit in
`io_uring_enter` waiting for the kernel; a process that exits without STOP_DEV
and DEL_DEV leaves them there, and **a thread stuck in the kernel cannot be
reaped** — systemd then finds a process it cannot kill and every subsequent
restart ends in `failed` mode. So a unit's `TimeoutStopSec` must stay above the
engine's own budget (~13 s), or SIGKILL lands in the middle of a teardown and
makes exactly that.

## Not built, or not wired

What earlier docs described and the code does not do, each with its issue:

- **NVMe userspace (VFIO) driver** — a stub; NVMe drives are served through
  the kernel, opened `O_DIRECT` (#167).
- **Drive-level RAID extras** — the write-intent journal is in memory only,
  and journal recovery, scrub, array rebuild (other than RAID 1 resync on
  `add_member`) and reassembly from superblocks are not wired; RAID 6 Q parity
  is scalar (#168). Per-volume redundancy is the rebuild path in use.
- **io_uring zero-copy send, the StormFS shared-ring IPC server**: code with
  nothing starting it; `arm64`/`mikrotik` gate nothing (#169).
- **Config the daemon ignores** — see *The config file* (#163, #164, #165).
- **`boot-iscsi` as a boot path** — it formats every run (#162).
- **The `ui` feature's pages are outside the token check** (#166).
- **Scrub** of mirror legs and parity on a schedule (#160), **erasure coding
  beyond P+Q** (#159), metadata at 40 PB a node (#155–#158), drive affinity,
  overcommit and StorageClass policy for claims (#151–#154).
- **"No C dependencies"** was never true: TLS brings in `aws-lc-sys` and
  `ring`.

## Docs

| | |
|---|---|
| `docs/auth.md` | who may call a node's API; the boot claim; host goldens |
| `docs/redundancy.md` | per-volume redundancy, failure domains, health, resync, automatic rebuild, drain, whole-disk goldens and import |
| `docs/multi-drive.md` | pools, placement, a drive's life, dedicated arrays, what a claim should ask for (part design) |
| `docs/pallets.md` | the pallet format and lifecycle (§2.6–§2.8 are design) |
| `docs/images.md` | building disk images and ISOs, local boot |
| `docs/composed-disks.md` | per-node disks composed from shared goldens |
| `docs/boot-hooks.md` | how the initramfs decides local disk vs appliance |
| `docs/stormfs-api.md` | the StormFS data-path routes |
| `docs/layering.md` | engine / serving / profile, and why maps reference slabs by UUID |
| `docs/metadata-scale.md` | allocation metadata at 40 PB a node (measurements and design) |
| `docs/m0-baseline.md`, `docs/protocol-overhead.md` | dated measurements |
| `contract/` | `/v1` wire fixtures shared with stormblock-csi |
| `docs/history/` | superseded design: the v0.1 spec, the LinuxBoot proposal, the placement note, the August deck |
| `CHANGELOG.md`, `CLAUDE.md` | what changed, and the work plan |

## Source layout

92k lines of Rust in `src/`, 13.7k in `tests/`, about 870 tests.

```
src/mgmt/       19.7k  management API (axum): every /api/v1 surface, /v1, kube resources,
                       auth, config, metrics, discovery, ublk exports, web UI (feature ui)
src/volume/     17.7k  thin volumes, GEM, redundancy (mirror/parity legs), snapshots and
                       clones, metadata, synonyms, chunks/versions (StormFS), GC, pressure,
                       relocation, composition
src/drive/      10.4k  BlockDevice; O_DIRECT block devices (io_uring or blocking pool),
                       nvme-tcp:// and iscsi:// initiators, files; slabs and the registry;
                       ublk; handover; SMART; identity
src/image/       7.5k  image build (GPT, FAT, ISO, qcow2/VHD/VMDK), import decoders,
                       node layout, local boot
src/target/      6.7k  NVMe-oF/TCP and iSCSI targets, per-core reactor
src/fs/          5.6k  templates, ext4 and XFS seams, disk identity, files, image survey
src/serve/       4.0k  the serving layer (/serve/v1): wiring, reconciler, readiness, reaper
src/pallet/      3.9k  pallet format writer, GPT, store, manager, selection
src/placement/   2.9k  failure domains, placement, drain moves, rebalance
src/raid/        2.7k  drive-level RAID 1/5/6/10, parity
src/cluster/     2.6k  openraft membership, heartbeat, replication (feature cluster)
src/*.rs         8.8k  main.rs (CLI, daemon, subcommands), rebuild, drain, state, boot,
                       boot_iscsi, migrate, stormfs registration, http client
crates/pallet-format   the no_std pallet reader stormuefi links
```

## Storm components it talks to

| component | how |
|---|---|
| [stormcos](https://github.com/glennswest/stormcos) | ships the engine (`adopt-ublk` under stormpump), its goldens, and the initramfs that runs `boot-claim`/`boot-local` |
| [rustkube](https://github.com/glennswest/rustkube), rustkube-node | the built-in PVC driver: clones blanks and attaches them over ublk through `/api/v1` |
| [stormblock-csi](https://github.com/glennswest/stormblock-csi) | the CSI driver for third-party use, over `/v1` |
| [stormblock-registry](https://github.com/glennswest/stormblock-registry) (sbregistry) | builds blanks and goldens, posts image specs to `/api/v1/images/build` |
| [stormbootx](https://github.com/glennswest/stormbootx), [stormuefi](https://github.com/glennswest/stormuefi) | UEFI: claim `boothost/<tag>` and attach it over NVMe/TCP; boot a pallet (the reader is `crates/pallet-format`) |
| [stormdrive](https://github.com/glennswest/stormdrive) | registers and labels drives (`shelf`, `bay`, `hba`), reports their health |
| [stormstorage](https://github.com/glennswest/stormstorage) | distributed volumes: RAID 1 over NVMe/TCP legs through `/v1` and `/api/v1/arrays` |
| [stormcentral](https://github.com/glennswest/stormcentral) | stages the component and its goldens |
| [zeroboot](https://github.com/glennswest/zeroboot) | a boot hook the initramfs asks first |
| [StormFS](https://github.com/glennswest/stormfs) | the data path (`docs/stormfs-api.md`) and the ring-IPC client |

## License

TBD
