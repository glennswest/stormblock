//! A volume moved onto a named drive, slab or array, or off one, while it is
//! written, with progress read back (#426, stormvm#29): and its new extents
//! keep going where it was moved.

use crate::common;
use std::collections::BTreeSet;
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
    config.management.node_name = Some("mover".into());
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

/// A drive in a shelf with a slab on it: (drive id, slab id).
async fn add_drive(c: &reqwest::Client, api: &str, dir: &TempDir, name: &str, shelf: &str) -> (String, String) {
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
        Some(json!({ "device_path": path, "slot_size": MIB, "metadata_bytes": 0 })),
    )
    .await;
    assert!(s == 200 || s == 201, "{slab}");
    let slab_id = slab["id"].as_str().or(slab["slab_id"].as_str()).map(String::from).unwrap_or_else(|| panic!("{slab}"));
    (d["uuid"].as_str().unwrap().to_string(), slab_id)
}

/// The slabs a volume's data legs are on.
async fn slabs_of(c: &reqwest::Client, api: &str, id: VolumeId) -> BTreeSet<String> {
    let (_, v) = call(c, reqwest::Method::GET, format!("{api}/volumes/{}", id.0), None).await;
    v["placement"]["slabs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["legs"].as_u64().unwrap_or(0) + s["parity_legs"].as_u64().unwrap_or(0) > 0)
        .map(|s| s["id"].as_str().unwrap().to_string())
        .collect()
}

async fn wait_done(c: &reqwest::Client, api: &str, id: VolumeId) -> Value {
    for _ in 0..1000 {
        let (s, m) = call(c, reqwest::Method::GET, format!("{api}/volumes/{}/move", id.0), None).await;
        assert_eq!(s, 200, "{m}");
        if m["move"]["state"] != "running" {
            return m["move"].clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the move did not end");
}

async fn write(state: &AppState, id: VolumeId, extents: std::ops::Range<u64>, seed: u8) {
    let v = state.volume_manager.lock().await.get_volume(&id).unwrap();
    for i in extents {
        v.write(i * MIB, &vec![seed.wrapping_add(i as u8); MIB as usize]).await.unwrap();
    }
}

async fn check(state: &AppState, id: VolumeId, extents: std::ops::Range<u64>, seed: u8) {
    let v = state.volume_manager.lock().await.get_volume(&id).unwrap();
    for i in extents {
        let mut b = vec![0u8; MIB as usize];
        v.read(i * MIB, &mut b).await.unwrap();
        assert!(b.iter().all(|x| *x == seed.wrapping_add(i as u8)), "extent {i} changed");
    }
}

fn vid(v: &Value) -> VolumeId {
    VolumeId(v["id"].as_str().unwrap().parse().unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_volume_moves_onto_a_named_drive_while_written_and_stays_there() {
    let dir = TempDir::new().unwrap();
    let (state, api, c) = start(&dir).await;
    let (d0, s0) = add_drive(&c, &api, &dir, "d0.img", "A").await;
    let (d1, s1) = add_drive(&c, &api, &dir, "d1.img", "A").await;
    let (d2, s2) = add_drive(&c, &api, &dir, "d2.img", "B").await;
    let drive_of = |slab: &str| -> String {
        if slab == s0 { d0.clone() } else if slab == s1 { d1.clone() } else { d2.clone() }
    };

    // A VM's disk: one drive (#153, affine), 16 extents written.
    let (s, v) = call(&c, reqwest::Method::POST, format!("{api}/volumes"), Some(json!({ "name": "vm-disk", "size": "32M" }))).await;
    assert!(s == 200 || s == 201, "{v}");
    let disk = vid(&v);
    write(&state, disk, 0..16, 1).await;
    let from = slabs_of(&c, &api, disk).await;
    assert_eq!(from.len(), 1, "{from:?}");
    let from = from.into_iter().next().unwrap();
    let to_slab = [&s0, &s1, &s2].into_iter().find(|s| **s != from).unwrap().clone();
    let to_drive = drive_of(&to_slab);

    // Nothing moving yet.
    let (s, _) = call(&c, reqwest::Method::GET, format!("{api}/volumes/{}/move", disk.0), None).await;
    assert_eq!(s, 404);
    let (s, _) = call(&c, reqwest::Method::DELETE, format!("{api}/volumes/{}/move", disk.0), None).await;
    assert_eq!(s, 404);
    // Named wrong: refused, nothing set.
    let (s, e) = call(&c, reqwest::Method::POST, format!("{api}/volumes/{}/move", disk.0), Some(json!({ "to": { "drive": "/no/such" } }))).await;
    assert_eq!(s, 404, "{e}");
    let (s, e) = call(&c, reqwest::Method::POST, format!("{api}/volumes/{}/move", disk.0), Some(json!({ "to": { "drive": to_drive, "slab": to_slab } }))).await;
    assert_eq!(s, 400, "{e}");
    let (s, e) = call(&c, reqwest::Method::POST, format!("{api}/volumes/{}/move", disk.0), Some(json!({}))).await;
    assert_eq!(s, 400, "{e}");
    assert!(stormblock::volume::target::of(&disk).is_none());

    // Onto the drive, by name, while a writer rewrites every extent.
    let writer = {
        let state = state.clone();
        tokio::spawn(async move {
            write(&state, disk, 0..16, 50).await;
        })
    };
    let (s, m) = call(&c, reqwest::Method::POST, format!("{api}/volumes/vm-disk/move"), Some(json!({ "to": { "drive": to_drive } }))).await;
    assert_eq!(s, 202, "{m}");
    assert!(m["move"]["to_move"].as_u64().unwrap() > 0 || m["move"]["state"] == "done", "{m}");
    let done = wait_done(&c, &api, disk).await;
    writer.await.unwrap();
    assert_eq!(done["state"], "done", "{done:#}");
    assert_eq!(done["remaining"], 0, "{done:#}");
    assert_eq!(done["failed"], 0, "{done:#}");
    assert!(done["moved"].as_u64().unwrap() >= 1, "{done:#}");
    assert!(done["target"].as_str().unwrap().contains(&to_drive), "{done:#}");
    check(&state, disk, 0..16, 50).await;
    assert_eq!(slabs_of(&c, &api, disk).await, [to_slab.clone()].into());

    // The volume says where it is going, and its new extents keep there.
    let (_, got) = call(&c, reqwest::Method::GET, format!("{api}/volumes/{}", disk.0), None).await;
    assert_eq!(got["target"]["to"]["drive"], to_drive, "{got:#}");
    assert_eq!(got["move"]["state"], "done");
    write(&state, disk, 16..28, 7).await;
    check(&state, disk, 16..28, 7).await;
    assert_eq!(slabs_of(&c, &api, disk).await, [to_slab.clone()].into(), "new extents stay on the drive");
    // A tier move would undo it: refused while the target stands.
    let (s, e) = call(&c, reqwest::Method::POST, format!("{api}/volumes/{}/tier", disk.0), Some(json!({ "tier": "hot" }))).await;
    assert_eq!(s, 409, "{e}");

    // Off that drive ("off sdc"): everything leaves it and nothing comes back.
    let (s, m) = call(&c, reqwest::Method::POST, format!("{api}/volumes/{}/move", disk.0), Some(json!({ "except": { "drive": to_drive } }))).await;
    assert_eq!(s, 202, "{m}");
    let done = wait_done(&c, &api, disk).await;
    assert_eq!(done["state"], "done", "{done:#}");
    assert_eq!(done["to_move"], 28, "{done:#}");
    let now = slabs_of(&c, &api, disk).await;
    assert!(!now.contains(&to_slab), "{now:?}");
    write(&state, disk, 28..32, 9).await;
    assert!(!slabs_of(&c, &api, disk).await.contains(&to_slab));
    check(&state, disk, 0..16, 50).await;
    check(&state, disk, 16..28, 7).await;
    check(&state, disk, 28..32, 9).await;

    // Released: the target goes, and with it the refusal of a retier.
    let (s, r) = call(&c, reqwest::Method::DELETE, format!("{api}/volumes/{}/move?release=true", disk.0), None).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["released"], true);
    assert!(stormblock::volume::target::of(&disk).is_none());
    let (_, got) = call(&c, reqwest::Method::GET, format!("{api}/volumes/{}", disk.0), None).await;
    assert!(got.get("target").is_none(), "{got:#}");

    // A mirror cannot be moved onto one drive: its legs stay apart.
    let (s, m) = call(&c, reqwest::Method::POST, format!("{api}/volumes"), Some(json!({ "name": "m", "size": "8M", "redundancy": "mirror:2" }))).await;
    assert!(s == 200 || s == 201, "{m}");
    let mirror = vid(&m);
    let (s, e) = call(&c, reqwest::Method::POST, format!("{api}/volumes/{}/move", mirror.0), Some(json!({ "to": { "slab": s0 } }))).await;
    assert_eq!(s, 409, "{e}");
    // Off one drive, it can: each leg leaves it for a drive apart from the other.
    write(&state, mirror, 0..8, 3).await;
    let (s, e) = call(&c, reqwest::Method::POST, format!("{api}/volumes/{}/move", mirror.0), Some(json!({ "except": { "slab": s0 } }))).await;
    assert_eq!(s, 202, "{e}");
    let done = wait_done(&c, &api, mirror).await;
    assert_eq!(done["state"], "done", "{done:#}");
    assert_eq!(slabs_of(&c, &api, mirror).await, [s1.clone(), s2.clone()].into());
    check(&state, mirror, 0..8, 3).await;

    // Asked at creation (stormstorage#106): every extent on the slab named.
    let (s, v) = call(
        &c,
        reqwest::Method::POST,
        format!("{api}/volumes"),
        Some(json!({ "name": "leg", "size": "8M", "target": { "to": { "slab": s2 } } })),
    )
    .await;
    assert!(s == 200 || s == 201, "{v}");
    let leg = vid(&v);
    write(&state, leg, 0..8, 11).await;
    assert_eq!(slabs_of(&c, &api, leg).await, [s2.clone()].into());
    // A target that cannot be had is no volume at all.
    let (s, e) = call(
        &c,
        reqwest::Method::POST,
        format!("{api}/volumes"),
        Some(json!({ "name": "nope", "size": "8M", "redundancy": "mirror:2", "target": { "to": { "slab": s2 } } })),
    )
    .await;
    assert_eq!(s, 409, "{e}");
    let (_, list) = call(&c, reqwest::Method::GET, format!("{api}/volumes"), None).await;
    assert!(!list.to_string().contains("\"nope\""), "{list}");

    // Kept across a restart: read back from the data directory.
    stormblock::volume::target::set(leg, None);
    stormblock::volume::target::load(dir.path());
    let t = stormblock::volume::target::of(&leg).expect("the target came back");
    assert_eq!(t.to.unwrap().into_iter().map(|s| s.0.to_string()).collect::<Vec<_>>(), vec![s2.clone()]);

    // Deleted: its target goes with it.
    let (s, _) = call(&c, reqwest::Method::DELETE, format!("{api}/volumes/{}", leg.0), None).await;
    assert!(s == 200 || s == 204, "{s}");
    assert!(stormblock::volume::target::of(&leg).is_none());

    // `/v1` create, as stormstorage#106 sends it: a slab or a drive by name.
    let root = api.trim_end_matches("/api/v1").to_string();
    let v1 = |name: &str, size: u64, placement: Value| json!({ "name": name, "size_bytes": size, "replica_tier": { "slaves": 0 }, "placement": placement });
    let (s, v) = call(&c, reqwest::Method::POST, format!("{root}/v1/volumes"), Some(v1("leg-slab", 8 * MIB, json!({ "slab_id": s1 })))).await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = call(&c, reqwest::Method::POST, format!("{root}/v1/volumes"), Some(v1("leg-drive", 8 * MIB, json!({ "drive": d0 })))).await;
    assert_eq!(s, 200, "{v}");
    for (name, slab, seed) in [("leg-slab", &s1, 21u8), ("leg-drive", &s0, 31)] {
        let id = state.volume_manager.lock().await.find_volume(name).await.unwrap();
        write(&state, id, 0..8, seed).await;
        check(&state, id, 0..8, seed).await;
        assert_eq!(slabs_of(&c, &api, id).await, [slab.clone()].into(), "{name}");
    }
    let refused = [
        (json!({ "slab_id": uuid::Uuid::new_v4().to_string() }), 8 * MIB, 404),
        (json!({ "drive": "no-such-serial" }), 8 * MIB, 404),
        (json!({ "slab_id": s1, "drive": d0 }), 8 * MIB, 400),
        (json!({ "shelf": "A" }), 8 * MIB, 400),
        // More than the slab (a 64 MiB drive), less than the node: the slab's 409.
        (json!({ "slab_id": s1 }), 80 * MIB, 409),
        // More than the whole node: its 507, as before.
        (json!({ "slab_id": s1 }), 1024 * MIB, 507),
    ];
    for (i, (placement, size, want)) in refused.into_iter().enumerate() {
        let name = format!("refused-{i}");
        let (s, e) = call(&c, reqwest::Method::POST, format!("{root}/v1/volumes"), Some(v1(&name, size, placement.clone()))).await;
        assert_eq!(s, want, "{placement}: {e}");
        assert!(state.volume_manager.lock().await.find_volume(&name).await.is_none(), "{name} left behind");
    }
}
