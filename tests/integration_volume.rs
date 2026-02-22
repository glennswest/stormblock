//! Volume lifecycle integration tests — create, snapshot, delete, read/write.

mod common;

use std::sync::Arc;

use tempfile::TempDir;

use stormblock::drive::BlockDevice;
use stormblock::drive::filedev::FileDevice;
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
        let data = vec![(0x10 + i as u8); 4096];
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

    // Shrink back to 32 MB — data at offset 0 still intact
    vm.resize_volume(vol_id, 32 * 1024 * 1024).await.unwrap();
    assert_eq!(vol.capacity_bytes(), 32 * 1024 * 1024);

    let mut buf3 = vec![0u8; 4096];
    vol.read(0, &mut buf3).await.unwrap();
    assert_eq!(buf3, data);
}
