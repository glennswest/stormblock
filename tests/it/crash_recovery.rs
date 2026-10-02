//! Crash recovery tests — extent allocator consistency, RAID superblocks and
//! the write-intent bitmap at reassembly.

use crate::common;
use std::sync::Arc;

use tempfile::TempDir;

use stormblock::drive::BlockDevice;
use stormblock::drive::filedev::FileDevice;
use stormblock::raid::parity::ParityEngine;
use stormblock::raid::{read_superblock, scan, RaidArray, RaidArrayId, RaidLevel, Superblock, DATA_OFFSET};
use stormblock::volume::extent::ExtentAllocator;

#[test]
fn superblock_roundtrip_validation() {
    let sb = Superblock {
        array_uuid: uuid::Uuid::new_v4(),
        member_uuid: uuid::Uuid::new_v4(),
        slot: Some(0),
        level: Some(RaidLevel::Raid5),
        stripe_size: 65536,
        data_offset: DATA_OFFSET,
        data_size: 1024 * 1024,
        create_time: 1,
        update_time: 1,
        events: 1,
        bitmap_offset: 65536,
        bitmap_bytes: 4096,
        bitmap_chunk: 64 << 20,
        name: "set".into(),
        pool: String::new(),
        slots: Vec::new(),
    };
    let bytes = sb.to_bytes();
    assert_eq!(bytes.len(), 4096);
    assert_eq!(Superblock::from_bytes(&bytes).unwrap().unwrap(), sb);

    let mut bad = bytes.clone();
    bad[50] ^= 0xFF;
    assert!(Superblock::from_bytes(&bad).is_err(), "corruption must be detected");

    let mut other = vec![0u8; 4096];
    other[0..8].copy_from_slice(b"BADMAGIC");
    assert!(Superblock::from_bytes(&other).unwrap().is_none(), "not a superblock at all");
}

#[test]
fn extent_allocator_consistency() {
    let array_id = RaidArrayId(uuid::Uuid::new_v4());
    let extent_size = 4096u64;

    let mut allocator = ExtentAllocator::new(extent_size);
    allocator.add_array(array_id, 1024 * 1024); // 1MB

    // Allocate some extents
    let extents1 = allocator.allocate(array_id, 5).unwrap();
    assert_eq!(extents1.len(), 5);

    // All extents should have correct size and array
    for ext in &extents1 {
        assert_eq!(ext.array_id, array_id);
        assert_eq!(ext.length, extent_size);
    }

    // Allocate more
    let extents2 = allocator.allocate(array_id, 3).unwrap();
    assert_eq!(extents2.len(), 3);

    // Free first batch
    for ext in &extents1 {
        allocator.free(ext);
    }

    // Re-allocate should succeed (freed space reused)
    let extents3 = allocator.allocate(array_id, 5).unwrap();
    assert_eq!(extents3.len(), 5);
}

#[test]
fn extent_allocator_exhaustion() {
    let array_id = RaidArrayId(uuid::Uuid::new_v4());
    let extent_size = 4096u64;

    let mut allocator = ExtentAllocator::new(extent_size);
    // Small capacity: only room for 4 extents
    allocator.add_array(array_id, 4 * extent_size);

    let extents = allocator.allocate(array_id, 4).unwrap();
    assert_eq!(extents.len(), 4);

    // Should fail — no more space
    let result = allocator.allocate(array_id, 1);
    assert!(result.is_none(), "allocation should fail when exhausted");

    // Free one and try again
    allocator.free(&extents[0]);
    let extents2 = allocator.allocate(array_id, 1).unwrap();
    assert_eq!(extents2.len(), 1);
}

#[tokio::test]
async fn raid_superblock_written_to_members() {
    let dir = TempDir::new().unwrap();
    let devices = common::create_file_devices(&dir, 2, 4 * 1024 * 1024).await;
    let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();

    let mut seen = Vec::new();
    for i in 0..2 {
        let dev: Arc<dyn BlockDevice> = Arc::new(
            FileDevice::open(dir.path().join(format!("dev-{i}.bin")).to_str().unwrap()).await.unwrap(),
        );
        let sb = read_superblock(&dev).await.unwrap().expect("a superblock");
        assert_eq!(sb.slot, Some(i));
        assert_eq!(sb.level, Some(RaidLevel::Raid1));
        assert_eq!(sb.slots.len(), 2);
        seen.push(sb.array_uuid);
    }
    assert_eq!(seen[0], array.array_id().0);
    assert_eq!(seen[0], seen[1]);
}

/// A torn stripe after a crash: the bitmap names its chunk, and putting the
/// array back together from its drives recomputes the parity.
#[tokio::test]
async fn assembly_repairs_parity_of_a_dirty_chunk() {
    let dir = TempDir::new().unwrap();
    let devices = common::create_file_devices(&dir, 4, 2 * 1024 * 1024).await;
    let raw: Vec<Arc<dyn BlockDevice>> = devices.iter().map(Arc::clone).collect();

    let array = RaidArray::create(RaidLevel::Raid5, devices, Some(4096)).await.unwrap();
    let full_stripe: Vec<u8> = (0..12288u32).map(|i| (i % 256) as u8).collect();
    array.write(0, &full_stripe).await.unwrap();
    array.close().await.unwrap();
    drop(array);

    // Stripe 0's parity is on the last member. Tear it and set the chunk's
    // bit, as a crash between the data write and the parity write leaves it.
    raw[3].write(DATA_OFFSET, &[0xFF_u8; 4096]).await.unwrap();
    let mut page = vec![0u8; 4096];
    page[0] = 1;
    for d in &raw {
        d.write(stormblock::raid::bitmap::BITMAP_OFFSET, &page).await.unwrap();
    }

    let found = scan(&raw).await;
    let array = RaidArray::assemble(found.arrays[0].clone()).await.unwrap();

    let engine = ParityEngine::detect();
    let mut strips: Vec<Vec<u8>> = Vec::new();
    for d in &raw {
        let mut buf = vec![0u8; 4096];
        d.read(DATA_OFFSET, &mut buf).await.unwrap();
        strips.push(buf);
    }
    let refs: Vec<&[u8]> = strips.iter().map(|s| s.as_slice()).collect();
    let mut check = vec![0u8; 4096];
    engine.compute_xor_parity(&refs, &mut check);
    assert!(check.iter().all(|&x| x == 0), "parity not repaired at assembly");
    assert_eq!(array.dirty_chunks(), 0);

    let mut readback = vec![0u8; 12288];
    array.read(0, &mut readback).await.unwrap();
    assert_eq!(readback, full_stripe);
}
