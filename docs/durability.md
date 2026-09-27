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
* On metal: stormcentral's power-cut check (fastetcd, 300 objects, hard
  power-off, 5 runs). Passed 2026-09-27 on C2NR0Q2 with v19.2.1 in the engine
  and the initramfs: 1500 of 1500 objects (#171). Rule 9 has not yet met
  metal; that check is #172.

What the simulation cannot show: sectors torn inside one write, and drives
that lie about FLUSH. Torn sectors can be simulated; that is #191.
