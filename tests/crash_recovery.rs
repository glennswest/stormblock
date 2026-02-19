//! Crash recovery tests — journal persistence, extent allocator consistency,
//! RAID superblock validation.

mod common;

use std::sync::Arc;

use tempfile::TempDir;

use stormblock::drive::BlockDevice;
use stormblock::drive::filedev::FileDevice;
use stormblock::raid::journal::WriteIntentJournal;
use stormblock::raid::{RaidArray, RaidLevel, RaidSuperblock, RaidArrayId};
use stormblock::volume::extent::{ExtentAllocator, Extent};

#[test]
fn journal_persist_and_recovery() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("journal.bin");

    // Simulate a crash: mark stripes dirty, flush, then "crash" (drop without clean)
    {
        let mut journal = WriteIntentJournal::open(&path, 1024).unwrap();
        journal.mark_dirty(0);
        journal.mark_dirty(100);
        journal.mark_dirty(500);
        journal.mark_dirty(999);
        // Clean one to verify partial state
        journal.mark_clean(100);
        journal.flush().unwrap();
        // "Crash" — drop without clearing
    }

    // Recovery: reopen and verify dirty stripes
    {
        let journal = WriteIntentJournal::open(&path, 1024).unwrap();
        assert_eq!(journal.dirty_count(), 3);
        assert!(journal.is_dirty(0));
        assert!(!journal.is_dirty(100)); // was cleaned before crash
        assert!(journal.is_dirty(500));
        assert!(journal.is_dirty(999));

        let dirty = journal.dirty_stripes();
        assert_eq!(dirty, vec![0, 500, 999]);
    }
}

#[test]
fn journal_large_bitmap() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("large-journal.bin");

    let stripe_count = 1_000_000;
    {
        let mut journal = WriteIntentJournal::open(&path, stripe_count).unwrap();
        // Mark every 1000th stripe dirty
        for i in (0..stripe_count).step_by(1000) {
            journal.mark_dirty(i);
        }
        assert_eq!(journal.dirty_count(), 1000);
        journal.flush().unwrap();
    }

    {
        let journal = WriteIntentJournal::open(&path, stripe_count).unwrap();
        assert_eq!(journal.dirty_count(), 1000);
        // Verify sampling
        assert!(journal.is_dirty(0));
        assert!(journal.is_dirty(1000));
        assert!(!journal.is_dirty(1));
        assert!(!journal.is_dirty(999));
    }
}

#[test]
fn journal_clear_all_recovery() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("clear-journal.bin");

    {
        let mut journal = WriteIntentJournal::open(&path, 100).unwrap();
        journal.mark_dirty(10);
        journal.mark_dirty(50);
        journal.clear_all().unwrap();
    }

    {
        let journal = WriteIntentJournal::open(&path, 100).unwrap();
        assert_eq!(journal.dirty_count(), 0);
        assert!(journal.dirty_stripes().is_empty());
    }
}

#[test]
fn superblock_roundtrip_validation() {
    let uuid = uuid::Uuid::new_v4();
    let member_uuid = uuid::Uuid::new_v4();
    let sb = RaidSuperblock::new(
        uuid, 0, member_uuid,
        RaidLevel::Raid5, 4, 65536,
        100 * 1024 * 1024,
    );

    let bytes = sb.to_bytes();
    assert_eq!(bytes.len(), 4096);

    // Valid roundtrip
    let sb2 = RaidSuperblock::from_bytes(&bytes).unwrap();
    sb2.validate().unwrap();
    assert_eq!(sb2.array_uuid, *uuid.as_bytes());
    assert_eq!(sb2.member_index, 0);
    assert_eq!(sb2.level, 5);
    assert_eq!(sb2.member_count, 4);
    assert_eq!(sb2.stripe_size, 65536);
}

#[test]
fn superblock_corruption_detected() {
    let uuid = uuid::Uuid::new_v4();
    let sb = RaidSuperblock::new(
        uuid, 0, uuid::Uuid::new_v4(),
        RaidLevel::Raid1, 2, 65536, 50 * 1024 * 1024,
    );
    let mut bytes = sb.to_bytes();

    // Corrupt a byte in the middle
    bytes[30] ^= 0xFF;
    assert!(RaidSuperblock::from_bytes(&bytes).is_err());
}

#[test]
fn superblock_bad_magic() {
    let mut bytes = vec![0u8; 4096];
    bytes[0..8].copy_from_slice(b"NOTMAGIC");
    assert!(RaidSuperblock::from_bytes(&bytes).is_err());
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

    // Keep references for raw reads
    let dev0_path = dir.path().join("dev-0.bin");
    let dev1_path = dir.path().join("dev-1.bin");

    let array = RaidArray::create(RaidLevel::Raid1, devices, None)
        .await
        .unwrap();
    let _array_id = array.array_id();

    // Read raw superblocks from both members
    let dev0 = FileDevice::open(dev0_path.to_str().unwrap()).await.unwrap();
    let mut sb_buf = vec![0u8; 4096];
    dev0.read(0, &mut sb_buf).await.unwrap();
    let sb0 = RaidSuperblock::from_bytes(&sb_buf).unwrap();
    sb0.validate().unwrap();
    assert_eq!(sb0.member_index, 0);
    assert_eq!(sb0.level, 1); // RAID-1

    let dev1 = FileDevice::open(dev1_path.to_str().unwrap()).await.unwrap();
    dev1.read(0, &mut sb_buf).await.unwrap();
    let sb1 = RaidSuperblock::from_bytes(&sb_buf).unwrap();
    sb1.validate().unwrap();
    assert_eq!(sb1.member_index, 1);

    // Both should have the same array UUID
    assert_eq!(sb0.array_uuid, sb1.array_uuid);
}
