//! A release and the volume behind it (#106).
//!
//! The module's own rule is "a version somebody can find, a link they can pull
//! it from, a manifest saying what went into it, and notes saying what changed
//! — all four or none". Eight releases on forge had three of the four: their
//! volumes had been reclaimed, and nothing said so, so the index offered
//! downloads that could not happen and manifests describing bytes that were
//! gone.
//!
//! Two halves, tested here: the volume cannot be deleted while a release names
//! it, and a release whose volume went anyway says so rather than failing
//! obscurely.

mod common;

use std::sync::Arc;

use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::raid::{RaidArray, RaidLevel};
use stormblock::volume::{VolumeId, VolumeManager};

use tempfile::TempDir;
use tokio::net::TcpListener;

const SLOT: u64 = 1024 * 1024;

/// A release index needs somewhere to live, so this state has a data dir.
async fn setup(dir: &TempDir) -> Arc<AppState> {
    let devices = common::create_file_devices(dir, 2, 64 * 1024 * 1024).await;
    let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();
    let array_id = array.array_id();
    let backing: Arc<dyn BlockDevice> = Arc::new(array);

    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let mut vm = VolumeManager::with_data_dir(SLOT, data.clone()).unwrap();
    vm.add_backing_device(array_id, backing).await;

    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(data.to_string_lossy().to_string());
    let slab_registry = vm.registry().clone();
    let gem = vm.gem().clone();
    Arc::new(AppState::new(config, vm, slab_registry, gem))
}

async fn serve(state: Arc<AppState>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state);
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    common::wait_for_listener(addr).await;
    (format!("http://{addr}"), handle)
}

async fn create_volume(base: &str, c: &reqwest::Client, name: &str) -> String {
    let resp = c
        .post(format!("{base}/api/v1/volumes"))
        .json(&serde_json::json!({ "name": name, "size": "8M" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "create {name}");
    let body: serde_json::Value = resp.json().await.unwrap();
    body["id"].as_str().unwrap().to_string()
}

async fn publish(base: &str, c: &reqwest::Client, version: &str, volume: &str) {
    let resp = c
        .post(format!("{base}/api/v1/releases"))
        .json(&serde_json::json!({
            "version": version,
            "volume": volume,
            "notes": "the one with the fix in it",
            "manifest": [{
                "kind": "golden",
                "name": "stormcos",
                "digest": "sha256:c0ffee0000000000000000000000000000000000000000000000000000000000",
            }],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "publish {version}");
}

/// The half that stops it happening: withdrawing the release is a deliberate
/// act, and deleting the volume under it is not.
#[tokio::test]
async fn a_volume_a_release_names_cannot_be_deleted() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let (base, server) = serve(state).await;
    let c = reqwest::Client::new();

    let id = create_volume(&base, &c, "image-10-25").await;
    publish(&base, &c, "10.25", &id).await;

    let resp = c.delete(format!("{base}/api/v1/volumes/{id}")).send().await.unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    let msg = body["error"].as_str().unwrap();
    assert!(msg.contains("10.25"), "the refusal must name the release: {msg}");
    assert!(msg.contains("unpublish"), "and say what to do about it: {msg}");

    // force=true is for a dangling synonym, not for orphaning a release.
    let resp = c
        .delete(format!("{base}/api/v1/volumes/{id}?force=true"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);

    // Withdraw the release, and the volume is ordinary again.
    let resp = c.delete(format!("{base}/api/v1/releases/10.25")).send().await.unwrap();
    assert_eq!(resp.status(), 204);
    let resp = c.delete(format!("{base}/api/v1/volumes/{id}")).send().await.unwrap();
    assert_eq!(resp.status(), 204);

    server.abort();
}

/// The half that is honest about the ones already orphaned: the record stands,
/// the state says `archived`, and the download says Gone rather than 404.
#[tokio::test]
async fn a_release_whose_volume_went_reports_archived_and_410() {
    let dir = TempDir::new().unwrap();
    let state = setup(&dir).await;
    let (base, server) = serve(state.clone()).await;
    let c = reqwest::Client::new();

    let id = create_volume(&base, &c, "image-10-26").await;
    publish(&base, &c, "10.26", &id).await;

    // Available while the volume is there.
    let body: serde_json::Value = c
        .get(format!("{base}/api/v1/releases"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["items"][0]["state"], "available");
    let resp = c
        .get(format!("{base}/api/v1/releases/10.26/image.img"))
        .header("Range", "bytes=0-511")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 206);

    // Now take the volume away behind the guard's back — which is how the
    // eight on forge came to exist, before anything checked.
    {
        let mut vm = state.volume_manager.lock().await;
        vm.delete_volume(VolumeId(id.parse().unwrap())).await.unwrap();
    }

    let body: serde_json::Value = c
        .get(format!("{base}/api/v1/releases"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["items"][0]["state"], "archived");
    assert_eq!(body["items"][0]["size_human"], "—");

    let body: serde_json::Value = c
        .get(format!("{base}/api/v1/releases/10.26"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["state"], "archived");

    // The download is Gone, not Not Found: the release is right there.
    let resp = c.get(format!("{base}/api/v1/releases/10.26/image.img")).send().await.unwrap();
    assert_eq!(resp.status(), 410);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["state"], "archived");
    assert!(body["error"].as_str().unwrap().contains("archived"));

    // The record is the point of keeping it: the manifest and notes still
    // answer, so what 10.26 contained is still on the record.
    let resp = c.get(format!("{base}/api/v1/releases/10.26/manifest")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["count"], 1);
    let resp = c.get(format!("{base}/api/v1/releases/10.26/notes")).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    // A version that really does not exist is still a 404 — the two answers
    // stay different.
    let resp = c.get(format!("{base}/api/v1/releases/9.99/image.img")).send().await.unwrap();
    assert_eq!(resp.status(), 404);

    // The browser index drops the link rather than offering one that 410s.
    let page = c
        .get(format!("{base}/api/v1/releases/index.html"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("archived"), "the index must say so: {page}");
    assert!(!page.contains("image.img"), "and not offer the download: {page}");

    server.abort();
}
