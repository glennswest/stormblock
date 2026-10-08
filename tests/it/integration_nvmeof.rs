//! NVMe-oF/TCP full-stack integration tests.
//!
//! FileDevice → RAID 1 → ThinVolume → NvmeofTarget → TCP → NvmeofInitiator

use crate::common;
use stormblock::drive::{open_one_drive, BlockDevice, DriveType};
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

    // Not whole blocks (#301): read and read-modify-written byte-exact.
    let mut small = vec![0u8; 100];
    dev.read(1, &mut small).await.unwrap();
    assert_eq!(&small[..], &big[1..101]);
    dev.write(bs as u64 - 3, &[0xC3; 7]).await.unwrap();
    let mut around = vec![0u8; 13];
    dev.read(bs as u64 - 6, &mut around).await.unwrap();
    let mut want = big[bs - 6..bs + 7].to_vec();
    want[3..10].copy_from_slice(&[0xC3; 7]);
    assert_eq!(around, want, "only the bytes asked for changed");
    // Past the end is still refused.
    let mut past = vec![0u8; 10];
    assert!(dev.read(dev.capacity_bytes() - 5, &mut past).await.is_err());

    server.abort();
}

/// #331: an attached namespace has several I/O connections. Writes of 64
/// bytes into the same blocks (slot-table entries), from many tasks at once,
/// are read-modify-writes that can no longer share one connection: none may
/// undo another. Large reads and writes in parallel stay byte-exact.
#[tokio::test]
async fn parallel_io_over_several_connections_stays_exact() {
    let (_dir, vol, _vm) = common::setup_raid1_volume(64 * 1024 * 1024, 32 * 1024 * 1024).await;
    let (addr, server) = common::start_nvmeof_target(vol, default_nvmeof_config()).await;
    let uri = format!("nvme-tcp://{addr}/{SUBSYSTEM_NQN}?nsid=1");
    let dev: std::sync::Arc<dyn BlockDevice> = open_one_drive(&uri).await.expect("URI attach").into();

    // 256 entries of 64 bytes over 4 blocks, every entry its own task.
    let tasks: Vec<_> = (0..256u64)
        .map(|i| {
            let dev = dev.clone();
            tokio::spawn(async move { dev.write(i * 64, &[i as u8 ^ 0x5A; 64]).await.unwrap() })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    let mut back = vec![0u8; 256 * 64];
    dev.read(0, &mut back).await.unwrap();
    for i in 0..256usize {
        assert!(back[i * 64..(i + 1) * 64].iter().all(|b| *b == i as u8 ^ 0x5A), "entry {i} was undone");
    }

    // 16 MiB-at-once copies: 1 MiB each, written then read back in parallel.
    const MIB: u64 = 1024 * 1024;
    let tasks: Vec<_> = (1..17u64)
        .map(|k| {
            let dev = dev.clone();
            tokio::spawn(async move {
                let data: Vec<u8> = (0..MIB).map(|i| ((i * 7 + k) % 251) as u8).collect();
                dev.write(k * MIB, &data).await.unwrap();
                let mut got = vec![0u8; MIB as usize];
                dev.read(k * MIB, &mut got).await.unwrap();
                assert!(got == data, "MiB {k} read back different");
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    server.abort();
}

/// #301: a slab on an NVMe/TCP namespace opens, whatever its slot count.
/// The slot table is `total_slots × 64` bytes, which is rarely whole 4 KiB
/// blocks; #155's table scan read exactly that, the initiator refused it, and
/// no release whose slabs forge composed could boot ("bad slab magic", the
/// partition's real error swallowed by the scan of the disk's partitions).
#[tokio::test]
async fn a_slab_on_an_nvme_tcp_namespace_opens_in_either_format() {
    use stormblock::drive::slab::{Slab, SlabFormat, SLAB_VERSION, SLAB_VERSION_2};
    use stormblock::placement::topology::StorageTier;
    for version in [SLAB_VERSION, SLAB_VERSION_2] {
        let (_dir, vol, _vm) = common::setup_raid1_volume(96 * 1024 * 1024, 45 * 1024 * 1024).await;
        let id = {
            let fmt = SlabFormat::new(1024 * 1024, StorageTier::Hot)
                .with_version(version)
                .with_auto_metadata(vol.capacity_bytes());
            let mut slab = Slab::format_with(vol.clone(), fmt).await.unwrap();
            assert_ne!((slab.total_slots() * 64) % 4096, 0, "a table that is not whole blocks");
            let v = stormblock::volume::VolumeId(uuid::Uuid::new_v4());
            slab.allocate(v, 0).await.unwrap();
            slab.allocate(v, 1).await.unwrap();
            slab.sync().await.unwrap();
            slab.slab_id()
        };
        let (addr, server) = common::start_nvmeof_target(vol, default_nvmeof_config()).await;
        let uri = format!("nvme-tcp://{addr}/{SUBSYSTEM_NQN}?nsid=1");
        let dev = open_one_drive(&uri).await.expect("URI attach");
        assert_eq!(dev.block_size(), 4096);
        let slab = Slab::open(dev.into()).await.unwrap_or_else(|e| panic!("v{version} slab over NVMe/TCP: {e}"));
        assert_eq!(slab.slab_id(), id);
        assert_eq!(slab.format_version(), version);
        assert_eq!(slab.total_slots() - slab.free_slots(), 2, "both allocations read back");
        server.abort();
    }
}

/// #358: a command on a connection that went silent (the peer gone with no
/// FIN or RST) waited forever. On the Dell a persist's flush to the
/// appliance's slab never returned, and it held the volume manager for
/// hours. Through a proxy that stops forwarding mid-connection, a flush now
/// fails within the timeout, and the next I/O reconnects and works.
#[tokio::test]
async fn a_command_on_a_connection_that_went_silent_fails_in_bounded_time_and_the_next_reconnects() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    std::env::set_var("STORMBLOCK_NVME_TCP_IO_TIMEOUT_SECS", "2");

    let (_dir, vol, _vm) = common::setup_raid1_volume(96 * 1024 * 1024, 45 * 1024 * 1024).await;
    let (target, server) = common::start_nvmeof_target(vol, default_nvmeof_config()).await;

    // A proxy that can go silent: it keeps every socket open and moves nothing.
    let frozen = Arc::new(AtomicBool::new(false));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = listener.local_addr().unwrap();
    let f = frozen.clone();
    let proxy_task = tokio::spawn(async move {
        loop {
            let Ok((down, _)) = listener.accept().await else { return };
            let Ok(up) = tokio::net::TcpStream::connect(target).await else { continue };
            let (dr, dw) = down.into_split();
            let (ur, uw) = up.into_split();
            for (mut from, mut to, f) in [
                (Box::new(dr) as Box<dyn tokio::io::AsyncRead + Unpin + Send>, Box::new(uw) as Box<dyn tokio::io::AsyncWrite + Unpin + Send>, f.clone()),
                (Box::new(ur), Box::new(dw), f.clone()),
            ] {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 64 * 1024];
                    loop {
                        while f.load(Ordering::SeqCst) {
                            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        }
                        let n = match from.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        while f.load(Ordering::SeqCst) {
                            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        }
                        if to.write_all(&buf[..n]).await.is_err() {
                            return;
                        }
                    }
                });
            }
        }
    });

    let uri = format!("nvme-tcp://{proxy}/{SUBSYSTEM_NQN}?nsid=1");
    let dev = open_one_drive(&uri).await.expect("attach through the proxy");
    dev.write(0, &vec![0x11u8; 4096]).await.unwrap();
    dev.flush().await.unwrap();

    frozen.store(true, Ordering::SeqCst);
    let t0 = std::time::Instant::now();
    let r = tokio::time::timeout(std::time::Duration::from_secs(20), dev.flush())
        .await
        .expect("a flush on a silent connection must not wait forever");
    let took = t0.elapsed();
    assert!(r.is_err(), "a flush nobody answered reported success");
    assert!(took >= std::time::Duration::from_millis(1500), "gave up too early: {took:?}");
    assert!(took < std::time::Duration::from_secs(10), "bounded by the 2 s timeout, took {took:?}");

    frozen.store(false, Ordering::SeqCst);
    dev.write(4096, &vec![0x22u8; 4096]).await.expect("the next write reconnects");
    let mut back = vec![0u8; 8192];
    dev.read(0, &mut back).await.unwrap();
    assert!(back[..4096].iter().all(|b| *b == 0x11) && back[4096..].iter().all(|b| *b == 0x22));
    proxy_task.abort();
    server.abort();
}
