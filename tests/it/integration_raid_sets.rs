//! A shelf laid out as RAID sets with hot spares, over HTTP (#252): a drive
//! reported failed is replaced by a spare and rebuilt, volumes keep their
//! data, and a restart puts the sets back together from the drives alone.

use crate::common;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::net::TcpListener;

use stormblock::drive::filedev::FileDevice;
use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::{AppState, DriveInfo};
use stormblock::placement::domain::FailureDomain;
use stormblock::volume::{VolumeManager, DEFAULT_EXTENT_SIZE};

const DRIVES: usize = 14;
const DRIVE_BYTES: u64 = 48 * 1024 * 1024;

async fn serve(state: Arc<AppState>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state);
    let h = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    common::wait_for_listener(addr).await;
    (format!("http://{addr}"), h)
}

fn engine() -> Arc<AppState> {
    let vm = VolumeManager::new(DEFAULT_EXTENT_SIZE);
    let reg = vm.registry().clone();
    let gem = vm.gem().clone();
    Arc::new(AppState::new(StormBlockConfig::default(), vm, reg, gem))
}

/// The shelf's drives, opened (again) from their files, labelled by bay.
async fn open_drives(dir: &TempDir, state: &AppState) -> Vec<Arc<dyn BlockDevice>> {
    let mut out: Vec<Arc<dyn BlockDevice>> = Vec::new();
    let mut infos = Vec::new();
    for i in 0..DRIVES {
        let path = dir.path().join(format!("bay-{i:02}.bin"));
        let dev: Arc<dyn BlockDevice> =
            Arc::new(FileDevice::open_with_capacity(path.to_str().unwrap(), DRIVE_BYTES).await.unwrap());
        infos.push(DriveInfo {
            device: dev.clone(),
            path: path.to_str().unwrap().to_string(),
            labels: FailureDomain::from_labels([("shelf", "ds1".to_string()), ("bay", i.to_string())]),
        });
        out.push(dev);
    }
    *state.drives.write().await = infos;
    out
}

fn pattern(seed: u64, len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i as u64).wrapping_mul(2654435761).wrapping_add(seed * 977) >> 11) as u8).collect()
}

async fn volume_bytes(state: &AppState, name: &str, len: usize) -> Vec<u8> {
    let vm = state.volume_manager.lock().await;
    let id = vm.find_volume(name).await.unwrap_or_else(|| panic!("no volume {name}"));
    let v = vm.get_volume(&id).unwrap();
    drop(vm);
    let mut b = vec![0u8; len];
    v.read(0, &mut b).await.unwrap();
    b
}

async fn wait_for(client: &reqwest::Client, url: &str, ok: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..600 {
        let v: Value = client.get(url).send().await.unwrap().json().await.unwrap();
        if ok(&v) {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("never reached the wanted state: {}", client.get(url).send().await.unwrap().text().await.unwrap());
}

#[tokio::test]
async fn a_shelf_survives_a_failed_drive_and_a_restart() {
    let dir = TempDir::new().unwrap();
    let state = engine();
    let drives = open_drives(&dir, &state).await;
    let (base, server) = serve(state.clone()).await;
    let c = reqwest::Client::new();

    // 14 drives: 2 spares, 2 RAID-6 sets of 6.
    let uuids: Vec<String> = drives.iter().map(|d| d.id().uuid.to_string()).collect();
    let r = c
        .post(format!("{base}/api/v1/shelves"))
        .json(&json!({ "name": "ds1", "drive_uuids": uuids, "level": "raid6", "sets": 2, "spares": 2 }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "{}", r.text().await.unwrap());
    let shelf: Value = r.json().await.unwrap();
    assert_eq!(shelf["state"], "clean");
    let sets = shelf["sets"].as_array().unwrap();
    assert_eq!(sets.len(), 2);
    assert_eq!(shelf["spares"].as_array().unwrap().len(), 2);
    assert_eq!(sets[0]["name"], "ds1-a");
    assert_eq!(sets[0]["member_count"], 6);
    assert_eq!(sets[0]["slab"]["dedicated"], false);
    let domain = sets[0]["slab"]["domain"].as_str().unwrap();
    assert!(domain.contains("shelf=ds1") && domain.contains("set=ds1-a"), "{domain}");
    assert_eq!(sets[0]["members"][3]["labels"], "shelf=ds1/bay=3");

    // A drive already in a set is refused for another (#215).
    let r = c
        .post(format!("{base}/api/v1/arrays"))
        .json(&json!({ "level": "raid1", "drive_uuids": [uuids[0], uuids[1]] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);

    // A volume per set, and one mirrored across the two sets.
    for (name, red) in [("plain", None), ("across", Some("mirror:2@set"))] {
        let mut body = json!({ "name": name, "size": "16M" });
        if let Some(red) = red {
            body["redundancy"] = json!(red);
        } else {
            body["array_id"] = sets[0]["id"].clone();
        }
        let r = c.post(format!("{base}/api/v1/volumes")).json(&body).send().await.unwrap();
        assert!(r.status().is_success(), "{name}: {}", r.text().await.unwrap());
    }
    let data = pattern(7, 12 * 1024 * 1024);
    for name in ["plain", "across"] {
        let vm = state.volume_manager.lock().await;
        let id = vm.find_volume(name).await.unwrap();
        let v = vm.get_volume(&id).unwrap();
        drop(vm);
        v.write(0, &data).await.unwrap();
        v.flush().await.unwrap();
    }

    // Bay 2 (set a, slot 2) dies.
    let r = c
        .post(format!("{base}/api/v1/drives/{}/health", uuids[2]))
        .json(&json!({ "state": "failed", "reason": "test" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let rep: Value = r.json().await.unwrap();
    assert_eq!(rep["raid_member"]["failed"], true, "{rep}");
    assert_eq!(rep["raid_member"]["slot"], 2);

    // The shelf's own spare takes the slot and the set rebuilds.
    let set_a = sets[0]["id"].as_str().unwrap().to_string();
    let a = wait_for(&c, &format!("{base}/api/v1/arrays/{set_a}"), |v| {
        v["status"]["state"] == "clean" && v["status"]["rebuild"]["running"] == false
    })
    .await;
    let spare_paths: Vec<String> =
        shelf["spares"].as_array().unwrap().iter().map(|s| s["path"].as_str().unwrap().to_string()).collect();
    let slot2 = a["members"][2]["device_path"].as_str().unwrap().to_string();
    assert!(spare_paths.contains(&slot2), "slot 2 is {slot2}, not a spare");
    let spares: Value = c.get(format!("{base}/api/v1/spares")).send().await.unwrap().json().await.unwrap();
    assert_eq!(spares["count"], 1);
    let health: Value = c.get(format!("{base}/api/v1/health")).send().await.unwrap().json().await.unwrap();
    assert_eq!(health["raid"], "clean");

    for name in ["plain", "across"] {
        assert_eq!(volume_bytes(&state, name, data.len()).await, data, "{name} after the rebuild");
    }

    // A scrub finds the sets consistent.
    let r = c.post(format!("{base}/api/v1/arrays/{set_a}/scrub")).json(&json!({ "repair": false })).send().await.unwrap();
    assert_eq!(r.status(), 202);
    let s = wait_for(&c, &format!("{base}/api/v1/arrays/{set_a}/scrub"), |v| v["running"] == false).await;
    assert_eq!(s["mismatches"], 0, "{s}");

    // Stop cleanly, then start a new engine on the same drive files.
    server.abort();
    for info in state.arrays.read().await.values() {
        info.array.close().await.unwrap();
        info.array.stop();
    }
    drop(state);

    let state = engine();
    let drives = open_drives(&dir, &state).await;
    let (report, claimed) = stormblock::mgmt::raid_sets::assemble_and_adopt(&state, &drives).await;
    assert_eq!(report.arrays.len(), 2, "{report:?}");
    assert!(report.refused.is_empty(), "{report:?}");
    assert_eq!(report.spares.len(), 1);
    assert!(report.volumes_adopted >= 2, "{report:?}");
    assert_eq!(claimed.len(), DRIVES);
    for a in state.arrays.read().await.values() {
        assert_eq!(a.array.status().state, "clean", "{}", a.array.name());
    }
    for name in ["plain", "across"] {
        assert_eq!(volume_bytes(&state, name, data.len()).await, data, "{name} after the restart");
    }
    for info in state.arrays.read().await.values() {
        info.array.stop();
    }
}

#[tokio::test]
async fn a_shelf_layout_that_does_not_fit_is_refused() {
    let dir = TempDir::new().unwrap();
    let state = engine();
    let drives = open_drives(&dir, &state).await;
    let (base, server) = serve(state.clone()).await;
    let c = reqwest::Client::new();
    let uuids: Vec<String> = drives.iter().take(9).map(|d| d.id().uuid.to_string()).collect();
    // 9 drives, 2 spares, 2 RAID-6 sets: a set of 3 is too small.
    let r = c
        .post(format!("{base}/api/v1/shelves"))
        .json(&json!({ "name": "small", "drive_uuids": uuids, "level": "raid6", "sets": 2, "spares": 2 }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert!(state.arrays.read().await.is_empty());
    server.abort();
}
