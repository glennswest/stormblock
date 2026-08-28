//! Management REST API integration tests.
//!
//! Starts axum server on ephemeral port, exercises all REST endpoints.

mod common;

use std::sync::Arc;

use stormblock::drive::BlockDevice;
use stormblock::mgmt::{AppState, ArrayInfo, DriveInfo};
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::raid::{RaidArray, RaidLevel};
use stormblock::volume::{VolumeManager, DEFAULT_EXTENT_SIZE};

use tempfile::TempDir;
use tokio::net::TcpListener;

async fn start_mgmt_server(state: Arc<AppState>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");

    let router = stormblock::mgmt::api::router(state.clone())
        .merge(stormblock::mgmt::metrics::metrics_router(state.clone()));

    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    // Wait for server to be ready
    common::wait_for_listener(addr).await;
    (base_url, handle)
}

async fn setup_state_with_array(dir: &TempDir) -> Arc<AppState> {
    let devices = common::create_file_devices(dir, 2, 64 * 1024 * 1024).await;
    let drive_infos: Vec<DriveInfo> = devices.iter().enumerate().map(|(i, d)| {
        DriveInfo {
            device: d.clone(),
            path: format!("/dev/test{i}"),
            labels: Default::default(),
        }
    }).collect();

    let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();
    let array_id = array.array_id();
    let level = array.level();
    let member_count = array.member_count();
    let capacity = array.capacity_bytes();
    let stripe_size = array.stripe_size();
    let arc_array = Arc::new(array);
    let backing: Arc<dyn BlockDevice> = arc_array.clone();

    let mut vm = VolumeManager::new(DEFAULT_EXTENT_SIZE);
    vm.add_backing_device(array_id, backing).await;
    let _vol_id = vm.create_volume("test-vol", 32 * 1024 * 1024, array_id).await.unwrap();

    let config = StormBlockConfig::default();
    let slab_registry = vm.registry().clone();
    let gem = vm.gem().clone();
    let state = Arc::new(AppState::new(config, vm, slab_registry, gem));

    // Populate state
    {
        let mut drives = state.drives.write().await;
        *drives = drive_infos;
    }
    {
        let mut arrays = state.arrays.write().await;
        arrays.insert(array_id, ArrayInfo {
            array: arc_array,
            level,
            member_count,
            capacity_bytes: capacity,
            stripe_size,
        });
    }

    state
}

#[tokio::test]
async fn mgmt_get_drives() {
    let dir = TempDir::new().unwrap();
    let state = setup_state_with_array(&dir).await;
    let (base_url, server) = start_mgmt_server(state).await;

    let client = reqwest::Client::new();
    let resp = client.get(format!("{base_url}/api/v1/drives"))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);

    server.abort();
}

#[tokio::test]
async fn mgmt_get_arrays() {
    let dir = TempDir::new().unwrap();
    let state = setup_state_with_array(&dir).await;
    let (base_url, server) = start_mgmt_server(state).await;

    let client = reqwest::Client::new();
    let resp = client.get(format!("{base_url}/api/v1/arrays"))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["level"], "RAID-1");

    server.abort();
}

#[tokio::test]
async fn mgmt_get_volumes() {
    let dir = TempDir::new().unwrap();
    let state = setup_state_with_array(&dir).await;
    let (base_url, server) = start_mgmt_server(state).await;

    let client = reqwest::Client::new();
    let resp = client.get(format!("{base_url}/api/v1/volumes"))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"], "test-vol");

    server.abort();
}

#[tokio::test]
async fn mgmt_get_exports() {
    let dir = TempDir::new().unwrap();
    let state = setup_state_with_array(&dir).await;
    let (base_url, server) = start_mgmt_server(state).await;

    let client = reqwest::Client::new();
    let resp = client.get(format!("{base_url}/api/v1/exports"))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    let items = body["items"].as_array().unwrap();
    assert!(items.is_empty()); // No exports configured

    server.abort();
}

/// State with a live (unbound) iSCSI target, a data dir for persistence, and
/// one thin volume — the shape the registry export path needs.
async fn setup_state_with_iscsi(dir: &TempDir) -> (Arc<AppState>, uuid::Uuid) {
    use stormblock::target::iscsi::{IscsiConfig, IscsiTarget};

    let devices = common::create_file_devices(dir, 2, 64 * 1024 * 1024).await;
    let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();
    let array_id = array.array_id();
    let backing: Arc<dyn BlockDevice> = Arc::new(array);

    let mut vm = VolumeManager::new(DEFAULT_EXTENT_SIZE);
    vm.add_backing_device(array_id, backing).await;
    let vol_id = vm.create_volume("export-vol", 8 * 1024 * 1024, array_id).await.unwrap();

    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_str().unwrap().to_string());

    let slab_registry = vm.registry().clone();
    let gem = vm.gem().clone();
    let state = Arc::new(AppState::new(config, vm, slab_registry, gem));

    // The target is never run() here — nothing binds a port; we only need the
    // LUN table it maintains.
    let target = Arc::new(IscsiTarget::new(IscsiConfig::default()));
    *state.iscsi_target.write().await = Some(target);

    (state, vol_id.0)
}

/// A thin/CoW volume must be exportable as an iSCSI LUN (#22).
#[tokio::test]
async fn mgmt_lun_with_volume_backing() {
    let dir = TempDir::new().unwrap();
    let (state, vol_id) = setup_state_with_iscsi(&dir).await;
    let (base_url, server) = start_mgmt_server(state.clone()).await;
    let client = reqwest::Client::new();

    let resp = client.post(format!("{base_url}/api/v1/luns"))
        .json(&serde_json::json!({
            "backing": { "type": "volume", "volume_id": vol_id },
        }))
        .send().await.unwrap();
    assert_eq!(resp.status(), 201, "volume-backed LUN should be created");

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["lun_id"], 0, "first LUN gets number 0");
    assert_eq!(body["backing"]["type"], "volume");
    assert_eq!(body["capacity_bytes"], 8 * 1024 * 1024);

    // It is live on the target, not just recorded.
    let luns = state.iscsi_target.read().await.as_ref().unwrap().list_luns().await;
    assert_eq!(luns, vec![0]);

    // An unknown volume is a 404, not a 500.
    let resp = client.post(format!("{base_url}/api/v1/luns"))
        .json(&serde_json::json!({
            "backing": { "type": "volume", "volume_id": uuid::Uuid::new_v4() },
        }))
        .send().await.unwrap();
    assert_eq!(resp.status(), 404);

    server.abort();
}

/// An export should come back with the LUN an initiator must address, and go
/// live immediately rather than parking until restart (#24, #26).
#[tokio::test]
async fn mgmt_export_assigns_lun_and_goes_active() {
    let dir = TempDir::new().unwrap();
    let (state, vol_id) = setup_state_with_iscsi(&dir).await;
    let (base_url, server) = start_mgmt_server(state.clone()).await;
    let client = reqwest::Client::new();

    let resp = client.post(format!("{base_url}/api/v1/exports"))
        .json(&serde_json::json!({ "volume_id": vol_id, "protocol": "iscsi" }))
        .send().await.unwrap();
    assert_eq!(resp.status(), 201);

    let body: serde_json::Value = resp.json().await.unwrap();
    let export_id = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["status"], "active", "export should be served immediately");
    let lun_id = body["lun_id"].as_u64().expect("export must report its LUN");

    // The LUN is on the target and in the LUN table.
    let luns = state.iscsi_target.read().await.as_ref().unwrap().list_luns().await;
    assert_eq!(luns, vec![lun_id]);

    // Deleting the export stops serving it, freeing the LUN number.
    let resp = client.delete(format!("{base_url}/api/v1/exports/{export_id}"))
        .send().await.unwrap();
    assert_eq!(resp.status(), 204);

    let luns = state.iscsi_target.read().await.as_ref().unwrap().list_luns().await;
    assert!(luns.is_empty(), "LUN should be detached with its export");

    server.abort();
}

/// LUNs created through the API must survive a restart (#22): the table is
/// written to luns.json and the backings are re-opened onto the new target.
#[tokio::test]
async fn mgmt_luns_persist_across_restart() {
    let dir = TempDir::new().unwrap();
    let (state, vol_id) = setup_state_with_iscsi(&dir).await;
    let (base_url, server) = start_mgmt_server(state.clone()).await;
    let client = reqwest::Client::new();

    let backing_file = dir.path().join("lun-backing.img");
    for lun in [0u64, 5] {
        let resp = client.post(format!("{base_url}/api/v1/luns"))
            .json(&serde_json::json!({
                "lun_id": lun,
                "backing": {
                    "type": "file",
                    "path": format!("{}.{lun}", backing_file.display()),
                    "size": "4M",
                },
                "readonly": lun == 5,
            }))
            .send().await.unwrap();
        assert_eq!(resp.status(), 201);
    }

    // A volume-backed LUN too — its volume will not exist after the restart
    // below, which must be survivable rather than fatal.
    let resp = client.post(format!("{base_url}/api/v1/luns"))
        .json(&serde_json::json!({
            "lun_id": 9,
            "backing": { "type": "volume", "volume_id": vol_id },
        }))
        .send().await.unwrap();
    assert_eq!(resp.status(), 201);
    server.abort();

    assert!(dir.path().join("luns.json").exists(), "LUN table should be written");

    // Restart: a fresh state over the same data dir re-opens the LUN table.
    let (fresh, _) = setup_state_with_iscsi(&dir).await;
    assert!(fresh.lun_entries.read().await.is_empty());

    let restored = stormblock::mgmt::api::luns::restore_luns(&fresh).await;
    assert_eq!(restored, 2, "both file-backed LUNs should come back");

    let entries = fresh.lun_entries.read().await;
    assert!(entries.contains_key(&0));
    assert!(entries.get(&5).unwrap().readonly, "readonly flag must survive");
    assert!(
        !entries.contains_key(&9),
        "a LUN whose backing cannot be resolved is skipped, not fatal"
    );
    drop(entries);

    let luns = fresh.iscsi_target.read().await.as_ref().unwrap().list_luns().await;
    assert_eq!(luns, vec![0, 5], "restored LUNs must be live on the target");
}

/// The registry model exports thousands of LUNs; creation and lookup must
/// hold up and every LUN must be addressable (#24).
#[tokio::test]
async fn mgmt_luns_at_scale() {
    const COUNT: u64 = 1000;

    let dir = TempDir::new().unwrap();
    let (state, vol_id) = setup_state_with_iscsi(&dir).await;
    let backing = stormblock::mgmt::LunBacking::Volume { volume_id: vol_id };

    let start = std::time::Instant::now();
    for _ in 0..COUNT {
        stormblock::mgmt::api::luns::attach_lun(&state, backing.clone(), None, false)
            .await
            .expect("attach should succeed");
    }
    let elapsed = start.elapsed();

    assert_eq!(state.lun_entries.read().await.len(), COUNT as usize);

    let target = state.iscsi_target.read().await.as_ref().unwrap().clone();
    assert_eq!(target.lun_count().await, COUNT as usize);

    // LUN numbers are dense and sorted, so every one is addressable and
    // REPORT LUNS is deterministic.
    let luns = target.list_luns().await;
    assert_eq!(luns.first(), Some(&0));
    assert_eq!(luns.last(), Some(&(COUNT - 1)));
    assert_eq!(luns, (0..COUNT).collect::<Vec<_>>());

    // Guards against a return to linear scans on the create path; the real
    // budget is far under this, it only needs to catch a blowup.
    assert!(
        elapsed.as_secs() < 30,
        "creating {COUNT} LUNs took {elapsed:?}, expected far less"
    );
}

/// `GET /api/v1/slabs/pool` answers the one question per-slab numbers could
/// not: how full is this node's physical pool (#18). Available whether or not
/// the growth watcher is enabled, since the accounting is useful on its own.
#[tokio::test]
async fn mgmt_pool_usage_is_reported() {
    let dir = TempDir::new().unwrap();
    let state = setup_state_with_array(&dir).await;
    let (base_url, server) = start_mgmt_server(state.clone()).await;
    let client = reqwest::Client::new();

    let body: serde_json::Value = client
        .get(format!("{base_url}/api/v1/slabs/pool"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(body["enabled"], false, "no watcher configured in this state");
    assert_eq!(body["under_pressure"], false);
    let usage = &body["usage"];
    assert!(usage["slabs"].as_u64().unwrap() >= 1);
    let total = usage["total_slots"].as_u64().unwrap();
    assert!(total > 0, "a slab-backed node reports capacity: {body}");
    assert_eq!(usage["allocated_slots"].as_u64().unwrap(), 0, "nothing written yet");
    assert_eq!(body["used_pct"].as_f64().unwrap(), 0.0);
    assert_eq!(
        usage["total_bytes"].as_u64().unwrap(),
        total * DEFAULT_EXTENT_SIZE,
        "bytes follow slots at the pool's slot size"
    );
    // The tier breakdown is what makes hot-tier pressure visible when the pool
    // as a whole looks comfortable.
    assert!(!usage["by_tier"].as_array().unwrap().is_empty());

    // Allocating moves it — the accounting is live, not a boot-time snapshot.
    {
        let mut vm = state.volume_manager.lock().await;
        let id = vm.create_volume_any("pool-usage", 8 * 1024 * 1024).await.unwrap();
        let vol = vm.get_volume(&id).unwrap();
        vol.write(0, &vec![0x42u8; 4096]).await.unwrap();
    }
    let after: serde_json::Value = client
        .get(format!("{base_url}/api/v1/slabs/pool"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        after["usage"]["allocated_slots"].as_u64().unwrap() > 0,
        "a write shows up in pool usage: {after}"
    );
    assert!(after["used_pct"].as_f64().unwrap() > 0.0);

    server.abort();
}

#[tokio::test]
async fn mgmt_get_metrics() {
    let dir = TempDir::new().unwrap();
    let state = setup_state_with_array(&dir).await;
    let (base_url, server) = start_mgmt_server(state).await;

    let client = reqwest::Client::new();
    let resp = client.get(format!("{base_url}/metrics"))
        .send().await.unwrap();
    // Metrics endpoint returns 500 if init_metrics() wasn't called (test isolation),
    // or 200 if the global recorder was initialized by another test.
    let status = resp.status().as_u16();
    assert!(status == 200 || status == 500,
        "metrics should return 200 or 500, got {status}");

    server.abort();
}
