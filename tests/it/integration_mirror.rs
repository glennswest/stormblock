//! A live mirror of an in-use volume to a peer engine's namespace (#295):
//! every byte the same on both at `synced` and after the stop, with a writer
//! busy throughout; a target that fails leaves the writer untouched and the
//! mirror `failed`; one a restart left behind reports `failed`.

use crate::common;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use stormblock::drive::filedev::FileDevice;
use stormblock::drive::{BlockDevice, DeviceId, DriveError, DriveResult, DriveType};
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::raid::RaidArrayId;
use stormblock::target::nvmeof::NvmeofConfig;
use stormblock::volume::{VolumeId, VolumeManager};
use tempfile::TempDir;
use tokio::net::TcpListener;
use uuid::Uuid;

const MIB: u64 = 1 << 20;
const NQN: &str = "nqn.2026-10.test:peer";

/// The target engine's volume, which can be made to fail every write.
struct Breakable {
    inner: Arc<dyn BlockDevice>,
    broken: AtomicBool,
}

#[async_trait]
impl BlockDevice for Breakable {
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
        if self.broken.load(Ordering::SeqCst) {
            return Err(DriveError::Other(anyhow::anyhow!("the target's disk is gone")));
        }
        self.inner.write(offset, buf).await
    }
    async fn flush(&self) -> DriveResult<()> {
        self.inner.flush().await
    }
    async fn discard(&self, offset: u64, len: u64) -> DriveResult<()> {
        self.inner.discard(offset, len).await
    }
}

/// The source engine (S): its API on a port, a slab on a file.
async fn source(dir: &TempDir) -> (Arc<AppState>, String) {
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().join("s").to_string_lossy().to_string());
    std::fs::create_dir_all(dir.path().join("s")).unwrap();
    let mut vm = VolumeManager::new(MIB);
    let pool = dir.path().join("s-pool.bin");
    let dev = FileDevice::open_with_capacity(pool.to_str().unwrap(), 256 * MIB).await.unwrap();
    vm.add_backing_device(RaidArrayId(Uuid::new_v4()), Arc::new(dev)).await;
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    let state = Arc::new(AppState::new(config, vm, reg, gem));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    common::wait_for_listener(addr).await;
    (state, format!("http://{addr}/api/v1"))
}

/// The target engine (T): a fresh thin volume served over NVMe/TCP.
async fn target(dir: &TempDir, size: u64) -> (Arc<dyn BlockDevice>, Arc<Breakable>, String, VolumeManager) {
    let mut vm = VolumeManager::new(MIB);
    let array = RaidArrayId(Uuid::new_v4());
    let pool = dir.path().join(format!("t-pool-{}.bin", Uuid::new_v4()));
    let dev = FileDevice::open_with_capacity(pool.to_str().unwrap(), 256 * MIB).await.unwrap();
    vm.add_backing_device(array, Arc::new(dev)).await;
    let id = vm.create_volume("migrated-disk", size, array).await.unwrap();
    let vol: Arc<dyn BlockDevice> = vm.get_volume(&id).unwrap();
    let b = Arc::new(Breakable { inner: vol.clone(), broken: AtomicBool::new(false) });
    let cfg = NvmeofConfig { listen_addr: "127.0.0.1:0".parse().unwrap(), nqn: NQN.into(), ..Default::default() };
    let (addr, _server) = common::start_nvmeof_target(b.clone() as Arc<dyn BlockDevice>, cfg).await;
    (vol, b, format!("nvme-tcp://{addr}/{NQN}?nsid=1"), vm)
}

async fn call(c: &reqwest::Client, m: reqwest::Method, url: String, body: Option<Value>) -> (u16, Value) {
    let mut r = c.request(m, url);
    if let Some(b) = body {
        r = r.json(&b);
    }
    let resp = r.send().await.unwrap();
    let s = resp.status().as_u16();
    (s, resp.json().await.unwrap_or(Value::Null))
}

async fn all_bytes(d: &dyn BlockDevice, len: u64) -> Vec<u8> {
    let mut b = vec![0u8; len as usize];
    d.read(0, &mut b).await.unwrap();
    b
}

/// A writer on the volume: 64 KiB writes over the whole volume, and now and
/// then a whole extent discarded. Never expected to see an error.
fn writer(v: Arc<dyn BlockDevice>, size: u64, stop: Arc<AtomicBool>) -> tokio::task::JoinHandle<u64> {
    tokio::spawn(async move {
        let mut n = 0u64;
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        while !stop.load(Ordering::SeqCst) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let at = (x % (size / 65536)) * 65536;
            if n % 40 == 39 {
                let e = at / MIB * MIB;
                v.discard(e, MIB).await.expect("a discard sees no error");
            } else {
                v.write(at, &vec![(n % 251) as u8 + 1; 65536]).await.expect("a write sees no error");
            }
            n += 1;
            if n % 16 == 0 {
                tokio::task::yield_now().await;
            }
        }
        v.flush().await.unwrap();
        n
    })
}

async fn wait_state(c: &reqwest::Client, url: &str, want: &str) -> Value {
    for _ in 0..1200 {
        let (_, v) = call(c, reqwest::Method::GET, url.to_string(), None).await;
        if v["mirror"]["state"] == want {
            return v["mirror"].clone();
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the mirror never became {want}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_volume_in_use_is_mirrored_to_a_peer_byte_for_byte() {
    let dir = TempDir::new().unwrap();
    let c = reqwest::Client::new();
    let size = 48 * MIB;
    let (state, api) = source(&dir).await;
    let (tvol, _b, uri, _tvm) = target(&dir, size).await;
    let (s, v) = call(&c, reqwest::Method::POST, format!("{api}/volumes"), Some(json!({"name": "vm-disk", "size": "48M"}))).await;
    assert!(s == 200 || s == 201, "{v}");
    let id = v["id"].as_str().unwrap().to_string();
    let vol: Arc<dyn BlockDevice> = state.volume_manager.lock().await.get_volume(&VolumeId(id.parse().unwrap())).unwrap();
    // Data before the mirror: half the extents written, a few of them zeros.
    for x in 0..24u64 {
        let fill = if x % 6 == 5 { 0u8 } else { x as u8 + 100 };
        vol.write(x * 2 * MIB, &vec![fill; MIB as usize]).await.unwrap();
    }
    vol.flush().await.unwrap();

    // Refused: a target smaller than the volume, a URI that is not one.
    let (_small, _sb, small_uri, _svm) = target(&dir, 8 * MIB).await;
    let (s, e) = call(&c, reqwest::Method::POST, format!("{api}/volumes/{id}/mirror"), Some(json!({"target": small_uri}))).await;
    assert_eq!(s, 409, "{e}");
    let (s, _) = call(&c, reqwest::Method::POST, format!("{api}/volumes/{id}/mirror"), Some(json!({"target": "http://x"}))).await;
    assert_eq!(s, 400);

    // The writer starts first and runs through the copy.
    let stop = Arc::new(AtomicBool::new(false));
    let w = writer(vol.clone(), size, stop.clone());
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (s, m) = call(&c, reqwest::Method::POST, format!("{api}/volumes/{id}/mirror"), Some(json!({"target": uri}))).await;
    assert_eq!(s, 202, "{m}");
    let (s, e) = call(&c, reqwest::Method::POST, format!("{api}/volumes/{id}/mirror"), Some(json!({"target": uri}))).await;
    assert_eq!(s, 409, "a second mirror: {e}");
    let (s, e) = call(&c, reqwest::Method::PATCH, format!("{api}/volumes/{id}/resize"), Some(json!({"new_size": "64M"}))).await;
    assert_eq!(s, 409, "no resize while mirrored: {e}");
    let synced = wait_state(&c, &format!("{api}/volumes/{id}"), "synced").await;
    assert_eq!(synced["bytes_remaining"], 0, "{synced}");
    assert!(synced["bytes_total"].as_u64().unwrap() > 0, "{synced}");
    // Mirrored synchronously from here: keep writing a while.
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop.store(true, Ordering::SeqCst);
    let writes = w.await.unwrap();
    assert!(writes > 100, "the writer ran ({writes})");
    assert_eq!(all_bytes(vol.as_ref(), size).await, all_bytes(tvol.as_ref(), size).await, "every byte at synced");

    // The switch-over: stopped, flushed, both still the same.
    vol.write(5 * MIB, &[0xAB; 4096]).await.unwrap();
    let (s, st) = call(&c, reqwest::Method::DELETE, format!("{api}/volumes/{id}/mirror"), None).await;
    assert_eq!(s, 200, "{st}");
    assert_eq!((st["state"].as_str(), st["flushed"].as_bool()), (Some("synced"), Some(true)), "{st}");
    assert_eq!(all_bytes(vol.as_ref(), size).await, all_bytes(tvol.as_ref(), size).await, "every byte after the stop");
    // After it, nothing is sent: S changes, T does not.
    let t_before = all_bytes(tvol.as_ref(), size).await;
    vol.write(0, &[0xCD; 4096]).await.unwrap();
    assert_eq!(all_bytes(tvol.as_ref(), size).await, t_before);
    let (_, v) = call(&c, reqwest::Method::GET, format!("{api}/volumes/{id}"), None).await;
    assert!(v.get("mirror").is_none(), "{v}");
    let (s, _) = call(&c, reqwest::Method::DELETE, format!("{api}/volumes/{id}/mirror"), None).await;
    assert_eq!(s, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_target_that_fails_never_reaches_the_writer_and_a_restart_says_failed() {
    let dir = TempDir::new().unwrap();
    let c = reqwest::Client::new();
    let size = 32 * MIB;
    let (state, api) = source(&dir).await;
    let (_tvol, broken, uri, _tvm) = target(&dir, size).await;
    let (_, v) = call(&c, reqwest::Method::POST, format!("{api}/volumes"), Some(json!({"name": "vm-disk", "size": "32M"}))).await;
    let id = v["id"].as_str().unwrap().to_string();
    let vol: Arc<dyn BlockDevice> = state.volume_manager.lock().await.get_volume(&VolumeId(id.parse().unwrap())).unwrap();
    for x in 0..32u64 {
        vol.write(x * MIB, &vec![x as u8 + 1; MIB as usize]).await.unwrap();
    }
    let stop = Arc::new(AtomicBool::new(false));
    let w = writer(vol.clone(), size, stop.clone());
    let (s, _) = call(&c, reqwest::Method::POST, format!("{api}/volumes/{id}/mirror"), Some(json!({"target": uri}))).await;
    assert_eq!(s, 202);
    // The target's disk goes away mid-copy (or just after).
    broken.broken.store(true, Ordering::SeqCst);
    let failed = wait_state(&c, &format!("{api}/volumes/{id}"), "failed").await;
    assert!(!failed["message"].as_str().unwrap_or("").is_empty(), "says why: {failed}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    stop.store(true, Ordering::SeqCst);
    assert!(w.await.unwrap() > 0, "the writer never saw an error");

    // A failed mirror stays until stopped; the stop forgets it.
    let (s, st) = call(&c, reqwest::Method::DELETE, format!("{api}/volumes/{id}/mirror"), None).await;
    assert_eq!((s, st["state"].as_str()), (200, Some("failed")), "{st}");

    // A restart: the record outlives the mirror, which is reported failed.
    broken.broken.store(false, Ordering::SeqCst);
    let (s, _) = call(&c, reqwest::Method::POST, format!("{api}/volumes/{id}/mirror"), Some(json!({"target": uri}))).await;
    assert_eq!(s, 202);
    let h = state.volume_manager.lock().await.get_volume_handle(&VolumeId(id.parse().unwrap())).unwrap();
    h.stop_mirror().await.expect("held"); // what a restart does to the live half
    let (_, v) = call(&c, reqwest::Method::GET, format!("{api}/volumes/{id}"), None).await;
    assert_eq!(v["mirror"]["state"], "failed", "{v}");
    assert!(v["mirror"]["message"].as_str().unwrap().contains("restarted"), "{v}");
    let (s, _) = call(&c, reqwest::Method::DELETE, format!("{api}/volumes/{id}/mirror"), None).await;
    assert_eq!(s, 200, "a restart's record is forgotten");
    let (_, v) = call(&c, reqwest::Method::GET, format!("{api}/volumes/{id}"), None).await;
    assert!(v.get("mirror").is_none(), "{v}");
}
