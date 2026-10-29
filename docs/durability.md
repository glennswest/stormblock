# What survives a power cut

A consumer's `fsync` is a FLUSH on its block device (ublk, NVMe/TCP, iSCSI).
When that FLUSH completes, everything written before it must survive a power
cut: the engine's own metadata included, on any drive that honours FLUSH. This
page is the set of rules that makes it so. They came from issue #171, where
fastetcd's redb reported "All roots are corrupted" after every hard power-off
of a stormcos node.

## The model: a volatile write cache

A drive with a write-back cache may keep **any subset** of the writes it has
not flushed, in any order, when the power goes. Only flushed writes are
certain. `drive::crashdev::CrashDevice` models exactly that and is what the
tests run on.

## The rules

1. **Nothing durable names a slot before its data is.** A slot allocated for
   a write (a first write, a copy-on-write, a rebuild, a move) is taken in
   memory only (`Slab::allocate_deferred`). Its table entry is written at the
   next `Slab::sync`, which flushes the device (the data is on the media),
   writes the entries of slots confirmed since, and flushes again. A volume
   FLUSH calls `sync` on every slab the volume maps; writing the volume
   records (`VolumeManager::persist`) syncs every slab first.
   **No lock is held across a device flush** (#269): `slab::sync_registered`
   takes the registry's read lock only to publish the confirmed entries
   between its two flushes, and a persist takes its records in memory, then
   syncs every slab (all at once), then writes the records under a lock that
   never puts an older generation over a newer. Holding the registry across a
   flush let the first allocation queue for its write lock, after which every
   read of every volume queued behind it — on server3's spinning disk the
   whole node's I/O and API stopped for as long as the flow-over flushed.
   Device flushes are themselves shared (`drive::flushgate`): a flush covers
   every caller that asked before it began.
   `sync` is a group commit (#264): one runs at a time per slab, and it
   covers every caller that asked before it began, so concurrent FLUSHes on
   one slab cost one round of device flushes. Unserialised, a second caller
   could find the `ready` set already taken by a sync still writing it and
   acknowledge its FLUSH before its slots' entries were on the device.
   Before: the entry was written at allocation and the data after it, so a
   power cut could keep the entry without the data, and recovery mapped the
   extent to a slot that never got it, losing what had been fsync'd in the
   rest of that extent.
2. **A freed slot is not reused until its free is durable.** Otherwise a cut
   can keep the new tenant's data and the old owner's entry. Freed slots wait
   for the next `sync` (or a flush when the slab has nothing else free).
3. **A first write fills the whole slot.** What the volume never wrote reads
   as zeros, never as the slot's previous tenant. Discard does not zero on most
   SSDs, and does nothing on an HDD. The rest of the slot is zeroed with the
   device's own zeroing (#173): `BLKZEROOUT` on a block device (the drive's
   WRITE ZEROES / WRITE SAME where it has one), a punched hole in a file, with
   only the unaligned edges written. Writing a megabyte of zeros per extent made
   formatting a large blank about six times slower. Data and zeros are both
   covered by the next flush, before the slot's entry is published.
   `STORMBLOCK_ZERO_BY_WRITING=1` writes the zeros, to measure the difference.
4. **Share counts only move down after what replaced the share is durable.**
   A count on disk that is too low lets a write land in place in a slot
   another volume still reads, which corrupts a blank or a snapshot.
5. **Recovery does not trust a record that is behind the slot tables.** The
   volume records are rewritten only now and then. The slot tables carry every
   allocation since then. So `restore`:
   * takes a slot-table generation newer than the record, as before;
   * drops a recorded mapping whose slot has since been freed, taken again by
     the same volume for another extent, or taken by another live volume;
   * keeps a mapping to an ancestor's slot at the same extent, or to a
     slot shared by more than one volume (a composed disk's golden);
   * raises share counts to the mappings it restored. It never lowers them:
     a count that is too high costs one needless copy, never data.

   `adopt_slabs` (a daemon taking over the slabs on its drives) uses the
   same reconciliation, over every drive's slabs at once.

   **A moved primary is a generation newer** (#277). A move (flow-over,
   drain, rebalance) allocates the new slot at the extent's generation + 1,
   the generation the map takes. Until the persist that records the move, the
   record names the source; a write to the extent lands in place on the new
   slot, and its fsync publishes that slot's entry. At one generation the two
   slots tied and the record's (the source) won, so a cut there lost an
   acknowledged write; now the slot table is newer and restore takes the new
   slot. With no write since the move the two hold the same bytes. A mirror
   or parity leg keeps its generation (legs are told from a stale copy by
   sharing it): that window is still open for them, #316.
6. **WRITE_ZEROES is a promise.** On a thin volume it leaves unmapped extents
   unmapped and writes zeros into mapped ones. A failure is reported as EIO.
   Before, it was a discard: that ignored partial ranges and answered success
   when it failed.
7. **Discard leaves a shared extent alone.** Unmapping it is not durable until
   the record is rewritten, so after a cut it would be mapped again.
8. **A handover reads the slabs after the incumbent is gone.** `adopt-ublk`
   stands the incumbent down, waits for its process to exit, then restores
   (`handover::take_over`). Reading first left the incumbent's last
   allocations out of the successor's map, with their slots looking free.
   The cost is a gap in which nothing serves the devices, root included
   (#190). What reads nothing from a slab happens before the stand-down
   (#303, owner: A): each slab device is attached then (the block device
   opened, the NVMe/TCP namespace connected), so the gap does not pay for
   it; the partition table, slab headers, slot tables and records are read
   only after the incumbent has exited. Three rules cover the gap:
   - Before the stand-down (while refusing still leaves the incumbent
     serving), every local path the restore reads (`--meta`, a slab file or
     device) must be on memory or a disk that is not ublk
     (`drive::backing`). An overlay counts as ublk. A fabric host name is
     resolved then, because the resolver reads `/etc`. In the gap, such a
     read would not fail but wait for the server that is doing the reading.
   - A restore that fails in the gap is retried with backoff
     (`handover::take_over_retrying`: 1 s doubling to 15 s, for
     `STORMBLOCK_ADOPT_RESTORE_SECS`, default 120). Each failure is said
     on the console.
   - Giving up (or adopting no device) is loud and leaves the devices
     held. adopt-ublk writes `/run/stormblock/adopt-failed.json`, prints a
     FATAL and exits **75**. Held devices make reads wait instead of fail,
     and the next `adopt-ublk` takes them as it would from an incumbent
     (and removes the record).
9. **A flow-over cut short is finished, not lost** — either half (#285: the
   data half moves in the background too, quarantined and recorded on the
   local data slab like the system half on the system slab). When the local records
   name a slab that is not on the machine (the appliance clone a flow-over
   was moving from), `boot-local` claims a fresh clone of the same image,
   which carries the same slabs with the same bytes. It maps the unmoved
   extents onto the clone and hands the rest of the move to the successor.
   For that, the local system slab records every volume still on a flow
   source from the moment the flow-over starts (#258), not only the ones it
   has reached; otherwise a cut before a golden moved left a disk that did not
   name it, and the initramfs installed over the disk and its data half
   (11.63: 0 of 300 objects). `slab holds` answers such a disk with exit 3
   (the same release, unfinished), and `/init` boots it, handing the engine
   the clone it compared and the machine's name (#259). A clone that does not
   carry the slab the records need is another machine's image: `boot-local`
   refuses to boot rather than drop those mappings and run a root of holes.
10. **A move and an I/O on the same slot never overlap** (#239). An I/O
   looks its extent up and then uses the slot it found; the unreplicated path
   holds no lock between the two. A move (the flow-over, a drain, a
   rebalance) copies the slot, points every map at the copy and frees the
   source, and a free discards it. An I/O in that gap loses a write made after
   the copy, or reads zeros from the freed slot. A copy-on-write that reads
   zeros copies them into the clone's new slot around the bytes it writes.
   That was `cni-bin` on 11.56: the system half flowed onto the local disk while
   Cilium filled it, and its root directory came back without its checksum
   tail. The slot fence (`volume/fence.rs`) closes the gap. I/O holds the slots
   it looked up shared and looks again once it has them. A move holds the slot
   exclusive for the copy and the map rewrite, waiting for it before it takes
   the map and the registry. A move that already holds those only tries, and
   reports the slot busy. Every copy is read back and compared before any map
   names it. The flow-over quarantines its sources, so a copy-on-write made
   meanwhile lands on the local disk rather than behind the move.
   A parity volume's stripe is fenced whole (#240): its read-modify-write,
   discard, verify and resync hold every member and parity leg shared, after
   the stripe lock, for the whole operation, so a drain cannot move a parity
   leg between the read of the old parity and the write of the new. A read
   holds the stripe's slots while it reads its member, and lets them go
   before it takes the stripe lock to reconstruct one it cannot read. A slot
   is never held twice by one operation: the fence's lock queues a second
   reader behind a waiting move. StormFS chunks are freed by looking them up
   and unmapping them under one map write lock, the lock a move publishes
   under, so a move either went first and its copy is what is freed, or comes
   after and finds the extent gone.
   The flow-over's copy holds the fence and nothing else (#269): the
   registry is taken to allocate the destination (reserved against the
   collector), and the map and the registry to publish, after checking that
   the extent still names the source and carrying the source slot's share
   count as the slab has it then. Holding both for the copy queued every
   volume's I/O and every API call behind each move.
   **Moves run in windows** (#331): up to 64 extents (`STORMBLOCK_FLOW_WINDOW`,
   and at most 64 MiB of them), each slot once. Since #401 a window's slots
   whose fences are free are moved together: sources read 16 at once
   (`STORMBLOCK_FLOW_PARALLEL`, over the initiator's 8 connections),
   destination slots allocated together and written in slot order as merged
   runs, each run read back before any map names it, then one publish under
   the map and the registry with the same checks per extent. A fence is never
   waited for while others are held: a slot whose fence is taken moves on its
   own afterwards, as before. A source that reads all zeros is unmapped from
   every map naming it instead of copied, only where every reference is that
   slot alone; its slot is owed like a moved one. **One persist for a batch
   of moves** (#410): every 256 (`STORMBLOCK_FLOW_BATCH`), or 10 s after the
   last while anything is unpersisted (`STORMBLOCK_FLOW_PERSIST_SECS`), at the
   end, and before giving up; then the release of their source slots. A
   source slot stays owed, never freed, until the map naming its copy is
   durable, so a cut anywhere in a batch leaves each extent on its source or
   on a copy that is whole, and the batch is moved again.
   **What the node reads goes first** (#411): every read of a slot still on
   a source is counted (`flowhot`; the initramfs engine leaves its counts in
   `/run/stormblock/hot-extents.json` for its successor), and each pass moves
   the counted slots first, hottest first, the rest in disk order; reads
   during the pass move theirs up, at most once a second. Order changes when
   a slot moves, never how.
   **Paced by the node's own I/O** (#410; before it, any foreground I/O at
   all gave the disk back for a quarter of a window, #269, and the
   destination's own flush time over 1 s halved the window and paused, #282,
   which on a DRAM-less SSD flushing what the flow-over wrote held it to an
   extent a flush, ~1 MB/s on server8). Every volume read, write and flush is
   timed (`fglat`), apart while the flow-over moves and while it does not
   (its pauses, the boot grace). Flat out while the node's p90 while moving
   is under its floor (reads/writes `STORMBLOCK_FLOW_FG_IO_MS` 25, flushes
   `STORMBLOCK_FLOW_FG_FLUSH_MS` 100) or under `STORMBLOCK_FLOW_FG_RATIO` (3)
   times its p90 without; over both, the window halves and the flow-over
   pauses about that long (0.25–5 s), which also measures the node without
   it, and doubles back once under. A disk slow either way (an SMR disk, a
   cheap SSD) is moved onto at full speed. `STORMBLOCK_FLOW_PACE=0` turns it
   off. Pacing changes when moves happen, never the order above.
11. **Nothing is written in place on a slab being emptied** (#239, reopened).
   Rule 9's fresh clone carries the *image's* bytes, not what the last boot
   wrote on its own clone, so a write left on the appliance side is lost the
   moment the boot is cut short. Copy-on-writes already went to the local disk
   (rule 10's quarantine). In-place writes did not: a clone's own extents (the
   slot its UUID stamp copied, which holds an ext4's superblock, group
   descriptors, bitmaps and the start of its inode table) were written where
   they were. A resumed boot then read the local disk's directory blocks
   against the pristine image's inode table, and every file the node had
   created named an inode that table never got. That was the first free inodes
   of cadvisor, stormlb, vmimages, stormvm and stormimds on 11.57
   (`iget: checksum invalid`, #155–#164), and of hubble-relay on 11.50 (#1410).
   So the appliance's system slabs are quarantined as soon as a boot knows a
   flow-over is coming (`boot-local` once the disk is laid or the flow-over
   resumed, `adopt-ublk` before it serves), and a write to an extent with a leg
   on a quarantined slab goes through copy-on-write onto a slab that stays,
   freeing the old slot. Only with no room anywhere else does it fall back to
   writing in place.

12. **A change list is applied in the order it was taken** (#158, metadata
   format v2). A v2 persist writes what changed, not a snapshot, so the v1
   rule "never write an older record over a newer one" becomes: batches are
   applied in the order their records were taken, a batch taken before a
   failed write is dropped and the store is written whole next time, and a
   persist that stops before writing does the same. Rule 1 holds as before:
   the slabs are flushed after the records are taken and before the log
   record is written, and a log record is durable at its flush. A checkpoint
   writes new pages, flushes, then the other superblock; the pages it stops
   naming are reused only after that superblock is flushed, so the previous
   superblock's tree is whole until the new one is.
13. **A flush owes exactly what finished before it** (#338). A volume
   counts the writes, discards and write-zeroes that have finished on it
   (when each returns, failed or dropped included). A flush records the
   count it started from once it succeeds. A flush that finds nothing
   finished since then has nothing to make durable and returns without
   touching a device. Without that, a fresh clone's ext4 mount sent its
   FLUSHes through a device-wide sync, queued behind other volumes' syncs.
   A write still in flight when a flush starts is not owed by it, and it
   bumps the count when it lands, so the next flush is a full one. A
   handle's first flush is always full.
14. **A controller reset is waited out, not passed up** (#391). A local
   block device (a slab's drive, opened by path) is read and written
   through `drive::ridethrough`: an I/O that fails with a transport error
   (EIO, ENXIO, ENODEV, ENOLINK, ETIMEDOUT, EAGAIN, EBUSY, EREMOTEIO,
   ESHUTDOWN, ECONNRESET, or not ready) is tried again with backoff (100 ms
   doubling to 2 s) for a window, 60 s by default
   (`STORMBLOCK_TRANSPORT_WINDOW_SECS`, 0 = off). That is what an HBA host
   reset looks like from above: the disk answers nothing for seconds, then
   answers. Meanwhile the ublk request waits (privileged ublk has no
   per-request timeout), so the root filesystem and etcd see a slow I/O,
   not EIO, and nothing is marked failed. A medium error (ENODATA, EILSEQ)
   or a bad request (EINVAL) is not retried. Past the window the drive is
   given up: ERROR in the log, `gave_up` in health's `drives_unreachable`,
   and each I/O then tries once and fails, so the node is loudly degraded
   rather than silently hung or diskless. The first I/O that goes through
   clears it. Nothing acknowledged is acknowledged before the device took
   it, so the retry changes no ordering rule above.

15. **A FUA write is durable when it returns, and owes nothing else**
   (#409). A ublk device declares FUA (`UBLK_ATTR_FUA`), and the NVMe/TCP
   and iSCSI targets take the FUA bit, so a journal's commit block or an
   `O_DSYNC` write arrives as one FUA write rather than a write followed by
   a FLUSH of everything. The thin volume writes each leg of it with FUA at
   the drive when the leg's slot is already on the device: `RWF_DSYNC` on
   the O_DIRECT device where the drive itself takes FUA (`queue/fua` = 1:
   NVMe, a SAS/SATA drive the kernel lets FUA through), and a write and the
   device's group-committed flush where it does not (a SATA drive under
   libata's default) — there `RWF_DSYNC` becomes a kernel flush per
   request, which shares nothing and measured slower: that write waits for itself, not for every
   other volume's data in the drive's cache. When the write takes a slot (a
   first write, a copy-on-write) or lands in one whose entry is not
   published yet, its data is not enough: the entry has to be durable too
   (rule 1), so it is written and then made durable with the volume's flush,
   as a FLUSH would. A parity volume's FUA write is always a write and a
   flush. `STORMBLOCK_FUA_BY_FLUSH=1` makes every FUA write a write and a
   flush, for measuring. The marker is the drive's own FUA; there is no
   per-volume log, because a drive's cache flush is device-wide in any
   design and a FUA write needs none.

16. **What cannot be recorded is not made** (#428). Forge's hot slab
   (format 1) grew a record larger than its 100 MiB copy: every persist
   failed from 2026-10-10, the engine went on making volumes in memory, and
   the restart of 2026-10-14 lost the 483 made since. Now a failed record
   write is a durability fault: while it stands, creates, clones, snapshots
   and compositions are refused with its reason (each first retries the
   write, so the fault clears itself when it can); `/api/v1/health` says
   `status: degraded` with `durability`; the extent GC frees nothing (a
   record not written cannot be the GC's evidence) and its `POST` answers
   409. Deletes still persist,
   so removing volumes brings the record back under its copy and the fault
   clears by itself. Format 2 does not rescue such a slab: its entries carry
   a 25-byte key each and measured larger than format 1's record for the
   same volumes, so a record that outgrew a v1 copy does not fit the region
   as v2 either (the migration, when it is tried, takes the whole region
   with no v1 copy, and is refused as full when it does not fit). A WARN
   says when a record passes 80 % of what its slab reserves.

17. **A flow-over is proven before the node lets go of the appliance**
   (#412; the owner: "a post-flow-over validation via sha", on every
   flow-over, an OS feature). The flow-over holds every source slot it
   moved from (persisted away from, never freed: a freed slot is discarded,
   and over NVMe/TCP that unmaps the clone's extent on forge). When both
   halves have moved, every destination slot a sealed volume names is read
   back from the local drive (O_DIRECT there) and from its source, and the
   sha256s compared, paced as the flow-over is (#410), 16 at once. A slot
   only writable volumes name has been written since and is skipped (its
   golden's slots are checked). A mismatch is written again from the
   source, flushed and read back, up to 3 times. One still different is
   **FlowOverCorrupt**: every map naming it is pointed back at its source
   (the node reads it, correctly, from forge), persisted, said on the
   console and in health (`flow_over_stalled`, `flow_over.validation.state =
   corrupt`), and the node does not go local: no local boot, the sources
   kept, the install not reported. Only a pass frees the sources, lays local
   boot, retires them and reports the install, so a cut anywhere before it
   leaves the intent `install` (#220) and the next boot moves and checks
   again. The reference is the clone's own bytes (the owner's decision A):
   per-extent hashes recorded at seal come with #384, once no node can roll
   back past golden-stormblock-47e88a267077.
   **The validation is the flow-over's check** (#490, decided A on
   2026-10-22): a flow-over whose sources are held for it writes its copies
   without reading each back as it goes (#239's read-back halved a
   disk-bound destination's rate). What that read-back caught, a drive
   acknowledging a write it did not make, the validation catches before any
   source is let go, and repairs or reverts. Until then the node can read
   such a copy; nothing it wrote from it is let go of on the appliance's
   side, and the validation's repair rewrites the extent itself. A move with
   no hold (a drain, a rebalance) still reads every copy back.
18. **A delete lets go of a volume's slots once its writes have landed**
   (#487). A copy-on-write reads an extent's location, copies it, remaps it
   under the extent map's lock and then lets go of the old slot. A delete
   between the read and the remap took the volume's references, and the
   write then let go of the shared slot a second time: the slot was freed
   (and secure delete overwrote it) while its golden still mapped it.
   server1 12.40 lost a slot of the cilium golden that way, deleting a root
   while stormpump's mount was still writing it. So a delete holds the
   volume's write gate, which every write and discard holds shared for its
   whole length, and refuses the volume from then on: a write queued behind
   the delete fails instead of remapping a volume that is gone.
19. **A volume being attached cannot be deleted under its device** (#493).
   A ublk attach used to hold its volume only once the device was ready,
   and a delete could finish while the device was still starting, so the
   attach answered a device over a volume that was gone (pvetest1 12.42:
   the kubelet's startup sweep deleted three container roots it was
   attaching, and their mounts got EINVAL and ENXIO). The hold is now
   taken when the device starts. A delete reads the holds again after its
   write gate, with nothing awaited before the volume is removed, so a
   hold taken while it waited refuses it. An attach whose volume went
   anyway (the delete removed it first) takes its device down and answers
   404. An attach also answers only once its device is live (START_DEV
   returned), not when `/dev/ublkbN` exists: a device of the same number
   deleted a moment before can leave its node behind, and an open of it
   gives ENXIO.
20. **A detached device holds its volume until it is gone** (#502).
   A detach used to let go of the volume's hold the moment it told the
   device's server to stop. The server goes on serving what the kernel
   already queued, and a mount still being made reads through it, so a
   delete right after the detach went through and those reads found a
   volume that had been deleted (server1 12.54: the kubelet gave up on a
   cilium root whose mount was taking 30 s on the SMR disk, detached and
   deleted it; the mount got EIO). The volume is now held (`… being
   stopped`) until the server's thread has exited, and a detach answers
   once that has happened (bounded at 10 s; past it the volume stays held
   and a delete answers 409). A detach also refuses a device that is held
   open exclusively, as a mount is from its first moment, though it is in
   no mount table until it is made.

## T10 protection information (#389)

A drive formatted with PI type 1 (stormdrive formats shelf drives 4096 + PI1
where they can, 512 + PI1 elsewhere) has the block layer's integrity profile
`T10-DIF-TYPE1-CRC`: on every write the kernel generates each block's guard
tag (a CRC of its data) and reference tag (its LBA), the HBA and the drive
check them, and on every read they are checked again before the data reaches
the engine. The engine needs nothing for that: it follows the drive's
logical block (O_DIRECT alignment, the slab, the GPT at the drive's own LBA),
and volumes are 4096, so a 4Kn drive takes every request whole with no
read-modify-write and a quarter of the commands of a 512 one. Checked in
QEMU on scsi_debug PI drives at 4096 and at 512 (`ci-pi-verify.sh`).

**How it fits with the engine's own checks.** The engine keeps **no
per-block checksums**: a scrub compares mirror legs and parity (#160), a
sealed volume has a whole-volume digest (#384), and a flow-over validates
what it moved against its source (#412). PI is then the only per-block check
on the path, and it covers what those cannot: a block corrupted between the
kernel and the platter (the HBA, the cable, the drive's own memory), or a
misdirected write landing on the wrong LBA. It does not cover the engine's
own memory: the tags are generated in the kernel from the buffer the engine
hands it. Passing PI from the engine's buffers end to end (io_uring
integrity metadata, Linux 6.13+) is the follow-up, #511.

## How it is checked

* `tests/integration_power_cut.rs`:
  * `fsynced_writes_survive_a_power_cut_at_any_point` runs 300 random runs of
    writes, flushes, discards, churn from a second volume and record rewrites
    on a clone of a blank, then a cut keeping a random part of the cache, then
    a restore from the slabs alone. Every block acknowledged by a flush must
    read what it held then, or something written since. Before the fix, 182
    of the 300 lost data.
  * `a_stale_record_and_several_cow_generations_recover`.
  * `a_write_to_an_extent_just_moved_survives_a_cut_before_the_persist`
    (both formats, rule 5, #277): two extents moved the way the flow-over
    moves them, one written and fsync'd, a cut before any persist, a restore
    from the slabs alone. With the old rule (`MOVE_SAME_GEN_277=1`) the write
    reads as the value from before the move.
* `tests/integration_handover_order.rs` (rule 8):
  `the_successor_maps_what_the_incumbent_allocated_in_the_window`, and
  `restoring_before_the_incumbent_is_gone_misses_the_window` showing the old
  order's loss.
* `tests/integration_flowover_resume.rs` (rule 9):
  `a_cut_short_flow_over_resumes_from_a_fresh_clone`, and
  `without_a_source_the_unmoved_extents_are_missing_and_said_so`.
* `cli::install_tests` (rule 10):
  `an_io_that_found_its_slot_before_a_move_reads_what_was_there` holds a read
  of a golden, then a copy-on-write of its clone, inside the device while the
  flow-over moves the slot. Without the fence (`FENCE_OFF_239=1`) the read is
  16 of 16 blocks zeros, and the copy-on-write leaves 15 of 16.
  `the_flow_over_moves_a_live_clone_without_losing_a_byte` runs the flow-over
  under eight writers and four readers of a clone and its golden, over a
  source with a 0–4 ms round trip.
  `a_fresh_install_seeds_every_data_volume_byte_for_byte` builds an image with
  an e2fsprogs blank in its data slab and installs it onto an empty disk. It
  checks every volume's sha256 after the seed, after a fresh open of the image
  and the disk, and from the disk alone.
* `cli::install_tests::a_power_cut_anywhere_in_the_flow_over_keeps_every_acknowledged_write`
  (rules 9 and 10, #172). It installs a release onto an `emulated://…&volatile=1`
  disk (80 GiB, writes held until a flush). Consumers write a system volume, a
  data volume and a stamped service clone, which owns its first extent on the
  claim, with an fsync every 16 writes, and every acknowledged block is logged.
  The flow-over of both halves is cut at 10, 60, 75 and 95 % of its extents, and
  `emulated::crash` keeps a random half of what was not flushed. The next boot
  opens the disk and resumes from a fresh claim. Every acknowledged block reads
  what it held, or something written after it, and every other block reads the
  release's bytes. That is checked after the resume, after the flow-over
  finishes, and from the disk alone. With relocate-on-write off
  (`RELOCATE_OFF_239=1`), acknowledged writes to the owned extent read as the
  golden after the resume.
* On metal: stormcentral's power-cut check (fastetcd, 300 objects, hard
  power-off, 5 runs). Passed 2026-09-27 on C2NR0Q2 with v19.2.1 in the engine
  and the initramfs: 1500 of 1500 objects (#171). Rule 9 has not yet met
  metal; that check is #172.

**Torn writes and cuts inside an operation (#191).**
- **Torn writes:** `CrashDevice::crash_with` tears the writes it keeps. A kept
  write lands as a prefix of its blocks (a drive writing in order), or as any
  subset of them (out of order). It tears at the drive's atomic unit: 4096
  bytes, or 512 with `with_atomic_unit(512)` for a drive with 512-byte
  sectors, where even one 4 KiB slot-table page or record block can land in
  part.
- **Cuts inside an operation:** `CrashDevice::cut_at(n)` takes the power as
  the nth write arrives, so a persist or a slot-table sync is cut part-way,
  with its pieces cached.
- **Results:** format v2 keeps every acknowledged write through 300 cuts of
  each kind (none, prefix, scatter, 512-byte sectors), most of them inside an
  operation. Format v1 does not survive a cut inside a persist: #340, open.
  Its runs cut between operations until that is fixed.

What the simulation cannot show: drives that lie about FLUSH.
