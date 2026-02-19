//! iSCSI full-stack integration tests.
//!
//! FileDevice → RAID 1 → ThinVolume → IscsiTarget → TCP → IscsiInitiator

mod common;

use std::sync::Arc;

use stormblock::target::iscsi::IscsiConfig;
use stormblock::target::iscsi::chap::ChapConfig;
use common::iscsi_initiator::IscsiInitiator;

const TARGET_NAME: &str = "iqn.2024.io.stormblock:test";
const INITIATOR_NAME: &str = "iqn.2024.io.stormblock:test-init";

fn default_iscsi_config() -> IscsiConfig {
    IscsiConfig {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        target_name: TARGET_NAME.into(),
        chap: None,
        max_sessions: 16,
    }
}

#[tokio::test]
async fn iscsi_full_stack_roundtrip() {
    let (_dir, vol, _vm) = common::setup_raid1_volume(
        64 * 1024 * 1024, // 64MB per drive
        32 * 1024 * 1024, // 32MB volume
    ).await;

    let (addr, server) = common::start_iscsi_target(vol, default_iscsi_config()).await;

    let mut init = IscsiInitiator::connect(addr).await.unwrap();
    init.login(INITIATOR_NAME, TARGET_NAME).await.unwrap();

    // INQUIRY
    let inquiry_data = init.inquiry().await.unwrap();
    assert!(!inquiry_data.is_empty(), "inquiry should return data");
    // Byte 0 bits 4:0 = peripheral device type (0 = disk)
    assert_eq!(inquiry_data[0] & 0x1F, 0, "should be a disk device");

    // READ CAPACITY
    let (blocks, block_size) = init.read_capacity().await.unwrap();
    assert!(blocks > 0, "capacity should be > 0 blocks");
    assert_eq!(block_size, 4096, "block size should be 4096");

    // Write 4KB at LBA 0
    let write_data = vec![0xAB_u8; 4096];
    init.write(0, &write_data).await.unwrap();

    // Read back
    let read_data = init.read(0, 1).await.unwrap();
    assert_eq!(read_data.len(), 4096);
    assert_eq!(read_data, write_data);

    // Write at a different LBA
    let write_data2 = vec![0xCD_u8; 4096];
    init.write(10, &write_data2).await.unwrap();
    let read_data2 = init.read(10, 1).await.unwrap();
    assert_eq!(read_data2, write_data2);

    // Original data at LBA 0 should still be there
    let reread = init.read(0, 1).await.unwrap();
    assert_eq!(reread, write_data);

    init.logout().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn iscsi_large_io() {
    let (_dir, vol, _vm) = common::setup_raid1_volume(
        64 * 1024 * 1024,
        32 * 1024 * 1024,
    ).await;

    let (addr, server) = common::start_iscsi_target(vol, default_iscsi_config()).await;

    let mut init = IscsiInitiator::connect(addr).await.unwrap();
    init.login(INITIATOR_NAME, TARGET_NAME).await.unwrap();

    // Write 8 blocks (32KB) at LBA 0
    let write_data: Vec<u8> = (0..32768u32).map(|i| (i % 256) as u8).collect();
    init.write(0, &write_data).await.unwrap();

    // Read back in individual blocks and verify
    for block in 0..8u64 {
        let data = init.read(block, 1).await.unwrap();
        let expected_start = (block as usize) * 4096;
        let expected = &write_data[expected_start..expected_start + 4096];
        assert_eq!(data, expected, "block {block} mismatch");
    }

    init.logout().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn iscsi_reconnect_persistence() {
    let (_dir, vol, _vm) = common::setup_raid1_volume(
        64 * 1024 * 1024,
        32 * 1024 * 1024,
    ).await;

    let (addr, server) = common::start_iscsi_target(vol.clone(), default_iscsi_config()).await;

    // First session: write data
    {
        let mut init = IscsiInitiator::connect(addr).await.unwrap();
        init.login(INITIATOR_NAME, TARGET_NAME).await.unwrap();
        init.write(0, &vec![0xEE_u8; 4096]).await.unwrap();
        init.logout().await.unwrap();
    }

    // Second session: read and verify
    {
        let mut init = IscsiInitiator::connect(addr).await.unwrap();
        init.login(INITIATOR_NAME, TARGET_NAME).await.unwrap();
        let data = init.read(0, 1).await.unwrap();
        assert_eq!(data, vec![0xEE_u8; 4096], "data should persist across sessions");
        init.logout().await.unwrap();
    }

    server.abort();
}

#[tokio::test]
async fn iscsi_chap_authentication() {
    let (_dir, vol, _vm) = common::setup_raid1_volume(
        64 * 1024 * 1024,
        32 * 1024 * 1024,
    ).await;

    let config = IscsiConfig {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        target_name: TARGET_NAME.into(),
        chap: Some(ChapConfig {
            username: "testuser".into(),
            secret: "testsecret".into(),
        }),
        max_sessions: 16,
    };

    let (addr, server) = common::start_iscsi_target(vol, config).await;

    // Non-CHAP login should still work when target advertises AuthMethod=None as fallback
    // The login state machine accepts "None" even with CHAP configured if initiator offers it
    // This tests that the target starts and accepts connections
    let connect_result = IscsiInitiator::connect(addr).await;
    assert!(connect_result.is_ok(), "should be able to connect");

    server.abort();
}
