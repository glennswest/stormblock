//! iSCSI full-stack integration tests.
//!
//! FileDevice → RAID 1 → ThinVolume → IscsiTarget → TCP → IscsiInitiator

mod common;


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
        max_connections: 4,
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

/// MC/S: two TCP connections on **one** session, both serving I/O (#31).
///
/// The second connection logs in with the first's ISID and TSIH, which is how
/// RFC 7143 §6.3.1 says "add a connection to that session" — as opposed to
/// "make me a new one". If joining did not work, this would silently become
/// two sessions, each with one connection, and the assertion on the session
/// count is what tells the two apart.
#[tokio::test]
async fn iscsi_mcs_two_connections_on_one_session() {
    let (_dir, vol, _vm) = common::setup_raid1_volume(
        64 * 1024 * 1024,
        32 * 1024 * 1024,
    ).await;
    let (addr, server) = common::start_iscsi_target(vol, default_iscsi_config()).await;

    // Leading connection, asking for MC/S.
    let mut a = IscsiInitiator::connect(addr).await.unwrap().wanting_connections(4);
    a.login(INITIATOR_NAME, TARGET_NAME).await.unwrap();
    assert_eq!(
        a.negotiated_max_connections, 4,
        "the target refused MC/S — it used to clamp MaxConnections to 1"
    );
    let (_blocks, block_size) = a.read_capacity().await.unwrap();

    // Second connection joins the same session.
    let mut b = IscsiInitiator::connect(addr)
        .await
        .unwrap()
        .wanting_connections(4)
        .joining(a.isid(), a.tsih(), 1);
    b.login(INITIATOR_NAME, TARGET_NAME).await.unwrap();
    assert_eq!(b.tsih(), a.tsih(), "the second connection landed on a different session");

    // One session, two connections — not two sessions.
    let sessions = server.target().sessions().await;
    assert_eq!(sessions.len(), 1, "MC/S became two sessions: {sessions:?}");
    assert_eq!(sessions[0].connections, 2, "{sessions:?}");

    // Both connections serve real I/O, and see each other's writes: they are
    // one session over one volume, which is the point of the exercise.
    let payload_a = vec![0xA1u8; block_size as usize];
    a.write(0, &payload_a).await.unwrap();
    assert_eq!(b.read(0, 1).await.unwrap(), payload_a, "connection b did not see a's write");

    let payload_b = vec![0xB2u8; block_size as usize];
    b.write(1, &payload_b).await.unwrap();
    assert_eq!(a.read(1, 1).await.unwrap(), payload_b, "connection a did not see b's write");

    // Interleave, so the session-wide CmdSN window is actually exercised from
    // both sides rather than one draining before the other starts.
    for i in 2..10u64 {
        let block = vec![i as u8; block_size as usize];
        if i % 2 == 0 {
            a.write(i, &block).await.unwrap();
            assert_eq!(b.read(i, 1).await.unwrap(), block);
        } else {
            b.write(i, &block).await.unwrap();
            assert_eq!(a.read(i, 1).await.unwrap(), block);
        }
    }

    // One connection going away leaves the session and its sibling working —
    // the failure this replaces killed the whole session on any close.
    a.logout().await.ok();
    drop(a);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let after = server.target().sessions().await;
    assert_eq!(after.len(), 1, "the session died with one of its connections");
    assert_eq!(after[0].connections, 1, "{after:?}");
    assert_eq!(
        b.read(0, 1).await.unwrap(),
        payload_a,
        "the surviving connection stopped serving"
    );

    server.abort();
}

/// An initiator that does not ask for MC/S still gets exactly one connection,
/// so raising the target's cap cannot change what an existing consumer sees.
#[tokio::test]
async fn iscsi_single_connection_initiators_are_unaffected() {
    let (_dir, vol, _vm) = common::setup_raid1_volume(
        64 * 1024 * 1024,
        32 * 1024 * 1024,
    ).await;
    let (addr, server) = common::start_iscsi_target(vol, default_iscsi_config()).await;

    let mut init = IscsiInitiator::connect(addr).await.unwrap(); // wants 1, the default
    init.login(INITIATOR_NAME, TARGET_NAME).await.unwrap();
    assert_eq!(
        init.negotiated_max_connections, 1,
        "the target pushed MC/S onto an initiator that asked for one connection"
    );

    let (_blocks, block_size) = init.read_capacity().await.unwrap();
    let payload = vec![0x77u8; block_size as usize];
    init.write(0, &payload).await.unwrap();
    assert_eq!(init.read(0, 1).await.unwrap(), payload);

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
        max_connections: 4,
    };

    let (addr, server) = common::start_iscsi_target(vol, config).await;

    // Non-CHAP login should still work when target advertises AuthMethod=None as fallback
    // The login state machine accepts "None" even with CHAP configured if initiator offers it
    // This tests that the target starts and accepts connections
    let connect_result = IscsiInitiator::connect(addr).await;
    assert!(connect_result.is_ok(), "should be able to connect");

    server.abort();
}
