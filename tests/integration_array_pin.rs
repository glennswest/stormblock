//! A volume that *is* an array, over the API (#150).
//!
//! stormstorage builds a RAID1 on a head across NVMe-TCP legs and needs a
//! volume to attach that is that mirror. Here: two drives become a RAID1
//! (dedicated by default), a third drive is the node's general pool; a volume
//! created with `array_id` — on `/api/v1` and on `/v1` — lives entirely on the
//! array, an ordinary volume never does, `GET /arrays/{id}` names the slab and
//! its volumes, and the array cannot be deleted from under them.

mod common;

use std::sync::Arc;

use serde_json::{json, Value};
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::volume::{VolumeId, VolumeManager};
use tempfile::TempDir;
use tokio::net::TcpListener;

const MIB: u64 = 1 << 20;

async fn start(dir: &TempDir) -> (Arc<AppState>, String, reqwest::Client) {
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_string_lossy().to_string());
    config.management.node_name = Some("head".into());
    let vm = VolumeManager::new(MIB);
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    let state = Arc::new(AppState::new(config, vm, reg, gem));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    common::wait_for_listener(addr).await;
    (state, format!("http://{addr}"), reqwest::Client::new())
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

async fn open_drive(c: &reqwest::Client, api: &str, dir: &TempDir, name: &str) -> (String, String) {
    let path = dir.path().join(name).to_string_lossy().to_string();
    let (s, d) = call(c, reqwest::Method::POST, format!("{api}/drives"), Some(json!({ "path": path, "size_bytes": 64 * MIB }))).await;
    assert!(s == 200 || s == 201, "{d}");
    (d["uuid"].as_str().unwrap().to_string(), path)
}

/// Slab ids every leg of a volume sits on.
async fn slabs_of(c: &reqwest::Client, api: &str, id: &str) -> Vec<String> {
    let (_, v) = call(c, reqwest::Method::GET, format!("{api}/volumes/{id}"), None).await;
    let mut out: Vec<String> = v["placement"]["slabs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["legs"].as_u64().unwrap_or(0) > 0)
        .map(|s| s["id"].as_str().unwrap().to_string())
        .collect();
    out.sort();
    out.dedup();
    out
}

async fn fill(state: &AppState, id: &str, extents: u64, seed: u8) {
    let v = state.volume_manager.lock().await.get_volume(&VolumeId(id.parse().unwrap())).unwrap();
    for i in 0..extents {
        v.write(i * MIB, &vec![seed.wrapping_add(i as u8); MIB as usize]).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_volume_on_a_dedicated_array_is_that_array() {
    let dir = TempDir::new().unwrap();
    let (state, base, c) = start(&dir).await;
    let api = format!("{base}/api/v1");

    // The node's general pool: one drive with a slab.
    let (_, gpath) = open_drive(&c, &api, &dir, "general.img").await;
    let (s, g) = call(&c, reqwest::Method::POST, format!("{api}/slabs"), Some(json!({ "device_path": gpath, "slot_size": MIB }))).await;
    assert!(s == 200 || s == 201, "{g}");
    let general = g["slab_id"].as_str().or(g["id"].as_str()).unwrap().to_string();

    // Two drives, one RAID1 — dedicated by default.
    let (a, _) = open_drive(&c, &api, &dir, "leg-a.img").await;
    let (b, _) = open_drive(&c, &api, &dir, "leg-b.img").await;
    let (s, arr) = call(&c, reqwest::Method::POST, format!("{api}/arrays"), Some(json!({ "level": "Raid1", "drive_uuids": [a, b] }))).await;
    assert_eq!(s, 201, "{arr}");
    let array = arr["id"].as_str().unwrap().to_string();
    let aslab = arr["slab"]["id"].as_str().unwrap().to_string();
    assert_eq!(arr["slab"]["dedicated"], true, "{arr:#}");
    assert_eq!(arr["slab"]["self_describing"], true);
    assert_eq!(arr["slab"]["role"], "data");
    assert_ne!(aslab, general);

    // An ordinary volume, written well past what one slab's free-most choice
    // would keep off the array: none of it lands there.
    let (s, plain) = call(&c, reqwest::Method::POST, format!("{api}/volumes"), Some(json!({ "name": "container-root", "size": "16M" }))).await;
    assert!(s == 200 || s == 201, "{plain}");
    let plain = plain["id"].as_str().unwrap().to_string();
    fill(&state, &plain, 16, 1).await;
    assert_eq!(slabs_of(&c, &api, &plain).await, vec![general.clone()]);

    // The consumer volume, pinned by `array_id`.
    let (s, pv) = call(&c, reqwest::Method::POST, format!("{api}/volumes"), Some(json!({ "name": "dist-vol", "size": "16M", "array_id": array }))).await;
    assert!(s == 200 || s == 201, "{pv}");
    let pv = pv["id"].as_str().unwrap().to_string();
    fill(&state, &pv, 16, 50).await;
    assert_eq!(slabs_of(&c, &api, &pv).await, vec![aslab.clone()]);
    // Its redundancy is the array's.
    let (s, e) = call(&c, reqwest::Method::POST, format!("{api}/volumes"), Some(json!({ "name": "x", "size": "4M", "array_id": array, "redundancy": "mirror" }))).await;
    assert_eq!(s, 400, "{e}");

    // The same on /v1: a volume with a /v1 identity, carved on the array.
    let (s, v1) = call(
        &c,
        reqwest::Method::POST,
        format!("{base}/v1/volumes"),
        // No replicas on other nodes: the array is this volume's redundancy,
        // and whether to add a second node on top is another axis.
        Some(json!({ "name": "dist-vol-2", "size_bytes": 8 * MIB, "replica_tier": { "slaves": 0 }, "placement": { "array_id": array } })),
    )
    .await;
    assert!(s == 200 || s == 201, "{v1}");
    let local = state.volume_manager.lock().await.find_volume("dist-vol-2").await.expect("backed locally");
    fill(&state, &local.0.to_string(), 8, 90).await;
    assert_eq!(slabs_of(&c, &api, &local.0.to_string()).await, vec![aslab.clone()]);

    // The array names its slab and what is on it.
    let (_, got) = call(&c, reqwest::Method::GET, format!("{api}/arrays/{array}"), None).await;
    let mut names: Vec<String> = got["volumes"].as_array().unwrap().iter().map(|v| v["name"].as_str().unwrap().to_string()).collect();
    names.sort();
    assert_eq!(names, vec!["dist-vol", "dist-vol-2"], "{got:#}");
    assert!(got["volumes"].as_array().unwrap().iter().all(|v| v["pinned"] == true));
    assert!(got["slab"]["free_bytes"].as_u64().unwrap() < got["slab"]["total_bytes"].as_u64().unwrap());

    // It cannot be deleted from under them, and deleting an unrelated volume
    // does not change that; once they are gone, it goes and takes its slab.
    let (s, e) = call(&c, reqwest::Method::DELETE, format!("{api}/arrays/{array}"), None).await;
    assert_eq!(s, 409, "{e}");
    assert!(e.to_string().contains("dist-vol"), "{e}");
    for id in [pv.clone(), local.0.to_string()] {
        let (s, e) = call(&c, reqwest::Method::DELETE, format!("{api}/volumes/{id}"), None).await;
        assert!(s == 200 || s == 204, "{e}");
    }
    let (s, e) = call(&c, reqwest::Method::DELETE, format!("{api}/arrays/{array}"), None).await;
    assert_eq!(s, 204, "{e} — the container root on the general pool is no reason to keep the array");
    let (_, slabs) = call(&c, reqwest::Method::GET, format!("{api}/slabs"), None).await;
    assert!(!slabs.to_string().contains(&aslab), "the array's slab is gone: {slabs}");
    // The ordinary volume is untouched.
    let v = state.volume_manager.lock().await.get_volume(&VolumeId(plain.parse().unwrap())).unwrap();
    let mut buf = vec![0u8; MIB as usize];
    v.read(3 * MIB, &mut buf).await.unwrap();
    assert!(buf.iter().all(|&x| x == 4));
}

/// `"dedicated": false` keeps the old behaviour: the array's slab is part of
/// the general pool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shared_array_joins_the_pool() {
    let dir = TempDir::new().unwrap();
    let (_state, base, c) = start(&dir).await;
    let api = format!("{base}/api/v1");
    let (a, _) = open_drive(&c, &api, &dir, "a.img").await;
    let (b, _) = open_drive(&c, &api, &dir, "b.img").await;
    let (s, arr) = call(&c, reqwest::Method::POST, format!("{api}/arrays"), Some(json!({ "level": "Raid1", "drive_uuids": [a, b], "dedicated": false }))).await;
    assert_eq!(s, 201, "{arr}");
    assert_eq!(arr["slab"]["dedicated"], false);
    let (s, v) = call(&c, reqwest::Method::POST, format!("{api}/volumes"), Some(json!({ "name": "anything", "size": "4M" }))).await;
    assert!(s == 200 || s == 201, "{v}");
}
