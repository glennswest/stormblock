//! vhost-user-blk over the volume API (#82): a socket per volume on this
//! node, held against a delete, read-only at the device, gone on detach.
//! The protocol itself is tested in `drive::vhost_user_blk::tests`.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use serde_json::{json, Value};
use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::raid::{RaidArray, RaidLevel};
use stormblock::volume::VolumeManager;
use tempfile::TempDir;
use tokio::net::TcpListener;

use crate::common;

const SLOT: u64 = 4096;

async fn node(dir: &TempDir) -> (Arc<AppState>, String) {
    let devices = common::create_file_devices(dir, 2, 32 * 1024 * 1024).await;
    let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();
    let array_id = array.array_id();
    let backing: Arc<dyn BlockDevice> = Arc::new(array);
    let mut vm = VolumeManager::new(SLOT);
    vm.add_backing_device(array_id, backing).await;
    let mut config = StormBlockConfig::default();
    // A short data dir: a socket path is at most 107 bytes.
    config.management.data_dir = Some(dir.path().to_string_lossy().to_string());
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    let state = Arc::new(AppState::new(config, vm, reg, gem));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    common::wait_for_listener(addr).await;
    (state, format!("http://{addr}"))
}

async fn call(c: &reqwest::Client, method: reqwest::Method, url: String, body: Option<Value>) -> (u16, Value) {
    let mut r = c.request(method, url);
    if let Some(b) = body {
        r = r.json(&b);
    }
    let resp = r.send().await.unwrap();
    let s = resp.status().as_u16();
    (s, resp.json().await.unwrap_or(Value::Null))
}

/// GET_FEATURES over the socket, as a VMM's first message.
fn features(socket: &str) -> u64 {
    let mut s = UnixStream::connect(socket).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut m = 1u32.to_le_bytes().to_vec();
    m.extend_from_slice(&1u32.to_le_bytes());
    m.extend_from_slice(&0u32.to_le_bytes());
    s.write_all(&m).unwrap();
    let mut r = [0u8; 20];
    s.read_exact(&mut r).unwrap();
    u64::from_le_bytes(r[12..20].try_into().unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_volume_is_served_to_a_vmm_over_a_vhost_user_blk_socket() {
    let dir = TempDir::new().unwrap();
    let (state, base) = node(&dir).await;
    let id = state.volume_manager.lock().await.create_volume_any("vm-disk", 8 << 20).await.unwrap().0;
    let c = reqwest::Client::new();
    let url = format!("{base}/api/v1/volumes/{id}/attach");

    let (s, v) = call(&c, reqwest::Method::POST, url.clone(), Some(json!({ "transport": "vhost-user-blk" }))).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["transport"], "vhost_user_blk");
    assert_eq!(v["read_only"], false);
    let socket = v["socket"].as_str().unwrap().to_string();
    assert!(std::path::Path::new(&socket).exists(), "{socket}");

    // Attaching again is the same socket; in the other mode, refused.
    let (s, again) = call(&c, reqwest::Method::POST, url.clone(), Some(json!({ "transport": "vhost_user_blk" }))).await;
    assert_eq!((s, again["socket"].as_str()), (200, Some(socket.as_str())));
    let (s, _) = call(&c, reqwest::Method::POST, url.clone(), Some(json!({ "transport": "vhost", "mode": "ro" }))).await;
    assert_eq!(s, 409);
    // Another node cannot be handed a local socket.
    let (s, _) = call(&c, reqwest::Method::POST, url.clone(), Some(json!({ "transport": "vhost", "node": "elsewhere" }))).await;
    assert_eq!(s, 409);

    // A VMM's first question is answered: a read-write disk.
    let f = tokio::task::spawn_blocking({ let s = socket.clone(); move || features(&s) }).await.unwrap();
    assert_ne!(f & (1 << 32), 0, "VIRTIO_F_VERSION_1");
    assert_eq!(f & (1 << 5), 0, "not read-only");

    let (s, g) = call(&c, reqwest::Method::GET, url.clone(), None).await;
    assert_eq!(s, 200);
    assert_eq!(g["info"]["transport"], "vhost_user_blk");

    // Held: a guest may be on it.
    let (s, d) = call(&c, reqwest::Method::DELETE, format!("{base}/api/v1/volumes/{id}"), None).await;
    assert_eq!(s, 409, "{d}");
    assert!(d.to_string().contains("vhost-user-blk"), "{d}");

    let (s, _) = call(&c, reqwest::Method::DELETE, url.clone(), None).await;
    assert_eq!(s, 200);
    assert!(!std::path::Path::new(&socket).exists(), "the socket goes on detach");

    // Read-only: offered as RO to the VMM.
    let (s, v) = call(&c, reqwest::Method::POST, url.clone(), Some(json!({ "transport": "vhost-user-blk", "mode": "ro" }))).await;
    assert_eq!(s, 200, "{v}");
    let socket = v["socket"].as_str().unwrap().to_string();
    let f = tokio::task::spawn_blocking(move || features(&socket)).await.unwrap();
    assert_ne!(f & (1 << 5), 0, "VIRTIO_BLK_F_RO");
    let (s, _) = call(&c, reqwest::Method::DELETE, url.clone(), None).await;
    assert_eq!(s, 200);

    let (s, d) = call(&c, reqwest::Method::DELETE, format!("{base}/api/v1/volumes/{id}"), None).await;
    assert!(s == 200 || s == 204, "detached: deleted ({s} {d})");
}

/// The transport is the volume API's; /v1 (the CSI contract) says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v1_attach_refuses_vhost_user_blk() {
    let dir = TempDir::new().unwrap();
    let (_state, base) = node(&dir).await;
    let c = reqwest::Client::new();
    let (s, v) = call(&c, reqwest::Method::POST, format!("{base}/v1/volumes"), Some(json!({ "name": "pvc-1", "size_bytes": 4u64 << 20 }))).await;
    assert!(s == 200 || s == 201, "{v}");
    let id = v["id"].as_str().unwrap();
    let (s, v) = call(&c, reqwest::Method::POST, format!("{base}/v1/volumes/{id}/attach"), Some(json!({ "node": "n1", "mode": "read_write", "transport": "vhost-user-blk" }))).await;
    assert_eq!(s, 400, "{v}");
    assert!(v.to_string().contains("/api/v1/volumes"), "{v}");
}
