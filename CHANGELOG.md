# Changelog

## [Unreleased]

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
