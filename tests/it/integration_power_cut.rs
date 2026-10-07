//! Power cut: what a consumer fsync'd must be there after the engine restores
//! from its slabs alone (#171).
//!
//! fastetcd's redb came back "All roots are corrupted" after every hard
//! power-off of a stormcos node: writes it had fsync'd were not on the disk.
//! This reproduces that shape without hardware. A `CrashDevice` holds every
//! write in a volatile cache until a flush and, at the crash, keeps a random
//! subset of the unflushed ones — what a drive with a write-back cache may do
//! when the power goes. On it:
//!
//! 1. a blank, fully written (a formatted filesystem's metadata), and a
//!    copy-on-write clone of it — the shape of a claim;
//! 2. random 4 KiB writes into the clone, numbered, with flushes (the
//!    consumer's fsync), discards, and a second volume churning allocations so
//!    slots are freed and reused;
//! 3. a crash at a random point, then a fresh engine that restores from the
//!    slabs alone — no data directory, as on a stormcos node;
//! 4. every block of the clone checked: a block acknowledged by a flush reads
//!    what was written before that flush or anything written to it since;
//!    a block never written by the clone reads the blank's content.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rand::{Rng, SeedableRng};
use stormblock::drive::crashdev::{CrashDevice, Tear};
use stormblock::drive::slab::{Slab, SlabFormat, SlabRole};
use stormblock::drive::BlockDevice;
use stormblock::placement::topology::StorageTier;
use stormblock::volume::VolumeManager;

const SLOT: u64 = 64 * 1024;
const BLOCK: u64 = 4096;
const VOL: u64 = 2 * 1024 * 1024; // 32 extents, 512 blocks
const BLOCKS: u64 = VOL / BLOCK;

/// A block's content: its index and a value, repeated, so a block read from
/// the wrong place is caught as surely as a stale one.
fn block(idx: u64, value: u64) -> Vec<u8> {
    let mut b = vec![0u8; BLOCK as usize];
    if value == 0 {
        return b;
    }
    for c in b.chunks_mut(16) {
        c[..8].copy_from_slice(&idx.to_le_bytes());
        c[8..].copy_from_slice(&value.to_le_bytes());
    }
    b
}

fn value_of(idx: u64, b: &[u8]) -> Result<u64, String> {
    if b.iter().all(|&x| x == 0) {
        return Ok(0);
    }
    let i = u64::from_le_bytes(b[..8].try_into().unwrap());
    let v = u64::from_le_bytes(b[8..16].try_into().unwrap());
    if i != idx || block(i, v)[..b.len()] != *b {
        return Err(format!("block {idx} holds garbage (claims index {i}, value {v})"));
    }
    Ok(v)
}

/// The value of each 512-byte sector of a block (#191). A consumer's 4 KiB
/// write the drive cut part-way holds sectors of the old value and the new:
/// that is the consumer's to sort out (it did not flush), as long as every
/// sector is one of the values the block may hold.
fn sector_values(idx: u64, b: &[u8]) -> Result<HashSet<u64>, String> {
    b.chunks(512).map(|s| value_of(idx, s)).collect()
}

fn blank_value(idx: u64) -> u64 {
    1_000_000 + idx
}

/// What a block may read after a crash.
#[derive(Clone)]
enum Allowed {
    /// One of these values.
    Values(HashSet<u64>),
    /// Anything that is not garbage: a discard since the last write.
    Any,
}

async fn trial(seed: u64, version: u32, tear: Tear, atomic: usize) -> Result<(usize, bool), String> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let dev = Arc::new(CrashDevice::new(32 * 1024 * 1024).with_atomic_unit(atomic));
    let fmt = SlabFormat::new(SLOT, StorageTier::Hot)
        .with_role(SlabRole::Data)
        .with_version(version)
        .with_auto_metadata(dev.capacity_bytes());
    let slab = Slab::format_with(dev.clone() as Arc<dyn BlockDevice>, fmt).await.map_err(|e| e.to_string())?;
    let sid = slab.slab_id();
    let mut vm = VolumeManager::new(SLOT);
    vm.add_slab(slab).await;
    vm.persist_to_slab(sid);

    // The blank: every block written, as a formatted filesystem's metadata
    // would be, then made durable.
    let blank = vm.create_volume_any("blank", VOL).await.map_err(|e| e.to_string())?;
    let bv = vm.get_volume(&blank).unwrap();
    for i in 0..BLOCKS {
        bv.write(i * BLOCK, &block(i, blank_value(i))).await.map_err(|e| e.to_string())?;
    }
    bv.flush().await.map_err(|e| e.to_string())?;
    vm.persist().await;
    // The claim: a copy-on-write clone of the blank.
    let clone = vm.create_snapshot(blank, "clone").await.map_err(|e| e.to_string())?;
    let cv = vm.get_volume(&clone).unwrap();
    cv.flush().await.map_err(|e| e.to_string())?;
    // A second volume, to free and reuse slots under the clone.
    let other = vm.create_volume_any("other", VOL).await.map_err(|e| e.to_string())?;
    let ov = vm.get_volume(&other).unwrap();

    // What each block of the clone may read after a crash.
    let mut allowed: HashMap<u64, Allowed> =
        (0..BLOCKS).map(|i| (i, Allowed::Values([blank_value(i)].into_iter().collect()))).collect();
    // The value each block holds now.
    let mut current: HashMap<u64, Option<u64>> = (0..BLOCKS).map(|i| (i, Some(blank_value(i)))).collect();

    let ops = rng.gen_range(20..200);
    // In most trials the power goes in the middle of an operation (#191): as
    // a random write arrives, so a persist or a slot-table sync is caught
    // part-way, its pieces cached, to be kept, torn or lost.
    if rng.gen_bool(0.8) {
        dev.cut_at(rng.gen_range(1..=ops as u64));
    }
    let mut seq = 1u64;
    // The operation the power went in, if it went in one.
    let mut cut_in = "none";
    for _ in 0..ops {
        // What a block may read had the power gone before this operation.
        let before = allowed.clone();
        let r: f64 = rng.gen();
        let op = if r < 0.70 {
            "write"
        } else if r < 0.85 {
            "flush"
        } else if r < 0.90 {
            "discard"
        } else if r < 0.97 {
            "churn"
        } else {
            "persist"
        };
        if r < 0.70 {
            let i = rng.gen_range(0..BLOCKS);
            seq += 1;
            cv.write(i * BLOCK, &block(i, seq)).await.map_err(|e| e.to_string())?;
            current.insert(i, Some(seq));
            if let Allowed::Values(s) = allowed.get_mut(&i).unwrap() {
                s.insert(seq);
            }
        } else if r < 0.85 {
            // The consumer's fsync: from here on, a block may read only what
            // it holds now.
            cv.flush().await.map_err(|e| e.to_string())?;
            for i in 0..BLOCKS {
                let a = match current[&i] {
                    Some(v) => Allowed::Values([v].into_iter().collect()),
                    None => Allowed::Any,
                };
                allowed.insert(i, a);
            }
        } else if r < 0.90 {
            // Discard a whole extent: its blocks are undefined until rewritten.
            let e = rng.gen_range(0..VOL / SLOT);
            cv.discard(e * SLOT, SLOT).await.map_err(|e| e.to_string())?;
            for i in (e * SLOT / BLOCK)..((e + 1) * SLOT / BLOCK) {
                current.insert(i, None);
                allowed.insert(i, Allowed::Any);
            }
        } else if r < 0.97 {
            // Churn: the other volume takes and gives back slots.
            let e = rng.gen_range(0..VOL / SLOT);
            if rng.gen_bool(0.5) {
                ov.write(e * SLOT, &vec![0xEE; SLOT as usize]).await.map_err(|e| e.to_string())?;
            } else {
                ov.discard(e * SLOT, SLOT).await.map_err(|e| e.to_string())?;
            }
        } else {
            // Something that rewrites the volume records.
            vm.persist().await;
        }
        if dev.cut_taken() {
            cut_in = op;
            // The power went during this operation: a block may read what
            // was allowed before it or after it.
            for (i, a) in allowed.iter_mut() {
                *a = match (&before[i], &*a) {
                    (Allowed::Values(b), Allowed::Values(c)) => Allowed::Values(b.union(c).copied().collect()),
                    _ => Allowed::Any,
                };
            }
            break;
        }
    }

    // The power goes.
    let keep = rng.gen_range(0.0..1.0);
    let after = Arc::new(dev.crash_with(seed, keep, tear));
    let torn = after.torn_writes();
    let mid_op = dev.cut_taken();
    let slab = Slab::open(after.clone() as Arc<dyn BlockDevice>).await.map_err(|e| format!("reopen: {e}"))?;
    let mut vm2 = VolumeManager::new(SLOT);
    vm2.add_slab(slab).await;
    vm2.persist_to_slab(sid);
    vm2.restore().await.map_err(|e| format!("restore: {e}"))?;
    let id = vm2.find_volume("clone").await.ok_or("the clone did not come back")?;
    let rv = vm2.get_volume(&id).unwrap();
    let mut buf = vec![0u8; BLOCK as usize];
    for i in 0..BLOCKS {
        rv.read(i * BLOCK, &mut buf).await.map_err(|e| format!("read block {i}: {e}"))?;
        // Whole blocks, unless the drive can tear inside one (#191).
        let vs = if atomic < BLOCK as usize {
            sector_values(i, &buf)
        } else {
            value_of(i, &buf).map(|v| HashSet::from([v]))
        }
        .map_err(|e| format!("seed {seed} ({tear:?}, atomic {atomic}): {e}"))?;
        match &allowed[&i] {
            Allowed::Values(s) if !vs.is_subset(s) => {
                return Err(format!(
                    "seed {seed} (keep {keep:.2}, {tear:?}, atomic {atomic}, {ops} ops, cut in {cut_in}): block {i} reads {vs:?}, allowed {:?}",
                    s
                ))
            }
            _ => {}
        }
    }
    Ok((torn, mid_op))
}

#[tokio::test]
async fn fsynced_writes_survive_a_power_cut_at_any_point() {
    power_cuts(1, Tear::None, 4096).await;
}

/// The same 300 cuts over a slab in format v2 (#158): the volume records are
/// a log of changes and a copy-on-write tree, not two whole copies.
#[tokio::test]
async fn fsynced_writes_survive_a_power_cut_at_any_point_in_format_v2() {
    power_cuts(2, Tear::None, 4096).await;
}

/// #191: the same cuts, with half the kept multi-block writes torn — only
/// their first blocks landed, as a drive writing in order leaves them when the
/// power goes mid-write. The volume records and the slot-table pages are
/// such writes, and must survive it.
#[tokio::test]
async fn fsynced_writes_survive_a_power_cut_that_tears_writes() {
    power_cuts(1, Tear::Prefix(0.5), 4096).await;
}

#[tokio::test]
async fn fsynced_writes_survive_a_power_cut_that_tears_writes_in_format_v2() {
    power_cuts(2, Tear::Prefix(0.5), 4096).await;
}

/// #191: torn out of order — any subset of a write's blocks landed.
#[tokio::test]
async fn fsynced_writes_survive_a_power_cut_that_scatters_writes() {
    power_cuts(1, Tear::Scatter(0.5), 4096).await;
}

#[tokio::test]
async fn fsynced_writes_survive_a_power_cut_that_scatters_writes_in_format_v2() {
    power_cuts(2, Tear::Scatter(0.5), 4096).await;
}

/// #191 on a drive with 512-byte sectors: any 512-byte sector of any kept
/// write may be missing, so even a single 4 KiB slot-table page or record
/// block can land in part.
#[tokio::test]
async fn fsynced_writes_survive_a_power_cut_that_tears_sectors() {
    power_cuts(1, Tear::Scatter(0.5), 512).await;
}

#[tokio::test]
async fn fsynced_writes_survive_a_power_cut_that_tears_sectors_in_format_v2() {
    power_cuts(2, Tear::Scatter(0.5), 512).await;
}

async fn power_cuts(version: u32, tear: Tear, atomic: usize) {
    let mut failures = Vec::new();
    let (mut torn, mut mid_op) = (0usize, 0usize);
    for seed in 0..300u64 {
        match trial(seed, version, tear, atomic).await {
            Ok((t, m)) => {
                torn += t;
                mid_op += m as usize;
            }
            Err(e) => failures.push(e),
        }
    }
    println!("{tear:?} at {atomic} bytes: {mid_op} of 300 cuts inside an operation, {torn} write(s) torn");
    assert!(
        failures.is_empty(),
        "{} of 300 power cuts lost acknowledged data; first: {}",
        failures.len(),
        failures.join("\n")
    );
    if tear != Tear::None {
        assert!(torn > 0, "the cuts tore no write: the test exercised nothing");
    }
    assert!(mid_op >= 100, "too few cuts landed inside an operation to test anything: {mid_op}");
}

/// Recovery from a record that is behind the slot tables, with several
/// copy-on-write generations of one extent (#171's acceptance).
///
/// The record is written once, early; afterwards the clone copies extent 0
/// twice (once from the blank, once more after a snapshot of the clone
/// shares it again), writes extent 1 into a slot of its own, gives that slot
/// back, and a second volume takes it. Everything is flushed, the power goes
/// with nothing unflushed kept, and a fresh engine restores from the slabs.
/// Extent 0 must read the newest generation in the clone, the middle one in
/// the snapshot and the original in the blank; extent 1 of the clone, whose
/// recorded slot now belongs to the other volume, must not read that
/// volume's bytes.
#[tokio::test]
async fn a_stale_record_and_several_cow_generations_recover() {
    stale_record(1).await;
}

#[tokio::test]
async fn a_stale_record_and_several_cow_generations_recover_in_format_v2() {
    stale_record(2).await;
}

async fn stale_record(version: u32) {
    let dev = Arc::new(CrashDevice::new(32 * 1024 * 1024));
    let fmt = SlabFormat::new(SLOT, StorageTier::Hot)
        .with_role(SlabRole::Data)
        .with_version(version)
        .with_auto_metadata(dev.capacity_bytes());
    let slab = Slab::format_with(dev.clone() as Arc<dyn BlockDevice>, fmt).await.unwrap();
    let sid = slab.slab_id();
    let mut vm = VolumeManager::new(SLOT);
    vm.add_slab(slab).await;
    vm.persist_to_slab(sid);

    let blank = vm.create_volume_any("blank", VOL).await.unwrap();
    let bv = vm.get_volume(&blank).unwrap();
    for i in 0..BLOCKS {
        bv.write(i * BLOCK, &block(i, blank_value(i))).await.unwrap();
    }
    bv.flush().await.unwrap();
    let clone = vm.create_snapshot(blank, "clone").await.unwrap();
    let cv = vm.get_volume(&clone).unwrap();
    // Extent 1 into a slot of the clone's own, then recorded.
    let e1 = SLOT / BLOCK;
    cv.write(e1 * BLOCK, &block(e1, 7)).await.unwrap();
    cv.flush().await.unwrap();
    vm.persist().await;

    // Generation 2 of extent 0.
    cv.write(0, &block(0, 2)).await.unwrap();
    cv.flush().await.unwrap();
    let snap = vm.create_snapshot(clone, "snap").await.unwrap();
    // Generation 3: the snapshot shares generation 2, so this copies again.
    cv.write(0, &block(0, 3)).await.unwrap();
    // Extent 1 given back, and taken by another volume.
    cv.discard(SLOT, SLOT).await.unwrap();
    cv.flush().await.unwrap();
    let other = vm.create_volume_any("other", VOL).await.unwrap();
    let ov = vm.get_volume(&other).unwrap();
    for e in 0..(VOL / SLOT) {
        ov.write(e * SLOT, &vec![0xEE; SLOT as usize]).await.unwrap();
    }
    ov.flush().await.unwrap();

    // Power cut, nothing unflushed kept.
    let after = Arc::new(dev.crash(1, 0.0));
    let slab = Slab::open(after.clone() as Arc<dyn BlockDevice>).await.unwrap();
    let mut vm2 = VolumeManager::new(SLOT);
    vm2.add_slab(slab).await;
    vm2.persist_to_slab(sid);
    vm2.restore().await.unwrap();

    assert_eq!(read_block(&vm2, "clone", 0).await, Ok(3), "the clone's newest generation");
    assert_eq!(read_block(&vm2, "snap", 0).await, Ok(2), "the snapshot's generation");
    assert_eq!(read_block(&vm2, "blank", 0).await, Ok(blank_value(0)), "the blank's original");
    // Discarded: anything but the other volume's bytes.
    let r = read_block(&vm2, "clone", e1).await;
    assert!(r.is_ok(), "the clone's discarded extent reads another volume's data: {r:?}");
    // The rest of the clone is still the blank's.
    for i in 1..e1 {
        assert_eq!(read_block(&vm2, "clone", i).await, Ok(blank_value(i)));
    }
    let _ = (snap, other);

    // And the share counts let a write copy rather than land in place: a
    // write to the clone's extent 2 (shared with the blank and the snapshot)
    // must not change the blank.
    let id = vm2.find_volume("clone").await.unwrap();
    let v = vm2.get_volume(&id).unwrap();
    let e2 = 2 * SLOT / BLOCK;
    v.write(e2 * BLOCK, &block(e2, 99)).await.unwrap();
    assert_eq!(read_block(&vm2, "blank", e2).await, Ok(blank_value(e2)));
    assert_eq!(read_block(&vm2, "snap", e2).await, Ok(blank_value(e2)));
}

async fn read_block(vm: &VolumeManager, name: &str, idx: u64) -> Result<u64, String> {
    let id = vm.find_volume(name).await.unwrap_or_else(|| panic!("{name} did not come back"));
    let v = vm.get_volume(&id).unwrap();
    let mut b = vec![0u8; BLOCK as usize];
    v.read(idx * BLOCK, &mut b).await.unwrap();
    value_of(idx, &b)
}

/// #277: a write to an extent the flow-over (or a drain) has just moved,
/// fsync'd, then the power goes before the persist that records the move.
///
/// The write went in place to the destination slot and the fsync published
/// that slot's entry; the durable record still names the source. The two
/// slots used to carry one generation, so restore kept the record's (the
/// source) and the acknowledged write was gone. Restored from the slabs
/// alone, the write must be there — and the rest of the moved extent, and a
/// moved extent nobody wrote, must read what they held.
#[tokio::test]
async fn a_write_to_an_extent_just_moved_survives_a_cut_before_the_persist() {
    write_after_move(stormblock::drive::slab::SLAB_VERSION).await;
}

#[tokio::test]
async fn a_write_to_an_extent_just_moved_survives_a_cut_before_the_persist_in_format_v2() {
    write_after_move(stormblock::drive::slab::SLAB_VERSION_2).await;
}

async fn write_after_move(version: u32) {
    let fmt = |dev: &Arc<CrashDevice>| {
        SlabFormat::new(SLOT, StorageTier::Hot)
            .with_role(SlabRole::Data)
            .with_version(version)
            .with_auto_metadata(dev.capacity_bytes())
    };
    let src_dev = Arc::new(CrashDevice::new(32 * 1024 * 1024));
    let dst_dev = Arc::new(CrashDevice::new(32 * 1024 * 1024));
    let src = Slab::format_with(src_dev.clone() as Arc<dyn BlockDevice>, fmt(&src_dev)).await.unwrap();
    let src_id = src.slab_id();
    let mut vm = VolumeManager::new(SLOT);
    vm.add_slab(src).await;
    vm.persist_to_slab(src_id);

    // A volume wholly on the source, recorded.
    let v = vm.create_volume_any("sys", VOL).await.unwrap();
    let h = vm.get_volume(&v).unwrap();
    for i in 0..BLOCKS {
        h.write(i * BLOCK, &block(i, blank_value(i))).await.unwrap();
    }
    h.flush().await.unwrap();
    vm.persist().await;

    // The destination arrives (the local disk of an install).
    let dst = Slab::format_with(dst_dev.clone() as Arc<dyn BlockDevice>, fmt(&dst_dev)).await.unwrap();
    let dst_id = dst.slab_id();
    vm.add_slab(dst).await;
    vm.persist_to_slabs(vec![dst_id, src_id]);
    // The new slab's own record, before anything moves (as the install's
    // first persist would write it).
    vm.persist().await;

    // Extents 0 and 1 move, the way the flow-over moves them.
    let engine = stormblock::placement::PlacementEngine::new();
    for e in 0..2u64 {
        let leg = vm.gem().read().await.lookup(v, e).unwrap().primary();
        assert_eq!(leg.slab_id, src_id);
        let fence = stormblock::volume::fence::exclusive(leg).await;
        engine.migrate_leg_unlocked(vm.gem(), vm.registry(), v, e, leg, dst_id, &fence).await.unwrap();
    }
    // A write to extent 0, in place on the destination, and its fsync. No
    // persist: the records still name the source slots.
    h.write(0, &block(0, 77)).await.unwrap();
    h.flush().await.unwrap();

    // The power goes on both drives, nothing unflushed kept.
    let src_after = Arc::new(src_dev.crash(1, 0.0));
    let dst_after = Arc::new(dst_dev.crash(2, 0.0));
    let mut vm2 = VolumeManager::new(SLOT);
    for d in [&dst_after, &src_after] {
        vm2.add_slab(Slab::open(d.clone() as Arc<dyn BlockDevice>).await.unwrap()).await;
    }
    vm2.persist_to_slabs(vec![dst_id, src_id]);
    vm2.restore().await.unwrap();

    assert_eq!(read_block(&vm2, "sys", 0).await, Ok(77), "the fsync'd write to the moved extent");
    for i in 1..BLOCKS {
        assert_eq!(read_block(&vm2, "sys", i).await, Ok(blank_value(i)), "block {i}");
    }
    // The moved slot holds extent 0.
    let id = vm2.find_volume("sys").await.unwrap();
    assert_eq!(vm2.gem().read().await.lookup(id, 0).unwrap().primary().slab_id, dst_id);
}
