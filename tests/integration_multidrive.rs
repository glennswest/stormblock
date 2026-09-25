//! A node with several drives, end to end over the API (#142): what the
//! multi-drive design builds on, proved rather than described.
//!
//! Four drives in two shelves; volumes placed across them; a drive fails
//! (health report → quarantine, an automatic rebuild of the mirror, then a
//! drain of what has no redundancy), is emptied and rebuilt around; a drive
//! is added and the pool grows.

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
    config.management.node_name = Some("multi".into());
    let vm = VolumeManager::new(MIB);
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    let state = Arc::new(AppState::new(config, vm, reg, gem));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    common::wait_for_listener(addr).await;
    (state, format!("http://{addr}/api/v1"), reqwest::Client::new())
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

/// Open a drive in a shelf and give it a slab. Returns the drive's id.
async fn add_drive(c: &reqwest::Client, api: &str, dir: &TempDir, name: &str, shelf: &str) -> String {
    let path = dir.path().join(name).to_string_lossy().to_string();
    let (s, d) = call(
        c,
        reqwest::Method::POST,
        format!("{api}/drives"),
        Some(json!({ "path": path, "size_bytes": 64 * MIB, "labels": { "shelf": shelf } })),
    )
    .await;
    assert!(s == 200 || s == 201, "{d}");
    let (s, slab) = call(
        c,
        reqwest::Method::POST,
        format!("{api}/slabs"),
        // No volume record in the slab: a slab that holds the record cannot
        // be drained, and draining is part of the story.
        Some(json!({ "device_path": path, "slot_size": MIB, "metadata_bytes": 0 })),
    )
    .await;
    assert!(s == 200 || s == 201, "{slab}");
    d["uuid"].as_str().unwrap().to_string()
}

async fn placement(c: &reqwest::Client, api: &str, id: VolumeId) -> Value {
    call(c, reqwest::Method::GET, format!("{api}/volumes/{}", id.0), None).await.1["placement"].clone()
}

/// Legs per shelf, from a volume's placement.
fn legs_by_shelf(p: &Value) -> std::collections::BTreeMap<String, u64> {
    let mut m = std::collections::BTreeMap::new();
    for s in p["slabs"].as_array().unwrap() {
        let d = s["domain"].as_str().unwrap();
        let shelf = d.split('/').find_map(|r| r.strip_prefix("shelf=")).unwrap_or("?").to_string();
        *m.entry(shelf).or_insert(0) += s["legs"].as_u64().unwrap();
    }
    m
}

async fn fill(state: &AppState, id: VolumeId, extents: u64, seed: u8) {
    let v = state.volume_manager.lock().await.get_volume(&id).unwrap();
    for i in 0..extents {
        v.write(i * MIB, &vec![seed.wrapping_add(i as u8); MIB as usize]).await.unwrap();
    }
}

async fn check(state: &AppState, id: VolumeId, extents: u64, seed: u8) {
    let v = state.volume_manager.lock().await.get_volume(&id).unwrap();
    for i in 0..extents {
        let mut b = vec![0u8; MIB as usize];
        v.read(i * MIB, &mut b).await.unwrap();
        assert!(b.iter().all(|x| *x == seed.wrapping_add(i as u8)), "extent {i} changed");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_with_several_drives_places_fails_rebuilds_and_grows() {
    let dir = TempDir::new().unwrap();
    let (state, api, c) = start(&dir).await;
    let d0 = add_drive(&c, &api, &dir, "d0.img", "A").await;
    add_drive(&c, &api, &dir, "d1.img", "A").await;
    add_drive(&c, &api, &dir, "d2.img", "B").await;
    add_drive(&c, &api, &dir, "d3.img", "B").await;

    // Every slab names its shelf and its drive.
    let (_, slabs) = call(&c, reqwest::Method::GET, format!("{api}/slabs"), None).await;
    let domains: Vec<&str> = slabs["items"].as_array().unwrap().iter().map(|s| s["domain"].as_str().unwrap()).collect();
    assert_eq!(domains.len(), 4);
    assert!(domains.iter().all(|d| d.contains("shelf=") && d.contains("drive=")), "{domains:?}");

    // A volume with no redundancy spreads: each extent goes to the most-free slab.
    let (s, plain) = call(&c, reqwest::Method::POST, format!("{api}/volumes"), Some(json!({ "name": "plain", "size": "32M", "redundancy": "none" }))).await;
    assert!(s == 200 || s == 201, "{plain}");
    let plain = VolumeId(plain["id"].as_str().unwrap().parse().unwrap());
    fill(&state, plain, 16, 1).await;
    let p = placement(&c, &api, plain).await;
    assert_eq!(p["drives"].as_array().unwrap().len(), 4, "16 extents over four drives: {p:#}");

    // A mirror across shelves: one leg of every extent in each shelf.
    let (s, m) = call(&c, reqwest::Method::POST, format!("{api}/volumes"), Some(json!({ "name": "m", "size": "16M", "redundancy": "mirror:2@shelf" }))).await;
    assert!(s == 200 || s == 201, "{m}");
    let m = VolumeId(m["id"].as_str().unwrap().parse().unwrap());
    fill(&state, m, 8, 50).await;
    let by = legs_by_shelf(&placement(&c, &api, m).await);
    assert_eq!(by.get("A"), Some(&8));
    assert_eq!(by.get("B"), Some(&8));

    // Three copies across two shelves cannot be promised, so it is refused.
    let (s, e) = call(&c, reqwest::Method::POST, format!("{api}/volumes"), Some(json!({ "name": "m3", "size": "16M", "redundancy": "mirror:3@shelf" }))).await;
    assert!(s >= 400, "{e}");

    // Drive 0 fails. Nobody asks for a rebuild (#146): the report quarantines
    // it, rebuilds the mirror from its surviving legs, and then drains what
    // has no redundancy off the drive.
    let (s, h) = call(&c, reqwest::Method::POST, format!("{api}/drives/{d0}/health"), Some(json!({ "state": "failed", "reason": "test" }))).await;
    assert_eq!(s, 200, "{h}");
    let job = h["rebuild"].as_u64().unwrap_or_else(|| panic!("a rebuild was started: {h}"));
    assert_eq!(h["drain_after_rebuild"], true, "{h}");
    let mut rebuilt = Value::Null;
    for _ in 0..800 {
        rebuilt = call(&c, reqwest::Method::GET, format!("{api}/rebuilds/{job}"), None).await.1;
        if rebuilt["state"] != "running" && rebuilt["state"] != "queued" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(rebuilt["state"], "done", "{rebuilt:#}");
    let vols = rebuilt["volumes"].as_array().unwrap();
    assert_eq!(vols.len(), 1, "only the mirror: the plain volume has nothing to rebuild from: {rebuilt:#}");
    assert_eq!(vols[0]["name"], "m");
    assert!(vols[0]["legs_rebuilt"].as_u64().unwrap() > 0);
    let mut drained = Value::Null;
    for _ in 0..800 {
        let (s, d) = call(&c, reqwest::Method::GET, format!("{api}/drives/{d0}/drain"), None).await;
        drained = d;
        if s == 200 && drained["state"] != "running" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(drained["state"], "empty", "{drained}");
    let listing = call(&c, reqwest::Method::GET, format!("{api}/rebuilds"), None).await.1;
    assert_eq!((listing["queued"].as_u64(), listing["running"].as_u64()), (Some(0), Some(0)), "{listing:#}");
    let (s, set) = call(&c, reqwest::Method::PUT, format!("{api}/rebuilds/settings"), Some(json!({ "parallel": 2, "max_bytes_per_sec": 1048576 }))).await;
    assert_eq!(s, 200, "{set}");
    assert_eq!((set["parallel"].as_u64(), set["max_bytes_per_sec"].as_u64()), (Some(2), Some(1048576)));
    let (s, set) = call(&c, reqwest::Method::PUT, format!("{api}/rebuilds/settings"), Some(json!({ "max_bytes_per_sec": 0 }))).await;
    assert_eq!((s, set["max_bytes_per_sec"].as_u64()), (200, Some(0)));

    // The mirror is rebuilt around it; nothing of either volume is left on it.
    let d0_path = dir.path().join("d0.img").to_string_lossy().to_string();
    for (id, name) in [(m, "m"), (plain, "plain")] {
        let p = placement(&c, &api, id).await;
        assert!(
            p["slabs"].as_array().unwrap().iter().all(|s| s["drive"]["path"] != d0_path.as_str()),
            "{name} still has legs on the failed drive: {p:#}"
        );
    }
    let p = placement(&c, &api, m).await;
    assert_eq!(p["legs"]["missing"], 0, "{p:#}");
    assert_eq!(p["legs"]["health"], "healthy");
    let by = legs_by_shelf(&p);
    assert_eq!((by.get("A"), by.get("B")), (Some(&8), Some(&8)), "still one leg per shelf: {by:?}");
    check(&state, plain, 16, 1).await;
    check(&state, m, 8, 50).await;

    // A drive is added in a third shelf: the pool grows by it.
    let before = call(&c, reqwest::Method::GET, format!("{api}/slabs/pool"), None).await.1["usage"]["total_bytes"].as_u64().unwrap();
    add_drive(&c, &api, &dir, "d4.img", "C").await;
    let after = call(&c, reqwest::Method::GET, format!("{api}/slabs/pool"), None).await.1["usage"]["total_bytes"].as_u64().unwrap();
    assert!(after >= before + 60 * MIB, "{before} -> {after}");
    // …and now three shelves can hold three copies.
    let (s, e) = call(&c, reqwest::Method::POST, format!("{api}/volumes"), Some(json!({ "name": "m3b", "size": "16M", "redundancy": "mirror:3@shelf" }))).await;
    assert!(s == 200 || s == 201, "{e}");
}
