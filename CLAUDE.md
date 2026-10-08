# StormBlock Development Guide

## Project Overview
Pure Rust enterprise block storage engine. Turns raw NVMe/SAS drives into network-accessible volumes over NVMe-oF/TCP and iSCSI. Part of the Storm ecosystem (StormBlock, StormFS, StormForce, StormOS).

## Design Principle: Single-node first, scale-out later
StormBlock must be fully functional as a **standalone single-node** storage engine — no cluster requirement. A single node handles its own drives, RAID, volumes, and exports independently. Clustering (replication, Raft) is layered on top and strictly optional. New nodes can be added to an existing deployment at any time without disrupting running nodes.

## Build and test with sc-build, on dev

**Every `cargo build`, `cargo test`, `cargo check` and every image build runs on
dev.g8.lo, through `sc-build` after `git push`** — never on the session host,
never as root, never "just to check quickly". `sc-build` fetches the pushed
commit onto dev as the unprivileged `stormbuild` user, builds it in a scratch
directory and deletes it; there is no checkout on dev to use. `sc-build 'cmd'`
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
builds (stormcentral#61). Known red: `integration_image` (#120), and on a busy
box `mgmt_luns_at_scale` (#134) and
`integration_fstemplates::a_create_whose_caller_gives_up_still_finishes` (#173).

The test container (#139): `test/build.sh` builds `stormblock` and
`stormblock-test` (musl) and the `FROM scratch` image;
`sc-build 'sh test/build.sh && podman run --rm --user 65532 --tmpfs /results:rw,mode=1777 stormblock-test short'`
runs a suite the way the Job does (`medium`, `long` likewise; `STORM_WAVE_MAX`
caps long's waves).

## Build
```bash
sc-build 'cargo nextest run --locked'                               # the routine check
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
  (`STORMBLOCK_BIN`), a kernel device or privileges. They are not part of the
  routine check: `STORMBLOCK_BIN=<binary> cargo test -p stormblock-runtime-tests`.
  Nothing runs them today (#222).
- `src/main.rs` is a wrapper; the command line is `stormblock::cli`, compiled
  and tested once as part of the library.

**Build settings.** The routine check never runs `cargo build --release`.
- `dev`/`test`: `debug = "line-tables-only"`, no debug info for dependencies.
- `release`: thin LTO, 16 codegen units, for performance measurement.
- `dist`: fat LTO, 1 codegen unit, meant for goldens (`--profile dist`); the
  golden build does not use it yet (stormcos#169).

Dependencies: one TLS backend (ring), and `cluster` (openraft) is opt-in. Every
sc-build job starts from an empty drive, so every dependency is compiled on
every build: adding one costs every build.

Features: `default = ["nvmeof", "iscsi", "stormfs-data"]` (`cluster` opt-in
since #209); `ui` is
the old web UI (off since v12.2.0, stormview is the UI); `arm64` and
`mikrotik` are profile names that gate no code (#169). Without `nvmeof` the
tree does not compile (#161).

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

Drives are opened by the kernel and `O_DIRECT` everywhere; the VFIO NVMe
driver is a stub (#167). **RouterOS specifics:** container on RouterOS 7+ (or
CHR); no PCIe passthrough; 256 MB–1 GB of memory; NVMe-TCP is the transport
(`/disk add type=nvme-tcp`, confirmed taking writes 2026-08-13, #39); RAID 1 is
the relevant level; the binary must be small.

**PVCs on stormcos** are the built-in driver (class `stormblock`): the kubelet
clones the sealed `pvc-ext4j-<MiB>m` blank of the claim's size class through
`/api/v1/fstemplates/{id}/clone` and attaches it over ublk — no CSI. CSI
(stormblock-csi, `/v1`) is for third-party drivers only.

## Architecture (bottom-up)
- `src/drive/` — `BlockDevice`; `sas.rs` + `direct.rs` (O_DIRECT block devices: io_uring on its own thread, or the blocking pool), `nvmeof_dev.rs` and `iscsi_dev.rs` (initiators), `filedev.rs` (tests/dev only), `partition.rs`, `slab.rs` + `freemap.rs` + `slottable.rs` + `slab_registry.rs` (slot entries published after their data by `Slab::sync`, #171; the table read through a bounded page cache, no per-slot record in memory, #155), `discover.rs`, `ublk.rs`, `handover.rs` (`take_over`: stand down, then restore), `identity.rs`, `crashdev.rs` (a volatile write cache for power-cut tests), SMART; `nvme.rs` is a VFIO stub (#167); `uring_server.rs` is not started by anything (#169)
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
- `src/rebuild.rs` (automatic per-volume rebuild), `src/drain.rs`, `src/state.rs` (engine state in the `stormblock-state` volume), `src/boot.rs`, `src/boot_iscsi.rs` (formats every run, #162), `src/migrate.rs`, `src/stormfs.rs` (registration, served by stormstorage, #170), `src/http.rs`
- `src/cli.rs` — CLI, the daemon, and every subcommand (`open_slabs_resuming`: a flow-over cut short claims a fresh clone, #171); `src/main.rs` only calls `stormblock::cli::run` (#209)
- `test/` — `stormblock-test`, the test container: short/medium/long suites that run the engine of the same commit in the pod (#139)
- `tests/it/` — the in-process integration tests, one binary (nextest); `tests-runtime/` — tests against the built binary, devices or privileges (#209, not run anywhere yet: #222)

## Current State
**v20.0.0** (2026-09-28): boot intent (#148), the build/test split (#209,
`cluster` opt-in — BREAKING) and per-host NVMe/TCP subsystems with DH-HMAC-CHAP
(#210 — BREAKING: the shared subsystem is closed by default); before it
v19.4.0 (universal boot #200, boothost names #199). Unreleased since v20.0.0
(Cargo still says 20.0.0; goldens staged for the P0s through stormcos#168):
RAID sets on a shelf with hot spares, reassembly, bitmap, rebuild, scrub
(#252, #168, #175, #215);
per-volume LBA 512|4096, metadata V9 (#228); the install stopgap and
`slab holds` (#236, #239); the slot fence and no in-place writes on a slab
being emptied (#239, durability rules 10–11); and in the initramfs: every
`console=` (#237), the firmware's boot name from EFI variables (#249),
mlx4_en and late netdevs (#250), the NTP clock step (#251). 99k lines in `src/`, 12k
in `tests/it/`, 3.5k in `tests-runtime/`, ~920 tests, plus the test container
crate (`test/`). The full suite (nextest) passes on dev apart from #134 when
the box is busy; #120 is now in `tests-runtime/`, which nothing runs (#222).
v20.0.0 was cut at the owner's request on #148 (2026-09-28); the forge rollout
(settings, rollback = VM snapshot only) is answered there. #171 (power-cut
durability) is closed: the on-metal acceptance passed on 2026-09-27 (C2NR0Q2,
11.48 = v19.2.1 engine and initramfs, 5 cuts, 1500/1500 objects). The
flow-over resume of v19.2.2 has not met metal yet; that check is #172. The README is the reference for what the code does, rewritten from the code in
#131 and refreshed from the code on 2026-09-27, 2026-09-28 (#199, #200,
#148, #209, #210) and 2026-10-02 (#228–#251: the initramfs command line,
`slab holds`, boot-time files, stormcos#65's corrections #242); the docs in `docs/` were checked against it and the
superseded ones moved to `docs/history/`. What earlier docs promised and the code does not do is
listed in the README's "Not built, or not wired" with its issues (#159–#170, #205–#248).
The golden has been held since v17 for the token rollout to the engine's
clients (#107; stormcentral#30, stormcos#89 and the rest); when to release
it is the owner's call, #194.

For special runtime testing that needs its own machine, spin up a VM with
terragrunt (`deploy/terragrunt/`). DNS: 192.168.1.252, 192.168.1.154
(dns.gw.lo).

---

## TODO — Implementation Roadmap

### Emulated directory backing, mkfs-ext4 off v3.0.0 (2026-10-07, #300, P2) — PARKED (master: #331 first)

Part 2 (mkfs-ext4 v4.1.0 for 256 TiB / 1 PiB in core) is blocked on a
fio-ext4 release that pins it (fio.ext4.rs#10, commented): one tag for both,
or two copies of mkfs-ext4. Part 1 measured at HEAD with
`examples/emulated_format` (the API's template create: format + seal) and
`drive::emulated::dir_stats` (calls/time per directory-store operation):
64G 0.5/0.7 s, 128G 0.8/1.2, 256G 1.4/2.1, 512G 2.9/5.7, 1T 5.7/11.6
(memory/directory) — linear, no remove/rescan/punch; the >180 s at 1T of
the report (08b6d72, through the HTTP API with the watchdog) does not
reproduce in-process. A 2T/4T/16T run was started when parked (result not
read). Next: read that run; reproduce through the daemon's HTTP API (the
heartbeat stall); then the issue's asks (handle cache, running stored count
instead of `dir_stored` rescans).

### FileDevice::write returned before the bytes were in the file (2026-10-07, #279) — DONE

Flaky in the full suite: `pressure::tests::an_existing_slab_on_a_source_is_
adopted_with_its_data` (check() not Grew). Found: `FileDevice::write` is
seek + `write_all` on a `tokio::fs::File`, whose poll_write hands the write
to the blocking pool and returns; that File orders its own later ops after
it, but a second `FileDevice` on the path (the watcher's reopen) can read
the file before it lands — under the suite's load the pool is busy.
- [x] `write` flushes the tokio File (no fsync): it returns once the bytes
      are in the file
- [x] regression test: the blocking pool kept busy, the file read directly
      after `write` returns; fails without the fix (throwaway branch); the
      pressure test prints what check() returned; CHANGELOG
- Verified on build VMs: the regression test fails 3/3 without the fix
  (write 0) and passes 5/5 with it; the pressure test 40/40 alone; two full
  suites 1004/1004. FileDevice is tests/development only: no golden

### CrashDevice tears writes; cuts inside an operation (2026-10-07, #191, P2) — DONE

From #171: `CrashDevice` kept or dropped each unflushed write whole; a drive
can persist part of a multi-block write.
- [x] `Tear::Prefix|Scatter` at the drive's atomic unit (4096, or 512 with
      `with_atomic_unit`), `crash_with`, `torn_writes()`
- [x] found that tears never reached metadata (cuts fell between operations,
      when persists and syncs had flushed): `cut_at(n)` takes the power as
      the nth write arrives — inside a persist or a slot-table sync
- [x] the 300-cut test under none/prefix/scatter/512, formats v1 and v2;
      sector-wise check where a block can tear; counts reported, a run that
      tears nothing or cuts too few operations fails
- [x] found: format v1 loses a clone's unwritten extent to a cut inside a
      persist (9/300, not tearing) — #340; v1 runs cut between operations,
      `v1_survives_a_power_cut_inside_a_persist` ignored repro. v2: 0 lost
- [x] verified on a build VM at fedaaf3: 17/17 power-cut tests (v2: 205/300
      cuts inside an operation, 0 lost, 98–247 torn writes per run); the
      ignored v1 repro still fails 9/300 (#340's repro); full nextest
      1003/1003. Test infrastructure only: no golden needed. #340 is next

### Import refuses an XFS dirty log or an ext4 pending journal (2026-10-07, #198, P2) — DONE

An image taken from a running or crashed system walked clean and was sealed:
`fio-xfs` never read the log, and the import ignored ext4's RECOVER. Linux
replays on first mount, so what the engine surveyed (os-release, entries)
was not what the guest sees. fio-xfs v0.3.0 (fio.xfs.rs#6) reads the log.
- [x] pin fio-xfs v0.3.0 (same dependencies; Cargo.lock by hand, built
      `--locked`)
- [x] survey: `log` per filesystem (`clean`, `dirty`, `external`,
      `unreadable`; ext4 `needs_recovery`) and anything not clean an error,
      so the import fails unless `verify: false`; an external XFS log is not
      verifiable (fio.xfs.rs#17); a dirty XFS log's message says a clean one
      is on rare occasions read dirty (fio.xfs.rs#16)
- [x] XFS seal blockers: the log not clean
- [x] tests: an engine-made XFS reads clean and seals; its log made dirty by
      hand (records after the unmount record): survey dirty, the import
      verdict fails (passes with verify false), seal refused; ext4 with
      RECOVER: needs_recovery, verdict fails; README, CHANGELOG
- Verified on a build VM at 423696b: full nextest 996/996 (`--locked`);
  `ci-xfs-verify.sh` PASS: the Rocky 9 cloud image's two XFS partitions read
  clean (no false dirty), import + xfs_repair -n clean; engine blanks/claims
  xfs_repair -n clean

### A flush with nothing to make durable returns at once (2026-10-07, #338, P1) — DONE

stormpump#107 / rustkube-node#95: a pod's 64Mi claim spends 553–878 ms in
its ext4 mount on a warm node (46–88 cold). The mount's ublk FLUSHes each run
`sync_registered` on every slab the volume touches: a device-wide flush,
queued behind any other volume's sync on that slab. A FLUSH only has to
make durable the writes completed before it was issued.
- [x] per volume handle: `completed` (bumped when a write, discard or
      write-zeroes finishes — success, error or dropped, by a guard) and
      `synced` (the `completed` a successful flush started from). `flush`
      returns at once when nothing completed since; otherwise the full
      ordered sync, then `synced = max(synced, start)`. Starts 1/0: a
      handle's first flush is always full
- [x] tests: a clean flush touches no device and does not wait behind a
      slow flush of another volume on the slab; a write that completes
      during a flush makes the next flush a full one; power-cut and
      durability tests unchanged; durability.md, CHANGELOG
- Note: an rw ext4 mount writes its superblock, so its post-write flush
  still pays; the clean ones (the pre-flush, barriers before any write) do
  not. stormpump's per-step timing on the next probe says how much
- Verified on a build VM at eb63634: the three new tests 3/3 each; full
  nextest 994/994 (16 power-cut/durability/crash tests among them). Golden
  golden-stormblock-7b7707f4cbed (stormcos#353). Not on metal: stormpump's
  mount timing on a release with this engine

### The stall watchdog's task dump panicked a ublk device's runtime (2026-10-07, #334, P0) — DONE

Dell 11.91 under rustkube-node's `medium` suite: `ublk-adopt-36` panicked
`RefCell already borrowed` (tokio current_thread/mod.rs:723), then :9090
stopped answering. Found by reading tokio 1.53.1: requests stalled > 10 s, so
the watchdog (#269) ran `task_dump`, which spawns `dump()` on every
registered runtime — the ublk-adopt/-export current-thread runtimes too. On a
current-thread runtime `dump()` holds the core's RefCell while it polls each
task in trace mode; our I/O futures are not tokio's, so tracing runs them,
and a task that finishes and releases a tokio lock wakes another on the same
runtime: `schedule()` borrows the core again → panic. That device's server
thread dies and its I/O hangs. The multi-thread (API) runtime traces with
the core taken out and the workers parked: not affected.
- [x] `task_dump` never dumps a current-thread runtime (names it, says why;
      /debug/threads still shows its threads)
- [x] test: an I/O task holds a tokio Mutex an older task waits on; its I/O
      completes underneath; the dump comes first (tokio traces oldest first,
      and a fresh leaf poll in trace mode registers nothing — three earlier
      versions of the test did not reproduce it for those reasons). Without
      the fix: `RefCell already borrowed` at current_thread/mod.rs:723:40,
      and the I/O and its waiter never go on (3/3, throwaway branch). With
      it: 5/5. The panic is caught by tokio; the lost wake is what hung the
      Dell's API
- [x] docs (debug), CHANGELOG
- Verified on a build VM at 43982c6: full nextest 991/991. Not on metal:
  rustkube-node's `medium` suite on the Dell with this engine (stormcos#36)

### Health says whether the node runs from local or remote slabs (2026-10-07, #322, P1) — DONE

stormcentral#353: C2NR0Q2 passed "local boot" and "fresh slab" while running
entirely from a forge clone (#268). Ask: the open `/api/v1/health` says
where the node's slabs are. Not the remote URI: health is unauthenticated,
and the URI (forge's address, the clone's subsystem NQN, the host NQN; boot
hosts connect with no secret, #210) is what attaching the machine's boot
clone takes.
- [x] `slabs`: `diskless`, `system`/`data` = local|remote|mixed|none (by
      where the volumes' legs are), and each slab: role, source, local
      device or remote transport, volumes with legs on it. Never waits:
      computed with try_read, cached 10 s; no remote slab registered = all
      local with nothing counted (forge, a disk-booted node)
- [x] tests; README (health), auth.md; tell stormcentral#353
- Verified on a build VM at 7d66a33: full nextest 990/990, incl. the #314
  netboot test's node over NVMe/TCP (`diskless`, `remote`, no URI/NQN/address
  in the body) and a local node (`local`). Not on metal: a stormcos release
  with this engine; then stormcentral#353 fails a diskless install

### Delete with `scrub=used` (2026-10-07, #313, P1) — DONE

Owner (2026-10-06): data removal is "overwrite only what's used, once",
everywhere (reset #312, registry cleanup stormblock-registry#70, user
deletes). Already true since #286: a delete marks every slot whose last
reference goes `Erasing`, the eraser overwrites it once (the node's default)
and discards it on flash, then frees it; copy-on-write aware; every local
delete path. Missing: the word, and the report.
- [x] `?scrub=used` on `DELETE /api/v1/volumes/{id}` and
      `/serve/v1/volumes/{id}`: at least `once`; answers 200 `{volume,
      scrub: {level, slots, bytes}}` (the slots this delete queued, counted
      at retire under the registry lock). Without it, 204 as before.
      Completion: the volume's record in `GET /api/v1/erasures`
- [x] tests (a clone sharing its golden's slots: only its own scrubbed; the
      golden's delete then scrubs the rest; bytes overwritten on the device);
      docs/erase.md, README, CHANGELOG; tell #312 and registry#70
- Verified on a build VM at 643cdef: the new HTTP test and every #286 erase
  test; full nextest 987/987. `/serve/v1`'s `scrub` is the same call, not
  driven over HTTP by a test. Told #312 and stormblock-registry#70

### A flush to a failed mirror leg degrades it (2026-10-07, #308, P1) — DONE

stormcos#92's shelf (160 emulated drives): after a drive under a mirror leg
was failed, the next flush returned EIO for the whole volume and health said
healthy. Found: `flush` returned the first slab's sync error for every
volume; the write path marks a failing leg's slab failed and goes on, but a
flush that is the first I/O to reach a dead drive did not (on 160 drives the
writes before it usually land elsewhere; on 8 they hit it first).
- [x] flush: a media error from a slab's sync on a redundant volume marks
      that slab failed for the volume and goes on; error only when something
      is then unreadable; an unreplicated volume's error stays the volume's
- [x] test `thin::redundancy_tests::a_flush_to_a_failed_leg_degrades_the_
      mirror_not_the_volume` (emulated drives): passes at 428b11a, fails
      without the fix at the flush (checked on a throwaway branch); full
      nextest 985/986 (#333's port race); CHANGELOG, docs/redundancy.md
- Not on stormcos#92's shelf yet: its pb-scale run on an engine with this

### Units' first I/O waits on the engine handover (2026-10-07, #303, P1) — PART DONE, WAITING ON THE OWNER

stormcos#300: stormcert-init's first write waits for `Adopted 65 device(s)`
(5.4 s Dell, 5.7 s pve); on a reboot from the Dell's HDD the incumbent let go
~21 s after `adopting 64 volume(s)`. The issue's ask 2 (open the slabs and
restore *before* the incumbent lets go) is the order the owner ruled out on
#171 (durability rule 8: reading first lost the incumbent's last
allocations), so it is not done here without the owner. Done here:
- [x] ask 1: timing lines `[adopt +T s] … (Δ s)` for every handover step
      (stand-down asked, quiesced, incumbent exited, each slab path attached
      and opened with its slot count, records read, restored, devices live),
      and `[boot-local stop +T s]` in the incumbent's stop
- [x] ask 3: `/run/stormblock/handover-state.json` `adopting` → `serving`
      (with `took_ms`) for stormpump to hold units on (stormpump issue)
- [x] verified on a build VM at 1cb6aa7: ci-adopt-retry-verify.sh ALL PASS
      (`timing` step: the lines and `serving`), full nextest 985/985;
      README, CHANGELOG; stormpump#109 (hold units on the state)
- Asked on #303 (needs-owner): A keep rule 8 (pre-attach only), B read
  ahead + reconcile, C decide after the Dell's timing lines (recommended).
  The issue stays open. Its part is in golden-stormblock-65b6578787be

### The initramfs mounts the container volumes in parallel (2026-10-07, #302, P1) — DONE

stormcos#300: 63 volumes mounted one at a time, ~130–200 ms each, 8.6–10.5 s
of every boot (Dell, install and reboot). Bare `mount` probes types; the
device wait is per entry.
- [x] `# --- BEGIN container mounts`: one wait for every device
      (`STORM_MOUNT_WAIT`, 15 s); then mounts in waves by mount-point depth
      (a nested mount point never before its parent), at most
      `STORM_MOUNT_PARALLEL` (16) at once; `-t ext4` first, a probing mount
      if that fails (XFS volumes); the same `mounted:` / `WARNING:` lines,
      and one line with the count and the time
- [x] `tests/initramfs-container-mounts.sh` (stubbed mount and devices):
      all mounted, parallel (time), bounded, nested order, XFS fallback, a
      bad volume warned, a missing device waited for once; under sh and
      busybox sh; the generated /init parses. README, CHANGELOG
- Verified on a build VM: the new test 17/17 under sh and busybox sh (20
  volumes at 0.3 s each: 0.63 s, not 6), every other initramfs test under
  both, the generated /init parses under both. Not on metal: the Dell's
  `mounted` span on a stormcos release with this initramfs
- Golden: golden-stormblock-65b6578787be (stormcos#353), staged at 3372dde
  once stormcentral#362 was fixed; carries #302, #303, #308, #313, #322 and
  #334

### adopt-ublk stops in order on SIGTERM (2026-10-07, #144, P1) — DONE

It waited for SIGINT only; stormpump's stop and a successor's stand-down are
SIGTERM, so it died by the default action: no final state capture (up to
10 s lost), no ublk teardown. Adopted devices are recoverable, so an orderly
stop releases them (never STOP/DEL) and a handover is unchanged.
- [x] `StopSignal` (SIGINT | SIGTERM), registered once the take-over
      succeeded; on a stop: release the devices, final capture and a bounded
      metadata persist side by side (≤ ~10 s); boot-iscsi and the attach
      command use it too
- [x] `ci-adopt-retry-verify.sh` +2 (handover by SIGTERM exits 0 and the
      next serves; a file written just before SIGTERM survives into the next
      adopter); README, CHANGELOG
- Verified on a build VM at 33eff88: `ci-adopt-retry-verify.sh` ALL PASS
  (7 steps, kernel 7.2.8); full nextest 985/985. Not on metal: stormpump's
  shutdown on a release with this engine (stormpump#50)

### `installed` from the first boot off the local disk (2026-10-07, #220, P1) — WAITING ON THE OWNER

Owner on #148: the install is proven by the host booting from its own disk;
the report comes from that boot. Today the successor posts `installed`
(`report_installed`) once `run_local_boot` judges the disk bootable, before
any boot from it, and that report is also what flips the intent to `local`.
Found: the issue's "intent stays `install` until the local boot reports"
loops: stormbootx installs again whenever the intent is `install`, and only
`local` boots the disk. Asked on #220 (needs-owner, `stormcentral
wait-owner`): **A** intent `local` once laid + boothost `install` state
laid→booted, failure surfaced to stormcentral (recommended, engine-only);
**B** A + automatic re-install after N min, K tries; **C** a `verify`
intent in stormbootx. Same engine part in all three: the ticket written into
the node's state on the disk; the first boot from local slabs only, no
claim, posts `installed`. Independent of #221. Nothing built yet.

### /v1 refuses `encrypted: true`: nothing encrypts (2026-10-07, #232, P1) — DONE

`POST /v1/volumes {"encrypted": true}` was stored and reported `encrypted:
true` on plaintext. Until #74's design (per-volume DEK, dm-crypt at the node)
is built: refuse it, report false.
- [x] `V1Error::Unsupported` → 422 `{code: "unsupported"}` (stormblock-csi's
      client maps 400/422 to InvalidArgument: `CreateVolume` fails with the
      message); checked before anything is allocated or looked up by name
- [x] a persisted volume recorded `encrypted: true` reads back false, said
      at startup (and written back at the next persist)
- [x] tests; README "Not built", CHANGELOG; stormblock-csi told
- Verified on a build VM at 6131382: both new tests, every contract_v1_wire
  test; full nextest 984/985 (`integration_forge::a_node_is_a_forge_by_
  default…`, 1/1 alone: a port race). stormblock-csi#48

### The management API over TLS, a node-CA client certificate as a credential (2026-10-07, #203, P1) — DONE

stormcos#81's rule (owner, 2026-09-25): every API on a node is TLS with a
stormcert pair, every client verifies the node CA and authenticates, nothing
answers anonymously but health. `[management] tls_cert`/`tls_key` already
serve :9090 over HTTPS (adopt-ublk too, from `--config`). Missing here (the
owner's validation, 2026-10-02): client verification against the node CA as
an alternative to the token. stormcos ships the pair and settings, callers
move to https: theirs.
- [x] `[management] tls_client_ca`: client certificates requested and
      verified against it, optional (token callers and probes still work); a
      verified certificate = the node token's tier (ordinary verbs,
      attestation reads); a destructive verb still needs the admin token or
      a reviewed Kubernetes bearer (#274), audited as `client-cert:<sha256>`
- [x] the pair and the CA re-read when their files change (stormcert renews
      them), the old kept when a new one does not load
- [x] tests: HTTPS with the node CA; a client cert from the node CA gets
      ordinary verbs, no destructive; one from another CA is refused at the
      handshake; no cert + token works; health anonymous; a renewed pair is
      served without a restart. Docs (auth.md, README), CHANGELOG; stormcos
      issue (ship pair + CA, `tls_client_ca`)
- Verified on dev: `integration_mgmt_tls` (3) with real handshakes, every
  auth and destructive test. Not on a node: stormcos#374 (the pair, the CA
  and the keys in the engine's config; callers to https)

### adopt-ublk: a restore that fails after the incumbent exited (2026-10-07, #190, P1) — DONE

Since v19.2.1 (#171) `adopt-ublk` stands the incumbent down, waits for it to
exit, then restores. A restore that fails there leaves every ublk device,
root included, quiesced with no server; nothing retries, times out or says
so. And "restore touches nothing ublk-backed" was only stated. By reading:
- `--meta` (or the record's `meta`) is read after the stand-down: on the
  root filesystem that read waits on the device this process is about to
  serve (a hang, not an error). With no meta and no slab metadata, the
  fallback `<parent of first slab>/meta` of an `nvme-tcp://` URI is a
  *relative* path, created in the cwd (the root)
- an `nvme-tcp://` host that is a name is resolved after the stand-down
  (resolv.conf, hosts: the root)
- "adopted none … the root is still served by whoever had it" is false since
  #171: nobody serves it
Plan:
- [x] before the stand-down (the incumbent still serving, so refusing is
      safe): every local path the restore reads (`meta`, file slabs) must
      not be on a ublk-backed or unknown (overlay) filesystem; fabric host
      names resolved to addresses then; no cwd-relative meta for a fabric URI
- [x] restore retried with backoff (1 s doubling to 15 s) for
      `STORMBLOCK_ADOPT_RESTORE_SECS` (120); each failure on the console
- [x] giving up (restore, or no device adopted): `/run/stormblock/adopt-failed.json`
      (what, why, the devices held), a FATAL on the console, exit 75 — the
      devices stay held in recovery so running adopt-ublk again takes them;
      the marker removed on success
- [x] tests: take_over retry units; `ci-adopt-retry-verify.sh` (QEMU, real
      ublk): restore fails twice then adopts, I/O blocked in the gap
      completes; never succeeds → exit 75 + marker, devices held, a second
      adopt-ublk takes them and the data reads back; a meta dir on the ublk
      device refused before the stand-down. Docs, CHANGELOG; stormcos issue
      (the boot unit restarts adopt-ublk on 75)
- Verified on dev: `ci-adopt-retry-verify.sh` ALL PASS (kernel 7.2.8);
  the retry, backing and handover tests. Not on metal: needs a stormcos
  release with this engine; stormcos's boot unit should run adopt-ublk
  again on exit 75 (filed)

### A network-booted node lists and verifies its boot pallet (2026-10-07, #314, P3) — DONE

sectionsystems#7 verifies the boot pallet through the node's engine
(`GET /api/v1/pallets/{stormuefi.pallet}`, `POST …/verify`). `/pallets` was
built from `state.drives` only, and `adopt-ublk` (the node's engine, netbooted
or not) registers no drives: the disks its slabs were opened from — the
claimed clone's `nvme-tcp://` namespace, or the local disk — were in no store.
The issue's option 1, read-only:
- [x] `open_slabs_with_disks` (what `open_slabs_resuming` wraps) hands back
      the disk each slab path was opened from; `adopt-ublk` keeps them as
      `AppState.boot_disks` (`set_boot_disks`)
- [x] `/pallets` reads (list, status, chain, get, verify) see `drives` +
      `boot_disks`; every write verb sees `drives` only (404 on a boot disk)
- [x] test `forge_mode_tests::a_netbooted_node_lists_and_verifies_the_boot_
      pallet_it_booted` (appliance serves a release disk over NVMe/TCP, the
      node claims and opens it, GET/verify ok, activate/successful/DELETE 404)
      on dev; docs/pallets.md §9, README, CHANGELOG
- Not on a live node: needs a stormcos release with this engine; then
  sectionsystems#7's check on a netbooted node

### Concurrent export persists no longer race (2026-10-07, #174) — DONE

stormblock-registry's long test: `rename exports.tmp -> exports.json: ENOENT`
under concurrent `/serve/v1` exports, and leaked volumes.
- [x] `write_atomic`: a temporary file per write, removed on failure; the
      engine's `exports.json` through it too
- [x] both export persists serialised (static lock, snapshot under it)
- [x] a failed export undone (row + entry); a volume created for an export
      that failed is deleted
- [x] tests: 16 concurrent exports, a failing persist leaves nothing;
      serve/export/wiring tests on dev 69/69

### local-boot keeps the proven boot pallet, drops the failed one (2026-10-07, #205) — DONE

stormuefi's installed-node test: after B failed 3 times and A was marked
successful, laying C removed A and re-armed B. `rerank`/`evictable` ordered by
`(priority, version)` only and reset tries on every exhausted pallet.
- [x] `boot_state` (proven > candidate > exhausted) before `order_key` in
      `rerank` and `evictable`; the last proven pallet never evicted; tries
      reset only on pallets copied this run (`fresh`); `BelowActive` goes
      below the top non-exhausted pallet
- [x] tests: the issue's scenario, an exhausted pallet not re-armed; every
      local_boot test and #122's staging tests on dev (12/12); docs/images.md,
      README (known fault removed), CHANGELOG

### A claim by an unknown name with a known MAC reaches that host (2026-10-07, #204, P1) — DONE

stormbootx#23 claims `boothost/<first DNS label>` with `{mac, serial}`; an
unknown name used to become a new host even when the machine was one under
another name.
- [x] `SynonymStore::resolve_named_claim` (`NamedClaim`): a known name as
      before; else the MAC's host, or the serial's when it is an operator-set
      alias (`alias_owner`, never an assignment's name); provisional → renamed
      (old name an alias), named → the claimed name added as an alias;
      neither → new from the default with the MAC as its alias
- [x] `ClaimRequest.serial`; `claim_boothost` resolves, keeps the new host's
      MAC; reply `host.resolved`
- [x] tests on dev (49/49 synonym + auth); README, auth.md, CHANGELOG

### Console: WARN and the stage lines only; the record in a file (2026-10-07, #243, P1) — DONE

Owner (server8's VGA): "the info messages we need to turn off to the
console. Overload". The engine's stderr reaches every console: in the
initramfs through `/init`'s follower, on the node through stormpump's echo.
- [x] node modes (`boot-local`, `adopt-ublk`, `boot-iscsi`): the record
      (INFO, or `RUST_LOG`, or `stormblock.log=` on the kernel line) goes to
      `/run/stormblock/stormblock.log` (`STORMBLOCK_LOG_FILE`; `/run` moves
      into the real root, so one file for the boot); stderr, which is the
      console, WARN (`STORMBLOCK_CONSOLE_LOG`, `stormblock.console_log=`);
      the stage lines are println! and still show. Other commands unchanged
- [x] `unauthorized …` warns: one per minute, then a count
- [x] `/init`'s engine report names the record; tests; README, CHANGELOG
- Verified on dev: the built binary's split (`boot-local`: stderr only its
  error, the record its INFO; raised with `STORMBLOCK_CONSOLE_LOG=info`;
  `slab list` unchanged); full nextest 966/967 (#134); every initramfs test
  under sh and busybox sh. Not on metal: server8's console on a release with
  this engine

### A stormupdate reboot or rollback reads as the same release (2026-10-07, #265, P1) — WAITS ON stormupdate#3

Engine side done in #122 (staged goldens keep the release's ids; N's set
aside as `<name>@<N>`; held counts sealed goldens only). Re-verified on dev at
b30cecf: #122's four tests pass. Since #311 a misread is a system-half
re-lay, not a wipe; #244 fixed the same-id data loss in that path. Left:
the on-metal stage → reboot → rollback → reboot check, which needs
stormupdate's sequence (stormupdate#3, v0.0.0 today) and a stormcos release
with the #122 engine (11.88 is 9cbe6ca, before it). Proposed after
stormupdate#3.

### A local root that does not come up falls back to the claimed image (2026-10-07, #244, P1) — DONE

server1 11.56: a held disk whose root would not mount stopped at a shell.
- [x] `/init`: `launch_local` (everything from the diskless claim to the
      engine's start), `wait_root`, block `root fallback`: once per boot,
      from a local disk (no hook), an appliance known, the name not a guess
      (#249): stop the engine (SIGTERM, zombie-aware wait, SIGKILL), claim if
      needed, `SLAB=$CLAIMED`, `INSTALL_OVER=<disk>`,
      `STORMBLOCK_RELAY_SYSTEM_HALF=1`, launch again; root not appearing,
      plain mount, overlay lower
- [x] engine: `STORMBLOCK_RELAY_SYSTEM_HALF=1` skips the up-to-date shortcut
      (`holds_everything`)
- [x] found by the test: an install of the release the disk already holds
      lost the node's data volumes that share the claim's ids (adopt kept the
      claim's record) — `install::adopt` drops the claim's same-id copy first
- [x] tests: `tests/initramfs-root-fallback.sh`, every initramfs test under
      sh and busybox sh, the generated /init parses (both); the held-disk
      install test, relay unit test, #311's two install tests on dev
- Not on metal: needs a stormcos release with this initramfs and engine

### First boot slower while the flow-over runs (2026-10-07, #278, P1) — DONE

stormcentral's metrics: pvetest1 boot→apiserver 223 s (11.73), 334 s
(11.78), and since 11.82 (stormblock 9cbe6ca ⊇ bc825e7, 71785d4, the
reopened-#269 work) 63, 74, 65, 109, 92 s; Dell 325 s (11.79) → 179 (11.82),
165 (11.88). The issue's target is met. What is left is the owner's comment:
a *reboot* during the flow-over on the Dell's SMR disk still runs stormpump
15.1 s vs 7.9 and the apiserver 30.2 s vs 15.3 (11.88): boot reads compete
with the moves. Owner's suggestion, the engine-only form: a boot grace.
- [x] the successor's flow-over waits, before its first move, until volume
      I/O has been quiet for 10 s or 90 s have passed
      (`STORMBLOCK_FLOW_BOOT_GRACE_SECS`, 0 = off); `flow_over_remaining`
      reported during it
- [x] unit tests (quiet node starts after the quiet window; a busy one at
      the bound); full nextest at 22aface 956/959 (the qcow2 deadline,
      #134, #305 — the erase test 3/3 alone); README, CHANGELOG
- Not on metal until a release carries it: the Dell's reboot phases

### #239's fix on metal (2026-10-07, #257, P1) — HALF VERIFIED, waits on stormcentral#503

From stormcentral's install records and serial logs (no install of mine):
11.88 carries stormblock@9cbe6ca (⊇ b189b3e, 2ceec10) in engine and
initramfs. pvetest1 (11.88) and pvetest2 (11.88-flowsdn): fresh slab,
power cut mid flow-over, `a flow-over cut short. Finishing it from …`,
durability 301/300, **0** ext4 errors in the full serial logs. server3's
SOL capture dies inside stormbootx on every run (no kernel lines), so the
blade proves nothing either way, and the 11.57 question cannot be answered.
Not verified: `e2fsck -fn` of the service clones after the flow-over (no
run lasts that long; pvetest1 had 2320 extents left after ~5.5 h, #282) →
stormcentral#503 (after-settle ext4 check as a run check, blades' SOL).
#257 proposed after it.

### Live migration of a VM disk: ANA, epoch at the target (2026-10-06, #83, P3) — DONE

Item 3 (durable export table) was done in 2603646. Item 2 now has its
contract: stormstorage#33 settled #6's leg attach contract (epoch on attach,
412 stale, fence revokes lower-epoch attachments, per host, persisted).
Item 1 (ANA) is the target's alone. Cross-node multipath works through the
per-volume serve subsystems (`<prefix>:vol-<uuid>`, NSID 1: one NQN per
volume on every node); the shared and per-host subsystems name the node.
- [x] target: a namespace removal drains (revoked flag + in-flight count,
      checked right before the device op, after any R2T data): a removed
      namespace's commands fail Invalid Namespace, and the removal returns
      only once nothing in flight can still land
- [x] target ANA: CMIC (multi-port, multi-ctrl, ANA), OAES ANA change,
      ANATT/ANACAP/ANAGRPMAX/NANAGRPID/MNAN, NMIC shared + ANAGRPID in
      Identify NS, log page 0x0C (groups by state: 1 optimized, 2
      non-optimized, 3 inaccessible, 4 persistent loss, 5 change), ANA change
      AEN, I/O on a non-serving state fails with the path status (SCT 3);
      per-volume state process-wide (every subsystem serving it); a cntlid
      range per node (`[nvmeof] cntlid_min/max`) so two nodes' controllers
      of one subsystem never collide
- [x] API: `GET/PUT /api/v1/volumes/{id}/ana {state}`, kept in
      `<data_dir>/ana.json` (a node told "inaccessible" stays so across a
      restart)
- [x] /v1 epoch (#6's contract): `epoch` on attach (412 stale; absent after
      a fence = 412), attachment records with epoch on the volume, fence
      revokes lower-epoch attachments before answering (namespace out of the
      host's subsystem, drained; shared namespace released; ublk removed);
      an attach that raced a fence undoes itself
- [x] `POST /api/v1/volumes {id}` (one volume, one NGUID, on two nodes)
- [x] tests: target units 42/42 and the targeted suite 118/118 on dev
      (`integration_ana_epoch` 2); docs/migration.md, README, CHANGELOG,
      contract README; stormblock-csi#42 (send `epoch` on attach)
- [x] `ci-ana-verify.sh` on dev at 720f60d, ALL PASS (10 guest checks,
      kernel 7.2.8): one multipath head over two engines, reads follow the
      ANA moves both ways with no reconnect, a fenced leg goes from the
      guest, a stale reattach 412. Found by it: an empty `change` group made
      Linux arm ANATT and reset the controller (now only listed when used).
      Full nextest at 8f2ae03 953/955 (#134, the qcow2 deadline, #173
      class); `--features cluster` checks
- Not on metal: a stormvm live migration through stormstorage's heads

### A ready fstemplate whose sealed volume is gone (2026-10-06, #281, P1) — DONE (golden-stormblock-ae10dc337da8)

rustkube-node#140: every claim of `pvc-ext4j-64m` failed (404 `volume … not
found`, or 500 `has no sealed snapshot`) while the template listed `ready`.
- [x] `FsTemplate.broken` (reason; beside `state`, so an older engine still
      reads the store); listed as `state: broken` + `broken`; the listing
      also looks for the volume live
- [x] `verify_ready` at startup (after `resume_formats`): re-seal a volume
      that is there and unsealed, mark broken when missing / not recorded /
      will not seal, clear when whole
- [x] clone/claim check the volume first: 409 `fstemplate … is broken: … It
      is not sealed and cannot be cloned; delete it and mint it again` ("is
      not sealed" kept for rustkube-node e3ca68d's re-mint)
- [x] test `integration_fstemplates::a_ready_template_whose_volume_is_gone_…`
      on dev; full nextest at 3af8e7c 945/945; README, CHANGELOG;
      rustkube-node#140 told (match `is broken` / `state: broken`)

### Install keeps the data half: only the system half is re-laid (2026-10-06, #311, P0) — DONE (golden-stormblock-3fed70dce709, stormcos#353)

Owner (2026-10-06, the #1 rule for installs): an install touches the system
drive only, and only its system half; the data slab and every data volume
are adopted, not recreated; the release's keep/replace/migrate policy (#122)
applies; a failure never falls back to a wipe (stop and report, data
untouched). Reverses #261's install = wipe (7eaa520).

Found reading: the update path (`take_local_disk`, a disk with both halves,
no force) is half built. `boot-local` resolves its exports from the claimed
image *before* the disk is taken, and the kept data slab is registered
without its records ("adopting them … is the upgrade path, which is not built
yet"), so the node ran fresh data volumes from the appliance beside unread
old ones of the same names (stormcos#236).

Plan:
- [x] engine `image::install` (`plan`, `adopt`) (in `take_local_disk`'s update
      path, all checks before `update_system_slab` writes anything): read the
      kept data/bulk slabs' records and the release's policy
      (`/etc/stormblock/data-volumes`, unlisted data = keep); refuse (data
      untouched) when a kept volume has a leg outside the data half or the
      records cannot be read. Then: keep = the node's volume (same id, same
      name), the image's fresh clone deleted; replace/migrate = the node's
      renamed `<name>@<old release>` and kept, the image's takes the name;
      new = the image's; a sealed golden of another id = the node's renamed
      aside (its clones still name it by id). The data slab's records are
      adopted into the manager, metadata routed to the local slabs, and what
      is left of the image's data half flows in (`data_flow`). Migrations
      listed for stormupdate (handover record → generations file)
- [x] `boot-local`: take the local disk before the exports are resolved, so
      the mounts name the kept volumes
- [x] `/init`: an install over a disk with a data slab is never `force`d
      (probe `INSTALL_OVER`, `slab holds` 1, the install ticket); console
      `INSTALL: replacing the system half of <disk>; its data half is kept`.
      A lone data slab with no system half is left alone (said so)
- [x] tests: install N, node writes `state` (keep), `logs` (replace) and a
      volume no release names (a PVC); an extra drive with known bytes;
      install N+1 over it: every byte checked after the install, after the
      flow-over and from the disk alone; the extra drive untouched; a kept
      volume with a leg in the system half stops the install, data
      untouched. Initramfs tests (boot-hook) updated
- [x] docs (boot-hooks.md, staging.md, durability/README), CHANGELOG; answer
      stormcentral#462's question on #311 (same id and name)
- Verified on dev: both install tests; every initramfs test under sh and
  busybox sh; full nextest at fa6706d 942/944 (#134, the qcow2 deadline,
  #173 class); cluster checks. Golden golden-stormblock-3fed70dce709
  (stormcos#353). #317 filed (default_role prefers System). Not on metal: a
  stormcos release with this engine + initramfs installing over a running
  node (stormcentral#462's standing gate)

### A write to a just-moved extent lost on a cut before the persist (2026-10-06, #277, P1) — DONE (in golden-stormblock-3fed70dce709)

By reading: `move_slot` and `migrate_leg_unlocked` allocate the destination
at the source's generation while `rewrite_legs` bumps the map's by one. A cut
between a write+fsync to the moved extent (in place on the destination, its
entry published by the fsync) and the next persist leaves two slots at equal
generations; restore keeps the record's, the source: the write is gone.
- [x] test `integration_power_cut::a_write_to_an_extent_just_moved_survives_
      a_cut_before_the_persist` (+ `_in_format_v2`): fails with
      `MOVE_SAME_GEN_277=1` (block 0 reads its pre-move value, not 77), passes
      with the fix (f74491f)
- [x] a moved primary is allocated at `generation + 1` (= the map's after
      `rewrite_legs`), `PlacementEngine::moved_generation`, both paths; a
      mirror or parity leg keeps its generation (they tie by design: #316)
- [x] durability.md rule 5, CHANGELOG, #316 filed
- [x] targeted on dev: 50/50 (power cuts, placement, flow-over, drain,
      migrate); full nextest at eba4123: 939/941 = #134 and
      `a_flow_over_copy_holds_no_lock_the_node_needs` (#297/#305 flake,
      but it goes through `migrate_leg_unlocked`)
- [x] that test alone ×5 on dev at eba4123: 5/5 (0.16–1.4 s): the flake

### Optional mount entries `?vol:path` (2026-10-06, #288, P1) — DONE

stormcos#208: one stormpump golden serves every flavor (cilium, flowsdn), so
its `/etc/stormblock/mounts` must name per-flavor volumes that a release may
not have. A `?` line is mounted when the slab has the volume and skipped
otherwise; plain lines stay required; `rd.stormblock.mount=` still wins (and
takes `?` too).
- [x] `/init` mount list block: `mounts_optional <slab>` resolves `?`
      entries from `slab volumes` (one listing, only when there is a `?`);
      absent = "optional, not in this release"; listing unreadable = skipped,
      said so. Done before the ublk numbering, so indices stay in step
- [x] probe: a `?` entry is never counted missing (ef4c376)
- [x] tests (`tests/initramfs-mounts.sh` +10, `tests/initramfs-no-appliance.sh`
      +2): every initramfs test under sh and busybox sh on dev, the generated
      /init parses; README, CHANGELOG
- Not on metal: needs a stormcos release with this initramfs; stormcos then
  moves the list into the stormpump golden with `?` entries and drops
  `rd.stormblock.mount=` (guarded against older initramfs, #236)

### Forge 13.7.0 → ≥ v20 (2026-10-06, #235, P1) — WAITING ON THE OWNER

No code left here: the intent route shipped in v20.0.0. Forge still answers
13.7.0. The owner approved an in-place upgrade on 2026-09-28, then said on
2026-10-01 "no forge upgrade — forge is recreated" (stormcentral#210). Asked
on #235 (needs-owner): upgrade in place, or close as superseded. Noted
there: format 2 is the default since #158 (44fc8e3), so an upgraded forge's
slabs migrate to v2 and only the VM snapshot rolls back. The upgrade is a
root step on the VM host, not this session's.

### /serve/v1 served every /api/v1 export on an open portal (2026-10-06, #217, P0) — DONE

By reading, confirmed: reconcile step 1a wires every NVMe/iSCSI entry of
`state.exports` on a per-volume portal with `HostAccess::Any` and rewrites
its nsid to 1; and the engine's `restore_exports` puts serve's own entries
(nsid 1, no subsystem) on the shared subsystem at NSID 1 at every start.
- [x] `ExportEntry.serve` (set by `/serve/v1`); an unmarked entry is serve's
      only by serve's per-volume name and no host binding
      (`serve::wiring::serve_owned`), marked on the first pass
- [x] reconciler wires serve's entries only; a row it made for another
      export drains; `restore_exports` leaves serve's to serve (465deb9)
- [x] `integration_serve_own_exports` (2): host-bound and shared exports keep
      NSIDs 3 and 7, no portal in the range answers for them (in-house
      initiator), an earlier engine's row drains, serve's legacy and marked
      exports are wired; restore puts the shared one back at NSID 5 and not
      serve's. Both fail with `serve_owned` = always (the old rule). Docs
      (nvme-access.md, README), CHANGELOG
- An `/api/v1` shared export whose NSID an earlier engine rewrote to 1 is
  restored at 1: the number it had is not recorded anywhere

### A power cut during the install's flow-over (2026-10-06, #172, P0) — DONE here, metal acceptance stormcentral's

The fix is in (9344473 resume from a fresh clone; b189b3e relocate-on-write
#239; 81537b0 record_flow_over #258); what is open is the on-metal
acceptance (stormcentral's: BMC cut ×3 mid-flow-over, every acknowledged
write verified). The in-process tests cut at clean points only (every write
already in the file). Before handing on:
- [x] `emulated://…&volatile=1`: writes held in a cache until a flush;
      `emulated::crash(name, seed, keep)` keeps a random subset (a power cut)
- [x] `open_slabs_resuming` and friends open `emulated://` (not only
      `nvme-tcp://`) as a device path
- [x] test `cli::install_tests::a_power_cut_anywhere_in_the_flow_over_keeps_
      every_acknowledged_write`: system, data and a stamped service clone
      (owns extent 0 on the claim) written with logged fsyncs; cut at 10, 60,
      75, 95 %; half the cache lost; resume from a fresh claim; checked after
      the resume, after the flow-over and from the disk alone. Passes on dev;
      `RELOCATE_OFF_239=1` fails it (acknowledged owned-extent writes read as
      the golden after the resume: #172's failure)
- [x] docs (durability.md, README), CHANGELOG; full nextest at 24297af
      929/931 (#134; the pressure flake #279, 5/5 alone); golden
      golden-stormblock-934b03fe8db5
- On metal: stormcentral's BMC cut ×3 with every acknowledged write checked,
  on a release with golden-stormblock-32186e26d36f or later (#301: earlier
  post-#155 releases do not boot at all)

### No release boots: the post-#158 initramfs cannot open forge 13.7's slabs (2026-10-06, #301, P0) — DONE (golden-stormblock-32186e26d36f)

11.87: `open slab nvme-tcp://…: bad slab magic` for slabs forge's 13.7.0
engine composed (`compose/slab`, `compose/disk`); 11.82's initramfs
(9cbe6ca) opens the same layout. The new engine must read v1 slabs, and the
format gate must never turn a v1 slab into "bad magic".
- [x] cause: not the format. #155's `SlotTable::scan` reads
      `total_slots × 64` bytes, and the `nvme-tcp://` initiator refused I/O
      that was not whole blocks (`check_aligned`); 13.7 padded its table read.
      The partition scan swallowed each partition's error, leaving the whole
      disk's "bad slab magic". Files and O_DIRECT (RMW) accept partial blocks,
      so no test saw it
- [x] fix: the initiator reads the covering blocks / read-modify-writes them
      under one connection hold (f29f1a0); `slabs_in_partitions_why` names
      each partition's error. Test `integration_nvmeof::a_slab_on_an_nvme_tcp_
      namespace_opens_in_either_format` (v1 and v2): fails with the old rule
      re-imposed on dev, passes with the fix
- [x] full nextest at 2639a5d: 927/929 (#134 and the qcow2 deadline, #173
      class); staged golden-stormblock-32186e26d36f, stormcos#304
- Not on metal: a release composed with this golden (and its initramfs)
  booting from forge 13.7's slabs is the proof; until then compose with
  `--pick stormblock=golden-stormblock-bf3921ab490c`

### Boot-chain attestation and the per-machine TPM mark (2026-10-06, #216, P2) — DONE

For stormcert's `require.attestation` (stormcert#23, #53). Owner: TPM is
marked per machine, set by the platform, never self-reported. The shape was
left to this side:
- `Host.tpm` (`required` | `none`, unset = none) + `tpm_set_at`, in
  `synonyms.json`; `PUT/DELETE /api/v1/boothost/{name}/tpm` are destructive
  (admin token or a SubjectAccessReview `update`/`delete` boothost): a node
  token cannot downgrade it. A host may be marked before its first claim.
- `Host.last_claim`: every boothost claim records clone, claimed_at,
  claimed_as, host NQNs it was bound to (#210), host golden, assigned golden
  and the assignment's version
- `GET /api/v1/boothost/{name}/attestation` (by name only, never an alias):
  host, tpm, requires (`boot_chain` | `tpm_quote`), host_nqns, clone, host
  golden, golden (sealed, synonym version/label, release digest when it is a
  published release), and the chain the engine checked now (clone exists,
  unsealed, parent = host golden; host golden sealed, parent = golden; golden
  sealed). Readable with the node/admin token or a Kubernetes bearer allowed
  `get` on `boothost` (stormcert's ServiceAccount, no shared token)
- [x] store + claim record (`synonym::{TpmMark, ClaimRecord}`), routes
      (`api/boothost.rs`), auth class `Class::Attestation` (ba6e3d9). Found:
      the kube review cache ignored the name; keyed by it now
- [x] tests: `integration_synonyms::an_attestation_states_the_boot_chain_
      the_engine_served`, `integration_destructive::stormcert_reads_an_
      attestation_with_its_own_bearer_and_only_that`; docs (auth.md, README),
      CHANGELOG; stormcert's two questions answered on #216
- Not proven: the claimant is the machine (stormcos#35); stage 2 (TPM quote)

### An install booted the old disk: no appliance, no check (2026-10-05, #294, P0) — DONE

server8, 11.82 over an older disk: forge missed the one 3 s health check
right after the mlx4 link came up, BOOTHOST stayed empty, the local-slab
probe and the release check (`[ -n "$BOOTHOST" ]`) were skipped without a
word, and `boot-local` died on `volume 'kubelet-data' not found`, scrolled
off above `FATAL: root device /dev/ublkb0 not found`.
- [x] `# --- BEGIN appliance discovery`: a boothost the network names
      (`/run/stormblock-boothost`) is asked again for `STORM_BOOTHOST_WAIT`
      (90 s); why there is none is kept (`BOOTHOST_WHY`)
- [x] the probe runs with no appliance (`probe_fallback`): a disk it would
      have sent to the appliance stops the boot (FATAL naming what is missing
      and `disk_release`); one that can boot says `RELEASE CHECK SKIPPED`
- [x] engine output to `/run/stormblock/engine.log`, followed onto the
      console (`tail -f`); `engine_report` repeats its last 25 lines after the
      root FATAL
- [x] `tests/initramfs-no-appliance.sh` and every initramfs test under sh and
      busybox sh; `ci-no-appliance-verify.sh` (the shipped initramfs in QEMU,
      no network): missing kubelet-data stops with a FATAL naming it and the
      release; an engine failure is repeated after the FATAL; a good disk
      boots. Docs (boot-hooks.md, README), CHANGELOG
- Not on metal: needs a stormcos release with this initramfs; then server8
  installing over its older disk (a named boothost answering late)

### Metadata format v2: paged index, u64, log, extent classes (2026-10-05, #158 + #157 + #156, P1) — DONE

The one format change (owner: #158 2026-10-01, #157 B, #156 1 MiB hot /
8 MiB bulk). Design and stages: `docs/metadata-v2.md`. Written behind a gate:
the engine writes v1 until stage E.
- [x] A. u64 slot/extent indexes in memory; v1 encodes u32 (refuses a slab
      past 4 Gi slots). Nothing on disk changed: bincode's varints make a
      u64 below 2^32 the same bytes (`metadata::u64_slot_compat`). Extent
      entry 28 B; GEM 29.0 B/extent (33.9 scattered). nextest 902/902,
      `--features cluster` checks (b0a2323)
- [x] B. v2 reader/writer: slab header v2; metadata region = superblocks +
      COW B-tree + log + checkpoint + recovery; `metadata.v2` in the data
      dir; persist appends changes (#157); gate `[metadata] format = 2`.
      `volume/metav2.rs` (store), `volume/persist_v2.rs` (sinks, ordered
      batches), `gem::Changes`; `metav2::read_slab` for every reader. On
      dev: nextest 920/920 with the gate off AND on (every slab v2), power
      cuts 300/300 in both formats, `--features cluster` checks;
      `examples/persist_cost` 100k extents: 4 KiB / 2.7 ms per persist (v1:
      2.7 MB / 9.4 ms)
- [x] C. extent map in memory as a cache of the tree (load on open, evict
      idle clean maps, `cache_mb`). Plan (2026-10-05):
      - [x] C1 the v2 header carries the volume's extent size (format final
            before anything ships; #156 uses it in D)
      - [x] C2 `MetaV2::scan_volume` (one volume's keys, tree + log)
      - [x] C3 GEM: a map is Resident or Cold; every accessor on a Cold map
            panics (loud, never "no extents": that reads zeros, allocates over
            and lets GC free its slots)
      - [x] C4 paging in the manager: a handle's entry points and the
            manager's per-volume paths load a cold map; evict when clean (its
            sinks written, nothing pending), unheld, no handle outside the
            manager, at least one v2 sink carries it; LRU under
            `[metadata] cache_mb`
      - [x] C5 walkers stream cold maps one at a time (GC's live set becomes
            per-slab bitmaps; drain, rebuild, placement, flow-over, restore)
      - [x] C6 the whole suite with the gate on and the cache at 0 (every
            eligible map evicted after each persist): a missed path panics;
            measure resident bytes with cold goldens. Done: forced 920/921
            (the qcow2 import deadline under load, #173 class), gate off and
            on 921/921, cluster checks; map_cache: 12.4 B/extent, all freed
            cold, 0.4 ms to load 2 000 extents. Walks that pin (load all,
            transiently): flow-over, drain, resync, data seed; GC streams
- [x] D. extent size per volume/pool (#156); the install's 8 MiB bulk slab.
      Pickers by size, creation rule (asked / ≥64 GiB bulk / default),
      clones and compositions keep size, restore checks a volume's size
      against its slabs, install lays data (¼, ≥64 GiB) + bulk (last) when
      format 2 and the data half ≥ 256 GiB; `/v1 extent_size_bytes`
      (stormblock-csi#37). Full suite gate off/on/forced green (bar the
      qcow2 deadline flake once, gate off)
- [x] E. `slab upgrade` (in place, header last), gate default v2 for new
      slabs, emulated 1 PiB runs (#208), docs; close #158, #157, #156.
      Built: migration (`Slab::upgrade_to_v2`: record into both v1 copies,
      v2 store in the region's first half, header last; a cut test), at the
      first persist once 2 is the default (owner/master on #158: "old slabs
      migrate on first open"), `slab upgrade`, `POST /slabs/{id}/upgrade`
      (destructive). Default flipped to 2 (44fc8e3; #301 was its fallout,
      fixed). Scale run on dev (2026-10-06): `integration_metadata_v2::
      a_v2_node_on_petabyte_drives_keeps_its_volumes_across_restarts`
      (ignored, 145 s): 1 PiB + 2 × 256 TiB emulated, one migrated from v1,
      extent indexes past 2^32, mirror, goldens + clone, cold maps, two
      restarts, every byte back; restart 39 s = slot tables (#307). Slot
      index past 2^32: `a_v2_slab_past_four_gi_slots…` passes (open 58 s).
      Full nextest at 54084cb: 938/939 (#134). Closed #158, #157, #156

### Resident compaction (2026-10-05, #155, P1) — DONE

docs/metadata-scale.md §3.2, no on-disk format change. Measured with
`examples/metadata_footprint` on dev (4 M slots, heap bytes counted):

| | before | after |
|---|---|---|
| a free slot | 40.6 B | 0.6 B (the free map) |
| an allocated slot, slab side | +65.8 B | +4.1 B (the bounded page cache) |
| an extent in the GEM | 220.5 B | 25.0 B (29.4 scattered, 25.0 clone maps) |
| per PB written, extrapolated | 313.7 GB | 28.7 GB |

- [x] 1. GEM without a reverse index: `slab_extents`/`slab_parity` walk the
      maps (one entry per slot); a slot's owner (share counts after a
      copy-on-write, `sync_refs`) is its slot table's. The flow-over takes one
      list per pass; a pass skips what changed under it (64 passes that move
      nothing count as one failure). Found by the live-clone flow-over test:
      counting each stale item against the per-extent retry limit failed it
- [x] 2. `volume/extable.rs`: 24-byte entries (slab ordinal from a
      process-wide interner, slot, share count, generation), chunks of 64
      virtual extents, mirrors out of line; `lookup` returns a location by
      value; the record on disk unchanged (`to_btree`)
- [x] 3. `drive/slottable.rs`: the slab keeps its free map and `pending`
      (entries that differ from the device) only; entries read through a
      page cache (`STORMBLOCK_SLOT_CACHE_MB`, 16); open, restore (`SlotView`,
      transient) and GC read the table in one pass; GC scans with no lock and
      re-checks each orphan under it; hot paths prefetch pages before taking
      the registry (#269)
- [x] full nextest on dev: 891/891; `--features cluster` checks
- Restore still holds every allocated slot's entry for its duration
  (`SlotView`): a transient spike at PB scale, gone with #158's paged index

### Emulated drives for scale tests (2026-10-05, #208, P1) — DONE

stormcos#92 / #204 (owner: a simulator first). `drive/emulated.rs`:
`emulated://<name>?size=…[&backing=<dir>]` reports any capacity and stores
only what is written (memory pages or 1 GiB sparse chunk files; zeros and
discards store nothing); one name = one drive per process; `DriveType::
Emulated`; `[[drives]] kind = "emulated"`; `POST /api/v1/drives/{id}/emulate
{failed}` (EIO). Slab format zeroes its table through `write_zeroes`.
- [x] tests: unit (4), `integration_emulated` (mirror on 3 × 256 TiB rebuilds
      after one fails; 1 PiB over the API; config); full nextest 898/898
- [x] `examples/emulated_scale` on dev: ~130 MiB/PiB resident (the free map),
      reopen 3.3 s per 1 PiB; docs (README, metadata-scale.md), CHANGELOG
- Left to stormcos#92: the node-level `long` suite on 160 of them

### Stage the next release on a running node (2026-10-06, #122, P1) — DONE

Owner, 2026-10-06 (on #122, master's recommendation accepted): **1(b)** a
release may mark a data volume replace or migrate (hook the release ships);
**2(A)** stormupdate re-points `boothost/<tag>` to N+1 before the reboot,
install = wipe unchanged. Owner's note on #122 (2026-10-06 00:47): an install
should wipe only the system half; application data survives — not this
issue (#261's install path), filed separately.

Design (engine side, stormupdate#1):
- `drive/httpdev.rs`: a read-only `http://` image by Range GET (from
  `examples/slab_audit`), writes held in memory
- `image/stage.rs`: open N+1's image, read its policy
  `/etc/stormblock/data-volumes` from its root volume (`<vol> keep|replace|
  migrate [hook]`; unlisted: system → replace, data → keep), and copy into the
  local slabs as `<name>@<v>`: a golden under **its own id** (so `slab holds`
  answers held, #265), unsealed while copying and sealed when complete; a
  clone as a CoW clone of its local parent plus its own extents; a volume
  whose id is already here is shared, not copied. Boot pallet copied
  **below** the active one (`lay_local_boot` placement)
- `slab holds`: a golden counts only when the local copy is sealed (a stage
  cut short is never "held")
- `<data_dir>/release-generations.json`: current, staged (complete?), previous
- API (destructive class): `POST/GET /api/v1/releases/{v}/stage {source,
  current?, root?, disk?}` (a job), `POST /{v}/activate` (N's → `<n>@<N>`,
  staged → plain, pallet raised), `POST /api/v1/releases/rollback`,
  `GET /api/v1/releases/generations`. The previous generation is deleted
  when the next one is staged. Migrations are listed for stormupdate to run
  the hook; the engine runs nothing of the release's
- [x] httpdev, `CreateOptions.id`, `rename_volume`, `lay_local_boot_ranked`
      / `raise_local_boot` / `local_boot_ladder`, held needs sealed (017482b);
      `image/stage.rs`, `api/release_stage.rs`, auth (982132c)
- [x] tests: `cli::install_tests::a_release_stages_activates_and_rolls_back_
      on_a_running_node` (install 11.90 from a claim, node writes `state` and
      `logs`, stage 11.91 over HTTP Range, plan: state kept, logs staged
      (replace), kubelet-data added, svc a CoW clone, golden under the
      release's id; held for both; an unsealed copy not held; activate; the
      disk alone reopened: root/svc/logs N+1's, state the node's, logs@11.90
      the node's; rollback; discard) and `stormupdate_stages_activates_and_
      rolls_back_over_the_api` (job, generations record, 404/409s, held from
      the disk alone, rollback restages, discard, the stage after removes the
      generation before last but keeps state.golden@11.90, which the node's
      state is cloned from); `local_boot::a_staged_release_waits_below_the_
      active_one_until_raised`; `release_staging_verbs_are_destructive`
- [x] docs/staging.md, README, CHANGELOG; stormcos#326 (the policy file),
      stormupdate#1 (the sequence); #311 (owner's install note, needs-owner)
- Not end-to-end: a boot pallet through the API (the test images carry
  none; the ranking is unit-tested), a real stormupdate run, metal

### The initramfs names the node and says why (2026-10-05, #238, P1) — DONE

stormcos#191: C2NR0Q2 registers as `storm-06f96d` with a reservation and a
confirmed PTR naming it `stormblock1` (microdns sends no option 12,
microdns#14; with no DNS server in the lease the PTR step was silent).
- [x] `# --- BEGIN node name`: option 12 > forward-confirmed PTR > storm-<mac>,
      each step saying why it gave none; the domain (option 15/119, else the
      name's own) set as `/proc/sys/kernel/domainname`, the FQDN printed
- [x] `# --- BEGIN dhcp name hint`: udhcpc `-x hostname:<name>` from the
      declared `[node] hostname` (stormcos-state `/config/stormcos.toml` on
      the local disk), else the firmware's boot name (#249); never a guess
- [x] `tests/initramfs-node-name.sh`; every initramfs test under sh and
      busybox sh; `ci-node-name-verify.sh` (QEMU, filter-dump: option 12 =
      stormblock1 in the guest's DHCP request); docs (README), CHANGELOG
- Not on metal: needs a stormcos release built after this; C2NR0Q2 then
  registers `stormblock1` once microdns#14 sends option 12 or 6

### Destructive verbs need the admin token or a storage-admin SAR (2026-10-05, #274, P1) — DONE

Owner, 2026-10-05: **B**. The node token keeps the ordinary verbs (clone,
attach, detach, create, and delete an unsealed volume that is no template or
golden). Destructive: slabs (format, delete, gc), arrays and their members,
spares, forge on/off, sealed goldens and templates, pallets' table-writing
verbs, an emulated drive's fault, and today's `is_destructive` list (with
detach-like DELETEs carved out).
- [x] classification (`serve::api::classify`): Public / Ordinary /
      Destructive / VolumeDelete(id) (destructive when sealed, a template
      or a golden)
- [x] an admin token always: config/env, else read or minted into
      `admin_token_file` (default `/run/stormblock-admin/admin_token`, 0600:
      never under `/run/stormblock`, which every service mounts)
- [x] a Kubernetes bearer for a destructive verb: TokenReview, then
      SubjectAccessReview (`storage.storm.io`, resource = path segment, verb
      delete/create/update), cached briefly; `[management.kubernetes]`
      api_url, ca_file, token_file (stormcos provides)
- [x] `admin_gate = "enforce"` (default) | `"audit"` (allows the node token
      and logs who would be refused, for a rollout); `STORMBLOCK_ADMIN_GATE`
- [x] audit log: every destructive call (who, what, target, decision,
      status) to `<data_dir>/audit.log` and the log
- [x] tests; docs (auth.md, README), CHANGELOG; issues for the callers that
      break (stormcluster forge, stormstorage arrays, stormdrive slabs and
      drives) and stormcos (admin token mount, kube credentials, the gate)
- [x] verified on dev: `integration_destructive` (4), `integration_auth`;
      full nextest 901/902 (the one a 2 s timing test under load, 6/6 alone,
      filed). Callers that break under enforce are filed; stormcos is asked to
      set `STORMBLOCK_ADMIN_GATE=audit` until they move

### Decide: #5–#7 in the engine or on stormstorage's heads (2026-10-05, #179, P1) — DECIDED (b)

Owner, 2026-10-05: **(b)**. Cross-node RAID1 (partner prestage, promotion,
bounded dual-attach) is built on stormstorage's RAID heads, over NVMe/TCP legs
from the engines' dedicated arrays (#149, #150), not in the engine.
- #5 (prestage) and #7 (dual-attach): stormstorage's, via stormstorage#33,
  re-scoped there; closed here with a pointer.
- #6 keeps the engine's part: epoch fencing on leg attaches (a fenced head's
  leg attach refuses its writes). Built when stormstorage#33 settles the
  head ↔ engine contract.
- The `/v1` replica surface stays as it is, control plane only (README "Not
  built"). Whether stormblock-csi reads sync state from the head through the
  engine's `/v1` or from stormstorage directly is settled on stormstorage#33
  and stormblock-csi#29, not changed here first.

### Extent size by pool class (2026-10-05, #156, P1) — AFTER #158, two questions asked

Decided (owner, 2026-10-01): extent size is a per-pool property fixed at
creation, 1 MiB for hot pools, 64 MiB for bulk. Read, not built: the volumes
document has one `extent_size` for the node (`VolumeMetadata.extent_size`),
and `VolumeManager` refuses a slab of another slot size (`mod.rs:653`, `:903`).
A per-volume extent size is a new record field, a format change, so it ships
in #158's one format change (as noted on #156 on 2026-10-02). Asked on #156
(needs-owner): (1) which volumes are bulk, (2) where 64 MiB slabs live on a
node disk.

### Incremental metadata persistence: change log + checkpoints (2026-10-05, #157, P1) — AFTER #158

Owner, 2026-10-05, on #157: **B**. The change log lives in each metadata
slab's region (where the initramfs and `boot-local` read the record), which
is a new region format. It ships inside #158's one format change: paged
extent index, u64 indexes, format version, in-place migration (owner on
#158, 2026-10-01). No separate region format. #157 is proposed after #158.
- The in-memory change tracking (what changed since the last persist,
  per volume and extent; GEM dirty sets) is built with #158: on its own it
  has no consumer.
- Design: docs/metadata-scale.md §3.3–3.4 (the log is checkpointed into the
  paged B-tree; recovery replays it; the versions-before-map order of
  `volume/versioned.rs` holds through it).

### Forge mode on by default on every node (2026-10-05, #287, P1) — DONE

Owner (stormcos#273): "a sno node should have it by default." #272 left it
off until an admin `PUT`, and nothing off the node holds that token.
- [x] `forge.json` is the persisted state: settings (on) or
      `{"enabled": false}` (off); a file from #272 reads as on, an unreadable
      one as off (loudly)
- [x] `adopt-ublk`: nothing persisted = on with `forge::default_settings`
      (`0.0.0.0:4420`, `nqn.2026-08.lo.storm:<node>`, the #210 closed
      policy), never written down; persisted off = off. The daemon keeps
      #272's rule (`restore(state, None)`: off unless kept)
- [x] `PUT` persists on with settings; `DELETE` persists off (from the
      default too). `GET`: `state` on|off, `from` default|persisted|config
- [x] tests (`integration_forge`, 4); docs (README, auth.md), CHANGELOG
- Not on metal: needs a stormcos release with this engine; then server8
  (fresh SNO) answers a boot claim with no PUT (stormcos#273 step 3)

### Secure delete: freed data overwritten before reuse (2026-10-05, #286, P1) — DONE

Owner's request: overwrite deleted data so it cannot be recovered. One pass by
default, DoD-style multi-pass as an option; crypto-erase designed now and built
later. `docs/erase.md`.
- [x] `SlotState::Erasing` (3; an older engine reads it as Free). `retire` is
      the one hook: a freed slot is marked, its entry written (the level in the
      share-count field, the owner kept), and it stays out of the bitmap.
      `Slab::open` re-queues it. `is_owned()` is used by GC, the GEM rebuild,
      restore, inc/dec_ref, reassign and `shares()`
- [x] `drive::erase::EraseLevel`: none | once | dod3 | dod7. Set by
      `[erase] default` (once) through `SlabRegistry::set_erase_default`; a
      delete may raise it with `?erase=` (`delete_volume_erasing`). Not set on
      fabric slabs
- [x] `volume::erase::Eraser`: batches, a flush after each pass, the last pass
      read back for dod, discard unless HDD, then `finish_erase` (a durable
      free); foreground first. Audit in `erasures.json` and
      `GET /api/v1/erasures`; metrics. Started by the daemon and `adopt-ublk`
      (`AppState::start_eraser`)
- [x] tests (slab, eraser, HTTP); full nextest on dev: 888/888
- [x] docs/erase.md, README, CHANGELOG
- Not built: a per-volume or per-class stored level; crypto-erase (design only)

### Install: the data half moves in the background (2026-10-05, #285, P1) — DONE

Dell 11.79 install boot: the data seed is 166 s of a 352 s boot (47%),
synchronous in `boot-local` before the root is exported — the rule from
before the slot fence (#239). With the fence and quarantined sources
(relocate-on-write) the data half moves while mounted exactly as the system
half does.
- [x] `FlowOver.data_flow` (serde default false); the fresh-lay path sets it
      instead of seeding (`STORMBLOCK_SEED_DATA_SYNC` = the old seed); the
      kept-data-slab update path is unchanged
- [x] quarantine and `record_flow_over` for both halves (`flowing_into`: a
      list of (dest, sources)); a resumed flow-over carries `data_flow`
- [x] `spawn_flow_over`: system half, then data half (`flow_slabs`, `extra`
      so `flow_over_remaining` never dips to 0 between), then local boot
- [x] tests: `a_fresh_install_seeds_every_data_volume_byte_for_byte` (now
      through the successor's two halves), `the_data_half_moves_while_
      written_and_a_cut_resumes_it`; full nextest on dev 880/880. Docs
      (README, durability), CHANGELOG. Needs an engine ≥ this with the
      initramfs (an older successor ignores `data_flow`)
- Not on metal: the Dell's next install — boot→apiserver ~160 s expected;
  the data half then moves for minutes in the background (persist per
  extent, #277)

### install-config.yaml from stormbootx onto /state (2026-10-04, #275, P1) — DONE

stormbootx#79 hands the boot media's `install-config.yaml` to Linux in
volatile EFI variables (`StormBootInstallConfig` = `v1:<len>:<N>:<sha256>`,
chunks `StormBootInstallConfig0..N-1`, 768 bytes each). `/init`:
- [x] `# --- BEGIN install config read`: right after the boot identity,
      verified (length, sha256), staged 0600 in RAM, every variable deleted
      whatever came of it (an unreadable header too); content never printed
- [x] `# --- BEGIN install config write` (before `root ready`):
      `/state/config/install-config.yaml` (0600) only when `/state` is in
      the mount table and has none; staged copy removed before switch_root
- [x] `tests/initramfs-install-config.sh` (23 checks); every initramfs test
      under sh and busybox sh on dev, /init parses; dev's busyboxes have
      `sha256sum` and `chattr`. Docs (README, boot-hooks), CHANGELOG
- Not on metal: a node booted from storminstall media with stormbootx ≥
  b5784f9 and a stormcos release with this initramfs

### NVMe/TCP: dropped connections said, shared namespaces held, target metrics (2026-10-04, #276, P0) — DONE

The build box's "Link has been severed" (~2.5 min into a 527 MB golden on
forge 13.7) turned out to be dev's own kernel (`Bad page state`, the
nvme-tcp sender failing). Still the engine's:
- [x] `duplicate IDs in subsystem`: on 13.7 one golden attached by two
      parallel builds got two NSIDs. The current engine gives it one NSID,
      but one detach removed it for every job using it: `NsRecord.holders`
      (attach `holder`, detach `&holder=`; an export holds as `export:<id>`;
      none named = the anonymous holder, as before; a pre-holder record is
      released as before; whole-volume detaches release all)
- [x] every connection the target closes is logged (info, warn unless the
      host closed it) with reason, host, controller, queue, lifetime,
      commands and last command; the target enforces no KATO and exports
      have no lease (documented)
- [x] `/metrics`: `stormblock_nvmeof_connections_{opened,closed}_total`,
      `_keepalives_total`, `_io_errors_total{op}`, `_io_seconds{op}`
- [x] tests (`integration_nvme_hosts`: holders, export holders, metrics);
      full nextest on dev 879/879 (one run; the pressure test flaked twice
      in full runs, never alone: #279). Docs (nvme-access.md, README),
      CHANGELOG
- Not on forge: forge is 13.7 until the new forge; stormcentral to send
  its job as `holder` (filed)

### #269 reopened: the API still stalls during flow-over on server3 (2026-10-04, P0 — THE most critical) — FIXED here, proof on server3 pending

11.79 (with bc825e7): the Dell's API answers through its flow-over, server3's
(one 7200 rpm ST2000DM008) does not. **Why, found with a task dump** of a
model (`cli::flow_over_api_tests::server3_api_during_flow_over`, ignored:
one actuator, seek per I/O, flush cost `FLOW_API_FLUSH_MS`, a slow appliance,
the node's own I/O, the real flow-over and router):
- `persist` and every volume `flush` held the **slab registry read lock
  across device flushes**; the first allocation (a clone's copy-on-write)
  queued for the write lock, and tokio's RwLock then queued every reader of
  every volume behind it. `persist` also held the **volume manager** across
  its flushes, and the flow-over persists after every extent. A flush of
  seconds (server3's disk) = the node's I/O and API stopped for seconds,
  chained. With 1.5 s flushes: clone 41 s, `GET /api/v1/volumes` 17 s.
- [x] instrument: `/debug/stalls|tasks|threads|locks` (open, read-only) and
      the stall watchdog (OS thread; > 10 s request → capture); tokio task
      dumps (`.cargo/config.toml` `--cfg tokio_unstable`, `taskdump`); every
      device flush timed (`drive::flushgate::summary`)
- [x] fix: `slab::sync_registered` (no registry lock across a flush),
      `MetadataWriter`, persist = records → sync (all slabs at once) → write
      (generation-checked), `persist_detached` (flow-over, mint); device
      flushes shared (`FlushGate`); unchanged metadata copies not rewritten;
      a mint's stamp leaves its flush to the persist; the flow-over yields
      its whole last move (≤ 2 s) to foreground I/O (API persists count)
- [x] model, after: 25 ms flushes clone p99 0.80 s, list 4 ms (target met);
      300 ms clone p50 3.4–4.8 s, list ≤ 0.1 s; 1.5 s clone ~20 s (6 durable
      round trips on one actuator: the disk itself, no engine lock held —
      both dumps show every lock free). Full nextest on dev 876/876
- Found and filed: #277 (a write to a just-moved extent can be lost on a
  cut before the next persist: equal generations, the record wins)
- Not on metal: server3 on a release with this engine; `/debug/stalls`
  then reports its real flush times

### An install never takes a shelf drive (2026-10-03, #273, P1) — DONE

The owner puts a NetApp shelf on C2NR0Q2 for stormraid (stormraid#1); the
Dell's line is `rd.stormblock.slab=/dev/sda rd.stormblock.assimilate=any`
and every install wipes. `any` took any non-slab drive (a stormraid member,
a foreign table); `force` and the install's data-slab scan took whatever the
bus listed. In `/init`'s survey block (`named_disk`, `drive_external`,
`drive_signature`, `may_take`):
- [x] the drive `rd.stormblock.slab=` names, when present, is the only one
      (survey loop and `local_data_slab`)
- [x] never a drive behind a SAS expander or in an SES enclosure
      (`rd.stormblock.allow-external=1` overrides)
- [x] "nobody's" = first and last MiB zeros; stormraid's `STORMRD1` named;
      `force` clears only the named drive when it carries a signature
- [x] 15 cases in `tests/initramfs-boot-hook.sh` (named + shelf + stormraid,
      expander path, tail signature, install over the layout, no intent with
      a data slab only in the shelf, allow-external); all initramfs tests
      under sh and busybox sh on dev, /init parses. Engine: nothing scans
      drives on its own (adopt-ublk/boot-local open only what /init names)
- Not on metal: the Dell with the shelf attached, next install

### The mount list leaves the kernel command line (2026-10-03, #262, P1) — DONE

x86's command line is 2048 bytes; the EFI stub truncates and boots anyway,
and `rd.stormblock.mount=` (one ~1.8 KB word) went whole: 11.68 mounted
nothing (stormcos#236). The issue's option 1: the list lives in the root
volume, `/etc/stormblock/mounts`, one `<vol>:<path>` per line, written by
stormcos at build time.
- [x] `stormblock slab cat --slab <s>… --volume <v> --out <file> <path>`:
      a file out of a volume's filesystem, nothing attached (userspace ext4);
      exit 0 read, 1 no such file, 2 the slabs/volume cannot be read
- [x] `/init` `# --- BEGIN mount list`: with no `rd.stormblock.mount=`,
      `mounts_from <slab>` reads the root volume's list — in the local-disk
      probe (that disk's own release) and again once the slab is settled
      (claimed clone or local), before `boot-local` exports anything; the
      cmdline still works and wins. The console says where the list came from
- [x] tests: `cli::slab_cat_tests`, `tests/initramfs-mounts.sh` (6 cases);
      on dev: nextest 875/875, every initramfs test under sh and busybox sh,
      the generated /init parses, `slab cat` CLI exit 2 on a missing slab.
      Docs (README), CHANGELOG
- Not on metal: stormcos moves its list to `/etc/stormblock/mounts` in the
  stormpump golden and drops `rd.stormblock.mount=` (stormcos#236 follow-up)

### Forge mode turned on per node, kept by the engine (2026-10-03, #272, P1) — DONE

For stormcos#187: one image, forge mode switched on a running node, no argv
or mount change on the stormcos side. Owner's preferred shape (option 1).
Every mode is a day-2 choice (owner, relayed on #272; #284): nothing is
chosen at install, and stormcos#82's install-config carries no forge role:
- [x] `GET/PUT/DELETE /api/v1/forge` (`mgmt/forge.rs`, `api/forge.rs`;
      PUT and DELETE need the admin token): PUT starts the shared target live
      (bound before it is published) and keeps the settings in
      `<data_dir>/forge.json`; DELETE stops accepting (open connections
      finish) and forgets them; GET reports `source` api|config
- [x] `adopt-ublk` (and the daemon with no export device) serves
      `forge.json` at start when `--config` has no `[nvmeof]`
- [x] the live `[nvmeof]` is `AppState::nvmeof_settings()` (policy, claim
      attach, `/v1` attach, usage)
- [x] a target the command line or `--config` set up answers 409
- [x] tests `integration_forge` (3), `forge_mode_is_set_by_the_admin`; full
      nextest on dev 874/874, `--features cluster` checks. Docs (README,
      auth.md), CHANGELOG
- The caller is stormcluster's day-2 operation (stormcluster#16), not an
  install. Since #287 a node is forge by default; `DELETE` turns it off

### Forge mode on a stormcos node: adopt-ublk serves NVMe/TCP (2026-10-03, #206, P1) — DONE

A bastion (stormcos#90) is a stormcos node that is also forge: `adopt-ublk`
read `--config` only for `/serve/v1` and never built the shared NVMe-oF
target, so `state.nvmeof_target` was None and a boothost claim's `attach`
was null. One engine owns the node's slab, so it is this engine that serves.
- [x] `adopt-ublk` starts the shared target when the config has an
      `[nvmeof]` section (`adopted_nvmeof_target`: `listen_addr`, `nqn`, the
      #210 host policy); no section = as now (stormcos's baked config has
      none). No raw drive namespaces: the slab is the engine's pool
- [x] the daemon's store/policy/restore/run sequence is
      `serve_shared_nvmeof`, used by both
- [x] tests `cli::forge_mode_tests` (no section → no target; with one, a
      boothost claim's attach is connected with the engine's initiator as
      the boot host's NQN and reads the release's bytes); full nextest on
      dev 870/870, `--features cluster` checks. Docs (README, here), CHANGELOG
- Not run as the real `adopt-ublk` on a node (needs root + a handover):
  the bastion install (stormcos#90, server8) is that check; stormcos's
  bastion unit passes `--config` with `[nvmeof]`

### The node API stalls during flow-over (2026-10-03, #269, P0) — DONE

11.78 on C2NR0Q2 (engine 507750d), the first real install onto its 2 TB
spinning sda: while the flow-over moves 7528 extents from forge, template
clones and `GET /api/v1/volumes` time out (60 s), so every claim and VM
stalls for the whole flow-over. By reading: `move_slot` reads the source
(forge, NVMe/TCP), writes sda and reads it back **holding the GEM and the
registry write locks**; every volume's I/O lookup and every API operation
waits behind each copy. The slot fence (#239) already keeps I/O off the
slot being moved, so the global locks are needed only to allocate and to
publish.
- [x] `flow_system_half` → `PlacementEngine::migrate_leg_unlocked`: copy
      with only the fence held (registry write to allocate, reserved; no
      locks for read/write/read-back; GEM+registry write to publish,
      re-checked, slab share count carried). Per-extent persist unchanged
      (bc825e7)
- [x] foreground first: the flow-over waits between moves (the last move's
      time, ≤ 250 ms) when volume I/O has run since (`thin::FOREGROUND_IO`)
- [x] item 3 is #260 (`flow_over_remaining`, already in 507750d)
- [x] test `a_flow_over_copy_holds_no_lock_the_node_needs`; full nextest on
      dev: 868/868 (the #239 live-clone, cut-short and power-cut flow-over
      tests included); `--features cluster` checks. Docs (README, durability
      rule 10), CHANGELOG
- Not on metal: the Dell's next install with this engine — claims and VMs
  during the flow-over, `flow_over_remaining` falling to 0

### The Dell never installs: a guessed name takes no slab drive (2026-10-03, #268, P0) — DONE here, proof on metal pending

C2NR0Q2 on 11.77 booted stormbootx 0.4.0 from the iDRAC's virtual optical,
which hands no name down, so `/init` named it from SMBIOS (a guess, #249).
sda held an older stormcos slab, so #249's guard left it and every boot ran
from a fresh forge clone (durability 0/300). Asking a guess to install over
any stormblock slab would be #249's own failure on the MicroCloud blades
(shared chassis serial). Owner's decision (2026-10-03): **B** — the master put
stormbootx v0.14.0 (hands `StormBootTag` down) on the Dell's virtual CD;
#249 stays. **No stormblock code changes.** With the name from the
firmware, the existing rules install onto sda: `tests/initramfs-boot-hook.sh`
"no intent, the unbootable disk holds another release: wiped (#261)",
re-run at e3c922d under sh and busybox sh, every initramfs test passing.
- Proof is the master's: 11.78 on C2NR0Q2 must come up from sda and keep
  300/300 in the durability stage (posted on #268)

### A mounted image clone is deleted under its containers (2026-10-03, #267, P0) — DONE

pvetest1 11.73 (engine 953cba6): a running container's executable changes
on its image clone and the process SIGSEGVs, 19–60 min into stormcos_qa's
turbomode load test. Not a CoW miscount — by reading all three components:
1. rustkube-node `pull_image`: sbregistry `POST /v1/clones` (state Claimed),
   attach **ublk**, mount at /run/stormpump/images/<vol>; never `bind`s it
2. sbregistry `reap_clones`: Claimed older than 900 s → `drop_clone` →
   export delete
3. engine `serve/reconcile.rs` step 3/4: the export's portal has 0 sessions
   (the node uses ublk) → withdrawn → `ephemeral` → `vm.delete_volume` while
   the ublk device is mounted. Reads of the unmapped extents give zeros, its
   slots are freed and reused by the new PVCs
Only the API's DELETE asked `what_is_serving`; ~20 internal paths call
`VolumeManager::delete_volume` directly.
- [x] `ServeHolds` shared by the volume manager and the ublk export manager:
      a ublk device (created or adopted) holds its volume; `delete_volume`
      refuses a held volume (`VolumeError::InUse`) — every path at once;
      DELETE answers 409 (18ddf69)
- [x] reconciler: an ephemeral volume whose delete is refused keeps its
      withdrawn row and is deleted on a later pass, once detached
- [x] tests: `integration_serve_in_use` (2), ublk_export and holds units;
      full nextest on dev: 867/867, `--features cluster` checks;
      `ci-ublk-qd-verify.sh` on a real kernel in QEMU: attach over ublk,
      ephemeral export withdrawn → volume kept with its data, DELETE 409,
      deleted ~3 s after detach. Docs (README attachments), CHANGELOG
- [x] filed rustkube-node#143 (bind the pulled clone), stormblock-registry#63
      (ask `in_use` before reaping a Claimed clone)
- Not on metal: needs a stormcos release with this engine; then
  stormcos_qa's turbomode run (stormcos_qa#26) again

### Health reports the flow-over still running (2026-10-03, #260) — DONE

stormcentral#301 waits for an installed node to settle before measuring;
one condition is "no extents left on a remote slab". The node's API is
closed except `/api/v1/health`, so: `flow_over_remaining` there, open and
read without waiting. Not a `try_read` of the GEM like `raid`: absent while
the map is busy would read as settled mid-flow. The flow-over loop keeps the
count in an atomic on `AppState` instead.
- [x] `AppState.flow_over_remaining` (-1 = no flow-over in this engine);
      `flow_system_half` counts from the extent lists it already reads;
      0 when done (and when there was nothing to move); stays > 0 when
      abandoned (still on the appliance) (2d72924)
- [x] health: `flow_over_remaining` when ≥ 0
- [x] tests: `the_flow_over_counts_down_what_is_left_on_the_appliance`,
      `integration_auth::health_reports_what_the_flow_over_has_left`; full
      nextest on dev at 7203a33: 863/863; `--features cluster` checks.
      Docs (README health, auth.md), CHANGELOG
- Not on metal: needs a stormcos release with this engine

### ublk serves one request at a time: QD1 on a spinning disk (2026-10-03, #264, P1) — DONE

#264 (server3, 7200 rpm disk): pod `sandbox` 265–621 ms, first pod 2.3 s,
vs 72–84 ms on SSD. The issue reads it as a clone + attach + journal sync per
pod. **Not so, by reading:** rustkube-node's `sandbox` step is stormpump
`SandboxAcquire` + Cilium CNI ADD + status (`pod_manager.rs` start_pod); a
busybox pod's root is the boot-mounted golden `/p/busybox`, so a pod start
makes no stormblock clone or attach at all (filed rustkube-node#139: split
the step). What is stormblock's: every volume the node runs on (cni-bin,
kubelet-data, pod-logs, fastetcd-data…) is a ublk device, and `queue_worker`
(`drive/ublk.rs`) handled each request with `block_on` before taking the
next: one queue, depth 128 advertised, **QD1 served**. Every FLUSH stalled
every read and write on that volume behind it.
- [x] queue worker: every request runs as a task; completions return through
      an eventfd armed in the queue's ring; the tag's buffer moves with it;
      a stand-down waits (5 s) for requests still running.
      `STORMBLOCK_UBLK_SERIAL=1` = the old worker, for measuring (b43d6c1)
- [x] `Slab::sync` group commit (`SyncGate`): one at a time per slab, one
      sync covers every caller who asked before it began. Also closes a
      #171-class hole: an unserialised sync could ack a FLUSH while another
      volume's sync was still writing the slots it had taken
- [x] `ci-ublk-qd-verify.sh` (QEMU, engine as root in the guest, dm-delay
      8 ms per I/O): 16 parallel readers 4460/4620 → 310/309 ms; reads while
      another process fsyncs 1469/4299 → 589/609 ms; concurrent-write round
      trip exact in both modes. Full nextest at b43d6c1+: 860/861 (#134)
- [x] docs (README Serving, durability.md rule 1), CHANGELOG
- Not on metal: needs a stormcos release with this engine; then server3's
  `storm.io/start-timing` again (with rustkube-node#139's split)

### Install = wipe at boot (2026-10-02, #261 reopened, P0) — DONE

Owner: "We should not be updating at boot time like that, it should be a
wipe. An update is done from a running system, not a half-ass install."
aed9f7e still kept the data half on "cannot say", and with an intent stated
it updated the system half over any old data half; stormcos#236 (11.68: kept
data slabs without `kubelet-data`) is that path.
- [x] /init survey: booting the claimed image with a local data slab, with or
      without intents: `slab holds` 1 (or probe's INSTALL_OVER) = wipe both
      slabs (force); 0/3 = recovery, kept; 2 = `ASSIMILATE=held`, every local
      drive left alone. Console `INSTALL:`/`RECOVERY:`/`LEFT ALONE:`
- [x] tests, docs (boot-hooks.md, README), CHANGELOG
- [x] sc-build at 7eaa520: every initramfs test under sh and busybox sh
      (boot-hook: 7 #261 cases rewritten/added), the /init parses. No Rust
- Not on metal: needs a stormcos release with this initramfs; then an
  upgrade boot (console `INSTALL:` + fresh data slab) and a reboot (`RECOVERY`)

### An upgrade without intents boots the old slab (2026-10-02, #261, P0) — superseded above

C2NR0Q2 (Dell R230) claimed 11.65 over its 11.56 disk and came up on 11.56
(forge 13.7, no intents). By reading: the probe found the 11.56 disk "missing
N mounted volume(s)" (11.65's cmdline mounts a volume 11.56 lacks), so it
booted the claimed image with no `INSTALL_OVER`; #258's no-intent rule then
kept every disk carrying a data slab (`NOT an install`). server3 11.64→11.65
had the same volume set, so its disk was bootable and `slab holds` said 1.
- [x] /init survey: before keeping a disk with a data slab, `slab holds
      <disk> <claimed>`: 1 = UPGRADE (INSTALL_OVER = that disk, forced), 0/3/2
      kept; printed on the console
- [x] boot-hook test cases; docs (boot-hooks.md, README), CHANGELOG
- [x] sc-build at aed9f7e: every initramfs test under sh and busybox sh
      (boot-hook +5 cases), the generated /init parses. No Rust changed
- Not on metal: needs a stormcos release with this initramfs; then
  stormcentral's `testhost install C2NR0Q2 <release>` (fresh-slab stage)

### Finishing a cut-short flow-over claims as the SMBIOS serial (2026-10-02, #259, P0) — DONE

server3, 11.64 (after #258): the probe claimed `boothost/server3` (the
firmware's name, #249) and `slab holds` said "unfinished" (exit 3), but
`boot-local`'s resume (`open_slabs_resuming`, #171) worked the name out again
with `machine_tag()` = SMBIOS serial `S11075924402016` (shared by the blades;
server1's old 11.50 image). That clone does not carry the slab the records
need: every mapping on it dropped, PID 1 SIGSEGV, panic. /init never exported
its `BOOTTAG` (`STORMBLOCK_BOOT_TAG` existed, unset).
- [x] /init: export `STORMBLOCK_BOOT_TAG` once the identity is resolved; on
      exit 3 hand the clone already claimed to the engine
      (`STORMBLOCK_RESUME_SOURCE=$CLAIMED`), no second claim (9d906c2)
- [x] engine `machine_tag`: env > EFI `StormBootTag` > SMBIOS serial > UUID
- [x] engine: a resume that leaves extents with no leg on any slab it has
      refuses to boot (named error) instead of dropping the mappings
      (`stranded_extents`; parity volumes left out: degraded is their normal)
- [x] tests: the #258 test first resumes from another image (refused);
      `the_resume_names_the_machine_as_the_firmware_did`; boot-hook cases.
      On dev at ad2bd77: nextest 861/861, every initramfs test under busybox
      sh, the generated /init parses. Docs (README, boot-hooks, durability)
- Not on metal: needs a stormcos release with this initramfs and engine;
  then stormcentral's durability stage (cut mid-flow-over, reboot)

### A power cut during the install's flow-over re-installs the node (2026-10-02, #258, P0) — DONE

server3, 11.63: 300 objects written, BMC power off, and the next boot laid a
fresh slab (every namespace re-created). Cause, by reading:
1. while the background flow-over runs, a system volume not yet moved has no
   leg on the local system slab, so `per_slab_metadata` records it only in the
   appliance clone's slab: the local disk's record leaves it out;
2. after the cut the probe sees `stormpump` but "missing N mounted
   volume(s)" and asks the appliance (the #171 resume is never reached); and
   `slab holds` called a cut-short disk "not held" (#239's Unfinished, exit 1)
   = an install;
3. with no boot intent (forge 13.7) #236's stopgap calls every claimed boot
   an install and passes `--local-disk-force`: the data slab is destroyed.
- [x] engine: `record_flow_over` (set by `quarantine_flow_sources`, persisted
      at once): the destination system slab records every volume with a leg
      on a source (81537b0)
- [x] `slab holds` Unfinished = exit 3; `/init` boots the disk then (the
      engine resumes, #171). Reverses #239's "unfinished = install again":
      its reason (writes left in place on the clone) is gone since b189b3e
- [x] /init: the no-intent stopgap forces only with `INSTALL_OVER` or no local
      data slab; otherwise `NOT an install`, data half kept (65a3b2d)
- [x] tests: `a_power_cut_before_the_flow_over_moves_anything_keeps_the_disk_bootable`
      (fails as server3 did with `RECORD_FLOW_OFF_258=1`: the disk names only
      the data half); boot-hook cases (all pass under sh and busybox sh); full
      nextest on dev at 65a3b2d: 860/860; generated /init parses (busybox)
- [x] docs (boot-hooks.md, durability.md rule 9, README), CHANGELOG
- Not on metal: needs a stormcos release with this initramfs and engine; then
  stormcentral's durability stage (BMC power off mid-flow-over) is the test

### RAID sets on a shelf, with hot spares (2026-10-02, #252, P2) — DONE

Owner (2026-10-02, on #252): **B** — drive-level RAID sets with hot spares,
**several sets per shelf**, each its own failure domain, spares per shelf or
global; volumes are allocated onto the sets; #168 wired first. Reverses
#142's "never a RAID across drives" for parity at shelf scale; per-volume
`mirror` stays for surviving the loss of a set or shelf. `docs/raid-sets.md`.

Found reading `src/raid/` (only RAID 1 legs had ever been used): RAID-6 never
computed Q; RAID-10 was RAID-1 over all members reporting pairs × capacity;
partial-stripe RMW took no lock; reads went to a rebuilding member (#175);
RAID-5/6 errors failed nothing; the journal was in memory; nothing
reassembled; `start_rebuild` was a stub; two RAID-6 arrays shared one failure
domain (serial = level); `migrate_to_local` compared states to "Active".
Only RAID-1's data layout is kept (data at 1 MiB) for stormstorage's legs.

- [x] parity: GF(2^8) tables, real Q, recovery of any two lost strips
- [x] layout (`layout.rs`): RAID-5/6 left-symmetric P/Q, RAID-10 near-2; stripe
      locks; RMW / reconstruct-write; errors fail the member unless that loses
      data; reads below a rebuilding member's watermark only
- [x] superblock v2 (`superblock.rs`: slot table, events, name, pool) and the
      on-disk write-intent bitmap (`bitmap.rs`); `assemble`, dirty chunks
      resynced
- [x] `replace` + background rebuild (watermark, checkpoint, resume);
      `SparePool` (`spares.rs`), own pool then global; supervisor (`start`)
- [x] scrub (P and Q, mirrors), progress, cancel
- [x] engine (`mgmt/raid_sets.rs`): assembly at startup before the slab scan,
      `POST /api/v1/arrays/assemble`, `/api/v1/shelves`, `/api/v1/spares`,
      members `{slot}/fail` and `/replace`, `scrub`, `rebuild` rate; 409s
      (#215); drive health `failed` fails the member; `set` rung; health
      `raid`; `stormblock_raid_*` gauges; delete wipes superblocks
- [x] tests: `src/raid/tests.rs` (in-memory faulty drives), parity, superblock,
      bitmap units; `tests/it/integration_raid_sets.rs` (14-drive shelf over
      HTTP, failure → spare → rebuild, restart reassembles); full nextest on dev
      at 63e486c: 859/859; RAID tests 5 runs in a row green; `--features
      cluster` and the `dist` musl build pass
- [x] `examples/raid_set_rate` on dev (11 × RAID-6 on O_DIRECT files, all on
      one virtual disk): seq write 57 MiB/s, 4K random write 684 IOPS, read
      310 MiB/s healthy / 98 MiB/s with two lost, rebuild 22 MiB/s per member
      (~240 MiB/s physical: the virtual disk's ceiling), rebuilt member verified
- [x] docs (raid-sets.md, multi-drive.md reversal, README), CHANGELOG; filed
      stormdrive#44 (bay labels, fault LED), stormstorage#43 (assemble after a
      restart), stormconsole#67 (shelves view)
- Not on metal: needs a shelf (DS2246) on a node; `[[arrays]]` config is still
  not acted on (#165)

### The initramfs steps the clock from NTP, bounded (2026-10-01, #251, P1) — DONE

X9 blades have no RTC battery (stormcos#213): after a power cut the kernel
boots at a 2000-era time, and stormcert/fastetcd start beside `timesync`
(stormpump has no ordering) and check certificates against it.
- `# --- BEGIN clock step` in `/init`, run after the network, before the
  engine starts (so before switch_root and everything after it): one
  `busybox ntpd -n -q` under `timeout` (`STORM_NTP_WAIT`, 3 s) to
  `/run/ntp-servers` (option 42), then again to fixed addresses
  (162.159.200.1, 216.239.35.0) — no DNS, so the old 50 s cannot recur.
  Skipped with no address or link-local only; `rd.stormblock.ntp=off`.
- success: `hwclock -w -u`, console `clock stepped by +N s from <server>`
- failure: clock before the image's build date (`/etc/stormblock/build-date`,
  epoch, `SOURCE_DATE_EPOCH` or the build's `date`) → set to it, loudly.
  Never blocks the boot.
- [x] block + `tests/initramfs-clock.sh` (stubbed ntpd/date/hwclock)
- [x] sc-build: every initramfs test under sh and busybox sh; an image
      generated on dev carries `build-date`, the block, and the ntpd/hwclock/
      timeout applets, and its /init parses. `ci-clock-verify.sh` (QEMU, PID 1,
      RTC at 2000): real ntpd stepped +844 Ms from 216.239.35.0, and
      `hwclock -r` read 2026 afterwards. With nothing reachable it floored at
      the build date in 6 s. Found by it: ash printed "Terminated" when
      timeout killed ntpd (silenced)
- [x] docs (README), CHANGELOG. Also fixed: a relative output path lost the
      main archive
- Not on metal: needs a stormcos release with this initramfs, then an X9
  blade with its power pulled

### The ConnectX-3 port in the initramfs: mlx4_en (2026-10-01, #250, P0) — DONE

server7/server8 (X9 blades, 11.58): stormbootx's mlx4 brings the ConnectX-3
up, the initramfs lists only the Intel. `mlx4_core` matches the PCI ID;
`mlx4_en` matches `auxiliary:mlx4_core.eth`, a device `mlx4_core` creates at
the end of a probe that takes seconds — after the modalias walk stopped (a
pass that loaded nothing new). Both modules are in `kernel/drivers/net`,
copied whole; ConnectX-3 needs no firmware file.
- after discovery (`# --- BEGIN protocol halves`): a table of core → network
  half (`mlx4_core:mlx4_en`; mlx5/qede/bnxt/ice/i40e carry their own netdev
  or match a PCI ID — checked with modinfo on 6.17), loaded when the core is
- then wait (bounded, `STORM_NETDEV_WAIT`, 15 s) until every network-class
  PCI function with a driver bound has a netdev, re-walking auxiliary-bus
  modaliases each second; name what never appeared
- uplink tie (equal speed, both carrier): unchanged and pinned by a test
  (sort -r: the later name); both ports are printed
- [x] blocks + `tests/initramfs-netdev.sh` (7) + a tie case in
      `tests/initramfs-nic-selection.sh` (4bea801..d38b07c); all initramfs
      tests pass under sh and busybox sh on dev
- [x] an initramfs built on dev for 6.17.1 carries mlx4_core.ko.xz and
      mlx4_en.ko.xz (modules.dep: en → core) and both blocks; /init parses
- [x] docs (README "The initramfs network"), CHANGELOG; stormcos#211 is the
      running system's side
- Not on metal: needs a stormcos release whose initramfs is built from
  stormblock ≥ 4bea801, then an X9 blade cabled on the Mellanox only

### The initramfs claims as the machine stormbootx claimed as (2026-10-01, #249, P0) — DONE

server8 (X9 MicroCloud blade): stormbootx claimed `boothost/server8` (DHCP +
reverse DNS), then `/init` claimed `boothost/<SMBIOS serial>` — the chassis
serial all eight blades share, server1's old trial synonym, 11.50 — and laid
the local disk from it with `--local-disk-force` (#236). Nothing hands the
firmware's name down, so the SMBIOS fallback always ran.

Mechanism (picked here, the stormbootx side filed there): **a volatile EFI
variable** set by stormbootx before it starts stormuefi — attributes
BOOTSERVICE_ACCESS|RUNTIME_ACCESS (0x6, not NV), vendor GUID
`ab361f54-0166-44a4-a088-1ac22e98ab76` (`STORMBOOT_GUID` in /init), `StormBootTag` = the name it claimed on (the
engine's host name when the claim reply gave one), `StormBootHostNqn` = the
NQN it attached as, ASCII, no NUL. Linux reads them at
`/sys/firmware/efi/efivars/<Name>-<guid>` (4 attribute bytes, then the
value). No change to the pallet's cmdline or to stormuefi.
- `/init`: firmware variable > `rd.stormblock.tag=` > SMBIOS serial > SMBIOS
  UUID; the source is kept (`BOOTTAG_FROM`). A cmdline tag that differs from
  the firmware's is reported and the firmware's used; an SMBIOS serial that
  differs is reported.
- A guessed identity (SMBIOS) never installs over a disk: the probe boots the
  local disk instead of an install, and the survey takes only a blank drive
  (no #236 force, no ticket force). `rd.stormblock.trust-smbios=1` restores
  the old behaviour for an image whose machines are named by serial.
- [x] identity block + guards in `scripts/build-stormblock-initramfs.sh` (d9c0f00)
- [x] `tests/initramfs-boot-hook.sh` cases (efivar, cmdline, differ, guess)
- [x] docs (boot-hooks.md, README), CHANGELOG (4c02258); stormbootx#76 filed
- [x] sc-build at d9c0f00: boot-hook test (+20 cases) under sh and busybox
      sh, the generated /init parses, console and NIC tests pass. No Rust
      changed, so no nextest run for this
- Not on metal: needs stormbootx#76 and a stormcos release with this
  initramfs; then server8's console must say "Machine name from the firmware"

### #239 reopened: service goldens still corrupt on 11.57 (2026-10-01, P0) — DONE (golden-stormblock-d3181a25ed62)

Owner, 11.57 (initramfs = stormblock@955a8f8, the fence included), fresh
install on server3: `iget: checksum invalid` on cadvisor, stormlb, vmimages,
stormvm, stormimds — inodes #155/#156/#160/#164 only, the same one or two
inode-table blocks in every volume. Seen on 11.50 too (inode #1410).
Found: stormcos builds every service golden **on forge** (13.7): a
`golden-<name>` volume exported over NVMe/TCP, mkfs'd and filled by the
kernel, sealed. So the corruption may be in the goldens themselves.
- [x] `examples/slab_audit.rs`: reads a published release by Range GET,
      extracts, `e2fsck -fn`, diffs clone vs golden (11.50: clean)
- [x] mechanism named and fixed (below)
- Ruled out by reading (2026-10-01), so far: the goldens leave the build
  checked (stormcentral `sc-build-out close` runs `e2fsck -fn` before the
  digest); the clone's UUID stamp writes the superblock block only
  (mkfs-ext4 v3 `flush_superblock`); ublk WRITE_ZEROES zeroes exactly its
  range; every ThinVolumeHandle read/write/CoW path takes the fence; 11.57's
  stormblock golden (296e38521d4c) is 955a8f8, fence included.
- Where the bad bytes are: a 32M `mkfs.ext4 -b 4096 -O ^has_journal -m 0`
  has its inode table at block 37, so #145–176 are blocks 46–47 (184–192 KiB),
  in extent 0 — the one slot of each slab clone that is private (the stamp's
  CoW, made on forge by 13.7's `compose/slab`) and so the one the background
  flow-over moves while it is mounted. Data-half volumes (seeded before
  export) are not reported.
- Unblocked (master, 2026-10-01): read-only Range GETs of published release
  images are allowed. `slab_audit` reads `http://…/image.img` by Range GET.
- **11.50's published image is clean** (ef71961, on dev): all 126 volumes
  pass `e2fsck -fn`; every clone differs from its golden in block 0 only
  (the stamp). cadvisor there: inode table at blocks 7-518, ITABLE_ZEROED,
  inodes 1-154 used, "Free inodes: 155-8192" — so the node's lookups of
  #155/#156/#160/#164 name inodes that table never had: a directory and an
  inode table from different states. The corruption is made on the node.
- [x] `cli::install_tests::a_real_release_installs_byte_for_byte` (ignored,
  `AUDIT_239_IMAGE`): the real image installed fresh, over a used disk,
  through a successor reopening claim + disk, 64 live writers on every
  clone's first extent during the flow-over — all byte-exact on dev. The
  engine's own install is not where the bytes go wrong.
- [x] 11.50 hubble-relay: 1409 inodes used, so #1410 is also the first free
  inode. Every case: a directory naming a runtime-created inode, read
  against an inode-table block (extent 0, the clone's own stamped slot) of
  an older state.
- [x] cause found by reading: owned extents on the appliance's per-boot clone
  were written in place; copy-on-writes went local. A boot cut short resumes
  from a fresh pristine clone (rule 9), so in-place writes vanished and CoWs
  stayed. Fix b189b3e: quarantine flow sources from the moment the boot knows
  (boot-local, adopt-ublk), relocate-on-write for owned extents there.
  Test `a_write_during_the_install_boot_survives_a_flow_over_cut_short`
- [x] full nextest on dev at 2ceec10: 848/848; without the fix
  (`RELOCATE_OFF_239=1`) the new test loses exactly the owned-extent write
- [x] owner's ask: `slab holds` = finished too (`Unfinished`, exit 1, 2ceec10)
- [x] golden-stormblock-d3181a25ed62 (stormcos#168). Filed: #244 (root
  fails to mount → install), #245 (`slab holds` "intact": goldens verified)
- Not on metal: needs a stormcos release with this initramfs and engine
- verify seeded/flowed volumes against their goldens: moved out, #245

### A fresh install's data volumes come out corrupt (2026-09-30, #239, P0) — fence DONE (golden-stormblock-296e38521d4c), reopened above

server3 on 11.56 (#236 fresh-slab path): cni-bin (256M data-role blank,
e2fsprogs `mkfs.ext4 -b 4096`, imported as `cni-bin.golden` + stamped clone
`cni-bin` in the image's data slab) reads its root directory block without
the dirent csum tail (`No space for directory leaf checksum`) — a block that
is not the golden's. Suspect: `seed_data_half` (the data half's flow-over on
install) or the successor's restore of what it moved.
- [x] reproduce on files. The data seed is clean: every volume's sha256 is
      the same after the seed, after a fresh open and from the disk alone
      (`take_local_disk`, c692344). **cni-bin is not in the data slab.**
      stormcos `deploy/image.toml` puts it under `[[slab.golden]]`, the
      system half, which `spawn_flow_over` moves in the background while it
      is mounted and Cilium writes to it
- [x] cause: I/O looked up its slot and used it with no lock held; a move
      copied the slot, rewrote the maps and freed (discarded) the source
      in between. A read got zeros, a write after the copy was lost, and a
      copy-on-write copied zeros into the clone. Reproduced deterministically
      (an I/O held inside the device during the move): without the fence the
      copy-on-write leaves 15 of 16 blocks zeros, the golden read 16 of 16
- [x] fix: `volume/fence.rs` slot fence (I/O shared, a move exclusive);
      flow-over/seed/drain wait for it before the locks; `move_slot` reads
      its copy back; the flow-over quarantines its sources (087fcbe..)
- [x] size ruled out: a 256M e2fsprogs 1.47.3 blank has 223.6 MiB free
      (12.8 MiB reserved), ~5x the ~40 MB of plugins; the error is not ENOSPC.
      The blank has `metadata_csum_seed`, so stamping is one superblock write
- [x] full nextest on dev at c7e6c2f: 847/847. Golden staged
      (golden-stormblock-296e38521d4c, stormcos#168). Not on metal yet: needs a
      stormcos release with this engine and initramfs, then a fresh install
- moved out: item 3 (re-seed a corrupt derived volume) is a question, #241;
  parity and StormFS paths not fenced, #240

### Boot messages on every console= (2026-09-30, #237, P1) — DONE

Owner: "can we get the stormcos boot messages to also go to vga?" The release
cmdline is `console=tty0 console=ttyS0,115200n8`; `/dev/console` is the last
one (ttyS0), so after the kernel's own lines VGA shows nothing.
- `/init` reads the `console=` list from /proc/cmdline, keeps the devices
  that exist and open, and when there are two or more sends its stdout and
  stderr (and the engine's, which it starts) through a fifo to a `tee` onto
  each. The fan-out ignores HUP/INT/QUIT/TERM/PIPE and holds the fifo's read
  end itself, restarting `tee` if it is killed, so a writer never sees a
  broken pipe (the engine's `println!` panics on one). One console or none:
  nothing changes. Serial stays exactly as it is.
- stdout goes back to `/dev/console` before `switch_root` (PID 1 hands init
  what it had) and before the emergency shell.
- The emergency shell (`rescue_shell`): `/bin/sh` on `/dev/console` as now,
  plus one `setsid` shell on every other console, so VGA gets a prompt too.

- [x] console block (`# --- BEGIN console fan-out`), `rescue_shell` for
      every `exec /bin/sh`, restore before switch_root (78eb14e)
- [x] `tests/initramfs-console.sh`; `ci-console-verify.sh` (QEMU, PID 1,
      two serial ports); README, CHANGELOG
- [x] on dev: both tests under sh and busybox sh, boot-hook and NIC tests,
      the generated /init parses. Found by the QEMU run and fixed:
      `/dev/console` is not always the last `console=` (8250: one console for
      every ttyS, the first wins) → read `/sys/class/tty/console/active`; a
      ttyS with no UART opens → skip sysfs `type` 0
- [ ] not verifiable here: server1 (VGA via BMC KVM), pvetest1 (serial) —
      needs a stormcos release with this initramfs

### Stopgap: an install boot always lays a fresh slab (2026-09-30, #236, P0) — DONE (golden-stormblock-436eb75ed6c9)

Owner: "can we just make installs always run, and then fix the intent?" Forge
is 13.7 (#235), so no boot intent is served, and today a netboot of a new
release either boots the old local disk (the local-slab probe finds it
bootable and never asks) or, when that fails, assimilates it keeping the old
data half (11.53 on 11.51's fastetcd, stormcentral#196). The decision is the
initramfs's (`/init`, built here), so this is stormblock's.

What counts as an install without an intent: stormbootx claims on every boot
(`auto`), so a netboot alone is not one — that would wipe the node on every
reboot. **An install is a boot whose assigned release the local disk does
not hold.** Same release = a reboot: boot the local disk, keep its data.
- `stormblock slab holds <local> <image>`: exit 0 when the local disk holds
  every sealed volume (golden) of the image, 1 when it does not, 2 when it
  cannot say. By volume id, the same test the flow-over's "already up to
  date" uses.
- `/init` probe: a bootable local disk with a boothost known → claim, then
  `slab holds`. Not held → boot the claimed image and install fresh; held,
  or no answer → boot the local disk as before.
- A boot that claimed and takes a local disk installs fresh
  (`--local-disk-force`, a new data slab), never keeping the old data half,
  unless the claim reply stated an intent (a v20 engine, #148): then the
  intent decides and this stopgap path is off by itself. `assimilate=off`
  still means no. The console says the old slab was discarded.
- Not covered: reinstalling the *same* release fresh (repoint to another
  release first, or use the intent once forge is on v20).

- [x] `slab holds` + tests (`image::local::release_held`, 984f203)
- [x] `/init`: claim function, probe comparison, fresh install without intent
- [x] `tests/initramfs-boot-hook.sh` cases; docs (boot-hooks.md, README), CHANGELOG
- [x] sc-build at 984f203: boot-hook test 13 new cases, all pass under sh and
      busybox sh; nextest 842/843 (the one is #134); `slab holds` exit 2 on
      blank/missing checked with the built binary
- [x] golden staged (golden-stormblock-436eb75ed6c9, stormcos#168); #236
      closed. Not verified on metal (needs a stormcos release with this
      initramfs, then a netboot of a new release on C2NR0Q2)

### Boot media at 512-byte LBAs (2026-09-29, #228, P0) — IN PROGRESS

server1 (AMI Aptio 4) and pve's OVMF cannot boot a release: every volume is
presented at 4096-byte LBAs (`ThinVolumeHandle::block_size` is a constant),
so firmware reads a 4K GPT and a 4K FAT. Owner: what firmware reads is
512; everything the kernel alone reads stays 4096 (owner's split: a boot
volume @512 = ESP + boot pallet, the slabs @4096).

Engine piece, needed by any shape of the split: **a per-volume LBA**.
- `lba` (512 | 4096, default 4096) on the volume: `ThinVolumeHandle`
  presents it to ublk, NVMe-oF (Identify Namespace LBADS) and iSCSI
  (READ CAPACITY), and to every reader of `block_size()`.
- Persisted: `VolumeRecord.lba`, metadata **V9**; the encoder writes **V8**
  when every volume is at 4096, so a slab or `volumes.dat` holding no 512
  volume stays readable by an older engine (a release's slabs never hold
  one — an older initramfs keeps reading them).
- Inherited by clones and snapshots; set by `POST /api/v1/volumes {lba}`
  and by `compose/disk`'s `lba` (the disk is presented at the LBA its GPT
  was written for); shown on every volume and in the attach reply.
- Left for the owner: whether the release must become two volumes (boot@512
  + slabs@4096) or one composed disk at 512 is enough — the slabs inside are
  read by the engine at byte offsets and the volumes it exports from them are
  4096 whatever the disk's LBA. Then stormcos's `compose-release.py` (disk
  `lba` and the ESP's sector size) follows.

- [x] handle + record (V9, V8 when all 4096) + clone inheritance (4823b35)
- [x] API: create, compose/disk, volume JSON (the `/v1` AttachInfo is the
      CSI contract and is left alone; Identify Namespace carries the size)
- [x] tests: unit (identify LBADS 9, 512 write in a 4K block, clone,
      restart), metadata V8/V9, HTTP compose at 512; full suite on dev at
      2155b9c: 841/841. `ci-boot512-verify.sh` on dev (8fdfe3a): the kernel
      over NVMe/TCP sees 512 (plain volume 4096), both partitions, mounts the
      512 ESP; OVMF boots the clone as a pve-style virtio disk → stormuefi →
      kernel with the pallet's cmdline. ALL PASS
- [x] docs (composed-disks.md, README), CHANGELOG
- [ ] owner: one release disk at 512, or the two-volume split? (asked on
      #228); stormcos compose-release.py follows either way (filed)
- [ ] not verified: server1 (Aptio 4) and a real pve VM — the owner's
      acceptance, after stormcos composes a release at 512

### NVMe/TCP: per-host subsystems, allowed hosts, DH-HMAC-CHAP (2026-09-28, #210, P0) — DONE

pve connected to forge's `:4420` with no arrangement and saw 71 namespaces:
every golden, every release, every machine's boot clone. Owner: a host sees
only the clones attached to it; goldens never show up. Found while reading:
`/v1` attach records (`nvme_nsids`) are persisted but never put back on the
target at restart, so a recorded NSID can later be handed to another volume
and one volume can sit at two NSIDs — the kernel's "duplicate IDs in
subsystem" (the NGUID is the volume id).

Design:
- **Target**: many subsystems on one listener. Each has namespaces (with a
  read-only flag: NSATTR write-protected, writes refused) and an access list:
  any host, or named host NQNs each with an optional DH-HMAC-CHAP key.
  Connect to an unknown subsystem is refused; a host not on the list gets
  Connect Invalid Host (SCT 1/SC 0x84, DNR). Discovery lists only what the
  asking host may connect to. One volume is never two NSIDs of one subsystem.
- **DH-HMAC-CHAP** (target, and our initiator): NULL DH group, SHA-256/384/512,
  DHHC-1 secrets, unidirectional; ATR in the Connect response on every queue;
  nothing but Auth Send/Receive until authenticated. Wire format read from
  Linux's `drivers/nvme/{host,target,common}/auth.c`. HMAC written over `sha2`
  (no new crate), pinned by RFC 4231 vectors.
- **Engine**: attach (`/v1`, `/api/v1/volumes/{id}/attach`, exports) takes
  `host_nqn`; the volume goes into `<nqn>:host:<id>` whose only allowed host
  is that NQN. `dhchap: true` (or `[nvmeof] require_dhchap`) mints the host a
  secret, returned in the reply, kept in `<data_dir>/nvme_hosts.json` (0600)
  with the subsystems and namespaces, restored at start.
- **The shared subsystem is closed**: `[nvmeof] allowed_hosts` (default none);
  `allow_any_host = true` reopens it and says so on every boot. An attach
  without `host_nqn` on a closed node is refused (400). Sealed goldens never go
  on the shared subsystem, and only read-only to a named host; `mode=ro` is a
  read-only namespace.
- **Boot claims**: stormbootx presents `nqn.2026-09.lo.storm:host-<name>`
  (`attach.host` from the reply), so the engine binds the clone to that NQN
  (and the host's aliases) without a firmware change: subsystem
  `<nqn>:host:<name>`, `[nvmeof] boothost_host_nqn` template.
- Out of scope, filed: `/serve/v1` per-volume subsystems (RouterOS); callers
  sending `host_nqn`/secrets (stormcentral, stormstorage, rustkube-node, csi,
  stormvm, stormbootx DH-HMAC-CHAP).

- [x] target: subsystems, access, Connect Invalid Host, discovery per host,
      read-only namespaces, no volume twice (`target/nvmeof/mod.rs`)
- [x] DH-HMAC-CHAP: `target/nvmeof/auth.rs`, target state machine, initiator
      side in `nvmeof_dev` (checked against Linux's host auth.c and tcp.c:
      Auth Send goes in-capsule on both queue types)
- [x] engine: `mgmt/nvme_hosts.rs` (store, restore, `nvme_hosts.json` 0600),
      `host_nqn`/`dhchap` on attach/exports/claims, closed shared subsystem,
      sealed rules, `nvme_nsids` restored at start, boothost claim bound to its
      host (never a secret for a boot host: firmware cannot be handed one)
- [x] tests: `tests/it/integration_nvme_hosts.rs` (9), `auth.rs` units (RFC
      4231 vectors); docs `docs/nvme-access.md`, README, auth.md; CHANGELOG
- [x] full suite on dev at 53cd2d3: 837/838, the one failure #134; release
      build ok. Follow-ups filed: #212 (/serve/v1 open), #213 (drive secret),
      stormstorage#27, stormcentral#143, stormblock-csi#34, stormvm#53,
      rustkube-node#93
- [x] the Linux kernel as initiator, without root: `ci-nvme-hosts-verify.sh`
      boots dev's 6.17 kernel in QEMU with nvme-cli against an engine on dev,
      13/13 (discover, refusals, clone rw, golden ro, DH-HMAC-CHAP). Rollout:
      forge needs `allow_any_host = true` until its callers send `host_nqn`,
      or they break; boot claims need nothing. Golden held (#194)

### Boot intent beside boothost/<tag> (2026-09-27, #148, stormbootx#11) — DONE (v20.0.0)

stormbootx v0.4.0 reads `GET /api/v1/synonyms/boothost/<tag>/intent` before
it claims (`install` | `local` | `auto`; any doubt = `auto`); a machine with
no stated tag reads under its MAC (12 hex). Owner: `install` is one-shot —
back to `local` once the flow-over completes; the claim reply tells the
initramfs (it is `--local-disk-force`, "this drive is spent").

Design: the intent lives on the `Host` record (`intent`, `install_claim`),
so a rename carries it. The name resolves through `host_of` (alias, MAC);
unknown → 404. GET is open (firmware has no token), PUT needs the admin
token when one is configured (install destroys the disk's identity). A
claim served under `install` records its boot clone as `install_claim` and
answers `intent`. `boot-claim` writes `/run/stormblock/install.json` when
told `install`; the initramfs then assimilates with force; `boot-local`
carries the ticket in the handover record; the successor, once the
flow-over and local boot succeed, reports `POST …/boothost/<tag>/installed
{volume}` (open; resets only when `volume` is the clone claimed under the
install). The intended golden's key in the reply (stormbootx#19) is left
out: that is the owner's call.
- [x] store: `BootIntent`, `Host.intent`/`install_claim`, set/note/done (0e3c47b)
- [x] routes GET/PUT intent, POST installed; open GET/POST in auth (0e3c47b)
- [x] claim: `intent` in reply, `install_claim` recorded; boothost view
- [x] node: boot-claim ticket, initramfs force, record, successor report
      (8a076f8; the report only after the flow-over moved everything and
      `run_local_boot` says the disk is bootable; retried for an hour)
- [x] tests, docs (auth.md, README, boot-hooks.md), CHANGELOG; on dev at
      the fix commit: lib 33/33 (store, auth, handover), integration_synonyms
      24/24, integration_auth 5/5, tests/initramfs-boot-hook.sh all ok
- [x] full suite at ef7112e on dev: 837/838 (the one is #134); `dist` musl
      build passes. Released in **v20.0.0** at the owner's request (#148,
      2026-09-28); rollout answers on #148 (rollback = VM snapshot only: V8
      metadata; forge needs `require_auth = false` + `allow_any_host = true`;
      input goldens need stormcentral#143)
- [x] re-verified on dev at eeab4dd (2026-09-29; route code unchanged since
      v20.0.0): 46/46 (synonym store, serve::api open/guarded, integration
      synonyms incl. the intent test, auth). #148 closed: the route shipped in
      v20.0.0 (golden `golden-stormblock-c92340fbd86f`, stormcos#168); forge
      still runs 13.7.0 until the owner upgrades it
- moved out: `installed` from the first boot off the local disk is #220
  (P1), which waits on #221 (install ticket in the clone?). Not verified: a
  real machine through stormbootx

### Universal boot: a default claim carries a MAC (2026-09-27, #200, P0) — DONE (v19.4.0)

Owner: one ISO for every machine; "I don't want copies, I want cows". Today a
claim of `boothost/default` itself is tag `default`: one `hostgolden/default`
and one `boothost-default` boot clone for *every* machine, each claim
releasing the clone another machine runs on. Built on #199 (hosts, aliases,
rename).

Design: `POST /api/v1/synonyms/boothost/default/claim` must carry the
machine's first NIC MAC (`?mac=` or body `{"mac": …}`; the only option a boot
claim reads); without one → 400. The MAC resolves to a host by alias (a named
machine boots as itself); else the provisional host `mac-<12 hex>` is created
with the MAC as its alias and claimed through the ordinary path (pinned to
the default release, its own sealed `hostgolden/mac-<hex>` CoW clone, fresh
boot clone per boot per #107). Naming = the #199 rename; the golden is kept.
`GET /api/v1/boothost?unnamed=1` lists hosts still under a `mac-` name.
- [x] store: `provisional_host_name(mac)`, `is_provisional`, record on claim
- [x] claim: default + MAC → host; refuse default without MAC; response
      `host.provisional`, `host.mac`
- [x] `GET /boothost?unnamed=1`
- [x] tests: two machines, one serial, one ISO → two goldens of one base;
      same MAC → same golden; rename → claim by name and by default+MAC gets
      the same golden; no MAC → 400; unnamed listing
- [x] docs (auth.md, README), CHANGELOG (fab5769, b39b9ac); targeted
      tests on dev at b39b9ac: lib 12/12, synonyms 23/23, auth 5/5
- [x] full suite at b39b9ac on dev: 865 passed, 5 failed = #120 ×2, #134,
      #173 and the qcow2 import deadline (same class, noted on #173); release
      build passed. Released v19.4.0 (6af6ad3); #200 and #199 closed;
      tell the owner the version (they build the golden, test server1-4).
      Owner confirmed (2026-09-27): the per-machine golden is what stays with
      the machine through renames; each boot is a fresh CoW clone of it

### boothost names are DNS names; serials and MACs are aliases (2026-09-27, #199) — DONE (v19.4.0)

Owner: a machine is known by its DNS name (`stormblock1` is the Dell, today
`boothost/C2NR0Q2`); stormbootx will claim `boothost/<DNS name>`. A boothost
record is keyed by name and carries aliases (SMBIOS serial, MACs); a claim by
any alias resolves to the same record, so agents claiming by serial keep
working. A rename keeps the boot history and the clones (#127). Two hosts may
never share an alias — refused, naming both; nothing becomes an alias
automatically (MicroCloud nodes share a chassis serial).

Design: host records live in the synonym store (`synonyms.json`, `hosts`,
`serde(default)`), so a rename and the synonyms it moves persist together.
- [x] store: `Host {name, aliases, former_names}` in `SynonymStore.hosts`;
      `host_of`, `set_aliases`, `rename_host`, conflicts name both (7d788c0)
- [x] claim/resolve/re-point/rollback resolve aliases; claim reports `host`;
      clone + host-golden collection by former names (7d788c0)
- [x] `/api/v1/boothost` router (`src/mgmt/api/boothost.rs`) (7d788c0)
- [x] tests: 5 store units in `src/volume/synonym.rs`, 3 HTTP tests at the end
      of `tests/integration_synonyms.rs` (2dbf0ad); docs + CHANGELOG (053bc5d)
- [x] built and run on dev at 9bb3463: `--lib synonym` 11/11,
      `integration_synonyms` 21/21, `integration_auth` 5/5. Choices made:
      rename keeps the old name as an alias by default; DELETE of a boothost
      synonym takes the exact name only; `boot-claim --tag` still defaults to
      the SMBIOS serial (resolves by alias)

### Docs checked against the code (2026-09-27) — DONE

README, `docs/` and CLAUDE.md checked against the code since 2026-09-18 by
three read-only surveys (config/CLI/env; routes/auth/ports/metrics/shipping;
design docs), then corrected by hand. Code the docs describe and that is
wrong became #196 (Dockerfiles) and #197 (dead env names, unset metric).

### Test containers: short, medium, long (2026-09-26, #139, P1) — DONE (v19.3.0)

Per stormcentral `docs/test-standard.md`: `test/Containerfile` (context =
repo root), `/test <suite>`, optional `test/build.sh`, JSON lines + summary,
exit 0/1/2, results under `/results`, no machine assumptions, Job in the
run's own namespace. Pattern: stormd's `test/` (runs its binary in the pod).

Design: the node's engine is closed (v17) and a Job has no token for it
(stormcos#89), so:
- **node**: `GET /api/v1/health` on `STORM_NODE:9090` (public) — up, version,
  auth; unreachable = skip. The authenticated node checks run only with
  `STORM_STORMBLOCK_TOKEN`, else skip (not pass).
- **the engine under test runs in the pod**: `/stormblock` of the same
  commit on sparse file slabs under `/results/work`, NVMe-oF on 127.0.0.1,
  driven through its API with its own minted token; data checked through the
  engine's userspace NVMe/TCP initiator. Unprivileged, no ublk, no devices.

- [x] `test/` crate `stormblock-test` (workspace member, depends on the lib
      for `http` and `nvmeof_dev`): harness (engine child, API, report).
      The in-pod engine uses two plain drive files whose data slabs are
      adopted on restart (`--raid` formats a new slab every start)
- [x] short (< 2 min): node health; engine up; create, clone (ext4 blank →
      claim), attach over NVMe/TCP, write/read, detach, delete; nothing left
- [x] medium (< 30 min): auth closed; restart and kill -9 keep flushed data;
      snapshot/group snapshot + restore; mirror:2 across two domains, drive
      failed → rebuild → data intact; sealed refuses rw attach; discard
      reclaims
- [x] long: waves (claim, attach, write, verify, delete) sized from the
      pod's CPU/memory and the pool, until `STORM_TIMEOUT`; per-wave latency,
      residue (volumes, allocated slots, engine RSS and fds); regression fails
- [x] `test/build.sh`, `test/Containerfile`, `test/stormblock-test.yaml`;
      all three suites run on dev in podman as uid 65532 (image 30 MB)
- [x] docs, changelog, close

Found by the suites and fixed: `adopt_slabs` lost flushed data across a
daemon restart (medium; 31bf732, #171), and concurrent NVMe/TCP attaches
shared a namespace, so siblings read each other's writes (long; a3c6a96).

### /v1 snapshots of engine volumes (2026-09-26, #130, stormvm#28) — DONE (v19.2.0)

A VM's disks are engine volumes made through `/api/v1` (clone of a golden,
cidata seed), and `/v1/snapshots` and `/v1/group-snapshots` looked only in
`v1.volumes` → 404. `/api/v1/volumes/snapshots` is no substitute: it restamps
the GPT GUID and takes one volume per call. Taking the issue's proposal:

- [x] a member that is not a `/v1` volume resolves to an engine volume (id or
      name) and is snapshotted through the same `create_snapshots_atomic`
      fence, sealed, no identity restamp; `source_volume_id` is what the
      caller passed
- [x] `ready` only when this node holds the data (`local_id` set); a group is
      ready only when every member is
- [x] restore: `POST /v1/volumes {source: {kind: snapshot}}` of such a
      snapshot is a clone of it (already the path once `local_id` is set)
- [x] tests (group of two engine volumes: one point in time, GUIDs kept,
      restore; a remote-master snapshot is not ready), docs, CHANGELOG, close

### P0: fsync'd writes lost on power cut (2026-09-26, #171) — DONE (v19.1.4)

fastetcd's redb reports "All roots are corrupted" after every hard power-off
of C2NR0Q2 (stormcos#105, 11.44 marked broken). Found by reading the write
path with a volatile drive cache in mind:

1. **A slot's table entry can become durable before its data.**
   `Slab::allocate_gen` persists the entry immediately; the copy-on-write copy
   (1 MiB) and the new data are written after, with no flush between. A power
   cut can keep the small entry and lose the copy: recovery maps the extent to
   a slot that never got its data, losing what the consumer fsync'd long
   before in the rest of that extent. A clone of an ext4 blank CoWs every
   metadata extent on first touch.
2. **An unreplicated first write does not zero-fill its slot** (the mirrored
   path does); the rest of the slot is a previous tenant's data. Discard on
   free does not zero on an SSD and is a no-op on an HDD.
3. **Restore maps a recorded slot without checking it** — freed and reused by
   another volume, it is mapped anyway.
4. **Refcounts**: a CoW's decrement of the old slot is persisted at once and
   can land before the new slot's entry.
5. **`UBLK_IO_OP_WRITE_ZEROES`** (advertised) discards whole slots only,
   ignores partial ranges and reports success on error.

Rule for the fix: nothing durable references a slot before its data is.

- [x] a crash-simulating `BlockDevice` (`drive/crashdev.rs`) and a
      randomized test (`tests/integration_power_cut.rs`): on the old code 182
      of 300 cuts lost acknowledged data
- [x] slots allocated in memory only (`allocate_deferred`), confirmed after
      their data is written, entries written by `Slab::sync` between two
      device flushes (`ThinVolumeHandle::flush`, `VolumeManager::persist`);
      a freed slot is not reused until its free is durable; CoW decrements
      wait for the sync
- [x] zero-fill on first write; write-zeroes that writes zeros (ublk EIO on
      failure); discard leaves shared extents
- [x] restore: drop a recorded slot that is free or taken elsewhere; raise
      share counts to the maps restored
- [x] test: stale record + several CoW generations of one extent
- [x] docs/durability.md, changelog
- [x] full suite on dev (849 passed; the 3 failures are #120 and #134 —
      `mgmt_luns_at_scale` took 46 s to 570 s on identical code); release
- [x] (31bf732) `adopt_slabs` reconciles records with slot tables; the
      daemon adopts every drive at once — a daemon restart lost flushed data
- [x] **handover order** (owner, 2026-09-26; v19.2.1 c585c0b): `adopt-ublk` read the slabs and
      restored *before* standing the incumbent down, so allocations the
      incumbent made in between were missing from the successor's map (and
      their slots looked free). Now: stand down, wait for the incumbent's
      process to exit (zombie = exited; SIGKILL after the grace), THEN
      restore; ublk recovery holds I/O in the gap. `handover::take_over` pins
      the order; a test allocates in the incumbent inside the window and checks
      the successor maps it. Release v19.1.5, tell the owner the commit.
- [x] **flow-over cut short** (owner's second finding, 2026-09-27): a cut
      mid flow-over bricked the node (the next boot claimed a new clone, the
      old clone's slab was not attached, 9087 mappings dropped, erofs root with
      holes). boot-local now claims a fresh clone (same sealed image = same
      slabs by id, same bytes), attaches the missing slab data-only, restores
      onto it, and hands the flow-over to the successor.
      `tests/integration_flowover_resume.rs`. Release v19.2.2.
- [x] on-metal power-cut check (300/300 × 5): passed 2026-09-27 on
      C2NR0Q2 with 11.48 (v19.2.1 engine and initramfs); #171 closed. The
      flow-over cut is #172, still to be checked on metal with an initramfs
      built from ≥ fd9fa51

### A boot claim releases every old clone of its tag (2026-09-26, #127) — DONE (v19.1.3)

Forge had 60 unsealed `boothost-C2NR0Q2` volumes. Owner: a claim for a tag
deletes that tag's old claims; at most the live one is kept until it is
unexported; tags do not accumulate. Cause: `claim_boothost` released *one*
predecessor, `find_volume("boothost-<tag>")` — whichever volume of that name
it hit first — so with several same-named clones, or with the one it hit
inside the double-claim grace (#97), the rest were never collected.

- [x] release **every** volume named `boothost-<tag>` but the new one, each
      through the existing guards (not claimed within grace, not named by a
      synonym, not sealed, a clone of something this tag has booted)
- [x] report what was released in the claim response (`released`)
- [x] tests: an accumulated pile is collected by one claim; clones inside
      the grace window survive and the rest go
- [x] docs (auth.md host goldens), changelog, close

### A presentation of purpose and functionality (2026-09-26, #132) — DONE

Owner: every component gets a short deck. `docs/presentation.md`, Marp
Markdown, 8–15 slides, drawn from the #131 README and docs; only what works,
planned work on its own slide; where it sits taken from stormcentral's
relationships graph (stormblock depends on nothing; stormcos, stormpump,
stormvm, sbregistry, stormdrive, stormstorage, stormblock-csi, stormuefi,
stormbootx and buildbox2 depend on it).

- [x] write the deck
- [x] render it with marp-cli on dev (sc-build) and check the slide count
- [x] link from README; changelog; close

Rendered on dev with marp-cli 4 (node 22): 11 slides, 127 KB of HTML.

### Documentation from the code (2026-09-26, #131) — DONE (v19.1.2)

Owner: every component rewrites its docs from the code as it is now
(stormbootx b1347d9 / stormuefi b15dcba are the pattern); a stormcos
consistency pass follows. PVCs: stormcos has a **built-in** driver (class
`stormblock`) — a claim is a CoW clone of a sealed pre-formatted blank of its
size class, attached over ublk by the kubelet; CSI is for third-party drivers
only, and the built-in path is not described as an exception to it.

- [x] survey from source: CLI (every subcommand/flag/env, defaults), config
      (every key, defaults), HTTP routes + auth + metrics, ports, how it ships
- [x] README.md rewritten from that survey
- [x] docs/: each file checked — design marked as design, stale corrected
      or removed
- [x] cross-references checked against the other components' code
- [x] CLAUDE.md status current; module docs where behaviour changed
- [x] what the docs promise and the code does not do → issues; close

Surveyed by read-only agents from the source, then written by hand. Found on
the way and filed: #162 (`boot-iscsi` formats every run), #163 (`--data-dir`
reaches only the volume manager), #164 (config-file CHAP ignored — an
unauthenticated target), #165 (config sections never acted on), #166 (`ui`
outside the token check), #167 (VFIO driver a stub), #168 (RAID extras not
wired), #169 (dead code, empty features), #170 (StormFS registration target),
rustkube#103. The removed iPXE runbook had lab credentials (IPMI `ADMIN/ADMIN`,
`root/changeme`) that remain in git history — flagged to the owner.

### /v1 attach names its transport (2026-09-26, #149) — DONE (v19.1.1)

stormstorage exports each leg with `/v1/volumes/{id}/attach {node: master}`;
since 2337c8a made ublk the default for a local attach, the engine answers
`ublk` for exactly those requests and the RAID head cannot use it. The
request says who asks, not where the I/O comes from.

- [x] one parser for the transport a caller wants (`ublk` | `nvme_tcp`,
      `nvme-tcp`/`nvmeof` accepted | absent = engine's choice), used by both
      `/v1` and `/api/v1` attach
- [x] `/v1` `AttachRequest.transport` (optional, additive): `nvme_tcp` skips
      the ublk offer and returns NVMe-oF coordinates, or 409 saying why there
      are none (no target, not backed here); `ublk` insists or 409
- [x] tests (HTTP, with an NVMe-oF target running), docs, CHANGELOG; file the
      contract addition on stormblock-csi; close

### Pin a volume to an array; dedicated array slabs (2026-09-26, #150) — DONE (v19.0.0)

stormstorage#2: a consumer volume carved on a RAID1-over-NVMe-TCP array must
*be* the mirror. Today `array_id` on create is only checked (every extent
goes anywhere of its role), the array's slab is general-purpose (anything on
the head can land on it), and `DELETE /arrays/{id}` refuses while any volume
exists on the node yet leaves the slab registered when it succeeds.

- [x] slab header `flags` bit 0 = DEDICATED (byte 101, always 0 until now; an
      older engine ignores it); `SlabFormat::dedicated`
- [x] registry: dedicated slabs are not `allocatable` — every general picker,
      the pool counts and placement's own pickers skip them; rebalance too
- [x] `PlacementPolicy.pinned: Option<SlabId>`: a pinned volume allocates only
      there (redundancy must be `none` — the array is the redundancy); clones
      inherit the pin; persisted as the record's existing `array_id`
      (restore, and adopt via the slab's own `arrays` records)
- [x] `POST /api/v1/arrays {"dedicated": true}` (default true): slab in the
      data role, with its own metadata region, registered as a metadata slab
      that carries only the volumes pinned to it
- [x] `array_id` on `POST /api/v1/volumes` pins; `placement.array_id` on
      `POST /v1/volumes`; `GET /api/v1/arrays/{id}` names its slab and volumes;
      delete refuses only for *its* volumes and takes the slab out
- [x] tests, docs, CHANGELOG, close

Decided per the issue's proposal: API-created arrays are dedicated by
default (`"dedicated": false` opts out); config/CLI arrays (`--raid`, the
node's own disks) keep the general pool. Not here: API-created arrays are
still not reassembled at restart by the engine itself — the slab is adopted
by whoever reassembles the members (stormstorage), which is what the
self-describing slab is for.

### XFS alongside ext4 (2026-09-25, #147) — DONE (v18.6.0)

Owner: XFS in formatting and import as well as ext4, via the new crates
`mkfs-xfs` (glennswest/mkfs.xfs.rs) and `fio-xfs` (glennswest/fio.xfs.rs).
Both first milestones closed; both at **v0.2.0**. What they give: a
`mkfs.xfs`-identical formatter (300 MB … 1 PiB, v5, no checker yet), and a
read-only walker/extractor with every v5 CRC checked (no writing yet). The
two crates have **separate** `BlockDevice` traits; one adapter implements both.

- [x] `src/fs/xfs.rs`: the seam — adapter (discard for `write_zeroes` on a
      thin volume), `format`, `read_layout` (primary superblock, CRC
      checked), `seal_blockers` (in-progress, needs-repair, bad CRC), `check`
      (open with fio-xfs and walk the whole tree), `stamp_uuid` the way
      `xfs_admin -U` does it (sb_uuid new, sb_meta_uuid keeps the old,
      `META_UUID` incompat, CRC — every AG's superblock, primary last),
      `stamp_label`
- [x] `FsKind::Xfs`: templates, blanks and claims format with `mkfs-xfs`;
      seal and resume dispatch on the kind; `seed` and ext `features` refused
      for XFS (fio-xfs cannot write yet)
- [x] clones of an XFS volume get a fresh UUID (the kernel refuses to mount
      two XFS filesystems with one UUID), read back
- [x] `probe`: `fs.kind: xfs` in volume metadata
- [x] import: find the filesystems inside a GPT image (XFS and ext4), open
      each, walk it, read `/etc/os-release`; report them on the import;
      a recognised filesystem that does not read fails the import unless
      `verify: false`
- [x] tests (format → seal → clone → stamp read back; a GPT image with an
      XFS partition imports and reports its OS); `ci-xfs-verify.sh` on dev:
      `xfs_repair -n` and `blkid` on a blank and a clone; docs; close

Verified by `ci-xfs-verify.sh` on dev (xfsprogs 6.15): engine-made blank and
claims pass `xfs_repair -n`; `blkid` UUIDs match the engine's; claims keep the
blank's `meta_uuid`. A Rocky 9 GenericCloud image imports in 19 s, root found
(partition 4, "Rocky Linux 9.8"), walked 37,473 entries, `xfs_repair -n` clean.
Not done here: a kernel mount (no root; mkfs-xfs's own `kernel-mount.sh` covers
its output). `stamp_uuid` belongs in mkfs-xfs (mkfs.xfs.rs#6); seeding XFS
waits for fio-xfs writes.

### Per-volume rebuild at scale (2026-09-25, #146) — DONE (v18.5.0)

Owner: redundancy is per volume, members on different drives; a failed
drive's volumes rebuild **in parallel across the pool, per volume**, most
endangered first, throttled against live I/O — hours, not days.
Already there: placement by failure domain at the volume's rung (v10,
#142), spread per extent over every slab, distrust on a health report.
Missing: anything that *runs* a rebuild. `resync` is one manual call per
volume holding the volume manager throughout.

Found in reading it, and has to be fixed first:
- **A mirror resync publishes its new legs at the end**, after the extent
  locks are released: a write between the copy and the publish reaches the
  healthy legs and not the new one, which then serves stale data.
- **Publishing is O(every extent on the node)** (`rewrite_legs`,
  `add_leg_beside` scan every map), and a parity resync does it per stripe.
- A resync frees the replaced slots before the map naming the new ones is
  durable (the owed-slot rule, b270bfd).

Plan:
- [x] targeted publish under the extent lock for an unshared extent
      (`ref_count == 1`); shared extents (never written in place) batched
      into one sweep, rechecked under the map lock
- [x] `ResyncOptions`: concurrency within a volume, a shared throttle, a
      cancel flag, a checkpoint that persists and then releases owed slots
- [x] `VolumeHealth.margin`: failures the least-protected extent can still
      take
- [x] `src/rebuild.rs`: one node-wide queue ordered by margin, N volumes at
      once, jobs with progress; a volume hit again while running reruns
- [x] health report → rebuild automatically; `failed`/`missing` drain after
      the rebuild (for what has no redundancy); manual resync and drain
      refuse while a rebuild holds the volume/drive
- [x] `GET/POST /api/v1/rebuilds`, `DELETE …/{id}`, `PUT …/settings`;
      `[rebuild] automatic`, `parallel`, `extents_in_flight`, `max_bytes_per_sec`
- [x] tests: stale-write race, parallel rebuild after a drive failure,
      margin order, throttle, partial; multidrive over HTTP; docs; close

Found by the multidrive test once the rebuild ran before the drain: **a
failed drive excluded its whole domain at the policy's rung**, so an `@shelf`
volume had nowhere to rebuild (and could not place new extents while
degraded). A failed slab now keeps out only its drive; a dead member holds no
domain. Measured (`examples/rebuild_rate`, dev): page cache 1.8 → 3.1 GB/s
from 1 to 8 volumes at once; O_DIRECT 126 → ~200 MiB/s, one virtual disk's
ceiling. Follow-ups: #159 (EC k+m), #160 (scrub — owner decision:
no checksums, so what repairs a mismatch).

### Allocation metadata at 40 PB a node (2026-09-25, #145) — DONE (v18.4.0, docs/metadata-scale.md)

Owner's scale: 160 × 256 TB drives per 4U node (~41 PB), 1 PB drives
coming. The issue counts the slab's `extent_index`; the resident cost is more
than that — `Slab.slots: Vec<Slot>` holds an entry for **every** slot, free
ones included, and the GEM holds a forward entry and a reverse entry per
allocated slot. And allocation finds a free slot with `first_one()`, a scan
from the start of the bitmap, every time.

- [x] measure: `examples/metadata_footprint.rs` — RSS per slot empty and
      allocated (slab), per extent (GEM), allocation time as a slab fills;
      extrapolate to a 256 TB drive and a 41 PB node
- [x] first-fit through a per-chunk free summary (`drive/freemap.rs`):
      allocation flat at ~27 µs/slot from empty to 90% full (was 25→168 µs)
- [x] `docs/metadata-scale.md`: the budget, where the bytes go, the design
      (extent size by class, what stays resident, paged index, free-extent
      trees, 64-bit indexes and the format change), and the work split
- [x] what needs the owner's word, called out; issues filed (#155–#158);
      close #145. Measured: ~41 B per free slot, ~326 B per full slot,
      ~313 GB/PB full. Decisions open: the budget (proposed ≤ 1 GiB/PiB),
      extent classes (A, #156), 64-bit bundled with the paged format (B, #158)

### Multi-drive: the design (2026-09-25, #142) — DONE (v18.3.1, docs/multi-drive.md)

Owner: "we need to figure out multi-drive soon"; and (2026-09-25) redundancy
is **per volume** — a volume's members on different drives, never a RAID
across drives — placed by failure domain (drive < shelf of 160 < rack) and
rebuilt per volume (#146). A design issue: write it down, then split it.

- [x] survey what the engine already does (slab choice per extent, domains,
      drive add/drain/health/resync/rebalance, capacity, StorageClass
      parameters on both drivers)
- [x] `docs/multi-drive.md`: pools, placement, failure domains, add /
      drain / fail a drive, overcommit, what the console shows — what exists,
      what is missing, and the proposals that need the owner's word
- [x] prove what exists on a multi-drive node in a test (file drives in
      shelves), so the document describes behaviour rather than intent
- [x] split into issues (here and in the other repos), link them, close #142

Found by the test and fixed: a drain moved legs apart only at `drive` (broke
`@shelf`), and no move checked the destination slab's role. Open for the
owner: drive affinity for `none` volumes (#153), the default claim policy
(#151), whether a new drive joins automatically (#154).

### A size-class blank that never formats (2026-09-25, #141) — DONE (v18.3.0)

The 1 TiB class blank for a 600Gi claim sat in `awaiting_format` on C2NR0Q2.
Not the size: a 1 TiB blank formats and seals in 5-7 s on dev (library: 14 s
on a file slab). What was wrong is that nothing finished a format once its
create stopped: the template is persisted `awaiting_format` before the
format, a failure rolls back, but a caller that gave up (the handler future
dropped) or an engine that stopped left it there for good.

- [x] the API's create runs on a task of its own — finishes or rolls back
      whoever is still waiting
- [x] `FsTemplate.formatting` (persisted) + `resume_formats` at startup:
      discard the raw volume to zeros, format, seal; a failure rolls back so
      the next claim mints afresh. Pre-flag stores: any `awaiting_format`
      template with no filesystem and nothing serving its raw volume.
- [x] found by `ci-template-resume.sh` (kill -9 mid-format, restart): an
      **empty volume came back as `System`** on a data-only node — both
      `restore` (data dir) and `adopt_slabs` defaulted an extent-less volume's
      role to System, so every write asked for a system slab. Now: the role of
      the slab whose metadata records it, else a half the node has. On a node
      with both halves the same bug put an unwritten data volume (a fresh PVC)
      in the half an install replaces.
- [ ] rustkube-node#70 (open, not implemented there yet): wait for `ready` when a found blank is still
      formatting; an Event on the claim.

### No file I/O for real storage (2026-09-25, #140) — DONE (v18.2.0)

The identity half of #140 landed with #136 (v17.1.0): slabs on the installed
disk name `drive=<serial>`, one disk is one domain. The owner's direction on
the issue goes further: "we will get rid of the file/block copies … I don't
want any file IO." Every drive — the installed disk included — is opened as a
**block device** (O_DIRECT, io_uring); `FileDevice` is for tests and
development and says so loudly when it ends up under real storage.

Found first: the block backend we had (`SasDevice`) is **queue depth one and
blocks the async runtime** — a `std::Mutex` held across `submit_and_wait` on
the executor thread for every request. Putting the node's root disk on it as
it stood would have stalled the engine.

- [x] `drive/direct.rs`: asynchronous O_DIRECT I/O on an fd — an io_uring on
      a thread of its own (eventfd-woken, many requests in flight, every
      buffer owned by its operation so a dropped future cannot free what the
      kernel is writing into), and `pread`/`pwrite` on the blocking pool where
      io_uring is not available (RouterOS, a container with it disabled).
- [x] `SasDevice` on it, with read-modify-write for requests that are not
      whole logical blocks (callers keep `FileDevice`'s permissive semantics),
      a read-only open, and an O_DIRECT open of a regular file for tests.
- [x] `drive::open_path` — fabric URI, block device (never `FileDevice`),
      else a file — and every place a slab, drive or disk is opened by path
      uses it: slab paths at boot, the flow-over disk, local boot, `slab`/
      `image lay-node`/`local-boot` CLIs, drives and slabs API, pool sources.
- [x] `FileDevice` on a block device warns; a slab on a regular file under a
      serving engine warns ("tests and development only").
- [x] tests (O_DIRECT on files: aligned, unaligned, concurrency, both
      engines), docs, CHANGELOG, close #140.

Not verified here, and cannot be without root: a real block device (BLKGETSIZE64,
BLKSSZGET, a partition's sysfs). The engine and the O_DIRECT contract were
exercised on O_DIRECT regular files on dev; the ioctl half first meets a disk
on the R230's next install.

### Volumes view: in use, with a consumer; images marked (2026-09-25, #138, #126) — DONE (v18.1.0)

Owner (comment on #138): the UI's point of view, **no storage change**.
Goldens stay volumes underneath; the console shows an Images view (goldens,
blanks, media) and a Volumes view of only what running containers/VMs use.
Here: what that view needs.

- [x] `kind` on every volume: `volume`, `golden`, `blank`, `media`,
      `snapshot`, `template` (a template's scratch volume) — the rule in one
      place instead of every tool's naming conventions (#126).
- [x] `in_use` + `attachments` from what the engine actually serves:
      exports, iSCSI LUNs, ublk devices (with the mount point), per-volume
      NVMe subsystems, the serve wiring table — and the boot devices an
      adopting engine serves, which were recorded nowhere.
- [x] `consumer`: the volume's owner (#115) when one is set; else, for a
      mounted ublk device, the mount (`{kind: "Mount", name: "/data/…"}`).
- [x] Filters: `?kind=` (one kind, `image` for all non-volumes, `all`),
      `?in_use=true|false`, `?unowned=true` (documented since #115, never
      implemented). No filter keeps today's listing (compatibility).
- [x] tests, docs, CHANGELOG, close #138 and #126.

### Retire standby clones: mint at claim time (2026-09-25, #137) — DONE (v18.0.0)

Owner: "why do we have unclaimed clones? Does not make sense." #55 keeps one
pre-minted clone per sealed template because minting was "seconds"; that
hides the cost and leaves volumes that are nobody's (rustkube-node#59: every
volume is a PV + PVC).

- [x] **Measure** a claim with the standby out of the way, per step
      (snapshot, identity, check, export) — `examples/claim_timing.rs`,
      numbers posted on the issue.
- [x] **Make it cheap:** identity is one superblock write on a
      `metadata_csum_seed` blank (no backups, no group descriptors); the
      per-clone fsck goes — a sealed blank was checked when it was sealed and
      cannot change.
- [x] **Remove** `standing` / `ensure_standing*` / `standing_*` /
      `/fstemplates/standby` / `/{id}/standby`; the boot-time top-up; delete
      the standing clones already on a node at startup.
- [x] Re-measure; tests, docs, CHANGELOG (breaking: endpoints removed).

Measured on dev (file-backed slab, virtual disk). **Before:** an inline mint
over HTTP was 345 / 355 / 768 ms median (64M / 1G / 10G), because each paid
two metadata persists, a template-store write and a background standby top-up
competing with it; 30 standby volumes had piled up. **After:** claim 173 /
186 / 246 ms and no standby volumes. In the library a mint is the snapshot
(<1.3 ms) plus the stamp's flush (30-55 ms), and the fsck it no longer runs
was 2-90 ms. What is left is fsync latency; the stamp's flush stays, because
without it a crash before the consumer's first flush could leave a clone
with its blank's UUID.

### Per-volume placement in the API (2026-09-25, #136, with #114) — DONE (v17.1.0)

Owner: attach stormvolume/drive/shelf and RAID-partner info to what
rustkube-node mirrors; "look at a drive, and know how much storage is left".

- [x] **Drive identity.** `DeviceId.wwn`; `FileDevice`/SAS read serial, model
      and WWN from sysfs for a block device (a partition resolves to its
      disk). `BlockDevice::drive_id()` — a `PartitionDevice` answers with its
      drive's. Slab domains and drive labels key on that, so **two slabs on
      one drive are one failure domain** (today `drive=file+<offset>` makes
      them two, and a mirror's legs can share a spindle). Slabs report
      `drive {path, serial, wwn, model}`.
- [x] **`placement` on a volume** — always on `GET /api/v1/volumes/{id}`,
      opt-in `?placement=true` on the list (O(extents)): per slab (role,
      tier, domain, array, drive, node, state ok/failed/quarantined/draining
      with drain progress, legs, bytes), per drive, leg totals from health,
      and each RAID array's members with their state.
- [x] **`generation`** on the volume listing, bumped whenever the node's
      volume metadata is persisted; `If-None-Match` / `?since=` answer 304,
      so a mirror asks "changed?" instead of re-reading everything.
- [x] tests, docs, CHANGELOG, close #136 and #114.

Verified against dev's live sysfs: 25 NVMe-oF namespaces identified, every
partition resolved to its disk. Namespaces of one controller share its serial
(`SB010A`), and WWN tells them apart, so the domain keys on the serial.

Not here: *progress* of a rebuild — a volume `resync` is one synchronous
call, and a drive-level RAID rebuild's progress is returned and never kept
(#69). State is reported (`degraded`, `failed`, member `rebuilding`), not a
percentage.

### CSI VolumeSnapshot = a golden (2026-09-25, #111) — DONE (v17.0.1)

rustkube asked whether `CreateSnapshot` maps onto goldens and CoW clones or
stays separate. **It maps, and already did:** stormblock-csi implements
`CreateSnapshot`/`DeleteSnapshot`/`ListSnapshots` (controller.rs:480-545) on
`/v1/snapshots`, which the engine serves as a CoW snapshot volume with lineage
(`VolumeManager::create_snapshot`, group: `create_snapshots_atomic` — one
fence); a restore (`/v1/volumes` `source: {kind: snapshot}`) is a CoW clone of
it. The one gap: the snapshot volume was left **unsealed**, so it was a
writable engine volume rather than a golden, and anything that attached it rw
could change what the VolumeSnapshot holds.

- [x] seal the snapshot volume at creation (single and group)
- [x] tests: sealed, refuses writes, restore is a clone of it, a restored
      volume survives the snapshot's deletion
- [x] docs (spec §snapshots), CHANGELOG, answer on the issue
- Separate, and tracked: #130 (engine volumes made through `/api/v1` cannot
  be snapshotted through `/v1`; `ready: true` with no local backing).

### Close the management API by default (#107) — DONE (v17.0.0; golden held, see below)

**Owner decision (2026-09-25, on the issue):** each host gets its own sealed
golden from the default or assigned release; only that host can claim it;
every boot is a fresh CoW clone of it with the previous one deleted; the claim
is the only unauthenticated verb and can do nothing else; everything else
requires the token. The mechanism (docs/auth.md, v14.0.0) is already there —
what changes is the default and the claim.

- [x] **Host goldens.** `boothost/<tag>` stays the *assignment* (what
      stormcentral PUTs and reads back). A claim resolves it — or, on a tag's
      first appearance, `boothost/default`, pinning `boothost/<tag>` to that
      target — and keeps `hostgolden/<tag>`: a sealed CoW clone of the
      assignment owned by the tag. Reused while its parent is the assignment;
      a repoint makes a new one; the old one is deleted once nothing
      references it (no synonym, no clone).
- [x] **Fresh every boot.** The boot clone `boothost-<tag>` is a clone of the
      host golden; the superseded one is released as today (grace for the
      firmware→initramfs double claim, #97) — its lineage check now also
      accepts the tag's previous host goldens.
- [x] **The claim is the one public write**, exactly `POST
      /api/v1/synonyms/boothost/<tag>/claim`, and in that namespace it takes
      no options (name, namespace, size, label, unsealed_ok ignored): it can
      only hand tag X a clone of X's golden.
- [x] **Closed by default:** `require_auth` unset means required — mint into
      the token file; with nowhere to write, an in-memory token (closed, and
      said loudly) rather than open. `require_auth = false` stays the explicit
      way to open a node.
- [x] Callers without a token: audit every component; file issues where they
      must present the node token (stormcentral first — owner named it).
- [x] tests, docs/auth.md + docs for host goldens, CHANGELOG, major bump
      (default behaviour change), close #107.

**Golden held.** The audit (2026-09-25) found most outside callers sending no
token; issues filed: stormcentral#30, stormcos#89 (and where a node keeps its
token — the engine's data dir is `/run/stormblock/engine` there), stormconsole#30,
stormdrive#14, stormvm#44, stormcos_qa#19, rustkube-node#66, stormblock-csi#20,
stormblock-registry#40, stormstorage#12, vmcloud-image-operator#7. Shipping
v17 before they present a token breaks them; the rollout order is the owner's
call. Inside this repo: cluster heartbeat/join/Raft now present the shared
token; the ci scripts present one; `ci-auth-verify.sh` passes on dev.

Later, per the decision, not in this cut: attaching the boot clone read-only
with a writable overlay; binding a claim to the host (TOFU host key / TPM /
mutual boot auth, stormcos#35) — until then the tag is the binding.

### Local boot on an installed disk (2026-09-24, #123) — DONE (v16.2.0)

A flowed-over disk carries only the two slabs, so every cold boot still needs
the network claim. The loader is **stormuefi** (already what the netbooted
image runs): it scans every block device for `kind = boot` pallets and starts
the best one. So the disk needs an **ESP with stormuefi** and the image's
**boot pallet(s)**, beside the slabs. The owner scoped it (comments on #123):
A/B is the running-upgrade path (#122), *not* this issue, but what is laid
here must be the same pallet ladder that upgrade writes, not a second
selection mechanism.

- [x] `LocalLayout.boot_bytes`: a boot area at the front (free GPT space the
      ESP and pallets are allocated into), system slab after it, data last.
      `update_system_slab` carves it out of the system partition when an older
      layout has none (the system half is reformatted there anyway), and the
      "already up to date" shortcut does not apply to a disk without one.
- [x] `image/fat.rs` reader, so the ESP can be **rebuilt** at the local disk's
      sector size — the image is served at 4096, a local drive is usually 512,
      and a FAT must declare its medium's sector size or firmware cannot read it.
- [x] `image/local_boot.rs`: copy the source's ESP (byte copy when sector
      sizes match, rebuild otherwise; typed BASIC until complete, then ESP, so
      a torn copy is "no ESP" and the node netboots) and every `kind = boot`
      pallet not already present by manifest digest (`copy_pallet`, verified).
      Local ladder: newest copy at priority 14, older renumbered below it,
      prune to 2 — **capped below 15** so a netbooted image's own boot pallet
      always outranks a local one (stormuefi scans every device).
- [x] handover `Record.local_boot`; the successor lays it after the flow-over
      finishes (the initramfs engine does not live long enough for ~0.5–1 GB);
      `stormblock image local-boot --disk --from` for doing it by hand.
- [x] tests; `ci-local-boot-verify.sh` on dev: 4Kn source image → 512 local
      disk, `fdisk`, `fsck.fat`, `pallet verify`, OVMF finds the ESP
- [x] docs (images.md / pallets.md), CHANGELOG, close #123

Also found and fixed on the way: the flow-over wrote its GPT at `FileDevice`'s
4096 on every drive (invisible to firmware on a 512-byte drive; now
`BLKSSZGET`, and an old table is re-expressed at the next install), and two FAT
writer bugs only `fsck.fat` could see (a FAT one sector short; every `..` at the
root). `ci-local-boot-verify.sh` passes on dev: OVMF with only the node disk
attached → stormuefi → B selected over A → kernel with B's cmdline.

Not here: marking a boot successful once healthy, tries accounting, staging B
on a running node (#122). Not yet seen on metal: the R230 picks this up with
the next release it installs.

### Commit Cargo.lock (2026-09-24, #128) — DONE

Goldens are built from a commit with `cargo build --release --locked`, so the
lockfile is part of the source. Until this change it was in `.gitignore`; the only
copy that existed was hidden state in dev's old `/root/stormblock` checkout.

- [x] `.gitignore`: stop ignoring `Cargo.lock` (fuzz's own stays ignored)
- [x] generate it on dev (`cargo generate-lockfile` via sc-build), commit it
      (a899377: cargo 1.95.0, 352 packages, one `mkfs-ext4`)
- [x] verify at 797a943: `cargo build --release --locked` (5m14s) and
      `cargo test --locked --no-fail-fast`, every test binary run:
      785 passed, 3 failed, 22 ignored. Two failures are #120
      (`integration_image`). The third, `mgmt_luns_at_scale`, is a 30 s
      wall-clock limit on a loaded shared box: it passed twice when run alone
      (45.8 s, then 29.1 s for the whole test). Filed as #134.
      Tests need `TMPDIR` inside the scratch tree: dev's `/tmp/stormblock-*`
      are root-owned from old root builds (183 EACCES otherwise; a host fix).
- [x] `cargo update` from now on is a deliberate commit of its own

### Node disk layout: data last, and growable (2026-09-23) — DONE (v16.1 pending)

Asked for by the owner: the data half goes at the **end** of the drive so it can
be expanded; the node is a test box, so reinstalling it is fine.

- [x] `lay_node_slabs`: system slab first at a fixed size (goldens only; every
      install replaces it), data slab last taking the rest of the drive.
- [x] Slab header bytes 120..124: `table_capacity` (slots the table has room
      for; 0 = legacy = `total_slots`). `SlabFormat::with_growth(bytes)`
      reserves table room; the data slab reserves 4× its size (0.024% of it).
- [x] `Slab::grow()`: extend `total_slots` into the reserved table, up to the
      device's length. No data moves.
- [x] `image::local::grow_data_half(disk)`: when the data partition is last and
      the drive has room after it, extend its GPT entry to the end, rewrite both
      table copies, grow the slab. Run by `boot-local` on a local node disk
      before it opens the slabs, and by `stormblock slab grow <disk>`.
- [x] Tests: layout order, header round trip (with and without capacity), grow
      in place keeps data, grow at boot after the backing file is extended.
- [x] Reinstall the R230 onto the new layout; verify persistence again.
      Verified 11.34: system 0–116.4 GiB, data 116.4–1863 GiB with 4x table
      room; a hard power cut kept a ConfigMap; booted local, no claim.

### Layered goldens — engine items (2026-08-19)

Master checklist lives in **stormblock-registry/CLAUDE.md**, "Layered goldens
— the plan". The engine owns two of its items:

- [x] **`FROM` for templates** — `TemplateSpec.parent` makes a template's raw
      volume a CoW clone of a parent's sealed snapshot instead of a blank, so
      a runtime several images share is stored once. New `awaiting_seed`
      state; a fresh filesystem UUID is stamped at creation, because two
      children must not both claim the parent's identity and under
      `metadata_csum` that UUID seeds every checksum in the filesystem.
- [x] **Volume groups — answered by a data pallet** (2026-08-28). The group is
      a pallet of `PalletKind::Data`, named `data1`, `data2` and so on beside
      `system1` and `kernel1`. A pallet is a GPT partition, so it is the hard
      allocation boundary this entry said was required, rather than a
      preference a tier-based policy can fall back out of. The system drive
      carries one; a drive that is not a system drive is mostly these. The
      property wanted was that the system disk can be replaced wholesale
      without touching state, and it follows: a new system pallet is published
      and activated beside a data pallet that was never the same partition.
      Sizing for two generations still applies to the *system* pallet, since a
      rebase transiently holds both.

- [ ] **Placing a claim among many data pallets.** Discovery already supports
      any arrangement — `PalletStore::scan` walks every drive and appends
      everything it finds, so several data pallets on one drive and across
      several drives need no configuration. **Selection is what does not fit.**
      `select()` is `max_by_key` over priority then version: a *ladder*, which
      is the right question for boot, kernel and system, where exactly one
      wins. Data pallets are a **pool**. Nothing selects a data pallet; a claim
      is *placed into* one, and the inputs are free capacity and failure
      domain, neither of which the ladder consults. This wants a new call
      beside `select` rather than a change to it — `select` is correct for what
      it answers, and firmware links the read-only half of that code.
      Blocked on the failure-domain work below for the second input; free
      capacity alone is enough to start.
- [ ] **Failure-domain topology.** `placement/topology.rs` models
      `StorageTier` (Hot/Warm/Cool/Cold) and `Locality` — *how fast and how
      far*. It has no notion of *what fails together*: no chassis, rack, row,
      floor, building or site. Those are orthogonal — two drives can both be
      Hot and local and share a power supply — and the second is what
      placement needs to keep a volume's only copies out of one blast radius.
      **Density makes this urgent rather than theoretical.** A MikroTik node
      carries ~16 drives; a 4U 160-bay NVMe server (Supermicro
      ASG-4116S-NU160R, FMS 2026) carries 160+. At that point a *node* is
      already a failure domain worth reasoning about internally, and a rack of
      them is 4,000+ drives. stormblock has to know its own drives first, then
      where those drives are.
      Ties directly to volume groups: a group is "a set of slabs", and with
      topology it becomes "a set of slabs constrained by failure domain" —
      which makes "replace the system disk" and "survive losing a rack" the
      same mechanism.
- [x] **Raw import** — `POST /mk/v1/volumes/{id}/raw`, sparse-aware. Landed
      in stormblockmk 2026-08-19 and proven end to end; **belongs down here**
      with the rest of layer 2 (see below).
- [ ] **Promote the serving layer out of stormblockmk** — see
      [docs/layering.md](docs/layering.md). Measured: 4,865 lines in
      stormblockmk, of which the RouterOS-specific part is 11 mentions in
      config defaults and startup composition. The wiring table, reconciler,
      readiness, reaper, tar/raw import, trim and live-session detection are
      deployment-agnostic — a stormos profile wants ~3,700 of those lines and
      can only fork them today. Includes fixing the export-durability gap the
      split exposes: the engine keeps its export table in memory only and the
      profile persists it, which is a correctness requirement living in the
      wrong layer. **Design constraint from the notes:** layer 2 serves
      *volumes*, and must not assume the thing attaching is a container —
      VMs and micro-VMs are the easier case, since a clone already *is* a
      block device.


### Phase 0: Build fixes (get it compiling) — DONE
- [x] Fix `openraft` version: 0.10 → 0.9
- [x] Add `anyhow` to dependencies
- [x] Make `io-uring` dependency Linux-only via `[target.'cfg(target_os = "linux")'.dependencies]`
- [x] Make `nix` dependency Linux-only
- [ ] Add `#[allow(unused)]` or `#[cfg]` gates so empty modules don't warn (not needed yet — no code to warn about)
- [x] Verify the full dependency set resolves and compiles (confirmed on macOS, Linux targets need cross-compiler)

### Phase 1: Drive layer (`src/drive/`) — DONE
- [x] Define `BlockDevice` trait (async read/write/flush/discard)
- [x] `dma.rs` — Page-aligned buffer allocator (DmaBuf with alloc/zeroed/pool)
- [x] `dma.rs` — Hugepage-backed slab allocator for VFIO
- [x] `nvme.rs` — Struct definitions (NvmeDevice, IoQueuePair, SQ/CQ entries, registers)
- [x] `nvme.rs` — VFIO init, BAR0 mapping, queue pairs
- [x] `sas.rs` — Open /dev/sdX with O_DIRECT, detect SSD/HDD, read serial/model from sysfs
- [x] `sas.rs` — io_uring read/write/flush/discard
- [x] `filedev.rs` — NEW: Portable tokio file I/O fallback (MikroTik, dev, testing)
- [x] `mod.rs` — Drive enumeration: auto-detect block device vs file, open appropriate backend
- [x] `main.rs` — Wired up drive init with `--device` CLI flag
- [x] Drive health monitoring (SMART via sysfs + REST endpoint)

### Phase 2: RAID engine (`src/raid/`) — DONE
- [x] RAID superblock format (on-disk metadata: member drives, layout, state)
- [x] RAID 1 (mirror) — read balancing, write duplication
- [x] RAID 5 — stripe layout, XOR parity compute
- [x] RAID 6 — dual parity (P + Q, GF(2^8) multiplication)
- [x] RAID 10 — striped mirrors
- [x] `parity.rs` — SIMD XOR: AVX2 (x86_64), NEON (aarch64), scalar fallback
- [x] `parity.rs` — GF multiply for RAID 6 Q syndrome (AVX2 shuffle, NEON vtbl)
- [x] `journal.rs` — Write-intent bitmap: mark dirty stripes before write, clear after
- [x] `journal.rs` — Journal recovery on startup (partial stripe detection)
- [x] `rebuild.rs` — Background rebuild: read surviving members, recompute parity/mirror
- [x] `rebuild.rs` — Rate limiting (don't starve foreground I/O)
- [x] Scrub/verify (background read + parity check)

### Phase 3: Volume manager (`src/volume/`) — DONE
- [x] On-disk metadata persistence (`metadata.rs` — binary envelope, atomic writes, CRC32C, restart recovery)
- [x] `extent.rs` — Free-space bitmap, extent allocation (first-fit or best-fit)
- [x] `extent.rs` — Extent deallocation, coalescing
- [x] `thin.rs` — Thin volume: virtual-to-physical extent mapping
- [x] `thin.rs` — On-demand allocation on first write (allocate-on-write)
- [x] `thin.rs` — Discard/TRIM handling (return extents to free pool)
- [x] `snapshot.rs` — COW snapshot creation (clone extent map, bump refcounts)
- [x] `snapshot.rs` — Snapshot deletion (decrement refcounts, free unreferenced extents)
- [x] `snapshot.rs` — Snapshot diff (for incremental backup)
- [x] Volume resize (grow/shrink)

### Phase 4: Target protocols (`src/target/`) — DONE
- [x] `reactor.rs` — Per-core single-threaded tokio runtimes, round-robin dispatch
- [x] `reactor.rs` — Core affinity via sched_setaffinity (Linux), no-op on macOS
- [x] `nvmeof/pdu.rs` — NVMe-oF/TCP PDU parsing (ICReq, ICResp, CapsuleCmd, CapsuleResp, C2HData, H2CData, R2T)
- [x] `nvmeof/discovery.rs` — NVMe-oF discovery subsystem (discovery log page)
- [x] `nvmeof/fabric.rs` — Fabric Connect, Property Get/Set, controller register emulation
- [x] `nvmeof/admin.rs` — Identify Controller/Namespace, Active NS List, Get Log Page
- [x] `nvmeof/io.rs` — NVMe I/O: Read, Write, Flush, Dataset Management (TRIM)
- [x] `nvmeof/mod.rs` — NVMe-oF target server (ICReq/ICResp handshake, command loop)
- [x] `nvmeof` — io_uring zero-copy send for C2H data
- [x] `iscsi/pdu.rs` — iSCSI PDU parsing (48-byte BHS, CRC32C digests, text params)
- [x] `iscsi/login.rs` — iSCSI login state machine (security + operational negotiation)
- [x] `iscsi/chap.rs` — CHAP MD5 authentication (constant-time verify)
- [x] `iscsi/scsi.rs` — SCSI command dispatch (INQUIRY, READ/WRITE 10/16, READ_CAPACITY, MODE_SENSE, UNMAP, REPORT_LUNS, VPD pages)
- [x] `iscsi/session.rs` — Session registry, TSIH allocation, CmdSN/StatSN tracking
- [x] `iscsi/mod.rs` — iSCSI target server (login phase, full-feature phase, Data-In chunking)
- [x] `main.rs` — CLI flags for target config, startup with Ctrl+C graceful shutdown
- [x] `iscsi` — Multi-connection sessions, R2T/Data-Out for large writes
- [x] MPIO/ALUA support for multipath

### Phase 5: Management plane (`src/mgmt/`) — DONE
- [x] `config.rs` — Parse `stormblock.toml` into typed config structs
- [x] `config.rs` — Config validation (drive paths exist, ports not conflicting, etc.)
- [x] `api/drives.rs` — REST routes: `GET /api/v1/drives` (enumerate)
- [x] `api/arrays.rs` — REST routes: `GET/POST/DELETE /api/v1/arrays` (RAID create/delete/status)
- [x] `api/volumes.rs` — REST routes: `GET/POST/DELETE /api/v1/volumes` (create/delete/snapshot)
- [x] `api/exports.rs` — REST routes: `GET/POST/DELETE /api/v1/exports` (NVMe-oF/iSCSI target mappings)
- [x] `metrics.rs` — Prometheus metrics endpoint (`/metrics`)
- [x] `mod.rs` — AppState, DriveInfo, ArrayInfo, ExportEntry, start_management_server()
- [x] `main.rs` — Config loading, CLI merge, AppState wiring, mgmt server spawn
- [x] TLS for management API (rustls)

### Phase 6: Cluster scaling (optional — single-node must work without any of this) — DONE
- [x] Node discovery: new node announces itself via REST to an existing node or seed list
- [x] Cluster membership store: track known nodes, health, capacity (local JSON or embedded DB)
- [x] `api/cluster.rs` — REST routes: `GET/POST/DELETE /api/v1/cluster/nodes` (list, join, remove)
- [x] Node health heartbeat (periodic ping between peers, mark unreachable)
- [x] Raft consensus via openraft (leader election, log replication) for metadata coordination
- [x] Synchronous replication (write to N replicas before ack)
- [x] Asynchronous replication (background catchup)
- [x] Volume migration/rebalance: move volumes between nodes when capacity added
- [x] Online node addition: join a running cluster, receive replicated volumes without downtime
- [x] TLS for cluster RPCs (Raft, heartbeat, join) via rustls — shares management API cert/key

### Phase 7: Integration & hardening — DONE
- [x] End-to-end test: FileDevice → RAID 1 → ThinVolume → iSCSI/NVMe-oF target → TCP initiator → read/write/verify
- [x] Crash recovery testing (journal persist/recovery, superblock validation, extent allocator consistency)
- [x] RAID degraded mode tests (RAID 1 + RAID 5 with failed members)
- [x] Management REST API tests (drives, arrays, volumes, exports, metrics endpoints)
- [x] Volume lifecycle tests (create, snapshot COW, delete, multi-extent writes)
- [x] Criterion micro-benchmarks (parity throughput, extent allocation, PDU parsing)
- [x] fio macro-benchmark scripts (iSCSI + NVMe-oF, 4K random + sequential)
- [x] Container images (Dockerfile x86_64 + aarch64, deployed via StormBase)
- [x] StormFS registration (announce volumes to StormFS metadata cluster)

### Container Extent Store — Organic Data Placement

Replaces rigid DiskPool/VDrive/ExtentAllocator with organic, cellular storage. Each device is a Container (flat array of 1 MB slots). Volumes spread across any device on any tier. GEM is the single source of truth for extent placement.

**Phase 1: Foundation (additive, non-breaking) — DONE**
- [x] `src/drive/container.rs` — Container extent store with on-disk format, slot table, free bitmap, CRC32C (~550 lines, 11 tests)
- [x] `src/drive/container_registry.rs` — Tier-indexed container lookup with best-fit allocation (~150 lines, 3 tests)
- [x] `src/volume/gem.rs` — Global Extent Map with forward+reverse index, COW snapshot cloning, rebuild-from-containers (~300 lines, 10 tests)
- [x] Module declarations in `src/drive/mod.rs` and `src/volume/mod.rs`

**Phase 2: Volume layer rewrite — DONE**
- [x] Rewrite `src/volume/thin.rs` — ThinVolume backed by GEM + SlabRegistry instead of array_id + ExtentAllocator
- [x] Add VolumePurpose (Partition, StormFS, ObjectStore, KeyValue, Boot) and PlacementPolicy
- [x] Rewrite `src/volume/snapshot.rs` — COW via GEM clone + slab inc_ref
- [x] Update `src/volume/mod.rs` — VolumeManager uses GEM + SlabRegistry
- [x] Update `src/volume/metadata.rs` — V2 format with slab refs
- [x] Update external references: boot.rs, mgmt/api/volumes.rs, mgmt/mod.rs, main.rs, placement/mod.rs, tests/

**Phase 3: Placement integration — DONE**
- [x] `src/placement/mod.rs` — PlacementError, migrate_extent(), evacuate_slab(), rebalance() (EvenDistribution + TierAffinity)
- [x] `src/volume/gem.rs` — slab_extents() helper for reverse-index slab queries
- [x] `src/migrate.rs` — Slab-based extent migration via migrate_to_slab() (alongside existing RAID-level migrate_to_local())
- [x] 6 new tests: migrate_extent, evacuate_slab, rebalance_even, rebalance_tier_affinity, placement_error_display, migrate_to_slab

**Phase 4: API + cleanup — DONE**
- [x] Deleted `pool.rs`, `vdrive.rs`, `container.rs`, `container_registry.rs`
- [x] Created `src/mgmt/api/slabs.rs` — Slab REST API (list, get, format, delete, list slots)
- [x] Updated AppState: `slab_registry` + `gem` (Arc<Mutex>) instead of `pools` (RwLock<HashMap>)
- [x] Replaced CLI `pool` subcommand with `slab` (format, list, info)
- [x] Removed `DriveType::VDrive`, `PoolConfig`, `VDriveConfig`
- [x] Simplified `migrate_to_local()` — uses RAID 1 directly, no DiskPool/VDrive
- [x] All tests pass (229), clean clippy

### External iSCSI Test Infrastructure — DONE
- [x] `tests/common/iscsi_initiator.rs` — Pure Rust iSCSI initiator (two-phase login, SCSI read/write/inquiry/capacity/logout)
- [x] `tests/external_iscsi.rs` — 3 integration tests against real LIO Target (discovery, write/read/verify, multi-block I/O)
- [x] `Containerfile.iscsi-test` — Pre-built test container for fast iteration via mkube job runner
- [x] `run-iscsi-test.sh` — Unified runner (pre-built binary or cargo build fallback)
- [x] `test-iscsi.sh` — Build script for mkube job submission
- [x] Verified against LIO Target at 192.168.10.1:3260 (MikroTik, 10 GB, 512-byte blocks)

### Shared Ring IPC — DONE
- [x] `src/drive/uring_channel.rs` — Ring buffer protocol, SQE/CQE types, shared memory layout
- [x] `src/drive/uring_server.rs` — Unix socket server, per-client memfd+eventfd, I/O dispatch

### Boot-from-iSCSI with Live Migration — DONE
- [x] `src/drive/iscsi_dev.rs` — Production iSCSI initiator BlockDevice (login, READ/WRITE(10), READ CAPACITY, UNMAP, NOP-Out)
- [x] `DriveType::Iscsi` variant added to drive layer
- [x] `src/boot_iscsi.rs` — Boot disk orchestrator (BootDiskLayout, IscsiBootManager, multi-volume provisioning)
- [x] CLI `boot-iscsi` subcommand — provision partitioned boot disk on remote iSCSI target
- [x] CLI `migrate-boot` subcommand — migrate boot volumes from iSCSI slab to local disk
- [x] 11 integration tests (layout parsing, provisioning on file slab, slab migration with data verification)
- [x] `boot-iscsi-test.sh` — CI script for mkube job runner

### /v1 CSI Contract API — DONE (issues #3, #8, #9, #10; API layer of #5/#6/#7)
- [x] `src/mgmt/api/v1.rs` — full /v1 surface per stormblock-csi docs/stormblock-api.md (MockEngine is the executable spec): volumes (name-idempotent create, COW clone via `source`, expand, attach/detach with mode gating), snapshots + group snapshots, placement/prestage, fence/promote (epoch CAS), bounded dual-attach, node capacity/topology, `{code,message,current_epoch?}` error envelope (404/409/412/507), optional bearer auth (`management.api_token`)
- [x] `VolumeManager::create_volume_any` (array-free create) + `create_snapshots_atomic` (GEM+registry locks held across all members = single consistency fence for VolumeGroupSnapshot)
- [x] Empty-volume snapshot/delete fix in `src/volume/snapshot.rs` (never-written volumes have no GEM map)
- [x] Config: `management.api_token`, `management.node_name`, `management.topology`; /v1 state persisted to `<data_dir>/v1_state.json`
- [x] 11 HTTP-level integration tests (`tests/integration_v1_api.rs`) ported from the MockEngine spec + engine-backed COW divergence
- [ ] Engine data path for #5/#6/#7 (cross-node replication, epoch-carrying writes, resync) — control-plane state only for now

### boot-local + storage-role systemd unit — DONE (issues #11, #12)
- [x] CLI `boot-local` — attach existing slab(s) non-destructively (`open_backing_device`, no reformat), restore volumes.dat, resolve boot volume by UUID/name/boot.toml, export as /dev/ublkb0 (+ optional image-store as ublkb1), `--local-disk` zeroboot flow-over (per-extent lock cycling so root I/O keeps flowing)
- [x] initramfs `/init` local-slab path: `rd.stormblock.slab=` / baked boot.toml, erofs root, no network needed
- [x] `systemd/stormblock-target.service` — storage-role target server (config from /etc/stormblock/stormblock.toml); SIGTERM now shuts down gracefully like Ctrl+C
- [x] 3 CLI integration tests (`tests/integration_boot_local.rs`)

### LinuxBoot-style Fedora on iSCSI — DONE
- [x] `--ublk` flag on `boot-iscsi` CLI — UblkServer per partition (Linux 6.0+)
- [x] `scripts/build-stormblock-initramfs.sh` — minimal initramfs (busybox + stormblock + ublk_drv + /init)
- [x] `install-fedora-iscsi.sh` — 8-phase mkube CI job (provision, format, install Fedora, configure, verify)
- [x] `systemd/stormblock-ublk.service` — post-switch_root safety net

### Registry-scale export path — DONE (issues #22, #24, #25, #26)

Driven by the stormblock-registry / stormblockmk design: a CoW clone per
container instance means thousands of volumes, each needing an export, each
reclaiming space when dropped.

**#22 — thin/CoW volumes exportable as iSCSI LUNs**
- [x] `LunBacking::Volume { volume_id }` — resolve via `VolumeManager::get_volume`
- [x] Persist LUN↔backing mappings to `<data_dir>/luns.json`, restore on startup

**#24 — scale to 1000s of LUNs**
- [x] `AppState::lun_entries`: `Vec<LunEntry>` → `HashMap<u64, LunEntry>` (O(1) lookup)
- [x] Drop the per-SCSI-command `list_luns()` Vec allocation (only REPORT LUNS needs it)
- [x] REPORT LUNS: full LUN LIST LENGTH when truncated, SELECT REPORT handling, >255 LUNs
- [x] `/api/v1/exports` reports the assigned LUN/NSID and goes active; auto-assign on create
- [x] Scale test at 1000 LUNs (attach + dense sorted numbering) and REPORT LUNS at 2000

**#25 — UNMAP/discard → GEM/slab reclaim**
- [x] VPD 0xB2 Logical Block Provisioning (LBPU/LBPWS/LBPRZ, thin) — without it Linux never issues discards (root cause of monotonic growth)
- [x] Data-out collection for UNMAP/WRITE SAME — second root cause: UNMAP's parameter list was never read
- [x] VPD 0xB0: optimal unmap granularity + alignment from `BlockDevice::discard_granularity()`
- [x] WRITE SAME(16)/(10) with UNMAP bit
- [x] Reclaim reporting: slab allocated/free gauges sampled on `/metrics`

**#26 — NVMe-oF dynamic namespaces + advertised address**
- [x] `add_namespace_dynamic(&self)` / `remove_namespace(&self)` — interior mutability like iSCSI
- [x] `management.advertised_addr` config; AttachInfo + discovery log page stop reporting 127.0.0.1

Not covered (would need a live initiator on rose1 to verify): NVMe-oF
namespace-scale benchmarks, and steady-state memory profiling at 1000 LUNs.

### Preformatted filesystem templates — DONE (issue #38)

*mkfs once, clone forever.* Moved into core from the mk profile and
stormblock-registry, and made generic: the consumer list is RouterOS
containers, StormOS, Proxmox VMs, microVMs and x86 hosts, only one of which
is RouterOS. A profile should carry platform *choices*, not
platform-independent capabilities.

- [x] `src/fs/ext4.rs` — the seam onto
      [`mkfs-ext4`](https://github.com/glennswest/mkfs.ext4.rs), a from-scratch
      async `mke2fs`/`e2fsck` in pure Rust: `VolumeDevice` (thin volumes format
      in place, zeroing is a discard), `format`, `read_layout`, `check` /
      `repair`, `stamp_uuid` / `stamp_label`. Replaced the hand-rolled writer
      that shipped first.
- [x] `src/fs/template.rs` — create → format → seal → clone lifecycle over
      the `VolumeManager`, persisted to `<data_dir>/fstemplates.json`
- [x] Seal guard: every flag a consumer acts on (`VALID_FS`, `ERROR_FS`,
      `RECOVER`, `ORPHAN_FS`) **plus a real fsck** — a superblock that says it
      is clean is a claim, not evidence (stormblock-registry#10)
- [x] Every clone fsck'd before hand-off, and discarded if it does not check
      out; `POST /api/v1/volumes/{id}/fsck` (`?repair=true`) for any volume,
      since RouterOS has neither an fsck nor a clean unmount
- [x] Clone-time UUID stamping (stormblockmk#12) — the piece that can only
      live here, since every consumer clones *through* the engine
- [x] Per-template features in `mke2fs` vocabulary: kind (`ext2`/`ext3`/`ext4`),
      `journal` tri-state, and an `-O` list. The default is what
      `mke2fs -t ext4` writes — journal, `flex_bg`, `64bit`, `metadata_csum`,
      `metadata_csum_seed` — which is also what RouterOS's own `format-drive`
      produces (#39)
- [x] Nothing holds a lock across a format, a check or a stamp: templates
      build and clones mint concurrently
- [x] `/api/v1/fstemplates` (create/list/get/seal/clone/delete) and
      `from_template` on `POST /api/v1/volumes`
- [x] `ci-fstemplate-verify.sh` — e2fsck + real mount through an iSCSI
      initiator, on a real kernel. **Passing** on dev.g8.lo (Fedora 6.17.1):
      four clones attached at once, `blkid`, `e2fsck -fn`, mount rw, write,
      unmount, check again, no kernel complaint
- [x] The volume reports its logical sector size, so blocks are never smaller
      than the sectors underneath them (#40) — the failure that passed fsck
      and refused to mount

Deliberately **not** here: writing image *content* into a filesystem (tar,
whiteouts, hashing, image config) stays with the consumer that owns the
content — stormblock-registry keeps its full ext4 writer for that.

Seeding *content* into a template before sealing (skeleton rootfs, kernel
cmdline, `boot.toml`) is done — `src/fs/files.rs` writes files into a volume
the engine already holds through
[`fio-ext4`](https://github.com/glennswest/fio.ext4.rs), with no mount, no loop
device and no attach. fio.ext4.rs#1 is resolved: the crate declares its own
`mkfs-ext4` by git rather than by sibling path, so it can be taken as a git
dependency. Both pins track **v1.2.0** and must move together, so cargo
resolves one copy of `mkfs-ext4` and the two crates agree on the `BlockDevice`
trait.

Not done: `boot_iscsi.rs` cloning its ESP and root from templates rather than
constructing them each time.

**Confirmed on RouterOS (2026-08-13, #39 closed).** A clone of a v8.2.0
template attached over NVMe-TCP takes writes: `/file add` succeeds, and the
disk table corroborates it rather than the return code alone — free space
234 438 656 → 234 434 560 (one 4 KiB block) and free inodes 65 524 → 65 523.
The geometry shows the new profile: 65 536 inodes against the old 16 384, and
~32 MB less free space on the same 256 MiB volume, which is the journal. The
clone carried a fresh UUID with its checksums still valid, so
`metadata_csum_seed` held the stamp to one superblock write rather than the
structural rewrite that was the risk. Verified against stormblockmk v0.7.0
with six templates, 64m–10240m, each built in 0.06–0.81 s.

---

## Session 2026-08-18 — ext4 crate releases (v9.2.0 → v9.2.1)

`mkfs-ext4` v1.3.0 and `fio-ext4` v1.3.0, then `fio-ext4` v1.3.1. Two things
worth not re-deriving:

- **Both pins move together.** `fio-ext4` pins `mkfs-ext4` by tag itself, and
  two different tags are two cargo source ids — cargo then resolves two copies
  and the `BlockDevice` trait from one does not satisfy the other. Release
  order is `mkfs.ext4.rs` → `fio.ext4.rs` → here. Check what tag the `fio-ext4`
  tag you are taking depends on: v1.3.1 pins `mkfs-ext4` v1.3.0, which is why
  only one pin moved for this release.
- **`fio-ext4` v1.3.1 is a correctness fix that our own checking could not
  see.** An extent leaf's checksum went at the end of the block rather than at
  `EXT4_EXTENT_TAIL_OFFSET`; the offsets coincide at 1 KiB and 4 KiB and differ
  at 2 KiB, 8 KiB and 32 KiB. A template built on 2 KiB blocks passed our
  `fsck` and its content digest and was still refused by the kernel with EIO.
  The lesson for template work: our reader agreeing with our writer proves
  nothing — `e2fsck` 1.47.3 and a real mount on `dev.g8.lo` are the check that
  counts.

---

## Session 2026-08-18 — issue sweep (v8.2.1 → v9.2.0)

Nine issues closed. What each one turned out to be, so the next session does
not re-derive it:

- **#46** (32 MiB template clone fails verify) — not a size-specific defect.
  The same copy-on-write short-copy fixed in v8.2.1 (`8dc3134`), seen from the
  clone side: at that geometry the root directory's data block lands in the
  second half of the first 4 MiB slot, and a copy that stopped at 2 MiB left
  the inode table intact with the directory blocks reading as zeros. Regression
  test runs at 4 MiB and 8 MiB slots because every earlier test used the 1 MiB
  default, under tokio's cap.
- **#47** (template volumes leaked) — three leaks: the scratch volume outlived
  a successful create, a create that failed at seal left both halves, and
  `DELETE` kept the volumes by default. **v8.3.0 changed the DELETE default**;
  `?purge=false` restores the old behaviour.
- **#48** (failed discard reported as discarded) — `let _ = delete_volume(…)`
  in three places. Now retries once and returns `TemplateError::Leaked` with
  the volume id, which is the only handle that exists since a clone carries the
  *caller's* name. The orphan sweep's in-use set is a **required argument**, so
  it cannot be forgotten.
- **#41** (sequential heartbeat) — fixed by concurrency *and* by giving each
  probe its own deadline: the cluster HTTP client's timeout is 10 s, ten
  heartbeat intervals, which is what let one wedged peer swallow a round.
- **#19** (ublk never learns a new size) — the real find was that
  `UBLK_U_CMD_GET_FEATURES` is `_IOR`, not `_IOWR`. Encoded wrongly it errored,
  and for a *feature query* an error is indistinguishable from "no such
  feature", so `UBLK_F_UPDATE_SIZE` was never negotiated on a kernel that has
  it. **Only the on-metal test could have found this.** `resize_volume` is now
  grow-only; `shrink_volume` is the explicit door.
- **#18** (pool growth on pressure) — sources are configured, never discovered.
  A `directory` source only creates new files and is also how to grow into the
  free tail of the node's own disk. A `device` source already carrying a slab
  is **adopted with its data**, not reformatted.
- **#20** (volume move) — offline, filesystem-level, two calls. The copy pipes
  `pack_tar` into `unpack_tar` over a bounded channel driven by one `join!`:
  no scratch file, fixed memory, and tar is what preserves hard links and
  xattrs (SELinux labels). Restartable, **not** resumable mid-copy.
- **#35 / #34** — `qos_class` taxonomy pinned; CSI wire fixtures vendored into
  `contract/` and round-tripped. They passed as copied: the two sides already
  agreed. Note the stale-epoch *message wording* differs between the fixture
  and what the engine emits — only `current_epoch` is contractual.
- **#31** (iSCSI MC/S) — the whole job was moving **CmdSN to the session** and
  leaving StatSN on the connection (RFC 7143 §4.2.2.1). Also fixed a live bug:
  any connection closing used to tear down the whole session.

---

## Session 2026-08-20 — StormFS data-path surface (#49, #50) — DONE

Worked the issue list in reverse, in the lane the pallet work (#51–#60) was
not in. #50 is the newer number but sits on top of #49 by its own account
("base chunk lifecycle is the immediate blocker"), so #49 landed first.

- [x] `src/volume/chunk.rs` — chunk lifecycle (#49). A chunk is a run of whole
      slab slots inside one volume, addressed `(volume, offset, len)`.
      `allocate` is **eager and tier-scoped**: StormFS owns which tier,
      StormBlock owns where on it, so slots come from `best_slab_for_tier` and
      the GEM mapping is recorded now rather than left to allocate-on-write,
      which would place the chunk by the *volume's* policy and could fail on
      space this call reported as free. Deallocate is idempotent by
      construction. Trim is the same call with one bit changed: both free the
      slots, only deallocate returns the address range.
- [x] `src/volume/versioned.rs` — the three primitives (#50). CAS and atomic
      multi-block write are **one mechanism**; pins are the existing COW
      retention exposed.
- [x] `src/mgmt/api/stormfs.rs` + spec §9.1.1/§9.1.2, 18 HTTP tests.

### The journal the plan called for was not needed, and that is the finding

The plan above said a swap needs a commit journal in `<data_dir>` to roll a
partial swap forward, on the reasoning that the slab slot table is what
recovery reads. That was wrong about which record is authoritative. **The
durable record of an extent map is the volume metadata file**, written whole
and atomically with a checksum by `MetadataStore` — `rebuild_from_slabs` is
the fallback for when there is no such file, and the existing COW path already
leaves duplicate slot claims that only that fallback would ever see. So a
commit is untearable across a crash for free: the map is written in one piece.
Slots are still re-pointed (`Slab::reassign_slot`) so the fallback agrees, but
nothing hinges on it.

What *does* need care is that versions live in a second file. **The write
order is load-bearing**: versions first, then the map. A crash between them
leaves the version ahead of the map, so a stale writer is told to re-read and
finds the old data — it retries, which costs nothing. The other order leaves
the version behind a map that has already moved, and that writer would commit
over committed data. Versions must be monotonic, not gapless.

Also worth not re-deriving: a `409` from `commit` carries `current_version`,
so the writer never needs a second round trip to ask what it missed — the
same shape as `current_epoch` in the `/v1` surface (#35).

Not done: nothing here has met a real StormFS client. The primitives are
tested against the engine and over HTTP, but the two sides have not been run
against each other, which is the check that counts — the same lesson as the
ext4 work, where our reader agreeing with our writer proved nothing.

### Still open, and why

All of them are blocked on something outside this repo:

- **#44** (StormKV for the GEM), **#42** (SWIM gossip) — need work in StormKV
  first; #42 wants `stormkv-gossip` extracted into a shared crate.
- **#36 / #30** — need a box with a real NVMe namespace. See below.
- **#17**, **#16** — other repos.
- **#15**, **#7**, **#6**, **#5**, **#3**, **#2** — need the multi-node lab.
  The `/v1` API layer for #5/#6/#7 is done; only the engine data path is not.

### #36: the 5.4x inversion does not reproduce, but this rig cannot say more

`examples/qd_sweep.rs` measures 4K random write against a `ThinVolume`
directly — no iSCSI or NVMe-oF, since the transport is not what #30 is about.

**Read the harness's own noise floor before believing any number from it.**
The first two single-pass runs on dev.g8.lo disagreed with each other: one put
the warm peak at QD32 with no inversion, the next at QD1 with a 0.85x
"inversion". That is run-to-run variance, not a depth effect, and reporting
either would have been reporting noise. The harness now samples each depth
`--repeats` times with the passes **interleaved** (so host drift cannot land on
one depth) and reports the median with the spread beside it, calling the result
`INCONCLUSIVE` when the depth effect is inside the spread.

What is established:

- **No 5.4x inversion.** Every depth on this rig sits within a narrow band of
  every other; an effect that size would be unmissable.
- **CPU-seconds per million I/Os is flat (~22–26) across all depths, in every
  run.** That is the stable number and the discriminating one: lock contention
  makes the per-operation cost *rise* with depth. It does not.

What is **not** established: the backing is a QEMU virtual disk, not an NVMe
namespace, and the ~10–14k IOPS ceiling is probably that disk. So this says
nothing about the engine on real NVMe, and #36 stays open for exactly that.
#30 has lost its main justification and should be re-argued on its own merits
rather than treated as confirmed.

---

## Session 2026-09-08 — the management API had no gate (#107) — DONE (v14.0.0)

A node answered `GET /api/v1/volumes` and `POST /api/v1/fstemplates` from a
workstation with no credential. The mechanism to stop it — `require_token`,
with a read token, an admin token for destructive verbs, a public-path
exemption — **had been written and never wired to anything.**
`management.api_token` was checked by `/v1` alone, so a node whose config
named a token still served create/clone/seal/delete, exports, releases and
synonyms openly on `0.0.0.0:9090`. See [docs/auth.md](docs/auth.md).

Worth not re-deriving:

- **A guard on one surface is worse than a guard on none.** The setting
  existed and the token was configured, so the node read as closed. There is
  now exactly one check (`serve::api::decide`) behind one layer, over the
  whole router; `/v1` keeps its `{code, message}` envelope by prefix, not by
  a second middleware.
- **`/api/v1/health` must stay public.** It is the question a booting node
  asks of every address DHCP gave it, before it has any credential; a 401
  there is indistinguishable from "not an appliance" and drops the node to a
  shell. It reports `auth: required|none`, which is also how a fleet can be
  asked which of its nodes are open.
- **`/metrics` is not a probe** and `is_public` had always said so — but the
  engine merged the metrics router *beside* the guarded one, so the answer
  the code gave and the answer the process gave differed. It is merged inside
  now.
- **The default is still open, deliberately.** `boot-claim` — and the
  firmware a stage earlier — claims a machine's image before it has any
  credential, so flipping the fleet closed from inside the engine would stop
  machines booting. Closing it is a migration: distribute the token, then set
  `require_auth = true`. What was fixed unconditionally is the *silence*: an
  open node names what is exposed on every boot.
- **A minted token is local.** It authenticates a caller to the node that
  minted it and means nothing to a peer, so only a *shared* token
  (`api_token` / `$STORMBLOCK_API_TOKEN`) is presented outward — cluster
  replication, migration handoffs, `image build`, `boot-claim --token`.
- **The RouterOS profile had not compiled since `c709b7c`**, and the default
  build could not see it: `#[arg(env = ...)]` was reaching clap through
  feature unification from a default-only dependency. `--no-default-features`
  is the build that tells the truth about features — the same lesson as
  building on dev rather than the Mac, one layer down.

---

## Session 2026-09-08 (later) — #105, #106

Two faults that are the same shape: **a promise nothing was checking.**

### #105 — a stop that leaves the kernel holding the devices

The flush was already bounded (`da62b4b`). What was missing is that *nothing
ever told the ublk exports to stop*: the daemon flushed and returned while
every export's queue threads sat in `io_uring_enter`. A process that exits
without STOP_DEV and DEL_DEV leaves those threads in the kernel, and a thread
stuck in the kernel cannot be reaped — which is the four-day defunct process
on forge, and why *every* restart after it ended in failed mode.

- The signal goes out **before** anything is waited on, and the flush and the
  teardown settle together — a stop's budget is the sum of what it does in
  series.
- Waiting is done on a flag each export's thread sets, not on `JoinHandle`:
  `join` cannot be given a deadline. The subcommand paths (`boot-local`,
  `boot-iscsi`, `adopt-ublk`) *did* join, unbounded, which is the same fault
  one level down and on the node's own root path.
- **A unit's `TimeoutStopSec` must stay above the engine's budget (~13 s).**
  SIGKILL landing mid-teardown is what manufactures the unreapable thread, so
  the two numbers are one mechanism, not two settings.
- Verified on metal on dev: attach → `/dev/ublkb0`, SIGTERM, "stopping 1 ublk
  export(s)" → worker exits → device removed, 0.16 s, no defunct process.
  Deliberately did **not** demo the "before" — reproducing it on a shared
  build box leaves a zombie only a reboot clears.

### #106 — a release that outlived its volume

Eight versions with manifests, digests and download links for bytes that were
gone. The module's own rule ("all four or none") was right and unenforced
after publication.

- **State is derived, never stored.** `available`/`archived` comes from
  whether the volume resolves on every read. A stored flag would be a second
  copy of a fact the volume manager holds, and would be wrong exactly after
  the volume went — the event nothing was watching for in the first place. It
  also reads correctly on a node whose slab is not attached yet.
- **410, not 404.** The release *exists*; what is gone is the image. A 404
  sends someone looking for a typo in a version printed on the page in front
  of them.
- The reference goes in `what_is_serving`, not in the delete handler, so the
  move guard and the template sweep inherit it. `force=true` deliberately does
  not cover it: that flag is for a dangling synonym.
- Still not done, and deliberately (the issue's option 3): no automatic sweep
  of archived releases, and nothing here touches the 43 unpublished-artifact
  volumes on forge — that is `/build/forge-cleanup.py`, operational.

---

## Session 2026-09-08 (later still) — #109, #108

Two halves of one question: **who decides where a node boots from, and what
can they actually check.**

### #109 — the decision is a hook's to make

`/init` runs every executable in `/etc/stormblock/boot.d`, then
`/sbin/zeroboot`, before its own probe, and honours `boot-local` (exit 0) or
`ask-appliance` (exit 2). Generic on purpose — this repo takes no dependency
on zeroboot — and inert when nothing is installed.

- **`eval` on a hook's stdout is a hole, not a convenience.** The contract is
  `KEY='value'` lines because busybox has no `jq`, and the issue's own sketch
  was `eval "$(hook boot)"`. In PID 1 that makes a stray log line a command
  run as root before there is a system to run it on. The four values are read
  out with `sed`; the test installs a hook that prints a command among its
  assignments and checks it did not run.
- **A hook is asked, never obeyed.** Exit 0 with no slab, a slab that is not
  on this machine, or an action that disagrees with the exit status → the next
  hook, then the ordinary probe. Believing it trades a working fallback for a
  boot that commits and then drops to a shell.
- The block carries `# --- BEGIN/END boot hook` markers and is sourced by
  `tests/initramfs-boot-hook.sh` — the same pattern as the uplink selection —
  and is run under **busybox ash** on dev, which is the shell that actually
  runs it.

### #108 — and what it can check: `stormblock slab volumes`

Offline, read-only, no daemon/reactor/ublk/root. Reads the slab's own
metadata region the way `slab info` reads the header.

- **Three answers, not two.** `holds no volumes` (the slab can say, and is
  empty — the flow-over case) and `keeps no volume metadata` (the slab cannot
  say; the records are wherever `rd.stormblock.meta=` points) are different
  facts, and `read_metadata` returns `Ok(None)` for both — the discriminator
  is `has_metadata_region()`. A caller that conflates them boots off a disk
  that was formatted and never filled.
- **The probe was broken and nobody had noticed.** It checked the boot volume
  with `image inspect "$SLAB"`, which reads a *disk* and wants a GPT. Handed
  the partition a loader entry names (`/dev/sda2`) it fails with "no usable
  GPT on this device", which the branch read as "no boot volume" — so such a
  node asked the appliance on every boot however good its disk. Found by
  running the command, not by reading it.
- **`FileDevice::open` creates what it cannot find**, and opens it for
  writing. A survey command must not: `open_read_only` is the door for
  anything that inspects, and `slab volumes /dev/sdz` no longer creates
  `/dev/sdz`.
- `slab format --role system` reserved **no** metadata region — fixed in the
  same session. `image build` had always given both roles one, so the two
  ways of making a disk produced different things, and the hand-made one
  could only be read by attaching it. Every formatting path now auto-sizes a
  region (`--metadata-bytes 0` opts out); `Slab::format` stays the plain
  primitive that reserves nothing, because ~20 callers and tests depend on
  its geometry.

---

## Pallets — engine support (2026-08-19, #51/#52)

A **pallet** is a GPT partition holding a named, versioned, self-contained set
of sealed member images plus the manifest that describes them. On-disk format
is specified in `docs/pallets.md` (it began as `stormuefi/docs/PALLET-SPEC.md`,
now a pointer here); the reader is `crates/pallet-format`, which stormuefi
links. stormblock owns the **producer** side.

The engine had no notion of one: a drive carried a slab and nothing else, so
there was effectively a single implicit grouping. What the model needs is
**many pallets, several per drive, spread over several drives**, discovered by
scanning rather than configured.

- [x] `src/pallet/format.rs` — v1 writer + reader, byte-compatible with
      `crates/pallet-format` (once `stormuefi-map`). Content first, header last, so a torn publish leaves a
      pallet that fails its own CRC rather than one that lies.
- [x] `src/pallet/gpt.rs` — GPT read/write, protective MBR, primary + backup.
      Activation is an **attribute write** (bits 48–63), never a data write.
      Allocation is first-fit in free space, 1 MiB aligned, and refuses to
      alias — firmware does not publish a handle for an overlapping entry.
- [x] `src/pallet/store.rs` — discovery across every opened drive; selection
      order (priority desc, version desc) with the spec's candidate rule.
- [x] `src/pallet/manager.rs` — the lifecycle library (#52): compose, publish,
      verify, activate, mark-successful, roll back, prune with keep-N-1.
- [x] `/api/v1/pallets` + `stormblock pallet` CLI.
- [x] `docs/pallets.md`.
- [x] `src/pallet/select.rs` — the read-only half, as pure functions plus a
      `PalletBrowser` that cannot write. This is what a firmware or initramfs
      consumer holds, and what stormuefi mirrors.
- [x] A pallet **kind** (boot/system/kernel/kube/app/runtime/data) and a
      version label, in the superblock's reserved area, zero meaning
      "unspecified". Priority orders only pallets of the same kind.
- [x] Moves: a whole pallet between drives keeping its identity, and one member
      between pallets as a new version of each.
- [x] Whole-drive pallets (one pallet per device, no GPT) stay discoverable and
      `adopt_whole_drive` migrates them onto a partitioned drive.
- [x] `convert_drive(src, dest)` — the whole-drive operation: copy every pallet,
      verify each at the destination, remove from the source, optionally hand
      the source a fresh table. Refuses to wipe a source that is still the only
      copy of something that failed to convert.

### Standing clones (2026-08-19, #55) — RETIRED by #137 (v18.0.0)

A sealed template holds one pre-minted clone; `claim` takes it and replenishes
behind the caller. The engine owns it because the engine owns templates,
snapshots and volumes — so the invariant holds without anyone asking, and
stormboot's fast path works before the registry is up.

Three things that are cheap now and expensive later, all from the interim
version in sbregistry:

- **Keyed by the template**, never by a name or tag. A template is derived from
  the manifest digest, so a moved tag needs no detection or repair — the new
  manifest is a new template with its own standing clone.
- **One, not a pool.** A second only helps when two starts of the same template
  collide, and these nodes are memory constrained.
- **Never handed out twice.** `claim` takes the field under the store lock;
  the loser of a race mints its own. Two containers on one writable filesystem
  is the worst outcome available here, and it is silent.

sbregistry's interim implementation (`standing` on `CloneRec`,
`POST /v1/clones/claim`) should be **removed**, not kept in parallel.

**Check and fix are separate verbs.** `GET /api/v1/fstemplates/standby` reports
which templates would make a start wait; `POST` on the same path mints what is
missing. A supervisor must be able to ask whether a node is warm without the
asking making it true. Both are idempotent — safe on every start.

**A take is a take**: an ordinary clone tops the template back up too, not only
a claim. A standby mint is flagged (`CloneSpec.standby`) so it does not count
against `clones` — that number means "how many went somewhere" — and does not
trigger a top-up of itself, which is what would make minting recursive.

### The format's read side is a crate (2026-08-19, #53)

`crates/pallet-format` — `no_std`, no allocation, no async, no I/O, **no write
path**. Firmware links it, so it must stay small enough to read in one sitting
and structurally unable to write. stormblock keeps emission and writes at the
offsets `pallet_format::layout` defines.

The rule this encodes: **one on-disk format may have only one reader.** Two
hand-maintained readers in two repos drift, and the drift fails as *the node
does not boot*. Emission needs no such sharing because there is only ever one
writer.

Check it with `cargo check -p stormblock-pallet-format --target
x86_64-unknown-uefi --no-default-features --features verify` — the `no_std`
claim is verified, not asserted. Its tests work from hand-built bytes on
purpose: a decoder tested only against its own encoder proves nothing.

### Image building (2026-08-19)

`stormblock image build` assembles disk images and ISOs out of pallets —
`src/image/`, [docs/images.md](docs/images.md), verified by
`ci-image-verify.sh`. An image file is a drive, so the builder drives the
ordinary `PalletManager` rather than reimplementing publishing.

**Two things external tools found that ours could not**, and the reason
`ci-image-verify.sh` exists:

- `mtools` showed `BOOTX64.EFI` stored as `BOOTX6~1`: the FAT name sanitiser
  replaced the dot before splitting the extension, so every plain 8.3 name
  became a long one.
- `xorriso` showed the El Torito boot image saturating its 16-bit sector count.
  **FAT32's 65,525-cluster floor (~33 MiB) sits just above El Torito's 32 MiB
  ceiling** — the two do not overlap, which is why `fat.rs` writes FAT16 too.
  An ESP for an ISO must be ≤ 32 MiB.

**Learned, and worth not re-deriving:** the GPT LBA size is *not* the device's
block size. A file device reports 4096 because that is the I/O size it prefers;
an image assembled as a file needs 512, which is what every tool and firmware
assumes. A 4Kn table on an image is one our own reader accepts and `fdisk`
cannot find — so the check that counts is an external one, byte by byte.

Not covered here, and still open on #51: volume-level sealed/read-only attach
refusal, and per-leg physical offsets for a read-only consumer that must not
reconstruct RAID.

---

## Session 2026-08-26 — NVMe-TCP initiator (#73) — DONE

The engine can now *attach* NVMe-TCP, not just serve it.
`drive/nvmeof_dev.rs` is an initiator `BlockDevice`;
`nvme-tcp://host:port/<nqn>?nsid=N` (and `iscsi://host:port/iqn`) are
accepted everywhere a device path is — `[[drives]]`, `POST /api/v1/drives`,
RAID `add_member`. Proven on dev end to end: engine A attached engine B's
namespace through the API and created a RAID-1 across a local drive and the
remote leg, both members active. This is the cross-node RAID-leg transport
stormstorage's DistVolume model orchestrates.

Worth not re-deriving:

- **Reuse the test initiator's framing.** `tests/common/nvmeof_initiator.rs`
  already carried the initiator direction, proven against this target *and*
  the Linux kernel (the FCTYPE-at-byte-4 lesson lives there). The device is
  that wire logic productionized: admin conn (QID 0) identifies at open and
  is dropped; one I/O conn (QID 1) behind a Mutex, cleared on error so the
  next op reconnects — a bounced remote degrades to per-op errors RAID can
  see (#69 is where those errors should start flipping member state).
- **`DeviceId.uuid` is uuid5 of the attach URI** — stable across reopens.
  Do this for every new backend; #65 is the cost of not doing it.
- **libc's ioctl request type moves.** c_ulong on glibc, c_int on musl, and
  it has changed across libc releases — keep raw u32 consts and cast
  `as _` at the call site (`nvme_smart` broke exactly this way when the
  lockfile advanced).
- **The TOML `[iscsi]`/`[nvmeof]` sections are dead** — targets read the
  CLI values with clap defaults (#75). Two engines on one host need
  `--iscsi-addr/--nvmeof-addr/--nvmeof-nqn` until that's fixed.
- The NVMe-oF target only starts when an `export_device` exists at boot
  (`main.rs:1128`); dynamic namespaces exist (#26) but no boot device means
  no listener at all.

---

## Volume-level redundancy — the RAID the design actually needs (2026-08-28)

**Correction of record.** `src/raid/` is drive-level: `RaidArray` mirrors or
stripes whole member devices and a slab sits on top, so every volume on that
slab gets the same protection and a node can have exactly one answer. That is
not the model. **Redundancy is a property of a volume**, realised by placing
that volume's extents across N distinct physical drives, and a node carries a
mix — `app-data-1` as a two-way mirror, another volume as 4+1 parity, a
golden's clones inheriting the golden's policy — side by side on the same
drives. System and kernel pallets are mirrored as pallets (#56); data is
mirrored or parity-protected per volume. This is what zeroboot installs onto,
so it is the blocking item; drive-level `RaidArray` stays as a leg transport
and for whole-device use, nothing more.

### Design

- **`FailureDomain`** (`placement/domain.rs`): an ordered chain of
  `rung=value` — `site/building/room/row/rack/node/hba/shelf/bay/drive` (#72's
  vocabulary, as a chain not a flat map, so #71 is not built twice). A slab
  carries one; by default it is `drive=<device serial|uuid>`, and a drive
  registered with `labels` (#70 item 1) extends it. Two slabs are *the same
  domain at rung R* when their chains agree through R.
- **`RedundancyPolicy`** (`volume/redundancy.rs`): `none` | `mirror:N` |
  `raid5:D+1` | `raid6:D+2`, plus the rung to spread at (default `drive`).
  Spelled `mirror`, `mirror:3`, `raid1`, `raid5:4+1`, `raid6:4+2`, `raid10`
  (= mirror on organic placement, since striping is what slabs already do).
- **A hard boundary, not a preference.** Every leg of an extent — and every
  data member and parity leg of a stripe — lands on a distinct domain at the
  policy's rung, or the allocation fails. Creation is refused up front when
  the node cannot satisfy the policy at all.
- **GEM**: `ExtentLocation` gains `mirrors: Vec<Leg>`; `legs()` is primary
  plus mirrors. Parity volumes keep one data leg per extent and a per-volume
  `ParityGroup` per stripe (stripe = `data` consecutive virtual extents; P and
  Q legs, own ref count, so a clone shares parity until a COW moves it). The
  reverse index covers every leg, so GC and evacuation see them. Parity slots
  record `PARITY_TAG | leg << 56 | stripe` as their virtual extent so
  `rebuild_from_slabs` can tell them apart. Metadata **V4**.
- **I/O**: mirror writes go to every leg and ack when all healthy legs have
  it; reads pick a leg and fall through on error. Parity writes are
  read-modify-write under a per-stripe lock; a lost data slot is
  reconstructed from the stripe. A leg whose write fails puts its slab in
  the volume's **failed set** (persisted): skipped for reads and writes,
  volume reports *degraded*, until `resync` rebuilds every leg that was on
  it onto a fresh domain and clears it. That is also how `none → mirror:2`
  and `mirror:2 → mirror:3` are applied: set the policy, resync. The RAID-5
  write hole is the same as md without a journal; `resync?verify=true`
  recomputes parity.
- **Clones inherit** the source's policy: shared extents are already
  replicated, and every COW re-replicates.
- **Surface**: `redundancy` + `spread` on `POST /api/v1/volumes`, on
  `TemplateSpec`, on `[[volumes]]`; `redundancy` and `health` on every volume
  response; `PUT /api/v1/volumes/{id}/redundancy`; `POST
  /api/v1/volumes/{id}/resync`. Slabs carry `domain`; drives accept `labels`
  and `uuid` on `POST /api/v1/drives` and list their slabs (#70 items 1–2).
- **Out of this cut**: chunk/versioned (StormFS) volumes stay `none` — StormFS
  replicates above; converting to or from parity (a restripe); drain over
  HTTP (#70 item 3) and the health inbound (#70 item 4).

### Work plan — DONE (v10.0.0)

- [x] `placement/domain.rs` + registry domain tracking + domain-aware best-slab
- [x] `volume/redundancy.rs`
- [x] GEM: legs, parity groups, reverse index, rebuild
- [x] metadata V4 (V3 shape kept, converted on load)
- [x] every consumer of a location frees/shares *all* legs
- [x] thin.rs: mirror + parity paths, failed set, health
- [x] VolumeManager: create options, inherit, persist/restore, resync, set policy
- [x] HTTP + template + config surface; drives `labels`/`uuid`, `/drives/{id}/slabs`
- [x] tests: mirror across two slabs, degrade, resync; parity 2+1 reconstruct;
      clone COW keeps policy; insufficient domains refused; V3 → V4 load;
      RAID-6 two-member loss; restart; set+resync
- [x] docs/redundancy.md, CHANGELOG, README; build + test on dev
- [x] pallets: `copies` on publish, legs reported in status, resync (#56)

### Worth not re-deriving

- **A removed slab has no domain, and an empty domain must constrain
  nothing.** The first resync test failed with "no slab apart from 2
  domains" because the *lost* slab's empty chain was in the exclusion set
  and `same_at` treats unknown as shared. Right for a candidate (never
  place on a slab you cannot tell apart), wrong for an exclusion.
- **The reverse index has owners.** `insert` used to drop the reverse
  entries of the old location unconditionally — so a clone COWing an
  extent took the *source's* slot out of the index. Every removal now
  checks ownership. This was pre-existing and would have made evacuation
  miss shared slots.
- **Restore precedence.** "Slot table wins" only worked by iteration order:
  two slots for one extent (a COW's old and new) both had generation 1.
  `allocate_gen` records the COW generation, and restore takes the record
  unless the slot table is provably newer.
- **Lock order.** Redundant writes take the extent/stripe shard *before* the
  volume lock; `discard` therefore must not take the volume lock for a
  redundant volume. Parity never takes the volume lock at all.
- **`sync_refs` after `dec_ref`.** The GEM's `ref_count` on the *owner* is
  otherwise never lowered when a clone diverges, so the owner COWs for
  nobody forever; for parity that also meant the source's group never
  went back to in-place RMW.

### Follow-on — the pieces left out of v10.0.0 (2026-08-28) — DONE (v11.0.0)

- [x] **Drain over HTTP** (#70 item 3): `POST /api/v1/drives/{id}/drain` → a
      background task moving every leg off every slab on that device, one
      extent at a time (locks per extent, so I/O keeps flowing), progress at
      `GET …/drain`, terminal `empty` = safe to remove. Slabs being drained
      (or quarantined) take no new allocations.
- [x] **Health inbound** (#70 item 4): `POST /api/v1/drives/{id}/health` with
      a stormdrive report → quarantine the drive's slabs for placement and
      put them in the failed set of every *redundant* volume with a leg
      there (an unreplicated volume's only copy stays readable); `failed`
      orders a drain.
- [x] **Rebalance by failure domain** (#71 item 3): fix legs that collide at
      a rung (placed before labels existed) and even out allocation across
      domains after a shelf is added.
- [x] **Topology as a chain** (#72 item 1 remainder): `[management].topology`
      feeds the node rungs of every slab's domain and /v1 reports the chain.
- [x] **Dirty-stripe log** for the parity write hole: mark a stripe before
      its read-modify-write, clear lazily, verify only the dirty stripes on
      restart.
- [x] **Restripe**: change a policy to or from parity by copying into a new
      placement and swapping the map; refused while exported.

### #76 — a template is a volume that has been sealed (2026-08-28) — DONE (v12.0.0)

Lineage, sealing and filesystem identity move onto the **volume**:

- `VolumeRecord` (metadata **V5**) gains `parent`, `sealed` and `fs`
  (kind, journal, features, 64bit, metadata_csum, csum_seed, label, uuid).
  `create_snapshot` records the parent and inherits `fs`.
- A sealed volume refuses writes, discards and shrinks — sealing is a state
  transition, not a snapshot into a second object. `fs::template::seal`
  seals the raw volume **in place**: one template is one volume, so the
  `-raw` half that leaked (#47) no longer exists.
- **Cloning always stamps.** `fs::clone_volume(vm, source, spec)` is the one
  clone: snapshot, fresh filesystem UUID when the source carries a
  filesystem, fsck, lineage recorded. `clone_template`, the volume snapshot
  API and the /v1 `source: volume` path all go through it.
- `POST /api/v1/volumes/{id}/seal`, `POST …/{id}/clone`, `GET …/{id}/lineage`;
  `parent`, `sealed`, `fs` on every volume response; `from_template` on
  `POST /api/v1/volumes` also accepts a sealed volume by id or name — the
  blank-ext4-built-into-the-image case that was in neither namespace.
- The `FsTemplate` store stays as the HTTP view (name, standing clone,
  clone count); everything that is a property of the filesystem or of
  lineage is read from and written to the volume record. Persisted
  templates are adopted at startup: their sealed volumes are marked sealed
  and given their `fs`.
- **#78, same split one layer up:** attach lived only on `/v1`, whose
  volume registry is a second store, so a clone made through `/api/v1`
  had no path to a block device. `POST /api/v1/volumes/{id}/attach` is
  the volume-level door (same `AttachInfo`, same ublk/NVMe machinery,
  no epochs or fencing — that stays `/v1`'s contract). The natural end
  of this line is `/v1` becoming a view over engine volumes rather than
  its own map; not done.

### Follow-on 2 (2026-08-28): #77, #72, #79 — DONE (v12.2.0)

- [x] **#77** — `stormblock image build` seals every golden it lays down
      (and records its `fs`), so a blank arrives cloneable and the claim
      path asserts instead of repairing.
- [x] **#72** — the discovery beacon carries the node's topology chain, so
      `/v1/nodes/capacity` reports `topology_chain` for peers too.
- [x] **#79** — dependency cut (335 → 212 default, 262 → 186 RouterOS; `hyper`/`hyper-util` stay direct — they cost nothing beside axum): one hyper-based HTTP client instead of
      `reqwest`; `/metrics` rendered on the axum route instead of
      `metrics-exporter-prometheus`; no direct `hyper`/`hyper-util`; `ui`
      off by default; parse-only TOML. Measure before and after.

### Kubernetes-shaped resources, served by the engine (2026-08-28, #80)

Every component serves its own resources (Glenn: "the kube resources should
be in each component"). stormblock: `/apis/storage.storm.io/v1/{volumes,
slabs,drives,nodes}` — `apiVersion/kind/metadata/spec/status`, API
discovery at `/apis` and `/apis/storage.storm.io/v1`, `?watch=1` as a
newline-delimited event stream, writes on `Volume.spec` (redundancy,
sealed, resync) and `Drive.spec` (labels, drain) only. `metadata.name` is
the uuid — engine names are not unique — with the human name in
`spec.name` and `metadata.labels["storm.io/name"]`; get accepts either.
Read-mostly projections of the same state the REST API serves: no second
store.

- [x] `src/mgmt/api/kube.rs` + tests (v12.3.0); stormdrive 0.6.0 serves `drives`/`enclosures`

### VM disk goldens and cloud-image import (2026-08-28) — DONE (v12.4.0)

- [x] `fs/disk.rs`: recognise GPT/MBR on a volume (`fs.kind = gpt|mbr`,
      `uuid` = disk GUID / MBR signature); stamp a fresh one on every clone
      so clones attached to one host do not collide on PARTUUID. `seal`
      needs no `force` for a partitioned image.
- [x] `image/decode/`: disk-image readers (`image/formats` is the *output* side) — raw (as now) and **qcow2**
      (v2/v3, zero clusters, zlib-compressed clusters, no backing chain) —
      detected by magic, used by `[[slab.golden]] from=` and by the import.
- [x] `POST /api/v1/volumes/import {name, file|url, format?, redundancy?}`:
      async job, streams a URL to `<data_dir>/imports/`, writes only the
      allocated clusters, seals the result with its `fs` recorded. This is
      how a cloud image becomes a golden. VMDK (sparse, streamOptimized,
      flat descriptor) and the VMDK inside an OVA too; an ISO is raw and
      recognised as `iso9660`.

### Composed disks — a per-node disk is a chain of goldens (2026-09-03)

**Where it stood.** `POST /api/v1/volumes/compose` (v13.3) makes a volume
that is a *list of* goldens, sharing their slots. It is not bootable: it has
no partition table, and a pallet published by `image build` lays its members
down as *partition bytes*, so a node's disk is still a copy of every golden
it carries — 8.7 GB of pallets for 2.4 GB of content, per node.

**The change.** Everything on a disk becomes a golden, and a disk is a chain
of them (Glenn: "GPT could be in a golden, and beginning of the chain"):

- **A pallet is a sealed volume.** `compose_pallet` builds the pallet header
  and lays each member down at a **slot-aligned** offset
  (`PalletBuilder::content_align`), so a member that is already a golden is
  *shared in* by `gather_into` rather than copied. Only the header (and any
  inline `text` member) is written. The result verifies through the ordinary
  `Pallet::read` + `verify_all`, is sealed with `fs.kind = "pallet"`, and is
  what every disk of that version composes in.
- **The GPT is two goldens.** For a layout — LBA size, disk size, and the
  ordered partitions with their types, sizes and attributes — the protective
  MBR + primary header + entries fit in the first slot and the backup entries
  + header in the last. Both are minted once per layout, named by a digest of
  it, and reused by every disk with that layout. Partition GUIDs are derived
  from the layout too, so `root=PARTUUID=` is fleet-stable.
- **A disk is `compose(head, partitions…, tail)`.** Nothing is written:
  `copied` is zero unless `fresh_guid` asks for a per-node disk GUID, which
  costs the two GPT slots. The disk's `fs` is recorded as `gpt`.
- **LBA size defaults to 4096**, because that is what the NVMe/TCP and ublk
  paths present a volume at (`ThinVolumeHandle::block_size`), and firmware
  parses a GPT in the media's own block size (§2.4 of docs/pallets.md). A
  disk meant to be copied onto a 512-byte drive says `lba = 512`.

Work plan — DONE (v13.4.0):
- [x] `PalletBuilder::content_align`, `MemberSpec::reserve`
- [x] `Gpt::create_for` / `Gpt::render` — head and tail bytes without a device
- [x] `fs::disk::detect` recognises a pallet (`STORMPAL` at 0)
- [x] `volume/disk.rs`: `compose_pallet`, `compose_disk`, GPT goldens by layout
- [x] `POST /api/v1/volumes/compose/pallet`, `POST /api/v1/volumes/compose/disk`
- [x] tests: 8 unit (`volume::disk`) + `tests/integration_compose_disk.rs`
- [x] docs/composed-disks.md, CHANGELOG, README
- [x] `ci-compose-disk-verify.sh` — the built binary, a real kernel and
      initramfs, an ESP with shim + grub, ublk attach, then `fdisk`, `blkid`,
      a mount, the kernel digest, `pallet verify`, and an OVMF boot

Not done: a CLI subcommand; `image build` pointing at composed pallets; an
ESP built from a directory over HTTP; a stormcos node actually booting from a
composed disk over NVMe/TCP.

### Worth not re-deriving

- **"copied becomes non-zero"** is the retier report's `copied` count —
  extents shared with another volume that demotion copies rather than moves.
  While a node's disk was a byte copy of the image nothing was shared and it
  was always zero; composed disks are what make it real.
- **`DEFAULT_EXTENT_SIZE` is not `DEFAULT_SLOT_SIZE`** (4 MiB vs 1 MiB).
  A member's span is its golden rounded up to a *slot*, so the HTTP test had
  to be written in slots; the first version in megabytes failed on dev.
- **The GPT goldens keep the layout's GUID.** A `fresh_guid` stamp lands on
  the composed disk's copy-on-write slots, so the shared head and tail are
  never touched; the boot ladder writing `tries` into a disk's entry is the
  same COW.
- **`ThinVolumeHandle::name()` is async**; `ThinVolume::name()` is not.
- **The ESP must be formatted at the disk's sector size** (`mkfs.vfat -S
  4096`, ≥ 64 MiB for FAT16). A 512-sector FAT on a 4Kn disk is vfat to
  `blkid` and "can't read superblock" to `mount` and to firmware. Found by
  `ci-compose-disk-verify.sh`, not by any test of ours.
- **`allocated_bytes` on a composition counts what it maps.** The slab's
  free-slot count is what says whether anything was written.
- **A head golden alone is not a readable GPT** — its alternate LBA points
  past the golden's end. Read the header at LBA 1 directly, or read the
  composed disk.
