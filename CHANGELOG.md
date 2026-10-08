# Changelog

## [Unreleased]

### 2026-10-08
- **feat:** #365 (P0, owner): the engine's logging says who holds its locks, and its watchdog no longer floods the log.
  - **Named lock holders:** the volume manager, slab registry and extent map are `lockwatch::TrackedMutex` / `TrackedRwLock`. Their guards record the holder: an API request (`GET /api/v1/volumes (req 42)`, each request runs under that name) or a named task (the flow-over, the eraser, a rebuild, a drain, the serve reconciler and reaper). They also record since when, and the waiters. A hold or wait over 1 s is logged when it ends (WARN over 10 s). Records are removed by guards, so an error, panic or cancelled request leaves nothing behind (tested).
  - **Watchdog:** one line per stalled request, naming each lock it waits on with that lock's holder, age and waiters. No thread or task dump on a timer.
  - **Dumps:** `/debug/tasks` and `/debug/threads` are admin-only, on demand. `/debug/locks` reports the holders and waiters (route families in the open view).
  - **Per-request line:** method, path, status, total time, lock wait, caller (admin, node, client-cert, bearer, anonymous or open) and peer address. Probes are at DEBUG; a request abandoned by its caller is said.
  - The log limiter that dropped warnings and holder lines is stormcast's: filed there.
- **fix:** #362 (P0, stormcos#459): `compose/slab` places a golden by the extents its source maps, not by its declared size. A blank is its metadata, so a ladder of templates up to 16 TiB fits a small data slab. Before, every slot of the declared size was taken, so a ladder declaring about 21 TiB was refused in a 16 GiB slab. The golden keeps its declared size, its holes read as zeros, and its clones allocate on write. A source with parity keeps the full contiguous layout. `compose_slab` also pages a cold source map in before reading it. The image build, install, flow-over and staging already lay goldens by their mapped extents.
- **fix:** #358 (P0, live on the Dell): the volume manager was held for 3.5 h by `POST /api/v1/fstemplates`. Two causes, both fixed. (1) The NVMe/TCP initiator bounded nothing: no connect timeout, no I/O timeout, no TCP keepalive, so a command on a connection that died without a FIN or RST waited forever. Every command and connect is now bounded (`STORMBLOCK_NVME_TCP_IO_TIMEOUT_SECS`, default 30 s), and keepalive probes after 10 s idle. A timed-out command fails, its connection is dropped, and the next command reconnects. (2) After a complete flow-over the appliance's slabs stayed registered, so every persist on the node, made under the volume manager's lock by every create, clone and delete, went on flushing and writing a remote slab it no longer needed. Once everything has moved and the local boot is laid, the remote sources are retired (`VolumeManager::retire_drained_slab`; never a local slab, never one that still has a leg).
- **fix:** #358 (P0): `GET /api/v1/fstemplates` (and one template) no longer waits on the volume manager. The listing looked for each template's sealed volume under the manager's mutex, once per template. Every create, clone, delete and seal holds that mutex through its durable persist (slab flushes included, seconds each on the Dell's SMR disk), so under sbregistry's test-image builds the listing queued behind each of them in turn and didn't answer within 30 s. It now reads `VolumePresence`, a set of existing volume ids kept beside the manager's map. **feat:** `/debug/locks` and the API watchdog's stall captures list every metadata persist in progress (generation, phase, age, slabs, and whether its caller holds the volume manager), so a stall behind the manager says what it waits on.
- **fix:** #120: the image build report's `allocated_bytes` is what its doc says, the bytes a volume maps, not what it costs the slab (a golden and its first clone share every slot, so both read 0 since 5e4e5d3). **test:** #222: the engine e2e runtime tests send the node token (#107) and name the attaching host (#210); they predated both. The flow-over resume runtime test expects #259's refusal (named stranded volumes), not the dropped mappings of before. `ci-runtime-tests.sh` runs every test binary (`--no-fail-fast`).
- **fix:** #222: `boot-local --meta <dir>` (the fallback when no slab carries metadata) read only v1's `volumes.dat`, so it refused every data directory written since format 2 became the default (#158), which keeps `metadata.v2`. `VolumeManager::load_data_dir` reads either. Found by the runtime tests' first run (`check.sh`); nodes keep their metadata on their slabs and never take this path.
- **docs:** #356: `docs/data-classes.md` reviews the data classes **system / partner / customer** (pallet kinds `system`, `vendor`, `user`) from stormcos#17/#20, f94effa (pallets.md §2.5–2.8) and the owner's rules (#311, #312, #349, stormcos#456). It covers each class's partition, app data, owner and signer, and what an install, a demote/reset and a factory wipe do to it. `system-data` is the system class's kept data. Built today: two halves (slab role, volume `origin`) and no class split; `vendor`/`user` are written by nothing. The doc ends with the questions the build needs answered. pallets.md §2.5 names the classes; boot-hooks.md and README point to the doc.
- **fix:** #360: the test image's suites failed at `engine-up` since #274 (`POST /slabs` is destructive, and the harness held only the node token). The harness now runs its in-pod engine with `admin_token_file` in the work dir and uses that token. `check.sh` runs the `short` suite in the image too.
- **fix:** #357: 87d2d99 did not compile (E0597 in `ensure_system_data`: the registry guard was borrowed by the block's tail expression; fixed in e978796), and the test image could not build. **build:** `check.sh` is the one routine check now (`sc-build 'sh check.sh'`): the test image (`test/build.sh`), every initramfs test under sh and busybox sh, the nextest suite, the runtime tests. A commit isn't called done until it passes.
- **feat:** #355 (P0, stormcos#456): `system-data`, the node's kept record of itself.
  - **The volume:** `boot-local` makes it (ext4, 4 GiB thin) in a local data half when it isn't there yet, and exports it after every other ublk device. Every install keeps it; a diskless boot makes none.
  - **Mounted by `/init`** at `/run/stormblock/system-data` (which survives `switch_root`). It writes `config/mounts.release` (the release's mount list), `history/boots/<time>.json` (release, tag, cmdline, the disk verdict and storage inventory, the handover) and `history/installs/<time>.json` for a boot that installed, keeping the newest 500 boot records.
  - **Mounts** come from `/etc/stormblock/mounts` (#262); stormcos#259 takes the command-line list away.
- **feat:** #177: a boot claim leaves a record a manager can read.
  - **What it holds:** `last_claim` gives when the machine claimed and as what, what it got (clone, host golden, the release it was pointed at, the assignment's version), the host NQNs, and the boot agent's `agent` and `inventory` as sent with the claim (stormbootx#90, #20; each kept when a JSON object of at most 16 KiB).
  - **Where:** on `GET /api/v1/boothost/{name}` and its list (it showed three fields), and now also on `GET /api/v1/synonyms/boothost/<tag>` and the synonym list. Persisted.
- **feat:** #188: `/serve/v1` is no longer capped at 128 NVMe exports per node.
  - **One listener:** each export is now a subsystem (its own NQN, the volume as namespace 1, its host access) on one listener at `[serve] portal_base`. It used to be a target on its own port from a 128-port span.
  - **The drain, per subsystem:** the target gives every subsystem an accept gate and a live-connection count. A withdrawn export refuses new Connects and goes when its own connections end, which is the guarantee the per-port drain gave. iSCSI exports keep a port each.
  - **Upgrading:** an export restored from before is served on the shared port, and its attach parameters say so.
  - **Test:** 300 exports on a span of 8, each reachable with its own volume; one drained under an attached host while its neighbours serve on.
- **fix:** #187: `systemd/stormblock-ublk.service` `TimeoutStopSec` 10 → 30, above the engine's ~13 s stop budget, so SIGKILL never lands mid-teardown (#105). A test (`cli::tests::every_unit_outlasts_the_engines_stop`) reads every unit in `systemd/` and fails one at or below the budget.
- **fix:** #349 (P0, stormblock-registry#104): an install dropped sealed volumes the node had made in the system half (registry goldens, held media) without a word.
  - **Origin on every volume:** `release`, `node` or `unmarked`. `image build`, the claimed release image at `boot-local` and staged copies are `release`; everything the engine creates is `node`. It is carried in format v2's volume header after the fields older engines read (they skip it), and shown on every volume as `origin`.
  - **The install:** of the system-half volumes the new release doesn't name, an old release's are dropped, and the node's and unmarked ones (sealed or not) are carried. Their extents move into the data slab, keeping id, sharing and record, before the system half is laid again. It is reported as `carried` on the console and in the report.
  - **When it stops instead:** no room in the data half, or parity in the system half, stops the install before anything moves.
  - **Also:** `role` on `POST /api/v1/volumes/import` (working since #93) is now named in the route's doc and covered by a test.
- **fix:** #347 (P0, owner's correction to #344): every SES enclosure is a shelf, and the system disk is chosen by the shelf's position.
  - **The model:** shelves are internal, front, rear or external, each with its own id (the enclosure's logical id), position and bays. Shelf membership is never a reason to refuse a drive.
  - **The rule:** the system half goes on the machine's own shelves (internal, front, rear) unnamed. An external shelf's drives are taken only when `rd.stormblock.slab=` names them.
  - **Telling them apart:** behind an expander means external, unless the SES identity is a known server backplane (Dell `DP BP…`); when in doubt, external.
  - **Reported:** `local_disk.shelf` names the shelf and bay, and the storage inventory gives every drive's shelf.
  - **Replaces** #344's first fix ("a shelf is only behind an expander").
- **fix:** #346 (P0): a blank disk was refused (`the data slab will not open: bad slab magic`) and the node ran diskless.
  - **Cause:** `take_local_disk_for` and the identity guard went by partition types. A disk that kept a node table with nothing in it (zeroed but for its table, or a lay cut short before its data slab) counted as "this node's", and its data slab "would not open".
  - **Now:** the data partition's slab magic decides. With no magic there is nothing to keep, and both halves are laid fresh without `force`. A slab with its magic present that does not open is still refused and named, and so is a data partition that cannot be read.
- **fix:** #344 (P0): the Dell ran diskless from forge with its slabs on `sda`, and said nothing.
  - **Cause:** #273's survey took any drive in an SES enclosure for a disk shelf. The R230's own bays on its mpt3sas HBA are an SES enclosure, so with no `rd.stormblock.slab=` and no boot intent, `sda` was skipped and refused.
  - **The rule now:** a shelf is a drive behind a SAS expander.
  - **Never silent:** the boot's verdict on the machine's own disk (taken, refused with why, failed, none) is written to `/run/stormblock/local-disk.json` by the survey and by `boot-local`. It is said on the console, by `adopt-ublk` too, and reported in health as `slabs.local_disk`.
- **fix:** #195: `/v1` promote and dual-attach expiry tear down what they drop.
  - **Promote:** every attachment record it drops takes its data path with it: the ublk device is removed and the namespace released.
  - **Expiry:** an expired window aborts the way `close {outcome: abort}` does.
  - **On time:** windows expire on every `/v1` call, reads and detach included, and on a one-second timer, not only inside a create/attach/fence/promote.
- **feat:** #212: `/serve/v1` exports bound to one host.
  - **`host_nqn`:** accepted on `POST /serve/v1/exports` and on a `/serve/v1/volumes` create with `export`. The export's own subsystem then admits that host alone, and discovery on its portal shows it to that host only. The host is kept on the wiring row and returned with the export.
  - **`[serve] allow_any_host`:** default `true`, today's behaviour: an export naming no host admits any host, said at start. `false` refuses such an export and closes rows from before host binding.
- **fix:** #218: the volume listing's `generation` moves on what changes without a metadata persist.
  - **What moves it:** attach and detach on every transport (mounts included), slab presence, quarantine and failure, drains, the rebuild queue, RAID member states and owners. A mirror that trusted the `304` from `?since=N` / `If-None-Match` missed these before.
  - **How:** a fingerprint of those inputs is taken on each listing request. The generation is the volume manager's plus the number of times the fingerprint changed, so it only ever grows.
  - **Not covered:** `allocated_bytes` (it moves with every write).
  - **Also fixed:** a volume attached to a host's own NVMe subsystem (#210) now shows as `in_use`, with its attachment. The listing missed per-host subsystems.
- **feat:** #189: the handover record carries the incumbent's engine version.
  - **Recorded:** `boot-local` writes `engine_version` into `handover.json`. Each `adopt-ublk` writes its own once it serves, so the next handover compares with the real incumbent.
  - **Compared:** `adopt-ublk` checks it before anything is stood down. The same version is said on the console. Another version, or none (an engine older than this), is a `WARNING:` naming both.
  - **Refused when asked:** `--version-mismatch refuse` (`STORMBLOCK_ADOPT_VERSION_MISMATCH`) refuses instead, and the incumbent serves on. 11.45 shipped a v19.1.4 engine over a v16.1.0 initramfs.
  - **Not the commit:** the record compares versions only, which is #341's to extend.
- **feat:** #213: an `nvme-tcp://` drive takes a DH-HMAC-CHAP secret beside its path, never in it.
  - **How it is given:** `POST /api/v1/drives {path, dhchap_secret}` over HTTP; `[[drives]] dhchap_secret` or `dhchap_secret_file` in config.
  - **Where it lives:** the secret goes to the initiator and stays with the open drive, which reconnects with it. It is never echoed (responses, listings, errors), its `Debug` is redacted, it is never serialized, and it is never written to disk.
  - **Reporting:** `GET /api/v1/drives` shows `dhchap: true`.
  - **Refusals:** a secret for anything but `nvme-tcp://` is a 400 over HTTP and an error at startup; so is giving both `dhchap_secret` and `dhchap_secret_file`.
  - **Tests:** `integration_nvme_hosts::a_drive_whose_host_has_a_secret_is_opened_with_it` covers the drive opened with its secret, data through it, no secret / a wrong / a malformed one refused, and nothing echoed. `a_drive_secret_is_read_and_never_shown` covers the config side.
- **feat:** #229: the initramfs applies the node's declared static boot-NIC address instead of `ip=dhcp`.
  - **Where it reads it:** before the network, `/init` finds the local `stormcos-state` volume: on the drive `rd.stormblock.slab=` names, else on the first internal, non-removable disk (never a shelf drive, #273). It copies `/config/stormcos.toml` and `/config/install-node.toml` with `slab cat`.
  - **What it applies:** `[network]` is read as stormpump's `plan_for` reads it. A `static` declaration on an exact, present port with carrier puts its addresses, gateway, MTU, DNS and domain on `stormbr0`, and no DHCP is sent. A `dhcp` declaration on a named port makes that port the first one tried. Anything else (a missing port, no carrier, a pattern, no prefix) is reported and the boot falls back to DHCP.
  - **Command line:** a static `ip=` still wins, and its `<device>` field is now honoured. A dotted mask becomes a prefix. `ip=off`/`none` no longer reads as an address. `rd.stormblock.declared-net=off` ignores the declaration.
  - **Name hint (#238):** the DHCP name hint reads the same copies, so a netbooting node's declared name is now found too, and so is `install-node.toml`'s.
  - **Tests:** `tests/initramfs-boot-nic.sh` covers both blocks. `ci-boot-nic-verify.sh` boots them in QEMU.
- **docs:** #254 (stormcos#65's pass): stale pointers corrected.
  - **Block size:** release disks are composed at 4096 with a `-S 4096` FAT16 ESP (owner, #233), not at 512. The old firmware failures were a FAT32-labelled ESP. The per-volume 512 LBA stays as a supported option (#248). Updated in the README and `docs/composed-disks.md`.
  - **Golden:** it is no longer "held since v17". Releases since 11.55 ship it, and only forge runs an old engine (CLAUDE.md, `docs/presentation.md`).
  - **Composed disks:** releases are composed by stormcentral; stormcos's `compose-release.py` only makes a base. Release disks boot over NVMe/TCP, so the "not done" line about that is gone.
  - **Other pointers:** `sbregistry image` is what calls the engine (`docs/layering.md`). Marking a boot successful is stormcos's `boot-ok.sh` (`docs/images.md`). The test container's missing token is stormcentral#133, not stormcos#89 (README, `test/`).
- **fix:** #240: the slot fence covers parity stripes and StormFS chunk frees.
  - **Parity:** a parity volume's write (read-modify-write), discard, stripe verify and resync now hold every member and parity leg of the stripe shared, after the stripe lock, for the whole operation (`fenced_stripe`; the map is re-read until it names the same slots). A drain's parity-leg move (`migrate_parity_leg_at`, which only tries the fence) finds them busy instead of copying a parity leg and letting it go while a write folds its delta into it. Parity reads fence the stripe too, and let it go before taking the stripe lock to reconstruct.
  - **StormFS:** `chunk::free` looks its extents up and unmaps them under one map write lock. Before, a move that published between the read-locked lookup and the unmap had its new slot leaked and the slot it replaced freed a second time. `versioned::commit` already did everything under the write locks, and neither does device I/O on a mapped slot, so neither needs the fence.
  - **Tests:** `a_parity_leg_is_not_moved_under_a_stripe_write` (a write stopped inside the device while the parity leg is moved: Busy, then moved afterwards, with parity right and member 0 rebuilt from it). `FENCE_OFF_240=1` shows the loss.
- **feat:** #253: the initramfs reads stormbootx's `StormBootClock` EFI variable.
  - **`synced:<server>`:** logged as `clock: firmware synced from <server>`, and the NTP step is skipped (up to `STORM_NTP_WAIT` s saved per try). It runs anyway if the clock still reads before the image's build date, or with `rd.stormblock.ntp=always`.
  - **`unsynced`:** logged as `clock: firmware did not sync`, and the step runs as before.
  - **Absent:** behaves as before.
  - **`/run/stormblock/clock`:** one line saying how the clock was set (`firmware <server>`, `ntp <server>`, `build-date`, `unset`), for the node.
  - **Tests:** 7 cases added to `tests/initramfs-clock.sh` (synced, unsynced, absent, stale synced clock, always, a malformed value, the floor recorded).
- **fix:** #231: a ublk attach no longer blocks an async worker, or the export table, while its device comes up.
  - **Before:** `UblkExportManager::ensure` polled with `std::thread::sleep`, up to 1 s for the kernel's id and then up to 5 s for the block device, on a runtime worker and holding `ublk_exports`. Every other attach, and every request that reads the export table (volume listings, health, usage), waited behind it.
  - **Now:** `ublk_export::attach` begins under the lock: it answers an existing export, waits for an attach of the same volume already under way, or starts a server and records it pending. It then waits asynchronously (`wait_ready`, same bounds and the same checks: an id assigned, the node present, nothing mounted on it) on a task of its own with no lock held, and finishes under the lock: the export is recorded, or its server is taken down. A caller that gives up leaves nothing half made.
  - **Tests:** `waiting_for_a_device_never_blocks_the_runtime`, `a_device_that_does_not_come_up_is_refused_in_time`, `the_export_table_is_free_while_a_device_comes_up`. `ci-ublk-qd-verify.sh` adds four volumes attached at once on a real kernel.
- **feat:** #282: the flow-over paces itself to what its destination disk sustains, and shingled disks are named.
  - **Pacing:** after each window it reads the destination disk's flush times over the last 30 s (`drive::flushgate::recent`, p90; a slab on a partition flushes its disk). Above `STORMBLOCK_FLOW_FLUSH_BOUND_MS` (1000; 0 = off) it halves the window and pauses for one flush time (at most 10 s); under it the window doubles back. Said once at WARN when it starts and at INFO when the disk keeps up again. `paced` is added to the flow-over's time breakdown. A disk that keeps up runs at #331's full speed.
  - **Shingled disks:** a block device whose disk reports `queue/zoned` host-managed or host-aware, or whose model is on Seagate's, WD's or Toshiba's published drive-managed SMR lists (server3's ST2000DM008 among them), is logged at WARN when opened and listed on `/debug/stalls`.
  - **Test fix:** the power-cut flow-over test now cuts by progress (extents left), not by window count, which could land past the last window (flaky at 95%).
  - **Tests:** `the_flow_over_slows_to_a_disk_whose_flushes_take_seconds` (the rule), `a_flow_over_paces_itself_to_its_destinations_flush_time` (300 ms flushes against a 100 ms bound: the window shrinks to one and pauses after each, everything moved), `identity::tests::a_shingled_disk_is_named_from_sysfs_or_its_model`.
- **fix:** #283: `/debug` stays open, but an open caller sees counts and timings, not other callers' work.
  - **Without the node or admin token** (or a node-CA client certificate), `/debug/stalls` shows each request in flight as its method, age and route family (`/api/v1/volumes/…`), never a volume id, name or boothost tag. A remote slab's flushes are named by transport, not by the URI attaching it takes. The watchdog's reports come as their open summary, and `/debug/threads` has no kernel stacks.
  - **With a token,** the full view is unchanged. The auth layer marks such a request (`mgmt::auth::FullView`); on a node that enforces no token, every caller has the full view.
  - **`/debug/tasks`:** takes one task dump at a time and answers from it for 5 s (`TASKS_FRESH`), so a loop of requests cannot pause the runtime over and over.
  - **Tests:** `debug::view_tests` (route families, remote devices) and `integration_auth::debug_is_open_but_an_open_caller_sees_no_paths_and_cannot_force_dumps` (a held request's id absent from the open view and present in the token view; 12 concurrent `/debug/tasks` calls make one dump).
- **feat:** #337: health names a ublk request that was never answered.
  - **Why:** on server3 (11.91), fastetcd's fdatasync did not return for 70 minutes while health said ok, and the engine could not say whether a FLUSH on its device was outstanding. The likely cause is #334, fixed in golden-stormblock-65b6578787be: under the same suite on the Dell, the stall watchdog's task dump panicked a ublk device's runtime and lost a wake. That device's requests then never completed.
  - **Now:** each ublk queue records when each tag's request was taken, and its op. `/api/v1/health` lists every request unanswered for 30 s or more (`ublk_stuck`: device, `/dev/ublkbN`, queue, tag, op, seconds), and the watchdog logs it every 30 s.
  - **Tests:** `ublk::tests::an_unanswered_request_is_listed_until_it_is_answered`, `integration_auth::health_names_a_ublk_request_that_was_never_answered`.

### 2026-10-07
- **perf:** #331: an attached NVMe/TCP namespace has several I/O connections (`STORMBLOCK_NVME_TCP_QUEUES`, 4), not one.
  - **Problem:** a flow-over's parallel 1 MiB copies (eight 128 KiB round trips each) and the node's own reads of extents still on the appliance queued on a single connection.
  - **Now:** each operation takes a free connection. The first connection is made at open, the rest when first needed. A partial-block read-modify-write holds a device lock exclusively, so no write on another connection lands inside it.
  - **Tests:** `integration_nvmeof::parallel_io_over_several_connections_stays_exact` (256 concurrent 64-byte entries in 4 blocks; 16 parallel 1 MiB copies).
- **perf/fix:** #331: the slot fence is one lock per slot in use, not 4096 hashed shards.
  - **Problem:** with the shards, two unrelated slots waited for each other. With the flow-over's 8 moves at once, the model's in-process appliance served a move's NVMe/TCP read from a slot in the shard the move held, so the move waited on itself and the flow-over hung (seen once, after 256 moves).
  - **Now:** a lock is made when a slot is first held and dropped with its last guard, in 64 tables.
  - **Tests:** `fence::tests::two_slots_never_wait_for_each_other_and_nothing_is_kept`. The two flow-over cut tests cut between windows of 4.
- **perf:** #331: the flow-over moves extents in windows.
  - **Before:** one extent at a time, a full persist (every slab flushed, local and remote) after each, and a sleep as long as the whole previous move whenever any volume I/O had run.
  - **Now:** windows of up to 64 extents (`STORMBLOCK_FLOW_BATCH`), with a slot shared by a golden and its clones moved once. Up to 8 move at once (`STORMBLOCK_FLOW_PARALLEL`), each holding only its slot's fence for the copy. Then one persist, then the release of their source slots. A source slot is still freed only after the map naming its copy is durable (durability rule 10).
  - **Yield:** a quarter of a window's time, at most 2 s, when foreground I/O ran.
  - **Timing:** the flow-over now says every 100 moves and at the end where its time went (`flow-over: N moved in Ts (X/h): yield, fence, copy, persist, release`).
  - **Model:** `flow_over_rate_model` (ignored) runs a netbooted install over NVMe/TCP.
  - **Note:** the hours on pvetest were the pve host's shared QLC NVMe (owner); server3 on its own disk flowed over in 36 s.
- **fix:** #279: `FileDevice::write` returned before its bytes were in the file. It is `seek` plus `write_all` on a `tokio::fs::File`, which hands the write to the blocking pool and returns. That `File` orders its own later operations after the write, but a second `FileDevice` on the same path could read the file before the write landed.
  - **Symptom:** the flaky `pressure::tests::an_existing_slab_on_a_source_is_adopted_with_its_data`, where the watcher reopens a slab another `FileDevice` had just formatted, and the full suite's load keeps the blocking pool busy.
  - **Fix:** `write` now flushes the tokio `File` (not an fsync), so a returned write is in the file.
  - **Tests:** `filedev::tests::a_write_is_in_the_file_when_it_returns` (the blocking pool kept busy, the file read directly after each write). The pressure test now prints what `check()` decided.
- **test:** #191: the power-cut simulation tears writes and cuts inside operations.
  - **Tearing:** `CrashDevice::crash_with(seed, keep, Tear)` tears the writes it keeps. `Tear::Prefix` keeps the first n units of a write (a drive writing in order) and `Tear::Scatter` any subset (out of order). It tears at the drive's atomic unit: 4096 bytes, or 512 with `with_atomic_unit(512)`, where one 4 KiB slot-table page or record block can land in part.
  - **Cut inside an operation:** `CrashDevice::cut_at(n)` takes the power as the nth write arrives, so a persist or a slot-table sync is caught part-way.
  - **Tests:** the 300-cut power-cut test runs under none, prefix, scatter and 512-byte sectors, in both formats. In format v2 most cuts land inside an operation. Each run says how many cuts and torn writes it made, and fails if it tore nothing or cut too few operations. Format v2: 0 lost in every variant.
  - **Found:** format v1 loses a clone's unwritten extent to a cut inside a persist (9 of 300, tearing or not), filed as **#340**. Its runs cut between operations until then, and `v1_survives_a_power_cut_inside_a_persist` (`#[ignore]`) reproduces it.
- **fix:** #198: an imported image that was not cleanly unmounted no longer passes verification. `fio-xfs` never read the XFS log, and the import ignored ext4's RECOVER flag, so an image taken from a running or crashed system walked clean and was sealed, though Linux replays it on first mount and the guest sees something other than what was surveyed.
  - **fio-xfs v0.3.0** (fio.xfs.rs#6) reads the log.
  - **Survey:** each filesystem in the import's survey has a `log` field: `clean`; for XFS `dirty`, `external` or `unreadable`; for ext4 `needs_recovery`. Anything else fails verification unless `"verify": false`.
    - An external XFS log is not verifiable (fio.xfs.rs#17).
    - A dirty XFS log's error notes that a clean one is on rare occasions read as dirty (fio.xfs.rs#16).
  - **Seal:** sealing an XFS volume is also refused while its log is not clean.
  - **Tests:** `fs::survey::tests`:
    - an engine-made XFS reads clean and seals;
    - with its log made dirty by hand (records after the unmount record, as fio-xfs's own fixture), it reads `dirty`, fails the import's verdict (passes with `verify: false`) and is refused a seal;
    - an ext4 with RECOVER reads `needs_recovery` and fails the verdict.
- **perf:** #338 (stormpump#107, rustkube-node#95): a flush with nothing to make durable returns at once. A fresh 64Mi claim spent 553–878 ms in its ext4 mount on a warm node, because every ublk FLUSH ran a device-wide sync of each slab the volume touches, queued behind any other volume's sync.
  - **How:** each volume counts the writes, discards and write-zeroes that have finished on it (when each returns, failed or dropped included), and a successful flush records the count it started from. A flush with nothing finished since touches no device. Durability rule 13.
  - **What it changes:** a write still in flight is not owed by a running flush, and makes the next flush a full one. A handle's first flush is always full.
  - **Caveat:** an rw ext4 mount writes its superblock, so the flush after that write still pays; the clean ones do not.
  - **Tests:** `thin::tests`:
    - a clean flush reaches no device (after reads too), and a write, write-zeroes or discard makes the next one sync;
    - a clean volume's flush returns in under 100 ms while another volume on the slab holds a 600 ms device flush;
    - a write that lands during a flush makes the next flush full.
- **fix:** #334 (P0, Dell 11.91 under rustkube-node's `medium` suite): the API stall watchdog's task dump panicked a ublk device's runtime. `ublk-adopt-36` panicked with `RefCell already borrowed` (tokio `current_thread/mod.rs:723`), its I/O hung, and :9090 stopped answering.
  - **Cause:** requests stalled over 10 s, so the watchdog (#269) ran `task_dump`, which spawned `dump()` on every registered runtime, including each adopted or exported ublk device's current-thread runtime. On a current-thread runtime, `dump()` holds the core while it polls each task in trace mode. Our I/O futures are not tokio's, so tracing runs them, and a task that finishes and releases a tokio lock wakes another task on the same runtime, so `schedule()` borrows the core again and panics. The API's multi-thread runtime traces with the core taken out and is not affected.
  - **Fix:** `task_dump` never dumps a current-thread runtime. It names it and says why; its threads are still in `/debug/threads`.
  - **What it broke:** tokio catches the panic in the task, so the runtime thread lives on. But the I/O task that was traced is cut short in the middle of its lock release, and the wake for the waiting task is lost. The I/O never completes, and anything waiting on that lock waits forever. That was the Dell's hung API.
  - **Test:** `debug::tests::a_task_dump_leaves_a_current_thread_runtime_running` reproduces it. An I/O task holds a tokio Mutex that an older task waits on, its I/O completes underneath, and the dump comes first. Without the fix it fails every time with `RefCell already borrowed` at `current_thread/mod.rs:723:40` (the Dell's line), and neither the I/O nor its waiter ever proceeds. With the fix both go on.
- **feat:** #322 (stormcentral#353): the open `/api/v1/health` says where the node runs from. C2NR0Q2 had passed the "local boot" and "fresh slab" stages while running entirely from a forge clone (#268).
  - **`slabs`:** `diskless`; `system` and `data`, each `local`, `remote`, `mixed` or `none` (by where the volumes' legs are); and each slab with its role, source, local `device` or remote `transport`, and the volumes with legs on it.
  - **Never the remote URI:** health is unauthenticated, and the URI (forge's address, the clone's subsystem NQN, the host NQN, with no secret for a boot host, #210) is what attaching the machine's boot clone takes.
  - **Cost:** never waits (try_read, cached 10 s). On a node with no remote slab (forge, a disk-booted node), nothing is counted.
  - **Tests:** `slab_report` units (diskless, mid-flow-over mixed, all local); the #314 netboot test's node over NVMe/TCP reports `diskless`/`remote` and its health names no URI, NQN or address; a local node's health reports `local` (`integration_auth`).
- **feat:** #313: `?scrub=used` on `DELETE /api/v1/volumes/{id}` and `/serve/v1/volumes/{id}`, the owner's rule for removing data ("overwrite only what's used, once"), with a report. Since #286 a delete already overwrites once every slot whose last reference goes with it (copy-on-write aware), discards it on flash, then frees it. What was missing was the word and the answer.
  - **The word:** `scrub=used` means at least `once`, even on a node whose `[erase] default` is `none`.
  - **The answer:** the delete returns **200** `{deleted, scrub: {volume, level, slots, bytes}}`, counting the slots this delete queued (counted when they are retired, under the registry lock). `/serve/v1` adds `scrub` to its body always. Completion is the volume's record in `GET /api/v1/erasures`. Without `scrub` the delete answers 204 as before.
  - `/serve/v1`'s delete now also wakes the eraser.
  - **Test:** `integration_erase::scrub_used_overwrites_what_the_last_holder_frees_and_reports_it` (a clone scrubs its own copy-on-write slot only, the golden's delete then scrubs the shared ones, the bytes are gone from the device, raised from a `none` default, 400 for an unknown scrub, 204 without it).
- **docs:** removed references to the CoreOS trademark (owner); the Ignition interface name `opt/com.coreos/config` stays where Ignition requires it
- **fix:** #308 (stormcos#92): a flush that reaches a mirror's failed drive degrades that leg instead of failing the volume. On the 160-drive shelf, a `mirror:2` volume's flush right after one of its drives was failed returned EIO (`flushing the filesystem: device I/O failed`), and health still said `healthy`. `flush` returned the first slab's sync error for every volume. The write path already marks a failing leg's slab failed and goes on, but a flush that is the first I/O to reach a dead drive did not: on 160 drives the writes before it usually land elsewhere (2 of 4 shelf runs), and on 8 drives they hit it first (4 of 4 passed).
  - **Fix:** on a redundant volume, a media error from a slab's sync now puts that slab in the volume's failed set (the volume is `degraded`) and the flush completes on the other legs. It fails only when something is no longer readable. An unreplicated volume's flush error is still the volume's.
  - **Test:** `thin::redundancy_tests::a_flush_to_a_failed_leg_degrades_the_mirror_not_the_volume` (emulated drives, as stormcos#92 runs them).
- **feat:** #303 (stormcos#300): the engine handover says where its time goes. A unit's first I/O waits for `Adopted N device(s)`: 5.4 s for stormcert-init on the Dell and 5.7 s on pve, and on a reboot from the Dell's HDD the incumbent let go about 21 s after `adopting 64 volume(s)`.
  - **Timing lines:** `[adopt +T s] … (Δ s)` for every step (stand-down asked, devices quiesced, incumbent exited, each slab path attached and its slabs opened with their slot count, records read, restored, devices live again). The incumbent's stop prints `[boot-local stop +T s]` (devices released, metadata persisted).
  - **Handover state:** `/run/stormblock/handover-state.json` is `adopting` from just before the stand-down and `serving` (with `took_ms`) once the devices are live, so a supervisor can hold units instead of letting them block in D state (stormpump).
  - **Not done:** reading the slabs before the incumbent lets go (the issue's ask 2) is the order the owner ruled out on #171 (rule 8), so it is put to the owner on #303.
  - **Test:** `ci-adopt-retry-verify.sh` `timing` step (QEMU, real ublk).
- **perf:** #302 (stormcos#300): the initramfs mounts the container volumes in parallel. They were mounted one at a time at ~130–200 ms each, which cost 8.6–10.5 s of every boot for 63 volumes on the Dell.
  - **Order:** in waves by mount-point depth, so a mount point inside another is never mounted before its parent. At most `STORM_MOUNT_PARALLEL` (16) run at once.
  - **Devices:** waited for once for the whole list (`STORM_MOUNT_WAIT`, 15 s), not up to 15 s per entry.
  - **Type:** `-t ext4` first (a bare `mount` probed erofs and others first), then a probing mount if that fails, so XFS volumes still mount.
  - **Output:** the same `mounted:` and `WARNING:` lines, plus one line with the count and the time.
  - **Tests:** `tests/initramfs-container-mounts.sh`, with stubbed mount and devices. Covered: all mounted, in parallel, bounded, nested order, XFS fallback, a bad volume warned, missing devices waited for once, a late device mounted.
- **fix:** #144: `adopt-ublk` (every node's engine) stops in order on SIGTERM. It waited for SIGINT only, so the SIGTERM a supervisor sends (stormpump on shutdown), and the one the next `adopt-ublk` sends to stand it down, killed it by the default action. It skipped the final state capture, losing up to 10 s of engine state on every stop, and never tore its ublk threads down.
  - **On SIGINT or SIGTERM:** the devices are released for recovery at once (never stopped: they outlive the process and the next adopter takes them). The final state capture and a metadata persist run side by side, each bounded to 10 s, so a stop stays within ~10 s.
  - **Same fix elsewhere:** `boot-iscsi` and the volume attach command waited for SIGINT only too; they now use the same `StopSignal`.
  - **Tests:** `ci-adopt-retry-verify.sh` gains two steps on a real kernel's ublk:
    - an adopter stood down by the next one exits 0 after "adopt: SIGTERM", and the next one serves the same bytes;
    - a file written to the engine's state dir just before SIGTERM is in the next adopter's restored state dir, which only the final capture can have done.
- **fix:** #232: `/v1` no longer reports a plaintext volume as encrypted. `POST /v1/volumes {"encrypted": true}` was stored and returned `encrypted: true`, and nothing in the engine encrypts. A StorageClass asking for encryption at rest was told it had it.
  - **Refused:** `encrypted: true` now answers **422** `{code: "unsupported"}` with a message naming #74, and nothing is created. The check comes before the name-idempotency check, so an existing plaintext volume of that name is never handed back. stormblock-csi's client maps 422 to InvalidArgument, so `CreateVolume` fails with that message.
  - **Reported false:** every volume reports `encrypted: false`. One an earlier engine recorded as encrypted reads back false, with a warning naming it, and the next persist writes that back.
  - **Tests:** `integration_v1_api::v1_encrypted_true_is_refused_and_creates_nothing` and `v1_a_volume_recorded_encrypted_reads_back_false`.
- **feat:** #203: a node-CA client certificate is a credential on the management API. `tls_cert`/`tls_key` already served :9090 over HTTPS; what was missing is the owner's other half (stormcos#81): clients authenticating with a certificate instead of sending a token.
  - **`[management] tls_client_ca`:** the HTTPS listener asks for a client certificate and verifies it against the node CA. It is asked for, not required, so token callers and probes still connect. A certificate from the node CA counts at the node token's tier: ordinary verbs and attestation reads. A destructive verb still needs the admin token or a reviewed Kubernetes bearer (#274). Such calls are audited as `client-cert:sha256:<16 hex>`. A certificate from another CA is refused in the handshake.
  - **Renewal:** the pair and the CA are re-read when their files change (checked at most every 5 s, as connections arrive). A set that does not load is logged and the previous one kept (`mgmt::tls::Reloader`).
  - **Config check:** `tls_client_ca` without `tls_cert` is refused at validation.
  - **Tests:** `integration_mgmt_tls` (3), through the engine's listener with certificates made by the openssl CLI (a node CA, the serving pair, a client pair, another CA's client pair). Covered: HTTPS only; health with no credential; the token without a certificate; a node-CA certificate reads and creates but gets 401 on a destructive verb; another CA's certificate gets no connection; a renewed pair served without a restart; a broken renewal leaves the old pair.
- **fix:** #190: `adopt-ublk` no longer leaves a node hung when its restore fails after the incumbent has exited. Since #171 the restore runs after the stand-down, and a failure there left every ublk device, root included, with no server, and nothing retried or reported it.
  - **Checked before the stand-down:** every local path the restore reads (`--meta`, a slab file or device) must be on memory or a disk that is not ublk (`drive::backing`, from `/proc/self/mountinfo` and sysfs). An overlay is treated as ublk. Such a read in the gap does not fail; it waits for the server doing the reading. A refusal leaves the incumbent serving. An `nvme-tcp://` host name is resolved there too, because the resolver reads `/etc`.
  - **No relative meta dir:** with no `--meta`, a fabric URI no longer gets a "meta beside the slab" directory. That path was relative and was created in the cwd, the root.
  - **Retried:** a restore that fails after the stand-down is tried again with backoff (`handover::take_over_retrying`: 1 s doubling to 15 s, for `STORMBLOCK_ADOPT_RESTORE_SECS`, default 120). Each failure is printed on the console.
  - **Then loud:** giving up, or adopting no device at all, writes `/run/stormblock/adopt-failed.json` (error, devices held, what to do next), prints a FATAL on the console and exits **75**. The devices stay held, so reads wait rather than fail, and the next `adopt-ublk` takes them and removes the record. The old message "the root is still served by whoever had it" was false since #171 and is gone.
  - **Tests:** `handover::retry_tests` (3) and `backing::tests` (2). `ci-adopt-retry-verify.sh` runs a real kernel's ublk in QEMU with a volume served by `boot-local`:
    - two failed restores (test hook `STORMBLOCK_ADOPT_TEST_FAIL_RESTORES`), then the device is taken, and a read started in the gap gets the right bytes;
    - every restore failing gives exit 75, the record, and the device held with a read waiting;
    - a second `adopt-ublk` takes the device, and the waiting read completes with the right bytes;
    - a slab on `/dev/ublkb0` is refused before the stand-down while the server keeps serving.
- **fix:** #314 (sectionsystems#7): a network-booted node could not verify its boot pallet. `/api/v1/pallets` was built from the engine's drives, and `adopt-ublk` (every node's engine) registers none, so the claimed clone's GPT, which carries the boot pallet, was in no store.
  - **Boot disks:** the disk each slab path was opened from (the claimed clone's `nvme-tcp://` namespace, or the local disk) is kept as `AppState.boot_disks`.
  - **Reads only:** list, `status`, `chain`, `GET /{id}` and `POST /{id}/verify` see the drives and the boot disks. Every write verb sees the drives only, so a pallet on a shared clone is never activated, marked, moved or deleted (404).
  - **Test:** `forge_mode_tests::a_netbooted_node_lists_and_verifies_the_boot_pallet_it_booted`: an appliance serves a release disk (slabs plus a boot pallet) over NVMe/TCP, the node claims it and opens its slabs as `adopt-ublk` does, and its API returns the pallet with member digests and `verify` gives `ok: true`. `activate`, `successful` and `DELETE` answer 404.
- **fix:** #174 (found by stormblock-registry's long test): concurrent export persists raced on one `exports.tmp`. `POST /serve/v1/exports` and `/volumes` answered 500 (`rename exports.tmp -> exports.json: No such file or directory`), could leave the table with another writer's bytes, and leaked volumes.
  - **Unique temporary files:** `serve::wiring::write_atomic` writes a file of its own (`<file>.<pid>.<seq>.tmp`), removed on failure. The engine's own `exports.json` now goes through it too.
  - **One persist at a time:** both export persists run one at a time, with the table read under the lock, so an older snapshot never lands on a newer one.
  - **No half-made exports:** a failed export is undone (its wiring row and export entry removed), so a retry does not make a second one.
  - **No leaked volumes:** `POST /serve/v1/volumes {export: true}` deletes the volume it made when the export fails.
  - **Tests:** `integration_serve_mounted::concurrent_exports_all_persist` (16 at once: all 201, all 16 on disk, no `.tmp` left) and `a_volume_whose_export_fails_is_not_left_behind`.
- **fix:** #205 (found by stormuefi's installed-node test): `local-boot`'s ladder dropped the proven boot pallet and re-armed a failed one.
  - **Ranking:** the pallets a disk already carries are now ranked by boot state first: proven (`successful`), then candidate (tries left), then exhausted (no tries, never proven). After that by `(priority, version)`. A failed pallet that keeps priority 14 no longer outranks the proven one at 13.
  - **Eviction:** `evictable` drops exhausted pallets first and never the last proven one.
  - **Tries:** they are reset only on a pallet copied by this run, so the loader's count, the one record that a release failed, is kept.
  - **Staged releases:** the "pallet the disk boots now", which a staged release goes below (#122), is the top pallet stormuefi does not skip.
  - **Tests:** `local_boot::tests::the_proven_pallet_stays_and_the_failed_one_goes` (the issue's A/B/C scenario: C on top, A proven below it, B gone) and `an_exhausted_pallet_is_not_rearmed`.
- **docs:** #234 closed, superseded: there is no `upgrade` boot intent. The owner ruled on 2026-10-02 that a release change is not decided at boot; the update path is staging on the running node (#122, stormupdate#3). Since #311, an `install` keeps the data half. The descriptions of `install` as "takes the disk with force" are corrected in `docs/auth.md`, `docs/boot-hooks.md`, the README and `BootIntent::Install`.
- **feat:** #204 (stormbootx#23): a boothost claim of a name no host has, carrying the machine's `mac` (and `serial`), reaches the host the machine already is instead of making a new one.
  - **Finding the host:** by its MAC, else by the serial when an operator made it one of that host's aliases. An assignment named by a serial never counts, since blades share chassis serials.
  - **A provisional `mac-<hex>` host** is renamed to the claimed name, its old name kept as an alias, golden and history moved.
  - **A named host** keeps its name, and the claimed name becomes an alias.
  - **Neither:** a new host pinned to the default, with its MAC (never the serial) as its alias.
  - **Reply:** `host.resolved` says which (`SynonymStore::resolve_named_claim`, `NamedClaim`).
  - **Tests:** `synonym::tests::a_named_claim_reaches_the_machine_it_already_is`, `integration_synonyms::a_named_claim_with_a_known_mac_reaches_that_host`.
- **feat:** #243: on a node the console gets the engine's warnings, not its INFO.
  - **Where:** `boot-local`, `adopt-ublk` and `boot-iscsi`. Their stderr is every console (`/init` follows it; stormpump echoes it), and at INFO a shutdown printed 2–3 lines per volume across ~55 volumes.
  - **The record:** the whole log goes to `/run/stormblock/stormblock.log` (`STORMBLOCK_LOG_FILE`) at `RUST_LOG`, else `stormblock.log=` on the kernel line, else `info`. `/run` moves into the real root, so it is one file for the boot.
  - **The console:** WARN and above, raised with `stormblock.console_log=` on the kernel line or `STORMBLOCK_CONSOLE_LOG`. The stage lines are `println!` and always shown. Other commands log as before.
  - **Refused requests:** `unauthorized …` is warned once a minute, the rest counted and said with the next one (`serve::api::RefusalLimiter`). That was stormstorage polling without a token, every 15 s on every console.
  - **`/init`:** its engine report after a FATAL names the record.
  - **Tests:** `logging::tests` (5), `refusals_are_said_once_a_minute_then_counted`, `tests/initramfs-no-appliance.sh` +2. Checked on dev with the built binary: `boot-local` against a missing slab printed only its error on stderr, with INFO in the record. `STORMBLOCK_CONSOLE_LOG=info` put INFO back on stderr. `slab list` wrote no record and logged to stderr as before.
- **docs:** #265: status. #122's stage/activate/rollback tests were re-run on dev at b30cecf: 4/4. The check on real hardware waits for stormupdate#3 and a stormcos release that carries the #122 engine.
- **feat:** #244: a local root that does not come up falls back to the claimed image instead of stopping at a shell. The root may never appear, or may fail to mount (server1 11.56: `erofs: cannot find valid erofs superblock`).
  - **When:** once per boot; when the root came from a local disk (no hook decided it); when an appliance is known; and when the name is not a guess (#249).
  - **What it does:** `/init` stops the engine and boots the claimed image (claiming one if needed). It installs that image over the disk, its system half laid again (`STORMBLOCK_RELAY_SYSTEM_HALF=1` bypasses the "already up to date" shortcut) and its data half kept (#311). The console says `ROOT FAILED` / `FALLING BACK`, or why it did not fall back.
  - **Code:** the post-probe launch is now `launch_local`, the root wait `wait_root`, and the new block is `root fallback`.
  - **Tests:** `tests/initramfs-root-fallback.sh`; `cli::install_tests::a_held_disk_whose_root_fails_is_laid_again_keeping_its_data`; `cli::relay_tests`.
- **fix (data):** #244/#311: an install of the release a disk already holds lost the node's data in every data volume the release ships. That covers an install ticket over the same release, and now #244's fallback.
  - **Cause:** the claim's copy carries the node volume's id, and `adopt_slabs` kept the record it already knew, the claim's. The node's bytes were unmapped, to be freed by GC.
  - **Fix:** `install::adopt` drops the claim's copy of a same-id unsealed volume before adopting, so the node's record and bytes are what stay, whatever the policy. Same-id goldens are one volume.
  - **Test:** the held-disk test failed on `state`'s bytes before the fix.
- **docs:** #284: forge mode is a day-2 switch. `PUT/DELETE /api/v1/forge` is called by stormcluster's day-2 operation (stormcluster#16); nothing chooses it at install, and stormcos#82's install-config carries no forge role. CLAUDE.md's #272 entry said otherwise and is corrected, and the README's `/api/v1/forge` row now says who calls it. `docs/auth.md` and `docs/boot-hooks.md` never made the claim.
- **perf:** #278: a successor's flow-over lets the node boot first. The first move waits until the node's volume I/O has been still for 10 s, or 90 s at most (`STORMBLOCK_FLOW_BOOT_GRACE_SECS`, 0 = no wait). `flow_over_remaining` is reported during the wait.
  - **Why:** on the Dell (SMR disk), a reboot during the flow-over ran stormpump in 15.1 s instead of 7.9 and the apiserver in 30.2 s instead of 15.3 (11.88). The per-move yield (#269) gives back one move's time, not a boot.
  - **The first-boot regression this issue was opened for is gone since 11.82** (the reopened-#269 work). pvetest1 reaches the apiserver in 63–109 s against 223 s on 11.73 and 334 s on 11.78; the Dell is at 165–179 s against 325 s on 11.79.
- **docs:** #257: #239's fix checked against stormcentral's 11.88 install runs. On pvetest1 and pvetest2 (a fresh install, a power cut mid flow-over, a resume from a fresh clone) the serial logs have no ext4 errors. Not checked: the e2fsck of the service clones after the flow-over, which needs stormcentral#503. The work plan records it.

### 2026-10-06
- **feat:** #83: moving a VM disk between nodes, on the engine's side (`docs/migration.md`).
  - **NVMe ANA:** Identify reports ANA (CMIC, OAES bit 11, ANATT, ANACAP, five groups by state, NN = MNAN = 1024; NMIC shared and ANAGRPID per namespace). Log page 0x0C is served, and a change sends an ANA change notice to every connected host of every subsystem that serves the volume. I/O on an `inaccessible`, `persistent_loss` or `change` path fails with the path status (SCT 3), which a multipath host fails over on.
  - **API:** `GET/PUT /api/v1/volumes/{id}/ana {state}` sets and reads the state. It is kept in `<data_dir>/ana.json`, written before it is applied.
  - **Controller IDs:** `[management] nvme_cntlid_range` gives a node's targets disjoint controller IDs, so a host can reach one subsystem through two nodes. `POST /api/v1/volumes` takes an `id`, so a volume served from two nodes carries one NGUID.
  - **Tests:** `integration_ana_epoch`; `ci-ana-verify.sh` has a QEMU guest kernel follow a move between two engines with native multipath.
- **feat (BREAKING for callers that leave it out after a fence):** #83 item 2 / #6, the contract set on stormstorage#33.
  - **Attach:** `/v1` attach takes `epoch`. An epoch other than the volume's, or no epoch once the volume has been fenced, gets `412 stale_epoch`. Attachments are recorded on the volume with their epoch (`attachments` on `GET /v1/volumes/{id}`).
  - **Fence:** `fence` takes away every attachment below the new epoch before it answers (`revoked`): the namespace leaves the host's subsystem (or the shared one), or the ublk device goes. An attach that raced a fence undoes itself and answers 412. A dual-attach `commit` keeps the target's attachment, and `abort` revokes it.
  - **Namespace removal** now returns only once nothing in flight can still land: a command waiting for its R2T data is refused, and one at the device is waited for.
- **fix:** #281 (rustkube-node#140): a `ready` fstemplate whose sealed volume is gone is no longer `ready`. Such a store had outlived its volume: a delete cut short, a reclaim, a store restored onto a slab seeded again.
  - **At startup** (`template::verify_ready`, after the formats a previous run left): a template whose volume is there but unsealed is sealed again. One whose volume is missing, or was never recorded, or will not seal, is marked `broken` (`FsTemplate.broken`, persisted).
  - **The listing** reports `state: broken` and the reason, and it looks for the volume each time.
  - **A clone or claim** of a broken template answers 409 `fstemplate <name> is broken: <why>. It is not sealed and cannot be cloned; delete it and mint it again`, instead of a 404 for a volume the caller never named or a 500. A template whose volume is back is cleared.
  - **Test:** `integration_fstemplates::a_ready_template_whose_volume_is_gone_lists_broken_and_refuses_clones_clearly`
- **fix:** #277: a fsync'd write to an extent the flow-over (or a drain or rebalance) had just moved could be lost if the power went before the next persist. The new slot was allocated at the source's generation while the map took one more (`rewrite_legs`), so restore saw two slots at one generation and kept the record's, the source. A moved primary is now allocated at `generation + 1` (`PlacementEngine::moved_generation`), and restore takes it. Mirror and parity legs keep their generation; their window is #316. Test: `integration_power_cut::a_write_to_an_extent_just_moved_survives_a_cut_before_the_persist` (both formats), which fails with `MOVE_SAME_GEN_277=1`
- **BREAKING (fix, P0):** #311 (owner, 2026-10-06: "How can you wipe a production node data?"): an install keeps the node's data half. It lays only the system half of the system drive again, superseding #261's install = wipe.
  - **Engine** (`image::install`, `take_local_disk`'s update path, force or not): `boot-local` takes the local disk before it resolves what it mounts. It adopts the data and bulk slabs' records, so every volume keeps its id and name.
  - **The release's policy** (`/etc/stormblock/data-volumes`, #122) settles each name it shares with the node. `keep`, the default, keeps the node's volume and deletes the claim's fresh clone. `replace` and `migrate` set the node's volume aside as `<name>@<old release>`, with migrations listed for stormupdate in the handover record and then in `release-generations.json` (`Migration.node`). A golden the release ships takes the name, and the node's is set aside.
  - **What the release adds** (stormcos#236's `kubelet-data`) moves onto the disk in the background.
  - **Refused before anything is written**, with the disk untouched and the node running from the appliance: unreadable records, a data volume with an extent in the system half, or an unsealed system-half volume the release does not bring back (#317).
  - **`/init`** never passes `--local-disk-force` for a drive that carries a data slab: not on an install the probe ruled, not on `slab holds` 1, not on the appliance's install intent, and not on `assimilate=force`. The console says `INSTALL: … laying its system half again …; its data half is kept`.
  - **Tests:** `cli::install_tests::an_install_over_a_node_keeps_every_byte_of_its_data_half` and `…an_install_that_would_lose_a_volume_stops_and_writes_nothing`; 11 `tests/initramfs-boot-hook.sh` cases now expect the data half kept
- **feat:** #288 (stormcos#208): optional mount entries. A `?<vol>:<path>` line in `/etc/stormblock/mounts` (or in `rd.stormblock.mount=`) is mounted when the slab has the volume and left out when it does not, with `optional, not in this release: <vol>` on the console. The decision is made before the ublk numbers are handed out, so devices and mount points stay in step. The local-disk probe never counts an optional entry as missing. The slab's volumes come from `slab volumes`, read once and only when the list has a `?`; a slab that cannot list them leaves its optional entries out and says so. Plain entries stay required. Tests: `tests/initramfs-mounts.sh` (+10), `tests/initramfs-no-appliance.sh` (+2)
- **chore:** test-fixture credentials marked `not a secret` (inline, or `.github/secret_scanning.yml` for files that cannot hold a comment) — owner
- **test:** #158 stage E at scale: `integration_metadata_v2::a_v2_node_on_petabyte_drives_keeps_its_volumes_across_restarts` (ignored, ~2.5 min). It runs a node on emulated 1 PiB and 256 TiB drives with metadata v2, one slab migrated from v1 at the first persist. It covers an 8 PiB thin volume with extent indexes past 2^32, a mirror across drives, a hundred goldens and a clone, evicted maps loaded back, and two restarts from the disks alone with every byte checked. Measured on dev: a 1 PiB v2 slab formats in 1.3 s, and a restart takes 39 s, almost all of it reading slot tables (#307). Docs: metadata-v2.md "As built (stage E)"
- **feat:** #122: stage the next release on a running node, activate it, roll it back. These are `POST/GET/DELETE /api/v1/releases/{v}/stage` (a job), `POST /api/v1/releases/{v}/activate`, `POST /api/v1/releases/rollback` and `GET /api/v1/releases/generations` (`<data_dir>/release-generations.json`); all but the GETs are destructive.
  - **Stage** reads the published image over HTTP `Range` (`drive::httpdev`, moved from `examples/slab_audit`). It copies the image into the node's slabs as `<name>@<v>`: goldens under the release's ids, sealed when whole; clones as copy-on-write clones of their parent; volumes already here shared.
  - **The release's policy** comes from `/etc/stormblock/data-volumes` in its root (`keep | replace | migrate <hook>`; owner's 1(b)); migrations are listed for stormupdate.
  - **The boot pallet** goes below the active one (`lay_local_boot_ranked`, `BootRank::BelowActive`).
  - **Activate** renames N aside (`<name>@<N>`), N+1 onto the plain names, and raises the pallet (`raise_local_boot`). Rollback does the reverse. One previous generation is kept.
  - **New building blocks:** `CreateOptions.id`, `VolumeManager::rename_volume`. Docs: `docs/staging.md`
- **fix:** #122/#265: `slab holds` counts a golden only when the local copy is sealed, so a stage cut short never reads as holding the release
- **fix (security):** #217 (P0): the `/serve/v1` reconciler served every export in the engine's table on a per-volume portal that admits any host. That included `/api/v1/exports {host_nqn}` exports bound to one host (#210) and shared-subsystem exports, and it rewrote their recorded NSID to 1. It now wires only the exports `/serve/v1` made. They are marked `ExportEntry.serve`; an older entry counts as serve's only if it has serve's `<nqn_prefix>:vol-<volume>` name and no host binding. A row an earlier engine made for another export drains, which closes its portal. At start, the engine's `restore_exports` also stops putting serve's NSID-1 exports on its shared subsystem (#98). Tests: `integration_serve_own_exports` (2), both failing with the old rule
- **test:** #172: `a_power_cut_anywhere_in_the_flow_over_keeps_every_acknowledged_write`. It installs onto an emulated disk with a volatile cache, writes a system volume, a data volume and a stamped service clone with logged fsyncs while both halves flow, and cuts at four points with a random half of the cache lost. It then resumes from a fresh claim and checks every acknowledged write and golden byte after the resume, after the flow-over finishes, and from the disk alone. It fails as #172/#239 did with `RELOCATE_OFF_239=1`
- **feat:** #172: `emulated://…&volatile=1`: an emulated drive held in memory keeps writes in a cache until a flush; `drive::emulated::crash` cuts the power (flushed writes kept, cached ones kept at random)
- **fix:** #172: `boot-local`'s slab opener, the resume source and the local-boot sources accept any `scheme://` drive (`emulated://`, `iscsi://`); only `nvme-tcp://` goes through the engine's initiator directly, and a fabric path no longer warns that it is a regular file
- **fix:** #301 (P0): no release booted with an initramfs built after #155. A slab's table is `total_slots × 64` bytes, rarely whole 4 KiB blocks. #155's table scan read exactly that, and the `nvme-tcp://` initiator refused any I/O that was not whole blocks. So every slab on an NVMe/TCP namespace failed to open: every release slab forge composes, in v1 or v2. The initiator now reads the covering blocks for a partial read, and read-modify-writes them for a partial write under one hold of its connection, as the O_DIRECT device does. Test: `integration_nvmeof::a_slab_on_an_nvme_tcp_namespace_opens_in_either_format`, which fails with the old rule
- **fix:** #301: a disk whose partitions hold no slab that opens now says why for each partition (`slabs_in_partitions_why`), instead of only the whole disk's "bad slab magic"
- **feat:** #216 (stormcert#23): boot-chain attestation and a per-machine TPM mark on the host record. `tpm: required | none` (unset = none) is set by `PUT`/`DELETE /api/v1/boothost/{name}/tpm`, which are destructive (admin token or a SubjectAccessReview), so a node cannot downgrade itself. Every boothost claim records what the engine served (`last_claim`: clone, claimed_at, claimed_as, host NQNs, host golden, golden, assignment version). `GET /api/v1/boothost/{name}/attestation` (by name, never an alias) returns both, the clone → host golden → golden chain checked when it is read (`chain: intact|broken|none`, `problems`), and the golden's release digest when it was published here. Readable with the node or admin token, or a Kubernetes bearer whose SubjectAccessReview allows `get` on `boothost/{name}` (stormcert's ServiceAccount)
- **fix:** #216: the Kubernetes review cache was keyed by bearer, resource and verb only, so a review allowed for one name answered for every name; it is keyed by name too

### 2026-10-05
- **BREAKING:** #158 stage E: metadata format 2 is the default. New slabs are written in format 2 (an engine before #158 refuses them), a serving engine migrates its v1 metadata slabs in place at their first persist, and a data directory moves from `volumes.dat` to `metadata.v2` (the old record set aside as `volumes.dat.pre-v2`, so an older engine finds none rather than a stale one). `[metadata] format = 1` / `$STORMBLOCK_METADATA_FORMAT=1` keeps format 1. Rollback to an engine before #158 needs a reinstall (a node) or a snapshot (the forge)
- **fix:** #158: `metadata.v2` in a data directory starts at 8 MiB and doubles when full: stormcos copies the data directory into its state volume a whole file at a time every ten seconds
- **feat:** #156 (with #158): extent size per volume, pools by size. Every slab picker, move and chunk allocation keeps an extent in slots of its size. A new volume takes what was asked (`POST /api/v1/volumes {extent_size}`, `/v1 {extent_size_bytes}`), else 8 MiB from 64 GiB where the role has an 8 MiB pool, else the default; clones keep their source's, compositions their members'. Other sizes need metadata format 2 (the v2 header carries it; a v1 record refuses). Restore checks each volume's size against the slabs it is on. With format 2 the install lays a 1 MiB data slab and an 8 MiB bulk slab (`stormblock-bulk`, last) when the data half is 256 GiB or more. Volumes report `extent_size`
- **feat:** #158 migration to metadata format 2, in place: the slab's record into both v1 copies, the v2 store into the region's first half, the header last (a cut leaves a v1 slab with its record). Automatic at a serving engine's first persist once format 2 is the default; by hand with `stormblock slab upgrade <slab>` or `POST /api/v1/slabs/{id}/upgrade` (destructive, admin)
- **feat:** #158 stage C: extent maps as a cache of the v2 store. A map is resident or cold (a summary kept); every accessor on a cold map panics rather than read it as empty. Handles and the manager's per-volume paths load a map at first use (`gem::ensure_resident`, `StorePager`); idle, clean, unattached maps leave memory under `[metadata] cache_mb` (least recently used, under the manager's lock). GC reads cold maps from their stores into per-slab bitmaps without loading them; a flow-over, drain, resync and the data seed pin maps in memory while they walk. v2 volume headers carry the volume's extent size (#156). `$STORMBLOCK_METADATA_CACHE_MB` evicts after every persist (tests). Measured: 12.4 B an extent freed per cold map; 0.4 ms to load 2 000 extents (`examples/map_cache`)
- **feat:** #158 stage B (with #157): metadata format v2 behind a gate (`[metadata] format = 2`, `$STORMBLOCK_METADATA_FORMAT`; default stays 1). Slab header v2 (64-bit table capacity; refused by older engines). The metadata region is a superblock pair, a change log and a copy-on-write B-tree (`volume/metav2.rs`), and a persist appends what changed: the GEM records changed extents, parity groups and whole volumes (`gem::Changes`), applied per store in the order the records were taken (`volume/persist_v2.rs`). The data directory keeps `metadata.v2` in format 2. `metav2::read_slab` reads either format. Slabs report `format`. Measured: one changed extent of 100 000 is 4 KiB and 2.7 ms per persist, against 2.7 MB and 9.4 ms in v1 (`examples/persist_cost`)
- **test:** #158: the store against a model three levels deep (no page leaked or named twice across reopens), torn log tails and superblocks; v2 through the volume manager (restarts, incremental persists, concurrent persists in order, v1 and v2 slabs together, the data directory); the power-cut test's 300 cuts in both formats
- **feat:** #158 stage A: slot and extent indexes are u64 in memory (legs, the GEM, slabs, slot table, chunks). No on-disk change: v1 metadata writes the same bytes for every index below 2^32, and formatting a slab past 4 Gi slots is refused (use larger slots) until format v2. A GEM extent is 29 B resident (was 25 B)
- **BREAKING (security):** Destructive verbs need the admin token or a Kubernetes bearer a SubjectAccessReview allows; the node token keeps the ordinary verbs, and every destructive call is audited (#274, owner's B, stormcos#250).
  - **The split** is `serve::api::classify`. Destructive:
    - slabs (format, delete, GC), arrays and their members, spares, forge on/off;
    - the pallet table writers, and an emulated drive's fault;
    - deleting a sealed volume or a template; seal; file and tar writes; `trim?apply`; `fsck?repair`; boot intents; other DELETEs.

    Ordinary: create, clone, attach, detach-like DELETEs, and deleting an unsealed volume.
  - **The admin token** always exists. It is configured, or read from or minted into `admin_token_file`, which defaults to `/run/stormblock-admin/admin_token` (0600, never under `/run/stormblock`, which every service mounts).
  - **A Kubernetes bearer** is checked with TokenReview, then a SubjectAccessReview against `[management.kubernetes]` (`storage.storm.io`, resource from the path, verb delete/create/update), cached for a minute. A user who is not allowed gets 403.
  - **`admin_gate = "audit"`** (or `$STORMBLOCK_ADMIN_GATE`) lets the node token through destructive verbs and logs each one, for the rollout.
  - **Audit log:** `<data_dir>/audit.log`, one JSON line per destructive call (who, what, target, decision, status).
  - **Callers that break until they move** to the admin token or a storage-admin service account: stormcluster (forge on/off), stormstorage (arrays), stormdrive (slab format, drive close). Issues filed
- **test:** `integration_destructive` covers the node token's verbs and every destructive one refused to it and accepted from the admin token; audit mode; a fake apiserver's TokenReview and SubjectAccessReview (alice allowed, bob 403, a bad bearer 401, cached); and the minted admin token (0600, its own 0700 directory, kept). `integration_auth` moved to a slab delete
- **docs:** #5–#7 (RAID1 prestage, fencing, dual-attach) are re-scoped onto stormstorage's RAID heads by owner decision (#179, b). #5 and #7 go to stormstorage#33; #6 keeps the engine's epoch fencing on leg attaches. The README's "Not built" now says `/v1` replication is control-plane only, and why
- **feat:** Emulated drives for scale tests (#208, stormcos#92).
  - `emulated://<name>?size=256T|1P[&backing=<dir>][&lbs=512]`, or `[[drives]] kind = "emulated"` with `size`, `backing` and `name`, is a drive (`drive/emulated.rs`) that reports any capacity and stores only what is written: in memory in 64 KiB pages, or in 1 GiB sparse chunk files. Zeros and discards store nothing.
  - It is accepted wherever a device path is. One name is one drive for the process. It reports `DriveType::Emulated`, and `GET /api/v1/drives` adds `emulated {name, stored_bytes, backing, failed}`.
  - `POST /api/v1/drives/{id}/emulate {"failed": true|false}` fails it (EIO on every I/O) or recovers it. A real drive answers 409.
  - Formatting a slab writes its zeroed table through the device's `write_zeroes`, so a 1 PiB slab formats in 0.1 s on an emulated drive.
- **test:** `drive::emulated` tests (1 PiB stores only what is written, one name one drive, fail and recover, directory backing across a reopen). `integration_emulated` covers:
  - a mirror on three 256 TiB drives: one fails, the volume degrades, a resync rebuilds every extent off it, every byte is intact, and the drives hold under 256 MiB;
  - a 1 PiB drive over the API: enrol, format a slab, fail, recover, and 409 for a real drive;
  - `[[drives]]` emulated config.

  `examples/emulated_scale` measured about 130 MiB resident per PiB (the free map) and 3.3 s to reopen a 1 PiB slab
- **feat:** The initramfs says why it could not name the node from the network, sets the domain, and sends its name in its DHCP requests (#238, stormcos#191). C2NR0Q2 registered as `storm-06f96d` with a reservation and a confirmed PTR naming it `stormblock1`. microdns sends no option 12 (microdns#14), and with no DNS server in the lease the PTR step was skipped without a word. Now:
  - each step that gives no name is said: no option 12, no DNS server in the lease, no PTR, or a PTR that does not resolve back;
  - the domain (option 15/119, else the name's own) is set as `/proc/sys/kernel/domainname`, and the FQDN printed;
  - `udhcpc` is given `-x hostname:<name>`: the declared `[node] hostname` from `stormcos-state:/config/stormcos.toml` on the local disk, else the firmware's boot name (#249), never an SMBIOS guess
- **test:** `tests/initramfs-node-name.sh` (24 checks, sh and busybox sh). `ci-node-name-verify.sh` boots the shipped blocks in QEMU with QEMU's packet dump: the guest's DHCP request carries option 12 = `stormblock1`, the lease applies, and with no option 12 back and no PTR the console says so and the node takes `storm-06f96d`
- **docs:** #157 (incremental metadata persistence) is built as part of #158's single format change, by owner decision (B, 2026-10-05). The change log lives in each metadata slab's region and is checkpointed into the paged extent index. Recorded in `docs/metadata-scale.md` §3.4 and the work plan
- **fix:** An install over an older release no longer boots the old disk when the appliance misses its first health check (#294, server8 on 11.82). Forge missed the one 3 s health check right after the mlx4 link came up, so no boothost was known, and the local-slab probe and the release check (both guarded on one) were skipped without a word. `boot-local` then died on `volume 'kubelet-data' not found`, scrolled off above `FATAL: root device /dev/ublkb0 not found`. `/init` now:
  - asks a boothost the network names (`/run/stormblock-boothost`) again for up to `STORM_BOOTHOST_WAIT` (90 s);
  - keeps why there is no appliance and says it;
  - runs the probe without one: a disk it would have sent to the appliance stops the boot with a FATAL naming what is missing and the release on the disk (`disk_release`: its root volume's os-release), and a disk that can boot boots with `RELEASE CHECK SKIPPED` on the console;
  - writes the engine's output to `/run/stormblock/engine.log`, followed onto the console, and repeats its last 25 lines after the root FATAL. The engine writing a file can also no longer meet a broken pipe
- **test:** `tests/initramfs-no-appliance.sh` covers a named boothost asked again until it answers, one that never does, and none named (no wait); the probe with no appliance on a disk missing `kubelet-data`, a disk that boots, and a device that is not a slab, plus the same with an appliance; the disk's release; and the engine report. It passes under sh and busybox sh, as does every other initramfs test. `ci-no-appliance-verify.sh` builds the shipped initramfs and boots it in QEMU with a laid node disk and no network, in three cases, all passing on dev:
  - the disk lacks `kubelet-data`: the boot stops naming it and "stormcos 11.79-test", and no engine is started;
  - the engine fails after the probe: the skipped release check is said, and the engine's error is repeated after the FATAL;
  - a disk that boots: the root device appears
- **fix:** The test container (`stormblock-test`) works again. Its attaches name the host NQN its initiator connects as: since #210 the shared subsystem admits no host, and every attach-based check in `short` and `medium` failed with 400. Its "nothing left" checks also wait for the eraser, because a deleted volume's slots stay allocated until they are overwritten (#286)
- **perf:** Resident compaction (#155, `docs/metadata-scale.md`). The engine no longer keeps a record of every slot in memory:
  - **The slab** keeps its free map and the entries that differ from the device. The slot table is read through a bounded cache of 4 KiB pages (`STORMBLOCK_SLOT_CACHE_MB`, default 16 per slab).
  - **The GEM** stores an extent in 24 bytes (slab ordinal, slot, share count, generation), in chunks of 64 virtual extents.
  - **There is no reverse index.** What is on a slab is found by walking the maps; who owns a slot is the slot table's to say.
  - **Measured on dev with `examples/metadata_footprint`:** a free slot went from 40.6 B to 0.6 B, an allocated slot's slab side from +65.8 B to +4.1 B, an extent from 220.5 B to 25–29 B, and per PB written from 313.7 GB to 28.7 GB. Allocation is flat at ~24 µs.
  - **Reading the table:** opening a slab, restore and the collector read it in one pass. The collector reads it with no lock held and checks each orphan again before freeing it. Copy-on-write, release, delete, clone, the eraser and both publishes read their pages before taking the registry (#269).
  - **No on-disk format change.**
- **fix:** A flow-over pass no longer counts what changed under it against the move (#155). The flow-over now takes one list of what is on the source per pass, since finding it walks the maps. Each stale item had counted against the 64-retry limit meant for one extent that keeps changing, and the live-clone test failed 6 of 8 runs. Now a pass skips them, the next pass lists what is still there, and only 64 passes in a row that move nothing count as a failure
- **test:** `volume::extable` (24-byte entry; a random mix of operations agrees with a B-tree). `metadata_footprint` measures scattered maps and clones and counts heap bytes. The live-clone flow-over test says what it left on the source
- **feat:** Forge mode on by default on a node (#287, for stormcos#273). With nothing kept, `adopt-ublk` serves the shared NVMe/TCP target and boot claims on `0.0.0.0:4420`. Its NQN is `nqn.2026-08.lo.storm:<node name>`, and it uses #210's closed policy: the shared subsystem admits no host, and claims and attaches get a subsystem per named host. The default is not written down. `forge.json` is the persisted state: the settings of a `PUT` (on), or `{"enabled": false}` after a `DELETE` (off, kept across restarts, so the default does not turn it back on). A file from #272 reads as on; one that does not parse reads as off, loudly. `GET /api/v1/forge` adds `state` (`on`/`off`) and `from` (`default`/`persisted`/`config`), and `source` can now be `default`. The daemon (an appliance) keeps #272's rule: off unless kept. stormcos changes nothing: same argv, no baked config
- **test:** `integration_forge::a_node_is_a_forge_by_default_until_told_off_and_stays_off` covers the daemon's default (off); a node's default (on, not written, a boot claim attached and read over it, an anonymous host refused); `DELETE` kept across a restart; a `PUT` served at the next start
- **feat:** Secure delete (#286). A slot whose last reference goes (a delete, a copy-on-write, a discard, a GC'd orphan, a drain) is marked `Erasing` in the slot table and is not free: never allocated, shared or mapped. The background eraser overwrites it, then frees it the ordinary durable way. Levels are `once` (zeros, the default), `dod3` (0x00, 0xFF, random, last pass read back) and `dod7`. A flush follows every pass, and a discard follows the passes except on a spinning disk. Foreground I/O comes first. `[erase] default` sets the node's level; `DELETE /api/v1/volumes/{id}?erase=` asks for more, never less. Slabs reached over a fabric are not erased. A stop or power cut resumes the erase, because `Slab::open` queues every `Erasing` entry. Each erased volume gets an audit record (log, `GET /api/v1/erasures`, `<data_dir>/erasures.json`). New metrics: `stormblock_erase_pending_slots`, `stormblock_erased_{slots,bytes}_total`. `/api/v1/slabs` reports `erasing_slots` and `erase`. An engine older than this reads an `Erasing` entry as free. `docs/erase.md` covers what an overwrite does not do on flash (only a sanitize or a crypto-erase is complete), and the crypto-erase design (not built)
- **test:** Slab tests: a freed slot is never handed out until it is overwritten, and is still owed after a reopen; dod3 is read back; the entry round trip. `volume::erase` tests: a deleted volume's data leaves the device, GC leaves erasing slots alone, and the audit record is kept across a restart; a delete asking for `dod7`. `integration_erase`: over HTTP, `?erase=dod3`, the started eraser, `GET /api/v1/erasures`, a bad level is 400
- **perf:** An install no longer waits for the data half (#285): `boot-local` seeded it onto the local disk before exporting the root, 166 s of the Dell's 352 s install boot (11.79). The fresh-lay path now hands it to the engine that adopts the boot (`FlowOver.data_flow`), which moves the appliance's system slabs into the local system slab, then its data slabs into the local data slab, while the volumes on them are mounted and written, then lays local boot. Both halves' sources are quarantined from the handover on (a write to an extent still on the appliance lands on the local disk), each local slab records every volume with a leg on its sources (`record_flow_over` per destination), a resumed flow-over carries the data half, and `flow_over_remaining` counts both halves (never 0 between them). `STORMBLOCK_SEED_DATA_SYNC` restores the seed before export. **Pairing:** an initramfs from this commit or later needs an engine from this commit or later as its successor; an older one leaves the data half on the appliance
- **test:** `the_data_half_moves_while_written_and_a_cut_resumes_it` (writes while the data half moves, a cut half-way, the next boot resumes from the image with every write); `a_fresh_install_seeds_every_data_volume_byte_for_byte` now goes through the successor's two halves and checks `flow_over_remaining` between them

### 2026-10-04
- **feat:** The initramfs takes the boot media's `install-config.yaml` from stormbootx (#275, stormbootx#79, stormcos#82). It reads `StormBootInstallConfig` (`v1:<length>:<chunks>:<sha256>`) and the chunks `StormBootInstallConfig0..N-1` right after the boot identity, uses them only when length and sha256 match, deletes every one of those variables once read (they are world-readable and carry `pullSecret` and `apiToken`) whatever came of it, and never prints the content. After the volumes are mounted it writes `/state/config/install-config.yaml` (0600) only when `/state` is mounted and holds none, so booting old media again never overwrites a node's applied config; the staged copy is removed before switch_root
- **test:** `tests/initramfs-install-config.sh`: verified, a bad digest, a wrong length, a missing chunk, a header that is not v1 or does not read (all deleted), no header; written on a first boot (0600), kept when one exists, not written when `/state` is not mounted, never printed
- **fix:** One job's detach no longer takes a shared namespace from another (#276). Two builds on one build box attach the same input golden: one volume is one namespace of the host's subsystem (#210), and the first job to detach removed it under the second (its filesystem then "can't read superblock"). A namespace attached to a host now has holders: an attach names one (`holder` on `POST …/attach`; an export holds as `export:<id>`), `DELETE …/attach?host_nqn=&holder=` releases that one, and the namespace goes with its last holder (`namespace_removed` in the reply). No holder named is the anonymous holder, as before; records from before holders are released as before; whole-volume detaches release every holder. (On forge's 13.7 the same golden got two NSIDs — the kernel's `duplicate IDs in subsystem` — which the current engine already refuses.)
- **feat:** Every NVMe/TCP connection the target closes is logged — reason (`host_closed`, `reset`, `protocol_error`, `io_error`, `handshake`, `connect`, …), host, controller, queue, how long it lived, commands served, its last command and how long before — at info, warn unless the host closed it (#276). It was a debug line, so a host's "Link has been severed" had nothing on this side to compare with. The target enforces no keep-alive timeout and exports have no lease: nothing on this side expires a connection mid-job
- **feat:** NVMe/TCP target metrics (#276, for stormcentral#374): `stormblock_nvmeof_connections_opened_total{queue}`, `stormblock_nvmeof_connections_closed_total{reason}`, `stormblock_nvmeof_keepalives_total`, `stormblock_nvmeof_io_errors_total{op}`, `stormblock_nvmeof_io_seconds{op}` (histogram)
- **test:** `integration_nvme_hosts`: `one_job_detaching_leaves_the_namespace_to_the_other`, `an_export_holds_its_namespace_until_it_is_deleted`, `connections_are_counted_open_and_closed`
- **fix:** The node API and every volume's I/O stalled during a flow-over on a slow disk (#269 reopened, server3 on 11.79: one 7200 rpm ST2000DM008; the Dell on the same build was fine). Found with a task dump of a model of the node: `persist` and every volume `flush` held the slab registry's read lock across device flushes; the first allocation (a template clone's copy-on-write) then queued for the write lock, and tokio's RwLock queued every later reader of every volume behind it, so each flush of seconds stopped the node for seconds, chained by the flow-over's persist after every extent; `persist` also held the volume manager across its flushes. Now no lock is held across a device flush: `slab::sync_registered` takes the registry only to publish entries between its two flushes, a slab's own `volumes.dat` is written outside it (`MetadataWriter`), and a persist takes its records in memory, syncs every slab at once, then writes them under a generation check. The flow-over and a mint persist detached (the manager held only to take the records). In the model with 1.5 s flushes `GET /api/v1/volumes` went from 17 s to 31 ms and a clone from 41 s to ~20 s (now only the disk: six durable round trips on one actuator); with 25 ms flushes clone p99 is 0.80 s
- **perf:** One cache flush per device for every caller that asked before it began (`drive::flushgate::FlushGate`, in `SasDevice`; a node's slabs are partitions of one disk); a slab's metadata copy is not rewritten when unchanged; a mint's UUID stamp leaves its flush to the persist that follows; the flow-over yields to foreground I/O for as long as its whole last move took (copy and persist, up to 2 s), and an API persist counts as foreground
- **feat:** `GET /debug/stalls`, `/debug/tasks`, `/debug/threads`, `/debug/locks` (open, read-only, #269) and an API stall watchdog on an OS thread of its own: a request waiting over 10 s is logged with the lock states, every async task of every runtime with the `.await` it is parked on (tokio task dump; the build sets `--cfg tokio_unstable`), every thread's kernel stack, and the device flush times
- **test:** `cli::flow_over_api_tests::server3_api_during_flow_over` (ignored: the model; `FLOW_API_FLUSH_MS`, `FLOW_API_SECS`), `drive::flushgate::concurrent_callers_share_flushes`
- **build:** `.cargo/config.toml` (`--cfg tokio_unstable`), tokio's `taskdump` feature, `Cargo.lock` gains `backtrace` and its dependencies

### 2026-10-03
- **fix:** An install never takes a shelf drive (#273, for the NetApp shelf on the Dell, stormraid#1). `rd.stormblock.assimilate=any` took any drive that was not a stormblock slab (a stormraid member, a drive with a foreign partition table), and `force` and the install's data-slab scan took whatever the bus listed. Now: (1) the drive `rd.stormblock.slab=` names, when it is on the machine, is the only one the survey or an install may take; (2) a drive behind a SAS expander or in an SES enclosure is never taken (`rd.stormblock.allow-external=1` for a server whose own bays sit behind one); (3) a drive that is not a stormblock slab is taken only when its first and last MiB are zeros (a stormraid superblock is named on the console), and `force` clears only the named drive when it carries a signature. Every drive left alone is said on the console
- **test:** `tests/initramfs-boot-hook.sh`: 15 cases with several drives (the named drive beside a blank shelf and a stormraid member, a drive behind a SAS expander, a partition table, data in the last MiB, an install over the node's layout with a data slab in the shelf, no intent with a data slab only in the shelf, `force` with no drive named, `allow-external`)
- **fix:** The mount list leaves the kernel command line (#262; 11.68 mounted nothing, stormcos#236). `rd.stormblock.mount=` was one ~1.8 KB word on a line x86 caps at 2048 bytes; past the cap the EFI stub truncates and boots anyway. With no `rd.stormblock.mount=` on the line, `/init` now reads `/etc/stormblock/mounts` (one `<vol>:<path>` per line, `#` comments) out of the root volume of the slab it boots, before `boot-local` exports anything: in the local-disk probe (the disk's own release) and again once the slab is settled (a claimed clone or the local disk). The console says where the list came from. The command line still works and wins, for older images
- **feat:** `stormblock slab cat --slab <s>… --volume <v> --out <file> <path>`: a file out of a volume's filesystem with nothing attached (userspace ext4). Exit 0 read, 1 no such file, 2 the slabs or the volume cannot be read
- **test:** `cli::slab_cat_tests` (a file out of a volume on a slab file; a missing file is 1, a missing volume 2); `tests/initramfs-mounts.sh` (the list from the root volume, from a named root, the command line winning, neither, no slab, an empty file)
- **feat:** Forge mode turned on per node and kept by the engine (#272, for stormcos#187: one image, the forge role chosen at install). `PUT /api/v1/forge` (admin token) takes the `[nvmeof]` settings (`listen_addr`, `nqn`, and the #210 host policy), starts the shared NVMe/TCP target live and keeps them in `<data_dir>/forge.json`, which goes wherever the data directory goes (`stormblock-state` on stormcos). Every later start of `adopt-ublk` (or a daemon with no export device of its own) serves it again, with no `--config` and no argv change. `DELETE` stops accepting new connections (open ones finish) and forgets the settings; `GET` reports what runs and whether the API or the configuration set it up. A target the command line or `--config` set up answers 409. The `[nvmeof]` settings in force are one value (`AppState::nvmeof_settings`) that the host policy, the boot claim's and `/v1`'s attach, and usage read. The target is bound before it is published, so a taken port is an answer, not a log line
- **test:** `integration_forge`: a node becomes a forge (a boot claim's attach is connected and read), keeps it, serves it again at the next start with its host policy, and stops being one; a configured target is not the API's to change. `serve::api::tests::forge_mode_is_set_by_the_admin`
- **feat:** Forge mode on a stormcos node (#206, for stormcos#90's bastion): `adopt-ublk` serves the shared NVMe/TCP target when its `--config` has an `[nvmeof]` section (`listen_addr`, `nqn`, and the #210 host policy: `allowed_hosts`, `allow_any_host`, `require_dhchap`, `boothost_host_nqn`), so a boothost claim answers with something to attach and exports work, from the engine that owns the node's slab. Without the section nothing changes (stormcos's baked config has none: no :4420). The slab's drive is never published raw (`export_drives` is ignored there). The daemon's start-up of the target (store, host policy, restore hosts, exports and NSIDs, run) is one function both use
- **test:** `cli::forge_mode_tests`: no `[nvmeof]`, no target; with one, a boot claim's attach is connected with the engine's NVMe/TCP initiator as the boot host's NQN and reads the release's bytes
- **fix:** The node's API and volumes stalled during a flow-over (#269, P0; 11.78 on the Dell, 7528 extents from forge onto a spinning sda: template clones and `GET /api/v1/volumes` timed out at 60 s, so every claim and VM failed until the flow-over finished). Each move read the appliance's slot, wrote the local disk and read it back holding the extent map and registry write locks. The flow-over now copies with only the slot's fence held (`PlacementEngine::migrate_leg_unlocked`): the registry is taken to allocate the destination (reserved against the collector), and the map and registry to publish, re-checking that the extent still names the source and carrying the slab's share count
- **perf:** The flow-over yields to foreground I/O (#269): when any volume was read or written since the last move, it waits as long as that move took (at most 250 ms) before the next. An idle node moves at full speed
- **test:** `a_flow_over_copy_holds_no_lock_the_node_needs`: a move parked inside its read of the appliance's slot leaves the map and the registry free, and a write and flush on another volume finish meanwhile
- **docs:** #268 (the Dell never installed to sda): no code change. Its stormbootx (0.4.0 on the iDRAC's virtual CD) handed no name down, so the name was an SMBIOS guess and #249 kept it off a drive carrying a slab. Owner's decision: put stormbootx v0.14.0 (which hands the name down) on the Dell instead of letting a guess install over a slab, which would reopen #249 on chassis that share a serial. With a firmware name, the existing rules install a release the disk does not hold (#261); re-checked under sh and busybox sh
- **fix:** A volume is never deleted while a ublk device serves it (#267, P0; pvetest1 11.73, stormcos_qa's turbomode test: a running container's executable changed under it and it died of SIGSEGV after 19–60 min). The kubelet pulls an image as a registry clone, attaches it over ublk and mounts it, and never binds it. The registry reaps a claimed clone after 900 s by deleting its export. The serving GC then saw no session on the export's portal (the node reads it over ublk), withdrew it, and deleted the ephemeral volume under the mounted filesystem: reads of its extents gave zeros, and its slots were reused by the PVCs being written. Now a ublk device, created or adopted, holds its volume (`volume::holds::ServeHolds`, shared by the volume manager and the ublk export manager), and `VolumeManager::delete_volume` refuses a held volume (`VolumeError::InUse`) on every path, not only the API's `DELETE`. The serving GC keeps a withdrawn ephemeral row whose volume is in use and deletes the volume once the device is detached; `DELETE /api/v1/volumes/{id}` and `/serve/v1/volumes/{id}` answer 409 for a held volume
- **test:** `integration_serve_in_use` (a held volume is not deleted; an ephemeral volume in use outlives its withdrawn export, data intact, and is deleted after release), `ublk_export::a_served_volume_is_held_until_its_device_goes`, `volume::holds`; `ci-ublk-qd-verify.sh` runs the chain against a real kernel (ublk attach, ephemeral export withdrawn, volume kept with its data, DELETE 409, deleted after detach)
- **feat:** `GET /api/v1/health` reports `flow_over_remaining` (#260, for stormcentral#301): the extents of this node's volumes still on a remote slab while the flow-over moves them onto the local disk, 0 once it has finished, left out on an engine running no flow-over. The flow-over keeps the count in an atomic as it looks at each extent, so the open probe neither waits on the extent map nor leaves the field out for being busy (absent would read as settled mid-move). An abandoned flow-over leaves it above 0
- **test:** `the_flow_over_counts_down_what_is_left_on_the_appliance`: from every extent left to 0, one fewer per extent moved
- **perf:** ublk devices serve every request at once (#264). The queue worker `block_on`'d each request before taking the next, so every ublk volume (on stormcos, everything a node runs on) was served at queue depth one whatever it advertised (128), and a FLUSH — a drive-cache flush, tens of milliseconds on a spinning disk — stalled every read and write on the volume behind it. Requests now run as tasks on the runtime and are committed by the queue thread as they finish (an eventfd armed in the queue's ring), each tag's buffer moving with its request. A stand-down waits (bounded, 5 s) for requests still running before the thread exits. `STORMBLOCK_UBLK_SERIAL=1` restores the old behaviour, for measuring only
- **fix:** `Slab::sync` is a group commit (#264): one at a time per slab, each covering every caller that asked before it began, so concurrent FLUSHes cost one round of device flushes. Unserialised, a FLUSH on one volume could find the slab's confirmed slots already taken by another volume's sync that had not yet written their entries, and be acknowledged before them (the #171 rule)
- **test:** `ci-ublk-qd-verify.sh`: the engine as root in a QEMU guest on dev, a ublk volume on a disk behind dm-delay (8 ms per read, write and flush, a 7200 rpm stand-in), served serially and at once: 16 parallel readers, reads while another process writes and fsyncs, and a concurrent-write round trip
- **docs:** #264: a busybox pod start makes no stormblock clone or attach (rustkube-node's `sandbox` step is stormpump `SandboxAcquire` + CNI ADD; the root is the boot-mounted golden), so the issue's per-pod clone/attach/journal syncs are not on that path; what is stormblock's is the ublk serving above

### 2026-10-02
- **fix:** Install = wipe; the same release = recovery (#261 reopened; owner, 2026-10-02: "an update is done from a running system, not a half-ass install"). The boot-time path that kept an old data slab under a new release is gone: it is what broke 11.68 (stormcos#236, the kept data slabs lacked `kubelet-data` and the mount chain stopped). A boot running from its claimed image asks `slab holds` of the local drive with this node's data slab, with or without a boot intent: another release (or a bootable disk the probe ruled an install) wipes the whole disk, system and data slab, with `--local-disk-force` (`INSTALL:` on the console); the same release, or the same release cut short (#258/#259), keeps it (`RECOVERY:`); cannot say leaves every local drive alone and runs from the appliance (`LEFT ALONE:`), so a doubt neither destroys data nor merges releases. A release change on a running node is stormupdate's (stormupdate#1)
- **test:** `tests/initramfs-boot-hook.sh`: another release is wiped with and without an intent; the same release and the same release cut short are kept; cannot say leaves the drive, even under `force`; the probe's install over a disk is forced with an intent too
- **fix:** An upgrade to a new release without boot intents booted the old slab (#261, P0; C2NR0Q2 claimed 11.65 and came up on 11.56). The probe rejected the 11.56 disk because it lacked a volume that 11.65's command line mounts. The claimed image then booted, and #258's no-intent rule kept every disk with a data slab, so the survey updated nothing that mattered and the old system and data came back. Now, before keeping such a disk, `/init` asks `slab holds <disk> <claimed>`. Exit 1 (another release) is an upgrade (`UPGRADE: … holds another release`), and a fresh slab is laid over that disk as before #258. The same release (0), the same release cut short (3, the #258 power cut) and cannot say (2) keep it. A guessed name still installs nothing (#249)
- **test:** `tests/initramfs-boot-hook.sh`: an unbootable disk of another release is forced (with the node's layout and with a lone data slab); the same release and the same release cut short are kept; a guessed name with another release is left alone
- **fix:** Finishing a cut-short flow-over claimed `boothost/<SMBIOS serial>`, not the machine's name (#259, P0; server3 on 11.64: the X9 blades share one serial, whose image is server1's old 11.50 one). That clone did not carry the slab the records needed, so every mapping on it was dropped, and PID 1 died of SIGSEGV. Now `/init` exports the name it resolved (`STORMBLOCK_BOOT_TAG`), and on `slab holds` exit 3 it hands `boot-local` the clone it just claimed and compared (`STORMBLOCK_RESUME_SOURCE`), so there is no second claim. The engine's own fallback reads stormbootx's `StormBootTag` EFI variable before SMBIOS. When the records still place extents with no leg on any slab `boot-local` has, it **refuses to boot** and names the volumes and slabs, instead of dropping the mappings
- **test:** `the_resume_names_the_machine_as_the_firmware_did` (env, then EFI variable, then SMBIOS); the #258 test now first resumes from another machine's image and must be refused; `tests/initramfs-boot-hook.sh` checks what boot-local is handed on exit 3 and that the claimed name is exported
- **fix:** A power cut during an install's flow-over re-installed the node and lost its data half (#258, P0; 11.63 on server3: 0 of 300 objects). There were three causes. (1) A golden the flow-over had not reached had no leg on the local system slab, so the local disk's record left it out, and after the cut the initramfs probe found the disk "missing N mounted volume(s)". The local system slab now records every volume with a leg on a flow source from the moment the sources are quarantined (`VolumeManager::record_flow_over`, persisted at once). (2) `slab holds` answered such a disk "not held", which is an install. It now exits **3**: the same release, unfinished. `/init` boots the disk, and the engine finishes the flow-over from a fresh clone (#171). This reverses #239's "unfinished = install again": since b189b3e nothing the node writes stays on the clone. (3) #236's no-intent stopgap forced over whatever disk a claimed boot met. Without a ticket it now forces only over a disk the probe ruled an install over, or when no local drive carries a data slab. Otherwise it says `NOT an install` and keeps the data half
- **test:** `cli::install_tests::a_power_cut_before_the_flow_over_moves_anything_keeps_the_disk_bootable`: after a cut before anything moved, the disk alone names every volume, `release_held` says `Unfinished`, and the resumed boot reads every volume as the image had it plus the data half's write. With `RECORD_FLOW_OFF_258=1` it fails as server3 did: the disk names only the data half. `tests/initramfs-boot-hook.sh` adds cases for exit 3, for keeping a data slab the probe could not boot, and for still forcing with `INSTALL_OVER`, with no data slab, or with a ticket
- **feat:** RAID sets on a shelf, with hot spares (#252, owner's choice B). `POST /api/v1/shelves` lays a shelf out as several drive-level RAID sets (default 2 × RAID-6) plus spares in the shelf's own pool. Each set is one slab in the general pool, in its own failure domain `shelf=<name>/set=<set>` (a new `set` rung), so volumes are allocated onto the sets and `mirror:2@set` spans two of them. A failed member (I/O error, a drive health report of `failed`/`missing`, or `POST /api/v1/arrays/{id}/members/{slot}/fail`) takes the smallest spare that fits, from its own pool and then the global one, and rebuilds onto it in the background under the stripe locks. Progress and a rate cap are on the array. `POST …/members/{slot}/replace`, `/api/v1/spares`, and `…/scrub` (verify and repair, with progress) round it out. `GET /api/v1/health` reports the worst set's state, and there are `stormblock_raid_*` gauges. `docs/raid-sets.md`; `docs/multi-drive.md` records the reversal
- **feat:** RAID arrays are reassembled from their drives (#168). Superblock v2 on every member: the slot table with each member's uuid, state and rebuild position, an `events` counter (the newest copy wins), the set's name and its spare pool. The daemon assembles every array its configured drives carry before it scans for slabs, adopts the slab on each, and leaves members and spares out of the slab scan and `--raid` (which used to format the array again on every start). A missing member is failed; more missing than the level tolerates is refused and the drives are left alone. A rebuild resumes where it stopped. `POST /api/v1/arrays/assemble` does the same for drives registered later and reports stale (replaced) members
- **feat:** An on-disk write-intent bitmap on every member (#168). A write's chunk bits are on disk before its data, and are cleared lazily after a flush and on a clean close. At assembly the dirty chunks are resynced: parity recomputed from the data, a mirror's other legs copied from the first
- **fix:** RAID-6 never computed or wrote Q: it was RAID-5 with an unused member and survived one failure, not two. Now P+Q, with recovery of any two lost strips (#168)
- **fix:** RAID-10 mirrored every member and reported pairs × the capacity; a write past one member's size failed. Now near-2: pairs mirror, units stripe across the pairs
- **fix:** RAID-5/6 partial-stripe writes took no lock, so two writers to one stripe lost a parity update. Writes are now done stripe by stripe under a stripe lock, read-modify-write when the members are there and reconstruct-write when one is missing
- **fix:** RAID reads went to a member still rebuilding and returned what was on the new drive (#175). A rebuilding member is read only below its rebuild watermark
- **fix:** A RAID-5/6 member whose I/O failed was never failed, and a RAID-1 read error did not try the other leg. Every level now fails the member and continues degraded, records it in the survivors' superblocks before acknowledging, and refuses to fail a member whose loss would lose data
- **fix:** `POST /api/v1/arrays` answers 409 for a drive that is a member or spare here, holds a registered slab, or carries the superblock of an array not held (`force` overwrites the last only) (#215). The same check covers replace, spares and a RAID-1 add. `DELETE /api/v1/drives/{id}` refuses a member or spare. Deleting an array wipes its superblocks
- **fix:** Two RAID arrays of one level shared the failure domain `drive=RAID-6` (the serial was the level); an array's serial is now `raid-<uuid>`
- **fix:** `migrate_to_local` compared member states to `"Active"`/`"Rebuilding"` while their text is lower-case, so it never saw the rebuild finish
- **test:** `src/raid/tests.rs` (in-memory drives with injected faults: every level round-trips; RAID-6 survives every pair lost; degraded writes keep parity; concurrent writers to one stripe; write and read errors fail the member; the last copy is kept; replace and rebuild; writes during a rebuild; #175; assembly whole, with one missing, refused, a stale member, a dirty chunk resynced; the bitmap on disk; spares by pool; a rebuild resumed after assembly; scrub). `parity.rs`: GF tables, Q, every one- and two-strip loss. `tests/it/integration_raid_sets.rs`: a 14-drive shelf over HTTP, a drive reported failed, the spare rebuilt, volumes (one mirrored across sets) intact, a clean restart reassembles both sets and adopts the volumes
- **perf:** A RAID set's large write works its stripes in parallel, and a rebuild its batch's units, each under its own stripe lock (#252)
- **feat(example):** `raid_set_rate` — an 11-member RAID-6 on O_DIRECT files: sequential and 4 KiB random write, read healthy and with two members lost, the rebuild of one member onto a spare, checked by reading with two others lost
- **docs:** #252 work plan — shelf/row RAID answer to the owner (how per-volume parity handles a failure and what it costs at row width); recommendation moved to drive-level RAID sets; waiting on the owner
- **docs:** Refreshed from the code since 2026-09-28 (#228, #236, #237, #239, #249, #250, #251). README: a table of every kernel command-line parameter `/init` reads (with defaults) and its bounded waits; `slab holds` in the subcommands; the initramfs's files (`boot.d`, `build-date`, `install.json`, `no-intent`, the stormbootx EFI variables); "Not built, or not wired" adds #240, #232, #228/#233/#248, #231, #215, #218, #205; source sizes. The cross-component corrections of stormcos#65 (#242): `compose-release.py` is stormcos's, the component registry is stormcentral's database, the PVC blanks are cut by stormcos's build (sbregistry names and surveys them), and the console reads `placement`. CLAUDE.md current state and the deck's status slide follow

### 2026-10-01
- **fix:** The initramfs steps the clock from NTP before anything checks a certificate, and writes the RTC (#251, stormcos#213). The X9 blades have no RTC battery: after a power cut the kernel started in 2000, and stormcert and fastetcd start next to `timesync`. After the network, `/init` runs one `busybox ntpd -n -q` under `timeout` (3 s, `STORM_NTP_WAIT`), first to the lease's option-42 servers, then to `162.159.200.1` and `216.239.35.0` (addresses only, no DNS). On a step it runs `hwclock -w -u` and prints `clock stepped by +N s from <server>`. Otherwise a clock before the image's build date (`/etc/stormblock/build-date`, `SOURCE_DATE_EPOCH` or the build's clock) is set to that date, with a loud warning. It never blocks the boot. `rd.stormblock.ntp=off` skips the NTP step
- **test:** `tests/initramfs-clock.sh` (stubbed ntpd/date/hwclock: lease first, fixed fallback, floor, ahead, no network, link-local, off, no RTC, the bound). `ci-clock-verify.sh` runs the block as PID 1 in QEMU with the RTC at 2000 and real busybox ntpd
- **fix:** `build-stormblock-initramfs.sh` given a relative output path wrote only the microcode archive there; the main archive went into the staging tree and was deleted with it
- **fix:** The initramfs brings up the Mellanox ConnectX-3 port (#250, P0). On the X9 blades only the Intel port existed in Linux, though stormbootx had just DHCP'd over the Mellanox. `mlx4_core` matched the PCI ID, but `mlx4_en` matches only `auxiliary:mlx4_core.eth`, which `mlx4_core` creates at the end of a seconds-long probe, after the modalias walk had stopped. Now a table of core → network half (`mlx4_core:mlx4_en`) is loaded by name after discovery. Before the uplink is chosen, `/init` waits up to 15 s (`STORM_NETDEV_WAIT`) for every bound network-class PCI function to have a netdev, re-walking auxiliary modaliases each second, and names any that never do
- **test:** `tests/initramfs-netdev.sh` (the half loaded by name, already loaded, refused, a late auxiliary device, non-NIC functions not waited for); `tests/initramfs-nic-selection.sh` pins the order of two ports at equal speed
- **fix:** The initramfs claims as the machine the firmware claimed as, not as its SMBIOS serial (#249, P0). On an X9 MicroCloud blade stormbootx claimed `boothost/server8` (DHCP + reverse DNS), then `/init` claimed `boothost/<the chassis serial all eight blades share>` — server1's old synonym, 11.50 — and laid the local disk from it with force. `/init` now reads the name and host NQN stormbootx hands down in two volatile EFI variables (`StormBootTag`, `StormBootHostNqn`, vendor GUID `ab361f54-0166-44a4-a088-1ac22e98ab76`; efivarfs is mounted if nothing has), ahead of `rd.stormblock.tag=` and SMBIOS; a cmdline tag or SMBIOS serial that differs is reported and the firmware's name used. A name guessed from SMBIOS still boots, but installs over no disk: the probe boots the local disk instead of an install, and the survey takes only a blank drive (no #236 force, no ticket force). `rd.stormblock.trust-smbios=1` lifts that. The stormbootx side is stormbootx's issue
- **test:** `tests/initramfs-boot-hook.sh`: boot identity (firmware variable, NQN, cmdline disagreeing, no variable, a value that is not a name) and the guessed-name guards in the probe and the survey
- **fix:** `stormblock slab holds` answers not held (exit 1) for an install that never finished: every golden is recorded, but the records still place extents on a slab that is not on the drive (the appliance clone a flow-over was moving from). The next boot installs again instead of booting a half-installed disk, as server1 did on 11.56 (#239, the owner's ask)
- **fix:** A write made during an install boot survives the boot being cut short (#239, reopened, P0). Until the flow-over reached it, a clone's own extent (the slot its UUID stamp copied: an ext4's superblock, bitmaps and the start of its inode table) sat on the appliance's per-boot clone and was written in place there. Copy-on-writes already landed on the local disk. A boot cut short resumes from a fresh, pristine clone, so the next boot read the local directory blocks against the image's inode table: `iget: checksum invalid` on the first free inodes (#155–#164 on 11.57's cadvisor, stormlb, vmimages, stormvm and stormimds; #1410 on 11.50's hubble-relay). Now the appliance's system slabs are quarantined as soon as the boot knows a flow-over is coming (`boot-local` after laying or resuming, `adopt-ublk` before serving), and a write to an extent on a quarantined slab goes through copy-on-write onto a slab that stays (in place only if there is no room anywhere else). Durability rule 11
- **test:** `cli::install_tests::a_write_during_the_install_boot_survives_a_flow_over_cut_short`: an image with an e2fsprogs service golden, boot 1 writes its clone's own first extent and a shared one, then boot 2 resumes from a fresh claim; both writes must be there (`RELOCATE_OFF_239=1` shows the loss)
- **test:** `cli::install_tests::a_real_release_installs_byte_for_byte` (ignored; `AUDIT_239_IMAGE=` a copy of a published image): the install with the real image, fresh and over a used disk, a successor that reopens claim and disk from their records, 64 writers on every clone's first extent during the flow-over. Passed on dev with 11.50 (128 volumes, 5146 extents)
- **feat(example):** `slab_audit` reads `http://…/image.img` by HTTP Range GETs (nothing on the server changes) and streams the clone/golden diff; `extract SLAB DIR [MAX]`

### 2026-09-30
- **docs:** #239 verified on dev (847/847) and staged as golden-stormblock-296e38521d4c. Size ruled out for cni-bin. Follow-ups: #240 (parity and StormFS paths not fenced), #241 (re-seed a derived volume, owner's question)
- **fix:** A fresh install no longer corrupts the volumes the system half's flow-over moves while they are in use (#239, P0). The flow-over moved a slot, then freed and discarded the source. An I/O that had looked up the old slot and not yet used it lost its write, or read zeros. A copy-on-write copied those zeros into the clone. On 11.56 that was `cni-bin`'s root directory while Cilium's install-cni-binaries filled it (`No space for directory leaf checksum`), so Cilium never started. Now a slot fence (`volume/fence.rs`) covers every read, write, copy-on-write, discard and resync: the I/O holds the slots it found shared, and a move holds its slot exclusive. The flow-over, the data seed and drain wait for the fence before taking the map and registry. Whole-run moves (rebalance, evacuation) only try it and get `Busy`
- **fix:** Every extent move reads its copy back and refuses one that differs from what it read, before any map names it (#239)
- **fix:** The background flow-over quarantines the slabs it empties, so a copy-on-write made while it runs lands on the local disk. Before, it could land on the appliance behind the move and be left there. If the flow-over gives up, the quarantine is lifted (#239)
- **test:** `cli::install_tests` (#239): an I/O held inside the device while the flow-over moves its slot (without the fence: zeros); the flow-over under concurrent writers and readers of a live clone; an install of an image with an e2fsprogs blank in its data slab, every volume's sha256 compared after the seed, a fresh open and from the disk alone. `boot-local`'s flow-over is `take_local_disk`, and the background loop is `flow_system_half`, so tests drive them
- **feat:** The initramfs writes its boot messages to every `console=` on the kernel line, not only `/dev/console` (the last one). With `console=tty0 console=ttyS0,115200n8` the screen now shows the install, the flow-over and a FATAL too; serial is unchanged. `/init` and the engine it starts write through a fifo to a `tee` onto each console that exists and opens; the reader ignores HUP/INT/QUIT/TERM/PIPE and reruns `tee` if it is killed, so the engine never meets a broken pipe. stdout goes back to `/dev/console` before `switch_root`. The emergency shell also starts on every other console; which one `/dev/console` is comes from `/sys/class/tty/console/active`. A serial port with no UART (sysfs `type` 0) is skipped. `tests/initramfs-console.sh`; `ci-console-verify.sh` runs it as PID 1 on dev's kernel in QEMU (#237)
- **fix:** An install boot lays a fresh slab without a boot intent (#236, stopgap until forge serves intents, #235). The initramfs's local-slab probe no longer boots a local disk on sight when a boothost is known: it claims the machine's image and boots the disk only if it already holds that release (`stormblock slab holds`, new: every golden by volume id); a release the disk does not hold is an install. A boot that runs from an image claimed from an appliance stating no intent (`/run/stormblock/no-intent`, written by `boot-claim`) takes its local disk with force — a new data slab, never the old data half — and says on the console that the old slab was discarded. `assimilate=off` still means no; with intents served, the intent decides.

### 2026-09-29
- **docs:** #148 closed: the boot-intent route (`GET`/`PUT …/boothost/<tag>/intent`, `POST …/installed`) shipped in v20.0.0; re-verified on dev. `installed` from the local boot is #220
- **feat:** A volume carries its logical block size, 512 or 4096 (default), and every transport presents it: ublk (512e — physical 4096), NVMe-oF Identify Namespace, iSCSI READ CAPACITY. `compose/disk` presents the disk at its `lba`; `POST /api/v1/volumes` takes `lba`; clones and snapshots inherit it; every volume response reports `lba`. Firmware reads boot media at 512: a 4096-byte ESP was `NOT_FOUND` to server1's AMI Aptio 4 and unbootable to OVMF on pve (#228)
- **feat:** Volume metadata V9 records the block size. A payload whose volumes are all 4096 is still written as V8, so every slab and `volumes.dat` without a 512 volume — every release slab — stays readable by an older engine (#228)
- **test:** `ci-boot512-verify.sh` (unprivileged, dev): a 512 boot disk (512-sector ESP with stormuefi, boot pallet with the host kernel) and its clone; the Linux kernel over NVMe/TCP sees 512 (a plain volume 4096), both partitions, and mounts the ESP; OVMF boots the clone as a virtio disk with no block-size override and stormuefi starts the kernel with the pallet's command line (#228)
- **docs:** composed-disks.md and README: block size per volume, what firmware reads (#228)

## [v20.0.0] — 2026-09-28

Major: `cluster` is opt-in (#209), and the shared NVMe/TCP subsystem admits no host by default (#210). Forge rollout settings and rollback: #148.

### 2026-09-28
- **docs:** Refreshed from the code since 2026-09-27 (#199, #200, #148, #209, #210). README: the default features (`cluster` opt-in), the build and test commands (nextest, `tests/it`, `tests-runtime`, the `dist` profile, the `ci-*.sh` scenarios), RouterOS marked not shipping (owner), `STORMBLOCK_DHCHAP_SECRET`, per-host NVMe subsystems in the diagram, the TLS backend (ring only), the gaps #212/#213/#217, and the source layout and sizes. That the golden build still uses `--release` rather than `dist` is stormcos#169. auth.md and README say when an install is reported (after `local-boot` lays the disk, before any boot from it — #220). CLAUDE.md: current state, features, RouterOS, TLS, architecture (`cli.rs`, `nvme_hosts.rs`, `auth.rs`, test layout). The deck and `contract/README.md` follow (#223)
- **BREAKING (security, #210):** NVMe/TCP serves a volume to the host an attach names. Every door that serves over NVMe/TCP (`/api/v1/volumes/{id}/attach`, `/v1` attach, `POST /api/v1/exports`, a non-boot claim) takes `host_nqn`: the volume goes into `<nqn>:host:<id>`, a subsystem whose only allowed host is that NQN. A host that is not allowed gets Connect Invalid Host; discovery lists only what the asking host may connect to. The shared subsystem admits no host unless `[nvmeof] allow_any_host = true` (or `allowed_hosts`), so an attach without `host_nqn` on such a node is refused (400). Before this, anything that reached `:4420` saw every golden, every release and every boot clone, read-write — pve saw 71 namespaces on forge
- **feat:** DH-HMAC-CHAP in-band authentication (NULL DH group, SHA-256/384/512) on the target and in the engine's own initiator. `"dhchap": true` on an attach (or `[nvmeof] require_dhchap`) gives the host a secret, returned as `dhchap_secret` in `nvme connect --dhchap-secret` form; every queue must then authenticate before anything else runs (#210)
- **fix:** `require_dhchap` does not apply to boot hosts: a claim is unauthenticated and firmware cannot be handed a secret, so a boot subsystem is bound by NQN only (a secret there would stop every machine booting) (#210)
- **feat:** A boot claim's clone is served from `<nqn>:host:<name>` to that machine's NQNs alone — `nqn.2026-09.lo.storm:host-<name>` (what stormbootx presents), its aliases and the tag it claimed as — with no firmware change; `[nvmeof] boothost_host_nqn` is the template. The next boot's clone replaces the released one there (#210)
- **fix:** A sealed golden is never served on the shared subsystem, even on an open node, and is always a write-protected namespace (NSATTR bit 0; writes answer Namespace is Write Protected). `mode: "ro"` attaches are write-protected namespaces too. Restored exports and attach records of goldens are no longer put on the shared subsystem (#210)
- **fix:** "duplicate IDs in subsystem": a subsystem never serves one volume at two NSIDs, and `/v1`/`/api/v1` attach records are put back on the shared subsystem at startup. They were persisted and never restored, so a recorded NSID could later be handed to another volume, and the first volume's consumer read that one (#210)
- **fix:** Dropping an export no longer removes the shared subsystem's NSID when that NSID is some other volume (a `/serve/v1` export is NSID 1 of its own subsystem) (#210)
- **fix:** `image build`'s golden reader exports the golden to itself (`host_nqn`, `dhchap`) and reads `nqn`/`port`, which the export reply now carries; it did not before, so that path could not have worked (#210)
- **feat:** `GET /api/v1/volumes/{id}/attach` lists the host subsystems serving a volume; `DELETE …/attach?host_nqn=` withdraws it from one host; a volume served to a host is busy for delete. Host subsystems, hosts, secrets and NSIDs persist in `<data_dir>/nvme_hosts.json` (0600) and are restored before anything is served (#210)
- **chore:** `.gitignore`'s `target` rule no longer hides `src/target/` (a new file there was silently left out of a commit)
- **docs:** `docs/nvme-access.md`; README config, ports, exports and files; auth.md points at it (#210)
- **test:** `ci-nvme-hosts-verify.sh`: the Linux kernel as initiator without root — dev's own kernel in QEMU with nvme-cli and the nvme-tcp/nvme-auth modules runs `nvme discover`/`nvme connect` (with and without `--dhchap-secret`) against an engine on dev; 13 checks (#210)
- **test:** The 25 in-process integration-test files are one test binary, `tests/it` (each file a module), instead of 25 binaries that each linked the whole engine. Run with cargo-nextest for per-test processes (#209)
- **test:** Runtime tests move to the `tests-runtime/` workspace crate, outside the routine check: `integration_boot_local`, `integration_flowover_resume`, `integration_image`, `integration_slab_volumes`, `integration_engine_e2e`, `iscsi_blockdev`, `external_iscsi` and `ublk_resize`. They drive the built binary through `STORMBLOCK_BIN`, kernel devices or privileges
- **refactor:** `src/main.rs` is a wrapper around `stormblock::cli::run`. The command line moved into the library, so it is compiled and tested once
- **docs:** The routine check is `cargo nextest run` (the crate has no doc tests). The RouterOS check is dropped (nothing ships for RouterOS)
- **build:** Build settings for everyday work (#209). `dev`/`test` keep line tables but not full debug info, and dependencies carry none. `release` is thin LTO across 16 codegen units. Fat LTO with one codegen unit moves to a `dist` profile that only golden builds use. The routine check is `cargo test`
- **build:** One TLS crypto backend, ring. rustls's defaults and hyper-rustls's `aws-lc-rs` feature were compiling aws-lc-sys, a large cmake C build
- **BREAKING:** `cluster` (openraft) is no longer a default feature; build with `--features cluster` for it. The engine is standalone-first and no stormcos node enables Raft. The feature pulled openraft, chrono, rust_decimal, borsh and thiserror 1.x into every build
- **build:** socket2 0.6, the version tokio already uses
<!-- New unreleased changes go here -->

### 2026-09-27 (#148)
- **feat:** boot intent beside `boothost/<name>` (stormbootx#11):
  `GET /api/v1/synonyms/boothost/<name>/intent` (open, like the claim; name or
  alias, a MAC's 12 hex digits included; 404 for a machine nobody knows) and
  `PUT …/intent {"intent": "install"|"local"|"auto"}` (admin token when one is
  configured). Kept on the host record, so a rename carries it; shown in
  `/api/v1/boothost`.
- **feat:** `install` is one-shot. A claim under it answers `intent: install`
  and records its boot clone; `boot-claim` writes `/run/stormblock/install.json`,
  the initramfs survey then takes the local disk with force (unless
  `rd.stormblock.assimilate=off`), `boot-local` carries it in the handover
  record, and the adopting engine, after the flow-over and local boot succeed,
  posts `POST …/boothost/<name>/installed {"volume"}` (open) — which sets the
  intent to `local` only for the clone claimed under the install.

## [v19.4.0] — 2026-09-27

### 2026-09-27 (#200)
- **feat:** universal boot. A claim of `boothost/default` carries the machine's
  first NIC MAC (`?mac=` or body `{"mac": …}`) and gets a copy-on-write clone
  of the default release for that machine alone: the MAC resolves to the host
  it is an alias of, or to the provisional host `mac-<12 hex>` (made on the
  first claim, MAC as its alias), which is pinned to the default and gets its
  own sealed `hostgolden/mac-<hex>`. Same MAC, same golden; naming is the #199
  rename, and the golden is kept. Response `host` gains `provisional`, `mac`,
  `new`.
- **fix:** a claim of `boothost/default` without a MAC (or with one that is
  not a unicast MAC) is a 400. It used to be tag `default`: one boot clone
  for every machine, each claim releasing the clone the previous machine was
  running on.
- **feat:** `GET /api/v1/boothost?unnamed=1` lists the machines that booted
  the default and are still called `mac-<hex>`; every host reports
  `provisional`.

### 2026-09-27 (#199)
- **feat:** boot hosts are known by their DNS name, with their SMBIOS serial
  and MACs as aliases. A claim of `boothost/<alias>` is a claim of the host it
  belongs to, so agents claiming by serial keep booting the same image; the
  response carries `host {name, claimed_as, aliases}`. Resolve, re-point and
  rollback in the `boothost`/`hostgolden` namespaces accept an alias too.
  - `/api/v1/boothost`: `GET` (every host: name, aliases, former names,
    assignment, host golden), `GET /{name|alias}`, `PUT /{name} {aliases}`,
    `POST /{name}/rename {to, keep_alias?}`. Token required, like everything
    but the claim.
  - A rename moves `boothost/<old>` and `hostgolden/<old>` with their versions
    and history, keeps the old name as an alias (unless `keep_alias: false`),
    and remembers it, so boot clones and host goldens made under it are still
    released and collected (#127).
  - Two hosts never share an alias or a name: setting one, renaming onto one,
    or creating `boothost/<x>` where `x` is another host's alias is a 409 naming
    both. Nothing becomes an alias by itself (MicroCloud nodes share a chassis
    serial). Matching is case-insensitive, and a MAC matches in any spelling.
  - Hosts are stored in `synonyms.json` (`hosts`); a file without it still
    loads.

### 2026-09-27 (docs checked against the code)
- **docs:** README, `docs/` and CLAUDE.md checked against the code for
  everything since 2026-09-18 (config, CLI, env, routes, auth, ports,
  metrics, shipping, design docs).
  - README: `pallet` has 22 actions; `golden --whiteouts/--fsck` are on by
    default and take a value; the host NQN on a flow-over resume; the route
    table lists every mounted surface (volumes, `/v1`, `/serve/v1`,
    discovery, cluster, `/raft`, `/apis` discovery, `/ui`), and placement is a
    field, not a route; the probes are open besides the boot claim; the token
    file falls back to `/etc/stormblock` only if that directory exists; the
    metrics list; what ships outside stormcos (`systemd/`, `deploy/`,
    `scripts/`, the stale Dockerfiles).
  - `docs/durability.md`: rules 8 (handover order) and 9 (flow-over resume)
    back in the list, with their tests; the on-metal result (#171).
  - `docs/multi-drive.md`: drive identity is #136 (not #140).
  - `docs/metadata-scale.md`: the pending set and the per-persist slab sync
    since #171.
  - `docs/images.md`: `STORMBLOCK_RESUME_SOURCE`. `docs/composed-disks.md`:
    the four compose routes. `docs/layering.md` and a comment in
    `src/fs/template.rs`: no clone is parked in advance (#137).
    `docs/presentation.md`: the on-metal result; the golden-release
    decision (#194). `docs/auth.md`: token-file fallback.
  - `docs/pallets.md`: the container design is tracked by #59 (closes #178).
  - CLAUDE.md: #171 closed on metal; #172 is the flow-over check; #194.
- **chore:** `tmp/` ignored; a committed `.terragrunt-cache` untracked.
- Filed: #196 (stale Dockerfiles), #197 (`STORMBLOCKMK_*` names in messages
  nothing reads; `stormblock_allocated_bytes` never set).

### 2026-09-27 (docs refresh)
- **docs:** refreshed from the code for everything since the #131 rewrite
  (v19.1.3 → v19.3.0).
  - README:
    - `adopt-ublk` stands the initramfs engine down and waits for it to exit
      before reading the slabs;
    - the new `STORMBLOCK_BOOTHOST`, `STORMBLOCK_BOOT_TAG` and
      `STORMBLOCK_RESUME_SOURCE`;
    - a flow-over cut short resuming from a fresh clone;
    - the per-export portal cap (128, #188);
    - the ublk unit's short stop timeout (#187);
    - known gaps (#96, #98, #175);
    - `docs/durability.md` in the docs table;
    - current sizes (93k/14.5k lines, ~880 tests, the `test/` crate).
  - `docs/images.md` §2b: a flow-over cut short.
  - `docs/presentation.md`: v19.3.0, #171's status, the power-cut and test
    container proofs.
  - CLAUDE.md: the known-red tests (#134, #173), how to run the test
    container, the new modules (`crashdev`, `handover::take_over`,
    `open_slabs_resuming`), current state.

## [v19.3.0] — 2026-09-27

### 2026-09-27 (test container)
- **feat(test):** stormblock's test container under stormcentral's test
  standard (#139): `test/` (crate `stormblock-test`, a workspace member),
  `test/build.sh`, `test/Containerfile` (`FROM scratch`, 30 MB) and
  `test/stormblock-test.yaml`, with `short`, `medium` and `long` suites.
  - It runs the engine of the same commit in the pod, unprivileged, and
    checks data over NVMe/TCP with the engine's own initiator.
  - It found the `adopt_slabs` restart loss fixed in 31bf732 (#171).
  - See README, "The test container".
- **fix(nvmeof):** concurrent attaches could share one namespace. Attaching
  a volume over NVMe/TCP picked the lowest free NSID and added the namespace
  in two steps, with nothing held between them. Two attaches at once got the
  same NSID; the second replaced the first, so both volumes' attach URIs named
  one namespace and each read the other's writes. The long suite found it: 29
  of ~50,000 volume cycles read a sibling's data, and one write failed with
  NVMe status 0x16. `NvmeofTarget::add_namespace_next` now picks and inserts
  under one lock (the export path too), and `ensure_nvme_namespace` holds the
  `/v1` state lock from check to record. NSID *reuse* after a detach is still
  #96.

## [v19.2.2] — 2026-09-27

### 2026-09-27 (flow-over cut short)
- **fix(boot-local):** a power cut in the middle of a flow-over no longer
  bricks the node (#171). The local records still named extents on the old
  appliance clone's slab. The next boot claimed a new clone, so that slab was
  "not attached": every unmoved extent was dropped (9087 on C2NR0Q2), the
  erofs root came up with holes, and the boot stopped at "Failed to mount
  root".
  - A fresh clone of the same sealed image carries the same slabs, by id,
    with the same bytes: a clone restamps the GPT, never the slabs.
  - `boot-local` now claims a clone when the local records name a slab that
    is not here. It attaches the missing slab for its data only; the clone's
    records are the image's and are never read. It then restores onto it.
  - The handover record names the clone and a flow-over into the local
    slabs, so the successor finishes the move.
  - The initramfs exports the appliance it found (`STORMBLOCK_BOOTHOST`), and
    the engine reads the machine's tag from SMBIOS (`STORMBLOCK_BOOT_TAG`
    overrides it).
  - Only `boot-local` does this: `must-gather` never claims, because a claim
    releases the machine's earlier clones.
- **test:** `tests/integration_flowover_resume.rs`: an image, a byte copy
  standing in for the next boot's clone, and a local slab half flowed over.
  `boot-local --check` drops nothing when given the clone
  (`STORMBLOCK_RESUME_SOURCE`), and without one it says the extents are
  missing.

## [v19.2.1] — 2026-09-26

### 2026-09-26 (handover order)
- **fix(adopt-ublk):** the successor reads the slabs only after the incumbent
  is gone (#171). `adopt-ublk` used to restore from the slabs first and stand
  the incumbent down after, so everything the incumbent allocated in between
  was missing from the successor's map. Its slots looked free and could be
  handed out again, and the incumbent's shutdown rewrote slot-table sectors
  from its own older copy. Nothing showed until a restore after a power cut.
  Now: SIGTERM, the devices quiesce, and the incumbent's process is waited
  for (a zombie counts as exited; SIGKILL after 30 s), then restore. ublk
  recovery holds the devices' I/O in the gap. `handover::take_over` pins the
  order, and `ublk::stand_down` now returns the pids it signalled.
  - Cost: a handover now waits for the incumbent to exit, which was measured
    before at up to ~15 s of boot.
  - If the successor then fails to restore, the devices stay held in recovery
    with no server: loud, and retryable.
- **test:** `tests/integration_handover_order.rs`. An incumbent allocates
  inside the window, flushes and exits; the successor maps the allocation and
  does not hand the slot out again. A second test shows the old order missing
  it.

### 2026-09-26 (adopting slabs)
- **fix(volume):** a daemon restart lost data that had been flushed (#171).
  `adopt_slabs`, which takes over the slabs on the drives at startup, mapped
  extents from the slabs' records alone. The records are rewritten only on
  persist, so everything allocated since the last one came back missing. It
  also adopted one drive at a time, which dropped a volume's legs on every
  drive after the first. It now reconciles the records with the slot tables
  the way `restore` does (same helper), and the daemon adopts every drive's
  slabs in one call. Found by the new medium test suite (#139): 4 MiB written
  and flushed, 2 MiB back after a restart. `boot-local` and `adopt-ublk` use
  `restore` and were not affected.

## [v19.2.0] — 2026-09-26

### 2026-09-26 (VM snapshots)
- **feat(v1):** `POST /v1/snapshots` and `POST /v1/group-snapshots` take engine
  volumes made through `/api/v1`, by id or name (#130, stormvm#28). Before,
  only `/v1` volumes were accepted, and every disk stormvm gives a VM got
  a 404. A group is one consistency point across its members, through the
  same fence as before: each member is a copy-on-write snapshot, sealed, and
  no GPT or filesystem identity is restamped. A restore is `POST /v1/volumes`
  with `source: {kind: snapshot}`.
- **fix(v1):** a snapshot is `ready` only where this node holds the data. A
  snapshot of a volume mastered on another node used to say `ready: true`
  with nothing copied. A group is ready only when every member is.

## [v19.1.4] — 2026-09-26

### 2026-09-26 (power cut)
- **fix(durability):** writes a consumer fsync'd could be lost on a hard power
  cut (#171; fastetcd's redb: "All roots are corrupted" after every power-off).
  - A slot's table entry was written before its data was durable. It is now
    published at the next flush, after the data.
  - A freed slot could be reused before its free was durable. It now waits.
  - A first write left the rest of its slot holding the previous tenant's
    bytes. It now fills the slot with zeros.
  - `restore` mapped a stale record's slot even after the slot was freed or
    reused. It now drops those mappings, and it raises share counts to the
    mappings it restored.
  - Persist now flushes the slabs before writing records.
  - Discard now leaves shared extents alone.
  - ublk WRITE_ZEROES was a discard that swallowed errors. It now writes zeros
    and reports EIO.
  - See `docs/durability.md`.
- **test:** `drive::crashdev::CrashDevice`, a device with a volatile write
  cache. `tests/integration_power_cut.rs` runs 300 randomized cuts, plus a
  stale record with several copy-on-write generations. Before the fix, 182 of
  the 300 lost acknowledged data.

## [v19.1.3] — 2026-09-26

### 2026-09-26 (boot claim)
- **fix(synonyms):** a boot claim (`POST /api/v1/synonyms/boothost/<tag>/claim`)
  now releases **every** earlier clone named `boothost-<tag>`, not only the
  first one `find_volume` met (#127). Forge had 60 of them. A claim used to
  release at most one, so with several already there, or with the one it
  found inside the double-claim grace, the rest stayed for good. Each earlier
  clone still passes the same guards: not claimed within the grace (this
  boot's), not named, not sealed, and a clone of something the tag has booted.
  Its export is dropped first. The first claim after this upgrade collects an
  existing pile. The response gains `released` and `kept`.

### 2026-09-26 (presentation)
- **docs:** `docs/presentation.md` is an 11-slide Marp deck of stormblock's
  purpose and functionality (#132). It covers the problem it solves, where it
  sits in stormcos (from stormcentral's relationships graph), how it works,
  what it does today, its interfaces, how it ships and runs, what is proven
  and by which test, what is planned, and the issues that matter. It is drawn
  from the README rewritten in #131. Render it with
  `npx @marp-team/marp-cli@4 docs/presentation.md -o out/presentation.html`.

## [v19.1.2] — 2026-09-26

### 2026-09-26 (docs from the code)
- **docs:** the README is rewritten from the code (#131). It covers:
  - where the engine runs (a stormcos node runs `adopt-ublk` under stormpump;
    the initramfs runs `boot-claim` / `boot-local`; appliances run the daemon);
  - PVCs on stormcos as the built-in driver, with CSI only for third-party
    drivers;
  - every daemon flag, environment variable and config key with its default;
  - ports, probes, metrics, the API surfaces and auth, the files it keeps,
    how it ships as a stormcos component, and a list of what earlier docs
    promised that the code does not do, with issues #159–#170.

  CLAUDE.md's build, platform, architecture and state sections are current,
  and building is `sc-build`, never root.
- **docs:** every file in `docs/` was checked against the code and
  corrected:
  - `pallets.md`: `copy_pallet` keeps boot state; sealed-attach refusal done;
    `/mirrors` and `/resync`.
  - `images.md`: `input`.
  - `composed-disks.md`: `allocated_bytes`; blank names.
  - `auth.md`: `/seal`; `/mk/v1` probes.
  - `boot-hooks.md`: `any` takes system-only drives.
  - `redundancy.md`: port 9090; `items`; V8; `topology_chain`.
  - `multi-drive.md`: the built-in PVC driver; design status.
  - `m0-baseline.md` and its generator, `protocol-overhead.md`, `layering.md`
    (what was built since), `contract/README.md`.
- **docs:** superseded design moved to `docs/history/`, each file with a note
  saying what is different now. That covers the v0.1 spec, the LinuxBoot
  proposal, the placement note and the August deck. The spec's accurate
  StormFS sections became `docs/stormfs-api.md`.
- **docs:** `docs/stormblock-ipxe-boot.md` is removed. It documented a
  `serve-boot` subcommand that never existed and a re-boot path that formats
  the disk (#162), and it contained lab credentials.
- **fix(cli):** `--help` for `must-gather` and `boot-claim` no longer shows
  another subcommand's first line. `ublk` and `boot-local` have their own help.
  The `migrate` stub names routes that exist.
- **docs(code):** `require_auth`'s doc comment says what v17 does (unset means
  required).

## [v19.1.1] — 2026-09-26

### Fixed
- `POST /api/v1/volumes/{id}/attach` with an explicit `nvme-tcp` on a node
  with no NVMe-oF target answers as it did before v19.1.0, instead of the 409
  v19.1.0 introduced there (`integration_synonyms` relies on it). Only `/v1`
  refuses an `nvme_tcp` it cannot give, which is what #149 asked for.

## [v19.1.0] — 2026-09-26

### 2026-09-26 (attach transport)
- **fix(v1):** `POST /v1/volumes/{id}/attach` takes an optional `transport`
  (`nvme_tcp` or `ublk`; `nvme-tcp` and `nvmeof` are accepted too; absent
  means the engine chooses) (#149). Since 2337c8a made ublk the default for a
  local attach, an orchestrator attaching on the master's behalf for a remote
  initiator got `ublk` for every request. That is how stormstorage exports
  each leg of a distributed volume to the RAID head, so assembly failed. With
  `nvme_tcp` the ublk offer is skipped and the NVMe-oF coordinates are
  returned. When there are none (no NVMe-oF target, or the volume is not
  backed here) the answer is a 409 that says which, and the refused
  attachment is not recorded. `ublk` insists, or 409. Additive: callers that
  send no transport are unchanged.
- **refactor(api):** `POST /api/v1/volumes/{id}/attach` parses `transport`
  with the same parser, and now also accepts the spelling `nvme_tcp`. Its
  answers are unchanged.

## [v19.0.0] — 2026-09-26

### Breaking
- `POST /api/v1/arrays` makes a **dedicated** array by default: volumes that
  are not pinned to it no longer allocate on its slab. Send
  `"dedicated": false` for the previous behaviour. `array_id` on a volume
  create now *pins* the volume to the array; before, it was only checked.

### 2026-09-26 (arrays)
- **feat(arrays):** an array created with `POST /api/v1/arrays` is *dedicated*
  by default (#150). Its slab is in the data role, carries its own metadata
  region, and takes allocations only for volumes pinned to it. No general
  placement, drain destination, rebalance, retier or chunk allocation lands
  there. The flag is bit 0 of the slab header's `flags` byte, so it survives
  restarts and adoption. `"dedicated": false` keeps the old general-pool
  behaviour.
- **feat(volumes):** `array_id` on `POST /api/v1/volumes` now pins the volume,
  so every extent is on the array's slab. Before this it was only checked,
  and the extents went anywhere of the volume's role. `placement.array_id` on
  `POST /v1/volumes` does the same and gives the volume a `/v1` identity for
  attach and fence. A pinned volume refuses a full array rather than
  spilling, takes no redundancy policy of its own, and passes its pin to its
  clones. Pins persist as the record's `array_id` and survive restore and
  adoption.
- **feat(arrays):** `GET /api/v1/arrays` and `GET /api/v1/arrays/{id}` name
  the array's slab (dedicated, role, total/free, self-describing) and the
  volumes on it.
- **fix(arrays):** `DELETE /api/v1/arrays/{id}` refused while *any* volume
  existed on the node, and when it did succeed it left the array's slab in
  the registry for the pool to keep using. It now refuses only for volumes
  on that array, and removes the slab with it.

## [v18.6.0] — 2026-09-25

### 2026-09-25 (xfs)
- **feat(fs):** XFS alongside ext4 (#147). `"fs": "xfs"` on a template,
  blank or claim formats with `mkfs-xfs` v0.2.0, the filesystem `mkfs.xfs`
  6.15 writes, from 300 MB. On a thin volume the log it zeroes becomes a
  discard: a 2 GiB XFS blank allocates 6 MiB. A seal checks the superblock
  (CRC, in-progress, needs-repair) and walks the whole tree with `fio-xfs`.
  Every clone and claim is restamped as `xfs_admin -U` does it (new
  `sb_uuid`, old UUID kept in `sb_meta_uuid`, `META_UUID` set, in every AG's
  superblock) and read back. `features`, `journal: false` and `seed` are
  refused for XFS. Volumes record `fs.kind: xfs`, and blanks adopted from a
  slab keep their kind.
- **feat(import):** an import finds the filesystems an image carries (XFS and
  ext2/3/4, the whole volume or each GPT partition), names the OS from
  `/etc/os-release` and walks each tree. `filesystems` appears on the import
  status. A recognised filesystem that does not read fails the import unless
  `"verify": false`.
- **feat(api):** `POST /api/v1/volumes/{id}/fsck` on an XFS volume walks it and
  reports; `repair=true` is refused for XFS.
- **test:** `ci-xfs-verify.sh` runs on dev. An engine-made XFS blank and two
  claims pass `xfs_repair -n`, `blkid` reports each UUID the engine recorded,
  and `xfs_db` shows each claim's metadata UUID is still the blank's. A Rocky
  9 cloud image imports through the served API and its XFS root passes
  `xfs_repair -n`. `examples/xfs_verify.rs`.
- **build:** `mkfs-xfs` and `fio-xfs` v0.2.0 (git tags); `Cargo.lock` gains
  only those two.

## [v18.5.0] — 2026-09-25

### 2026-09-25 (rebuild)
- **feat(rebuild):** a failed drive's volumes are rebuilt without anyone
  asking (#146). A drive health report of `degraded`, `failing`, `failed` or
  `missing` queues every redundant volume with a member on the drive. There is
  one queue for the node, most endangered first (by the new `margin`).
  `parallel` volumes rebuild at once and `extents_in_flight` extents of each at
  once, under one node-wide `max_bytes_per_sec` budget. The map is made durable
  every 4096 legs before the slots it replaced are freed. A volume hit again
  mid-rebuild is rebuilt once more. `failed` and `missing` drain the drive
  after the rebuild instead of alongside it.
  `GET/POST /api/v1/rebuilds`, `GET/DELETE /api/v1/rebuilds/{id}` and
  `PUT /api/v1/rebuilds/settings`; `[rebuild]` in the config.
  `placement.rebuild` reads `queued` or `running`. A manual resync of a volume
  the queue holds, or a drain of a drive whose rebuild is running, is refused
  (409).
- **feat(volume):** `margin` on a volume's health: how many more member
  losses its least protected extent or stripe can take.
- **fix(volume):** a resync published its rebuilt legs only at the end,
  after each extent's lock was released. A write in between reached the
  surviving legs and not the new one, which then served stale data. An
  extent only this volume maps is now published under its lock. Shared
  extents, which are never written in place, are published together at the
  end and redone under the lock if they stopped being shared.
- **fix(volume):** a failed drive ruled out its whole domain at the volume's
  rung, so a `mirror:2@shelf` or `raid5@shelf` volume could not be rebuilt
  after a drive failed: the other drives of that shelf were the only place
  the leg could go. New extents of such a volume could not be placed either.
  A failed slab now keeps out only its own drive, and a dead member holds no
  domain in its stripe. The multi-drive test only passed because a drain
  had moved the legs first.
- **fix(volume):** a resync freed the slots it replaced before the map
  naming their replacements was on disk. It now frees them after the map is
  persisted.
- **perf(volume):** a resync visits only the extents and stripes that need
  work, publishes each rebuilt leg into its own map instead of sweeping
  every map on the node (the sweep ran once per parity stripe), and can copy
  several extents at once.
- **feat(examples):** `rebuild_rate` measures rebuild throughput at several
  `parallel` and `extents_in_flight` settings, on O_DIRECT or the page cache.
- **docs:** `docs/redundancy.md` gains "Rebuilding after a failure"; the
  failure-domain rule for failed drives; `docs/multi-drive.md`'s drive-life
  table. Follow-ups filed: #159 (erasure coding k+m) and #160 (scrub, an
  owner decision).

## [v18.4.0] — 2026-09-25

### 2026-09-25 (metadata at scale)
- **perf(slab):** a slab finds its first free slot through a per-chunk free
  count (`drive/freemap.rs`) instead of scanning the bitmap from slot 0 on
  every allocation. It is still first-fit with the same answer. On 4 M slots,
  allocation now holds at ~27 µs a slot from empty to 90% full; before, it
  climbed from 25 to 168 µs (#145).
- **feat(examples):** `metadata_footprint` measures the resident bytes per
  free slot, per allocated slot and per GEM extent, and the allocation cost as
  a slab fills, then extrapolates to a 256 TB drive and a 41 PB node.
- **docs:** `docs/metadata-scale.md` covers allocation metadata at 40 PB a
  node (#145). Measured today: ~41 B a free slot and ~326 B a full one, which
  is ~80 GB for a full 256 TB drive at 1 MiB and ~313 GB per PB. It proposes
  a budget, per-class extent sizes, compact resident records, a paged and
  incrementally persisted extent index, and 64-bit indexes in one format
  change. The work is split into #155–#158 and stormcos#92.

## [v18.3.1] — 2026-09-25

### 2026-09-25 (multi-drive)
- **docs:** `docs/multi-drive.md`, the multi-drive design (#142). It covers
  implicit pools per role and tier, placement per extent, the failure-domain
  chain with stormdrive's shelf and bay, a drive's life (added, drained,
  failed), overcommit, and what the console shows. Each part says what exists
  and what is missing, and the work is split into #146, #151–#154,
  rustkube-node#71, stormblock-csi#21 and stormconsole#29, with three
  decisions for the owner.
- **fix(drain):** a drained leg keeps the volume's own spread rung. The drain
  kept legs apart only at `drive`, so a `mirror:2@shelf` leg moved off a failed
  drive could land in its sibling's shelf, and the volume still read healthy.
  A moved leg also no longer crosses the system/data boundary (#88): neither
  destination picker checked the slab's role.
- **test:** `tests/integration_multidrive.rs` runs four drives in two shelves
  over the API: spread, `mirror:2@shelf`, a refused `mirror:3@shelf`, a failed
  drive quarantined, drained and rebuilt around, and a drive added.

## [v18.3.0] — 2026-09-25

### 2026-09-25 (blanks)
- **fix(fstemplates):** a size-class blank can no longer be left in
  `awaiting_format` (#141). `POST /api/v1/fstemplates` runs its format and seal
  on a task of its own, so it finishes or rolls back even when the caller stops
  waiting. The 1 TiB class minted for a 600Gi claim was abandoned that way. A
  template records that the engine is formatting it (`formatting`, persisted
  and reported), and at startup the engine finishes any format a previous run
  left: it discards the raw volume to zeros, formats and seals, or rolls the
  template back on failure. Measured on dev, a 1 TiB blank formats and seals
  in 5–7 s on the served engine.
- **fix(volume):** a volume with no extents yet keeps its half across a
  restart. Both the data-directory restore and slab adoption made every such
  volume `System`. On a data-only node that made it unwritable ("no system
  slab"). On a node with both halves it placed an unwritten data volume, such
  as a fresh PVC, in the half an install replaces. Its role now comes from the
  slab whose metadata records it, else from a half the node actually has.
- **test:** `ci-template-resume.sh`, a 1 TiB blank on the served engine: the
  caller gives up, and the engine is killed mid-format and restarted.

## [v18.2.0] — 2026-09-25

### 2026-09-25 (block devices)
- **change(drive):** no file I/O for real storage (#140). The installed
  disk's slabs, the flow-over disk, local boot, the `slab`/`image lay-node`/
  `local-boot` CLIs, the slabs API, `image build` onto a device and its image
  sources all open a block device through `drive::open_path`: `O_DIRECT`, as the
  drive it is, never `FileDevice`. `FileDevice` is for tests and development.
  It warns when opened on a block device, and the node warns on every boot
  when a slab sits in a regular file. Slabs laid through `FileDevice` reopen
  O_DIRECT as they are, with nothing moved.
- **perf(drive):** the raw block device (`SasDevice`) is asynchronous. Its I/O
  runs on an io_uring on its own thread (eventfd-woken, many requests in
  flight, every buffer owned by its operation), or on `pread`/`pwrite` in the
  blocking pool where io_uring is unavailable (RouterOS). It used to hold a
  lock across `submit_and_wait` on the executor thread: queue depth one per
  drive, and a runtime worker blocked for every disk operation. Requests that
  are not whole logical blocks are read-modify-written by the device instead
  of refused.
- **fix(drive):** `DmaBuf::alloc(0)` asked the allocator for a zero-size
  layout, which is undefined behaviour.
- The drive-identity half of #140 (`drive=<serial>`, one disk one domain)
  shipped in v17.1.0 with #136.

## [v18.1.0] — 2026-09-25

### 2026-09-25 (volumes view)
- **feat(volumes):** every volume says what it is and whether something uses it
  (#138, #126). New fields: `kind`
  (`volume`/`golden`/`blank`/`media`/`snapshot`/`template`), `in_use` and
  `attachments` (ublk with its mount point, adopted boot devices, NVMe
  namespaces and per-volume subsystems, serve wiring, exports, iSCSI LUNs),
  and `consumer` (the owner, else the mount). They appear on the listing and
  on the single GET. The listing takes `?kind=` (with `image` and `all`),
  `?in_use=` and `?unowned=true`, the last documented since #115 and never
  implemented. With no filter the listing is unchanged.
- **fix(volumes):** a volume attached on the shared NVMe subsystem, and a boot
  device an adopting engine serves, now count as in use. Neither did before,
  so the delete guards and the template sweep could act on a volume a host was
  using.

## [v18.0.0] — 2026-09-25

### 2026-09-25 (claims)
- **BREAKING (fstemplates):** standby clones are retired (#137). A claim mints
  its clone now. `GET/POST /api/v1/fstemplates/standby` and
  `POST /api/v1/fstemplates/{id}/standby` are gone, a claim's response no
  longer carries `from_standby`, and nothing is minted after a seal or a
  clone. On startup a node deletes the clone each template recorded as
  standing. Only that one is deleted, because a claimed clone kept its
  `standby-…` name.
- **perf(fstemplates):** a mint writes the volume metadata once instead of
  twice (`create_snapshot_deferred`, `set_fs_info_deferred`). A clone of a
  sealed source is verified by reading back the superblock just stamped
  rather than by an fsck, since the sealed blank was checked at seal and
  cannot change. A full check still runs for an unsealed source, and on
  demand through `POST /api/v1/volumes/{id}/fsck`. Measured on dev over HTTP,
  a claim went from 345–768 ms to 173–246 ms median with no standby
  volumes, and in the library a mint is the snapshot (<1.3 ms) plus one flush.
- **test:** `examples/claim_timing.rs` (per step) and `ci-claim-timing.sh`
  (over HTTP: clone, claim, export) measure it.

## [v17.1.0] — 2026-09-25

### 2026-09-25 (placement)
- **feat(volumes):** `placement` on a volume (#136, #114). It lists each slab
  holding a leg, with its role, tier, domain, drive (serial, WWN, model,
  path), node, state (`ok`/`failed`/`quarantined`/`draining` with progress/
  `missing`), legs and bytes. It also groups them per drive, gives the leg
  totals and whether a rebuild is owed, and lists each drive-level RAID
  array's members with their state. It is always on
  `GET /api/v1/volumes/{id}`, and on the listing with `?placement=true`.
  `array_id` is filled when a volume is on one array.
- **feat(volumes):** the listing carries a `generation`, bumped on every
  metadata persist. `?since=N` or `If-None-Match` answers 304 when nothing has
  changed.
- **feat(drives):** drives know who they are. `FileDevice` and the SAS backend
  read serial, model and WWN from sysfs, and a partition resolves to its disk.
  `DeviceId` gains `wwn`, and `/api/v1/drives` and every slab
  (`/api/v1/slabs` → `drive`) report it.
- **fix(placement):** two slabs on one drive are one failure domain. A slab in
  a partition had the domain `drive=file+<offset>`, so two partitions of one
  spindle counted as two drives and a mirror's legs could share it. Domains
  and drive labels now key on the drive (`BlockDevice::drive_id`).

## [v17.0.1] — 2026-09-25

### 2026-09-25 (snapshots)
- **fix(v1):** a `/v1` snapshot, which is what a Kubernetes `VolumeSnapshot`
  becomes through stormblock-csi, is **sealed** at creation, and so is every
  member of a group snapshot (#111). It was an ordinary writable engine volume:
  anything that attached it read-write could change what the snapshot held.
  It is now a golden: immutable, carrying lineage to its source, and what a
  restore clones from. This answers #111. CSI snapshots map onto goldens and
  CoW clones and are not a separate story.

## [v17.0.0] — 2026-09-25

### 2026-09-25
- **BREAKING (auth):** the management API is **closed by default** (#107).
  `management.require_auth` unset now means required. The node mints a token
  into `management.token_file` (default `<data_dir>/api_token`), and with
  nowhere to keep one it closes with an in-memory token rather than falling
  open. `require_auth = false` is the one way to open a node. Every client that
  calls a node's API must present the token. Most outside clients send none
  today; issues are filed on each (see docs/auth.md).
- **feat(synonyms):** the boot claim is the one open write, and each machine
  boots from a sealed golden of its own (#107, owner decision 2026-09-25).
  `POST /api/v1/synonyms/boothost/<tag>/claim` needs no token and takes no
  options. It clones the tag's own `hostgolden/<tag>`, a sealed CoW clone of
  its assignment. A tag seen for the first time is pinned to
  `boothost/default`. Every boot is a fresh clone with the previous one
  released. A re-point makes a new host golden and collects the old one once
  nothing is cloned from it.
- **fix(cluster):** heartbeat, join and Raft RPCs present the cluster's shared
  token. They sent none, so a cluster whose nodes required a token could not
  talk to itself.
- **feat(auth):** the CLI reads a local engine's token
  (`$STORMBLOCK_TOKEN_FILE`, `/etc/stormblock/api_token`,
  `/var/lib/stormblock/api_token`) for `image build --engine
  http://127.0.0.1:…`, and never presents it to another host.
- **chore(ci):** the `ci-*.sh` scripts give their engines a token and present
  it. The benchmarks take `$STORMBLOCK_API_TOKEN`. The new `ci-auth-verify.sh`
  checks the served binary: closed by default, and the boot claim open.

## [v16.2.0] — 2026-09-24


### 2026-09-24
- **feat(image):** an installed disk boots on its own (#123). A flow-over
  leaves a boot area in front of the system slab. Once the goldens have moved,
  the adopting engine copies the attached image's ESP (stormuefi) and its
  `kind = boot` pallets into it. Local boot pallets form the same A/B ladder an
  upgrade uses: newest at priority 14, the previous one at 13, older ones
  removed. The ladder stays one below an attached image's 15, so a netbooted
  release still wins. Handover records carry `local_boot`. New
  `stormblock image lay-node` and `stormblock image local-boot` do the same by
  hand. Verified by `ci-local-boot-verify.sh`: OVMF boots the disk alone,
  stormuefi selects the newest local release and the kernel starts.
- **fix(image):** the node disk's GPT is written in the drive's own logical
  sector size (`BLKSSZGET`), not `FileDevice`'s 4096. On a 512-byte drive a
  4096-byte table is invisible to firmware. An installed disk laid the old way
  has its table re-expressed at the native size on the next install, with every
  partition at the same bytes; the system half gives up its front for the boot
  area.
- **feat(image):** `fat::read_tree` and `fat::format_from_tree`, so an ESP
  served at one sector size can be laid onto a drive of another. `PartitionDevice::with_block_size` presents a
  window at the medium's sector size.
- **fix(image):** the FAT writer could size a FAT one sector short of its
  cluster count (64 MiB at 512-byte sectors: 129024 clusters, room for 129022
  entries), and every subdirectory's `..` named the root rather than its
  parent. Both were found by `fsck.fat`.
- **fix(ci):** `ci-image-verify.sh` finds the GPT at the LBA size the builder
  writes, rather than assuming 512. Since stormcos#31 an image is 4096, and the
  script had stopped at its table check, before it ever reached mtools.
- **build:** `Cargo.lock` is committed (#128). It was in `.gitignore`, so a
  golden built `--locked` from a commit had no lockfile, and the builds that
  worked were using an untracked one on dev. Generated on dev with cargo 1.95.0;
  `cargo update` is now a deliberate commit.
- **fix(ublk):** stopping a device keeps its queues served until STOP_DEV
  returns. The workers were told to exit first, so the kernel's writeback of
  the device during teardown was never answered: STOP_DEV never returned, the
  device stayed with writes in flight, and every `sync` on the node hung
  (reproduced on the R230 step by step: `inflight 0 6`, an engine thread in
  `submit_bio_wait`, after a claim's detach).
- **fix(volumes):** detaching a ublk device that has a filesystem mounted on it
  is refused (409, naming the mount point). The devices are recoverable, so a
  detach under a mount left the kernel queueing that filesystem's I/O for a
  server that never returned; every `sync` on the R230 then hung in
  `submit_bio_wait`, and the node could only be power-cycled.

### 2026-09-23 (PVC templates)
- **feat(fstemplates):** `POST /api/v1/fstemplates` takes `role` (`data` or
  `system`), and the template's volume, and so every clone of it, lives in that
  half of the node's storage. A PVC blank minted on demand landed on the system
  half, which every install replaces, and its claims share the blank's
  unwritten extents.

## [v16.1.0] — 2026-09-23

### Changed

- **BREAKING (node disk layout):** `lay_node_slabs` puts the system slab first,
  at a fixed size (a sixteenth of the drive, 32 to 128 GiB), and the data slab
  **last, taking the rest of the drive**. The data half is the one that fills
  up (PVCs, VM disks, images), and the last partition is the only one that can
  grow into space the drive gains. On the R230 that is 116 GiB for goldens and
  about 1.7 TB for data, where it was 1.8 TB and 64 GB. Existing disks keep the
  old layout until reinstalled; `node_layout` finds both halves by type,
  whatever their order. `LocalLayout.data_bytes` is now `system_bytes`.
- **feat(slab):** a slab can grow in place. Header bytes 120..124 record the
  slot-table room (0 = exactly `total_slots`, so old slabs read unchanged), and
  `SlabFormat::with_growth` reserves it. The data slab reserves 4x, which costs
  0.024% of it. `Slab::grow` extends into the device's current length with a
  header write; nothing moves.
- **feat(boot-local, cli):** `image::local::grow_data_half` extends the last
  partition to the end of the drive, rewrites both table copies, then grows
  the slab (table first, so an interruption leaves a partition longer than its
  slab, which the next call fixes). `boot-local` runs it on a local node disk
  before opening it; `stormblock slab grow <disk>` runs it by hand.

## [v16.0.1] — 2026-09-23

### Fixed

- **fix(initramfs):** the assimilate survey takes a drive whose slabs are all
  system-role. Identity (the CA key and the ServiceAccount signing key) lives
  only in a data slab, and `boot-local`'s own guard already lays fresh slabs
  over such a drive, so only the survey refused. It refused on every boot: the
  R230's 2 TB disk carries one empty system slab across the whole disk, left
  by an older flow-over. The node therefore claimed a fresh appliance clone at
  every boot, and nothing written to a `-data` volume survived a reboot
  (#118). The survey block is now covered by `tests/initramfs-boot-hook.sh`.
- **fix(boot-local):** a data half laid this boot is seeded, and its records
  land on it. The laid slabs become metadata slabs, first, so each volume's
  record is written beside its extents instead of only onto the appliance
  clone that the next boot claims afresh. That gap was why seeding was off
  everywhere: the adopting engine found every data volume missing. Seeding a
  data slab *kept* from an earlier install is still `STORMBLOCK_SEED_DATA`
  only, because adopting its records over the fresh clone's volumes is the
  unbuilt upgrade path. `STORMBLOCK_NO_SEED_DATA` turns it off.
- **fix(adopt-ublk):** the engine that adopts a flow-over boot puts the laid
  drive's slabs first among its metadata slabs too, so a volume created at
  runtime and not yet written is recorded on the local disk, not only on the
  appliance clone.
- **fix(boot-local):** seeding persists every 64 extents, not after each one.
  Per extent it was 3301 whole-map writes to four metadata slabs, two on a
  spinning drive: 382.6 s on the R230. A source slot is still freed only after
  the map that stopped naming it is durable.
- **fix(initramfs):** the wait for the root device ends when the root appears
  or the engine exits. The deadline (now 30 min) only bounds an engine that is
  alive and stuck. The 300 s deadline gave up on a first boot one minute
  before its seeding finished, and PID 1 sat in a shell while every device
  came up behind it.
- **fix(ublk):** devices declare a volatile write cache
  (`UBLK_ATTR_VOLATILE_CACHE`), so the kernel sends FLUSH and FUA becomes
  write-plus-flush. They were declared write-through, so `fsync` in a guest
  filesystem never reached the engine's `sync_all`, while slab writes sat in
  the page cache and the drive's write-back cache. Once a node's data survived
  a power cycle, fastetcd's redb came back "All roots are corrupted".
- **fix(metadata):** `owner`, `Owner.namespace` and `Owner.uid` are always
  written. `skip_serializing_if` on a bincode-encoded field writes less than
  the decoder reads, so any volume without an owner made the whole
  `volumes.dat` undecodable (`UnexpectedEnd { additional: 1 }`). It went
  unnoticed because the test target had stopped compiling (#115's fixtures),
  and because a node that re-clones every boot never reads back what it
  wrote. The fixtures are fixed too.
- **fix(drain):** a drain frees the slots it has moved off. Since a
  migration's source slot became *owed* until the map is durable (b270bfd),
  the drain persisted and never paid, so a drained slab kept every slot and
  never emptied. It now releases after each persist, on cancel, and at the
  end. The placement tests that asserted an immediate free now pay first.

## [v16.0.0] — 2026-09-09

### 2026-09-08 (boot testing)
- **fix(initramfs):** the local-slab probe requires every volume the command
  line mounts, not just the root. A flow-over moves the *system* half and
  deliberately leaves the data half where it is, because migrating a slab that
  is being written corrupts it — so a drive part-way through has `stormpump`
  and every golden on it and none of the writable volumes. Answering on the
  root volume alone declared that bootable: the node attached its own disk,
  restored 75 volumes, dropped 5712 extent mappings pointing into the
  appliance's slabs, and died on `volume 'stormcert-data' not found in slab
  metadata` after listing the seventy-five it did have.
- **fix(slab list, slab volumes):** a disk whose *partitions* are slabs is a
  slab disk. The boot path has walked the GPT for a long time — it is how
  `rd.stormblock.slab=/dev/sda` works on a composed disk — and these two did
  not, so the same drive gave two answers depending on which asked. A node
  that had just migrated 4399 extents onto its own drive read `/dev/sda is not
  a slab` from its own boot probe, went back to the appliance, and left every
  one of them unused. The drive survey read the same answer and offered the
  drive up to be taken again.
- **fix(flow-over):** the engine that adopts the boot finishes the migration,
  and the initramfs engine no longer starts one it cannot finish. Laying the
  slabs is fast and bounded; copying the goldens onto them is minutes. The
  process doing the copying had seconds to live — twenty-six of them on this
  hardware, between laying the slabs and the successor adopting its ublk
  devices, with `switch_root` having already deleted the filesystem its binary
  came from. Every run was killed part-way, leaving a slab that is real,
  incomplete and unable to boot the node: exactly the shape the local-slab
  probe has to reject on the next boot. The boot lays the structure and writes
  down what it laid; the long-lived process does the long-running job.
- **fix(handover):** the record is written *after* the flow-over and names the
  disk it laid. Written before, it listed only the appliance's slabs, so the
  engine that took the devices over never learned there was a local disk at
  all — by 380 milliseconds.
- **feat(boot-local):** `--local-disk-force`, and `rd.stormblock.assimilate=force`
  in front of it. The identity guard cannot tell a dead identity from a live
  one: a drive carrying a data slab from an install that was abandoned —
  interrupted mid-migration, corrupted, replaced — looks exactly like a drive
  carrying the identity of a node that is running, so the guard refuses both,
  on every boot, with no sequence of boots that recovers the drive. `force` is
  that sequence. It is deliberately not a fleet policy in the sense `any` is:
  `any` says local drives are ours, this says *this drive is spent*. It names
  what it destroys before destroying it.
- **fix(initramfs):** the force flag travels with the policy, not with the
  survey branch that chose the drive. `slab list` and `boot-local` do not ask
  the same question — this drive answered "not a slab" to the survey, having
  no whole-device slab magic, and was chosen as nobody's; `boot-local` then
  read its GPT, found a partition typed as a data slab and refused it.
- **fix(initramfs):** the wipe reads back what it wrote. `dd` exiting 0 is not
  the bytes being gone: the first wipe printed `front cleared` and `back
  cleared`, and the next boot still found a GPT on the drive.
- **fix(boot-local):** a flow-over failure no longer takes the boot down with
  it. Every step of taking a local disk can fail for reasons that have nothing
  to do with the root filesystem, and the root is already attached and serving
  from the appliance by then — which is where it lived before flow-over
  existed. `refusing to format /dev/sda for flow-over` propagated out of
  `boot-local`, so `/dev/ublkb0` was never exported and the boot ended at
  `FATAL: root device /dev/ublkb0 not found after 30s`: a failure naming the
  root device, saying nothing about the local disk, on a node whose root was
  reachable throughout. It warns on the console now and boots.
- **fix(initramfs):** `rd.stormblock.wipe` re-reads the partition table.
  Clearing the bytes is not clearing the table — the kernel keeps what it read
  when it saw the disk, so `/dev/sda1` stays and anything that asks the kernel
  gets the answer from before the wipe. The wipe reported "front cleared" and
  the very next step still refused the drive for carrying a data slab in
  partition 1, naming a partition that no longer existed on the disk.
- **feat(boot):** the data half is seeded onto the local disk before anything
  is exported. The flow-over moves the goldens and deliberately not the data
  slab — migrating a slab while a filesystem on it is being written corrupts
  it — which left the data half never populated at all: a drive that had
  flowed over held `stormpump` and every golden and none of `stormcert-data`,
  `stormcos-state`, `registry-data` or the logs, and the node restored 75
  volumes and died on "volume stormcert-data not found". The window where
  copying it is safe is the one moment nothing is mounted and not a byte has
  been written this boot: after `boot-local` has attached the slabs and
  resolved the volumes, before it exports a single ublk device. Synchronous,
  because it is the difference between a node that boots from its own disk and
  one that asks the appliance forever.
- **fix(boot):** the data half is seeded **per volume**, not per slab. "Empty,
  or leave it alone" was too blunt by exactly one case, and it is the case the
  node was in: the local data half is registered from the boot that laid it,
  so ordinary allocation had put twenty volumes on it while the ones the
  command line mounts stayed on the appliance. A slab-wide test called that
  occupied and skipped it, and the probe went on refusing the drive for seven
  missing volumes, boot after boot, with the fix behind a guard that would
  never open. A volume already on the slab is this node's and is untouched; a
  volume that is not here cannot be overwritten by being copied here, and that
  is the whole safety argument at the granularity the danger has.
- **fix(boot):** seeded volumes are counted by uuid — `VolumeId` is an
  identity and deliberately not `Ord`.
- **fix(initramfs):** the root-device wait is not a deadline for copying a
  disk. Thirty seconds was set when the only thing between there and the root
  device was opening a slab; seeding the writable half took 28.3 s for 3041
  extents on this hardware, so the boot gave up 1.7 s before the work it was
  waiting for landed. A copy proportional to what a node stores cannot share a
  deadline with "opening a device took too long", and progress is printed now,
  so a wait that is doing something looks different from one that is not.
- **fix(initramfs):** forward-confirmed reverse DNS asks about **membership**.
  A node with two NICs on one network has one name and two A records — the
  ordinary arrangement — so taking the last address the resolver happened to
  list and demanding it equal this interface's made the check a coin toss:
  same node, same DNS, confirmed or rejected by the order of an answer nobody
  controls.

### 2026-09-08 (later still, cont.)
- **feat(boot): a system half that is already up to date is left alone.** An
  update has to include not updating: a node that netboots regularly would
  otherwise reformat its own system half and re-copy every golden on every
  boot, destroying a working local half to rebuild the same bytes and running
  from the appliance for the minutes that takes. The flow-over reads what the
  local system slab already holds — offline, from the slab's own record,
  nothing attached — and compares it by volume id, which is what a migration
  preserves: it moves a volume's extents rather than making a new volume, so a
  half this image has already flowed onto holds the same ids and a new build
  makes new ones. Only a superset counts as up to date, because a wrong "not
  up to date" merely re-copies while a wrong "up to date" leaves the node on
  the appliance — and neither loses anything.
- **feat(boot): a reinstall replaces the system half and keeps the data
  half.** The drive a node installed onto carries two things — the goldens,
  which a fresh boot exists to replace, and the data slab, which holds the
  node's CA key and its ServiceAccount signing key and cannot be made again.
  Laying a new table destroys the second to refresh the first; refusing the
  drive leaves the node running from the appliance for the rest of its life.
  Those were the only two answers. A drive that is already this node's layout
  — both halves, told apart by their partition types, which is what those
  types are for (#88) — is now *updated*: the system partition formatted
  afresh for the goldens the flow-over is about to copy in, the data partition
  opened and left exactly as it is, and the node boots normally with the
  identity it already had. No wipe, no new table, and no
  `--local-disk-force`, because nothing is destroyed that an install is not
  meant to destroy. The data slab is opened rather than assumed: a data
  partition that will not open as one is the abandoned-install case, and that
  is what `force` is for. `/init` takes such a drive instead of stepping over
  it — "already a stormblock slab, leaving it" is what made a reinstall a dead
  end.
- **BREAKING feat(initramfs): assimilation defaults to `any` — the drive is
  ours.** A node that netboots this image is being installed, and `off` made
  the common case (one drive, netbooted to be installed) do nothing and keep
  every write on the appliance until somebody knew to add a kernel parameter.
  Nobody netboots an installer at a machine whose disk they mean to keep; a
  node that must not touch its drive says `rd.stormblock.assimilate=off`.
  `any` still refuses a drive carrying one of *our* slabs — that is the node's
  identity, not garbage: the data partition holds the CA key and the
  ServiceAccount signing key, and nothing can mint those again. `force` stays
  the deliberate act for a drive whose identity is spent.
- **fix(image): taking a drive destroys what was on it.** `lay_node_slabs`
  wrote a fresh GPT over whatever was there, which leaves everything the old
  table described exactly where it was: an ext4 backup superblock, an LVM
  label, an mdraid superblock at the tail, a stale *backup* GPT whose header
  sits at a different offset because the old table used a different LBA size.
  Each is read by something that scans rather than asks — udev naming a drive
  after a filesystem that is gone, mdadm assembling an array out of a slab, a
  rescue tool offering to restore the table we replaced. The first and last
  few megabytes are zeroed before the table goes down, so the drive stops
  being what it was rather than merely stopping being described that way.
  Garbage cannot be interpreted safely, and the alternative to installing over
  it is a setup API and a remote UI to drive it — a great deal of machinery to
  decide something the boot already decided.
- **fix(initramfs): a hook's offer beats the default scan** and yields only to
  a policy named on the command line: the default is not an instruction, and
  the scan can only ask `slab list` whether a drive is ours while the hook
  read the drive.
- **feat(initramfs): a boot hook can also name the drive to assimilate onto
  (#109).** `ZB_TAKEABLE` becomes `boot-local --local-disk`. The
  `rd.stormblock.assimilate=` policies are fleet statements applied by a scan
  that can only ask `slab list` whether a drive is one of ours — the right
  question for a policy and a weak one for a drive, since a foreign ext4 or
  the four partitions a second-hand server carries from a previous life
  answers "not a slab" and is taken. An operator typing `--local-disk` has
  looked at the drive; that premise is gone the moment something other than a
  person chooses the path. A hook closes it by offering only a drive it read
  back as blank, which is strictly stronger than "carries no data slab", so
  nothing offered can trip `boot-local`'s guard and the guard stays the last
  word. Precedence: `assimilate=off` (an operator saying no) beats a policy's
  own choice, which beats the hook's offer. `/init` refuses an offer that is
  not on this machine or that names the drive this boot is reading from, and
  never passes `--local-disk-force` for one — a drive that had to be forced is
  by definition not the blank one a hook offered.
- **fix(slab): every slab this engine formats can say what is on it.**
  `slab format` and `POST /api/v1/slabs` reserved a metadata region for `data`
  alone. The reasoning went as far as it went — a data slab has to outlive
  whatever formatted it — but `image build` has always given *both* roles a
  region, so a disk formatted by hand and a disk the image builder laid down
  were not the same kind of thing. A system slab formatted by the CLI could
  not say what was on it: `slab volumes` answered "keeps no volume metadata",
  the initramfs boot probe could not verify the volume its loader entry names
  and fell back to the appliance, and the fallback the bounded shutdown flush
  leans on — each slab keeps its own copy, which is what adoption reads — did
  not exist for it. Auto-sized from the device for every role now, in the CLI,
  the API and the pool-growth path; `--metadata-bytes 0` (or
  `metadata_bytes: 0`) formats a slab that deliberately keeps none.
  `slab format` prints how much it reserved, because "keeps no volume
  metadata" months later is otherwise the first anyone hears of it.
  `Slab::format` stays the plain primitive that reserves nothing.

## [v15.1.0] — 2026-09-08

### 2026-09-08 (later still)
- **feat(initramfs): `/init` asks a boot hook before probing the device the
  command line names (#109).** The local-slab probe asks whether *the one
  device the cmdline names* is a slab this node can boot — the right question
  for the image the cmdline belongs to and the wrong one for a machine: the
  cmdline is a pallet member and identical on every machine that boots the
  image, a slab formatted and never filled boots nothing, and nothing in a
  superblock records whose disk it is. `/init` now runs every executable in
  `/etc/stormblock/boot.d` in order, then `/sbin/zeroboot`, and honours the
  first that decides — `boot-local` with a slab (exit 0) or `ask-appliance`
  (exit 2). Generic rather than zeroboot by name: nothing here depends on any
  particular hook, and no hook installed means the probe decides exactly as
  before.
- **fix(initramfs): nothing a hook prints is executed.** The contract is
  `KEY='value'` lines because the consumer is busybox `sh` with no `jq`, and
  the obvious reading of that is `eval "$(hook boot)"` — but this is PID 1,
  where `eval` makes a stray log line a command run as root before there is a
  system to run it on. The values are read out with `sed`. A hook is also
  asked rather than obeyed: exit 0 naming no slab, or one that is not on this
  machine, or an action disagreeing with the exit status, is refused and the
  next hook runs — believing it would trade the appliance fallback for a boot
  that commits and then drops to a shell.
- **feat(cli): `stormblock slab volumes <dev>` — what a slab holds, offline
  (#108).** The volume records are on the device, in the region the header's
  `meta_offset`/`meta_size` name, and the only way to read them was
  `attach`: the kernel module, root, a reactor, and the volume made live —
  a lot of machinery for a read-only question, and machinery you cannot use
  while still deciding whether this slab is one to touch at all. Positive
  evidence in `slab list`'s shape, and three distinct answers, because a boot
  decision turns on which: a volume list, `holds no volumes` (the slab says it
  is empty), and `keeps no volume metadata` (the slab cannot say — its records
  are wherever `rd.stormblock.meta=` points).
- **fix(initramfs): the boot-volume check reads the slab, not a disk (#108).**
  The probe ran `image inspect "$SLAB"`, which reads a *disk*: it wants a GPT,
  finds the slab partitions in it and reports what each holds. A loader entry
  names the partition — `rd.stormblock.slab=/dev/sda2` — and inspect answers
  that with "no usable GPT on this device", which this branch read as "no boot
  volume" and sent the node to the appliance, every boot, however good its
  disk. It now uses `slab volumes`, accepts the volume by uuid as well as by
  name, and trusts a slab that cannot answer only when `rd.stormblock.meta=`
  says where the records are instead.
- **fix(drive): `FileDevice::open_read_only` — an inspection cannot change
  what it inspects.** The ordinary door creates what it cannot find and opens
  it for writing, so `slab volumes /dev/sdz` on an unknown machine made a
  zero-byte `/dev/sdz` and called it "not a slab" — true, and not what
  happened.
- **feat(initramfs): `BOOT_HOOKS="/path/to/hook ..."` installs hooks into the
  image** and refuses a dynamically linked binary: there is no loader in this
  initramfs, so a glibc build fails at boot as "not found" on a file that is
  plainly there.
- **test:** `tests/initramfs-boot-hook.sh` — 24 checks against the shipped
  `/init`, extracted between markers so the test cannot drift from what runs,
  under busybox `ash` as well as the host shell. Includes a hook that prints a
  command among its assignments, to prove it is not run.
  `tests/integration_slab_volumes.rs` drives the real binary against a slab
  file, including that a missing path is not created and the slab comes back
  byte-identical.
- **docs:** [docs/boot-hooks.md](docs/boot-hooks.md), and README on reading a
  slab without attaching it.

## [v15.0.0] — 2026-09-08

### 2026-09-08 (later)
- **fix(shutdown): a stop takes the kernel devices down with it (#105).**
  Nothing told the ublk exports to stop. The daemon handled SIGTERM, flushed
  metadata and returned — while every export's queue threads sat in
  `io_uring_enter` waiting for work the kernel would never send. The process
  exited with the devices still up, leaving threads in the kernel; **a thread
  stuck in the kernel cannot be reaped**, which is how forge came to carry a
  defunct process in its unit's cgroup for four days and why every restart
  after it ended in "failed mode": systemd found something it could not kill.
  `UblkExportManager::shutdown_all` now signals every export and returns a
  `ShutdownWait` the caller settles with a deadline of its own (a flag set by
  each export's thread, not a `JoinHandle` — `join` takes no deadline, and the
  one thing a stop must not do is wait forever). The daemon signals before it
  waits on anything, then flushes and settles together: ~13 s worst case,
  inside the unit's `TimeoutStopSec`, which is the number that decides whether
  SIGKILL lands in the middle of a teardown. Proven on metal: attach →
  `/dev/ublkb0`, SIGTERM, device removed, stop in 0.16 s, no defunct process.
- **fix(shutdown): the subcommand paths join their ublk threads with a
  deadline.** `boot-local`, `boot-iscsi`, `adopt-ublk` already signalled and
  joined, but joined unbounded — one wedged teardown held the whole stop open
  until systemd escalated, which is the same fault on the node's own root
  path. `join_ublk_threads` bounds them and says how many did not finish.
- **feat(releases): a release says whether it can still be downloaded
  (#106).** Eight published versions on forge named volumes that had been
  reclaimed: manifests, digests and download links, with nothing behind them,
  and nothing distinguishing them from a release a consumer could fetch. Every
  release now reports `state` — `available` or `archived` — derived on read
  from whether its volume resolves, never stored. An archived release's
  `image.img` answers **410 Gone**, not a 404 that reads as "no such version",
  and says where its manifest still is; the browser index drops the link
  rather than offering one that cannot be taken. Its manifest and notes stand:
  the record of what a version contained is worth keeping after the bytes are
  not.
- **BREAKING fix(volumes): a volume a published release names cannot be deleted
  (#106).** The release joins exports, LUNs, ublk devices and StormFS pins in
  the one shared "what is still using this volume" answer, so delete *and* the
  move guard *and* the template sweep all refuse it, naming the version and
  saying to unpublish first. `force=true` does not cover it — that flag is for
  a dangling synonym, and orphaning a release is a different decision.

## [v14.0.0] — 2026-09-08

### 2026-09-08
- **BREAKING fix(mgmt): the management API is guarded, and a node that is not
  says so out loud (#107).** From a workstation, with no credential of any
  kind, a node on the fleet network answered `GET /api/v1/volumes`,
  `GET /serve/v1/exports` and — the telling one — `POST /api/v1/fstemplates`
  with a 422: a rejected credential returns 401 *before* the body is parsed, so
  422 means the request was accepted and only the body was wrong. The mechanism
  to stop that was already written — `serve::api::require_token`, with a read
  token, an admin token for destructive verbs and a public-path exemption —
  and **nothing ever wired it to the engine's router**. `management.api_token`
  guarded `/v1` alone, so a node whose config named a token still served every
  other surface openly on `0.0.0.0:9090`: create, clone, seal and delete
  volumes, add and withdraw exports, publish releases, and re-point
  `boothost/<tag>`, which chooses what a machine boots at its next power cycle.
  A configured token made the whole node read as closed. **Breaking:** a node
  with `management.api_token` set now requires it on `/api/v1`, `/serve/v1`,
  the kube surface and `/metrics`, not only on `/v1`.
- **feat(mgmt): a node can mint its own token.** `management.require_auth =
  true` takes one from `api_token`, `$STORMBLOCK_API_TOKEN` or
  `management.token_file`, and mints one into that file (default
  `<data_dir>/api_token`, mode `0600`) when there is none — a token cannot be
  baked into an image, because every node booting that image would carry the
  same one, so it is made on the node at boot and read there by whatever else
  runs on that machine. With nowhere to keep one, startup **fails** rather
  than falling back to open. `management.admin_token` (also
  `$STORMBLOCK_ADMIN_TOKEN`) reserves destructive verbs for a second token.
- **feat(mgmt): an open node cannot be silent.** It logs, on every boot, that
  its API is unauthenticated, what that exposes and how to close it; and
  `GET /api/v1/health` reports `auth: required|none`, so a fleet can be asked
  which of its nodes are open without trying to break into each one. The
  default is still open, deliberately: a machine claims its boot image before
  it has any credential, so closing the fleet from inside the engine would
  stop machines booting. That is a migration — distribute the token, then set
  `require_auth` — not a default. What is not deferred is the silence, because
  nothing fails while this is wrong.
- **fix(mgmt): `/api/v1/health` is public and `/metrics` is not.** Health is
  how an initramfs establishes that the address DHCP gave it is an appliance
  at all, before it has any credential; a 401 there reads as "not an
  appliance" and drops a booting node to a shell. A scrape, by contrast, names
  this node's volumes and says how full it is — `is_public` has said since it
  was written that a scrape is not a probe, and `/metrics` was being served
  beside the guarded router rather than inside it.
- **fix(http): peer and tool clients present a token when the fleet was given
  one.** Cluster replication and migration handoffs, `image build` reading a
  golden off an appliance, and `boot-claim --token` (or
  `$STORMBLOCK_API_TOKEN`). Only a *shared* token is presented outward: a
  minted one identifies a caller to the node that minted it, and means nothing
  to a peer.
- **docs:** [docs/auth.md](docs/auth.md).
- **build: clap's `env` feature is declared rather than borrowed.**
  `#[arg(env = "STORMBLOCK_ENGINE")]` compiled under the default feature set
  because something else in it turned clap's `env` on; the RouterOS profile
  (`--no-default-features`, no openraft) lost the feature with it and had not
  compiled since `c709b7c`. Found building the profile to check this change,
  not by the change.

### 2026-09-07
- **fix(initramfs): the driver classes that cannot carry a boot are dropped.**
  The archive went from 65.7 MB to 97.5 MB with no change to this script. The
  cause was the tree it is built from: an earlier build extracted only
  `kernel-modules-core`, and adding `kernel-modules` put every wireless driver
  in Fedora under `drivers/net` — which is copied whole, on purpose, so a node
  can DHCP on whatever card it has. Their stacks followed through the
  dependency closure: 802.11, Bluetooth for the combo chips, SDIO for the ones
  on an MMC bus. **431 extra modules, 31.6 MB**, read into RAM on every boot of
  every node. Proved by fetching the older initramfs back off the appliance —
  it is still there as a content-addressed blob — and diffing the two
  archives. `wireless wwan can ieee802154 wan hamradio` are now pruned:
  whole *classes*, not model numbers, so `drivers/net` stays a promise about
  every card a boot could arrive on. Pruned before the closure runs, so
  anything genuinely depended on by a driver that stays is copied back. 80.1 MB,
  and the only modules the older archive had that this does not are five
  cellular modems.
- **fix(initramfs): the appliance is discovered, never baked, and nothing is
  trusted on sight.** A diskless node has to reach an appliance, and its
  address is the most network-specific fact there is — so an image carrying
  one is an image per network, the same mistake as a service tag on the
  command line one level up. The node asks the network it is on, in order:
  `rd.stormblock.boothost=`, **DHCP option 17** (root-path, only when it is a
  URL — option 17 is classically an NFS export and a path is not an
  appliance), `boothost` on the lease's own search domain, the lease's
  next-server, and the DHCP server itself. Every candidate is *asked*
  (`/api/v1/health`) rather than assumed, and the first that answers as an
  engine wins; a wrong one costs three seconds. That last pair is what makes
  a network nobody prepared work at all: with no record and no option 17 the
  DHCP server is tried, and on a small network it is very often the
  appliance. The image says nothing about any of them.
- **fix(initramfs): a netbooting node reads its service tag from SMBIOS when
  the command line does not carry one, and composes its host NQN from it.**
  The init insisted on `rd.stormblock.tag=` on the grounds that the firmware
  would hand the name down - but nothing appends to the command line between
  the pallet and the kernel, so every image that booted diskless had the tag
  typed into its spec, one image per machine. `/sys/class/dmi/id/product_serial`
  is SMBIOS type 1 serial, the field stormbootx claims on, so the two names
  agree by construction. `rd.stormblock.tag=` and `rd.stormblock.hostnqn=`
  still win when present.

## [v13.7.0] — 2026-09-07

### 2026-09-07
- **feat(volumes): a slab can be composed — `POST /api/v1/volumes/compose/slab`.**
  The last copy in a release. `compose/pallet` and `compose/disk` already
  built a pallet and a GPT over goldens by their maps, but the slab a node
  clones and runs from was still laid by `image build`, which writes every
  golden into it a second time — the same bytes the pallets carry. A composed
  slab is formatted inside a fresh volume, each golden's slots are taken
  explicitly from the nested slab (a thin volume maps nothing until written,
  and nothing is written), and those slots are mapped onto the source volume's
  slots through the engine's own extent map. The nested slot size is the
  engine's, so a nested slot *is* an engine slot. What is written: the
  superblock, the slot table, `volumes.dat`, and one slot per clone stamped
  with its own filesystem identity (#76). The runs are checked contiguous —
  a gap would share the wrong slot and read as a different golden a slot in.
  The result opens as any slab does: `Slab::open`, attach, restore, and the
  goldens read as the volumes they map onto. `compose/disk` now types a slab
  volume from its recorded kind (`slab`, `data-slab`), so the composed disk
  carries the GPT types a node's discovery looks for.
- **feat(image): a golden can be a volume on an engine.** `GoldenSpec` accepts
  `from = "volume:<name>"`, resolved against `--engine` (or
  `STORMBLOCK_ENGINE`): the volume is exported over NVMe/TCP for the length
  of the build and withdrawn afterwards, so a golden that lives on the
  appliance needs no file on the build box. Named, not addressed: the spec
  says which golden, the invocation says which engine holds it. This removes
  the duplicate artefact; it does not remove the copy, which is what the
  composed slab above does instead.
- **fix(thin):** a thin volume honours the block size it advertises, in both
  directions. It reported 4096 and passed sub-block reads and writes straight
  down, so anything beneath it saw I/O it had every right to refuse. Short
  requests now cover the whole blocks they land in.
  Not a corner case: ext4 puts its superblock at byte 1024 and its group
  descriptors are smaller than a block, so every mkfs issues short requests.
  Cheap where it matters, because an unmapped extent is answered with zeros
  and no device I/O; the read only becomes real once the extent is allocated,
  which is exactly when skipping it would destroy the rest of the block. It
  cannot leak, because a slot is zero-filled across its whole length on first
  write.
- **fix(ext4):** device errors name the operation and its length. "device I/O
  failed at offset 45056" does not say whether a read, a write or a discard
  was refused, and the three have different causes.

## [v13.6.0] — 2026-09-06

- **fix(drive): `SasDevice` honours the `O_DIRECT` contract it opened the fd with (mkfs.ext4.rs#5, the half that was never explained).** Every ext4 template format on a 4 KiB-sector appliance failed at the first inode-table zeroing — 45056 for 64M, 593920 for 1024M, 2756608 for 5120M — with `EINVAL` at an offset and a length that were both whole 4096-byte blocks, and it kept failing byte-for-byte after the crate-side fixes. The offset was never the problem. `SasDevice` opens the drive `O_DIRECT` and submitted the caller's buffer pointer to io_uring as it was; `O_DIRECT` needs the buffer *address* aligned to the logical block too, and a `Vec` from malloc is 16-byte aligned. Reproduced on `losetup -b 4096`: a 4096-byte write from a `Vec` at offset 0 is `EINVAL`, the same bytes from a `DmaBuf` are written. The drive now bounces any buffer that is not block-aligned through a page-aligned `DmaBuf` on read and write, and an offset or length that is not whole blocks comes back as `DriveError::NotAligned` instead of the kernel's bare errno. Two tests run against a real 4 KiB loop device (`STORMBLOCK_4K_LOOP`, `--ignored`): the malloc-buffer round trip, and the whole provisioning path — slab, thin volume, ext4 format, fsck clean.

- **chore(deps): mkfs-ext4 v3.0.0, fio-ext4 v1.7.0.** Both crates clippy-clean under `-D warnings`; mkfs-ext4's major is the `no_std` `BlockReader` error type, which nothing here uses. Both pins move together so cargo resolves one copy of `mkfs-ext4` and one `BlockDevice` trait.

- **chore(deps): mkfs-ext4 v2.2.2, fio-ext4 v1.6.1 — formatting and opening a thin volume that enforces its 4096-byte logical block (mkfs.ext4.rs#5, fio.ext4.rs#4).** Every template format failed partway (`device I/O failed at offset 16384: Invalid argument`) and the state-volume check at boot could not read the superblock (`offset 9226421248 not aligned to block size 4096`). Two faults in the crates, neither here: sub-block I/O at aligned offsets — the 1024-byte superblock, one inode at a time — and a per-format sector below the device's own being honoured, which is how a 256M template chose 1 KiB blocks on a 4 KiB volume. Both crates now issue every device operation as a whole filesystem block at a block boundary, and the device's sector is a floor a caller cannot lower. Both pins move together so cargo resolves one copy of `mkfs-ext4` and one `BlockDevice` trait.

- **feat: a blank travels as a blank, so templates are derived rather than registered (#100).** A netbooted node attached its slab, had every blank it needed — `pvc-1M` through `pvc-1G`, `stormcos-state`, the `*-data` volumes — and no way to know which they were. `TemplateStore` is a separate registry kept under `management.data_dir`, `image build` never wrote one, and a node booting over the network has no data directory, so it loaded empty. The image already knew: `data1` declares its blanks `role = "blank"`. That lived on the pallet member and did not survive onto the volume. `GoldenSpec::template` and `VolumeRecord::template` carry it through, `image build` marks them, and `VolumeManager::templates()` derives the list from what is attached — nothing to register, nothing to keep in sync, and a blank added to an image is a template on the next boot. Old records load with no templates, which is what they had anyway.

- **feat(claim): a claim starts the volume's own subsystem and hands out its address (#98).** Step 1 was inert at first: the claim publishes namespaces through `ensure_nvme_namespace`, which hot-adds into the *shared* target and keeps its own nsid map, so the reconciler never saw a claim clone and never gave it a subsystem. `ensure_volume_subsystem` now serves the volume under `nqn...:vol-<guid>` on its own port — one namespace, nsid always 1 — and the claim returns that. A claim is exactly the moment a volume acquires a consumer, so it is the right moment to start serving it under its own name. Ports are allocated by *binding*: asking the kernel beats keeping a second opinion about what is free. `serve` publishes the prefix, range and reactor to the API at startup, since the settings live in its config. Verified on the wire — the subsystem presents exactly one namespace and it is the boot image.

- **feat(claim): a claim hands out the address that names the volume (#98, step 1).** The attach URI was `nqn...:stormcos?nsid=N` — a shared subsystem where the namespace number is the only discriminator, and numbers are reused, so a stale address resolves to whatever inherited the number rather than failing. A claim now prefers the volume's own subsystem, `nqn...:vol-<uuid>`, whose NQN carries the volume's GUID: nothing to go stale, and a deleted volume stops answering. `nsid` is always 1 there and carries no information, so nothing can disagree about it. The reconciler records where each volume is served (it assigns the port) and clears it on withdrawal, so an address never outlives what it names. Falls back to the shared form while a volume is not yet wired, which keeps a node mid-boot working across the change.

- **fix(synonyms): a claim no longer releases the clone the machine is booting on (#97).** A node claims **twice per boot** — stormbootx to load the kernel, the initramfs to find the root — and the supersede path read the second claim as "this consumer is done with what it had", releasing the first clone while the machine was still attached. On an R230 that was a boot loop: hot-added and attached at 15:09:15, released at 15:09:49 with the controller connected. The other guards all ask about *ownership*; none asks about **use** — and NVMe cannot be asked here, because one subsystem exposes every namespace, so a connected controller sees all of them. Age is the only signal there is: a clone younger than `claim_grace` (default 10 minutes, held on `AppState`) is the stage before this one, not an abandoned predecessor. The cost is at most one extra clone per boot cycle, collected by the next boot — the right way round, since the alternative is pulling a namespace out from under a booting machine.

- **feat(pallet): a pallet can be published as the whole volume** (`PublishSpec::whole_drive`) — superblock at byte zero, no partition table. The layout `whole_drive_pallet` already *read*; it could not be written, because publish always allocated a GPT slot first. That was the one thing blocking a release from being a composition: `compose/disk` maps partition **volumes**, and a pallet written into a GPT'd drive is a drive, not a partition. So every release copied every pallet again instead of mapping the ones that had not changed — 10.25 through 10.31 are seven near-identical 11 GB volumes where a few changed pallets plus seven GPTs would do (stormpump#20).

### 2026-09-06
- **fix(initramfs): console output is ASCII.** Twelve diagnostics carried em-dashes, and a serial console is 7-bit in practice — a UTF-8 em-dash arrives as two bytes of noise in the middle of the line you are trying to read. On a headless node the serial log is the only record there is, so the messages that matter most were the ones being corrupted. Comments keep their typography; only what is printed changed.

### 2026-09-05
- **fix(initramfs): the local-slab probe requires positive evidence.** It fell back to the appliance when `slab list` said *"not a slab"*, which assumed the only alternative to a slab is a disk that answers. On the R230 `/dev/sda` is sometimes the WD disk and sometimes the iDRAC virtual floppy: an empty removable drive answers `ENOMEDIUM`, not "not a slab", so the probe passed and the boot died on `Error: I/O error: No medium found (os error 123)`. It now falls back unless the device positively identifies itself (`: slab <uuid>`). A device path is not a stable identity.
- **feat(releases): a manifest table viewer** at `/api/v1/releases/{version}/manifest.html` — 54 components with kind, name, version, size, digest and provenance, each row marked `changed`/`new` against the previous release and showing what a changed component was before. Filter by text or status. Keyed by `(kind, name)` like `/changes`, since the same name appears as a binary and as the golden built around it.
- **fix(synonyms): a boot no longer leaks a clone.** The release of a superseded clone only ran when the caller asked for a *named* one, and the boot path claims with an empty body — so nothing was ever released and every boot left one behind (thirteen accumulated behind one service tag). The predecessor is now found by the clone's deterministic name regardless, and the guard accepts a clone of anything the name has ever pointed at rather than only what it points at now — re-pointing being exactly when the old clone becomes garbage.

- **feat(initramfs): one image boots both lives.** A node netboots once to install itself and then boots from the disk it installed onto, with the *same* command line — the cmdline is a pallet member and there is only one of it. So the local slab is tried first and `rd.stormblock.boothost=` is the fallback: on the bootstrap boot the disk holds no slab and the node asks the appliance; after the install it does, and the node stops asking. Nothing is rewritten between the two. Existence is not the test — the R230 that found this has a 2 TB disk with four partitions from a previous life, so `/dev/sda` is very much there and is not a slab; `stormblock slab list` decides.

- **feat(initramfs): CPU microcode ships with the image.** The initramfs carries no microcode, so a node ran whatever its BIOS shipped for the life of the machine. The R230 this was found on reported `x86/CPU: Running old microcode` against a BIOS dated 31 Jan 2018, and five mitigations the CPU could not apply — MDS, TAA, SRBDS, MMIO Stale Data and GDS all `Vulnerable ... no microcode`, every one of them shipped after that BIOS. An uncompressed cpio holding `kernel/x86/microcode/{GenuineIntel,AuthenticAMD}.bin` is now prepended to the archive, which is the only way to hand the kernel microcode before it brings up the other CPUs. Updating firmware fixes one machine once; this fixes every node that boots the image, and versions the microcode with it. Skipped with a warning when the build host has none.

- **feat(nvmeof): an initiator can say which machine it is.** The host NQN was `const HOST_NQN = "nqn.2024.io.stormblock:initiator"` — every stormblock initiator in the fleet presenting the same string, so a target could not tell one caller from another. It is now settable: `?hostnqn=` on the attach URI, `STORMBLOCK_HOST_NQN` in the environment, or `set_default_host_nqn()`, falling back to the old constant so nothing existing changes.

  This matters because the host NQN is the one identity NVMe carries on its own. stormbootx composes `nqn.…:host-<tag>` from SMBIOS and presents it on every connect, which is how the appliance knows which machine is asking before anything else exists — and until now Linux then reconnected anonymously, so the appliance saw one machine become a nameless second initiator halfway through its own boot. `rd.stormblock.hostnqn=` carries the name firmware already composed; the format stays in stormbootx and nothing re-derives it.

- **feat(netboot): a diskless node can reach its root.** The image's kernel command line said `rd.stormblock.slab=/dev/sda`, so a machine that had just streamed its kernel over NVMe/TCP went looking for its slab on a local disk — `bad slab magic`, then `root device /dev/ublkb0 not found after 30s`, then an initramfs shell. `rd.stormblock.boothost=<appliance>` with `rd.stormblock.tag=<tag>` claims that machine's image and uses the namespace it names as the slab; `rd.stormblock.slab=` now also takes an `nvme-tcp://` URI directly. `open_one_drive` has parsed fabric URIs since #73, so only *where the slab is* differs and the rest of the boot is the local path unchanged. Two things blocked it: the init waited for the slab to appear as a device file, which a URI never will, and a local-mode boot skipped networking entirely, which a remote slab cannot do.
- **feat:** `stormblock boot-claim --boothost <url> --tag <tag>` prints the attach URI on stdout and diagnostics on stderr, so it substitutes straight into `--slab`. It retries a restarting appliance and stops immediately on a 404 naming the synonym to create. `--tag` is required and deliberately not discovered: stormbootx reads the identity from SMBIOS and claims on it before Linux exists, so this takes that name handed down rather than implementing it a second time (stormbootx#7 widens it past Dell).

- **fix(initramfs): the uplink is chosen by carrier and speed, not by enumeration order.** It took the first non-loopback interface. "First" is a kernel enumeration order, not a statement about which port has a cable in it: on a Dell R230 booting over NVMe/TCP it picked `eth0` of a two-port Mellanox while the cable was in `eth1`, bridged the dead port, and sat in DHCP for four minutes before falling to link-local — with the hostname derived from the dead port's MAC. stormbootx, a stage earlier and with no drivers at all, had already enumerated the same four NICs, filtered to link up and confirmed one *answered* before committing; running after Linux has enumerated the same hardware, this should not know less than the firmware did.

  Every physical port is brought up first (carrier cannot be read from a down interface), the link is given time to negotiate — ending as soon as anything reports carrier, so a fast link does not wait for a slow one — and the candidates are ordered fastest first. DHCP then runs over each in turn: carrier says a cable is in the port, not that the port reaches a DHCP server, so the lease is the real test and failing it moves to the next candidate instead of falling to link-local with three good ports untried. Link-local remains the last resort, and now names every port that was tried.
- **test:** `tests/initramfs-nic-selection.sh` runs the shipped selection against a fake `/sys/class/net`, extracted from the generated init between markers so the test cannot drift from what runs. Covers the R230 layout, speed ordering, single port, no carrier anywhere, loopback and bridges, and an unreadable speed.

## [v13.5.0] — 2026-09-05

### 2026-09-05
- **fix:** The release download reads an aligned window into a `DmaBuf` and returns the slice asked for. A byte range from an HTTP client is arbitrary in offset, length and buffer address, and the volume underneath may be a device opened `O_DIRECT` where all three must be block multiples — so `curl -r 4096-4103` against a published release returned EINVAL.
- **fix:** `DriveError::is_media_failure()` — EINVAL, `NotAligned`, `BufferTooSmall`, `NoSpace` and `ReadOnly` are the request being refused, not the media going away, and `mark_failed` leaves the leg alone for them. The marking is sticky and persisted, so without this a bad range request took a sealed 11 GB release offline across restarts. Same reasoning as #92.
- **feat:** `POST /api/v1/volumes/{id}/legs/clear` — try a volume's failed legs again. Clears the markings and reads the volume to prove it; anything still broken marks itself again and the response says which slabs came back.
- **fix:** A data slab's metadata region is sized from the drive, not from a flat 4 MiB. A volume record carries its whole extent map, so what the region must hold scales with the slots the slab can hand out — an 11 GB volume is ~11k extents on its own. `POST /api/v1/slabs` and `stormblock slab format --role data` both use `auto_metadata_bytes` now; the CLI previously reserved **nothing at all** for a data slab.
- **fix:** A failed metadata persist is no longer a `warn!` the operation ignores. `persist()` reports every copy that failed, logs at `error!` as a durability fault, and records it; `persist_checked()` is the same path. Losing this quietly meant volumes were created, acknowledged, and never written — a node came back with 9 of 38 volumes and a published release that had never been on disk.
- **feat:** `GET /api/v1/slabs/durability` — whether the record is reaching the disk, and per-slab how large it encodes to against what that slab reserved.
- **fix (synonyms): re-claiming releases the clone it supersedes.** A claim
  mints a clone, and re-claiming re-points the consumer's own name at the new
  one — leaving the previous clone alive and named by nothing. It is invisible
  by construction: a clone shares every extent with its golden, so nothing runs
  short and nothing complains. Thirty-five accumulated behind a single service
  tag on forge before anyone looked.

  A consumer re-claiming is saying it is done with what it had, so the volume it
  has just stopped naming is released, along with its export — an export
  outliving its volume is a namespace answering for nothing. Deliberately timid:
  it happens only when nothing else names the volume, it is not sealed, and it
  is a clone of the same golden. Any of those failing leaves it alone and says
  so, because the cost of being wrong here is someone else's data.
- **refactor (exports): `drop_export` is shared.** Tearing an export down is not
  only something a caller asks for — releasing the volume behind it has to do
  the same.
- **fix (fat): the ESP declares the medium's sector size too (stormcos#31,
  second half).** Fixing the GPT made the partitions appear and the ESP be
  typed correctly — and it still would not mount: `FAT-fs: logical sector size
  too small for device`. The FAT builder had `const SECTOR = 512`, so an ESP
  written onto a 4096-byte device declared 512-byte sectors and no FAT driver,
  the kernel's or the firmware's, would touch it. Found, correctly typed, and
  unreadable is not better than not found.

  The sector size now comes from the device, as it does for the GPT. One
  consequence is real and correct: FAT32 needs 65525 clusters and a cluster is
  at least a sector, so at 4096 bytes a volume must be about eight times larger
  to be FAT32 — a 64 MiB ESP is now FAT16, which UEFI accepts. A test fixture
  that assumed 512-byte geometry was resized to match.
- **fix (image): the GPT is written in the block size the medium presents, not
  512 (stormcos#31).** A partition table is found at LBA 1 — one *block* in, in
  the medium's own block size. `image build` defaulted to 512-byte sectors while
  every medium it lands on presents 4096, so the header was written at byte 512
  and every reader looked at byte 4096, found zeros, and concluded the disk had
  no partition table. The image was intact and unbootable: a PowerEdge R230
  attached `stormcos-sno-10.22` over NVMe/TCP and reported that it published no
  ESP; `blkid` saw only `PTTYPE="PMBR"` and made no partition devices.

  The default now follows the output device. `block_size` in a spec is still
  honoured — an image built for media the builder is not writing onto is a real
  case — but a spec that disagrees with the device is warned about, naming the
  byte the header will land on and the byte a reader will look at.

  An existing test asserted `block_size == 512` and had to change: it was
  pinning the default that caused this.

## [v13.4.0] — 2026-09-03

### 2026-09-03
- **docs:** README gains what a claim answers with and how a machine's image is
  chosen — a `boothost/<service-tag>` synonym, claimed in one request that
  returns both the copy-on-write clone and the `nvme-tcp://` URI reaching it —
  and why that mapping does not belong in DHCP.
- **feat (synonyms): a claim returns the tuple that reaches the clone.**
  `POST /api/v1/synonyms/{ns}/{name}/claim` answered with a volume id, a name
  and a size — nothing an initiator can act on. Firmware doing an NVMe/TCP boot
  knows a volume exists somewhere and still has to ask where, which is a second
  request from a client whose entire state machine is "get an address, attach,
  boot", and a window in which the claim is held and nothing is served.

  The response now carries `attach` — protocol, address, port, nqn, nsid and a
  ready-made `nvme-tcp://` URI — matching what sbregistry's `/v1/clones/claim`
  has always returned. An existing export is reused rather than a second one
  minted: the nsid is part of the address, and issuing a new one for a volume
  that already has an address changes it under whoever holds the old one. The
  address is the advertised one, since a wildcard listen address tells a caller
  nothing and loopback is worse.
- **fix (releases): the change list is keyed by kind *and* name.** A name alone
  is not unique — `stormblock` and seven others appear both as the binary that
  was compiled and as the golden built around it, and they change for different
  reasons. Keyed by name, one silently masked the other and the diff compared
  the wrong pair: a real 10.21→10.22 diff reported 5 changed components where
  the true answer was 7, hiding that stormblock's own binary had moved.
- **feat (releases): a component carries its version and its own notes, and a
  release can say what changed.** A manifest entry had a name, a digest and a
  commit; it now also carries `version` — the number a person reads and compares
  — and `notes`, what changed in that component since the last release that
  carried it. The three answer different questions and none substitutes for
  another: the digest says whether the bytes moved, the commit says which source
  produced them, the version is what anyone actually cites.

  `GET /api/v1/releases/{version}/changes[?since=X]` answers the question a
  release note exists for. Comparison is **by content digest**, so a component
  rebuilt from the same source to the same bytes is not a change — a rebuild is
  not news, and a list padded with things that did not move is one nobody reads
  twice. Reports changed (with from/to version, commit and digest), added,
  removed, and a count of the unchanged. Without `since` it compares against the
  release published immediately before; the first release says so rather than
  showing an empty diff.
- **feat (volume): composed disks — a per-node bootable disk is a chain of
  goldens, and costs its map.** `POST /api/v1/volumes/compose` (v13.3) made a
  volume out of goldens but not a *disk*: it had no partition table, and every
  pallet `image build` lays down is bytes, so a fleet of a hundred nodes was a
  hundred copies of the same goldens. Now everything on a disk is a golden:
  - **A pallet is a sealed volume** (`POST /api/v1/volumes/compose/pallet`,
    `VolumeManager::compose_pallet`). `PalletBuilder::content_align` places
    every member on a slab slot boundary, so a member that is already a golden
    is shared in by its extent map rather than copied; only the header and any
    inline `text` member are written. `MemberSpec::reserve` keeps a golden's
    whole span even when the manifest digests fewer bytes, so the next member
    lands after all of its slots. The result is read back and verified with
    `Pallet::read` + `verify_all` before it is sealed as `fs.kind = pallet`; a
    version left out follows the highest sealed version of that pallet name.
  - **The GPT is two goldens.** `Gpt::render` produces the head (MBR, header,
    entries) and tail (entries, backup header) as bytes; `compose_disk` mints
    them once per *layout* — named by a digest of LBA size, disk size and the
    ordered partitions — and every disk of that layout shares them. Disk and
    partition GUIDs are derived from the layout, so `root=PARTUUID=` is the
    same on every node; `fresh_guid: true` stamps a per-disk GUID at the cost
    of the two GPT slots, on the disk's own copy-on-write slots.
  - **A disk is `compose(head, partitions…, tail)`**
    (`POST /api/v1/volumes/compose/disk`). Each partition is a volume laid
    out in order on slot boundaries; the type follows what the volume is, a
    pallet's `priority`/`tries` become its GPT attributes. The disk reads back
    through the map — both GPT headers, every pallet's manifest — before it is
    returned, and reports `written_bytes: 0`.

  **The LBA size defaults to 4096**, because that is what NVMe/TCP and ublk
  present a volume at and firmware parses a GPT in the media's own block size
  (docs/pallets.md §2.4). Cutting a new version is: import the changed
  component as a golden, compose a pallet, compose the disks — nothing else is
  rewritten. `fs::disk::detect` recognises a pallet by its magic.
  `ci-compose-disk-verify.sh` checks a composed disk with `fdisk`, `blkid`, a
  real mount, the kernel bytes digesting to the file, `stormblock pallet
  verify` against the ublk device, and an OVMF boot through shim and grub.
  See docs/composed-disks.md.

### 2026-09-03
- **feat (tiering): demotion that does not drag the current image down with the
  old one.** `PlacementEngine::migrate_leg` relocates a slot and rewrites every
  map that named it — right for draining a failing drive, exactly wrong for
  tiering, because demoting last month's image would pull the slots it shares
  with this month's onto the slow drive and the current image with them.

  `ThinVolumeHandle::relocate_extent` is copy-on-write with the same data, and
  one path covers both cases because the difference falls out of the reference
  count: a **shared** extent is copied to the destination and the original left
  for whoever else names it; an **exclusive** one is copied and the last
  reference dropped, which frees it. That is the demotion rule exactly — the old
  image ends up whole on the slow tier, what the new one still uses stays on the
  fast tier, and what nothing else references gives its space back.

  `VolumeManager::retier_volume` applies it extent by extent, yielding between
  each so the volume keeps serving, and reports moved/copied/failed separately
  because those are different facts about what happened.
- **feat (releases): `demote_previous` on publish.** A new release can move the
  one it replaces down a tier in the same call. The policy lives with whoever
  publishes, since only they know whether a build supersedes the last one or
  sits beside it; absent, nothing is demoted. A replicated volume is refused
  rather than half-relocated — moving one leg of a mirror is a resync decision,
  not a tiering one.
- **feat (releases): a download honours `Range`.** A 32 GB image over one HTTP
  request is a single dropped connection away from starting over. A `bytes=`
  range now returns 206 with `Content-Range`, a `Content-Length` of the slice,
  and only the requested bytes streamed off the volume; every response carries
  `Accept-Ranges: bytes`. Open-ended (`bytes=1000-`) and suffix (`bytes=-512`)
  forms both work, an end past the image is clamped rather than refused, and a
  start past it earns 416 with `bytes */total`.

  Ignoring `Range` was legal and awful: the caller asked for a megabyte and got
  thirty-two gigabytes, which is how a stray `curl -r` filled the build box's
  tmpfs. Anything unparseable, and a multi-range request, falls back to the
  whole image rather than to a guess — answering the first of several ranges
  would be a quiet lie about what was sent.
- **feat (releases): `/api/v1/releases` — what this appliance publishes, and how
  to get it.** An image on a shelf is not a release. A release is a version
  someone can find, a link they can pull it from, a manifest saying what went
  into it, and notes saying what changed; the surface serves all four.

  - `GET /api/v1/releases` — the index, newest first, with sizes and links.
  - `GET /api/v1/releases/index.html` — the same for a browser, because a
    download link nobody can click is not much of a link.
  - `GET /api/v1/releases/{version}` — the record, `/manifest`, `/notes`.
  - `GET /api/v1/releases/{version}/image.img` — the image itself.
  - `POST` publishes, `DELETE` withdraws.

  **Publishing copies nothing.** A release names a volume the engine already
  holds and the download streams straight out of it in 4 MiB chunks, so what a
  caller pulls is the image being served over NVMe/TCP at that moment rather
  than a copy that may have drifted from it. Withdrawing a release removes the
  record and leaves the volume alone: "stop offering this" and "destroy this"
  are different decisions and only one is reversible.

  The index survives a restart (`releases.json`, written temp-and-rename beside
  the volume metadata). A version is validated where it is published, since it
  becomes a URL path segment and a filename.
- **fix (exports): an NVMe-oF export survives a restart, with its namespace id
  intact.** Exports lived only in memory, so an engine that restarted stopped
  answering at addresses consumers had already written down — and firmware
  booting over NVMe/TCP has the subsystem and namespace baked into its
  configuration. The table is now written to `exports.json` beside the volume
  metadata (temp file and rename, so a crash cannot truncate it) and re-wired
  into the target at startup.

  **The namespace id is restored, not reassigned.** Handing a volume the next
  free nsid on restart would be a silent renumbering, and everything that
  attached by the old one would come back pointing at a different volume or at
  nothing. The number is part of the address, so it is part of the record.

  A volume that did not come back leaves its export recorded and `pending`
  rather than dropping it: an address that is temporarily unserved is a
  different thing from one that was withdrawn, and only one of them should be
  forgotten. Verified on forge: an export created, the service restarted, and a
  remote initiator read the image back byte-identical over the same nsid.

## [v13.3.3] — 2026-09-02

### 2026-09-02
- **feat (mgmt): the engine says at startup what consumers will be told to
  dial, and whether that was a guess.** A derived advertised address is a
  guess on a multi-homed node — forge reaches the world through one interface
  and serves its consumers on another — and a silent guess is what makes this
  class of problem expensive. The line names the address and, when it came
  from the default route, names `management.advertised_addr` as the knob that
  settles it. **Set it explicitly on any node whose consumers are not on the
  default route**: a storage fabric usually is not.

## [v13.3.2] — 2026-09-02

### 2026-09-02
- **fix (mgmt): an attach tells a remote initiator *this node's* address, not
  loopback.** Found testing on forge: with the targets on `0.0.0.0` and no
  `advertised_addr` — the ordinary configuration for a node that serves the
  network — an attach answered `{"traddr": "127.0.0.1"}`, which is not merely
  unhelpful to a remote initiator, it names the initiator's own machine. The
  wildcard case now answers the address this node reaches other machines from,
  and loopback only where there is no route at all. No packet is sent to find
  it: connecting a UDP socket picks a route and binds a source address, and
  the address it is pointed at is TEST-NET-1.

## [v13.3.1] — 2026-09-02

### 2026-09-02
- **fix (api): a plain create needs a slab, not an `array_id`.** Driving the
  built binary rather than the in-process router, a node whose drive carried a
  slab adopted at startup refused `POST /api/v1/volumes {"name","size"}` —
  the one request every consumer sends — with *array_id is required*. An array
  binding is legacy: a volume's extents pick their own slabs. The create now
  needs somewhere to pick from and nothing else, and a node with no slabs at
  all says **that**, instead of naming a parameter that would not have helped.
- **test: the engine as a process** (`tests/integration_engine_e2e.rs`,
  opt-in via `STORMBLOCK_BIN`). Every other test builds an `AppState`
  in-process and calls the router, which skips config parsing, drive adoption
  and every CLI default — exactly where the above hid. Tests that agree with
  each other prove nothing.
- **verified on real hardware:** #92's write path, end to end on dev.g8.lo
  (Fedora 6.17.1) — engine over a `role=data` slab, volume created, attached
  nvme-tcp, `nvme connect` from the host kernel, `dd` writes at several
  offsets and block sizes all succeed, and 1 MiB of random data written
  through the initiator compares equal on read-back. The seal guard, the
  synonym surface (`?since`, `If-None-Match` → 304) and `claim` were exercised
  against the same running engine.

## [v13.3.0] — 2026-09-02

### 2026-09-02
- **feat (volume): writes go to a clone, never to the golden — enforced at the
  attach.** A golden is the master copy and is sealed, so a read-write attach
  of one is now refused with the way forward in the message (clone it, or
  attach `mode=ro`), rather than letting a guest boot onto storage that
  answers every write with a refusal. `POST /api/v1/volumes/{id}/attach` takes
  `mode` (`rw` default, `ro`); `ro` is an assertion about intent, not a
  transport lock — the engine's own gate is what refuses the write, and it
  answers "write protected" when it does.
- **feat (volume): `POST /api/v1/synonyms/{ns}/{name}/claim`.** What a
  consumer wants from a name is not the golden it resolves to but a
  copy-on-write clone of it, with its own filesystem identity, costing nothing
  until written. A claim resolves, clones, and binds a name to the clone in
  the caller's own namespace — one golden behind many consumers, each holding
  a name of its own, each writing only to its own clone. A second claim
  re-points that consumer's name at its new clone. Claiming an unsealed target
  is refused unless `unsealed_ok=true`: an unsealed volume may be changing
  under the copy, which is the caller's consistency question to own out loud.

## [v13.2.0] — 2026-09-02

### 2026-09-02
- **feat (volume): synonyms — a stable name that points at a volume, and can
  be re-pointed at a new version.** A consumer refers to storage by a name it
  chose once; what the name should mean changes when a golden is imported or a
  version rolled back, and nothing carried that. `/api/v1/synonyms` is the
  binding, kept apart from the volume on purpose — a volume is extents, a
  synonym is a pointer, so dropping a name never touches data, and deleting a
  volume a name still points at is refused (`force=true` to leave it dangling
  knowingly, since a dangling name fails as *a machine does not boot*).
  Namespaced (`images/nginx`; a bare name is the `default` namespace) and
  persisted as `<data_dir>/synonyms.json`.

  **The version is how a client knows.** Every re-point bumps a monotonic
  `version` and pushes the old target onto a capped history; `?since=N`
  answers `changed: true|false` with the current target in the same round
  trip, and the same question in HTTP spelling — `If-None-Match: "N"` against
  the version-as-ETag — answers 304. A rollback goes *forward* in version: a
  client that saw the bad publish has to see a change when it is undone.

  A target is a volume on this node or a URI another node serves
  (`nvme-tcp://…`), and resolution says which, so a caller learns it is being
  sent off-node rather than finding out when the I/O is slow. Resolution also
  reports what the target is right now (size, `sealed`, `access`, `role`).
  Synonyms resolve wherever a volume is named by id or name, and the volume
  manager is asked first, so a synonym can never shadow a real volume.

## [v13.1.0] — 2026-09-02

### 2026-09-02
- **fix (slabs): a formatted slab is registered as one that keeps its own
  record.** `POST /api/v1/slabs` added the slab to the registry and stopped
  there, so nothing ever wrote the volume record into the region the slab had
  just reserved for it. Only adoption did that — which meant a slab formatted
  through the API held volumes that vanished on the next restart. Verified on
  forge: two 32 GB images imported, the service restarted, and both came back
  adopted from the drives with their content byte-identical.
- **fix (volume): an adopted volume keeps the role of the slab it lives in.**
  Adoption rebuilt handles with the default placement policy, so a volume
  adopted out of a data slab came back as `system` and could not allocate into
  the slab it was sitting in. It now derives the role from where its extents
  actually are — the same rule `restore` uses and for the same reason (#88).
- **fix (#92, #93): a volume is created where the node actually has slabs.**
  `PlacementPolicy::default()` is `SlabRole::System`, and nothing between
  `POST /api/v1/volumes` (or `/volumes/import`) and `create_volume_with` asked
  the node what it has — so on a registry box, whose drives carry only
  `role=data` slabs, every volume was created system-side. The create
  succeeded, because a thin create allocates nothing, and then every write
  failed at the first allocation with `no space: no system slab apart from 0
  domain(s)`: #93 as the import job reports it, #92 as an NVMe initiator sees
  it (reads work — unallocated extents read as zeros — and every write at
  every offset fails, with buffered writes lying until flush). The role is now
  settled once at create: `CreateOptions.role` is optional, so "the caller did
  not say" is distinguishable from "the caller said system", and
  `SlabRegistry::default_role()` answers the first case with the role the node
  can reach. The boundary itself is unchanged and still hard (#88).
  `ImportSpec` and `POST /api/v1/volumes` both take an explicit `role`, and a
  create reports the role it was really placed in.
- **fix (#92): out of space is not a media error.** The failed write came back
  as `sct 0x2 / sc 0x81` — *unrecovered read error*, on a write, for a volume
  with nothing wrong with its media, which sends an operator to the drive.
  `DriveError::NoSpace` carries the real reason out of the engine: NVMe
  answers Capacity Exceeded (generic 0x81, ENOSPC at the initiator), iSCSI
  answers DATA PROTECT / space allocation failed (0x27/0x07), and a genuine
  write failure is a write fault (media 0x80) rather than a read error.
- **feat (volume): `access` — read-write or read-only, at any point in a
  volume's life.** Sealing was the only way to stop a volume taking writes,
  and it is the wrong lever: sealing says what a volume *is*, the master copy
  clones descend from. `Access` (`rw`/`ro`) is a setting on an ordinary clone,
  it moves both ways, and it is persisted (metadata **V6**; a V5 record loads
  `rw`). Sealed still wins — a sealed volume is read-only whatever its access
  says, and unsealing does not silently make a read-only volume writable — so
  the two are reported separately: `access` is the setting, `writable` is
  whether a write would land. `GET`/`PUT /api/v1/volumes/{id}/access`, both
  fields on every volume response, `spec.access` and `status.writable` on the
  kube Volume. A refusal says which gate closed it: NVMe answers "namespace is
  write protected" (command-specific 0x20), iSCSI answers DATA PROTECT — which
  is also what `write_protected()` should always have been rather than ILLEGAL
  REQUEST, since initiators retry a bad command and a protected medium
  differently.
- **fix (volume): an empty manager no longer overwrites a record of real
  storage.** Knowing about no volumes is not the same as there being none. A
  restart that came up before its slabs were attached held nothing, and the
  next persist replaced a two-volume record with an empty one — the extents
  survived in the slot tables, but nothing was left to say which volume they
  belonged to or what it was called. `persist` now refuses to write an empty
  set over a non-empty record and says why.
- **feat (drives): the engine adopts the storage on its configured drives at
  startup.** Slabs were only ever registered by an explicit call, so an
  appliance whose drives *are* its storage pool came back from a restart with
  the pool invisible — and the only other way to register a slab is to format
  it, which is the wrong answer to "where did my volumes go".
- **feat (nvmeof): `export_drives` — do not publish the storage pool raw.**
  Publishing every configured drive as a namespace is right when the drives are
  what the node serves, and wrong when they are the pool it allocates from:
  there it hands every initiator an unmanaged second writer beside the volume
  exports that are the intended door. Defaults to true, so nothing changes for
  the file-per-image layout; forge sets it false.
- **fix (config): a command-line override no longer drops the rest of the
  `[nvmeof]` section.** It rebuilt the struct field by field, silently
  discarding everything the overrides did not mention, which is why a `nqn` set
  in the config file appeared to do nothing whenever the command line touched
  that section at all.
- **feat (slabs): `POST /api/v1/slabs` takes `metadata_bytes`, and a `data`
  slab reserves a region by default.** A slab with no metadata region cannot
  record what volumes are on it, so that statement lives only wherever the
  engine happened to keep it — and storage that arrived as a drive has no such
  place. **Not yet verified end to end:** forge still comes back from a restart
  with its slabs adopted and no volume records, so the write half of this is
  unfinished.
- **feat (volume): `POST /api/v1/volumes/{id}/tier` moves a volume between
  tiers, online.** The newest image belongs on the fastest drive and last
  month's does not. Every extent not already on a slab of the target tier is
  migrated to one, one extent per lock cycle so the volume keeps serving while
  it moves; the id, name, contents and exports are unchanged. The destination
  must match the volume's *role* as well as the tier — a data volume is not
  demoted onto a system slab, because the roles say different things about what
  an install may erase. Shared extents follow correctly: `migrate_leg` rewrites
  every map that named the old slot, so a golden and the disks composed from it
  move together rather than being torn apart.

  Measured on forge: 5328 extents of a 32 GB image moved from an 800 GB SSD to
  a 2 TB spinning drive, zero failures, and the image read back byte-identical.
- **feat (volume): the create API can place a volume by role.**
  `POST /api/v1/volumes` gains `role` (`system` or `data`, default `system`).
  `CreateOptions::in_role` existed in the library and nothing could reach it,
  so an appliance whose slabs are all `data` — content meant to outlive a
  rebuild of the box — could not create a volume at all: every request asked
  for a system slab and found none. The response now reports the role the
  volume actually has rather than the constant `system`.
- **fix (drive): a whole-drive slab is discovered.** `slabs_in_partitions`
  read a GPT and looked inside partitions, so a drive that is *itself* a slab —
  what `POST /api/v1/slabs` on a plain device produces — was found by nothing.
  A store built that way survived exactly as long as the process that made it.
- **fix (volume): an adopted slab keeps its own record.** Adoption now marks a
  slab with a metadata region as one this manager persists into. Storage that
  arrived as a disk has no data directory of its own, and a store whose contents
  live only in the running process is not a store.
- **feat (drives): adopt the storage already on a drive.**
  `POST /api/v1/drives/{id}/adopt` opens every slab in a drive's partitions and
  restores the volumes they describe, without writing anything. An appliance
  handed a whole-disk image could serve it as a namespace and do nothing else
  with it — the goldens inside are volumes in a slab in one of its partitions,
  and nothing had opened them, so an image's contents could only be reached by
  booting a node from it. Measured on forge: `stormcos-sno-10.21.img` yielded
  its data and system slabs and **102 volumes** in about six seconds, and the
  51 goldens among them became composable.

  A slab whose slot size disagrees with the engine's extent size is refused,
  naming both. Adoption is otherwise a door into the engine from a disk someone
  else formatted, and that mismatch is exactly the defect that corrupted the
  serving path.

  Safe to repeat: a slab already attached and a volume already known are counted
  and left alone. Adoption lasts for the run — it is an action against a drive
  the engine has open, not a change to what it is configured to hold.
- **refactor (drive): slab discovery moves into the library** as
  `drive::discover::slabs_in_partitions`, so the management API and the
  `boot-local` path find slabs the same way rather than one of them having a
  private copy.
- **feat (volume): compose a volume out of other volumes — a disk that is a
  *list of* goldens rather than a copy of them.** `POST /api/v1/volumes/compose`
  takes a name and a list of `{volume, at}` components and builds an extent map
  that shares their slab slots. Nothing is read and nothing is written: what it
  costs is the map. Copy-on-write already covers the rest — a consumer that
  writes to a composed disk gets its own slot for what it changed and keeps
  sharing everything else, so one golden is safe to hand to a fleet.

  A snapshot was this with one source and no offset (`clone_volume_map`); the
  missing piece was placing several sources at several offsets, now
  `GlobalExtentMap::gather_into`. Offsets must be slot-aligned, because an
  extent map cannot express anything else and would silently place the
  component at the slot below; components may not overlap, because each would
  believe it owned the shared extent. Both are refused with the offending pair
  named.

  What it is for: a stormcos image lands every golden twice today, once into a
  pallet partition and once into the slab — 8.7 GB of partitions and 2.3 GB of
  slab for about 2.4 GB of distinct content. Composed, a fleet is one set of
  goldens and one small map per node, and cutting a version writes maps instead
  of gigabytes.
- **feat (nvmeof): every configured drive is a namespace, in config order.**
  Only the first was exported, and only when there was exactly one — a second
  drive was reachable by no initiator at all, so the only way to put content on
  an appliance was to build it elsewhere and copy the finished file over.
  Namespace *n* is now the *n*th drive in the configuration, from 1, and each
  is logged with its path and size at startup because that ordering is the only
  contract an initiator has for telling them apart. This is what lets a drive
  be created on the appliance that will serve it, attached from the build box,
  written in place, and detached — with `image build --out <device>` above, the
  bytes never leave the machine that owns them.
- **feat (image): `image build --out` accepts a block device and writes the
  disk in place.** The point is where the device can come from: an appliance
  exports a drive over NVMe/TCP, the build box attaches it, and the image is
  built onto the machine that will serve it — there is no 32 GB file to copy
  afterwards. Previously `--out` unlinked whatever was already at the path,
  which for a device node deletes the *node*; the next open then created an
  ordinary file under `/dev`, and the build succeeded while serving nothing.
  A device is now opened as it is found, its size checked rather than
  extended (`TooSmall` names both), and the file path keeps the sparse-create
  behaviour it had. A device is left as found outside the regions the image
  writes, so a byte-for-byte reproducible image wants one nothing has written
  yet — which is what an appliance's freshly created sparse drive is.

### 2026-09-02
- **fix (serving): a volume extent is a slab slot, and the server was sizing
  it as neither.** `stormblock --config` built its `VolumeManager` with
  `DEFAULT_EXTENT_SIZE` (4 MiB) while slabs are formatted with
  `DEFAULT_SLOT_SIZE` (1 MiB). The volume layer divides an offset by that
  value to choose an extent and hands the remainder to the slab as an offset
  *within one slot* — so with a 4 MiB extent size, extent 0 was written across
  physical slots 0-3, extent 1 across 4-7, and each extent's overflow
  overwrote the next three. Every write was acknowledged. The volume read back
  with whole megabytes of zeros and of other extents' data scattered through
  it, and the damage only appeared once more than one extent had been written,
  which is why a small write looked fine and a 32 GB image did not.
  The serving path was the only one affected: `boot-local` and `image build`
  take their extent size from the slab they opened, so every image this engine
  built was correct while everything it served over NVMe/TCP was not. The
  server now takes its extent size from `drive::slab::DEFAULT_SLOT_SIZE`, and
  the pool-pressure watcher uses the same value rather than a second constant.
- **fix (slab): an offset past the end of a slot is refused, not aliased into
  the next one.** `slot_device_and_offset` bounds-checked the slot index but
  not the offset inside it, so `slot 1 + slot_size` and `slot 2 + 0` resolved
  to the same address — which is what turned the extent-size mismatch above
  into silent cross-extent corruption instead of a failed write. Any caller
  whose extent size disagrees with the slab's slot size now gets an error
  naming both.
- **fix (tests): `integration_image` did not compile.** It asserted on
  `VolumeReport::allocated`; the field is `allocated_bytes`. Introduced with
  feb3fd3, and it took the whole test binary with it — 686 tests now build and
  pass.

### 2026-09-01
- **fix:** **a whole-disk slab path yields every slab in the GPT** (#88
  follow-on), not the first that opens. A node boots with the disk on its
  command line, not a partition — `rd.stormblock.slab=/dev/sda` — and the data
  slab is allocated first so that growing the system slab across a release
  cannot move the partition holding the node's identity. That ordering made
  "first slab found" the data slab: a node attached identity storage, looked
  for `stormblock.volume=stormpump` inside it, and had no root device. The
  failure read as a missing volume rather than as the wrong partition, which
  is the kind of thing that sends you looking in entirely the wrong place.
  One path can now produce several slabs, so the per-slab metadata reporting
  zips against the source path recorded per slab rather than against the paths
  given, which are no longer 1:1 with the slabs opened. Unblocks
  glennswest/stormpump#12.

- **feat:** **a clone can cross the slab-role boundary, as a copy** (#88
  follow-on). A copy-on-write clone shares its source's slots, so a clone is
  only as durable as the slab its source is in — cloning a *system* golden
  gives a system volume however it is named, and an install replaces every
  extent it never wrote. Sharing is what makes clones free and sharing is
  what ties them to a partition; they are the same property, so the crossing
  has no cheap form. `POST /api/v1/volumes/{id}/clone` now takes
  `{"role": "data"}` and performs a real copy — holes not copied, lineage
  recorded, its own filesystem UUID — that shares nothing with its source and
  survives that source's slab being reformatted. Within a role, and by
  default, cloning is the ordinary free copy-on-write.
- **feat:** every volume reports `role` (`system` or `data`) — on
  `/api/v1/volumes`, and on the `Volume` kube resource as `spec.role` plus a
  `storm.io/slab-role` label, so `kubectl get volumes -l
  storm.io/slab-role=data` is a check. A clone is in its source's half
  whatever it is called, so the name is not evidence; this makes "is the
  volume I meant to be durable actually in the data slab?" a question with an
  answer.
- **docs:** `docs/images.md` §2a2 — why the blank a `-data` volume clones from
  has to be in the data slab, with what the extent map looks like when it is
  not. In an image spec the mistake is unspellable (each slab is filled with
  only itself attached); at runtime it was silent, and now it is neither.
- **fix:** an image spec that names a section the builder does not know is
  **refused**, not ignored (#81). `[[slab.clone]]` where `[[slab.golden]]` was
  meant built cleanly, reported success, and produced an image with two
  volumes missing; the symptom arrived one image, one copy and one boot later
  as a root device that never appeared, pointing at the mount list rather than
  at the spec. A spec is hand-edited and has no schema anywhere else. The cost
  of silence is higher now that `[data_slab]` exists: a misspelt section there
  puts the node's identity back in the partition an install replaces.
- **fix:** `evacuate_slab` **skips** an extent it cannot move instead of
  stopping at it, and returns which ones were left behind and why (#67). The
  comment said "skip this extent to avoid infinite loop" and the code broke
  out of the loop — the worst behaviour available for the case the call
  exists to serve, since one unreadable extent abandoned everything still
  readable on a failing drive. Skipping is also what actually avoids the
  loop: a failed extent is still in the GEM on the next pass.
- **fix(test):** the `/v1` attach tests pin the transport to nvme-tcp. The
  local ublk fast path is on by default, so `v1_rw_attach_only_on_master`
  answered `ublk` on any Linux host with `ublk_drv` loaded and `nvme_tcp` on
  one without — it was measuring the build host, not the contract.

## [v13.0.0] — 2026-09-01

### Added
- **A node's mutable storage is two slabs, and an install replaces only one of
  them (#88).** Tier-0 — the node CA private key, the apiserver serving cert
  and the **ServiceAccount token signing key** — sat in the same slab as the
  goldens, so anything that reformatted the slab to replace the goldens
  re-minted the node's identity. A re-minted signing key invalidates every
  ServiceAccount token in the cluster, and the node comes up looking healthy.
  `[data_slab]` in an image spec is a second slab partition with its own GPT
  type GUID (`7D3E5A91-6C24-4B8F-A05D-2E9147BC6F38`) and a role byte in its
  header. It is allocated *before* the system slab, which is the half that
  says `rest` and the half an image replaces, so growing one across a release
  does not move the other.
- `stormblock slab format --role data` and `POST /api/v1/slabs
  {"role":"data"}`. Both refuse to overwrite an existing data slab unless that
  same request says `data` — the role is asked of the device, since a caller
  supplies a path and a path proves nothing.
- `role` on `/api/v1/slabs`, `/api/v1/drives/{id}/slabs`, the `Slab` kube
  resource (`spec.role` and the `storm.io/slab-role` label) and `image
  inspect`.
- `VolumeManager::persist_to_slabs`, `metadata_slabs`, `is_metadata_slab`;
  `CreateOptions::in_role`; `SlabFormat::with_role`; `Slab::role` / `is_data`;
  `SlabRegistry::role_of`, `best_slab_for_tier_in_role`,
  `distinct_domains_with_space_in_role`.

### Fixed
- **`boot-local --local-disk` no longer formats a data slab.** A reinstall is
  "boot a fresh image and flow over onto the disk the previous install was
  on", and that disk held tier-0. The target is now judged by what is on it —
  the GPT type of any partition, then each slab's own header — and refused by
  name. A flow-over also never migrates a data slab's extents onto the system
  disk, which would put identity back in the half the next image replaces.
- An ISO conversion drops the data slab along with the system slab; it was
  carrying an empty partition into every installer image.

### Changed
- **Each slab carries its own `volumes.dat`.** The data slab's record of
  itself has to survive the system slab being replaced, so it cannot live in
  the system slab. A boot given several slabs reads each one's copy and merges
  them; each volume is written back to the slab its extents are on. A slab
  with no copy of its own is the older single-document arrangement, paired
  positionally as before.
- **The slab role is a hard allocation boundary.** A system volume never takes
  a slot in a data slab and a data volume never takes one in a system slab,
  and clones inherit the role from their source — otherwise the split leaks
  one copy-on-write extent at a time, which is the same loss more slowly. A
  volume's role is derived at restore from where its extents already are, so
  there is no metadata version to bump and no way for the record and the
  placement to disagree.

### Breaking
- `PlacementPolicy` gains a `role` field; struct literals need
  `..Default::default()`.
- `SlabRegistry::best_slab_for_tier_apart_from` takes a `SlabRole`.
  `best_slab_for_tier`, `best_slab` and `distinct_domains_with_space` now
  consider system slabs only; the `_in_role` variants are the general form.
- `VolumeManager::persist_to_slab` still works and now sets a one-element
  list; `metadata_slab()` returns the first of several.

### Documentation
- `docs/images.md` §2a1 — why the split is a partition boundary, what is
  enforced rather than documented, and where the blank a `-data` volume clones
  from has to live.

### Also in this release

### 2026-08-31
- **fix(ublk):** an export is not created until its device node exists. The
  id arrives at ADD_DEV but the block device only appears at START_DEV — a
  ~60 ms gap — and the attach API returned the path in between, so a
  hypervisor spawned against it died at "Could not open /dev/ublkbN" while
  the log said the export was created. Bounded wait for the node, and a
  refusal that names the volume if it never appears.

### 2026-08-31
- **feat(initramfs):** the node comes up **on a bridge** (`stormbr0`, with the
  uplink as a port), because a VM's NIC is a tap and a tap has to hang off
  something. Without it the only options are NAT — a private network the LAN
  cannot reach — or macvtap, which deliberately stops the node talking to its
  own guests. **With a fallback**: if any part of it fails the uplink is left
  as it was and the boot carries on with plain DHCP, because a node with no VM
  networking is a node and a node with no networking is a recovery job. The
  node's name still comes from the *uplink's* MAC: a bridge takes a random
  address until it has a port, so naming a machine after it would rename it
  every boot.
- **feat:** `POST /api/v1/volumes/{id}/cidata` — make a volume a **cloud-init
  seed**, as vfat with the label cloud-init actually looks for. The medium is
  the contract: NoCloud wants vfat or ISO 9660 labelled `cidata`/`CIDATA`, and
  an ext4 volume with the same label and the same files is not picked up —
  which presents inside the guest as `Did not find any data source, searched
  classes: ()` with the disk sitting right there. vfat rather than ISO because
  this engine already writes FAT with real long-name entries (`meta-data` does
  not fit 8.3) and a seed is per VM and writable, which an ISO is not.
  `image::fat::format_from_files` writes a set of in-memory files into the root
  of a labelled volume, with nothing staged on the node.
- **feat:** a **raw image imports straight off the wire**, with nothing staged.
  Raw is sequential, so spooling it to `<data_dir>/imports/` only meant needing
  room for the image's whole *virtual* size on a node about to store just the
  parts that are used — a 32 GB image with 9 GB in it failed with ENOSPC while
  the volume it was headed for had room three times over. The body is consumed
  through a bounded channel (backpressure: a slow disk slows the download), and
  only non-zero windows are written, so a sparse image still costs what it
  uses. qcow2 and VMDK still stage — their decoders seek.
- **feat:** clone a volume by **name** as well as by uuid. A golden is named by
  everything that references it, and nobody carries the uuid around.

### 2026-08-30
- **fix:** a ublk export lets the **kernel** assign its device number. It asked
  for `/dev/ublkb0`, then `1`, from a counter that starts at zero in a fresh
  process — on a node already serving 39 devices from boot — and a *requested*
  id makes `UblkServer` send STOP_DEV and DEL_DEV first. So the first API
  attach on a booted node deleted `/dev/ublkb0` out from under the filesystem
  mounted on it: on the machine this was found on, `/var/log/pods` went to
  "lost async page write", the kernel shut the filesystem down, and every later
  write returned EIO — while nothing said a block device had been deleted.
- **fix:** this node's name comes from the **kernel** when nothing else
  supplies one. Nothing exports `HOSTNAME` but a login shell, so a stormblock
  started by an init system called itself `localhost` on a node named
  `storm-2c91b3`, and every local attach failed with what read as a transport
  error. Both copies of the fallback are now one function — two copies is how
  `/v1` and the attach path came to disagree about the node's own name.
- **fix:** `management.ublk_transport` is **on by default**. Every guard that
  makes a local attach safe is checked at the call site — the volume must be
  backed here and the request must name this node — so the flag guarded
  nothing those did not, while its absence produced `409 Conflict: ublk is a
  local device …, or ublk_transport is off` on a node already serving 39 ublk
  devices. Where `ublk_drv` is missing the engine still falls back to nvme-tcp,
  which is what makes the default safe.
  Both found by a VM that would not start on a real node.

## [v12.4.0] — 2026-08-28

### 2026-08-28
- **feat:** Whole-disk goldens. `fs/disk.rs` recognises a GPT or MBR
  partition table and an ISO 9660 image on a volume (`fs.kind = gpt | mbr |
  iso9660`, `uuid` = disk GUID / MBR signature) — `seal` needs no `force`
  for a VM disk — and every clone of a `gpt`/`mbr` golden gets a fresh
  **disk identity** (both GPT headers re-CRC'd; MBR signature) so clones
  attached to one host do not collide on PARTUUID. An ISO has nothing to
  stamp and is left alone. What is *inside* the partitions stays the
  guest's job (cloud-init / sysprep).
- **feat:** Disk-image readers (`image/decode/`): **qcow2** (v2/v3, zero
  clusters, zlib-compressed clusters; backing files, external data files,
  extended L2 and zstd refused by name), **VMDK** (monolithicSparse,
  streamOptimized with compressed grains and the footer directory, and the
  text descriptor with FLAT/SPARSE extents), and the VMDK read straight
  out of an **OVA** (ustar walk, no extraction). Detected by magic, never
  by extension — a cloud image called `.img` is a qcow2. `[[slab.golden]]
  from=` accepts all of them.
- **feat:** `POST /api/v1/volumes/import {name, file|url, format?,
  redundancy?, size?, seal?}` — an async job (`GET …/import/{id}` for
  progress) that streams a URL to `<data_dir>/imports/` (never in memory),
  writes only the clusters the image carries, and seals the result with
  its disk shape recorded. This is how a cloud image, a VM export or an ISO
  becomes a golden. `http::Client::get_to_file` streams the download.
- **feat:** The image build report carries `sealed` and `fs_uuid` per
  volume, and stamps a disk identity on the first clone of a disk golden.

## [v12.3.0] — 2026-08-28

### 2026-08-28
- **feat:** Kubernetes-shaped resources served by the engine (#80):
  `/apis/storage.storm.io/v1/{volumes,slabs,drives,nodes}` in the
  `apiVersion/kind/metadata/spec/status` shape, API discovery at `/apis`,
  `/apis/storage.storm.io` and `/apis/storage.storm.io/v1`
  (APIResourceList), `labelSelector`, `?watch=1` as a newline-delimited
  `{type, object}` stream, Kubernetes `Status` error bodies. Writes:
  `PATCH volumes/{name}` `spec.{redundancy,sealed,retention,resync}`,
  `DELETE volumes/{name}` (refused while exported), `PATCH drives/{name}`
  `spec.{labels,drain}`. `metadata.name` is the uuid; the human name is
  `spec.name` / label `storm.io/name`; get accepts either. Projections of
  the state the REST API serves — no second store. stormdrive serves
  `drives`/`enclosures` in the same group.

## [v12.2.0] — 2026-08-28

### 2026-08-28
- **feat:** Dependency cut (#79). `src/http.rs` — a pooled HTTP(S) client on
  `hyper-util` + `hyper-rustls`, the subset of `reqwest` the engine used
  (`post/get/put/delete`, `json`, `send`, `status`, `json`, `text`, a
  timeout, an extra CA) — replaces `reqwest` in cluster RPCs, heartbeats,
  replication, migration and the StormFS announce (`reqwest` stays a
  dev-dependency for tests). `mgmt/metrics.rs` — an in-house `metrics`
  recorder that renders the Prometheus exposition format on the axum route
  replaces `metrics-exporter-prometheus`. `toml` → `basic-toml`. The
  embedded management UI (`ui`) is **off by default**: the engine serves
  an API, stormview is the UI. Measured with `cargo tree -e normal`:
  default build 335 → 212 crates, `mikrotik,nvmeof` 262 → 186,
  `Cargo.lock` 384 → 354.
- **fix:** The rustls crypto provider is installed explicitly
  (`http::ensure_crypto_provider`), so a build that has both `aws-lc-rs`
  and `ring` in its graph (any test build, via dev-dependencies) does not
  panic building a TLS config.
- **feat:** Per-drive metrics (#68) — `stormblock_drive_{healthy,
  temperature_celsius, media_errors, available_spare_pct, power_on_hours,
  capacity_bytes}{drive,serial}` sampled from every open drive at scrape
  time; `stormblock_drives_total` and `stormblock_capacity_bytes` refreshed
  at scrape rather than set once at startup. Prometheus itself runs
  elsewhere; `/metrics` is what it scrapes.
- **feat:** `stormblock image build` seals every golden it lays down and
  records its filesystem (#77), and stamps each first clone with its own
  UUID; the build report carries `sealed` and `fs_uuid`. A blank arrives
  cloneable; the claim path asserts instead of repairing.
- **feat:** Discovery beacons carry `topology` and `topology_chain` (#72),
  so `/v1/nodes/capacity` reports the chain for peers, not only the local
  node.

## [v12.1.0] — 2026-08-28

### 2026-08-28
- **feat:** Attach is a volume operation (#78) — `POST/GET/DELETE
  /api/v1/volumes/{id}/attach` serves any engine volume and returns the
  same `AttachInfo` `/v1` does: a local `ublk` device when this node is the
  one attaching and `ublk_transport` is on, `nvme_tcp` with the shared
  subsystem's NQN, address and this volume's NSID otherwise. `transport`
  may name one; a `ublk` that cannot be offered is refused (409), not
  downgraded. Idempotent; detach tears the device down and withdraws the
  namespace. A volume no longer has to have come through `/v1` to get a
  block device — the PVC path's last gap.
- **feat:** `/v1` `source: {kind: "volume", id}` falls back to an engine
  volume by id or name when the id is not a `/v1` volume, so a blank the
  image shipped or a golden sealed through `/api/v1` can be cloned from
  either door.

## [v12.0.0] — 2026-08-28

The template/volume split is gone as a *model*: a template is a volume that
has been sealed, and lineage, sealing and filesystem identity are recorded
on every volume. The `/api/v1/fstemplates` surface is unchanged in shape;
what changed underneath is that `seal` no longer makes a second volume.

### 2026-08-28
- **feat:** A template is a volume that has been sealed (#76). Lineage,
  sealing and filesystem identity are volume facts now — metadata **V5**
  records `parent`, `sealed` and `fs` (kind, journal, features, 64bit,
  metadata_csum, csum_seed, label, uuid); `create_snapshot` records the
  parent and inherits `fs`; a sealed volume refuses writes, discards and
  shrinks (`VolumeError::Sealed`). `VolumeManager::seal_volume`,
  `unseal_volume`, `set_fs_info`, `set_fs_uuid`, `parent`, `children`,
  `lineage`, `find_volume`.
- **BREAKING (behaviour):** `fs::template::seal` seals the raw volume **in
  place** instead of snapshotting it into a second volume and deleting the
  first — one template is one volume, so the `-raw` half that leaked (#47)
  no longer exists. `sealed_volume_id` is the volume that was formatted; a
  two-phase template that is still exported keeps its export, which now
  refuses writes.
- **feat:** One clone path — `fs::template::clone_volume(vm, source, spec)`
  snapshots any sealed volume, stamps a fresh filesystem UUID when the
  source carries a filesystem, fscks, and records the clone's own `fs`.
  `clone_template` is that plus template bookkeeping; the volume snapshot
  API and the /v1 `source: volume` path stamp too, so two live filesystems
  never share a UUID whichever door minted them.
- **feat:** `POST /api/v1/volumes/{id}/seal` (reads the ext superblock into
  the record; `DELETE` reopens), `POST …/{id}/clone`, `GET …/{id}/lineage`;
  `parent`, `sealed`, `fs` and a real `fs_uuid` on every volume response;
  `from_template` on `POST /api/v1/volumes` also accepts a sealed volume by
  id or name — the blank-filesystem-built-into-the-image case that was in
  neither namespace. `CloneResult.template_id` is now optional, beside
  `source`.
- **feat:** `fs::template::adopt_into_volumes` — at startup every sealed
  template's volume is marked sealed and given its `fs`, so a store written
  before V5 reads the same as one written after.

## [v11.0.0] — 2026-08-28

Major by the size of the change (the house rule), not by breakage: every
surface here is additive. This is the drive-plane half of volume-level
redundancy — a drive can be reported failing, quarantined, drained and
pulled; data spreads by failure domain after a shelf is added; the parity
write hole is bounded by a dirty-stripe log; and a policy can be re-striped.

### 2026-08-28
- **feat:** Drain over HTTP (#70 item 3) — `POST/GET/DELETE
  /api/v1/drives/{id}/drain`. `src/drain.rs` moves every leg (data and
  parity) off every slab on the device one extent at a time, locking per
  extent and yielding between, so I/O keeps flowing; slabs being drained are
  quarantined; a leg that fails to move is skipped and listed. Terminal
  `empty` = safe to remove; `stuck` keeps the quarantine. Refused for the
  slab holding the volume metadata.
- **feat:** Drive health inbound (#70 item 4) — `POST /api/v1/drives/{id}/health`
  quarantines the drive's slabs and puts them in the failed set of every
  *redundant* volume with a leg there (`VolumeManager::distrust_slab`; an
  unreplicated volume's only copy stays trusted); `failed`/`missing` or
  `drain: true` starts a drain; `healthy` lifts the quarantine.
- **feat:** `SlabRegistry` quarantine — `set_quarantined` keeps a slab out of
  every allocation path (`best_slab_for_tier`, `…_apart_from`, `best_slab`,
  `distinct_domains_with_space`, placement destinations) while leaving what
  is on it readable and writable.
- **feat:** `RebalanceStrategy::ByFailureDomain { rung }` (#71 item 3) —
  separates legs of one extent or stripe that share a domain at the rung,
  then evens allocation out across the domains.
- **feat:** Node topology as a chain (#72) — `[management].topology` sets the
  registry's node labels under every slab's domain; `/v1/nodes/capacity`
  reports `topology_chain` for the local node.
- **feat:** Dirty-stripe log (`volume/stripelog.rs`) — a parity volume with a
  data directory marks a stripe before its read-modify-write (one fsync per
  stripe per flush interval), `flush` clears the log, and restore
  recomputes exactly the stripes a crash left marked
  (`ThinVolumeHandle::verify_stripes`).
- **feat:** Restripe — `VolumeManager::restripe` /
  `POST /api/v1/volumes/{id}/restripe` changes a policy to or from parity by
  copying into a scratch placement and swapping the map
  (`GlobalExtentMap::rename_volume`); refused while exported.

## [v10.0.0] — 2026-08-28

Major by the size of the change, not by breakage: every API and file format
added here is additive and a V3 `volumes.dat` still loads. What changed is
where redundancy lives — it is now a property of a volume, placed across
failure domains, rather than of a drive.

### 2026-08-28
- **feat:** Volume-level redundancy — RAID is a property of a volume, not
  of a drive. `RedundancyPolicy` (`none`, `mirror:N`, `raid5:D+1`,
  `raid6:D+2`, `@rung`) places every leg of an extent — and every member
  and parity leg of a stripe — on a distinct failure domain, as a hard
  boundary: creation is refused (409) when the node cannot satisfy it. A
  node carries a mix on the same drives. Mirror writes go to every leg;
  parity writes are read-modify-write under a per-stripe lock with
  reconstruction from P (one loss) or P+Q (two). Clones inherit the
  policy. A slab a write fails on joins the volume's persisted **failed
  set** and is never read again until `resync` rebuilds what was on it.
  `docs/redundancy.md`.
- **feat:** `FailureDomain` (`placement/domain.rs`) — a `rung=value` chain
  `site/…/rack/node/hba/shelf/bay/drive` (#72's vocabulary, #71's
  input). Slabs carry one (device identity under the drive's labels);
  `SlabRegistry::best_slab_for_tier_apart_from` is the domain-aware
  allocation. Unknown domains constrain nothing; empty chains are treated
  as shared.
- **feat:** GEM legs — `ExtentLocation.mirrors`, `ParityGroup` per stripe
  (with `data_width`), reverse index over every leg, `rewrite_legs` /
  `add_leg_beside` / `drop_leg_everywhere` so a leg rebuilt for a golden
  is rebuilt for every clone sharing it. `rebuild_from_slabs` recovers
  legs (same extent, same generation) and parity slots (`PARITY_TAG`).
- **feat:** Volume metadata **V4** — legs, parity groups, policy and failed
  set persisted; V3 loads as unreplicated. Restore is record-first: the
  slot table wins only where it is provably newer (a higher-generation
  slot for the same extent), and `Slab::allocate_gen` now records the
  copy-on-write generation so that comparison means something.
- **feat:** `GET /api/v1/volumes/{id}/health`, `POST …/resync[?verify]`,
  `PUT …/redundancy`; `redundancy`, `health`, `physical_bytes` on every
  volume response; `redundancy` on `POST /api/v1/volumes` (with it,
  `array_id` is optional), on `POST /api/v1/fstemplates`, on
  `[[volumes]]` and on `--volume name:size:policy`.
- **feat:** stormdrive integration surface (#70 items 1–2): `labels` and
  `uuid` on `POST /api/v1/drives`, `PUT /api/v1/drives/{id}/labels`,
  `GET /api/v1/drives/{id}/slabs`; `SlabRegistry::label_device` widens
  every slab on the device. `domain` on slab responses and on
  `POST /api/v1/slabs`.
- **feat:** Pallet-level mirror (#56) — `copies` on publish places extra
  legs on drives holding none; a copy lands at priority 0 and takes the
  source's attributes once verified; `status` groups legs by name and
  version (`copies_wanted`, `degraded`); `POST /api/v1/pallets/resync`
  refills a lost leg; `PUT /api/v1/pallets/mirrors` records the policy in
  `<data_dir>/pallet_mirrors.json`.
- **fix:** `GlobalExtentMap::insert` / `remove` / `remove_volume` no longer
  drop a reverse entry the volume does not own — a clone re-mapping an
  extent it shared used to take the source's slot out of the index, so
  evacuation could miss it.
- **fix:** After a copy-on-write releases a shared slot, the owner's
  recorded share count is synced from the slot table, so a source whose
  last clone diverged writes in place again instead of copying for nobody.
- **refactor:** `placement::migrate_extent` moves one *leg* (`migrate_leg`,
  `migrate_parity_leg`); the destination is kept off the domains of the
  extent's other legs; shared slots are followed by every map that names
  them and freed outright.
- **feat:** `volume/stripe.rs` — GF(2^8) parity arithmetic in the RAID-6
  field the drive-level engine uses (asserted equal), with reconstruction
  of one member from P or Q and two from P+Q.
- **feat:** Array members carry their `uuid` and real `device_path` in
  `GET/POST /api/v1/arrays` responses (`RaidArray::member_details`) — the
  uuid is what member removal takes, so an orchestrator can now run the
  full leg-move sequence (add member → rebuild → remove member) over HTTP

## [v9.13.1] — 2026-08-27

### 2026-08-27
- **chore(deps):** `mkfs-ext4` v2.0.4 → v2.1.0 and `fio-ext4` v1.4.1 →
  v1.5.0, moved together so cargo still resolves one copy of `mkfs-ext4`
  (two tags are two source ids, and the `BlockDevice` trait from one does
  not satisfy the other). Brings the fixes for the measured 280x–1065x
  write amplification (mkfs.ext4.rs#4, fio.ext4.rs#3): streamed unpack
  writes each data block once, allocation resumes from the last block
  placed, and the new write-back `CachedDevice` is available for
  write-heavy consumers — this engine's own opens are unchanged, since a
  serving path must not hold completed writes in memory.

### 2026-08-26
- **feat:** NVMe-TCP initiator `BlockDevice` (#73) — `drive/nvmeof_dev.rs`
  attaches a remote NVMe-TCP namespace as a local drive via
  `nvme-tcp://host:port/<nqn>?nsid=N`, accepted everywhere a device path
  is: `[[drives]]` config, `POST /api/v1/drives`, and RAID members
  (`add_member`), which makes a cross-node RAID1 leg possible over the
  fleet fabric. Reuses the target's PDU types (same rule as the iSCSI
  initiator); admin connection (QID 0) identifies at open, one I/O
  connection (QID 1) serialized behind a Mutex, dropped-and-reconnected
  on error so a bounced remote degrades to per-op errors RAID can see.
  `DeviceId.uuid` is uuid5 of the attach URI — stable across reopens (the
  #65 lesson, applied). New `DriveType::NvmeTcp`.
- **feat:** `iscsi://host:port/iqn` accepted by `open_one_drive` too — the
  existing initiator was only reachable from boot-iscsi before.
- **test:** `nvme_tcp_uri_attaches_as_block_device` — URI attach against
  the in-process target: identity stability, block-boundary and
  chunk-crossing round-trips, discard, alignment enforcement.

### 2026-08-25
- **feat:** `state::StateStore` — the engine's own durable state, kept in an
  ext4 volume it reads *itself*. `fs::files` already reads and writes ext4
  directly against a `BlockDevice`, with no mount, no loop device and no ublk
  export, so the engine opens the volume in-process the same way a golden is
  built. Its writers go on doing synchronous file I/O into a working directory
  that is tmpfs on a node — fast, unable to block, and unable to reach any
  volume the engine serves — and the volume is restored into that directory at
  start and captured back on a timer and at shutdown. Only what changed is
  written. The volume is fsck'd when opened, because the engine can be killed
  between the data and the metadata and nothing ever mounted it to make that
  tidy. Verified on a node: state survives a reboot.
- **fix:** a block device's capacity is not its inode's size. `FileDevice` took
  it from `metadata.len()`, which is 0 for a block device node, so `Gpt::read`
  skipped every candidate LBA size as "device too small" and reported that a
  real disk had no partition table. Nothing noticed for as long as the kernel
  command line named the slab's partition directly; the first boot that had to
  *find* the slab failed with "bad slab magic".
- **note:** a node wedged four seconds into every boot with the engine's
  `--data-dir` pointing at a volume the engine itself served over ublk. Every
  volume delete ends in a synchronous metadata write under the volume-manager
  lock, so the engine blocked on storage only it could provide, and every
  container's disk I/O queued behind that lock. Worked around in the stormcos
  boot manifest by moving engine state to tmpfs; the durable fix is for engine
  state to live in the slab rather than in a file on a filesystem the engine
  is responsible for. `VolumeManager::persist` also does synchronous file I/O
  inside an async fn while holding that lock, which blocks a runtime worker as
  well as the lock.

### 2026-08-25
- **perf:** boot is **10 s** power-to-serving, from ~150 s. Ours is 4.6 s of
  it; the rest is OVMF's silent platform phase. See
  `stormcos/docs/BOOT-TIMING.md` for the breakdown and the rules it produced.
- **fix:** the initramfs closes its dependency set over the **source** tree's
  `modules.dep`, not its own. `depmod` records dependencies only between
  modules it can see, so a subset missing `failover.ko` produces a
  self-consistent, complete-looking and wrong map — and the node boots with no
  network because `virtio_net`'s dependency never loaded. Cost two boots.
- **feat:** the initramfs carries only what reaches the root — every storage
  and network driver, plus firmware for storage adapters that load it at probe.
  The rest is a golden in the kernel pallet, bound over `/lib/modules` and
  `/lib/firmware` once root is up. 373 MB → 49 MB.
- **fix:** a ublk handover waits for the *devices* to quiesce, not for the old
  process to exit. It was burning a full 15-second grace on a process that had
  released everything and was holding nothing. 17 s → 0.24 s.
- **fix:** an idle ublk worker notices a shutdown. `submit_and_wait(1)` sleeps
  until the kernel has something to say, so a device with no traffic never
  looked at its shutdown flag — six of seven devices released, the seventh hung
  the handover.
- **fix:** the export reconciler asks the NVMe-oF target how many connections it
  has instead of counting sockets in `/proc`, and stops the target accepting
  before it asks — so a count of zero means finished rather than not-started.
  It had been releasing an export 26 ms after two controllers attached and
  deleting the volume mid-write.
- **fix:** per-export portal ports cycle through the range instead of always
  taking the lowest free one, which had two targets briefly sharing a port.
- **fix:** readiness reports what the engine has done. Four blockers were fields
  inherited from stormblockmk with nobody left to set them, so every node
  reported "slab not open" while serving.
- **feat:** `stormblock attach` — open any slab and list, export or mount any
  volume in it, on a disk, a partition or an image file.
- **feat:** `stormblock must-gather` — kernel state, storage inventory, device
  firmware and NVMe wear/temperature, pstore crash records, the handover record
  and the supervisor's logs, in one directory. Read-only throughout.
- **feat:** `drive::handover` — the incumbent records which volume is behind
  each device it created, so `adopt-ublk` needs no arguments and cannot be given
  a list that is short by one.
- **feat:** `adopt-ublk` serves the management API and `/serve/v1`; the process
  that holds the slab is the engine, and nothing else can answer for it.
- **feat:** the initramfs sets a hostname (DHCP's, else `storm-<mac>`), applies
  its DHCP lease, and prints per-stage timing from `/proc/uptime`.

### 2026-08-24
- **fix:** the initramfs ships the **whole kernel module tree**, compressed as
  the kernel package ships it, rather than a chosen set of subtrees. A driver's
  dependencies are not confined to its own subtree: `net_failover` links
  against `kernel/net/core/failover.ko`, which is under no driver directory, so
  with `kernel/net` left out `depmod` recorded no dependency, `modprobe` loaded
  it bare, the kernel refused it on unresolved symbols, and `virtio_net` was
  never reached. The node came up with no network and discovery reported
  success. 73 MB of `.ko.xz` against 137 MB for the decompressed subset it
  replaces — complete and smaller. kmod is now required, since it is what reads
  a compressed module.
- **fix:** the build fails if any path `modules.dep` names is missing from the
  image, and `/init` greps `dmesg` for "Unknown symbol" after discovery. A
  module the kernel *rejected* is a driver that is present and broken; one that
  matched nothing is hardware this machine does not have. `modprobe` reports
  both identically, so ask the kernel.
- **fix:** DHCP applies the lease it is given. `udhcpc` was run with
  `-s /bin/true`; it configures nothing itself, so every boot took an address
  from the server — which then appeared in the server's lease table, looking
  exactly like success — and put none of it on the interface.
- **fix:** a **recoverable ublk device is released on shutdown, never stopped**.
  Asked to stand down, the incumbent ran `STOP_DEV` on all six devices, which
  tears them down under the filesystems mounted on them (`EXT4-fs: shut down
  requested`, `JBD2: I/O error when updating journal superblock`) — after which
  there is nothing to quiesce and nothing to recover, and the successor cannot
  even be restarted because its own root was among them. `UBLK_F_USER_RECOVERY`
  at creation is the statement that the device outlives its server; releasing
  it is what honours that.
- **fix:** both halves of a handover wait on **device state** rather than
  process state — QUIESCED before `START_USER_RECOVERY`, LIVE after
  `END_USER_RECOVERY`. The old server exiting and the device being ready are
  two events milliseconds apart, and the race was reliably lost; the first
  write after adopting landed in the window and failed with EIO.
- **feat:** `drive::handover` — the incumbent records the slabs it opened and
  which volume is behind each device it created, so `adopt-ublk` needs no
  arguments. The list was previously kept by hand in two places that had to
  agree in order, and standing a server down stops **every** device it serves:
  a list short by one left those devices mounted with no server, returning EIO,
  including the engine's own root.
- **feat:** `adopt-ublk` serves the **management API and `/serve/v1`**. The
  process that holds the slab is the engine — one writer per volume means no
  second process can answer for it — and an engine that serves a node's root
  while answering nothing about it is half there.
- **feat:** `stormblock attach` — open any slab and list, export or mount any
  volume in it, on a disk, a partition or an image file, finding the slab
  inside a partition table if that is what it was handed. Listing and attaching
  are one command. Refuses to attach writable while another server is serving,
  because two writers on one volume corrupt it silently.
- **feat:** `stormblock must-gather` — one directory holding what the kernel
  saw, what the storage layer has, which devices exist and who serves them, the
  handover record, the supervisor's per-workload logs, and the contents of the
  log and data volumes. Read-only throughout.

### 2026-08-24
- **feat:** a volume records whether it is meant to be **kept or thrown away**
  (`Retention`, metadata V3). Nothing recorded that before, so a container
  root and a customer's database looked identical to the engine, and anything
  acting on one had to be told which it was by whoever happened to mount it —
  context the mounter often does not have. It belongs to the volume because
  the same volume may be mounted by different things over its life and the
  answer must not change when it is.
- **note:** the default is **keep**, deliberately. Too much kept is a cleanup;
  something thrown away that should not have been is unrecoverable. A record
  written before the question existed loads as kept.
- **note:** `Ephemeral` is a tmpfs in intent and a CoW clone in mechanism — it
  costs nothing until written, resets to its golden rather than being
  recreated, and the golden is still there as the fallback. `reset_volume`
  already does the reset; what was missing was the marking.
- **fix:** metadata V2 records decode through their own shape rather than
  being read as V3 with a defaulted field. **bincode is not self-describing**,
  so `#[serde(default)]` does nothing for it: a V3 decoder reading a V2
  payload runs off the end of the record, or reads the next record's bytes as
  this one's. Every version that ever existed keeps its shape and converts on
  load, as V1 already did.
- **feat:** ublk devices can be handed from one server to another, so the
  engine serving root is no longer unrepeatable. `boot-local` creates its
  devices with `UBLK_F_USER_RECOVERY` (and `_REISSUE`, so I/O in flight when
  the old server goes is handed to the new one rather than failed), and
  `stormblock adopt-ublk` takes them over: `GET_DEV_INFO` for the geometry the
  device was created with, `START_USER_RECOVERY`, fresh `FETCH_REQ` on every
  queue, `END_USER_RECOVERY`. The block device never disappears, so a
  filesystem mounted on it stays mounted across the swap.
- **note:** why this matters more than it sounds. `switch_root` **deletes the
  initramfs**, so the engine it started runs from an unlinked binary —
  `/proc/<pid>/exe` reads `(deleted)` and nothing on the node could exec it
  again. The one process the root filesystem depends on could not be
  restarted, by anything, for the life of the boot. Now it can be handed to a
  process that lives in a golden, which PID 1 can supervise, restart and
  upgrade.
- **note:** the flag is fixed at `ADD_DEV`, so it has to be asked for by
  whoever *creates* the device — minutes before the process that will want to
  adopt it exists. A device made without it can never be handed over, which is
  why `boot-local` now always asks.
- **refactor:** `open_slabs_and_restore` — `boot-local` and `adopt-ublk` need
  the same three things (open the slabs, find the metadata, restore what it
  describes), and the two halves of a handover disagreeing about any of them
  would be the worst kind of bug to have.
- **feat:** a local boot brings the network up when the command line asks for
  it (`ip=dhcp`, or a static `ip=addr::gw:mask::iface:none`). It used to be
  skipped entirely on the local path, on the reasoning that a local root needs
  no network — true of the root, false of the node. Nothing after
  `switch_root` configures an interface: a stormpump node's PID 1 starts
  containers, and a container on host networking inherits whatever the host
  has. The symptom was every service on the node coming up healthy and
  unreachable. Without `ip=` nothing changes, so no boot waits on a DHCP
  server it never asked for.

### 2026-08-24
- **feat:** `rd.stormblock.mount=<vol>:<path>,...` — the initramfs exports
  these volumes *and mounts them* into the real root before `switch_root`.
  `rd.stormblock.writable=` writes fstab entries, which only a systemd node
  ever reads; a stormpump node's PID 1 reads a boot manifest that registers
  **directories**, so a container's volume has to be a mounted directory by the
  time PID 1 starts. There is no later moment and nothing else on the node
  would do it. A volume that will not mount is reported and skipped rather than
  fatal — one container that cannot start beats a node that does not boot.

## [v9.13.0] — 2026-08-24

### 2026-08-24
- **fix:** `image build` refuses a golden whose ext4 blocks are smaller than
  the volume's logical sector, naming the fix (`mkfs.ext4 -b 4096`). Found by
  booting a real image: the host's `mkfs.ext4` picks 1024-byte blocks for a
  64 MB *file* — its size class, and a file reads as 512-byte sectors — and
  the volume it lands in has 4096-byte sectors. Everything downstream
  succeeded (image built, pallets verified, `boot-local` resolved the clone,
  ublk exported it) and the kernel then said `EXT4-fs (ublkb0): bad block size
  1024` and the node dropped to a shell. The engine knows both numbers at
  build time, so it fails there instead (#40).
- **fix:** `GoldenSource::read_at` seeks instead of assuming the caller reads
  in order — a source that is only correct when read sequentially is a trap
  for the next caller, and the block-size probe reads the front before the
  copy walks the whole thing.
- **verified on hardware:** #62's fix boots. Proxmox VM under OVMF, serial
  console: stormuefi 0.5.0 reads both pallets off raw disk, verifies the
  manifest and every member, selects `kernel1` and hands off; the initramfs
  runs `boot-local --slab /dev/sda4 --volume stormpump`, which reports
  **"Volume metadata from slab /dev/sda4"** — no metadata directory anywhere —
  restores both volumes, exports the CoW clone as `/dev/ublkb0`, and the
  kernel mounts it r/w. The remaining stop is stormpump exiting as PID 1
  (stormpump#1), which is outside this engine.

### 2026-08-23
- **fix:** volumes at or below 8 MiB get no journal unless one is asked for
  (`ext4::JOURNAL_FLOOR_BYTES`). 8 MiB is exactly where `mkfs-ext4`'s size
  class starts adding its 4 MB journal, and it is the one size where doing so
  makes a volume hold *less* than a smaller one: 3.3 MB usable against 6.4 MB
  at 7 MB. Above the floor the journal amortises — 32% at 16 MB, 13% at 64 MB
  — and is kept, because a consumer with no clean unmount needs it.
- **test:** `tests/small_volumes.rs` measures the whole range rather than
  reasoning about it: every megabyte to 8, then 16/32/64 MB, then 128 MB to
  2 TB, each formatted and `fsck`-checked on a 4 KiB-sector volume. A 1 MB
  golden works — 956 KB usable, 128 inodes. A 2 TB filesystem costs 42.6 MB on
  the backing store, so a large thin container is cheap to create.
- **fix:** found and fixed upstream while measuring the above
  (mkfs.ext4.rs#3): every ext4 filesystem below the journal size class's floor
  advertised a journal it did not have — `has_journal` set, zero journal
  blocks, no journal inode. `mke2fs` never emits that shape and a kernel
  refuses to mount it, and our own `fsck` passed it, which is why it survived
  a release. Fixed in mkfs-ext4 v2.0.4, which also reports the shape as
  `journal-advertised-but-absent`.
- **chore(deps):** mkfs-ext4 v2.0.0 → v2.0.4, fio-ext4 v1.4.0 → v1.4.1. Both
  pins move together, as always: fio-ext4 pins mkfs-ext4 by tag, and two tags
  are two cargo source ids, so a mismatched pair resolves two copies and the
  `BlockDevice` trait from one does not satisfy the other.
- **feat:** `image build` prints the GPT LBA size, and says so when a bootable
  image is written at 512 bytes. Firmware parses the GPT with the media's own
  block size and does not probe for it the way `Gpt::read` does, so a 512-LBA
  image on a 4Kn drive puts the header where firmware will not look. Set
  `block_size = 4096` in the spec for a 4Kn target.
- **fix:** `raid::journal::persist_and_reload` used a fixed temp filename, and
  `cargo test` runs the lib and every integration binary concurrently.

## [v9.12.0] — 2026-08-20

### 2026-08-20
- **feat:** the stock engine mounts `/serve/v1` (#60). `stormblock serve`
  mounted the management and metrics routers but not `serve::api`, so the
  serving surface existed only where a profile mounted it — stormblockmk, and
  nothing else. A consumer running against a RouterOS node and an x86 one
  could list drives on both and create a volume on only one.
  `docs/layering.md` puts this in layer 2: *"what it takes to serve volumes to
  something. None of this is a choice a deployment makes differently; it is
  the job."* A layer-2 surface only some profiles serve is a convention rather
  than a guarantee, which is what that document exists to end.
- **feat:** a `[serve]` config section — the *stock profile*. Every field is
  an override; leaving one unset takes the serving default or derives it.
  `advertise_addr` derives through the ladder the NVMe-oF discovery log
  already uses (explicit, then `management.advertised_addr`, then the
  NVMe-oF listen host, then the management listen host, then loopback),
  because a consumer told to attach to `0.0.0.0` cannot. `data_dir` defaults
  to `<management.data_dir>/serve`.
- **note:** one case refuses to serve rather than guessing — **no data
  directory anywhere**. The wiring table pins which LUN and which port each
  volume was given, and without somewhere durable to keep it a restart can
  hand a LUN a consumer is already attached to over to a different volume,
  which is the bug that table exists to prevent. Refused with a reason and
  never silently, so a consumer getting 404s can find out why from the log.
- **feat:** the StormFS data path (#49, #50) is behind a `stormfs-data`
  feature, on by default and **out of the `mikrotik` profile**, which builds
  `--no-default-features` and so gets the exclusion for free. A RouterOS node
  with 256 MB serves container volumes over NVMe-TCP and is not a StormFS
  data node: the surface
  would be weight in a binary meant to be small, and one that is mounted
  invites being called. The registration client is not gated — announcing
  volumes to a metadata server is the opposite direction and costs a periodic
  POST.
- **fix:** the `mikrotik` profile compiles again. It had been broken on four
  errors: `serve/ctx.rs` and `serve/reconcile.rs` imported
  `crate::target::nvmeof` unconditionally, and `mgmt/api/v1.rs` had a
  `let _ = volume_id;` in a `not(nvmeof)` branch of a function with no such
  parameter — a refactor leftover that nothing without the feature could
  compile past. Verified against every profile `CLAUDE.md` documents:
  `mikrotik,iscsi`, `arm64,iscsi,nvmeof`, default and all-features.
- **fix:** a **pure-NVMe build compiles**. `--no-default-features --features
  nvmeof` did not build while `iscsi` alone did, and the asymmetry was an
  assumption nobody had exercised rather than a decision: every profile
  shipped so far has iSCSI in it, so nothing ever compiled the crate without
  it. Three structural dependencies — `drive/iscsi_dev.rs` (the iSCSI
  *initiator* reuses the *target's* PDU parser rather than carrying a second
  copy of RFC 7143, and takes `boot_iscsi` and the `boot-iscsi` /
  `migrate-boot` subcommands with it), `serve/ctx.rs` (`Portal` and
  `shared_iscsi` name `IscsiTarget` directly), and `serve/reconcile.rs`
  (`mgmt::api::luns` plus the ensure_lun / start_portal / stop_portal path).
  The two transports are gated symmetrically now, so the matrix holds in both
  directions rather than only the one anyone happened to build.
- **fix:** `ServeContext::is_blocked` covers an NVMe row in a build with no
  NVMe-oF, not only an iSCSI row while iSCSI is off. Both are rows nothing
  will ever pick up, and a row left at `pending` holds readiness down for
  good. Which transport is missing decides what the operator can do about it,
  so the two warnings do not share a sentence.

### 2026-08-21
- **feat:** `priority` on `POST /api/v1/pallets`. The library has had
  `PublishSpec.priority` all along and REST could not reach it, so every
  published pallet was a **candidate** (the default is 1) — which is right for a
  pallet published to be used and wrong for one published to be examined. A lab
  boot pallet on a working node joins that node's ladder and can win it.
  Publishing at **0 never boots**, which is what makes publishing beside a live
  system safe rather than careful.

## [v9.11.0] — 2026-08-20

### 2026-08-20
- **feat:** `/api/v1/stormfs` — the data path StormFS consumes (#49). Four
  routes `docs/stormblock-spec.md` §9.1 has listed since v0.1 and nothing
  implemented; what `src/stormfs.rs` does is the opposite direction,
  announcing this node's volumes to a metadata server. A chunk is a run of
  whole slab slots inside one volume, addressed as the same
  `(volume, offset, len)` the client then reads and writes over iSCSI or
  NVMe-oF — whole slots because a slot is the unit the volume can reclaim,
  which is already what `discard_granularity` reports.
- **feat:** allocation is **eager and tier-scoped**. StormFS owns policy —
  which tier a file belongs on — while StormBlock owns placement, so the
  slots come from that tier and are mapped now rather than left to
  allocate-on-write, which would place them by the *volume's* policy instead
  and could fail later on space the call reported as available. A tier with
  no room is `507`, never a quiet substitution of a slower one. Batched,
  because a 1 GiB write at a 4 MiB chunk size is 256 allocations and one
  round trip each would put the round-trip count back in the data path the
  design exists to keep it out of.
- **feat:** deallocate is **idempotent by construction** — the sweeper can
  crash between freeing an extent and dropping its queue entry, so it will
  re-free, and making that an error wedges the queue permanently on one
  crash. Trim is the same call with one bit changed: both free the slots,
  only deallocate returns the address range, so a chunk StormFS has punched a
  hole in cannot be handed to a second caller.
- **feat:** `POST /api/v1/stormfs/commit` — versioned-map CAS and atomic
  multi-block write, which turn out to be one mechanism (#50). A writer fills
  scratch extents wherever it likes and the commit re-points the target range
  at them; because the swap moves extent *identity* rather than bytes it is
  cheap enough to run under one lock, validated in full before anything is
  applied. That is the atomic multi-block write, and it is why StormFS needs
  no journal of its own. Gating the same swap on a version is the CAS: a
  writer that stalled long enough to lose its lease is harmless rather than
  dangerous, since its data went into extents nobody points at and its swap
  fails the check — no fencing round trip, and no correctness argument that
  depends on clocks. A stale commit is `409` carrying `current_version`.
- **feat:** `/api/v1/stormfs/pins` — pinned-version reads, which are the
  engine's copy-on-write retention exposed rather than new machinery. A pin
  is a snapshot, so a commit that supersedes an extent decrements its
  reference instead of freeing it; the reader reads the snapshot volume
  through the ordinary export path, which keeps StormFS's rule that no
  process sits in the data path. It also makes tier migration invisible — a
  reader pinned to an older version keeps reading the source chunks until it
  releases.
- **note:** what makes a commit untearable is **not a journal**. The durable
  record of an extent map is the volume metadata file, written whole and
  atomically with a checksum, so a crash finds the whole swap or none of it.
  Versions live in `<data_dir>/stormfs.json` and **the write order is
  load-bearing**: versions first, then the map. A crash between them leaves
  the version ahead of the map, so a stale writer is told to re-read and
  finds the old data — it retries. The other order leaves the version behind
  a map that has already moved, and that writer would commit over committed
  data. Versions must be monotonic, not gapless, so burning one is free.
- **fix:** `DELETE /api/v1/volumes/{id}` asks the shared `what_is_serving`
  question instead of only checking the export table, so a volume backing a
  live iSCSI LUN or a ublk device is no longer deletable out from under it —
  the guard the move path and the template sweep have always used. A StormFS
  pin now counts as serving, since deleting a pin's snapshot behind its
  reader's back is exactly what the pin exists to prevent.
- **feat:** `Slab::reassign_slot` — re-point a slot at a different volume and
  virtual extent without touching the bytes. The slot table is what
  `rebuild_from_slabs` reads when there is nothing better, so leaving it
  naming the scratch address would make the recovery path disagree with the
  map.

## [v9.10.0] — 2026-08-20

### 2026-08-20
- **feat:** `POST /api/v1/drives` and `DELETE /api/v1/drives/{id}` — open and
  close a drive without restarting the node. The pallet store is rebuilt from
  `state.drives` on every request, so a drive that is not open does not exist to
  publish onto; until now adding one meant a restart, which takes down every
  volume the node is serving in order to add a disk that has nothing to do with
  them. `path` may be a device or a file; `size_bytes` creates or extends a
  sparse file first.
- **feat:** Closing refuses to strand data, **by identity rather than by
  convention**: the slab registry is asked whether any slab's device *is* this
  device (`Arc::ptr_eq`), so "this drive carries slab X" is a fact rather than a
  profile's belief about which index the slab took. `?force=true` exists for a
  disk that is already gone, and says so in the log.
- **layering:** This is engine-level for the reason `docs/layering.md` gives, and
  because of a concrete consumer: one registry serves a RouterOS node and an x86
  one, and must not learn two APIs to give a pallet a home. stormblockmk had this
  as `/mk/v1/drives` for a single release — wrong side of the line — and now keeps
  only *which* drives it carries, plus its own file-backed-only rule.

## [v9.9.0] — 2026-08-20

### 2026-08-20
- **feat:** `/api/v1/images` — build, convert and inspect over REST. The builder
  was already a library; the CLI is glue over it, and so is this: same
  `ImageBuilder`, same `BuildReport`, same verification inside the image. It is
  engine-level for the reason `docs/layering.md` gives — building an image is
  mechanism, not a deployment choice, so every profile that merges
  `mgmt::api::router` gets it and none of them forks it.
  - `POST /build` takes the spec as JSON (`ImageSpec` is already `Deserialize`),
    as TOML, or as a path to one, plus `out`, `format`, `keep_raw` and
    `include_slab`.
  - `POST /convert`, `POST /inspect` (GPT and the pallets in it, read through the
    ordinary pallet tooling), `GET /formats`.
- **fix (by construction):** the REST path **resolves** relative paths instead of
  `chdir`-ing. The CLI changes directory so a spec's paths resolve against the
  spec file, which is right for a process that then exits; a daemon cannot,
  because the working directory is process-global and a build would move the
  ground under every request in flight. Paths resolve against `base_dir` (or the
  spec file's own directory) and are refused, by name, when there is nothing to
  resolve them against.

## [v9.8.0] — 2026-08-19

### 2026-08-19
- **feat:** `standing_report` / `standing_needed`, and
  `GET /api/v1/fstemplates/standby` — *which templates would make a start
  wait*, answered without minting as a side effect. A supervisor has to be able
  to ask whether a node is warm without the asking making it true, so the check
  and the fix are separate verbs on one path: `POST` enforces, and both are
  idempotent and safe on every supervisor start.
- **feat:** A take is a take. An ordinary clone now tops the template back up
  as well, not only a claim, so the next start is fast whichever door the last
  one came through. A standby mint is flagged as such, so it neither counts
  against the template's `clones` — that number answers "how many went
  somewhere" — nor triggers a top-up of itself.
- **feat:** `stormblock pallet add-member`, `remove-member` and `copy-member`
  expose recompose at the CLI, which until now only the library and REST could
  reach. A sealed pallet is never edited in place, so each publishes a new
  version and says so; the previous one stays until pruned.
- **fix:** `clones` counts clones that were handed out, not clones that were
  minted. **v9.7.0 shipped with one failing test** because of this: the standing
  clone incremented the counter the moment it was minted, and
  `every_clone_gets_its_own_filesystem_uuid` correctly disagreed. Fixed here.

## [v9.7.0] — 2026-08-19

### 2026-08-19
- **feat:** A sealed `fstemplate` keeps **one clone standing by**, and
  `POST /api/v1/fstemplates/{id}/claim` takes it and mints the replacement
  behind the caller (#55). Minting is a snapshot, a fresh filesystem identity
  and a check — seconds, none of which depends on *when* the start happens, so
  all of it now happens before the start. What a claim pays is a lookup. This is
  what makes stormboot's fast path actually fast, and it works before the
  registry is up: the engine holds the invariant itself.
- **feat:** `POST /api/v1/fstemplates/{id}/standby` pre-warms a template
  explicitly, and a template reports its `standing` clone, so whether a start
  will be a lookup or a mint is visible rather than guessed.
- **feat:** A claim with nothing standing mints inline rather than refusing — a
  start that waits beats a start that does not happen — and reports
  `from_standby: false`, so a slow start is explainable instead of mysterious.
- **feat:** Sealing mints the first standing clone, and a startup pass gives
  every `Ready` template one. Both are spawned: a node should serve requests now
  and be fast shortly, not the other way round.
- **fix:** The standing clone is one of a template's volumes, so deleting the
  template takes it along (#47), and the serve-layer reaper's referenced set
  includes it — a clone minted before anyone asks for it is, by definition,
  referenced by nothing, which is exactly the shape that sweep collects.

## [v9.6.0] — 2026-08-19

### 2026-08-19
- **refactor:** The pallet format's **read side is a crate of its own** —
  `crates/pallet-format`, `no_std`, no allocation on the read path, no async, no
  I/O, and no write path at all (#53). `format.rs` claimed byte-compatibility
  with a reader that did not implement this format yet, and the way that claim
  becomes true matters: transcribing v1 into a second reader would reproduce the
  failure mode already argued against — two hand-maintained readers of one
  on-disk layout, in two repos, whose drift fails as *the node does not boot*.
  Now firmware and the engine link the same reader. Verified against
  `x86_64-unknown-uefi`, not asserted.
- **refactor:** stormblock keeps emission — there is no second writer, so there
  is nothing to keep in sync on that side — and lays bytes down at the offsets
  `pallet_format::layout` defines, so every field has exactly one definition.
  `pallet::Pallet` is now a thin async layer whose every decode is the shared
  reader; `crc32`, `crc32_continue` and `superblock_crc` come from it too.
- **test:** 12 crate tests work from **hand-built bytes** rather than from our
  own writer, because a decoder tested only against its own encoder proves
  nothing about either. The CRC is checked against the value every other
  implementation produces, and `firmware_reads_what_the_engine_wrote` reads an
  engine-written pallet back through the firmware path itself — synchronous,
  scratch buffer, `BlockReader` — including refusing a tampered one.
- **docs:** `docs/pallets.md` is the living specification; the links that
  pointed at `stormuefi/docs/PALLET-SPEC.md` now point here, since the format
  moved with the producer.

## [v9.5.0] — 2026-08-19

### 2026-08-19
- **feat:** **Image building** — `stormblock image build --spec image.toml --out
  disk.qcow2`, plus `inspect`, `convert` and `formats`. A disk image is a GPT
  plus a concatenation of pallets, so the builder reimplements none of it: an
  image file is a drive to this engine, so assembly opens the file and drives
  the ordinary `PalletManager`. Every pallet is verified *inside the image*
  after it lands, and a build whose pallet does not verify fails rather than
  warns. See [docs/images.md](docs/images.md).
- **feat:** One TOML spec describes an ESP, pallets (composed from files or
  imported byte-for-byte from another image), arbitrary raw partitions and a
  formatted slab. Order on disk is the order of the sections; sizes may be
  omitted and computed, and a declared size that does not fit is refused rather
  than truncated.
- **feat:** Output formats: raw, qcow2 (v3, sparse), fixed VHD, monolithic
  sparse VMDK, and a hybrid ISO. A 320 MB image with an empty slab converts to
  about 10 MB of qcow2 or VMDK.
- **feat:** The ISO is the same image seen twice — an ISO9660 filesystem with an
  EFI El Torito entry at the front, and a GPT in the 32 KiB system area
  describing the same bytes, so the file boots from optical media and from a
  USB stick. The pallets inside verify through the ordinary pallet tooling. The
  slab is left out by default: it is empty, and carrying it turned a 35 MB image
  into a 320 MB one.
- **feat:** `src/image/fat.rs` writes the ESP itself — FAT16 or FAT32 by size,
  with real VFAT long names. Both widths exist because FAT32's 65,525-cluster
  floor (~33 MiB) sits just above El Torito's 16-bit sector-count ceiling
  (32 MiB): without FAT16 there is no ESP size that satisfies both. Fixed
  timestamps and sorted entries, so the same tree builds the same bytes.
- **feat:** `PartitionDevice` — a partition as a `BlockDevice`, so anything that
  formats a device can be pointed at one inside an image without knowing it is
  inside one.
- **fix:** A name that is already 8.3 is stored plainly. The sanitiser replaced
  the dot before splitting the extension, so `BOOTX64.EFI` became `BOOTX6~1`
  with a long-name entry it never needed. Found by `mtools`, not by us.
- **fix:** The El Torito catalog is EFI-only, and an oversized ESP warns at
  build time. Found by `xorriso`: the boot image's 16-bit sector count had
  silently saturated at 65535, and the unbootable BIOS placeholder entry was
  being reported as a hidden image.
- **test:** `ci-image-verify.sh` — mtools reads the ESP and compares extracted
  files against their sources, an independent Python parser rebuilds the raw
  image from each container format's own metadata, `xorriso` reads the ISO, and
  `stormblock pallet verify` runs against the ISO itself.

## [v9.4.0] — 2026-08-19

### 2026-08-19
- **feat:** `pallet convert --from <drive> --to <drive>` — one call for what a
  drive replacement actually is: everything on the source becomes partitioned
  pallets on the destination. It covers both shapes a source can be in without
  the caller having to know which — a whole-drive pallet that cannot be
  partitioned in place, and an already-partitioned drive being evacuated.
  Copy, verify, then remove: nothing leaves the source until its copy has been
  read back at the destination and checked against the manifest's digests, and
  identities survive so every reference still resolves. A pallet that will not
  parse is skipped and reported rather than copied. `--reinit-source` gives the
  source a fresh table so it can carry pallets, and is **refused while anything
  failed to convert** — exactly the case where the source is still the only
  copy. `POST /api/v1/pallets/convert` and `PalletManager::convert_drive`.

## [v9.3.0] — 2026-08-19

### 2026-08-19
- **feat:** **Pallets** (#51, #52) — a pallet is a GPT partition holding a
  named, versioned, self-contained set of sealed member images plus the
  manifest describing them, and stormblock is now the producer for the format
  `stormuefi` already reads. `src/pallet/`: the v1 writer and reader
  (byte-compatible with `stormuefi-map`), a GPT reader/writer, discovery across
  drives, the selection policy, and the lifecycle manager. See
  [docs/pallets.md](docs/pallets.md).
- **feat:** A drive is **subdivided into many pallets** instead of being one.
  Several pallets per drive and several drives per node are the normal case,
  found by scanning each GPT rather than by configuration — the only
  arrangement that survives a disk moved between nodes or an image assembled
  elsewhere. A file-backed device is a drive like any other here.
- **feat:** A pallet carries a **kind** — `boot`, `system`, `kernel`, `kube`,
  `app`, `runtime`, `data` — and a human-readable **version label** beside the
  monotonic `pallet_version`. Both live in the superblock's reserved area and
  are defined so zero means "unspecified", so a pallet written before they
  existed still reads correctly. Priority orders only pallets of the same kind:
  a kube pallet does not outrank a boot pallet by carrying a bigger number.
- **feat:** A **read-only** selection surface for boot-time consumers —
  `pallet::select` is pure functions over plain data (no I/O, no device, no
  async) and `PalletBrowser` has no method that writes. This is what stormuefi
  mirrors; `select_verified` walks the fallback chain and returns the first
  pallet that passes with the reason each earlier one was rejected.
- **feat:** Lifecycle (#52): publish, verify, activate, mark successful, roll
  back, set read-only/sealed, prune keeping N-1. Nothing in use is ever
  rewritten — an upgrade is a new partition and a recompose is a new version —
  and activation is an attribute write, so there is no window with nothing to
  boot and rollback restores nothing.
- **feat:** **Moves.** A whole pallet moves between drives keeping its identity
  (copy, verify at the destination, adopt the GUID, drop the source — in that
  order, so no interruption leaves two disks claiming to be the same pallet).
  One member — a container, a kernel — moves between pallets as a new version
  of each, read through the source's extent map with nothing staged in between.
- **feat:** `/api/v1/pallets` and a `stormblock pallet` CLI over the same
  library. A member can be sourced from a volume, so the golden a pallet ships
  is published by being read out of the GEM.
- **feat:** Whole-drive pallets from before drives were subdivided are still
  discovered, verified and readable, and `adopt` migrates one onto a
  partitioned drive. Subdividing such a drive in place is refused: the table
  wants the bytes the superblock is in.
- **fix:** The GPT is written in 512-byte LBAs on a file-backed device rather
  than in the 4096-byte size it *prefers* for I/O. An image is how disks and
  ISOs are assembled, and a 4Kn table there is one this code reads back happily
  and `fdisk` cannot find at all. On read the LBA size is discovered rather
  than assumed. Validated against `fdisk` and byte-by-byte, not only against
  our own reader.
- **chore:** Logs go to stderr, so a subcommand's stdout is only its answer.

### 2026-08-19
- **test:** Layered goldens: `layers_stack_to_any_depth` (four levels deep) and
  `a_child_survives_its_parent_being_deleted` prove these are complete
  filesystems sharing refcounted blocks, not overlay layers borrowing them.
- **test:** `a_clone_flattens_the_stack_and_writes_only_to_itself` measures the
  runtime model: a clone of a 9 MiB two-level stack costs one slot, reads every
  level through one flat map, and its writes never reach the goldens beneath.
- **docs:** `docs/layering.md` — layer references are `(slab UUID, slot)`, so
  depth is free to read and squashing is a space decision, never a latency one;
  moving a pallet preserves every reference verbatim, while moving a volume away
  from its slabs is a rebuild.
- **chore:** Drop the dead `Slab::persist_header` (free_slots is derived and
  recounted by `open`; its call site was removed deliberately) and two redundant
  imports in `serve/api.rs`.

### 2026-08-19
- **feat:** a template's `parent_id` is exposed in the API, not only persisted.
  Lineage the engine keeps to itself makes "rebuild everything built on this
  base" a question nothing outside can answer — and the thing that wants to
  ask is not the engine. A template with no parent reports `null` rather than
  omitting the field, so a consumer can tell "no parent" from "not asked".
- **feat:** a template can be built **from** another — `FROM`, in the sense a
  container build means it. `TemplateSpec.parent` (and `parent` on
  `POST /api/v1/fstemplates`) makes the new template's raw volume a
  copy-on-write clone of the parent's sealed snapshot instead of a blank one,
  so it arrives already formatted and already carrying the parent's contents:
  write only what is new, then seal.
  The point is what it costs. Snapshots clone an extent map and raise a
  refcount on shared slab slots, so a runtime that several images have in
  common is **stored once rather than once per image** — measured across this
  fleet's 14 images, `stormd` is currently stored 5 times (46.4 MB) and
  `stormsh` 4 times (11.8 MB), about 46 MB of pure duplication. And because a
  snapshot owns a complete extent map of its own, nothing reads *through* a
  parent: there is no chain to walk however deep the layering goes, and
  deleting a parent stays safe because the blocks are refcounted, not borrowed.
  A child inherits the parent's filesystem shape — kind, journal, features,
  block layout — because it *is* that filesystem; only `size_bytes` may differ,
  and only upwards. It gets a **fresh filesystem UUID stamped at creation**,
  before anything is written: two children of one parent must not both claim
  the parent's identity, and under `metadata_csum` that UUID seeds every
  checksum in the filesystem, so stamping late would mean rewriting all of
  them (`metadata_csum_seed` is what makes doing it here one superblock write).
  New state `awaiting_seed` distinguishes "already a filesystem, waiting for
  content" from `awaiting_format`. Naming a parent implies `format: false`,
  and asking for both is refused rather than silently resolved — formatting
  over a parent would erase the thing the parent is for.

### 2026-08-18
- **chore(deps):** `mkfs-ext4` to v1.4.0 and `fio-ext4` to v1.3.2, moving both
  pins together as they have to be — two tags are two source ids, and the
  `BlockDevice` trait from one copy does not satisfy the other. `mkfs-ext4`
  v1.4.0 makes `fsck` verify the checksum on every extent-tree node that lives
  in a block of its own, which is the check whose absence let v1.3.1's bug
  through: walking a tree reads the entries and never looks at the four bytes
  after them, so a template could check clean here and still be refused by the
  kernel that mounts it. Every check the engine runs over a formatted volume
  now covers that. The `mkfs-ext4` pin was still on v1.3.0 and so did not carry
  the journal extent-leaf fix at all.

## [v9.2.1] — 2026-08-18

### 2026-08-18
- **fix(deps):** `fio-ext4` to v1.3.1, which writes an extent leaf's checksum at `EXT4_EXTENT_TAIL_OFFSET` — after the space `eh_max` entries occupy — rather than at the end of the block. The two coincide at 1 KiB and 4 KiB blocks and differ by four bytes at 2 KiB, 8 KiB and 32 KiB, so on those block sizes every file large enough to need an extent block was unreadable to a real ext4 reader: `e2fsck` 1.47.3 reports "extent block passes checks, but checksum does not match extent", and Linux 6.17 refuses the file with `EXT4-fs error … extent tree corrupted` and EIO. A template written here on 2 KiB blocks would clone, check clean under our own `fsck` and verify its contents, and still be rejected by the kernel that eventually mounts it — which is the shape of failure worth a release on its own. `mkfs-ext4` stays at v1.3.0, the tag `fio-ext4` v1.3.1 depends on.
- **chore(deps):** `mkfs-ext4` and `fio-ext4` both to v1.3.0. Two changes reach the engine. Formatting a template writes less: the reserved GDT blocks of a backup group and the bitmaps of a group flagged `BLOCK_UNINIT` or `INODE_UNINIT` are no longer written, because nothing reads them and `mke2fs` does not write them either — measured on a 1 TiB ext4 as 17,563.7 MiB in 35,463 writes becoming 17,429.8 MiB in 19,547. And `read_block_bitmap`/`read_inode_bitmap` now compute an uninitialised group's bitmap from the geometry, as its flag says to, rather than returning a block that was never written — which matters here precisely because a thin volume is *not* guaranteed to read back as zeros, so the old behaviour could see a bitmap full of whatever the slab held before. Both pins move together: two different tags are two source ids, cargo would resolve two copies of `mkfs-ext4`, and the `BlockDevice` trait from one does not satisfy the other.

## [v9.2.0] — 2026-08-18

### 2026-08-18
- **feat:** iSCSI MC/S — multiple connections per session (#31). Login negotiates `MaxConnections` up to `iscsi.max_connections` (default 4) instead of clamping to 1, and a login carrying a non-zero TSIH **joins** that session rather than starting a new one (RFC 7143 §6.3.1). The ISID must match too: the two identify the session together and a TSIH alone is guessable. New login statuses for the two ways joining can fail — `SessionNotFound` (0x0203) and `TooManyConnections` (0x0206) — rather than silently making a second session.
- **BREAKING (internal):** **CmdSN is now session-wide, StatSN stays per-connection** (RFC 7143 §4.2.2.1). This is the part that had to be right before MC/S could work at all: the command window lived on `ConnectionState`, which single-connection code could get away with, but two connections would then each advertise their own window and an initiator would be told two different things about one session's flow control. `Session` owns one `CmdSnWindow` shared by every connection on it, advanced with `compare_exchange` since two connections can acknowledge concurrently. `ConnectionState::exp_cmd_sn`/`max_cmd_sn` are methods now, not fields.
- **fix:** a closing connection removes **itself** from its session, not the whole session. Previously any connection closing tore the session down, which with MC/S would take its siblings' paths with it; the session now ends when its last connection does.
- **note:** negotiation takes the lower of the target's cap and the initiator's request, so an initiator asking for one connection still gets exactly one — raising the cap cannot change what an existing consumer sees. There is a test asserting precisely that.

## [v9.1.1] — 2026-08-18

### 2026-08-18
- **fix:** a volume that could not be thrown away is no longer reported as thrown away (#48). Three places in the template lifecycle discarded a volume with `let _ = delete_volume(…)` and then told the caller, in as many words, that it had been discarded — so when the delete failed the volume survived and nothing said so. That failure is not hypothetical: `delete_volume` releases slots best-effort and still returns `Err` for the ones it could not release (#37). The discard now retries once and, if the volume is still there, returns `TemplateError::Leaked` **carrying the volume id** — which is the only thing that makes it reclaimable, since a clone is created under the *caller's* name (`pvc-web-1`, not `fstemplate-…`) and is therefore indistinguishable by name from a live consumer volume. `POST /api/v1/fstemplates/{id}/clone` puts `leaked_volume_id` in the error body.
- **fix:** the orphan sweep will not reclaim a volume this node is serving (#48). `orphans` and `reclaim_orphans` now take the in-use set as a **required argument** rather than an optional guard, because the consequence of forgetting it is deleting something that is attached. The management layer computes it from the export table, the iSCSI LUN table and the ublk export map — one shared helper with the volume-move guard (#20), since they are the same question and two implementations of it means one is eventually wrong. A volume that will not delete is logged at `error` and reported as *not* reclaimed.

## [v9.1.0] — 2026-08-18

### 2026-08-18
- **feat:** first-class volume move — re-home or shrink a volume without losing it (#20). `POST /api/v1/moves` snapshots the source (copy-on-write, so it costs metadata and doubles as the rollback point), creates the target at the new size, formats it to match the source's profile, streams the *contents* across and fscks the result — then stops, with the source untouched. `POST /api/v1/moves/{id}/commit` deletes the source, and only the caller can say when, because only the caller knows whether its consumer has been repointed; `/abort` deletes the target instead. This is the operation `resize` cannot be: shrinking frees the extents past the new end and xfs cannot shrink into that, so the only safe form is a new smaller filesystem with the contents copied in.
- **feat:** the copy is streamed from one filesystem straight into the other — no scratch file, no whole-archive buffer — so a 64 GiB volume holding 2 GiB moves 2 GiB and the memory cost is fixed. It goes through tar rather than a hand-rolled tree walk, which preserves modes, ownership, timestamps, symlinks, hard links, device nodes and extended attributes (SELinux labels among them, without which a rootfs stops booting). Both ends count every category independently and any mismatch fails the move.
- **feat:** a move is offline by contract — an exported or attached volume is refused, since anything written during the copy would not be in the target, and the guard is re-applied at commit because the caller has been off repointing things in between. The move ledger is persisted, so a move interrupted between copy and commit is still nameable after a restart rather than leaving two volumes and no record of which is which.
- **feat:** the pool grows itself on disk pressure (#18). Thin volumes overcommit, so physical space runs out while every volume still reports free virtual space — nothing noticed until writes failed. `volume::pressure` adds the pool-level accounting that was missing (per-slab numbers existed; nothing summed them, including a per-tier breakdown so hot-tier pressure is visible when the pool as a whole looks comfortable) and a watcher that adds a slab at or above `high_water_pct` (default 80). Grow on pressure, never preallocate: preallocating to the virtual size gives back everything thin provisioning saved.
- **feat:** growth sources are configured and never discovered — formatting the wrong device is unrecoverable, and "it had no filesystem on it" is not consent. A `directory` source only creates new backing files, which is also how to grow into the unused tail of the node's own disk; a `device` source that already carries a readable slab is **adopted with its data** rather than reformatted, so a source claimed before a reboot comes back intact. A failing source is retired after one attempt rather than retried every interval, and `max_slabs` backstops a misconfigured list.
- **feat:** `GET /api/v1/slabs/pool` reports usage, `used_pct`, whether the pool is under pressure, sources left and what the last check decided — available whether or not growth is enabled, since the accounting is the useful half on its own. New gauges `stormblock_pool_used_pct` / `_total_bytes` / `_free_bytes` and counters `stormblock_pool_slabs_added_total` / `_growth_failures_total`. Pressure with every source claimed logs at **error** every interval: it does not resolve itself and is not a state to discover late.
- **note:** an empty pool reads as 100% used rather than 0%. No capacity is a pressure condition, not a comfortable one — reporting it as empty-and-fine is how a node with no slabs looks healthy right up until its first write.

## [v9.0.0] — 2026-08-18

### Breaking

- **`VolumeManager::resize_volume` grows only.** A smaller size returns `VolumeError::ShrinkRefused` (HTTP 409) rather than freeing every extent past the new end. On a mounted xfs — which cannot shrink at all — the old behaviour destroyed live filesystem data with nothing to undo it (#19). `VolumeManager::shrink_volume` performs it for a caller that names it.
- **`DELETE /api/v1/fstemplates/{id}` purges the template's volume** unless told `?purge=false` (#47). Shipped in v8.3.0, which understated it: a caller that relied on delete-the-entry-keep-the-volume must now say so. Purging a template with clones no longer requires `force` either — a clone holds its own refcounted reference to every extent, so it is unaffected.
- **`/v1` volume create rejects an unknown `qos_class`** with `400` instead of storing it (#35). A driver sending a class outside `bronze | silver | gold | platinum` now fails where it previously appeared to succeed.

### 2026-08-18
- **fix:** `UBLK_U_CMD_GET_FEATURES` is declared `_IOR`, not `_IOWR`. Encoded with the wrong direction bits it never reached its handler and came back as an error — which, for a *feature query*, is indistinguishable from a kernel that has no such feature. `UBLK_F_UPDATE_SIZE` was therefore never negotiated on a 6.17 kernel that offers it. Only the on-metal test could have found this, and did.
- **feat:** a ublk-exported volume follows its own resize (#19). `UblkServer` negotiates `UBLK_F_UPDATE_SIZE` at ADD_DEV — after asking the kernel for its feature mask, since a flag an older kernel does not know fails ADD_DEV outright and losing the resize is better than losing the device — and `update_size()` issues `UBLK_U_CMD_UPDATE_SIZE` with the new size in sectors. **No quiesce**: it is an independent control command with no consistency point to capture, so I/O keeps flowing; stalling a live `/var` to make it bigger would turn a day-2 operation into an outage. `/v1` expand pushes the new size down to the device after growing the backing volume, and says so loudly if the device does not follow — otherwise the volume grows and `xfs_growfs` finds nothing to grow into.
- **BREAKING:** `VolumeManager::resize_volume` grows only. A smaller size comes back as `VolumeError::ShrinkRefused` (HTTP 409) instead of freeing every extent past the new end — which, on a mounted xfs, silently destroyed live filesystem data with nothing to undo it (#19). Shrinking is still possible through `VolumeManager::shrink_volume`, so destroying data is something a caller has to name rather than something it can reach by passing a smaller number to the same function. A caller that wants a smaller volume *with its data* wants a move, which is a copy and a different operation (#20).
- **feat:** `/v1` volume create validates `qos_class` against the taxonomy agreed with stormblock-csi — `bronze | silver | gold | platinum` (#35, mirror of stormblock-csi#10). The wire field stays a string; only the accepted set is pinned, and an unknown class comes back as `400 bad_request` naming the set rather than being stored and never acted on. Validated before the name lookup, so a bad class on an existing name is still a bad request rather than an idempotent hit.
- **test:** the CSI wire-contract fixtures are vendored into `contract/` and asserted against the engine's own serializers (#34, mirror of stormblock-csi#8). `tests/contract_v1_wire.rs` round-trips all twelve — `Volume`, `SyncState`, both `AttachInfo` shapes (the nvme_tcp one with its shared subsystem NQN and `nsid`), both `VolumeSource` shapes, `DualAttachWindow`, `CreateVolumeRequest` with every field set, `Snapshot`, `GroupSnapshot`, `NodeCapacity` — plus both error envelopes, which are checked against what an error actually serializes to rather than parsed. One pin, held on two sides: a wire change now has to land in both repos or fail one of their builds.
- **perf:** the cluster heartbeat probes its peers concurrently (#41). A round used to await each peer in turn, so it cost `N × RTT` — and, worse for a failure detector, one hung peer stalled every peer behind it in the list: the condition the detector exists to notice was the one that made it slowest, and a healthy node's detection latency degraded in proportion to how many unhealthy ones happened to sort before it. Probes now go out together under a 64-at-a-time cap, so a round costs about one RTT and a dead peer costs one deadline wherever it sits.
- **fix:** a heartbeat probe carries its own deadline, derived from the heartbeat interval (floor 500 ms), instead of inheriting the cluster HTTP client's 10 s — ten intervals, which is what let a single wedged peer swallow a round whole. A round that overruns its interval now logs and skips to the next tick rather than queueing rounds behind it, and the round applies its results under one membership write lock instead of one per peer. New `stormblock_cluster_heartbeat_round_seconds` histogram.
- **note:** this leaves the heartbeat `O(N²)` fleet-wide per interval, which is a design property of all-to-all probing rather than of this loop; replacing it with a gossip failure detector is #42.

## [v8.3.0] — 2026-08-18

### 2026-08-18
- **fix:** a filesystem template no longer leaks its volumes (#47). Three separate leaks, all in the same lifecycle. (1) The scratch volume outlived a successful create — 94 `-raw` volumes against 17 templates on one node — so `seal` now drops it once the snapshot is taken; the sealed snapshot holds its own refcounted extents and never depended on the volume it came from. (2) A create that failed at seal left both halves standing, and the caller's retry paid for two more: every failure path in `create` — format, seed and now seal — goes through one rollback that forgets the template and deletes every volume it made. (3) `DELETE /api/v1/fstemplates/{id}` defaulted to keeping the volumes, which is what left 75 sealed volumes no template claimed; purging is the default now, and `?purge=false` is the way to keep them. Purging no longer needs `force` when clones descend from the template — a snapshot keeps its own reference to every extent, so the clone is untouched either way.
- **feat:** `GET /api/v1/fstemplates/orphans` lists volumes named like a template's that no template in the store claims, with what each has allocated, and `DELETE` on the same path reclaims them — the reconciliation a node already in this state needs, since nothing else could tell that debris apart from live volumes by name. Clones are named by their consumer and are never in the set.
- **BEHAVIOUR:** `DELETE /api/v1/fstemplates/{id}` deletes the template's volume now, where before it kept it unless asked to purge. A caller that relied on the old default — deleting the store entry and keeping the volume — must pass `?purge=false`. The old default is what #47 is about, so this is the change, not a side effect of it.
- **test:** a 32 MiB template clones clean on 4 MiB and 8 MiB slab slots — the geometry from #46, where the inode table ends around 2 MiB and the root directory's data block lands in the second half of the first slot. It fails on v8.2.0's `FileDevice` and passes on v8.2.1's, which confirms #46 as the copy-on-write short-copy fixed in `8dc3134` seen from the clone side rather than a size-specific defect of its own.

## [v8.2.1] — 2026-08-17

### 2026-08-17
- **fix:** copy-on-write lost half of every slot it copied. `FileDevice` passed up the byte count from a single `tokio::fs::File` read or write, and a single one of those moves at most 2 MiB — so copying a 4 MiB slab slot for a CoW clone copied the first 2 MiB and left the rest as whatever the new slot already held. A clone read its inherited data correctly until the guest wrote *anywhere* in a slot; from then on the parts of that slot the guest had not written read back as zeros, on disk, past `sync`, with `e2fsck` clean and no kernel complaint. `FileDevice` now transfers the whole buffer per call, which is what `BlockDevice` documents and what every caller assumes, and a short copy in `cow_write` fails the write instead of committing a half-copied slot. Nothing caught this because the engine sizes slots by device and every test used the 1 MiB default, under the cap; the two new tests use 4 MiB and 5 MiB.
- **test:** the filesystem-template CI script now seeds a template with content and reads it back through a real kernel — a deep path, a 200 KB multi-block file, and 400 names in one directory, which is enough to force a hash tree. It sweeps every entry's contents at mount and again after 32 MB is written into the clone, with the page cache dropped in between, and reads the tree with `debugfs -R htree_dump`. The CoW data loss above is what that sweep found on its first run.
- **chore:** both filesystem crate pins moved v1.0.2 → **v1.2.0**, together, so cargo still resolves one copy of `mkfs-ext4` and the two crates agree on the `BlockDevice` seam. Additive on both sides — nothing the engine calls changed shape, and the 378 tests here pass unchanged.
  - [`mkfs-ext4`](https://github.com/glennswest/mkfs.ext4.rs) gains extended attributes (v1.1.0) — the codecs for both places ext4 keeps them, in-inode and in a block of their own — so a filesystem written here can carry SELinux labels and POSIX ACLs; and the `dir_index` on-disk format with `Filesystem::lookup` walking the hash index (v1.2.0), which answers in a two- or three-block walk instead of a read of the whole directory. Its hashes are asserted against `debugfs -R dx_hash` from e2fsprogs rather than against itself.
  - [`fio-ext4`](https://github.com/glennswest/fio.ext4.rs) gains tar streaming with OCI whiteout semantics, hard links, `rename`, `write_at` and triple indirection (v1.1.0), and now *maintains* hash-indexed directories rather than only reading them (v1.2.0) — filling a directory with *n* names was *n*² block reads and is linear now. Two fixes there land on paths [`fs::files`](src/fs/files.rs) already uses: deleting a file whose xattrs lived in their own block leaked that block, and overwriting a file kept the old inode along with its mode, owner and labels.

### 2026-08-13
- **docs:** #39 confirmed fixed on RouterOS hardware against v8.2.0 and stormblockmk v0.7.0. A clone attached over NVMe-TCP takes writes, corroborated by the disk table rather than the return code: free space 234 438 656 → 234 434 560 (one 4 KiB block), free inodes 65 524 → 65 523. The geometry shows the new default profile — 65 536 inodes against the old 16 384, ~32 MB less free space on the same 256 MiB volume for the journal — and the clone carries a fresh UUID with valid checksums, so `metadata_csum_seed` kept the stamp to one superblock write. Six templates, 64m–10240m, built in 0.06–0.81 s each.

## [v8.2.0] — 2026-08-13

### 2026-08-13
- **fix:** `VolumeDevice` reports the volume's logical sector size to the formatter (#40). It implemented `size`/`read_at`/`write_at` but not `logical_sector_size`, so it inherited the 512-byte default and the size classes picked **1 KiB blocks** for a 256 MiB volume. Nothing downstream could correct it — kernel detection needs an fd to ioctl and a thin volume has none, so the device has to say. Measured on dev.g8.lo (kernel 6.17.1) with clones exported over iSCSI: `e2fsck -fn` passed clean on every one and **every mount failed**, `EXT4-fs (sdb): bad block size 1024`. Both crate pins moved v1.0.0 → v1.0.2 together, so cargo still resolves one copy of `mkfs-ext4`.
- **Verified on a real kernel.** `ci-fstemplate-verify.sh` on dev.g8.lo, Fedora kernel 6.17.1, four clones exported over iSCSI to the in-tree target and attached with open-iscsi: `blkid` reads them, `e2fsck -fn` is clean, all four mount read-write **at once**, take writes, unmount, and check clean again — with no ext4 complaint in the kernel log. Distinct filesystem UUIDs on every clone, the label carried through, and the `-O` overrides took.
- **perf:** measured there — one 256 MiB template formats and seals in **50 ms**; four concurrent take **79 ms** in total, not 4×50. A clone is 54–86 ms including its verification fsck. The journal-less, `metadata_csum`-less variant costs **3.9 s**, because without those features the inode tables cannot be left uninitialised and must be written out.
- **test:** the harness had three faults of its own, each of which reported success or hung on filesystems that were fine: a bare `wait` that also waited on the target (which never exits); a JSON field extractor that pasted its key path into a quoted `eval`, so every field read came back empty; and a build step that skipped when a binary already existed, verifying the *previous* commit. It also read the whole kernel log rather than the part this run produced, and its error pattern did not match `bad block size` — the one message the script exists to catch.

### 2026-08-12
- **feat:** the filesystem layer now formats and checks through [`mkfs-ext4`](https://github.com/glennswest/mkfs.ext4.rs) — a from-scratch async reimplementation of `mke2fs` and `e2fsck`, written against the e2fsprogs source and verified against a real kernel — instead of the hand-rolled writer that shipped a day earlier. `src/fs/ext4.rs` is now the seam: `VolumeDevice` adapts a stormblock `BlockDevice` to the one that crate formats through, so a thin volume is formatted in place with no loopback, no `/dev` node and no `mkfs.ext4` subprocess. `write_zeroes` becomes a discard on a blank thin volume, which is what keeps a template's allocation in kilobytes rather than the tens of megabytes its inode tables describe.
- **BREAKING:** template parameters are `mke2fs`'s vocabulary rather than one flag per feature: `fs` is `ext2`/`ext3`/`ext4`, `journal` is a tri-state (absent follows the kind), and `features` is an `-O` list (`"^64bit,^metadata_csum"`). The `64bit` request field is gone — say `"features": "^64bit"` to turn it off; it is reported back on the template alongside `metadata_csum` and `metadata_csum_seed`. The default is now what `mke2fs -t ext4` writes, which is also what RouterOS's own `format-drive` produces (#39): journal, extents, `flex_bg`, `64bit`, `metadata_csum`, `metadata_csum_seed`.
- **feat:** clone-time UUID stamping is cheap by construction. `metadata_csum_seed` puts the checksum seed in the superblock rather than deriving it from the UUID, so a new UUID invalidates nothing and the stamp stays one superblock write; a filesystem carrying `metadata_csum` without the seed has one pinned from its current UUID first, which is what `tune2fs -U` does for the same reason.
- **feat:** verification is a real check, not a reading of the state flags. The seal guard runs fsck over the template and names what it found; every clone is checked before hand-off and discarded rather than handed over if it does not check out (`"verify": false` to skip); and `POST /api/v1/volumes/{id}/fsck` checks any volume, with `?repair=true` correcting what can be corrected — RouterOS has no fsck and cannot cleanly unmount a network disk, so a volume it left dirty has nowhere else to be repaired.
- **perf:** formats and clones no longer queue. No lock is held across a format, a check or a stamp: the lifecycle takes the shared volume-manager and store locks in short windows, and the formatter takes `&self` so one format fans out across block groups. Two tests build four templates and mint eight clones concurrently and fsck every result.
- **Note:** the userspace file-I/O layer ([`fio-ext4`](https://github.com/glennswest/fio.ext4.rs)) is not wired in yet — it cannot currently be taken as a git dependency because its own `mkfs-ext4` dependency is a sibling path, filed as fio.ext4.rs#1. Seeding content into a template before sealing lands once that resolves.

### 2026-08-11
- **feat:** preformatted filesystem templates in core — *mkfs once, clone forever* (#38). `src/fs/ext4.rs` writes a blank ext4 in pure Rust (superblock, GDT, per-group bitmaps and inode tables, root, `lost+found`, optional journal), parses one back, and stamps identity; `src/fs/template.rs` runs the create → format → seal → clone lifecycle over the `VolumeManager`, persisted to `<data_dir>/fstemplates.json`. Formatting a 256 MiB ext4 over the network costs ~20 s; cloning a sealed template is a snapshot plus a 16-byte patch. Measured here: a 512 MiB template materialises under 64 MiB of slab, and a clone of it takes **exactly one slot**.
- **feat:** `/api/v1/fstemplates` — create (formats and seals in one call), list, get by id or name, `/{id}/seal`, `/{id}/clone`, delete (`?purge`, `?force`). `POST /api/v1/volumes` gains `from_template`, sharing one implementation with the clone endpoint so a clone always goes through the fresh-UUID stamp whichever door it came in; `size` and `array_id` become optional there (a clone knows its size and is placed by the slab registry).
- **feat:** the ext4 `64bit` feature is a per-template option (`{"64bit": true}`) — 64-byte group descriptors and block numbers past 2^32, required above 16 TiB and off below it because consumers that predate it are happier without. Formatting past 16 TiB without it is refused rather than silently truncated. It does not pull in `metadata_csum`, so a clone's UUID stamp stays a plain 16-byte patch either way.
- **feat:** journal on/off is a **per-template option**, not a build-time default. RouterOS cannot replay a journal, so one that ever goes dirty there leaves the filesystem read-only permanently, while a Linux host or VM wants the crash consistency — both variants coexist, told apart by name.
- **fix:** the seal guard checks every flag a consumer acts on — `VALID_FS` clear, `ERROR_FS` set, `RECOVER` pending, `ORPHAN_FS` pending — and names each one it found. Checking only `VALID_FS` is what let a template with `ERROR_FS` set and `RECOVER` pending seal cleanly and then surface days later, inside a container, as `Read-only file system` (stormblock-registry#10).
- **fix:** every clone is stamped with a fresh filesystem UUID (stormblockmk#12). Clones of one template were byte-identical, so two on one host collided on mount-by-UUID and in the blkid cache. This can only live in the engine: every consumer clones *through* it, so a UUID stamped in a layer above misses the clones that layer never touches. A stamp failure deletes the clone rather than handing out a duplicate identity.
- **Notes:** the format is deliberately conservative — `EXTENTS|FILETYPE` incompat, `SPARSE_SUPER|LARGE_FILE|EXTRA_ISIZE` ro_compat, no `metadata_csum`, `64bit`, `bigalloc` or `quota`. That is the set verified to mount read-write on RouterOS 7.22.2, and it is also what keeps the UUID stamp a 16-byte patch instead of a full group-checksum recompute. Backup superblocks are written at the start of the group's first block (e2fsprogs layout), `lost+found` exists so `e2fsck -fn` has nothing to report, and formatting a known-blank target skips all-zero blocks — which is why a template costs kilobytes rather than the tens of megabytes its inode tables describe.
- **Known limitation (addressed the following day):** a clone of one of these templates **mounted on RouterOS but rejected every write** (#39). RouterOS's own `format-drive ext4` produces a filesystem carrying `HAS_JOURNAL`, `64BIT`, `FLEX_BG` and `METADATA_CSUM`, none of which this formatter emitted except `64bit` on request. That profile is now the default — see the 2026-08-12 entries — and the write path was confirmed on RouterOS on 2026-08-13; #39 is closed.
- **test:** 25 unit tests (`src/fs/`), 9 HTTP-level tests (`tests/integration_fstemplates.rs`), and `ci-fstemplate-verify.sh` — template → seal → clone ×4 → iSCSI export → real open-iscsi initiator → `blkid` → `e2fsck -fn` → mount rw → write → umount → `e2fsck` again, checking that two clones of one template carry distinct UUIDs and that none mounts read-only.

- **docs:** `docs/protocol-overhead.md` — measured iSCSI vs NVMe-oF/TCP connection cost against both stormblock targets with real kernel initiators. Attach is **35.0 ms (NVMe-oF) vs 91.2 ms (iSCSI cold) / 75.5 ms (warm)**, p50 — a consistent 2.6× gap, but tens of milliseconds on both, *not* the seconds-vs-ms the observation suggested. The handshakes themselves are indistinguishable (NVMe connect 21.0 ms, iSCSI login 20.6 ms, 218 vs 255 packets); the gap is device materialisation — 14.0 ms vs 55.7 ms, where iSCSI pays a SCSI bus scan and `sd` probe (INQUIRY/VPD/READ CAPACITY/MODE SENSE) plus udev, and a separate TCP session for SendTargets discovery. Per-volume hot-add on an already-connected controller is **21.7 ms** with no reconnect or rescan (11 namespaces over 3 TCP connections), so 1000 containers cost ~21.7 s and 3 connections on NVMe-oF against ~75.5 s and ~2000 connections for session-per-volume iSCSI. Documents where second-scale attaches plausibly originate (iSCSI `login_timeout` 15 s / `replacement_timeout` 120 s retry paths, `iscsid` serialisation, udev/multipath settle under load, CSI-layer backoff) — none of which a steady-state benchmark exercises.
- **test:** `attach-bench.sh` (per-phase attach/detach for both protocols) and `hotadd-bench.sh` (per-volume cost on a live NVMe-oF controller).

## [v8.1.0] — 2026-08-11

### Added
- **feat:** background extent garbage collector (`[gc]` in `stormblock.toml`, on by default, 600 s interval). Reclaims slab slots no volume maps — the capacity #37 stranded, which is otherwise unrecoverable without reformatting the slab, since deleting a slab refuses any slab with allocated slots. `POST /api/v1/slabs/gc` runs a pass immediately (`?dry_run=true` to see what it would free, `?max_reclaim=N` to bound it); `GET /api/v1/slabs/gc` reports configuration and the last pass.
- **feat:** `SlabRegistry` reservations (`reserve` / `commit` / `is_reserved`). Allocation and mapping are two steps with the registry lock released between them, so a freshly allocated slot is briefly indistinguishable from a leaked one; reservations mark that window and the collector skips it. `ThinVolumeHandle` reserves on allocate and commits once the extent is in the GEM, including on the copy-on-write failure path, where the reservation is dropped so a slot stranded by a failed write becomes collectable rather than pinned for the process lifetime.

### Notes
- **Liveness is decided by the GEM's forward maps, never the reverse index.** The reverse index records only the *primary* owner of a copy-on-write slot, so `remove_volume` drops the entry for slots a surviving clone still shares — collecting on it would free live data. The union of the forward maps counts shared slots correctly by construction. `keeps_slots_a_clone_still_shares` pins this behaviour: it deletes a clone's source, asserts the reverse lookup is now empty, and asserts the slot survives.
- Two-pass confirmation (`confirm_passes`, default on) requires an orphan to be seen unreferenced by two consecutive passes, with the locks dropped in between, before its data is freed — defence in depth for any allocation path added later without a reservation. `max_reclaim_per_pass` (default 4096) bounds how long one pass holds the registry lock.
- **test:** 6 collector tests — the #37 leak state, clone-shared slots surviving, in-flight allocations skipped, dry run, two-pass deferral, and reclaim capping.

## [v8.0.0] — 2026-08-11

### Breaking
- **BREAKING:** `Slab::dec_ref_batch` returns `DecRefOutcome { freed, retained, rejected }` instead of `usize`, and no longer returns `Err` when a slot in the batch is already free — those slots come back in `rejected` while the rest of the batch is still released. Callers reading the old `usize` should use `.freed`; callers that matched on `Err` for a stale slot will no longer see one. Only code linking stormblock as a library is affected.

### Fixed
- **fix:** `delete_volume` silently leaked **every** extent of the volume being deleted (#37). `dec_ref_batch` validated the whole batch up front and returned `Err` if *any* slot was already free or at `ref_count == 0`, so a single stale extent-map entry rejected the batch and not one extent was released; `delete_snapshot` then discarded that error with `let _ =` and reported success. The slots stayed `Allocated` with `ref_count: 1` naming a volume that no longer existed, and nothing could reclaim them — `DELETE /api/v1/slabs/{id}` refuses any slab with `allocated_slots > 0`, which is exactly the state the leak created, so reformatting was the only recovery. Observed on rose1: 13 orphaned slots stranding 52 MB after every volume had been deleted. Release is now best-effort per slot — a stale entry costs that one extent, not the whole volume — and acquire stays all-or-nothing, since a half-applied `inc_ref` over-counts and is worth refusing while a half-applied release is strictly better than none.
- **fix:** a slot repeated inside one `dec_ref_batch` passed the up-front validation and was decremented twice, the second time against a slot the first pass had already set to `ref_count: 0` — underflowing to `u32::MAX` in release builds (panic in debug). Repeats are now detected and rejected.
- **fix:** the release path was silent everywhere it could lose space. `delete_snapshot` now logs rejected slots and the previously unreported case of a slab missing from the registry entirely; `reset_to_source` logs diverged extents it could not release; and the `let _ = dec_ref(...)` calls in `ThinVolumeHandle` (discard, copy-on-write, shrink) and `placement::migrate_extent` now warn instead of discarding the error. Silent divergence between the GEM and the slot table is what made the leak invisible.
- **test:** `one_stale_entry_does_not_strand_the_whole_batch` (12 extents, one stale — all 12 slots come back, previously zero) and `duplicate_slot_in_batch_does_not_underflow`.

### 2026-08-10
- **docs:** deck gains a Testing arc — the four-rung ladder (312 local tests → Linux CI → live interop against LIO/open-iscsi/kernel nvme → the M0 multi-host fleet), the `docs/m0-baseline.md` fio numbers from 3 storage nodes plus a kernel-initiator host, and what the fleet found that a single node hides: a 0.2–4.6 s sequential p99 tail that prices open issue #30.
- **docs:** `docs/presentation/stormblock.html` — 25-slide deep-dive deck on the engine (architecture, slab/GEM data model, drive backends, RAID, both target protocols, boot-from-StormBlock, cluster/placement), its feature usage (config, CLI, REST, clone-per-consumer), and the OpenShift interface via stormblock-csi (wandering master/slave pairs, the /v1 fencing contract). Keyboard/click navigation, prints to 16:9 PDF. Same house style as the llmpager deck.

## [v7.1.0] — 2026-08-09

### 2026-08-09
- **perf:** `V1State::save()` rewrote the entire control-plane state as pretty JSON on every mutation, making each operation O(total volumes) — measured at ~0.017 ms per existing volume, extrapolating to ~17 ms per clone at 1000 volumes and ~85 ms at 5000, which is the scale the registry model targets (#32). It now diffs against a cached copy of what is on disk and appends only the changed entries, rewriting the full snapshot once every 512 records. Call sites are unchanged.
- **perf:** clone latency is now flat in the number of existing volumes (0.64 / 0.62 / 0.69 ms at ~21 / ~42 / ~63 volumes, previously 0.81 / 1.09 / 1.51), and clone-and-attach p50 fell from 3.14 ms to **1.46 ms** — inside the 1–2 ms budget #4 asks for. Every operation roughly halved: clone 1.65 → 0.80 ms, attach 1.48 → 0.67 ms, delete 1.52 → 0.68 ms.
- **note:** durability is deliberately unchanged — the journal append is flushed and synced before `save()` returns, exactly as the full rewrite was. The alternative debounce approach would have traded away a property the CSI contract depends on. An append failure falls back to a full rewrite rather than dropping the change. Records are whole-entity upserts, which makes replay idempotent and compaction crash-safe: the snapshot is written first and the journal dropped second, so a crash in between re-applies entries the snapshot already holds. A torn final record stops replay and keeps everything before it.
- **test:** `ci-clone-attach-bench.sh` — clone/attach/reset/delete p50 and p99, plus latency against volume count.


## [v7.0.0] — 2026-08-09

### Breaking
- **BREAKING:** the Global Extent Map and Slab Registry moved from `Mutex` to `RwLock`, changing public signatures: `AppState::new`, the `AppState.gem` / `AppState.slab_registry` fields, `VolumeManager::gem()` / `registry()`, and `ThinVolumeHandle::new` now take `Arc<tokio::sync::RwLock<_>>`. Callers holding these types must switch `.lock().await` to `.read().await` or `.write().await`. Binary consumers are unaffected; only code linking stormblock as a library needs updating.

### 2026-08-09
- **perf:** GEM and SlabRegistry were each behind a `Mutex` shared by *every* volume, so an extent lookup on one volume blocked I/O on all of them. Both are now `RwLock`, so the read-dominated hot path (extent lookup, slab resolution — done per 4 KiB chunk) runs concurrently.
- **perf:** `ThinVolumeHandle::write` held the per-volume lock for the whole call, serialising every write to a volume regardless of which extent it touched. The lock is now taken only when the mapping actually changes (allocation or COW), and those paths re-read the extent under it — two writers can both observe "unmapped" before either allocates, and without that re-check the second would allocate a duplicate slot and discard the first writer's data. Steady-state writes to an exclusively-owned extent take no volume lock at all.
- **fix:** `--reactor-cores` did nothing. Both targets accepted a `&ReactorPool` and ignored it (the parameter was named `_reactor`), while `main` built a pool from the flag, never referenced it, and dropped it at shutdown — so every connection ran on the ambient runtime and the log showed a configured pool immediately followed by a throwaway single-core one. Connections now dispatch onto one shared pool, kept alive for the process lifetime. Verified on a 4-core host: `Reactor pool started: 4 cores, pin=true` / `Target connections dispatch across 4 reactor core(s)`, with the throwaway pool gone.
- **test:** concurrent first-writes to a single extent allocate exactly one slot and leave it untorn (this fails without the re-check), and concurrent writes to distinct extents all land correctly.


## [v6.6.0] — 2026-08-09

### 2026-08-09
- **fix:** `FileDevice::discard` was a no-op, so freeing a slot reclaimed it inside the slab while the backing store kept every byte it had ever written — measured live, allocation went 72 → 116 MB and never came down (#28). Regular files are now punched with `fallocate(PUNCH_HOLE|KEEP_SIZE)` (KEEP_SIZE preserves the apparent length so slab offsets stay valid) and block devices get `BLKDISCARD`. A device that cannot discard is treated as success rather than an error — the range is still logically free.
- **fix:** both reclaim routes now reach the device: `Slab::free()` and `dec_ref_batch()` discard the freed slots, coalescing contiguous runs into one call. Fixing only one would have left dropped clones leaking on the other. (#28)
- **fix:** NVMe-oF Set Features acked without filling completion DW0, so the host read a grant of zero — 0-based for "one queue" — producing `creating 1 I/O queues` and one core's worth of completions for the whole namespace. FID 0x07 now grants `min(requested, max_io_queues - 1)` and returns `(ncqa << 16) | nsqa`; Get Features reports the maximum. Verified live: a 4-core host went from `creating 1 I/O queues` to `creating 4 I/O queues`. (#27)
- **feat:** `GET /api/v1/sessions` — active iSCSI sessions with TSIH, ISID, initiator/target name, discovery flag and connection count, plus `stormblock_iscsi_sessions_total` / `_active` gauges. `active` excludes discovery sessions, which never address a LUN, so it is the number to check before withdrawing an export; counting them would make an idle target look busy. Consumers previously had to guess with a drain timer and could pull a LUN out from under a live mount. (#29)
- **fix:** the full-feature connection is now registered on its session, so the reported connection count reflects reality instead of always being zero (#29)


## [v6.5.1] — 2026-08-09

### 2026-08-09
Both fixes below were found by pointing a real `open-iscsi` initiator at the
target for the first time (Fedora 43, kernel 7.1.4). Neither could have been
caught by the existing tests: our own initiator issues one command at a time,
and the external iSCSI suite exercises *our initiator* against LIO rather than
our target.
- **fix:** iSCSI discovery never worked — the target echoed the declarative `SessionType` key back in the login response, and open-iscsi aborts the login on an unexpected key (`couldn't recognize text SessionType=Discovery`). `SessionType` is initiator-to-target only (RFC 7143 §12.21); it is now recorded as `SessionParams::discovery_session` instead of reflected. RouterOS never hit this because it is configured with an explicit IQN and skips discovery.
- **fix:** any iSCSI write larger than the immediate-data limit could kill the session. `receive_data_via_r2t` assumed the next PDU after an R2T belonged to that transfer; an initiator with several commands in flight interleaves them, so another command arriving mid-write was treated as a protocol error and the connection was dropped (`expected Data-Out PDU`, then a reconnect every 2s). Interleaved PDUs are now parked in a queue drained by the full-feature loop — mirroring what the NVMe-oF side already did for H2CData — and NOP-Out is answered inline so a long write cannot delay a keepalive past its timeout.
- **test:** `ci-iscsi-reclaim.sh` — end-to-end proof for #25 over a real initiator: ext4 → fill → delete → `fstrim`, watching the engine's own slab accounting. Verified on Fedora 43: `discard_max_bytes` is non-zero (so VPD 0xB2 advertising works), and allocation went 0 → 360 MB → 56 MB with 304 MB reclaimed.
- **test:** `ci-nvmeof-hotadd.sh` — Linux suite plus live hot-add against a real kernel `nvme_tcp` initiator; verified a hot-added namespace appears on an already-connected controller with no reconnect, and is withdrawn on detach.

## [v6.5.0] — 2026-08-08

### 2026-08-08 (reset primitive)
- **feat:** `POST /v1/volumes/{id}/reset` — discard a clone's divergence and return it to its source's contents without recreating the volume. Delete-and-reclone costs two reference updates for every extent in the golden image; reset touches only the extents the clone actually wrote, so a container restart scales with what that container changed rather than with the image it started from. Returns `freed_extents` / `restored_extents` / `shared_extents`. The volume keeps its id and attachment record. (#4)
- **feat:** `VolumeManager::reset_volume` and `snapshot::reset_to_source`; new references are taken before old ones are released, so an interruption leaks a reference rather than freeing live data. `GlobalExtentMap::inc_extent_ref` bumps the share count of a single extent — the GEM refcount is what makes a write copy instead of landing in place, so re-sharing must bump both sides or the next container write would scribble onto the golden image. (#4)
- **fix:** reset is refused with 409 while the volume is attached (contents cannot change under a live host) and for a volume that was not created from a source. `VolumeRec` now records `source_local`.

## [v6.4.0] — 2026-08-08

### 2026-08-08 (later)
- **feat:** NVMe-oF hot-add — a host connects once and later attaches cost an async event plus a rescan, with no Connect and no new TCP session per container. `add_namespace_dynamic`/`remove_namespace` raise a namespace-change event; the admin queue gets its own loop that selects over the socket and that event stream, holding the host's Asynchronous Event Requests and completing one the moment a namespace changes. Adds the Changed Namespace List log page (LID 0x04, cleared on read, with the `0xFFFFFFFF` rescan-everything sentinel when a connection falls behind) and advertises OAES bit 8, without which a host never arms for the event. (#4, #26)
- **BREAKING:** `AttachInfo::NvmeTcp` previously advertised a per-volume NQN (`nqn.2026-01.io.stormblock:<volume_id>`) that the target rejected at Connect — it only ever answered to its configured subsystem NQN, so the nvme-tcp attach path could not have worked as shipped. It now returns the real subsystem NQN plus a per-volume `nsid`. A per-volume NQN would also force a Connect per container, which hot-add exists to avoid. Consumers must read `nsid` to pick the right namespace. (#26)
- **feat:** `/v1` attach hot-adds the volume as a namespace and reports its NSID; detach withdraws it once no node holds the volume. Attach is idempotent — a replay reuses the namespace instead of leaking one. (#4)
- **fix:** `/v1` `delete_volume` tore down the ublk export but never released the NVMe namespace, so deleting a COW image left a namespace pointing at freed slots — hit constantly by the delete-and-reclone container restart cycle. Released before the backing volume goes away.
- **perf:** COW clone and delete batch their refcount persistence. Each `inc_ref`/`dec_ref` was a read-modify-write of a whole sector, so clone and delete cost two round trips per extent and scaled with image size; delete also rewrote the header once per freed slot. Slot entries are 64 bytes against 512/4096-byte sectors, so entries are now grouped by sector, a fully-covered sector skips its read, and the header is written once. Measured: 256 slots go from 256 writes + 256 reads to 4 writes and 0 reads. Matters most for VM images. (#4)

## [v6.3.0] — 2026-08-08

### 2026-08-08
- **fix:** iSCSI UNMAP/discard never reclaimed thin allocation, so usage only ever grew (#25). Two causes: the target never advertised thin provisioning — VPD page 0xB2 (Logical Block Provisioning) was absent and unlisted in the supported-pages page, so Linux left `discard_max_bytes` at 0 and issued no UNMAP at all — and even a well-behaved UNMAP would have failed, because only `WRITE_10`/`WRITE_16` collected a data-out payload, leaving `handle_unmap` with an empty parameter list. VPD 0xB2 now reports LBPU/LBPWS/LBPWS10/LBPRZ and thin provisioning type; READ CAPACITY(16) gains LBPRZ; data-out collection is driven by `is_data_out_command()` covering UNMAP, WRITE SAME(10/16) and MAINTENANCE OUT.
- **feat:** WRITE SAME(10/16) — with the UNMAP bit and an all-zero pattern it deallocates, otherwise it writes the pattern in bounded chunks (#25)
- **feat:** `BlockDevice::discard_granularity()` (default: block size; thin volumes report their slot size) drives the optimal unmap granularity and UGAVALID alignment in the Block Limits VPD page, so initiators align discards to something that actually frees space (#25)
- **feat:** `/metrics` samples slab capacity/allocated/free per slab and in total at scrape time, making thin growth and reclaim observable (#25)
- **feat:** `LunBacking::Volume { volume_id }` — thin/COW volumes export directly as iSCSI LUNs, resolved through the same handle `attach` uses (#22)
- **feat:** the LUN table is persisted to `<data_dir>/luns.json` (temp file + rename) and re-opened at startup, so API-created exports survive a restart; an unresolvable backing is skipped rather than fatal (#22)
- **feat:** `POST /api/v1/exports` now wires the export into the running target and returns `active` with the assigned `lun_id` (iSCSI) or `nsid` (NVMe-oF) instead of parking in `pending_restart`; DELETE tears it down on the target (#24, #26)
- **feat:** NVMe-oF namespaces can be added and removed at runtime (`add_namespace_dynamic`/`remove_namespace`/`list_namespaces`), replacing an `Arc<HashMap>` that panicked via `Arc::get_mut` once the target was shared (#26, #24)
- **feat:** `management.advertised_addr` config (also `$STORMBLOCK_ADVERTISED_ADDR`) — `/v1` attach info and the NVMe-oF discovery log page report a routable address instead of falling back to `127.0.0.1` (#26)
- **perf:** the iSCSI I/O path no longer allocates a `Vec` of every LUN ID per SCSI command — only REPORT LUNS gathers the list — and `AppState::lun_entries` becomes a `HashMap` keyed by LUN ID, so lookups are O(1) at thousands of LUNs (#24)
- **fix:** REPORT LUNS follows SPC-4 — LUN LIST LENGTH reports the full list size even when truncated to the allocation length, SELECT REPORT 0x00/0x02 return the list and 0x01 returns empty, reserved values and an under-sized allocation length are an illegal request; LUN encoding is peripheral below 256 and flat-space above, verified to 2000 LUNs (#24)
- **fix:** MAINTENANCE OUT (SET TARGET PORT GROUPS) is no longer refused on a readonly LUN — readonly rejection now uses `modifies_media()` rather than the data-out predicate (#25)
- **fix:** `DELETE /api/v1/luns/{id}` now succeeds for a LUN wired in from config at startup
- **feat:** `POST /api/v1/luns` may omit `lun_id` to be assigned the next free number; LUN numbers are handed out in one place shared with the export path, so the two cannot collide (#24)

### 2026-08-07
- **fix:** iSCSI target sequence numbers were at the wrong BHS offsets in every target→initiator PDU (StatSN written to the ExpCmdSN slot, ExpCmdSN to the DataSN slot) and login responses carried no StatSN/ExpCmdSN/MaxCmdSN at all — a spec-compliant initiator (RouterOS) saw `MaxCmdSN=0`, a closed command window, and could never issue a SCSI command, reconnecting every ~10s (#23). Login responses now carry correct sequence numbers seeded from the request CmdSN (immediate-aware), and the full-feature connection continues those counters.
- **fix:** iSCSI login operational stage (CSG=1) now captures `InitiatorName`/`TargetName`/`SessionType` — initiators that skip the security stage (RouterOS) no longer log in with an empty initiator name; CHAP-required targets reject the security-stage bypass (#23)
- **feat:** iSCSI full-feature-phase Text Request handling (`SendTargets` discovery reply with TargetName + TargetAddress) (#23)
- **feat:** per-PDU debug tracing in the iSCSI full-feature phase (opcode, ITT, CmdSN, CDB opcode, LUN, response status) — enable with `RUST_LOG=stormblock=debug`; unknown LUN and unsupported opcodes now log at warn (#23)
- **fix:** REPORT LUNS encoded LUN numbers into the wrong byte (`[lun, 0]` instead of peripheral `[0, lun]`), so reported non-zero LUNs could never be addressed back; LUN field decoding now masks the SAM-5 address-method bits (#23)
- **fix:** aarch64 build broken by hardcoded `*mut i8` cast in `gethostname` calls (`src/stormfs.rs`, `src/cluster/mod.rs`) — `c_char` is unsigned on aarch64/arm; now casts to `*mut libc::c_char` for portability (#21)

### 2026-07-19
- **feat:** ublk transport for the CSI `/v1` attach path — when `[management] ublk_transport = true` and a volume is attached on the node that holds its master, the engine exports the backing device as a local `/dev/ublkbN` and returns `AttachInfo::Ublk { device_hint }` instead of NVMe-oF/TCP coordinates, giving the CSI node a local device with no network round trip. Falls back transparently to nvme-tcp when ublk is unavailable (non-Linux, `ublk_drv` not loaded) — probed once at startup. Exports are torn down on detach/delete; the CSI node never disconnects a ublk device itself. Closes the ublk half of the attach contract (the `Ublk` variant existed in the wire type but was never produced). New `src/mgmt/ublk_export.rs`; policy `should_offer_ublk` and export bookkeeping are unit-tested off-Linux, the kernel export path is verified on dev.g8.lo.

### 2026-04-03
- **feat:** Dynamic iSCSI LUN management REST API (`POST/GET/DELETE /api/v1/luns`)
- **feat:** Readonly LUN support — `readonly` flag on LUN creation prevents SCSI writes (WRITE PROTECTED sense)
- **feat:** iSCSI target starts unconditionally — LUNs can be added at runtime via REST API, even with no initial device
- **feat:** `[[luns]]` TOML config section for declarative LUN provisioning at startup
- **feat:** `REPORT_LUNS` SCSI command now reports actual active LUN IDs (was hardcoded to LUN 0)
- **refactor:** iSCSI LUN map changed from `Arc<HashMap>` to `Arc<RwLock<HashMap>>` for runtime mutability
- **refactor:** `IscsiTarget` stored in `AppState` for REST API access
- **refactor:** `open_one_drive()` made public for runtime device opening

### 2026-03-26
- **feat:** `--ublk` flag on `boot-iscsi` CLI — exports each partition as `/dev/ublkbN` via UblkServer (Linux 6.0+)
- **feat:** `scripts/build-stormblock-initramfs.sh` — LinuxBoot initramfs builder (busybox + stormblock + ublk_drv + /init script)
- **feat:** `install-fedora-iscsi.sh` — 8-phase mkube CI job: provision iSCSI disk, format filesystems, install Fedora via dnf --installroot, configure for LinuxBoot-style boot
- **feat:** `systemd/stormblock-ublk.service` — safety net for post-switch_root (restarts stormblock if initramfs process dies)
- **fix:** ublk `UblkCtrlCmd` struct layout — match kernel UAPI `ublksrv_ctrl_cmd` exactly (32 bytes, len@6, addr@8)
- **fix:** ublk ioctl-encoded command numbers (`UBLK_U_CMD_*`) — required by kernel 6.1+ (was sending raw 0x04, now 0xC0207504)
- **fix:** ublk `queue_id` must be `-1` (0xFFFF) for ADD_DEV — kernel validates this field
- **fix:** ublk mknod fallback for `/dev/ublkcN` in containers (read major:minor from sysfs)
- **fix:** ublk submit FETCH_REQ before START_DEV — kernel requires all queues registered first (use Barrier for sync)
- **fix:** ublk START_DEV requires PID in `data[0]` — kernel validates `ublksrv_pid > 0`
- **fix:** ublk mknod `/dev/ublkbN` block devices in containers (sysfs major:minor fallback)
- **fix:** ublk orphan cleanup — STOP+DEL stale devices before ADD_DEV, request specific dev_id
- **fix:** ublk WRITE_ZEROES (op 5) handler — treat as discard for thin volumes
- **fix:** `install-fedora-iscsi.sh` — use `dnf5 group install` syntax (rawhide has dnf5, not dnf4)
- **fix:** `install-fedora-iscsi.sh` — `--use-host-config` for installroot repo access, explicit package list (no Minimal Install group), `tsflags=noscripts` for container scriptlet failures, vmlinuz copy from lib/modules, busybox install for initramfs build
- **chore:** Full 8-phase Fedora iSCSI install verified: 163 packages installed, vmlinuz (18M) + initramfs (4.4M) staged, all verification checks passed

### 2026-03-25
- **feat:** `IscsiDevice` — production iSCSI initiator implementing `BlockDevice` trait (login, READ/WRITE(10), READ CAPACITY, UNMAP, NOP-Out keepalive)
- **feat:** `DriveType::Iscsi` variant for iSCSI-backed block devices
- **feat:** `boot_iscsi` module — iSCSI boot disk orchestrator with multi-volume partitioned layout
- **feat:** `BootDiskLayout::parse()` — layout string parsing (e.g., `esp:256M,boot:512M,root:6G,swap:1G,home:rest`)
- **feat:** `IscsiBootManager::provision()` — connect to iSCSI, format slab, create ThinVolumes per partition
- **feat:** CLI `boot-iscsi` subcommand — provision partitioned boot disk on remote iSCSI target
- **feat:** CLI `migrate-boot` subcommand — migrate boot volumes from iSCSI slab to local disk via placement engine
- **test:** 11 boot-from-iSCSI integration tests (layout parsing, provisioning, slab migration)
- **test:** 10 iSCSI hardware integration tests (`tests/iscsi_blockdev.rs`) — IscsiDevice connect/read/write/flush, slab format+allocate+reopen, ThinVolume I/O, multi-volume isolation, live migration between disks
- **chore:** `boot-iscsi-test.sh` — CI script for mkube job runner (7 phases: build, test, IscsiDevice, slab+volume, migration, CLI, clippy)
- **fix:** iSCSI NOP-In handling — distinguish solicited (flush response) vs unsolicited (target ping) per RFC 7143
- **fix:** Slab slot table alignment for 512-byte sector devices — read-modify-write for sub-block slot entries
- **refactor:** Phase 4 API cleanup — replace DiskPool/VDrive with Slab REST API
- **BREAKING:** REST endpoint `/api/v1/pools` removed, replaced by `/api/v1/slabs` (list, get, format, delete, list slots)
- **BREAKING:** CLI subcommand `pool` removed, replaced by `slab` (format, list, info)
- **BREAKING:** `DriveType::VDrive` variant removed from public API
- **refactor:** AppState now holds `Arc<Mutex<SlabRegistry>>` + `Arc<Mutex<GlobalExtentMap>>` instead of `RwLock<HashMap<Uuid, DiskPool>>`
- **refactor:** `migrate_to_local()` simplified — no longer creates DiskPool/VDrive, directly uses RAID 1 add/rebuild/remove
- **chore:** Deleted dead code: `pool.rs` (714 lines), `vdrive.rs` (198 lines), `container.rs`, `container_registry.rs`
- **chore:** Removed `PoolConfig`, `VDriveConfig` from config parser
- **feat:** Placement engine Phase 3 — extent-level migration, slab evacuation, and rebalancing
- **feat:** `migrate_extent()` — move a single extent between slabs with data integrity, GEM update, and ref count management
- **feat:** `evacuate_slab()` — move all extents off a slab for device removal/maintenance
- **feat:** `rebalance()` — redistribute extents across slabs via EvenDistribution or TierAffinity strategy
- **feat:** `migrate_to_slab()` — format destination device as slab, register, and evacuate source slab
- **feat:** `slab_extents()` helper on GlobalExtentMap — collect all extents on a given slab via reverse index
- **feat:** `PlacementError` enum and result types for placement operations
- **feat:** `ci-test.sh` — comprehensive CI orchestrator for mkube job runner (5-phase: build, test+clippy, single-disk iSCSI, multi-disk iSCSI, release build)
- **test:** Multi-disk iSCSI tests — 3 disks (test1 10GB, stormblock-test2 5GB, stormblock-test3 5GB) exercised via job runner
- **fix:** iSCSI initiator — pad SCSI WRITE(10) data to block_size boundary (fixes CHECK CONDITION on 512-byte sector disks)
- **chore:** Dedicated 5GB iSCSI test disks (`boot-iscsi-src`, `boot-iscsi-dst`) for CI isolation
- **fix:** iSCSI initiator — track `ExpStatSN` from response PDUs (RFC 7143 §11.6.1); stale ExpStatSN caused target CmdSN window stall after ~128 commands, hanging large migrations
- **fix:** Resolve all compiler warnings and clippy lints for clean `clippy -- -D warnings` on Linux

### 2026-03-24
- **fix:** iSCSI initiator — strict two-phase login (Security→Operational→FullFeature) for LIO Target compatibility
- **fix:** iSCSI initiator — same ITT across all login PDUs per RFC 7143
- **fix:** iSCSI initiator — TSIH propagation from Phase 1 to Phase 2
- **fix:** iSCSI initiator — unique ISID per connection (atomic counter) to prevent session collisions
- **fix:** iSCSI initiator — ExpStatSN+1 after login for full-feature phase
- **fix:** iSCSI initiator — use target's ExpCmdSN from login response for SCSI command sequencing
- **fix:** iSCSI initiator — remove Immediate flag from SCSI write commands (LIO resets on Immediate writes)
- **fix:** iSCSI initiator — NOP-In handling in read loop
- **fix:** iSCSI initiator — use actual block_size from READ CAPACITY instead of hardcoded 4096
- **feat:** Containerfile.iscsi-test — pre-built iSCSI test container for fast iteration
- **feat:** run-iscsi-test.sh — unified runner for pre-built container or cargo build fallback
- **test:** All 3 external iSCSI tests pass against real LIO Target (discovery, write/read/verify, multi-block I/O)

### 2026-03-21
- **feat:** Shared io_uring-style ring buffer IPC — zero-copy shared-memory block I/O between StormFS and StormBlock via Unix socket + memfd + eventfd (`src/drive/uring_channel.rs`, `src/drive/uring_server.rs`)
- **refactor:** Rename Container → Slab throughout codebase — `container.rs` → `slab.rs`, `container_registry.rs` → `slab_registry.rs`, `ContainerId` → `SlabId`, magic `STRMCONT` → `STRMSLAB`
- **fix:** COW bug in Slab.free() — only remove from extent_index if it still points to the slot being freed (prevents index corruption after COW allocation)
- **feat:** Rewrite volume layer to use GEM + SlabRegistry (Phase 2) — ThinVolume is now config-only, all extent tracking via Global Extent Map, I/O routes through Slab slots, allocate-on-write and COW via slab slot allocation, VolumeManager formats Slabs internally from RAID arrays
- **refactor:** ThinVolumeHandle holds Arc<Mutex<GEM>> + Arc<Mutex<SlabRegistry>> instead of embedded extent_map + allocator
- **refactor:** snapshot_diff() now takes (&GlobalExtentMap, VolumeId, VolumeId) — compares slab slot mappings across volumes
- **refactor:** VolumeManager.create_volume() keeps backward-compatible array_id parameter, internally maps to slab preference

### 2026-03-20
- **feat:** Slab extent store — organic data placement with fixed-size 1 MB slots per device (`src/drive/slab.rs`)
- **feat:** Slab registry — tier-indexed slab lookup with best-fit allocation (`src/drive/slab_registry.rs`)
- **feat:** Global Extent Map (GEM) — cross-slab extent tracking with reverse index, COW snapshot cloning, rebuild-from-slabs recovery (`src/volume/gem.rs`)

### 2026-03-19
- **feat:** ublk server — exports BlockDevice as `/dev/ublkbN` via io_uring URING_CMD (replaces NBD)
- **feat:** Direct Linux boot — kernel cmdline and initramfs config generation (replaces iPXE scripts)
- **refactor:** Replace `stormblock nbd` CLI subcommand with `stormblock ublk`
- **refactor:** Migration orchestrator docs updated for ublk (NBD → ublk)
- **BREAKING:** NBD server removed (`src/drive/nbd.rs` deleted, `pub mod nbd` removed)
- **feat:** Placement engine with snapshot-fenced cold copies (`src/placement/`) — extent-level data replication across storage domains
- **feat:** Storage topology types — `StorageTier` (Hot/Warm/Cool/Cold), `Locality` (Local/Remote), `StorageDevice` wrapper
- **feat:** `ColdCopy` — snapshot-fenced replica with per-extent sync bitmap (bitvec), incremental update via `snapshot_diff()`
- **feat:** `PlacementEngine` — cold copy lifecycle management, device registry, async replication with rate limiting

## [v6.0.0] — 2026-03-19

### Added
- **DiskPool**: On-disk pool format with header, VDrive table, first-fit allocator (1 MB alignment), CRC32C checksums, free-space management
- **VDrive**: Offset-translating BlockDevice wrapper over parent device region, with bounds checking
- **NBD server**: Newstyle fixed negotiation protocol, exports any BlockDevice to kernel via `/dev/nbdN` (read/write/disc/flush/trim)
- **RAID 1 dynamic members**: `add_member()` spawns background rebuild, `remove_member()` validates minimum active count — enables live migration
- **DriveType::VDrive**: New variant for virtual drives backed by pool regions
- **Pool REST API**: `GET/POST/DELETE /api/v1/pools` and `/api/v1/pools/{id}/vdrives` for pool and VDrive management
- **RAID member API**: `POST /api/v1/arrays/{id}/members` and `DELETE /api/v1/arrays/{id}/members/{uuid}` for dynamic member management
- **Boot volume manager**: Template creation, per-machine COW snapshot provisioning, iPXE script generation for iSCSI sanboot
- **Migration orchestrator**: Live migrate from iSCSI to local disk via RAID 1 add/rebuild/remove — system never notices
- **CLI subcommands**: `stormblock pool format/list/vdrives/create-vdrive`, `stormblock nbd`, `stormblock migrate`
- **PoolConfig and BootConfig** in configuration parsing
- Pools tracking in AppState for runtime pool management
- 18 new tests (pool header roundtrip, VDrive offset translation, NBD handshake/IO, boot manager, migration)

### Changed
- RAID `members` field refactored from `Vec<MemberInfo>` to `std::sync::RwLock<Vec<MemberInfo>>` for concurrent access
- RAID `capacity` field changed to `AtomicU64` for thread-safe dynamic updates
- All RAID async I/O methods extract `Arc<dyn BlockDevice>` before `.await` (RwLock safety pattern)

## [v5.1.0] — 2026-03-09

### Added
- TLS for cluster RPCs — Raft, heartbeat, and join use HTTPS when `cluster.tls_enabled = true`
- Async replication retry with exponential backoff — retry queue (max 10K entries), up to 8 retries per request, 100ms–30s backoff, Prometheus metrics for retry success/failure/exhausted/dropped
- Fuzz testing for PDU parsers — 6 cargo-fuzz targets covering iSCSI BHS, iSCSI PDU read, iSCSI text params, NVMe-oF common header, NVMe-oF PDU read, NVMe-oF connect data
- StormBase ISO build script (`scripts/build-stormbase-iso.sh`)

### Fixed
- All compiler warnings (unused imports, dead code, unused variables)
- All 55 clippy warnings (Copy vs clone, redundant closures, derive Default, div_ceil, etc.)
- `.gitignore` now covers `target/` everywhere (was only `/target`)

### Changed
- Dockerfile: Alpine 3.21 runtime with storage tools (nvme-cli, smartmontools, fio, iproute2, util-linux, lsblk, e2fsprogs, xfsprogs, jq, ca-certificates)
- Dockerfile: stormblock binary installed to `/usr/bin/stormblock`
- TLS service error type for hyper-util compatibility
- IoUring type annotation for Linux build

## [v5.0.0] — 2026-02-23

### Added
- TLS support for management API via rustls (cert/key config in stormblock.toml)
- Drive health monitoring — SMART data via sysfs with REST endpoint (`GET /api/v1/drives/{id}/smart`)
- iSCSI multi-connection sessions and R2T/Data-Out for large write commands
- NVMe-oF io_uring zero-copy send for C2H data PDUs (Linux, 16KB+ threshold)
- SCSI ALUA (Asymmetric Logical Unit Access) for multipath I/O — REPORT/SET TARGET PORT GROUPS
- VFIO hugepage DMA allocator (MAP_HUGETLB with fallback) and IOVA lookup via /proc/self/pagemap
- NVMe VFIO driver init — open container/group/device, map BAR0, admin queue pair, controller enable
- StormFS registration stub — periodic volume announcement to StormFS metadata cluster

## [v4.0.0] — 2026-02-23

### Added
- Journal recovery and background scrub/verify for RAID engine
- Volume resize (grow/shrink) support with REST API endpoint
- HTMX + Askama web UI for storage management

## [v3.2.0] — 2026-02-19

### Added
- HTMX + Askama web UI for storage management (dashboard, drives, arrays, volumes, exports)

### Changed
- Switch reqwest to rustls-tls for fully static musl builds (no OpenSSL dependency)

### Fixed
- Fix ioctl calls to use `libc::Ioctl` for musl compatibility

## [v3.1.0] — 2026-02-19

### Added
- On-disk metadata persistence for volume state recovery (`--data-dir` flag)
- Binary envelope format with atomic writes and CRC32C checksums
- Restart recovery for extent allocator, thin volumes, and snapshots

## [v3.0.0] — 2026-02-19

### Added
- End-to-end integration tests (FileDevice → RAID 1 → ThinVolume → iSCSI/NVMe-oF target → TCP client)
- Crash recovery tests (journal persist/recovery, superblock validation, extent allocator consistency)
- RAID degraded mode tests (RAID 1 + RAID 5 with failed members)
- Management REST API tests (drives, arrays, volumes, exports, metrics endpoints)
- Volume lifecycle tests (create, snapshot COW, delete, multi-extent writes)
- Criterion micro-benchmarks (parity throughput, extent allocation, PDU parsing)
- fio macro-benchmark scripts (iSCSI + NVMe-oF, 4K random + sequential)
- Container images via Dockerfile for x86_64 and aarch64

### Breaking
- Major version bump for stabilized test/benchmark infrastructure

## [v2.0.0] — 2026-02-19

### Added
- **Phase 3 — Volume manager:** thin provisioning, COW snapshots, extent allocator with free-space bitmap, discard/TRIM handling, snapshot diff for incremental backup
- **Phase 4 — Target protocols:** iSCSI target (RFC 7143, CHAP MD5 auth, full SCSI command set including INQUIRY, READ/WRITE 10/16, READ_CAPACITY, MODE_SENSE, UNMAP, REPORT_LUNS, VPD pages), NVMe-oF/TCP target (fabric connect, discovery subsystem, admin + I/O commands, PDU parsing), per-core reactor pool with CPU pinning
- **Phase 5 — Management plane:** REST API via axum (drives, arrays, volumes, exports endpoints), TOML config parsing with validation, Prometheus metrics endpoint
- **Phase 6 — Cluster scaling:** Raft consensus via openraft 0.9, node discovery and membership, health heartbeat, synchronous and asynchronous replication, volume migration/rebalance, online node addition — all behind `#[cfg(feature = "cluster")]`

### Breaking
- Major version bump for new network protocol subsystems and cluster architecture

## [v1.0.0] — 2026-02-19

### Added
- **Phase 1 — Drive layer:** `BlockDevice` trait (async read/write/flush/discard), page-aligned DMA buffer allocator, SAS backend via io_uring (O_DIRECT, SSD/HDD detection, sysfs metadata), NVMe struct definitions (stub — needs bare metal), FileDevice portable fallback (tokio file I/O for MikroTik/dev/testing), drive enumeration and auto-detection
- **Phase 2 — RAID engine:** RAID 1 (mirror with read balancing), RAID 5 (XOR parity), RAID 6 (dual parity with GF(2^8) multiplication), RAID 10 (striped mirrors), SIMD parity compute (AVX2 x86_64, NEON aarch64, scalar fallback), write-intent bitmap journal with recovery, background rebuild with rate limiting, on-disk superblock format
- CLI entry point with `--device` flag, Ctrl+C graceful shutdown

## [v0.1.0] — 2026-02-17

### Added
- Initial project structure and module layout
- Specification document (`docs/stormblock-spec.md`)
- Source stubs for all planned modules
- Cargo.toml with dependency declarations (openraft 0.9, tokio, axum, io-uring, etc.)

### 2026-08-19
- **chore(deps):** mkfs-ext4 v2.0.0 (with `features = ["std"]`) and fio-ext4
  v1.4.0. `std` became a default feature in mkfs-ext4 so a UEFI driver can link
  its synchronous `no_std` read path — one implementation of the ext4 on-disk
  format for hosts and firmware both, rather than a second reader in firmware
  drifting against this one. `default-features = false` there now leaves the
  `no_std` core, so consumers that want the formatter ask for `std` explicitly.
  Both pins move together so cargo still resolves a single copy of mkfs-ext4.
- **feat:** the initramfs bonds the uplinks that are alike. A node was running
  on one port with a second cabled and idle, and `bond0` existed, was down and
  had no members — because loading the `bonding` module creates one empty bond
  by default and nothing ever put anything in it. Two cables into a machine
  mean somebody intended redundancy.
  `active-backup` by default, and that is a safety decision rather than a
  preference: 802.3ad needs a LAG configured on the switch, and pointing an
  LACP bond at a switch that has none leaves the ports unreliable — on the one
  step of the boot that can strand a node, from an initramfs with no way to
  ask. `rd.stormblock.bond=802.3ad` opts in on a node whose switch is known;
  `rd.stormblock.bond=off` disables it. A bond that cannot get a lease falls
  back to the single ports, and only ports at the top speed are bonded — a
  slow port is a fallback, not a peer.
- **fix:** a failed bond could leave a node with no bridge, and therefore no
  pod network. Two faults, both mine, both introduced with the bonding
  change: warnings inside `net_make_bond` printed to stdout and were captured
  as the device name, so the boot tried to bring up an interface called
  `WARNING: …`; and `net_bring_up` bridged only when `ip link add` *succeeded*,
  which is false when the bridge already exists — so the retry after a failed
  bond silently skipped bridging and put the address straight on the uplink.
  The node got a DHCP lease, had no `stormbr0`, and Cilium died at "unable to
  determine direct routing device" because it is configured with
  `devices: stormbr0` precisely because auto-detection skips bridges.
  The bridge is now created *or reused*, warnings go to stderr, and a bond
  device that is not in `/sys/class/net` is refused whatever it claims to be.
- **fix:** the initramfs never loaded `bridge.ko`, so `ip link add type bridge`
  failed with `RTNETLINK answers: Not supported` and the node came up with its
  address on the raw uplink and no `stormbr0`. Cilium is configured with
  `devices: stormbr0` — auto-detection skips bridges — so it died at "unable
  to determine direct routing device" and the node had no pod network while
  looking perfectly healthy from the outside. The module ships in the
  initramfs; nothing called `modprobe`.
- **feat(initramfs):** a machine with no SMBIOS serial identifies itself by its
  SMBIOS UUID (stormcos#46). A Dell has a service tag and a VM has none —
  Proxmox sets `uuid=` and leaves `serial=` empty — so a machine that was not
  hardware could not be told which image was its own, and dropped to a shell
  saying SMBIOS had no tag, which was true and not useful. The UUID is the
  same kind of fact: one per machine rather than per interface, stable across
  a NIC being replaced, already set by every hypervisor. A MAC was the other
  candidate and is worse on both counts. Serial still wins when it is set, so
  a deliberately-assigned `boothost/flow-1` beats a generated hex string, and
  nothing about the hardware path changes — on a Dell the serial *is* the
  service tag. Placeholder serials (`Not Specified`, `Default string`, `To Be
  Filled By O.E.M.`) are rejected: every VM from one hypervisor would
  otherwise answer the same string and claim each other's images.
