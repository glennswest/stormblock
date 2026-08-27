//! NVMe-oF/TCP full-stack integration tests.
//!
//! FileDevice → RAID 1 → ThinVolume → NvmeofTarget → TCP → NvmeofInitiator

mod common;


use stormblock::drive::{open_one_drive, DriveType};
use stormblock::target::nvmeof::NvmeofConfig;
use common::nvmeof_initiator::NvmeofInitiator;

const SUBSYSTEM_NQN: &str = "nqn.2024.io.stormblock:test";
const HOST_NQN: &str = "nqn.2024.io.stormblock:test-host";

fn default_nvmeof_config() -> NvmeofConfig {
    NvmeofConfig {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        nqn: SUBSYSTEM_NQN.into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn nvmeof_full_stack_roundtrip() {
    let (_dir, vol, _vm) = common::setup_raid1_volume(
        64 * 1024 * 1024,
        32 * 1024 * 1024,
    ).await;

    let (addr, server) = common::start_nvmeof_target(vol, default_nvmeof_config()).await;

    // Admin connection (QID=0) for identify commands
    {
        let mut admin = NvmeofInitiator::connect(addr).await.unwrap();
        admin.ic_handshake().await.unwrap();
        let cntlid = admin.fabric_connect(SUBSYSTEM_NQN, HOST_NQN, 0).await.unwrap();
        assert!(cntlid > 0, "should get valid controller ID");

        // Identify Controller
        let ctrl_data = admin.identify_controller().await.unwrap();
        assert!(!ctrl_data.is_empty(), "identify controller should return data");

        // Identify Namespace
        let ns_data = admin.identify_namespace(1).await.unwrap();
        assert!(!ns_data.is_empty(), "identify namespace should return data");
        let nsze = u64::from_le_bytes(ns_data[0..8].try_into().unwrap());
        assert!(nsze > 0, "namespace size should be > 0");
    }

    // I/O connection (QID=1) for read/write
    {
        let mut io = NvmeofInitiator::connect(addr).await.unwrap();
        io.ic_handshake().await.unwrap();
        io.fabric_connect(SUBSYSTEM_NQN, HOST_NQN, 1).await.unwrap();

        // Write 4KB at LBA 0
        let write_data = vec![0xAB_u8; 4096];
        io.write(1, 0, &write_data).await.unwrap();

        // Read back
        let read_data = io.read(1, 0, 1).await.unwrap();
        assert_eq!(read_data.len(), 4096);
        assert_eq!(read_data, write_data);

        // Flush
        io.flush(1).await.unwrap();

        // Write at a different LBA
        let write_data2 = vec![0xCD_u8; 4096];
        io.write(1, 5, &write_data2).await.unwrap();
        let read_data2 = io.read(1, 5, 1).await.unwrap();
        assert_eq!(read_data2, write_data2);

        // Original data at LBA 0 should still be there
        let reread = io.read(1, 0, 1).await.unwrap();
        assert_eq!(reread, write_data);
    }

    server.abort();
}

#[tokio::test]
async fn nvmeof_discovery() {
    let (_dir, vol, _vm) = common::setup_raid1_volume(
        64 * 1024 * 1024,
        32 * 1024 * 1024,
    ).await;

    let (addr, server) = common::start_nvmeof_target(vol, default_nvmeof_config()).await;

    let mut init = NvmeofInitiator::connect(addr).await.unwrap();
    init.ic_handshake().await.unwrap();

    // Connect with discovery NQN
    let discovery_nqn = "nqn.2014-08.org.nvmexpress.discovery";
    let cntlid = init.fabric_connect(discovery_nqn, HOST_NQN, 0).await.unwrap();
    assert!(cntlid > 0);

    server.abort();
}

#[tokio::test]
async fn nvmeof_reconnect_persistence() {
    let (_dir, vol, _vm) = common::setup_raid1_volume(
        64 * 1024 * 1024,
        32 * 1024 * 1024,
    ).await;

    let (addr, server) = common::start_nvmeof_target(vol.clone(), default_nvmeof_config()).await;

    // First session: write data (I/O queue)
    {
        let mut io = NvmeofInitiator::connect(addr).await.unwrap();
        io.ic_handshake().await.unwrap();
        io.fabric_connect(SUBSYSTEM_NQN, HOST_NQN, 1).await.unwrap();
        io.write(1, 0, &vec![0xEE_u8; 4096]).await.unwrap();
        io.flush(1).await.unwrap();
    }

    // Second session: read and verify (I/O queue)
    {
        let mut io = NvmeofInitiator::connect(addr).await.unwrap();
        io.ic_handshake().await.unwrap();
        io.fabric_connect(SUBSYSTEM_NQN, HOST_NQN, 1).await.unwrap();
        let data = io.read(1, 0, 1).await.unwrap();
        assert_eq!(data, vec![0xEE_u8; 4096], "data should persist across sessions");
    }

    server.abort();
}

/// stormblock#73: a remote NVMe-TCP namespace attached as a local drive
/// via the `nvme-tcp://` URI — the cross-node RAID-leg transport.
#[tokio::test]
async fn nvme_tcp_uri_attaches_as_block_device() {
    let (_dir, vol, _vm) = common::setup_raid1_volume(
        64 * 1024 * 1024,
        32 * 1024 * 1024,
    ).await;
    let (addr, server) = common::start_nvmeof_target(vol, default_nvmeof_config()).await;

    let uri = format!("nvme-tcp://{addr}/{SUBSYSTEM_NQN}?nsid=1");
    let dev = open_one_drive(&uri).await.expect("URI attach");
    assert_eq!(dev.device_type(), DriveType::NvmeTcp);
    assert_eq!(dev.id().path, uri);
    assert!(dev.capacity_bytes() > 0);
    let bs = dev.block_size() as usize;
    assert!(bs == 512 || bs == 4096);

    // Identity is stable across reopens of the same spec (#65 lesson).
    let dev2 = open_one_drive(&uri).await.expect("second attach");
    assert_eq!(dev.id().uuid, dev2.id().uuid);
    drop(dev2);

    // Small write/read at a block boundary.
    let data = vec![0x5A_u8; bs * 2];
    assert_eq!(dev.write(bs as u64, &data).await.unwrap(), data.len());
    dev.flush().await.unwrap();
    let mut back = vec![0u8; data.len()];
    assert_eq!(dev.read(bs as u64, &mut back).await.unwrap(), back.len());
    assert_eq!(back, data);

    // Large I/O crossing the 128 KiB chunking boundary.
    let big: Vec<u8> = (0..384 * 1024).map(|i| (i % 251) as u8).collect();
    dev.write(0, &big).await.unwrap();
    let mut big_back = vec![0u8; big.len()];
    dev.read(0, &mut big_back).await.unwrap();
    assert_eq!(big_back, big, "chunked round-trip must be byte-exact");

    // Discard is accepted (thin target reclaims).
    dev.discard(0, (bs * 8) as u64).await.unwrap();

    // Alignment is enforced.
    let mut small = vec![0u8; bs];
    assert!(dev.read(1, &mut small).await.is_err(), "unaligned offset must fail");

    server.abort();
}
