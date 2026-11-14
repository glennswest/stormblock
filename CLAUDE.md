# StormBlock Development Guide

## Project Overview
Pure Rust enterprise block storage engine. Turns raw NVMe/SAS drives into network-accessible volumes over NVMe-oF/TCP and iSCSI. Part of the Storm ecosystem (StormBlock, StormFS, StormForce, StormOS).

## Design Principle: Single-node first, scale-out later
StormBlock must be fully functional as a **standalone single-node** storage engine — no cluster requirement. A single node handles its own drives, RAID, volumes, and exports independently. Clustering (replication, Raft) is layered on top and strictly optional. New nodes can be added to an existing deployment at any time without disrupting running nodes.

## Build and test with sc-build, on a fresh build VM

**Every `cargo build`, `cargo test`, `cargo check` and every image build runs on
a build VM, through `sc-build` after `git push`** — never on the session host,
never as root, never "just to check quickly". `sc-build` clones a fresh VM
(dev.g8.lo is retired, stormcentral#107), fetches the pushed commit onto it as
the unprivileged `stormbuild` user, builds it on a drive of its own and
destroys the VM when the job ends; nothing is kept. `sc-build 'cmd'`
runs any command; `SC_BUILD_NO_ISSUE=1` keeps an exploratory run from filing a
`build-failure` issue. Builds queue for a slot, so give it a generous timeout.

The storage path — `io_uring`, `ublk`, `O_DIRECT`, `/dev/kmsg`, `mlockall` — is
`cfg(target_os = "linux")`, so another OS compiles a different, smaller
program; on the old macOS workstation `cargo test` ran 258 tests against 303 on
Linux. Build where the code runs.

```
commit  →  push  →  sc-build  →  read the result
```

Tests that create files need `mkdir -p tmp && export TMPDIR=$PWD/tmp` in the
scratch tree: dev's `/tmp/stormblock-*` directories are root-owned from old
builds (stormcentral#61). No known-red tests since #134 (the LUN table written behind attaches) and
#173 (a first write zeroes its slot without writing it).

The test container is its own repo since #371: glennswest/stormblock-test
(builds against this repo's main; `stormcentral test run stormblock short` on
a test machine). A change to what it relies on (`http::Client`, the NVMe/TCP
initiator `BlockDevice`, `.cargo/config.toml`, `admin_token_file`, the binary)
needs a stormblock-test issue.

## Build
```bash
sc-build 'sh check.sh'                                              # the routine check (#357)
sc-build 'cargo build --locked --profile dist --target x86_64-unknown-linux-musl'  # meant for goldens
sc-build 'cargo check --locked --features cluster'                   # the Raft layer, opt-in
```
No RouterOS check: nothing ships for RouterOS (owner, 2026-09-28).
The golden itself is built by stormcos's `deploy/build-goldens.sh` with
`--release`, not `--profile dist`: stormcos#169.

**Tests (#209, 2026-09-28).**
- `tests/it/` is **one** in-process integration-test binary; each former file
  is a module (`tests/it/main.rs`). Run it with **cargo-nextest**, which gives
  every test its own process, so tests that bind ports or set globals cannot
  interfere. `cargo nextest run` builds the lib, bins and tests, not examples.
- `tests-runtime/` holds the **runtime tests**: they drive the built binary
  (`STORMBLOCK_BIN`), a kernel device or privileges. `ci-runtime-tests.sh`
  (part of the routine check, #222, #357) builds the commit's binary and runs every
  one that needs no root; the `#[ignore]`d ones are listed, not run:
  `ublk_resize` (root, ublk_drv: #342), `external_iscsi`/`iscsi_blockdev` (an
  external iSCSI target: #343). The build VMs have no cargo-nextest
  (stormcentral#534): `cargo install cargo-nextest --locked` in the job.
- `src/main.rs` is a wrapper; the command line is `stormblock::cli`, compiled
  and tested once as part of the library.

**Main is the next release; work in progress never is its head** (#492:
12.39 staged c2fdcce, `WIP(#474)`). Pause on a `wip/` branch, never on main.
`.githooks/pre-push` refuses a push that leaves main's head `WIP…`: enable it
in every checkout with `git config core.hooksPath .githooks`
(`STORMBLOCK_ALLOW_WIP_MAIN=1` overrides). `check.sh` fails a `WIP…` head
(`CHECK_ALLOW_WIP=1` to check a branch's anyway).

**The routine check is `check.sh`** (#357): the musl release build of
`stormblock` (and stormblock-test's own check with `STORMBLOCK_TEST_DIR`, #371),
`cargo clippy --all-targets --workspace -- -D warnings` (#584),
every initramfs test under sh and busybox sh, the whole nextest suite, the
runtime tests. Run
it on a commit before calling it done: 87d2d99 reached main with neither its
tests nor the test image compiling (#357). A narrower `sc-build '…'` is for
iterating, never the last word on a commit.

**Build settings.**
- `dev`/`test`: `debug = "line-tables-only"`, no debug info for dependencies.
- `release`: thin LTO, 16 codegen units, for performance measurement.
- `dist`: fat LTO, 1 codegen unit, meant for goldens (`--profile dist`); the
  golden build does not use it yet (stormcos#169).

Dependencies: one TLS backend (ring), and `cluster` (openraft) is opt-in. Every
sc-build job starts from an empty drive, so every dependency is compiled on
every build: adding one costs every build.

Features: `default = ["iscsi", "stormfs-data"]` (`cluster` opt-in since
#209); `ui` is the old web UI (off since v12.2.0, stormview is the UI).
NVMe-oF is always built, not a feature (#161). `arm64` and `mikrotik` gated
no code and were removed (#169).

**RouterOS: not shipping** (owner, 2026-09-28) — the profile is neither built
nor checked. Kept as history of what it was and why:

**NVMe-TCP, not iSCSI, on RouterOS.** What StormBlock served there is
containers, PVCs and sbregistry, and those are 100% NVMe because **iSCSI is
slow**. Sharing an iSCSI disk and PXE-booting a bare-metal host are **mkube's**.
Measured, aarch64 release, since "the binary must be small" is a real
constraint:

| profile | bytes |
|---|---|
| `mikrotik,nvmeof` | 11,034,016 |
| `mikrotik,iscsi` | 11,398,192 |
| `mikrotik,iscsi,nvmeof` | 11,663,136 |

The profile leaves out `stormfs-data` too: a node with 256 MB is not a StormFS
data node, and a mounted surface invites being called.

`Cargo.lock` is committed; goldens build `--locked`; `cargo update` is a commit
of its own. TLS is rustls (no OpenSSL) on `ring` only since #209 — not C-free:
`ring` carries C and assembly.

## Where it runs (see README "Where it runs")

| where | started as | notes |
|---|---|---|
| stormcos node | stormpump boot unit `00-stormblock`: `adopt-ublk --api 0.0.0.0:9090 --data-dir /run/stormblock/engine` | takes over the initramfs engine's ublk devices; state restored from and captured to the `stormblock-state` volume; no shared :3260, no discovery, no cluster in this mode; forge mode (the shared :4420 target, boot claims) on by default unless `forge.json` says off (#287); `[nvmeof]` in `--config` (#206) or `PUT /api/v1/forge` (#272) set other settings |
| stormcos initramfs | `/init` → `boot-claim` + `boot-local` (`scripts/build-stormblock-initramfs.sh`) | boot hooks decide local vs appliance (`docs/boot-hooks.md`) |
| appliance / forge | the daemon | serves goldens, host clones and boot claims |
| RouterOS container | not shipping (owner, 2026-09-28); was the daemon, `mikrotik,nvmeof` profile | O_DIRECT on the block device, `pread`/`pwrite` on the blocking pool where io_uring is unavailable; never file I/O (#140) |

Drives are opened by the kernel and `O_DIRECT` everywhere; there is no
userspace (VFIO) NVMe driver (#167: the stub was removed). **RouterOS specifics:** container on RouterOS 7+ (or
CHR); no PCIe passthrough; 256 MB–1 GB of memory; NVMe-TCP is the transport
(`/disk add type=nvme-tcp`, confirmed taking writes 2026-08-13, #39); RAID 1 is
the relevant level; the binary must be small.

**PVCs on stormcos** are the built-in driver (class `stormblock`): the kubelet
clones the sealed `pvc-ext4j-<MiB>m` blank of the claim's size class through
`/api/v1/fstemplates/{id}/clone` and attaches it over ublk — no CSI. CSI
(stormblock-csi, `/v1`) is for third-party drivers only.

## Architecture (bottom-up)
- `src/drive/` — `BlockDevice`; `sas.rs` + `direct.rs` (O_DIRECT block devices: io_uring on its own thread, or the blocking pool), `nvmeof_dev.rs` and `iscsi_dev.rs` (initiators), `filedev.rs` (tests/dev only), `partition.rs`, `slab.rs` + `freemap.rs` + `slottable.rs` + `slab_registry.rs` (slot entries published after their data by `Slab::sync`, #171; the table read through a bounded page cache, no per-slot record in memory, #155), `discover.rs`, `ublk.rs`, `handover.rs` (`take_over`: stand down, then restore), `identity.rs`, `crashdev.rs` (a volatile write cache for power-cut tests), SMART (the VFIO stub `nvme.rs` was removed, #167); `vhost_user_blk.rs` (a VMM's virtqueues served directly, #82; `mgmt/vhost_export.rs`); `uring_server.rs` is not started by anything (#169; #82 did not use it)
- `src/raid/` — drive-level RAID 1/5/6/10 sets (#252): `layout.rs` (P/Q rotation, RAID-10 pairs), `superblock.rs` (v2: slot table, events, name, pool), `bitmap.rs` (on-disk write-intent bitmap), `spares.rs` (hot spares by pool), `rebuild.rs` (progress, rates); `mod.rs`: stripe locks, degraded I/O, assembly, rebuild onto a spare, scrub. `src/mgmt/raid_sets.rs` joins them to slabs and the API (`docs/raid-sets.md`)
- `src/volume/` — thin volumes (`thin.rs`), the compact extent table (`extable.rs`, #155), the slot fence (`fence.rs`: I/O shared, a move exclusive, #239), GEM (`gem.rs`), per-volume redundancy (`redundancy.rs`, `stripe.rs`, `stripelog.rs`), snapshots/clones, metadata (`metadata.rs`, V9; V8 written when every volume is 4096), synonyms, StormFS chunks/versions, GC, pressure, relocation, composition, `throttle.rs`
- `src/fs/` — templates (`template.rs`), ext4 (`ext4.rs`) and XFS (`xfs.rs`) seams, disk identity (`disk.rs`), files, image survey (`survey.rs`)
- `src/image/` — image build (GPT, FAT, ISO, qcow2/VHD/VMDK), import (`import.rs`, `decode/`), node layout (`local.rs`), local boot
- `src/pallet/` — pallet writer, GPT, store, manager, selection; the reader is `crates/pallet-format`
- `src/placement/` — failure domains (`domain.rs`), placement, drain moves, rebalance
- `src/target/` — NVMe-oF/TCP (`nvmeof/`: several subsystems per listener with allowed hosts, `auth.rs` DH-HMAC-CHAP, #210), iSCSI (`iscsi/`), per-core reactor
- `src/serve/` — the serving layer mounted at `/serve/v1` (wiring, reconciler, readiness, reaper, tar, raw, trim)
- `src/mgmt/` — the management API (`api/`: every `/api/v1` surface, `v1.rs`, `kube.rs`, `rebuilds.rs`, `boothost.rs`, …), auth, config, metrics, discovery, ublk exports, `nvme_hosts.rs` (per-host NVMe subsystems, `nvme_hosts.json`), `ui/` (feature `ui`)
- `src/cluster/` — openraft membership, heartbeat, replication (feature `cluster`, opt-in)
- `src/rebuild.rs` (automatic per-volume rebuild), `src/drain.rs`, `src/state.rs` (engine state in the `stormblock-state` volume), `src/boot.rs`, `src/boot_iscsi.rs` (opens its own slab, formats only a blank target or with `--format`, #162), `src/migrate.rs`, `src/registration.rs` (`[registration]`: this node's volumes announced to stormstorage; `[stormfs]` its old spelling, #170), `src/http.rs`
- `src/cli.rs` — CLI, the daemon, and every subcommand (`open_slabs_resuming`: a flow-over cut short claims a fresh clone, #171); `src/main.rs` only calls `stormblock::cli::run` (#209)
- the test container is glennswest/stormblock-test (#371; it was `test/`, #139)
- `tests/it/` — the in-process integration tests, one binary (nextest); `tests-runtime/` — tests against the built binary, devices or privileges (#209; `ci-runtime-tests.sh`, #222); `benches/` — `stormblock-benches`, the criterion benches (`cargo bench -p stormblock-benches`, #211)

## Current State
**v24.6.0** (2026-11-14, #587: the open health says whether the volume record is kept, `metadata`: ok, stale, short); **v24.5.2** (2026-11-14, #598: a node laid by `storage:` reboots: the plain data region found, `storage:` applied on an install only); **v24.5.1** (2026-11-14, #597: `storage: drives: all` takes every drive the inventory lists, virtio too; a drive left out is named); **v24.5.0** (2026-11-13, #595: every drive `storage:` takes formatted to 4096 + PI, NOT READY ones through sg, a refusal fatal); **v24.4.1** (2026-11-13, #592: the storage inventory waits until every SAS drive has come, not the first); **v24.4.0** (2026-11-13, #573: drives laid by `storage:` in the install-config: 4K format on every drive, a system leg on each that boots on its own, the data on a stormraid set, the layout reported at `/api/v1/storage/layout`, a keep-data reinstall keeps it); **v24.3.1** (2026-11-12, #584: clippy `--all-targets -D warnings` clean and in check.sh); **v24.3.0** (2026-11-12, #587: a stale slab region written again once it fits, not counted against durability `ok`; #585: `cargo test` passes as nextest does, the peer attach no longer holds the volume manager); **v24.2.2** (2026-11-11, #583: a full v2 slab region marked stale when the data directory holds the record); **v24.2.1** (2026-11-10, #546: a v2 store brought up to date by what differs, never a second tree; new regions 64 B a slot); **v24.2.0** (2026-11-10, #212: `allow_any_host` on the open `/serve/v1/health`); **v24.1.1** (2026-11-10, #571: the kube volume list no longer deadlocks the volume manager at start; `v1`/`ublk_exports` tracked, their order said); **v24.1.0** (2026-11-10, #574: NVMe/TCP answers checked against their command, a flow-over judged by what is left, install `failed` reported); **v24.0.1** (2026-11-07, #293: mkfs-ext4 v4.1.3 / fio-ext4 v1.8.3, the xattr block counted); **v24.0.0** (2026-11-07, #212: `[serve] allow_any_host` defaults to false); **v23.27.0** (2026-11-07, #565: rollback copies marked and pruned to `[rollback] keep` after boot-ok); **v23.26.0** (2026-11-07, #560: the node's name in each boot record, a rename said); **v23.25.1** (2026-11-07, #559: the initramfs takes a PTR name busybox prints with no trailing dot); **v23.25.0** (2026-11-07, #426: `/v1` create's `placement.slab_id` / `drive`); **v23.24.0** (2026-11-07, #426: a volume moved onto a named drive, slab or array, online, with progress); **v23.23.1** (2026-11-07, #521: a flow-over's sources let go by batches, a slow answer's line reaches the console); **v23.23.0** (2026-11-07, #557: a worker's reviews follow the cluster's record); **v23.22.3** (2026-11-07, #555: forge trust reviewed as `settrust`; #538: the stand-down back under a second); **v23.22.2** (2026-11-06, #545: path NICs beside the boot, DNS and DHCP retried by the second); **v23.22.1** (2026-11-06, #521: clones at once share their log flush, step times in `api:`); **v23.22.0** (2026-11-06, #484: a boot's page-in: read ahead in /init, 1 MiB of read-ahead on a ublk device); **v23.21.1** (2026-11-06, #544: the validation reads in disk order); **v23.21.0** (2026-11-06, #506: `/run/stormblock/appliance`); **v23.20.0** (2026-11-06, #514: an initramfs for its binary's architecture, aarch64 for the Orion O6N); **v23.19.2** (2026-11-06, #552: an install's pallet can boot, a node marks it good); **v23.19.1** (2026-11-06, #513: a cold map's 16 B an extent; a full drive measured); **v23.19.0** (2026-11-06, #505: ublk zero copy per request); **v23.18.0** (2026-11-06, #540: forge trust carries the apiserver's CA apart from the forge CA); **v23.17.0** (2026-11-04, #532: a refused mount repaired, what is still not mounted in health); **v23.16.0** (2026-11-03, #524: the boot verbs on their own plain port); **v23.15.0** (2026-11-03, #525: `tls_mode = "optional"`, and a count of who is still plain); **v23.14.0** (2026-11-03, #483: a TLS file not there yet is no refusal to start); **v23.13.1** (2026-11-03, #522: boot exports fit the fd limit); **v23.13.0** (2026-11-03, #520: `[fabric] peers_from = "kubernetes"`); before it **v23.12.0** (2026-10-29, #350): each staged golden since v21.0.0 carries a
tag (v21.1.0 … v23.12.0; the CHANGELOG has each). Since v21.0.0 on main: the
system drive known per machine (`system_drive`, first drive, #351, #478),
system pair and moving the system (#350), release erase (#291), erasure
coding `ec:K+M` off by default (#159), grow of a RAID-1 array (#318), release
of an array (#296), live mirror of an in-use volume (#295), forge back to
default (#290), multicast discovery (#104, off by default), a taken ublk
device survived (#299), path-per-NIC flow-over (#450), ublk ring/batch/
PER_IO_DAEMON tuning (#505), the fd leak fixed (#510), T10 PI in health
(#389), RAID-6 scrub locating a bad strip (#390), strict NVMe/TCP PDUs (#392),
admin-only image build (#393), the engine's commit in health (#466), logcast
(#477), the install boot's 20 s grace (#448), git dependencies by rev (#480).
The README is the reference for what the code does, refreshed from the code
on 2026-10-30 and checked again on 2026-11-07 (v23.23.0); "Not built, or not wired" lists what is promised and not done.
The older state follows.

**v21.0.0** (2026-10-17, #353, f23b508, golden-stormblock-595bf69e5baa):
version and changelog only, over everything below; breaking for callers:
#274/#382 (destructive verbs need the admin token or a `destroy` review),
#161 (`nvmeof` feature gone), #160 (RAID-set scrub reports by default),
#158 (metadata format 2 default), #311, #83/#6. **From here on a tag
(patch/minor) goes with each staged golden**, so a golden's version names a
tag. Before it, **v20.0.0** (2026-09-28): boot intent (#148), the build/test
split (#209, `cluster` opt-in) and per-host NVMe/TCP subsystems (#210).
What v21.0.0 carries since v20.0.0 (goldens staged through stormcos#168 and
#601 reported 20.0.0):
RAID sets on a shelf with hot spares, reassembly, bitmap, rebuild, scrub
(#252, #168, #175, #215);
per-volume LBA 512|4096, metadata V9 (#228); the install stopgap and
`slab holds` (#236, #239); the slot fence and no in-place writes on a slab
being emptied (#239, durability rules 10–11); and in the initramfs: every
`console=` (#237), the firmware's boot name from EFI variables (#249),
mlx4_en and late netdevs (#250), the NTP clock step (#251).
Also unreleased since the 2026-10-09 refresh (details in CHANGELOG.md and the
work plan below): volumes in namespaces with quotas (#408), secret volumes (#74), a reviewed Kubernetes bearer and
creator-scoped deletes (#382, #422), forge client certificates admin by
subject CN (#453), forge's per-host install-config (#459) and boot-agent
claims (#456), a system half on its own SSD (#402), post-flow-over sha256
validation (#412) and the faster, latency-paced flow-over (#401, #410, #411),
FUA and multi-queue ublk (#409), NVMe/TCP multipath and path-per-NIC
(#90, #450 engine half), `[fabric]` peer attach (#328), remote-host
connections (#292), RAID-6 SIMD (#255), erase/scrub/digest (#286, #160, #271,
#384), the lock work (#364, #418, #438), the VFIO stub removed (#167) and
`[registration]` (#170). 150k lines in `src/`, 22k in `tests/it/`, 3.8k in
`tests-runtime/`, ~1,250 tests; the test container is glennswest/stormblock-test
(#371). The full suite (nextest) is the routine check, `check.sh` (#357).
Pacing: P0/P1 only until single-node stormcos is stable (2026-10-17); parked
work lives on `wip/` branches. v20.0.0 was cut at the owner's request on #148
(2026-09-28); a v21.0.0 is asked on #353. #171 (power-cut durability) is
closed: the on-metal acceptance passed on 2026-09-27. The README is the
reference for what the code does, refreshed from the code on 2026-09-27,
2026-09-28, 2026-10-02 and 2026-10-17; the docs in `docs/` were checked
against it and the superseded ones moved to `docs/history/`. What earlier
docs promised and the code does not do is listed in the README's "Not built,
or not wired" with its issues.
The v17+ engine (closed by default, #107) ships in releases since 11.55
(golden-stormblock-296e38521d4c first); only forge still runs 13.7.0 (#235).

For special runtime testing that needs its own machine, spin up a VM with
terragrunt (`deploy/terragrunt/`). DNS: 192.168.1.252, 192.168.1.154
(dns.gw.lo).

---

## Work plan

The work plan and its session history are **[docs/work-plan.md](docs/work-plan.md)**. That file is not loaded by default. Find your section with `grep -n '^## ' docs/work-plan.md`, then read only that section. Write intentions there before starting a task, as before. Keep this CLAUDE.md short: it is loaded into every turn.
