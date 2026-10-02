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
   Before: the entry was written at allocation and the data after it, so a
   power cut could keep the entry without the data, and recovery mapped the
   extent to a slot that never got it, losing what had been fsync'd in the
   rest of that extent.
2. **A freed slot is not reused until its free is durable.** Otherwise a cut
   can keep the new tenant's data and the old owner's entry. Freed slots wait
   for the next `sync` (or a flush when the slab has nothing else free).
3. **A first write fills the whole slot.** What the volume never wrote reads
   as zeros, never as the slot's previous tenant. Discard does not zero on most
   SSDs, and does nothing on an HDD.
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
9. **A flow-over cut short is finished, not lost.** When the local records
   name a slab that is not on the machine (the appliance clone a flow-over
   was moving from), `boot-local` claims a fresh clone of the same image,
   which carries the same slabs with the same bytes. It maps the unmoved
   extents onto the clone and hands the rest of the move to the successor.
   For that, the local system slab records every volume still on a flow
   source from the moment the flow-over starts (#258), not only the ones it
   has reached; otherwise a cut before a golden moved left a disk that did not
   name it, and the initramfs installed over the disk and its data half
   (11.63: 0 of 300 objects). `slab holds` answers such a disk with exit 3
   (the same release, unfinished), and `/init` boots it.
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

## How it is checked

* `tests/integration_power_cut.rs`:
  * `fsynced_writes_survive_a_power_cut_at_any_point` runs 300 random runs of
    writes, flushes, discards, churn from a second volume and record rewrites
    on a clone of a blank, then a cut keeping a random part of the cache, then
    a restore from the slabs alone. Every block acknowledged by a flush must
    read what it held then, or something written since. Before the fix, 182
    of the 300 lost data.
  * `a_stale_record_and_several_cow_generations_recover`.
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
* On metal: stormcentral's power-cut check (fastetcd, 300 objects, hard
  power-off, 5 runs). Passed 2026-09-27 on C2NR0Q2 with v19.2.1 in the engine
  and the initramfs: 1500 of 1500 objects (#171). Rule 9 has not yet met
  metal; that check is #172.

What the simulation cannot show: sectors torn inside one write, and drives
that lie about FLUSH. Torn sectors can be simulated; that is #191.
