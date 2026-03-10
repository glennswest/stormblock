//! RAID degraded-mode and rebuild integration tests.

mod common;


use tempfile::TempDir;

use stormblock::drive::BlockDevice;
use stormblock::raid::{RaidArray, RaidLevel, RaidMemberState};

#[tokio::test]
async fn raid1_degraded_read() {
    let dir = TempDir::new().unwrap();
    let devices = common::create_file_devices(&dir, 3, 4 * 1024 * 1024).await;
    let mut array = RaidArray::create(RaidLevel::Raid1, devices, None)
        .await
        .unwrap();

    // Write known data
    let data = vec![0xAA_u8; 4096];
    array.write(0, &data).await.unwrap();
    array.flush().await.unwrap();

    // Fail one member
    array.set_member_state(0, RaidMemberState::Failed);

    // Should still be able to read from surviving members
    let mut buf = vec![0u8; 4096];
    array.read(0, &mut buf).await.unwrap();
    assert_eq!(buf, data, "degraded read should return correct data");

    // Fail a second member — RAID 1 with 3 members should still work with 1 active
    array.set_member_state(1, RaidMemberState::Failed);
    let mut buf2 = vec![0u8; 4096];
    array.read(0, &mut buf2).await.unwrap();
    assert_eq!(buf2, data, "should work with only 1 active member");
}

#[tokio::test]
async fn raid1_degraded_write() {
    let dir = TempDir::new().unwrap();
    let devices = common::create_file_devices(&dir, 2, 4 * 1024 * 1024).await;
    let mut array = RaidArray::create(RaidLevel::Raid1, devices, None)
        .await
        .unwrap();

    // Fail one member
    array.set_member_state(1, RaidMemberState::Failed);

    // Write should succeed with degraded array
    let data = vec![0xBB_u8; 4096];
    array.write(0, &data).await.unwrap();

    // Read should work from the active member
    let mut buf = vec![0u8; 4096];
    array.read(0, &mut buf).await.unwrap();
    assert_eq!(buf, data);
}

#[tokio::test]
async fn raid1_all_failed() {
    let dir = TempDir::new().unwrap();
    let devices = common::create_file_devices(&dir, 2, 4 * 1024 * 1024).await;
    let mut array = RaidArray::create(RaidLevel::Raid1, devices, None)
        .await
        .unwrap();

    // Fail all members
    array.set_member_state(0, RaidMemberState::Failed);
    array.set_member_state(1, RaidMemberState::Failed);

    // Read should fail
    let mut buf = vec![0u8; 4096];
    let result = array.read(0, &mut buf).await;
    assert!(result.is_err(), "read should fail with all members failed");
}

#[tokio::test]
async fn raid5_degraded_read_reconstructs() {
    let dir = TempDir::new().unwrap();
    let devices = common::create_file_devices(&dir, 4, 4 * 1024 * 1024).await;
    let mut array = RaidArray::create(RaidLevel::Raid5, devices, Some(4096))
        .await
        .unwrap();

    // Write a full stripe
    let data: Vec<u8> = (0..12288u32).map(|i| (i % 256) as u8).collect();
    array.write(0, &data).await.unwrap();
    array.flush().await.unwrap();

    // Read back normally
    let mut normal_read = vec![0u8; 12288];
    array.read(0, &mut normal_read).await.unwrap();
    assert_eq!(normal_read, data, "normal read should work");

    // Fail one member and verify degraded read reconstructs correctly
    array.set_member_state(0, RaidMemberState::Failed);
    let mut degraded_read = vec![0u8; 12288];
    array.read(0, &mut degraded_read).await.unwrap();
    assert_eq!(degraded_read, data, "degraded read should reconstruct correctly");
}

#[tokio::test]
async fn raid_member_states() {
    let dir = TempDir::new().unwrap();
    let devices = common::create_file_devices(&dir, 2, 4 * 1024 * 1024).await;
    let mut array = RaidArray::create(RaidLevel::Raid1, devices, None)
        .await
        .unwrap();

    let states = array.member_states();
    assert_eq!(states.len(), 2);
    assert!(states.iter().all(|(_, s)| *s == RaidMemberState::Active));

    array.set_member_state(0, RaidMemberState::Failed);
    let states = array.member_states();
    assert_eq!(states[0].1, RaidMemberState::Failed);
    assert_eq!(states[1].1, RaidMemberState::Active);
}

#[tokio::test]
async fn raid_rebuild_progress() {
    let dir = TempDir::new().unwrap();
    let devices = common::create_file_devices(&dir, 2, 4 * 1024 * 1024).await;
    let array = RaidArray::create(RaidLevel::Raid1, devices, None)
        .await
        .unwrap();

    // Start rebuild returns progress tracker
    let progress = array.start_rebuild(0).await.unwrap();
    assert!(progress.total_stripes > 0);
}
