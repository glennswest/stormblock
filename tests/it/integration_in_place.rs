//! A request planned in place (#505, step 2): what a zero-copy ublk device
//! sends straight to the drive, and everything else it leaves to the ordinary
//! path. The plan is made on a queue's thread, so it never waits; these drive
//! it on a device that models a drive's cache (no file descriptor: the queue's
//! READ_FIXED/WRITE_FIXED is `ci-ublk-qd-verify.sh`, on a real kernel).

use std::sync::Arc;
use std::time::Duration;

use stormblock::drive::crashdev::CrashDevice;
use stormblock::drive::slab::{Slab, SlabFormat, SlabRole};
use stormblock::drive::BlockDevice;
use stormblock::placement::topology::StorageTier;
use stormblock::volume::fence;
use stormblock::volume::redundancy::RedundancyPolicy;
use stormblock::volume::thin::ThinVolumeHandle;
use stormblock::volume::{VolumeId, VolumeManager};

const SLOT: u64 = 64 * 1024;
const BLOCK: usize = 4096;
const VOL: u64 = 2 * 1024 * 1024;

fn fmt(dev: &Arc<CrashDevice>) -> SlabFormat {
    SlabFormat::new(SLOT, StorageTier::Hot)
        .with_role(SlabRole::Data)
        .with_version(2)
        .with_auto_metadata(dev.capacity_bytes())
}

async fn node() -> (VolumeManager, Arc<CrashDevice>) {
    let dev = Arc::new(CrashDevice::new(32 * 1024 * 1024));
    let slab = Slab::format_with(dev.clone() as Arc<dyn BlockDevice>, fmt(&dev)).await.unwrap();
    let sid = slab.slab_id();
    let mut vm = VolumeManager::new(SLOT);
    vm.add_slab(slab).await;
    vm.persist_to_slab(sid);
    (vm, dev)
}

/// A volume whose first extents are written and flushed: owned, published.
async fn written(vm: &mut VolumeManager, name: &str) -> (VolumeId, Arc<ThinVolumeHandle>) {
    let id = vm.create_volume_any(name, VOL).await.unwrap();
    let h = vm.get_volume_handle(&id).unwrap();
    h.write(0, &vec![1u8; 2 * SLOT as usize]).await.unwrap();
    h.flush().await.unwrap();
    vm.persist().await;
    (id, h)
}

#[tokio::test]
async fn only_a_plain_request_on_a_published_owned_slot_is_planned() {
    let (mut vm, _dev) = node().await;
    let (id, h) = written(&mut vm, "v").await;
    let plan = |write, at, len| h.plan_in_place_on(write, at, len, false).is_some();

    assert!(plan(true, 0, BLOCK), "a write on a slot it owns");
    assert!(plan(false, 0, BLOCK), "a read");
    assert!(plan(true, SLOT - BLOCK as u64, BLOCK), "the last block of an extent");
    assert!(!plan(true, SLOT - BLOCK as u64, 2 * BLOCK), "across two extents");
    assert!(!plan(true, 512, BLOCK), "not whole blocks");
    assert!(!plan(true, 0, 1024), "not whole blocks");
    assert!(!plan(true, 0, 0), "nothing");
    assert!(!plan(false, 4 * SLOT, BLOCK), "a read of a hole answers zeros by the ordinary path");
    assert!(!plan(true, 4 * SLOT, BLOCK), "a first write takes a slot");

    // A slot just taken: not on the device until a flush publishes it (#171).
    h.write(4 * SLOT, &vec![2u8; BLOCK]).await.unwrap();
    assert!(!plan(true, 4 * SLOT, BLOCK), "its entry is not on the device yet");
    assert!(plan(false, 4 * SLOT, BLOCK), "a read of it is only a read");
    h.flush().await.unwrap();
    assert!(plan(true, 4 * SLOT, BLOCK), "published by the flush");

    // A clone shares every slot: a read may go in place, a write must copy.
    let clone = vm.create_snapshot(id, "clone").await.unwrap();
    let c = vm.get_volume_handle(&clone).unwrap();
    assert!(c.plan_in_place_on(false, 0, BLOCK, false).is_some(), "a read of a shared slot");
    assert!(c.plan_in_place_on(true, 0, BLOCK, false).is_none(), "a write to a shared slot");
    assert!(!plan(true, 0, BLOCK), "and the source shares it too");
    c.write(0, &vec![3u8; BLOCK]).await.unwrap();
    c.flush().await.unwrap();
    assert!(c.plan_in_place_on(true, 0, BLOCK, false).is_some(), "its own once copied");
    assert!(c.plan_in_place_on(true, SLOT, BLOCK, false).is_none(), "the next extent is still shared");

    // A slab being emptied: a write goes elsewhere (#239). A read is a read.
    let sid = vm.gem().read().await.lookup(clone, 0).unwrap().primary().slab_id;
    vm.registry().write().await.set_quarantined(sid, true);
    assert!(c.plan_in_place_on(true, 0, BLOCK, false).is_none());
    assert!(c.plan_in_place_on(false, 0, BLOCK, false).is_some());
    vm.registry().write().await.set_quarantined(sid, false);

    // Mirrors and parity have more than one thing to do.
    let w = vm.get_volume_handle(&id).unwrap();
    w.force_redundancy(RedundancyPolicy::mirror(2));
    assert!(w.plan_in_place_on(false, 4 * SLOT, BLOCK, false).is_none());
    assert!(w.plan_in_place_on(true, 4 * SLOT, BLOCK, false).is_none());
    w.force_redundancy(RedundancyPolicy::none());

    // Sealed and read-only refuse writes, by the ordinary path's own words.
    w.set_sealed(true);
    assert!(w.plan_in_place_on(true, 4 * SLOT, BLOCK, false).is_none());
    assert!(w.plan_in_place_on(false, 4 * SLOT, BLOCK, false).is_some());
    w.set_sealed(false);
    w.refuse("an unknown scheme".into());
    assert!(w.plan_in_place_on(false, 4 * SLOT, BLOCK, false).is_none());

    // The bytes land where the plan says.
    let plan = c.plan_in_place_on(true, BLOCK as u64, BLOCK, false).unwrap();
    plan.device.write(plan.offset, &vec![9u8; BLOCK]).await.unwrap();
    drop(plan);
    let mut back = vec![0u8; 2 * BLOCK];
    c.read(0, &mut back).await.unwrap();
    assert!(back[..BLOCK].iter().all(|&b| b == 3), "the block written by the ordinary path");
    assert!(back[BLOCK..].iter().all(|&b| b == 9), "the block written in place");
}

#[tokio::test]
async fn a_device_with_no_file_descriptor_plans_nothing_for_the_queue() {
    let (mut vm, _dev) = node().await;
    let (_, h) = written(&mut vm, "v").await;
    assert!(h.plans_in_place());
    assert!(h.plan_in_place(true, 0, BLOCK).is_none(), "CrashDevice has no fd to go straight to");
    assert!(h.plan_in_place_on(true, 0, BLOCK, false).is_some());
}

/// A plan holds what `write` holds: a move, a snapshot and a mirror's start
/// all wait for it, and it never waits for them: it steps aside.
#[tokio::test]
async fn a_plan_holds_off_a_move_and_a_snapshot_and_never_waits_for_one() {
    let (mut vm, _dev) = node().await;
    let (id, h) = written(&mut vm, "v").await;
    let leg = vm.gem().read().await.lookup(id, 0).unwrap().primary();

    let plan = h.plan_in_place_on(true, 0, BLOCK, false).unwrap();
    assert!(fence::try_exclusive(leg).is_none(), "the slot is fenced while the I/O runs");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), h.hold_writes()).await.is_err(),
        "the snapshot's gate waits for it"
    );
    drop(plan);
    let gate = h.hold_writes().await;
    assert!(h.plan_in_place_on(true, 0, BLOCK, false).is_none(), "a snapshot holds the gate: not planned, not waited for");
    assert!(h.plan_in_place_on(false, 0, BLOCK, false).is_some(), "a read does not need the gate");
    drop(gate);

    let moving = fence::try_exclusive(leg).unwrap();
    assert!(h.plan_in_place_on(true, 0, BLOCK, false).is_none(), "a move has the slot");
    assert!(h.plan_in_place_on(false, 0, BLOCK, false).is_none(), "for reads too");
    drop(moving);
    assert!(h.plan_in_place_on(true, 0, BLOCK, false).is_some());
}

/// A write in place is one a FLUSH owes (#338): a flush that came after it is a
/// real one, however clean the volume looked. With nothing kept of what was not
/// flushed, the write is there only if the flush went to the drive.
#[tokio::test]
async fn a_flush_after_a_write_in_place_reaches_the_drive() {
    let (mut vm, dev) = node().await;
    let (_, h) = written(&mut vm, "v").await;
    // Nothing owed: this one is skipped, and the next must not be.
    h.flush().await.unwrap();

    let plan = h.plan_in_place_on(true, 0, BLOCK, false).unwrap();
    plan.device.write(plan.offset, &vec![7u8; BLOCK]).await.unwrap();
    drop(plan);
    h.flush().await.unwrap();

    let after = Arc::new(dev.crash(1, 0.0));
    let mut vm2 = VolumeManager::new(SLOT);
    let slab = Slab::open(after.clone() as Arc<dyn BlockDevice>).await.unwrap();
    let sid = slab.slab_id();
    vm2.add_slab(slab).await;
    vm2.persist_to_slab(sid);
    vm2.restore().await.unwrap();
    let id = vm2.find_volume("v").await.unwrap();
    let mut back = vec![0u8; BLOCK];
    vm2.get_volume_handle(&id).unwrap().read(0, &mut back).await.unwrap();
    assert!(back.iter().all(|&b| b == 7), "the flushed write in place was lost");
}

/// Writes in place while every extent is moved over and over (#239's shape):
/// each slot is fenced for its I/O, the map checked again under the fence, so
/// nothing lands on a slot already copied from. Each writer reads its own write
/// back at once, and its last value is what the block reads afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writes_in_place_are_not_lost_to_a_move_running_under_them() {
    let (mut vm, _dev) = node().await;
    let (id, h) = written(&mut vm, "v").await;
    let other = Arc::new(CrashDevice::new(32 * 1024 * 1024));
    let slab = Slab::format_with(other.clone() as Arc<dyn BlockDevice>, fmt(&other)).await.unwrap();
    let (a, b) = {
        let leg = vm.gem().read().await.lookup(id, 0).unwrap().primary();
        (leg.slab_id, slab.slab_id())
    };
    vm.add_slab(slab).await;

    const EXTENTS: u64 = 2;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut writers = Vec::new();
    for e in 0..EXTENTS {
        let (h, stop) = (h.clone(), stop.clone());
        writers.push(tokio::spawn(async move {
            let (mut n, mut planned) = (0u64, 0u64);
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                n += 1;
                let data = vec![(n % 251) as u8 + 1; BLOCK];
                let at = e * SLOT;
                match h.plan_in_place_on(true, at, BLOCK, false) {
                    Some(plan) => {
                        planned += 1;
                        // As long as a real drive's write takes: a move gets
                        // its chance between the plan and the bytes landing.
                        tokio::time::sleep(Duration::from_micros(500)).await;
                        plan.device.write(plan.offset, &data).await.unwrap();
                        drop(plan);
                    }
                    None => {
                        h.write(at, &data).await.unwrap();
                    }
                }
                let mut back = vec![0u8; BLOCK];
                h.read(at, &mut back).await.unwrap();
                assert!(back == data, "extent {e}: write {n} is gone, a move ran under it");
            }
            (e, n, planned)
        }));
    }

    let engine = stormblock::placement::PlacementEngine::new();
    let mut moved = 0;
    for round in 0..200 {
        let dest = if round % 2 == 0 { b } else { a };
        for e in 0..EXTENTS {
            let leg = vm.gem().read().await.lookup(id, e).unwrap().primary();
            if leg.slab_id == dest {
                continue;
            }
            let fence = fence::exclusive(leg).await;
            if engine.migrate_leg_unlocked(vm.gem(), vm.registry(), id, e, leg, dest, &fence).await.is_ok() {
                moved += 1;
            }
        }
        // The moved slots' entries published, so they can be planned again.
        h.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);

    let mut total_planned = 0;
    for w in writers {
        let (e, n, planned) = w.await.unwrap();
        total_planned += planned;
        let mut back = vec![0u8; BLOCK];
        h.read(e * SLOT, &mut back).await.unwrap();
        let want = (n % 251) as u8 + 1;
        assert!(back.iter().all(|&x| x == want), "extent {e}: wrote {want} last, reads {}", back[0]);
    }
    assert!(moved > 50, "too few moves to test anything: {moved}");
    assert!(total_planned > 50, "too few writes in place to test anything: {total_planned}");
}
