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
6. **WRITE_ZEROES is a promise.** On a thin volume it leaves unmapped extents
   unmapped and writes zeros into mapped ones. A failure is reported as EIO.
   Before, it was a discard: that ignored partial ranges and answered success
   when it failed.
7. **Discard leaves a shared extent alone.** Unmapping it is not durable until
   the record is rewritten, so after a cut it would be mapped again.

## How it is checked

* `tests/integration_power_cut.rs`:
  * `fsynced_writes_survive_a_power_cut_at_any_point` runs 300 random runs of
    writes, flushes, discards, churn from a second volume and record rewrites
    on a clone of a blank, then a cut keeping a random part of the cache, then
    a restore from the slabs alone. Every block acknowledged by a flush must
    read what it held then, or something written since. Before the fix, 182
    of the 300 lost data.
  * `a_stale_record_and_several_cow_generations_recover`.
* On metal: stormcentral's power-cut check (fastetcd, 300 objects, hard
  power-off, 5 runs).

What the simulation cannot show: sectors torn inside one write, and drives
that lie about FLUSH.
