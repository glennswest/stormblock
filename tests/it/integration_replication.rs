//! `Replication` (#656) between two engines over HTTP: the peer serves its
//! listing, legs and content as it always has; the replica copies a golden
//! whole and a clone of it as a clone of its own copy, only what the clone
//! holds, and the bytes come out the same.

use crate::common;
use std::sync::Arc;

use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::volume::{VolumeId, VolumeManager, DEFAULT_EXTENT_SIZE};

use tempfile::TempDir;
use tokio::net::TcpListener;

async fn engine(dir: &TempDir) -> Arc<AppState> {
    let devices = common::create_file_devices(dir, 1, 1024 * 1024 * 1024).await;
    let mut vm = VolumeManager::new(DEFAULT_EXTENT_SIZE);
    vm.add_backing_device(stormblock::raid::RaidArrayId(uuid::Uuid::new_v4()), devices[0].clone()).await;
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_str().unwrap().to_string());
    config.management.ublk_transport = false;
    let slab_registry = vm.registry().clone();
    let gem = vm.gem().clone();
    Arc::new(AppState::new(config, vm, slab_registry, gem))
}

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

async fn put(state: &AppState, id: VolumeId, at: u64, byte: u8, len: usize) {
    let dev = state.volume_manager.lock().await.get_volume(&id).unwrap();
    dev.write(at, &vec![byte; len]).await.unwrap();
    dev.flush().await.unwrap();
}

async fn all_of(state: &AppState, id: VolumeId) -> Vec<u8> {
    let dev = state.volume_manager.lock().await.get_volume(&id).unwrap();
    let mut b = vec![0u8; dev.capacity_bytes() as usize];
    dev.read(0, &mut b).await.unwrap();
    b
}

async fn by_name(state: &AppState, name: &str) -> Option<VolumeId> {
    state.volume_manager.lock().await.find_volume(name).await
}

#[tokio::test]
async fn a_golden_and_its_clone_replicated() {
    let (da, db) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let (peer, here) = (engine(&da).await, engine(&db).await);
    let x = DEFAULT_EXTENT_SIZE;
    // The peer: a golden with data in extents 0 and 2, and a clone of it
    // that rewrote extent 2 and wrote extent 3, both sealed.
    let golden = {
        let mut vm = peer.volume_manager.lock().await;
        vm.create_volume_any("golden-base-aaaa", 4 * x).await.unwrap()
    };
    put(&peer, golden, 0, 0x11, 4096).await;
    put(&peer, golden, 2 * x + 8192, 0x22, 4096).await;
    peer.volume_manager.lock().await.seal_volume(golden, None).await.unwrap();
    let clone = peer.volume_manager.lock().await.create_snapshot(golden, "golden-app-bbbb").await.unwrap();
    put(&peer, clone, 2 * x, 0x33, 4096).await;
    put(&peer, clone, 3 * x, 0x44, 4096).await;
    peer.volume_manager.lock().await.seal_volume(clone, None).await.unwrap();
    // Something the default selection leaves alone: writable.
    let _scratch = peer.volume_manager.lock().await.create_volume_any("scratch", x).await.unwrap();

    let (purl, ph) = serve(peer.clone()).await;
    let (hurl, hh) = serve(here.clone()).await;
    let c = reqwest::Client::new();
    let made = c
        .post(format!("{hurl}/apis/storage.storm.io/v1/replications"))
        .json(&serde_json::json!({ "metadata": { "name": "forge" }, "spec": { "from": { "engine": purl }, "what": ["goldens"] } }))
        .send()
        .await
        .unwrap();
    assert_eq!(made.status(), 201, "{}", made.text().await.unwrap());

    let mut last = String::new();
    for _ in 0..200 {
        last = c.get(format!("{hurl}/apis/storage.storm.io/v1/replications/forge")).send().await.unwrap().text().await.unwrap();
        if last.contains(r#""phase":"Synced""#) || last.contains(r#""phase":"Failed""#) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(last.contains(r#""phase":"Synced","items":{"total":2,"done":2,"failed":0}"#), "{last}");

    let g = by_name(&here, "golden-base-aaaa").await.expect("the golden is here");
    let k = by_name(&here, "golden-app-bbbb").await.expect("the clone is here");
    assert!(by_name(&here, "scratch").await.is_none(), "a writable volume is not taken by default");
    {
        let vm = here.volume_manager.lock().await;
        assert!(vm.is_sealed(&g) && vm.is_sealed(&k));
        assert_eq!(vm.parent(&k), Some(g), "the clone is a clone of the golden's copy here");
    }
    assert!(all_of(&here, g).await == all_of(&peer, golden).await, "the golden's bytes");
    assert!(all_of(&here, k).await == all_of(&peer, clone).await, "the clone's bytes");

    // Again: nothing is copied twice, and it is still Synced.
    let again = c
        .put(format!("{hurl}/apis/storage.storm.io/v1/replications/forge"))
        .json(&serde_json::json!({ "spec": { "from": { "engine": purl }, "what": ["goldens"], "select": { "names": ["golden-*"] } } }))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 200);
    for _ in 0..200 {
        last = c.get(format!("{hurl}/apis/storage.storm.io/v1/replications/forge")).send().await.unwrap().text().await.unwrap();
        if last.contains(r#""observedGeneration":2"#) && last.contains(r#""phase":"Synced""#) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(last.contains(r#""bytes":{"total":"#) && last.contains(r#""phase":"Synced","items":{"total":2,"done":2"#), "{last}");
    let count = here.volume_manager.lock().await.volume_ids().len();
    assert_eq!(count, 2, "nothing made twice");
    let list: serde_json::Value = c.get(format!("{hurl}/apis/storage.storm.io/v1/replications")).send().await.unwrap().json().await.unwrap();
    assert_eq!(list["kind"], "ReplicationList");
    assert_eq!(list["items"][0]["metadata"]["name"], "forge");
    let bad = c
        .post(format!("{hurl}/apis/storage.storm.io/v1/replications"))
        .json(&serde_json::json!({ "metadata": { "name": "bad" }, "spec": { "from": {}, "mode": "once" } }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 422, "no peer engine");
    let gone = c.delete(format!("{hurl}/apis/storage.storm.io/v1/replications/forge")).send().await.unwrap();
    assert_eq!(gone.status(), 200);
    assert!(by_name(&here, "golden-app-bbbb").await.is_some(), "deleting the Replication deletes nothing it copied");
    ph.abort();
    hh.abort();
}
