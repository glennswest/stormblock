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
    // Three attempts of 2 s each and the backoff between (#359), bounded.
    assert!(took < std::time::Duration::from_secs(15), "bounded by the 2 s timeout and the retry policy, took {took:?}");

    frozen.store(false, Ordering::SeqCst);
    dev.write(4096, &vec![0x22u8; 4096]).await.expect("the next write reconnects");

    // #359: a blip the network recovers from is not an error at all. Frozen
    // for 2.5 s mid-flush: the first attempt times out (2 s), the retry
    // reconnects once the path is back, and the flush succeeds.
    frozen.store(true, Ordering::SeqCst);
    let f = frozen.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        f.store(false, Ordering::SeqCst);
    });
    let t0 = std::time::Instant::now();
    dev.flush().await.expect("a flush across a 2.5 s blip succeeds on a retry");
    assert!(t0.elapsed() >= std::time::Duration::from_secs(2), "it did wait out the blip: {:?}", t0.elapsed());
    let mut back = vec![0u8; 8192];
    dev.read(0, &mut back).await.unwrap();
    assert!(back[..4096].iter().all(|b| *b == 0x11) && back[4096..].iter().all(|b| *b == 0x22));
    proxy_task.abort();
    server.abort();
}

/// #116: the discovery record says where to dial, never the bind address. A
/// target listening on a wildcard, advertised by a host *name* (which used
/// to fail the address parse and fall back to `0.0.0.0` and the bound
/// port), names the address the name resolves to and the advertised port.
#[tokio::test]
async fn the_discovery_record_names_a_dialable_address_for_a_host_name() {
    let (_dir, vol, _vm) = common::setup_raid1_volume(64 * 1024 * 1024, 32 * 1024 * 1024).await;
    let advertised = stormblock::target::nvmeof::advertise_at("localhost", 3319).expect("localhost resolves");
    let config = NvmeofConfig {
        listen_addr: "0.0.0.0:4420".parse().unwrap(),
        nqn: SUBSYSTEM_NQN.into(),
        advertised_addr: Some(advertised),
        ..Default::default()
    };
    let (addr, server) = common::start_nvmeof_target(vol, config).await;
    let mut init = NvmeofInitiator::connect(addr).await.unwrap();
    init.ic_handshake().await.unwrap();
    init.fabric_connect("nqn.2014-08.org.nvmexpress.discovery", HOST_NQN, 0).await.unwrap();
    let page = init.get_log_page(0x70, 4096).await.unwrap();
    let numrec = u64::from_le_bytes(page[8..16].try_into().unwrap()) as usize;
    assert!(numrec >= 1, "the subsystem is listed");
    let field = |b: &[u8]| String::from_utf8_lossy(b).trim_end_matches(['\0', ' ']).to_string();
    let e = &page[1024..2048];
    let (trsvcid, subnqn, traddr) = (field(&e[32..64]), field(&e[256..512]), field(&e[512..768]));
    assert_eq!(subnqn, SUBSYSTEM_NQN);
    assert_eq!(traddr, advertised.ip().to_string(), "the resolved address, not 0.0.0.0");
    assert!(traddr == "127.0.0.1" || traddr == "::1", "{traddr}");
    assert_eq!(trsvcid, "3319", "the advertised port, not the bound one");
    server.abort();
}


/// A TCP proxy to `to` on `on` that can be killed: its listener and every
/// connection through it go at once, as a link that drops.
async fn killable_proxy(on: &str, to: std::net::SocketAddr) -> (std::net::SocketAddr, impl FnOnce()) {
    use std::sync::{Arc, Mutex};
    let listener = tokio::net::TcpListener::bind(on).await.unwrap();
    let at = listener.local_addr().unwrap();
    let tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> = Arc::default();
    let t2 = tasks.clone();
    let accept = tokio::spawn(async move {
        loop {
            let Ok((mut a, _)) = listener.accept().await else { return };
            let t = tokio::spawn(async move {
                let Ok(mut b) = tokio::net::TcpStream::connect(to).await else { return };
                let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
            });
            t2.lock().unwrap().push(t);
        }
    });
    let kill = move || {
        accept.abort();
        for t in tasks.lock().unwrap().drain(..) {
            t.abort();
        }
    };
    (at, kill)
}

/// #90: a namespace by two addresses is two paths. I/O goes by both (each
/// carries its share, round-robin), and when one path's link drops the
/// I/O goes on by the other, every byte right.
#[tokio::test]
async fn two_paths_carry_the_io_and_one_dropping_leaves_the_other() {
    use stormblock::drive::nvmeof_dev::{NvmeTcpSpec, NvmeofDevice};
    std::env::set_var("STORMBLOCK_NVME_TCP_IO_TIMEOUT_SECS", "3");
    let (_dir, vol, _vm) = common::setup_raid1_volume(96 * 1024 * 1024, 45 * 1024 * 1024).await;
    let (target, server) = common::start_nvmeof_target(vol, default_nvmeof_config()).await;
    let (p1, _kill1) = killable_proxy("127.0.0.1:0", target).await;
    let (p2, kill2) = killable_proxy("127.0.0.2:0", target).await;

    for policy in ["", "&iopolicy=queue-depth"] {
        let spec = NvmeTcpSpec::parse(&format!("nvme-tcp://{p1},{p2}/{SUBSYSTEM_NQN}?nsid=1{policy}")).unwrap();
        let dev = std::sync::Arc::new(NvmeofDevice::connect(&spec).await.expect("two paths attach"));
        let chunk = 128 * 1024usize;
        let writes: Vec<_> = (0..64u64)
            .map(|i| {
                let dev = dev.clone();
                tokio::spawn(async move {
                    let buf = vec![(i as u8).wrapping_add(1); chunk];
                    dev.write(i * chunk as u64, &buf).await.unwrap();
                })
            })
            .collect();
        for w in writes {
            w.await.unwrap();
        }
        let st = dev.path_stats();
        assert_eq!(st.len(), 2);
        assert!(st.iter().all(|p| p.bytes >= 4 * chunk as u64), "both paths carry writes ({policy}): {st:?}");
        let mut back = vec![0u8; chunk];
        for i in 0..64u64 {
            dev.read(i * chunk as u64, &mut back).await.unwrap();
            assert!(back.iter().all(|b| *b == (i as u8).wrapping_add(1)), "block run {i}");
        }
    }

    // The second path's link drops; I/O goes on by the first.
    let spec = NvmeTcpSpec::parse(&format!("nvme-tcp://{p1},{p2}/{SUBSYSTEM_NQN}?nsid=1")).unwrap();
    let dev = NvmeofDevice::connect(&spec).await.unwrap();
    let buf = vec![0xAB; 4096];
    for i in 0..8u64 {
        dev.write(i * 4096, &buf).await.unwrap();
    }
    kill2();
    let t0 = std::time::Instant::now();
    for i in 0..32u64 {
        dev.write(i * 4096, &[i as u8; 4096]).await.expect("a write after one path dropped");
    }
    let mut back = vec![0u8; 4096];
    for i in 0..32u64 {
        dev.read(i * 4096, &mut back).await.unwrap();
        assert!(back.iter().all(|b| *b == i as u8));
    }
    let st = dev.path_stats();
    assert!(st[1].down, "the dropped path is marked down: {st:?}");
    assert!(t0.elapsed() < std::time::Duration::from_secs(20), "failover took {:?}", t0.elapsed());
    server.abort();
}

/// #90: on a node with two storage addresses, a target told a wildcard
/// listens on both, and its discovery log names the subsystem at each, so a
/// host's `connect-all` makes a path through each NIC.
#[tokio::test]
async fn a_target_listens_on_and_lists_every_storage_address() {
    use std::sync::Arc;
    use stormblock::target::nvmeof::NvmeofTarget;
    use stormblock::target::reactor::{ReactorConfig, ReactorPool};
    let (_dir, vol, _vm) = common::setup_raid1_volume(64 * 1024 * 1024, 32 * 1024 * 1024).await;
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let a: std::net::IpAddr = "127.0.0.1".parse().unwrap();
    let b: std::net::IpAddr = "127.0.0.2".parse().unwrap();
    stormblock::netif::set_data_addresses(vec![a, b]);
    let mut target = NvmeofTarget::new(NvmeofConfig {
        listen_addr: format!("0.0.0.0:{port}").parse().unwrap(),
        nqn: SUBSYSTEM_NQN.into(),
        ..Default::default()
    });
    stormblock::netif::set_data_addresses(Vec::new());
    target.add_namespace(1, vol);
    let target = Arc::new(target);
    assert_eq!(target.listen_addrs().len(), 2);
    let reactor = ReactorPool::new(&ReactorConfig { core_count: 1, pin_cores: false });
    let t = target.clone();
    let server = tokio::spawn(async move {
        let _ = t.run(&reactor).await;
    });
    for ip in [a, b] {
        common::wait_for_listener(std::net::SocketAddr::new(ip, port)).await;
    }
    assert!(
        tokio::net::TcpStream::connect(std::net::SocketAddr::new("127.0.0.3".parse().unwrap(), port)).await.is_err(),
        "not on the wildcard"
    );
    let mut init = NvmeofInitiator::connect(std::net::SocketAddr::new(b, port)).await.unwrap();
    init.ic_handshake().await.unwrap();
    init.fabric_connect("nqn.2014-08.org.nvmexpress.discovery", HOST_NQN, 0).await.unwrap();
    let page = init.get_log_page(0x70, 4096).await.unwrap();
    let numrec = u64::from_le_bytes(page[8..16].try_into().unwrap()) as usize;
    assert_eq!(numrec, 2, "one record per address");
    let field = |b: &[u8]| String::from_utf8_lossy(b).trim_end_matches(['\0', ' ']).to_string();
    let mut traddrs: Vec<String> = (0..2).map(|i| field(&page[1024 * (i + 1) + 512..1024 * (i + 1) + 768])).collect();
    traddrs.sort();
    assert_eq!(traddrs, vec!["127.0.0.1", "127.0.0.2"]);
    server.abort();
}

/// #407: an extent moves as one command. The target advertises MDTS 11
/// (8 MiB) and takes a 1 MiB read and write in one command each; more than
/// it advertises is refused, not buffered. The engine's initiator sizes its
/// commands by the target's MDTS — 8 MiB here, and 128 KiB against a target
/// that says 128 KiB (every engine before #407, forge 13.7) — and reports its
/// path's MTU and the transfer.
#[tokio::test]
async fn an_extent_moves_as_one_command_and_an_older_target_still_gets_its_size() {
    use stormblock::target::nvmeof::{mdts_for, DEFAULT_MAX_TRANSFER};
    const MIB: usize = 1 << 20;
    let (_dir, vol, _vm) = common::setup_raid1_volume(64 * MIB as u64, 32 * MIB as u64).await;
    let (addr, server) = common::start_nvmeof_target(vol.clone(), default_nvmeof_config()).await;

    let mut ini = NvmeofInitiator::connect(addr).await.unwrap();
    ini.ic_handshake().await.unwrap();
    ini.fabric_connect(SUBSYSTEM_NQN, HOST_NQN, 0).await.unwrap();
    let ctrl = ini.identify_controller().await.unwrap();
    assert_eq!(ctrl[77], 11, "MDTS = 8 MiB in 4 KiB pages");
    assert_eq!(mdts_for(DEFAULT_MAX_TRANSFER), 11);
    let mut io = NvmeofInitiator::connect(addr).await.unwrap();
    io.ic_handshake().await.unwrap();
    io.fabric_connect(SUBSYSTEM_NQN, HOST_NQN, 1).await.unwrap();
    let extent: Vec<u8> = (0..MIB).map(|i| (i % 253) as u8).collect();
    io.write(1, 0, &extent).await.expect("a 1 MiB write is one command");
    let back = io.read(1, 0, (MIB / 4096) as u16).await.expect("a 1 MiB read is one command");
    assert_eq!(back, extent);
    // Past MDTS (8 MiB + one block): refused with Invalid Field, the
    // connection kept.
    let over = (DEFAULT_MAX_TRANSFER / 4096 + 1) as u16;
    // (This test initiator answers an error status with no data.)
    let refused = io.read(1, 0, over).await;
    assert!(refused.as_ref().map(|d| d.is_empty()).unwrap_or(true), "more than MDTS is refused, no data sent");
    assert_eq!(io.read(1, 0, 1).await.unwrap().len(), 4096, "and the connection still answers");

    // The engine's initiator: 8 MiB commands, an MTU it can name.
    let uri = format!("nvme-tcp://{addr}/{SUBSYSTEM_NQN}?nsid=1");
    let dev = open_one_drive(&uri).await.unwrap();
    let p = dev.fabric_path().expect("a fabric device says its path");
    assert_eq!(p.max_transfer as usize, DEFAULT_MAX_TRANSFER);
    assert_eq!(dev.optimal_io_size() as usize, DEFAULT_MAX_TRANSFER);
    #[cfg(target_os = "linux")]
    assert!(p.mtu >= 1500 && p.jumbo == (p.mtu >= 9000), "{p:?} (loopback)");
    let run: Vec<u8> = (0..12 * MIB).map(|i| (i % 241) as u8).collect();
    dev.write(4 * MIB as u64, &run).await.unwrap();
    let mut got = vec![0u8; run.len()];
    dev.read(4 * MIB as u64, &mut got).await.unwrap();
    assert!(got == run, "a 12 MiB run (two commands) is byte-exact");
    server.abort();

    // An older engine's target: MDTS 5, 128 KiB. The initiator goes down to it.
    let old = NvmeofConfig { max_transfer: 128 << 10, maxh2cdata: 128 << 10, ..default_nvmeof_config() };
    let (addr, server) = common::start_nvmeof_target(vol, old).await;
    let mut ini = NvmeofInitiator::connect(addr).await.unwrap();
    ini.ic_handshake().await.unwrap();
    ini.fabric_connect(SUBSYSTEM_NQN, HOST_NQN, 0).await.unwrap();
    assert_eq!(ini.identify_controller().await.unwrap()[77], 5);
    let uri = format!("nvme-tcp://{addr}/{SUBSYSTEM_NQN}?nsid=1");
    let dev = open_one_drive(&uri).await.unwrap();
    assert_eq!(dev.fabric_path().unwrap().max_transfer, 128 << 10);
    let mut got = vec![0u8; 12 * MIB];
    dev.read(4 * MIB as u64, &mut got).await.unwrap();
    assert!(got == run, "the same run in 96 commands of 128 KiB");
    let local = stormblock::drive::filedev::FileDevice::open_with_capacity(
        _dir.path().join("plain.bin").to_str().unwrap(),
        MIB as u64,
    )
    .await
    .unwrap();
    assert!(local.fabric_path().is_none(), "a local device has no fabric path");
    server.abort();
}

/// A proxy to `to` on `on` that records each connection's source address
/// and can cut every connection from one source, and refuse its next ones:
/// a NIC whose link drops.
async fn recording_proxy(
    on: &str,
    to: std::net::SocketAddr,
) -> (std::net::SocketAddr, std::sync::Arc<std::sync::Mutex<Vec<std::net::IpAddr>>>, impl Fn(std::net::IpAddr)) {
    use std::sync::{Arc, Mutex};
    let listener = tokio::net::TcpListener::bind(on).await.unwrap();
    let at = listener.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<std::net::IpAddr>>> = Arc::default();
    let cut: Arc<Mutex<Vec<std::net::IpAddr>>> = Arc::default();
    let tasks: Arc<Mutex<Vec<(std::net::IpAddr, tokio::task::JoinHandle<()>)>>> = Arc::default();
    let (s2, c2, t2) = (seen.clone(), cut.clone(), tasks.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut a, peer)) = listener.accept().await else { return };
            s2.lock().unwrap().push(peer.ip());
            if c2.lock().unwrap().contains(&peer.ip()) {
                continue;
            }
            let t = tokio::spawn(async move {
                let Ok(mut b) = tokio::net::TcpStream::connect(to).await else { return };
                let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
            });
            t2.lock().unwrap().push((peer.ip(), t));
        }
    });
    let cut_from = move |ip: std::net::IpAddr| {
        cut.lock().unwrap().push(ip);
        for (from, t) in tasks.lock().unwrap().iter() {
            if *from == ip {
                t.abort();
            }
        }
    };
    (at, seen, cut_from)
}

/// #450: one target address, two NICs that reach it: two paths, each bound
/// to its NIC's address, sharing the I/O; when one NIC's link drops the I/O
/// goes on by the other, every byte right, and nothing was configured.
#[tokio::test]
async fn every_nic_that_reaches_the_target_carries_a_path() {
    use stormblock::drive::nvmeof_dev::{NvmeTcpSpec, NvmeofDevice};
    std::env::set_var("STORMBLOCK_NVME_TCP_IO_TIMEOUT_SECS", "3");
    // Two "NICs" on loopback addresses; the target is on their subnet.
    std::env::set_var("STORMBLOCK_IFADDRS", "nic0=127.0.0.1/8,nic1=127.0.0.2/8");
    let (_dir, vol, _vm) = common::setup_raid1_volume(96 * 1024 * 1024, 45 * 1024 * 1024).await;
    let (target, server) = common::start_nvmeof_target(vol, default_nvmeof_config()).await;
    let (proxy, seen, cut_from) = recording_proxy("127.0.0.1:0", target).await;

    let spec = NvmeTcpSpec::parse(&format!("nvme-tcp://{proxy}/{SUBSYSTEM_NQN}?nsid=1")).unwrap();
    let dev = std::sync::Arc::new(NvmeofDevice::connect(&spec).await.expect("attaches"));
    let chunk = 128 * 1024usize;
    let writes: Vec<_> = (0..64u64)
        .map(|i| {
            let dev = dev.clone();
            tokio::spawn(async move {
                let buf = vec![(i as u8).wrapping_add(1); chunk];
                dev.write(i * chunk as u64, &buf).await.unwrap();
            })
        })
        .collect();
    for w in writes {
        w.await.unwrap();
    }
    let st = dev.path_stats();
    assert_eq!(st.len(), 2, "one path per NIC: {st:?}");
    assert_eq!(st[0].interface.as_deref(), Some("nic0"));
    assert_eq!(st[1].interface.as_deref(), Some("nic1"));
    assert_eq!(st[1].source.as_deref(), Some("127.0.0.2"));
    assert!(st.iter().all(|p| p.bytes >= 4 * chunk as u64), "both carry writes: {st:?}");
    {
        let seen = seen.lock().unwrap();
        let a: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let b: std::net::IpAddr = "127.0.0.2".parse().unwrap();
        assert!(seen.contains(&a) && seen.contains(&b), "connections from both NICs' addresses: {seen:?}");
    }
    assert_eq!(dev.id().path, spec.uri(), "the device is named by the URI as given");

    // nic1's link drops: the I/O goes on by nic0.
    cut_from("127.0.0.2".parse().unwrap());
    for i in 0..32u64 {
        dev.write(i * 4096, &[i as u8; 4096]).await.expect("a write after one NIC dropped");
    }
    let mut back = vec![0u8; 4096];
    for i in 0..32u64 {
        dev.read(i * 4096, &mut back).await.unwrap();
        assert!(back.iter().all(|b| *b == i as u8));
    }
    let mut big = vec![0u8; chunk];
    for i in 32..64u64 {
        dev.read(i * chunk as u64, &mut big).await.unwrap();
        assert!(big.iter().all(|b| *b == (i as u8).wrapping_add(1)), "block run {i}");
    }
    std::env::remove_var("STORMBLOCK_IFADDRS");
    server.abort();
}
