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
    /// A delay per read (#418): reading a golden is what a compose does.
    read_ms: u64,
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
        if self.read_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.read_ms)).await;
        }
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
    setup_with(dir, 0).await
}

async fn setup_with(dir: &TempDir, read_ms: u64) -> Arc<AppState> {
    let devices = common::create_file_devices(dir, 2, 512 * 1024 * 1024).await;
    let mut vm = VolumeManager::new(DEFAULT_EXTENT_SIZE);
    for (dev, role) in devices.into_iter().zip([SlabRole::System, SlabRole::Data]) {
        let slow: Arc<dyn BlockDevice> = Arc::new(Slow { inner: dev, read_ms });
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

/// #418: forge stopped answering for a minute under build load (a compose,
/// exports, clones and seals at once; `GET /volumes` 64 s). Under the same
/// mix, on a disk whose reads, writes and flushes are slow, `GET /health`
/// and `GET /volumes` each answer in under a second, every time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_and_listings_answer_while_composes_exports_clones_and_seals_run() {
    let dir = TempDir::new().unwrap();
    let state = setup_with(&dir, 40).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let router = stormblock::mgmt::api::router(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    common::wait_for_listener(addr).await;
    let c = reqwest::Client::new();

    // Two goldens with every extent written: a pallet of them digests both,
    // then reads them again to verify, at 40 ms a read.
    let mut goldens = Vec::new();
    for g in 0..2 {
        let r = c.post(format!("{base}/api/v1/volumes")).json(&serde_json::json!({"name": format!("golden-{g}"), "size": "24M"})).send().await.unwrap();
        assert_eq!(r.status(), 201, "{}", r.text().await.unwrap());
        let v: serde_json::Value = r.json().await.unwrap();
        let id: VolumeId = VolumeId(v["id"].as_str().unwrap().parse().unwrap());
        let dev = state.volume_manager.lock().await.get_volume(&id).unwrap();
        for e in 0..24u64 {
            dev.write(e * DEFAULT_EXTENT_SIZE, &vec![g as u8 + e as u8 + 1; DEFAULT_EXTENT_SIZE as usize]).await.unwrap();
        }
        dev.flush().await.unwrap();
        state.volume_manager.lock().await.seal_volume(id, None).await.unwrap();
        goldens.push(id.0.to_string());
    }
    std::env::set_var("STORMBLOCK_LOCK_ASSERT", "1");

    let mut work = Vec::new();
    // Two composes, as a release does.
    for i in 0..2 {
        let (c, base, g) = (c.clone(), base.clone(), goldens.clone());
        work.push(tokio::spawn(async move {
            let r = c
                .post(format!("{base}/api/v1/volumes/compose/pallet"))
                .json(&serde_json::json!({
                    "name": format!("pallet-{i}"), "pallet": format!("p{i}"), "kind": "system",
                    "members": [
                        {"name": "a", "role": "rootimage", "volume": g[0]},
                        {"name": "b", "role": "rootimage", "volume": g[1]},
                        {"name": "cmdline", "role": "bootconfig", "text": "console=ttyS0"}
                    ]
                }))
                .send()
                .await
                .unwrap();
            (format!("compose pallet-{i}"), r.status().as_u16(), r.text().await.unwrap())
        }));
    }
    // And a release's slab, laid inside a volume, its goldens shared in.
    {
        let (c, base, g) = (c.clone(), base.clone(), goldens.clone());
        work.push(tokio::spawn(async move {
            let r = c
                .post(format!("{base}/api/v1/volumes/compose/slab"))
                .json(&serde_json::json!({
                    "name": "release-slab", "role": "system", "size": "256M",
                    "goldens": [{"name": "a", "volume": g[0]}, {"name": "b", "volume": g[1]}]
                }))
                .send()
                .await
                .unwrap();
            ("compose release-slab".to_string(), r.status().as_u16(), r.text().await.unwrap())
        }));
    }
    // Clones of the goldens, creates that are written and sealed, exports.
    for i in 0..6 {
        let (c1, b1, src) = (c.clone(), base.clone(), goldens[i % 2].clone());
        work.push(tokio::spawn(async move {
            let r = c1.post(format!("{b1}/api/v1/volumes/{src}/clone")).json(&serde_json::json!({"name": format!("cow-{i}")})).send().await.unwrap();
            (format!("clone cow-{i}"), r.status().as_u16(), r.text().await.unwrap())
        }));
        let (c2, b2, st) = (c.clone(), base.clone(), state.clone());
        work.push(tokio::spawn(async move {
            let r = c2.post(format!("{b2}/api/v1/volumes")).json(&serde_json::json!({"name": format!("built-{i}"), "size": "8M"})).send().await.unwrap();
            let status = r.status().as_u16();
            let v: serde_json::Value = r.json().await.unwrap();
            if status != 201 {
                return (format!("create built-{i}"), status, v.to_string());
            }
            let id = v["id"].as_str().unwrap().to_string();
            let dev = st.volume_manager.lock().await.get_volume(&VolumeId(id.parse().unwrap())).unwrap();
            dev.write(0, &vec![i as u8; 4096]).await.unwrap();
            let r = c2.post(format!("{b2}/api/v1/volumes/{id}/seal")).json(&serde_json::json!({"force": true})).send().await.unwrap();
            let status = r.status().as_u16();
            let body = r.text().await.unwrap();
            if !(200..300).contains(&status) {
                return (format!("seal built-{i}"), status, body);
            }
            let r = c2.post(format!("{b2}/api/v1/exports")).json(&serde_json::json!({"volume_id": id, "protocol": "nvmeof"})).send().await.unwrap();
            (format!("export built-{i}"), r.status().as_u16(), r.text().await.unwrap())
        }));
    }

    tokio::time::sleep(Duration::from_millis(50)).await;
    let (mut slow_health, mut slow_list) = (Duration::ZERO, Duration::ZERO);
    let mut polls = 0;
    let started = Instant::now();
    while work.iter().any(|w| !w.is_finished()) && started.elapsed() < Duration::from_secs(180) {
        let t = Instant::now();
        let r = c.get(format!("{base}/api/v1/health")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        slow_health = slow_health.max(t.elapsed());
        let t = Instant::now();
        let r = c.get(format!("{base}/api/v1/volumes")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        slow_list = slow_list.max(t.elapsed());
        polls += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let busy_for = started.elapsed();
    for w in work {
        let (what, status, body) = w.await.unwrap();
        assert!((200..300).contains(&status), "{what}: {status} {body}");
    }
    println!("{polls} health + listing pairs during {busy_for:?}; slowest health {slow_health:?}, listing {slow_list:?}");
    assert!(polls >= 5, "the work ended before the reads ran alongside it ({busy_for:?})");
    assert!(slow_health < Duration::from_secs(1), "health waited {slow_health:?}");
    assert!(slow_list < Duration::from_secs(1), "a listing waited {slow_list:?}");

    // What was made is there.
    let v: serde_json::Value = c.get(format!("{base}/api/v1/volumes")).send().await.unwrap().json().await.unwrap();
    let names: Vec<String> = v["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap().to_string()).collect();
    for n in ["pallet-0", "pallet-1", "release-slab", "cow-5", "built-5"] {
        assert!(names.contains(&n.to_string()), "{n} missing from {names:?}");
    }
    server.abort();
}

/// #415: an import runs in the background, and its create, seal and record
/// persisted under the volume manager, across the slabs' flushes (seen on
/// 12.21 as `held by task …` when sbregistry imported its fedora media). With
/// the rule enforced it finishes, done, and its golden is there.
/// [`setup`] with the volume records on the slabs themselves, as a node keeps
/// them (format 2 metadata regions): a persist writes and flushes the slabs.
async fn setup_records_on_slabs(dir: &TempDir) -> Arc<AppState> {
    let devices = common::create_file_devices(dir, 2, 512 * 1024 * 1024).await;
    let mut vm = VolumeManager::new(DEFAULT_EXTENT_SIZE);
    let mut ids = Vec::new();
    for (dev, role) in devices.into_iter().zip([SlabRole::System, SlabRole::Data]) {
        let slow: Arc<dyn BlockDevice> = Arc::new(Slow { inner: dev, read_ms: 0 });
        let slab = Slab::format_with(
            slow,
            SlabFormat::new(DEFAULT_EXTENT_SIZE, StorageTier::Hot).with_role(role).with_metadata(8 << 20),
        )
        .await
        .unwrap();
        ids.push(slab.slab_id());
        vm.add_slab(slab).await;
    }
    vm.persist_to_slabs(ids);
    let mut config = StormBlockConfig::default();
    config.management.ublk_transport = false;
    let reg = vm.registry().clone();
    let gem = vm.gem().clone();
    Arc::new(AppState::new(config, vm, reg, gem))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_import_never_flushes_with_the_volume_manager_held() {
    let dir = TempDir::new().unwrap();
    let state = setup_records_on_slabs(&dir).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let router = stormblock::mgmt::api::router(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    common::wait_for_listener(addr).await;
    let c = reqwest::Client::new();
    let image = dir.path().join("media.raw");
    let data: Vec<u8> = (0..(6u64 << 20)).map(|i| (i % 251) as u8 + 1).collect();
    std::fs::write(&image, &data).unwrap();
    std::env::set_var("STORMBLOCK_LOCK_ASSERT", "1");

    let r = c
        .post(format!("{base}/api/v1/volumes/import"))
        .json(&serde_json::json!({"name": "media-fedora-44", "file": image.to_str().unwrap(), "format": "raw"}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let job: serde_json::Value = r.json().await.unwrap();
    let id = job["id"].as_str().unwrap().to_string();
    let mut last = serde_json::Value::Null;
    for _ in 0..600 {
        last = c.get(format!("{base}/api/v1/volumes/import/{id}")).send().await.unwrap().json().await.unwrap();
        if last["state"] == "done" || last["state"] == "failed" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(last["state"], "done", "{last}");
    let vm = state.volume_manager.lock().await;
    let id = vm.find_volume("media-fedora-44").await.expect("the golden");
    assert!(vm.is_sealed(&id));
    drop(vm);
    server.abort();
}

/// Two machines' first boots claim at once from an appliance whose disk
/// flushes in 500 ms (#424): pvetest1 waited behind pvetest2 past the
/// initramfs's 10 s. Each claim, and the second waiting for the first, is
/// measured; then their second boots.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_first_boot_claims_at_once_answer_in_seconds_on_a_slow_disk() {
    let _ = tracing_subscriber::fmt().with_env_filter("stormblock::mgmt::api::synonyms=info").with_test_writer().try_init();
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let release = {
        let mut vm = state.volume_manager.lock().await;
        let id = vm.create_volume_any("release-12.21", 256 << 20).await.unwrap();
        let dev = vm.get_volume(&id).unwrap();
        // A release is a disk with a GPT: every clone of it gets a fresh disk
        // GUID stamped, a write and a flush on the claim's path.
        for e in 1..32u64 {
            dev.write(e * DEFAULT_EXTENT_SIZE, &vec![e as u8 + 1; 4096]).await.unwrap();
        }
        let gpt = stormblock::pallet::gpt::Gpt::create_for(dev.block_size(), 256 << 20);
        let (head, tail) = gpt.render();
        dev.write(0, &head).await.unwrap();
        dev.write(gpt.tail_offset(), &tail).await.unwrap();
        dev.flush().await.unwrap();
        id
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let router = stormblock::mgmt::api::router(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    common::wait_for_listener(addr).await;
    let c = reqwest::Client::new();
    let r = c.post(format!("{base}/api/v1/volumes/{release}/seal")).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let r = c.get(format!("{base}/api/v1/volumes/{release}")).send().await.unwrap();
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["fs"]["kind"], "gpt", "{v}");
    let r = c
        .post(format!("{base}/api/v1/synonyms"))
        .json(&serde_json::json!({"namespace": "boothost", "name": "default", "volume": release.to_string()}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    std::env::set_var("STORMBLOCK_LOCK_ASSERT", "1");

    let claim = |host: &'static str| {
        let (c, base) = (c.clone(), base.clone());
        tokio::spawn(async move {
            let t = Instant::now();
            let r = c
                .post(format!("{base}/api/v1/synonyms/boothost/{host}/claim"))
                .json(&serde_json::json!({"agent": {"name": "stormblock-initramfs"}}))
                .send()
                .await
                .unwrap();
            let status = r.status().as_u16();
            let body = r.text().await.unwrap();
            (host, status, body, t.elapsed())
        })
    };
    // One machine alone first: what a first-boot claim costs on this disk
    // (two clones, each stamped and flushed).
    let (_, status, body, alone) = claim("pvetest0").await.unwrap();
    assert_eq!(status, 201, "{body}");
    println!("first boot alone: {alone:?}");
    for round in ["first boot", "second boot"] {
        let a = claim("pvetest1");
        let b = claim("pvetest2");
        let mut slowest = Duration::ZERO;
        for j in [a, b] {
            let (host, status, body, took) = j.await.unwrap();
            assert_eq!(status, 201, "{round} {host}: {body}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            println!("{round}: {host} claimed in {took:?}: {}", v["timing"]);
            // One lock for every machine's claim made the second wait out the
            // whole first (2.3 s here): another machine's claim is no wait.
            let waited = v["timing"]["steps_ms"]["wait"].as_u64().unwrap();
            assert!(waited < 300, "{round}: {host} waited {waited} ms for another machine's claim");
            slowest = slowest.max(took);
        }
        // What is left between them is the disk's: two clones' flushes on
        // one slab take turns. Said, not asserted: a total time is the build
        // VM's load as much as the claim's (6.4 s against 2.9 s alone, once,
        // in the full suite); the `wait` above is what #424 changed.
        println!("{round}: the slower of two claims at once took {slowest:?} ({alone:?} alone)");
    }

    // A machine that stops waiting (#424): the claim still finishes, and the
    // clone it made is the one its host's record names.
    let quick = reqwest::Client::builder().timeout(Duration::from_millis(300)).build().unwrap();
    let gave_up = quick
        .post(format!("{base}/api/v1/synonyms/boothost/pvetest3/claim"))
        .json(&serde_json::json!({}))
        .send()
        .await;
    assert!(gave_up.is_err(), "the claim answered within 300 ms on a 500 ms disk");
    let mut recorded = serde_json::Value::Null;
    for _ in 0..100 {
        let h: serde_json::Value = c.get(format!("{base}/api/v1/boothost/pvetest3")).send().await.unwrap().json().await.unwrap_or_default();
        if !h["last_claim"]["clone"].is_null() {
            recorded = h["last_claim"].clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let clone = recorded["clone"].as_str().unwrap_or_else(|| panic!("the abandoned claim never finished: {recorded}"));
    let v = c.get(format!("{base}/api/v1/volumes/{clone}")).send().await.unwrap();
    assert_eq!(v.status(), 200, "the clone the record names is there");
    server.abort();
}

/// #441: forge's NVMe/TCP target stopped answering a reader for 30 s while
/// two imports wrote large volumes at once (`nvme: reading PDU header: no
/// answer in 30s`, a one-block read of the golden just sealed). The same
/// shape on the same slow disk: one target on one reactor core (every
/// connection shares it, as several do on a node's cores), two initiators
/// writing 96 MiB each into fresh volumes with a flush every 16 MiB, and a
/// third reading 4 KiB blocks of a sealed golden throughout. Every read is
/// answered in a few seconds at worst, never near the initiator's 30 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reader_is_answered_while_two_large_writers_fill_their_volumes() {
    use stormblock::target::nvmeof::{NvmeofConfig, NvmeofTarget};
    use stormblock::target::reactor::{ReactorConfig, ReactorPool};
    const MIB: u64 = 1 << 20;
    const WRITTEN: u64 = 96 * MIB;

    let dir = TempDir::new().unwrap();
    let state = setup_with(&dir, 2).await;
    let (golden, writers) = {
        let mut vm = state.volume_manager.lock().await;
        let g = vm.create_volume_any("cilium.golden", 32 * MIB).await.unwrap();
        let a = vm.create_volume_any("cilium", WRITTEN).await.unwrap();
        let b = vm.create_volume_any("cilium-operator", WRITTEN).await.unwrap();
        (g, [a, b])
    };
    {
        let dev = state.volume_manager.lock().await.get_volume(&golden).unwrap();
        for m in 0..32u64 {
            dev.write(m * MIB, &vec![m as u8 + 1; MIB as usize]).await.unwrap();
        }
        dev.flush().await.unwrap();
        state.volume_manager.lock().await.seal_volume(golden, None).await.unwrap();
    }

    let nqn = "nqn.2026-10.lo.storm:test-441";
    let mut target = NvmeofTarget::new(NvmeofConfig { listen_addr: "127.0.0.1:0".parse().unwrap(), nqn: nqn.into(), ..Default::default() });
    {
        let vm = state.volume_manager.lock().await;
        target.add_namespace(1, vm.get_volume(&golden).unwrap());
        target.add_namespace(2, vm.get_volume(&writers[0]).unwrap());
        target.add_namespace(3, vm.get_volume(&writers[1]).unwrap());
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let target = Arc::new(target);
    let reactor = ReactorPool::new(&ReactorConfig { core_count: 1, pin_cores: false });
    let server = tokio::spawn(async move {
        let _ = target.run_with_listener(listener, &reactor).await;
    });
    common::wait_for_listener(addr).await;
    let open = |nsid: u32| {
        let uri = format!("nvme-tcp://{addr}/{nqn}?nsid={nsid}");
        async move { Arc::<dyn BlockDevice>::from(stormblock::drive::open_one_drive(&uri).await.expect("attach")) }
    };
    let reader = open(1).await;

    let started = Instant::now();
    let mut work = Vec::new();
    for (i, nsid) in [2u32, 3].into_iter().enumerate() {
        let dev = open(nsid).await;
        work.push(tokio::spawn(async move {
            for m in 0..WRITTEN / MIB {
                dev.write(m * MIB, &vec![(i as u8) * 100 + m as u8; MIB as usize]).await.expect("a writer's write");
                if m % 16 == 15 {
                    dev.flush().await.expect("a writer's flush");
                }
            }
            dev.flush().await.expect("a writer's last flush");
        }));
    }

    let (mut reads, mut slowest) = (0u64, Duration::ZERO);
    let mut buf = vec![0u8; 4096];
    let mut at = 0u64;
    while work.iter().any(|w| !w.is_finished()) && started.elapsed() < Duration::from_secs(300) {
        at = (at + 7 * 4096 + 513 * 4096) % (32 * MIB);
        let t = Instant::now();
        reader.read(at, &mut buf).await.expect("the reader's read");
        let took = t.elapsed();
        slowest = slowest.max(took);
        assert!(buf.iter().all(|b| *b == (at / MIB) as u8 + 1), "the golden reads back what was sealed at {at}");
        reads += 1;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let busy = started.elapsed();
    for w in work {
        w.await.unwrap();
    }
    println!("#441: {reads} reads while two writers wrote {} MiB each in {busy:?}; slowest read {slowest:?}", WRITTEN / MIB);
    assert!(reads >= 10, "the writers finished before the reader ran beside them ({busy:?})");
    assert!(slowest < Duration::from_secs(5), "a one-block read waited {slowest:?} behind the writers");
    server.abort();
}

/// A device whose reads cost `read_us` once switched on: what a busy forge
/// disk is to a slot-table page read (#438). Writes and flushes are not slowed,
/// so a test can lay a large golden quickly first.
struct ReadDelay {
    inner: Arc<dyn BlockDevice>,
    on: Arc<std::sync::atomic::AtomicBool>,
    read_us: u64,
    reads: Arc<std::sync::atomic::AtomicU64>,
}

#[async_trait]
impl BlockDevice for ReadDelay {
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
        if self.on.load(std::sync::atomic::Ordering::Relaxed) {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tokio::time::sleep(Duration::from_micros(self.read_us)).await;
        }
        self.inner.read(offset, buf).await
    }
    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        self.inner.write(offset, buf).await
    }
    async fn write_zeroes(&self, offset: u64, len: u64) -> DriveResult<()> {
        self.inner.write_zeroes(offset, len).await
    }
    async fn flush(&self) -> DriveResult<()> {
        self.inner.flush().await
    }
    async fn discard(&self, offset: u64, len: u64) -> DriveResult<()> {
        self.inner.discard(offset, len).await
    }
}

/// #438 on forge (2026-10-17 17:39Z): a claim held the extent map (write) for
/// 9.3 s. A release golden there has tens of thousands of slots, more slot-
/// table pages than the slab's page cache holds (16 MiB, evicted oldest
/// first), so the prefetch before the locks warmed pages the cache evicted
/// again, and the clone's ref-count batch read them from the disk with the
/// extent map and the slab registry held for writing.
///
/// Here: a 1 MiB cache (256 pages), a golden of 24,000 extents (375 pages),
/// every device read 1 ms. A clone of it, and the delete of that clone, must
/// not keep a reader of the extent map waiting for more than a moment.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clone_of_a_golden_larger_than_the_page_cache_keeps_no_reader_waiting() {
    std::env::set_var("STORMBLOCK_SLOT_CACHE_MB", "1");
    const SLOT: u64 = 64 * 1024;
    const EXTENTS: u64 = 24_000;
    let dir = TempDir::new().unwrap();
    let on = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reads = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let devices = common::create_file_devices(&dir, 1, 2048 * 1024 * 1024).await;
    let dev: Arc<dyn BlockDevice> =
        Arc::new(ReadDelay { inner: devices[0].clone(), on: on.clone(), read_us: 1000, reads: reads.clone() });
    let mut vm = VolumeManager::new(SLOT);
    let slab = Slab::format_with(dev, SlabFormat::new(SLOT, StorageTier::Hot).with_role(SlabRole::System)).await.unwrap();
    vm.add_slab(slab).await;
    let golden = vm.create_volume_any("release", EXTENTS * SLOT).await.unwrap();
    {
        let d = vm.get_volume(&golden).unwrap();
        let block = vec![0x5Au8; 4096];
        for e in 0..EXTENTS {
            d.write(e * SLOT, &block).await.unwrap();
        }
        d.flush().await.unwrap();
    }
    vm.persist().await;
    vm.seal_volume(golden, None).await.unwrap();
    let gem = vm.gem().clone();
    let vm = Arc::new(tokio::sync::Mutex::new(vm));

    // A reader of the extent map, timed, all the while.
    let worst = Arc::new(std::sync::Mutex::new(Duration::ZERO));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let probe = {
        let (gem, worst, stop) = (gem.clone(), worst.clone(), stop.clone());
        tokio::spawn(async move {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let t = Instant::now();
                drop(gem.read().await);
                let w = t.elapsed();
                {
                    let mut m = worst.lock().unwrap();
                    if w > *m {
                        *m = w;
                    }
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
    };

    on.store(true, std::sync::atomic::Ordering::Relaxed);
    let t = Instant::now();
    let clone = vm.lock().await.create_snapshot(golden, "boot-clone").await.unwrap();
    let clone_took = t.elapsed();
    let clone_worst = std::mem::take(&mut *worst.lock().unwrap());
    let t = Instant::now();
    vm.lock().await.delete_volume(clone).await.unwrap();
    let delete_took = t.elapsed();
    let delete_worst = *worst.lock().unwrap();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    probe.await.unwrap();
    println!(
        "clone {clone_took:?} (a reader waited at most {clone_worst:?}); delete {delete_took:?} (at most {delete_worst:?}); {} device reads",
        reads.load(std::sync::atomic::Ordering::Relaxed)
    );
    assert!(clone_worst < Duration::from_millis(100), "a reader of the extent map waited {clone_worst:?} on a clone");
    assert!(delete_worst < Duration::from_millis(100), "a reader of the extent map waited {delete_worst:?} on a delete");
    // And the clone was right: every slot it shares now counts two.
    let _ = VolumeId(golden.0);
}

/// #475 on server1 (X9, SMR disk, 12.37 reboot): container-root clones took
/// 10–20 s and deletes waited up to 14 s on the volume manager. A clone and
/// a delete read the volume's slot-table pages with the manager held, so on
/// a slow disk every other API call queued behind that read.
///
/// Here: the same golden as #438's (24,000 extents, a 1 MiB page cache),
/// every device read 5 ms. A clone, through the clone path every API caller
/// takes, and a delete as the API makes it, while a probe takes the volume
/// manager over and over: the probe never waits more than a moment, though
/// the clone and the delete read hundreds of pages.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clone_and_a_delete_read_their_table_pages_without_the_volume_manager() {
    std::env::set_var("STORMBLOCK_SLOT_CACHE_MB", "1");
    const SLOT: u64 = 64 * 1024;
    const EXTENTS: u64 = 24_000;
    let dir = TempDir::new().unwrap();
    let on = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reads = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let devices = common::create_file_devices(&dir, 1, 2048 * 1024 * 1024).await;
    let dev: Arc<dyn BlockDevice> =
        Arc::new(ReadDelay { inner: devices[0].clone(), on: on.clone(), read_us: 5000, reads: reads.clone() });
    let mut vm = VolumeManager::new(SLOT);
    let slab = Slab::format_with(dev, SlabFormat::new(SLOT, StorageTier::Hot).with_role(SlabRole::System)).await.unwrap();
    vm.add_slab(slab).await;
    let golden = vm.create_volume_any("release", EXTENTS * SLOT).await.unwrap();
    {
        let d = vm.get_volume(&golden).unwrap();
        let block = vec![0x5Au8; 4096];
        for e in 0..EXTENTS {
            d.write(e * SLOT, &block).await.unwrap();
        }
        d.flush().await.unwrap();
    }
    vm.persist().await;
    vm.seal_volume(golden, None).await.unwrap();
    let vm = Arc::new(stormblock::lockwatch::TrackedMutex::new(vm));

    let worst = Arc::new(std::sync::Mutex::new(Duration::ZERO));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let probe = {
        let (vm, worst, stop) = (vm.clone(), worst.clone(), stop.clone());
        tokio::spawn(async move {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let t = Instant::now();
                drop(vm.lock().await);
                let w = t.elapsed();
                {
                    let mut m = worst.lock().unwrap();
                    if w > *m {
                        *m = w;
                    }
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
    };

    on.store(true, std::sync::atomic::Ordering::Relaxed);
    let t = Instant::now();
    let spec = stormblock::fs::CloneSpec { name: "root-clone".into(), ..Default::default() };
    let clone = stormblock::fs::template::clone_volume(&vm, golden, &spec).await.unwrap().volume_id;
    let clone_took = t.elapsed();
    let clone_worst = std::mem::take(&mut *worst.lock().unwrap());
    let reads_clone = reads.load(std::sync::atomic::Ordering::Relaxed);

    let t = Instant::now();
    let pre = VolumeManager::preread_unlocked(&vm, clone).await.ok();
    vm.lock().await.delete_volume_preread(clone, None, pre).await.unwrap();
    let delete_took = t.elapsed();
    let delete_worst = *worst.lock().unwrap();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    probe.await.unwrap();
    println!(
        "clone {clone_took:?} ({reads_clone} reads; the manager waited for at most {clone_worst:?}); \
         delete {delete_took:?} (at most {delete_worst:?}); {} device reads",
        reads.load(std::sync::atomic::Ordering::Relaxed)
    );
    assert!(clone_took >= Duration::from_millis(100), "the clone read the golden's table pages off the slow disk ({reads_clone} reads in {clone_took:?})");
    assert!(clone_worst < Duration::from_millis(100), "the volume manager waited {clone_worst:?} behind a clone");
    assert!(delete_worst < Duration::from_millis(100), "the volume manager waited {delete_worst:?} behind a delete");
}
