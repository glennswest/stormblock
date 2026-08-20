# Changelog

## [Unreleased]

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
