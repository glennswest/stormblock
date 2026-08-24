//! Volume lifecycle integration tests — create, snapshot, delete, read/write.

mod common;

use std::sync::Arc;

use tempfile::TempDir;

use stormblock::drive::BlockDevice;
use stormblock::raid::{RaidArray, RaidLevel};
use stormblock::volume::{VolumeManager, DEFAULT_EXTENT_SIZE};

async fn setup_volume_manager(
    dir: &TempDir,
) -> (VolumeManager, stormblock::raid::RaidArrayId) {
    let devices = common::create_file_devices(dir, 2, 64 * 1024 * 1024).await;
    let array = RaidArray::create(RaidLevel::Raid1, devices, None)
        .await
        .expect("RAID-1 create");
    let array_id = array.array_id();
    let backing: Arc<dyn BlockDevice> = Arc::new(array);

    let mut vm = VolumeManager::new(DEFAULT_EXTENT_SIZE);
    vm.add_backing_device(array_id, backing).await;
    (vm, array_id)
}

#[tokio::test]
async fn volume_create_write_read() {
    let dir = TempDir::new().unwrap();
    let (mut vm, array_id) = setup_volume_manager(&dir).await;

    let vol_id = vm.create_volume("test-vol", 32 * 1024 * 1024, array_id).await.unwrap();
    let vol = vm.get_volume(&vol_id).unwrap();

    let data = vec![0xAB_u8; 4096];
    vol.write(0, &data).await.unwrap();

    let mut buf = vec![0u8; 4096];
    vol.read(0, &mut buf).await.unwrap();
    assert_eq!(buf, data);
}

#[tokio::test]
async fn volume_snapshot_cow() {
    let dir = TempDir::new().unwrap();
    let (mut vm, array_id) = setup_volume_manager(&dir).await;

    let vol_id = vm.create_volume("data", 32 * 1024 * 1024, array_id).await.unwrap();
    let vol = vm.get_volume(&vol_id).unwrap();

    // Write initial data
    vol.write(0, &vec![0xAA_u8; 4096]).await.unwrap();

    // Create snapshot
    let snap_id = vm.create_snapshot(vol_id, "snap1").await.unwrap();

    // Write new data to source
    vol.write(0, &vec![0xBB_u8; 4096]).await.unwrap();

    // Source has new data
    let mut src_buf = vec![0u8; 4096];
    vol.read(0, &mut src_buf).await.unwrap();
    assert!(src_buf.iter().all(|&b| b == 0xBB), "source should have new data");

    // Snapshot has old data (COW)
    let snap = vm.get_volume(&snap_id).unwrap();
    let mut snap_buf = vec![0u8; 4096];
    snap.read(0, &mut snap_buf).await.unwrap();
    assert!(snap_buf.iter().all(|&b| b == 0xAA), "snapshot should have original data");
}

#[tokio::test]
async fn volume_delete_frees_extents() {
    let dir = TempDir::new().unwrap();
    let (mut vm, array_id) = setup_volume_manager(&dir).await;

    let vol_id = vm.create_volume("to-delete", 16 * 1024 * 1024, array_id).await.unwrap();
    let vol = vm.get_volume(&vol_id).unwrap();
    vol.write(0, &vec![0xFF_u8; 4096]).await.unwrap();
    drop(vol);

    vm.delete_volume(vol_id).await.unwrap();
    assert!(vm.get_volume(&vol_id).is_none());

    // Should be able to create a new volume with freed space
    let new_vol_id = vm.create_volume("new-vol", 16 * 1024 * 1024, array_id).await.unwrap();
    let new_vol = vm.get_volume(&new_vol_id).unwrap();
    new_vol.write(0, &vec![0x11_u8; 4096]).await.unwrap();

    let mut buf = vec![0u8; 4096];
    new_vol.read(0, &mut buf).await.unwrap();
    assert!(buf.iter().all(|&b| b == 0x11));
}

#[tokio::test]
async fn volume_list() {
    let dir = TempDir::new().unwrap();
    let (mut vm, array_id) = setup_volume_manager(&dir).await;

    vm.create_volume("vol-a", 10 * 1024 * 1024, array_id).await.unwrap();
    vm.create_volume("vol-b", 20 * 1024 * 1024, array_id).await.unwrap();

    let list = vm.list_volumes().await;
    assert_eq!(list.len(), 2);

    let names: Vec<&str> = list.iter().map(|(_, name, _, _)| name.as_str()).collect();
    assert!(names.contains(&"vol-a"));
    assert!(names.contains(&"vol-b"));
}

#[tokio::test]
async fn volume_multiple_extent_writes() {
    let dir = TempDir::new().unwrap();
    let (mut vm, array_id) = setup_volume_manager(&dir).await;

    // Use small extent size to trigger multiple extent allocations
    let vol_id = vm.create_volume("multi", 32 * 1024 * 1024, array_id).await.unwrap();
    let vol = vm.get_volume(&vol_id).unwrap();

    // Write at different offsets spanning multiple extents
    let offsets = [0u64, DEFAULT_EXTENT_SIZE, DEFAULT_EXTENT_SIZE * 2];
    for (i, &offset) in offsets.iter().enumerate() {
        let data = vec![0x10 + i as u8; 4096];
        vol.write(offset, &data).await.unwrap();
    }

    // Read back each
    for (i, &offset) in offsets.iter().enumerate() {
        let mut buf = vec![0u8; 4096];
        vol.read(offset, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == (0x10 + i as u8)),
            "offset {offset} should have byte {:#x}", 0x10 + i as u8);
    }
}

#[tokio::test]
async fn volume_resize_grow_and_shrink() {
    let dir = TempDir::new().unwrap();
    let (mut vm, array_id) = setup_volume_manager(&dir).await;

    let vol_id = vm.create_volume("resize-test", 32 * 1024 * 1024, array_id).await.unwrap();
    let vol = vm.get_volume(&vol_id).unwrap();

    // Write 4 KB at offset 0
    let data = vec![0xDE_u8; 4096];
    vol.write(0, &data).await.unwrap();

    // Grow to 64 MB
    vm.resize_volume(vol_id, 64 * 1024 * 1024).await.unwrap();
    assert_eq!(vol.capacity_bytes(), 64 * 1024 * 1024);

    // Data at offset 0 still correct after grow
    let mut buf = vec![0u8; 4096];
    vol.read(0, &mut buf).await.unwrap();
    assert_eq!(buf, data);

    // Write beyond original 32 MB boundary
    let data_high = vec![0xEF_u8; 4096];
    vol.write(40 * 1024 * 1024, &data_high).await.unwrap();

    let mut buf2 = vec![0u8; 4096];
    vol.read(40 * 1024 * 1024, &mut buf2).await.unwrap();
    assert_eq!(buf2, data_high);

    // Shrinking is not something resize does — it frees the extents past the
    // new end, and a filesystem above cannot follow (#19).
    let refused = vm.resize_volume(vol_id, 32 * 1024 * 1024).await.unwrap_err();
    assert!(
        matches!(refused, stormblock::volume::VolumeError::ShrinkRefused { .. }),
        "{refused}"
    );
    assert_eq!(vol.capacity_bytes(), 64 * 1024 * 1024, "the refusal changed nothing");
    let mut still_there = vec![0u8; 4096];
    vol.read(40 * 1024 * 1024, &mut still_there).await.unwrap();
    assert_eq!(still_there, data_high, "the extent past 32 MB survived the refusal");

    // Naming the shrink is what performs it — data at offset 0 still intact.
    vm.shrink_volume(vol_id, 32 * 1024 * 1024).await.unwrap();
    assert_eq!(vol.capacity_bytes(), 32 * 1024 * 1024);

    let mut buf3 = vec![0u8; 4096];
    vol.read(0, &mut buf3).await.unwrap();
    assert_eq!(buf3, data);
}

/// A copy-on-write slot larger than one `tokio::fs::File` transfer.
///
/// `tokio::fs::File` moves at most 2 MiB per read or write and reports the
/// short count. `FileDevice` used to pass that count up as though the
/// transfer were complete, so copying a 4 MiB slot for copy-on-write copied
/// half of it and the clone read zeros for the rest. Every slot here is 1 MiB
/// in the other tests, which is why nothing caught it: the engine sizes slots
/// by device, and a real deployment gets 4 MiB.
#[tokio::test]
async fn cow_preserves_a_slot_larger_than_one_file_transfer() {
    const SLOT: u64 = 4 * 1024 * 1024;

    let dir = TempDir::new().unwrap();
    let devices = common::create_file_devices(&dir, 2, 256 * 1024 * 1024).await;
    let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.expect("RAID-1 create");
    let array_id = array.array_id();
    let backing: Arc<dyn BlockDevice> = Arc::new(array);

    let mut vm = VolumeManager::new(SLOT);
    vm.add_backing_device(array_id, backing).await;

    let vol_id = vm.create_volume("wide", 32 * 1024 * 1024, array_id).await.unwrap();
    let vol = vm.get_volume(&vol_id).unwrap();

    // Fill one whole slot with a position-dependent pattern, so a byte that
    // moved shows as clearly as a byte that vanished.
    let filled: Vec<u8> = (0..SLOT).map(|i| (i % 251) as u8).collect();
    vol.write(0, &filled).await.unwrap();
    vol.flush().await.unwrap();

    // The snapshot is what makes the slot shared, so the next write copies.
    let _snap = vm.create_snapshot(vol_id, "before").await.unwrap();

    // One 4 KiB write at the front triggers the copy of the whole slot.
    vol.write(0, &vec![0xEE_u8; 4096]).await.unwrap();
    vol.flush().await.unwrap();

    let mut back = vec![0u8; SLOT as usize];
    vol.read(0, &mut back).await.unwrap();

    assert_eq!(&back[..4096], &vec![0xEE_u8; 4096][..], "the write itself did not land");
    let tail_start = 4096usize;
    assert_eq!(
        &back[tail_start..],
        &filled[tail_start..],
        "copy-on-write lost the part of the slot the write did not cover",
    );
}

/// A copy-on-write clone is an audit surface, not only a cheap copy.
///
/// It shares every extent with its golden until something writes, and a write
/// replaces the shared extent with one the clone owns alone. So the extents a
/// clone does not share *are* what has been written to it — and a clone that
/// shares all of them is provably untouched, without reading a byte.
///
/// That is what makes "this system root should never have been written"
/// checkable rather than hopeful.
#[tokio::test]
async fn a_clone_can_prove_it_has_not_been_written() {
    use stormblock::drive::filedev::FileDevice;
    use stormblock::raid::RaidArrayId;
    use stormblock::volume::VolumeManager;

    const SLOT: u64 = 64 * 1024;
    let dir = std::env::temp_dir().join("stormblock-divergence");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("d-{}.img", uuid::Uuid::new_v4().simple()));
    let p = path.to_str().unwrap().to_string();
    let dev = FileDevice::open_with_capacity(&p, 16 * 1024 * 1024).await.unwrap();

    let mut mgr = VolumeManager::new(SLOT);
    let array = RaidArrayId(uuid::Uuid::new_v4());
    mgr.add_backing_device(array, std::sync::Arc::new(dev)).await;

    // A golden with content, then a clone of it.
    let golden = mgr.create_volume("root.golden", 1024 * 1024, array).await.unwrap();
    let gv = mgr.get_volume(&golden).unwrap();
    gv.write(0, &vec![0xAB; SLOT as usize]).await.unwrap();
    gv.write(SLOT, &vec![0xCD; SLOT as usize]).await.unwrap();
    gv.flush().await.unwrap();

    let clone = mgr.create_snapshot(golden, "root").await.unwrap();

    // Untouched: shares everything, owns nothing.
    let d = mgr.divergence(clone, golden).await;
    assert!(d.pristine(), "a fresh clone must be provably untouched: {d:?}");
    assert_eq!(d.bytes, 0);
    assert_eq!(d.shared, 2, "both extents still shared with the golden");

    // One write, and it shows — the extent it touched and nothing else.
    let cv = mgr.get_volume(&clone).unwrap();
    cv.write(SLOT, &vec![0x11; SLOT as usize]).await.unwrap();
    cv.flush().await.unwrap();

    let d = mgr.divergence(clone, golden).await;
    assert!(!d.pristine(), "a written clone is not pristine");
    assert_eq!(d.extents, vec![1], "only the extent that was written");
    assert_eq!(d.bytes, SLOT);
    assert_eq!(d.shared, 1, "the untouched extent is still shared");

    // And the golden is untouched by any of it — the whole point.
    let mut back = vec![0u8; SLOT as usize];
    gv.read(SLOT, &mut back).await.unwrap();
    assert!(back.iter().all(|&b| b == 0xCD), "the golden was written through");

    let _ = std::fs::remove_file(&p);
}
