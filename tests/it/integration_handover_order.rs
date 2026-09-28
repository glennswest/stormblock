//! A handover reads the slabs only after the incumbent is gone (#171).
//!
//! `adopt-ublk` used to restore from the slabs and only then stand the
//! incumbent down. Everything the incumbent allocated in between was missing
//! from the successor's map, and its slots looked free to the successor, to be
//! handed out again — nothing showed until a restore after a power cut. The
//! order now lives in `handover::take_over`; these tests run an incumbent that
//! allocates inside the window and check what the successor sees.

use std::sync::Arc;

use stormblock::drive::filedev::FileDevice;
use stormblock::drive::handover::take_over;
use stormblock::drive::slab::{Slab, SlabFormat, SlabRole};
use stormblock::drive::BlockDevice;
use stormblock::placement::topology::StorageTier;
use stormblock::volume::VolumeManager;
use tempfile::TempDir;

const SLOT: u64 = 64 * 1024;
const VOL: u64 = 4 * 1024 * 1024;

fn pattern(seed: u8) -> Vec<u8> {
    (0..SLOT as usize).map(|i| seed.wrapping_add((i / 512) as u8)).collect()
}

async fn open_slab(path: &str) -> Slab {
    let dev = Arc::new(FileDevice::open(path).await.unwrap()) as Arc<dyn BlockDevice>;
    Slab::open(dev).await.unwrap()
}

/// An engine over the slab file, restored from it — what a successor builds.
async fn successor(path: &str) -> VolumeManager {
    let slab = open_slab(path).await;
    let sid = slab.slab_id();
    let mut vm = VolumeManager::new(SLOT);
    vm.add_slab(slab).await;
    vm.persist_to_slab(sid);
    vm.restore().await.unwrap();
    vm
}

/// The incumbent: a slab file with a metadata region, a volume written and
/// recorded — the state a node has when its successor starts.
async fn incumbent(dir: &TempDir) -> (String, VolumeManager) {
    let path = dir.path().join("slab.img").display().to_string();
    std::fs::File::create(&path).unwrap().set_len(32 * 1024 * 1024).unwrap();
    let dev = Arc::new(FileDevice::open(&path).await.unwrap()) as Arc<dyn BlockDevice>;
    let fmt = SlabFormat::new(SLOT, StorageTier::Hot)
        .with_role(SlabRole::Data)
        .with_auto_metadata(dev.capacity_bytes());
    let slab = Slab::format_with(dev, fmt).await.unwrap();
    let sid = slab.slab_id();
    let mut vm = VolumeManager::new(SLOT);
    vm.add_slab(slab).await;
    vm.persist_to_slab(sid);
    let id = vm.create_volume_any("root", VOL).await.unwrap();
    let v = vm.get_volume(&id).unwrap();
    v.write(0, &pattern(1)).await.unwrap();
    v.flush().await.unwrap();
    vm.persist().await;
    (path, vm)
}

/// What the incumbent does inside the window: a write that allocates a new
/// extent, acknowledged by a flush — and then it exits (no persist: what is
/// flushed must be enough).
async fn allocate_in_window(vm: VolumeManager) {
    let id = vm.find_volume("root").await.unwrap();
    let v = vm.get_volume(&id).unwrap();
    v.write(5 * SLOT, &pattern(5)).await.unwrap();
    v.flush().await.unwrap();
    drop(v);
    drop(vm);
}

async fn read_extent(vm: &VolumeManager, name: &str, ext: u64) -> Vec<u8> {
    let id = vm.find_volume(name).await.unwrap();
    let v = vm.get_volume(&id).unwrap();
    let mut b = vec![0u8; SLOT as usize];
    v.read(ext * SLOT, &mut b).await.unwrap();
    b
}

#[tokio::test]
async fn the_successor_maps_what_the_incumbent_allocated_in_the_window() {
    let dir = TempDir::new().unwrap();
    let (path, inc) = incumbent(&dir).await;

    let mut vm = take_over(
        || async {
            allocate_in_window(inc).await;
            Ok(())
        },
        || async { Ok(successor(&path).await) },
    )
    .await
    .unwrap();

    assert_eq!(read_extent(&vm, "root", 0).await, pattern(1), "what was there before the window");
    assert_eq!(read_extent(&vm, "root", 5).await, pattern(5), "what the incumbent allocated in the window");

    // The slot is the incumbent's extent, not free space: new allocations in
    // the successor go elsewhere and leave it alone.
    let other = vm.create_volume_any("other", VOL).await.unwrap();
    let o = vm.get_volume(&other).unwrap();
    for e in 0..(VOL / SLOT) {
        o.write(e * SLOT, &pattern(0xEE)).await.unwrap();
    }
    o.flush().await.unwrap();
    assert_eq!(read_extent(&vm, "root", 5).await, pattern(5), "the successor handed the slot out again");
}

/// The old order, for the record: a successor that restores before the
/// incumbent is done does not know about the window's allocation.
#[tokio::test]
async fn restoring_before_the_incumbent_is_gone_misses_the_window() {
    let dir = TempDir::new().unwrap();
    let (path, inc) = incumbent(&dir).await;
    let early = successor(&path).await;
    allocate_in_window(inc).await;
    assert_ne!(
        read_extent(&early, "root", 5).await,
        pattern(5),
        "a successor that read the slabs first cannot know what came after"
    );
}
