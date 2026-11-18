//! A snapshot's changes (#399) and shipping them to a remote volume over
//! NVMe/TCP (#400): a full copy, then an incremental one; the target's bytes
//! match each snapshot and only the changed bytes went over the wire; a
//! freed extent arrives as WRITE ZEROES; a cancel, and a restart mid-job.

use crate::common;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use stormblock::drive::filedev::FileDevice;
use stormblock::drive::{BlockDevice, DeviceId, DriveResult, DriveType};
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::raid::RaidArrayId;
use stormblock::target::nvmeof::NvmeofConfig;
use stormblock::volume::VolumeManager;
use tempfile::TempDir;
use tokio::net::TcpListener;
use uuid::Uuid;

const MIB: u64 = 1 << 20;
const NQN: &str = "nqn.2026-11.test:backup";

/// The backup volume, counting what reaches it.
struct Counted {
    inner: Arc<dyn BlockDevice>,
    written: AtomicU64,
    zeroed: AtomicU64,
}

#[async_trait]
impl BlockDevice for Counted {
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
        self.written.fetch_add(buf.len() as u64, Ordering::SeqCst);
        self.inner.write(offset, buf).await
    }
    async fn flush(&self) -> DriveResult<()> {
        self.inner.flush().await
    }
    async fn discard(&self, offset: u64, len: u64) -> DriveResult<()> {
        self.inner.discard(offset, len).await
    }
    async fn write_zeroes(&self, offset: u64, len: u64) -> DriveResult<()> {
        self.zeroed.fetch_add(len, Ordering::SeqCst);
        self.inner.write_zeroes(offset, len).await
    }
}

async fn source(dir: &TempDir) -> (Arc<AppState>, String) {
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().join("s").to_string_lossy().to_string());
    std::fs::create_dir_all(dir.path().join("s")).unwrap();
    let mut vm = VolumeManager::new(MIB);
    let dev = FileDevice::open_with_capacity(dir.path().join("s.bin").to_str().unwrap(), 256 * MIB).await.unwrap();
    vm.add_backing_device(RaidArrayId(Uuid::new_v4()), Arc::new(dev)).await;
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    let state = Arc::new(AppState::new(config, vm, reg, gem));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    common::wait_for_listener(addr).await;
    (state, format!("http://{addr}/v1"))
}

/// The backup node: a thin volume served over NVMe/TCP, counted.
async fn backup(dir: &TempDir, size: u64) -> (Arc<dyn BlockDevice>, Arc<Counted>, String, VolumeManager) {
    let mut vm = VolumeManager::new(MIB);
    let array = RaidArrayId(Uuid::new_v4());
    let dev = FileDevice::open_with_capacity(dir.path().join("t.bin").to_str().unwrap(), 256 * MIB).await.unwrap();
    vm.add_backing_device(array, Arc::new(dev)).await;
    let id = vm.create_volume("vm-disk-backup", size, array).await.unwrap();
    let vol: Arc<dyn BlockDevice> = vm.get_volume(&id).unwrap();
    let c = Arc::new(Counted { inner: vol.clone(), written: AtomicU64::new(0), zeroed: AtomicU64::new(0) });
    let cfg = NvmeofConfig { listen_addr: "127.0.0.1:0".parse().unwrap(), nqn: NQN.into(), ..Default::default() };
    let (addr, _server) = common::start_nvmeof_target(c.clone() as Arc<dyn BlockDevice>, cfg).await;
    (vol, c, format!("nvme-tcp://{addr}/{NQN}?nsid=1"), vm)
}

async fn call(c: &reqwest::Client, m: reqwest::Method, url: String, body: Option<Value>) -> (u16, Value) {
    let mut r = c.request(m, url);
    if let Some(b) = body {
        r = r.json(&b);
    }
    let resp = r.send().await.unwrap();
    (resp.status().as_u16(), resp.json().await.unwrap_or(Value::Null))
}

async fn bytes(d: &dyn BlockDevice, len: u64) -> Vec<u8> {
    let mut b = vec![0u8; len as usize];
    d.read(0, &mut b).await.unwrap();
    b
}

async fn wait_job(c: &reqwest::Client, api: &str, id: &str) -> Value {
    for _ in 0..1200 {
        let (_, v) = call(c, reqwest::Method::GET, format!("{api}/ship-jobs/{id}"), None).await;
        if v["state"] != "copying" {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("ship job {id} never finished");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_then_an_incremental_ship() {
    let dir = TempDir::new().unwrap();
    let c = reqwest::Client::new();
    let size = 32 * MIB;
    let (state, api) = source(&dir).await;
    let (tvol, counted, uri, _tvm) = backup(&dir, size).await;
    let vid = state.volume_manager.lock().await.create_volume_any("vm-disk", size).await.unwrap();
    let vol = state.volume_manager.lock().await.get_volume(&vid).unwrap();
    // Extents 0..8 written, the rest never.
    for x in 0..8u64 {
        vol.write(x * MIB, &vec![x as u8 + 1; MIB as usize]).await.unwrap();
    }
    vol.flush().await.unwrap();
    let (s, s1) = call(&c, reqwest::Method::POST, format!("{api}/snapshots"), Some(json!({"name": "s1", "volume_id": vid.0.to_string()}))).await;
    assert_eq!(s, 200, "{s1}");
    let s1 = s1["id"].as_str().unwrap().to_string();

    // Without a base: the allocated extents, eight of them.
    let (_, ch) = call(&c, reqwest::Method::GET, format!("{api}/snapshots/{s1}/changes"), None).await;
    assert_eq!(ch["changed_bytes"], 8 * MIB, "{ch}");
    assert_eq!(ch["ranges"], json!([{"offset": 0, "length": 8 * MIB, "zero": false}]));

    // The full copy.
    let (s, job) = call(&c, reqwest::Method::POST, format!("{api}/snapshots/{s1}/ship"), Some(json!({"target_uri": uri, "max_bytes_per_sec": 0}))).await;
    assert_eq!(s, 202, "{job}");
    let job = wait_job(&c, &api, job["id"].as_str().unwrap()).await;
    assert_eq!(job["state"], "done", "{job}");
    assert_eq!(job["bytes_total"], 8 * MIB);
    assert_eq!(job["bytes_done"], 8 * MIB);
    let s1_vol = state.volume_manager.lock().await.find_volume("s1").await.unwrap();
    let s1_dev = state.volume_manager.lock().await.get_volume(&s1_vol).unwrap();
    assert!(bytes(&*tvol, size).await == bytes(&*s1_dev, size).await, "the target is s1");
    assert_eq!(counted.written.load(Ordering::SeqCst), 8 * MIB, "only the allocated bytes went over");

    // Three extents changed, one written after s1 and then freed.
    vol.write(2 * MIB, &vec![0xAA; MIB as usize]).await.unwrap();
    vol.write(3 * MIB, &vec![0xBB; MIB as usize]).await.unwrap();
    vol.write(12 * MIB, &vec![0xCC; MIB as usize]).await.unwrap();
    vol.write(5 * MIB, &vec![0xDD; MIB as usize]).await.unwrap();
    vol.discard(5 * MIB, MIB).await.unwrap();
    vol.flush().await.unwrap();
    let (_, s2) = call(&c, reqwest::Method::POST, format!("{api}/snapshots"), Some(json!({"name": "s2", "volume_id": vid.0.to_string()}))).await;
    let s2 = s2["id"].as_str().unwrap().to_string();
    let (_, ch) = call(&c, reqwest::Method::GET, format!("{api}/snapshots/{s2}/changes?base={s1}"), None).await;
    assert_eq!(
        ch["ranges"],
        json!([{"offset": 2 * MIB, "length": 2 * MIB, "zero": false}, {"offset": 5 * MIB, "length": MIB, "zero": true}, {"offset": 12 * MIB, "length": MIB, "zero": false}]),
        "{ch}"
    );
    // By the engine volume's name too (#130: what a promote carries).
    let (s, by_name) = call(&c, reqwest::Method::GET, format!("{api}/snapshots/s2/changes?base=s1"), None).await;
    assert_eq!(s, 200);
    assert_eq!(by_name["ranges"], ch["ranges"]);

    // The incremental copy: three extents, and one WRITE ZEROES.
    counted.written.store(0, Ordering::SeqCst);
    let (s, job) = call(&c, reqwest::Method::POST, format!("{api}/snapshots/{s2}/ship"), Some(json!({"target_uri": uri, "base": s1}))).await;
    assert_eq!(s, 202, "{job}");
    let job = wait_job(&c, &api, job["id"].as_str().unwrap()).await;
    assert_eq!(job["state"], "done", "{job}");
    assert_eq!((job["bytes_total"].as_u64(), job["zero_bytes"].as_u64()), (Some(3 * MIB), Some(MIB)));
    assert_eq!(counted.written.load(Ordering::SeqCst), 3 * MIB, "only the changed bytes went over");
    assert!(counted.zeroed.load(Ordering::SeqCst) >= MIB, "the freed extent as WRITE ZEROES");
    let s2_vol = state.volume_manager.lock().await.find_volume("s2").await.unwrap();
    let s2_dev = state.volume_manager.lock().await.get_volume(&s2_vol).unwrap();
    assert!(bytes(&*tvol, size).await == bytes(&*s2_dev, size).await, "the target is s2");

    // A cancel: throttled to 1 MiB/s, stopped early; the job is kept.
    let (_, job) = call(&c, reqwest::Method::POST, format!("{api}/snapshots/{s2}/ship"), Some(json!({"target_uri": uri, "max_bytes_per_sec": MIB}))).await;
    let jid = job["id"].as_str().unwrap().to_string();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (s, cancelled) = call(&c, reqwest::Method::DELETE, format!("{api}/ship-jobs/{jid}"), None).await;
    assert_eq!(s, 200);
    assert_eq!(cancelled["state"], "failed");
    assert!(cancelled["message"].as_str().unwrap().starts_with("cancelled"), "{cancelled}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (s, still) = call(&c, reqwest::Method::GET, format!("{api}/ship-jobs/{jid}"), None).await;
    assert_eq!((s, still["state"].as_str()), (200, Some("failed")), "a job that existed is never a 404");
    // s2 maps extents 0-4, 6, 7 and 12: 8 MiB.
    assert!(still["bytes_done"].as_u64().unwrap() < 8 * MIB, "it stopped: {still}");

    // A restart mid-job: a copying job recorded half way resumes from there.
    let half = 4 * MIB;
    // What a job cut short leaves: the extents after its offset not yet the
    // snapshot's (a ship writes only what the snapshot maps; the rest of a
    // backup volume is the earlier copies').
    for x in [4u64, 6, 7, 12] {
        tvol.write(x * MIB, &vec![0xEE; MIB as usize]).await.unwrap();
    }
    tvol.flush().await.unwrap();
    let file = dir.path().join("s").join("ship-jobs.json");
    let mut jobs: serde_json::Map<String, Value> = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let mut j = still.clone();
    j["id"] = json!("ship-restarted");
    j["state"] = json!("copying");
    j["message"] = Value::Null;
    j["next_offset"] = json!(half);
    j["bytes_done"] = json!(half);
    j["max_bytes_per_sec"] = json!(0);
    jobs.insert("ship-restarted".into(), j);
    std::fs::write(&file, serde_json::to_vec(&jobs).unwrap()).unwrap();
    stormblock::mgmt::ship::reload(&state);
    counted.written.store(0, Ordering::SeqCst);
    stormblock::mgmt::ship::resume(state.clone()).await;
    let job = wait_job(&c, &api, "ship-restarted").await;
    assert_eq!(job["state"], "done", "{job}");
    assert!(bytes(&*tvol, size).await == bytes(&*s2_dev, size).await, "the target is s2 again");
    let w = counted.written.load(Ordering::SeqCst);
    // From 4 MiB: extent 4, extents 6-7, extent 12.
    assert_eq!(w, 4 * MIB, "only what was after the recorded offset went again");

    // A job whose snapshot is gone reads failed after a restart.
    let mut jobs: serde_json::Map<String, Value> = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let mut j = job.clone();
    j["id"] = json!("ship-orphan");
    j["state"] = json!("copying");
    j["snapshot_volume"] = json!(Uuid::new_v4());
    jobs.insert("ship-orphan".into(), j);
    std::fs::write(&file, serde_json::to_vec(&jobs).unwrap()).unwrap();
    stormblock::mgmt::ship::reload(&state);
    stormblock::mgmt::ship::resume(state.clone()).await;
    let (s, o) = call(&c, reqwest::Method::GET, format!("{api}/ship-jobs/ship-orphan"), None).await;
    assert_eq!((s, o["state"].as_str()), (200, Some("failed")), "{o}");
}
