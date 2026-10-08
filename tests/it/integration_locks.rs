//! #364: the volume manager's lock is held for bookkeeping only, never across
//! I/O, and a listing never waits on a writer.
//!
//! The node's slabs sit on a device whose flush takes 500 ms and whose writes
//! take 30 ms, and which checks, on every flush, that the flushing task does
//! not hold the volume manager (`STORMBLOCK_LOCK_ASSERT=1` makes that a
//! panic). Creates, clones and a cross-role copy (a "build" of seconds) run
//! at once; every listing meanwhile answers in its normal time.

use crate::common;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use stormblock::drive::slab::{Slab, SlabFormat, SlabRole};
use stormblock::drive::{BlockDevice, DeviceId, DriveResult, DriveType};
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::placement::topology::StorageTier;
use stormblock::volume::{VolumeId, VolumeManager, DEFAULT_EXTENT_SIZE};
use tempfile::TempDir;
use tokio::net::TcpListener;

struct Slow {
    inner: Arc<dyn BlockDevice>,
}

#[async_trait]
impl BlockDevice for Slow {
    fn id(&self) -> &DeviceId {
        self.inner.id()
    }
    fn capacity_bytes(&self) -> u64 {
        self.inner.capacity_bytes()
    }
    fn block_size(&self) -> u32 {
        self.inner.block_size()
    }
    fn optimal_io_size(&self) -> u32 {
        self.inner.optimal_io_size()
    }
    fn device_type(&self) -> DriveType {
        self.inner.device_type()
    }
    async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        self.inner.read(offset, buf).await
    }
    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        tokio::time::sleep(Duration::from_millis(30)).await;
        self.inner.write(offset, buf).await
    }
    async fn flush(&self) -> DriveResult<()> {
        stormblock::lockwatch::assert_not_held("volume manager", "a device flush");
        tokio::time::sleep(Duration::from_millis(500)).await;
        self.inner.flush().await
    }
    async fn discard(&self, offset: u64, len: u64) -> DriveResult<()> {
        self.inner.discard(offset, len).await
    }
}

async fn setup(dir: &TempDir) -> Arc<AppState> {
    let devices = common::create_file_devices(dir, 2, 512 * 1024 * 1024).await;
    let mut vm = VolumeManager::new(DEFAULT_EXTENT_SIZE);
    for (dev, role) in devices.into_iter().zip([SlabRole::System, SlabRole::Data]) {
        let slow: Arc<dyn BlockDevice> = Arc::new(Slow { inner: dev });
        let slab = Slab::format_with(slow, SlabFormat::new(DEFAULT_EXTENT_SIZE, StorageTier::Hot).with_role(role))
            .await
            .unwrap();
        vm.add_slab(slab).await;
    }
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_str().unwrap().to_string());
    config.management.ublk_transport = false;
    let reg = vm.registry().clone();
    let gem = vm.gem().clone();
    Arc::new(AppState::new(config, vm, reg, gem))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn listings_keep_their_time_while_builds_and_creates_flush_a_slow_disk() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let router = stormblock::mgmt::api::router(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    common::wait_for_listener(addr).await;
    let c = reqwest::Client::new();

    // A source with 16 extents written: copying it across the role boundary
    // is a build of seconds on this disk.
    let r = c.post(format!("{base}/api/v1/volumes")).json(&serde_json::json!({"name": "source", "size": "64M"})).send().await.unwrap();
    assert_eq!(r.status(), 201, "{}", r.text().await.unwrap());
    let source: serde_json::Value = r.json().await.unwrap();
    let source_id = source["id"].as_str().unwrap().to_string();
    {
        let dev = state.volume_manager.lock().await.get_volume(&VolumeId(source_id.parse().unwrap())).unwrap();
        for e in 0..16u64 {
            dev.write(e * DEFAULT_EXTENT_SIZE, &vec![e as u8 + 1; 4096]).await.unwrap();
        }
        dev.flush().await.unwrap();
    }
    // Clones are taken from sealed volumes. Set up directly, then the rule
    // is enforced: from here, a flush with the manager held is a panic.
    state.volume_manager.lock().await.seal_volume(VolumeId(source_id.parse().unwrap()), None).await.unwrap();
    std::env::set_var("STORMBLOCK_LOCK_ASSERT", "1");

    // Two builds on different volumes, and twenty creates and clones, at once.
    let mut work = Vec::new();
    for i in 0..2 {
        let (c, base, src) = (c.clone(), base.clone(), source_id.clone());
        work.push(tokio::spawn(async move {
            let r = c
                .post(format!("{base}/api/v1/volumes/{src}/clone"))
                .json(&serde_json::json!({"name": format!("copy-{i}"), "role": "data"}))
                .send()
                .await
                .unwrap();
            (format!("copy-{i}"), r.status().as_u16(), r.text().await.unwrap())
        }));
    }
    for i in 0..10 {
        let (c1, b1) = (c.clone(), base.clone());
        work.push(tokio::spawn(async move {
            let r = c1.post(format!("{b1}/api/v1/volumes")).json(&serde_json::json!({"name": format!("v{i}"), "size": "16M"})).send().await.unwrap();
            (format!("create v{i}"), r.status().as_u16(), r.text().await.unwrap())
        }));
        let (c2, b2, src) = (c.clone(), base.clone(), source_id.clone());
        work.push(tokio::spawn(async move {
            let (c, base) = (c2, b2);
            let r = c
                .post(format!("{base}/api/v1/volumes/{src}/clone"))
                .json(&serde_json::json!({"name": format!("cow-{i}")}))
                .send()
                .await
                .unwrap();
            (format!("clone cow-{i}"), r.status().as_u16(), r.text().await.unwrap())
        }));
    }

    // Meanwhile: listings, one after another, while the work runs.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut slowest = Duration::ZERO;
    let mut lists = 0;
    let started = Instant::now();
    while work.iter().any(|w| !w.is_finished()) && started.elapsed() < Duration::from_secs(120) {
        let t = Instant::now();
        let r = c.get(format!("{base}/api/v1/volumes")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        slowest = slowest.max(t.elapsed());
        lists += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let busy_for = started.elapsed();
    for w in work {
        let (what, status, body) = w.await.unwrap();
        assert!((200..300).contains(&status), "{what}: {status} {body}");
    }
    println!("{lists} listings during {busy_for:?} of builds and creates; the slowest {slowest:?}");
    assert!(lists >= 3, "the work ended before any listing ran alongside it ({busy_for:?})");
    // One flush is 500 ms: a listing that waited behind a single persist
    // under the manager's lock would take at least that.
    assert!(slowest < Duration::from_millis(400), "a listing waited {slowest:?} behind the writers");

    // Everything is there, and the copies carry the source's bytes.
    let v: serde_json::Value = c.get(format!("{base}/api/v1/volumes")).send().await.unwrap().json().await.unwrap();
    let names: Vec<String> = v["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap().to_string()).collect();
    for n in ["copy-0", "copy-1", "v0", "v9", "cow-0", "cow-9"] {
        assert!(names.contains(&n.to_string()), "{n} missing from {names:?}");
    }
    let copy = state.volume_manager.lock().await.find_volume("copy-1").await.unwrap();
    let dev = state.volume_manager.lock().await.get_volume(&copy).unwrap();
    let mut b = vec![0u8; 4096];
    dev.read(15 * DEFAULT_EXTENT_SIZE, &mut b).await.unwrap();
    assert!(b.iter().all(|x| *x == 16), "the copy's last extent is the source's");
    server.abort();
}

/// Two long operations on one volume do not run at once; on two volumes
/// they do (#364).
#[tokio::test]
async fn one_long_operation_per_volume() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let ids: Vec<VolumeId> = {
        let mut vm = state.volume_manager.lock().await;
        let a = vm.create_volume_any("a", 16 << 20).await.unwrap();
        let b = vm.create_volume_any("b", 16 << 20).await.unwrap();
        vec![a, b]
    };
    let vm = state.volume_manager.lock().await;
    let first = vm.begin_op(ids[0], "resync").expect("free");
    assert_eq!(vm.begin_op(ids[0], "restripe").err(), Some("resync"), "one at a time per volume");
    let other = vm.begin_op(ids[1], "restripe").expect("another volume is free");
    drop(first);
    assert!(vm.begin_op(ids[0], "restripe").is_ok(), "released when its guard drops");
    drop(other);
}
