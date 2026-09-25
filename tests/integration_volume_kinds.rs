//! The Volumes and Images views (#138, #126): every volume says what it is,
//! whether something is using it and who; the listing filters on it.

mod common;

use std::sync::Arc;

use stormblock::drive::filedev::FileDevice;
use stormblock::drive::slab::Slab;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::{AppState, ExportEntry, ExportProtocol, ExportStatus};
use stormblock::placement::topology::StorageTier;
use stormblock::volume::metadata::FsInfo;
use stormblock::volume::{VolumeId, VolumeManager};

use serde_json::Value;
use tempfile::TempDir;
use tokio::net::TcpListener;

const SLOT: u64 = 4096;

async fn serve(dir: &TempDir, vm: VolumeManager) -> (Arc<AppState>, String, tokio::task::JoinHandle<()>) {
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_string_lossy().to_string());
    let reg = vm.registry().clone();
    let gem = vm.gem().clone();
    let state = Arc::new(AppState::new(config, vm, reg, gem));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state.clone());
    let h = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    common::wait_for_listener(addr).await;
    (state, format!("http://{addr}"), h)
}

fn fs(kind: &str) -> FsInfo {
    let mut f = FsInfo::vfat("x");
    f.kind = kind.into();
    f
}

async fn names(c: &reqwest::Client, url: String) -> Vec<String> {
    let v: Value = c.get(url).send().await.unwrap().json().await.unwrap();
    let mut n: Vec<String> = v["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap().to_string()).collect();
    n.sort();
    n
}

#[tokio::test]
async fn volumes_say_what_they_are_who_uses_them_and_the_listing_filters_on_it() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("slab.bin").to_string_lossy().to_string();
    let dev = FileDevice::open_with_capacity(&path, 16 * 1024 * 1024).await.unwrap();
    let mut vm = VolumeManager::new(SLOT);
    vm.add_slab(Slab::format(Arc::new(dev), SLOT, StorageTier::Hot).await.unwrap()).await;

    let golden = vm.create_volume_any("stormpump.golden", 1 << 20).await.unwrap();
    vm.seal_volume(golden, None).await.unwrap();
    let blank = vm.create_volume_any("pvc-ext4j-1g", 1 << 20).await.unwrap();
    vm.seal_volume(blank, None).await.unwrap();
    vm.mark_template(blank);
    let media = vm.create_volume_any("fedora-44-x86_64", 1 << 20).await.unwrap();
    vm.seal_volume(media, Some(fs("gpt"))).await.unwrap();
    let db = vm.create_volume_any("pvc-db", 1 << 20).await.unwrap();
    let root = vm.create_volume_any("stormpump", 1 << 20).await.unwrap();
    let idle = vm.create_volume_any("left-over", 1 << 20).await.unwrap();
    let _ = idle;
    let (state, base, server) = serve(&dir, vm).await;
    let c = reqwest::Client::new();

    // pvc-db is served over NVMe/TCP and belongs to a claim; stormpump is a
    // boot device the engine adopted.
    state.exports.write().await.push(ExportEntry {
        id: uuid::Uuid::new_v4(),
        volume_id: db.0,
        protocol: ExportProtocol::Nvmeof,
        target_id: "nqn.2026-09.lo.test:node".into(),
        status: ExportStatus::Active,
        lun_id: None,
        nsid: Some(3),
    });
    state.ublk_exports.lock().await.record_adopted(&root.0.to_string(), "/dev/ublkb0".into());
    let r = c
        .put(format!("{base}/api/v1/volumes/{}/owner", db.0))
        .json(&serde_json::json!({ "owner": { "kind": "PersistentVolumeClaim", "namespace": "shop", "name": "db" } }))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.status());

    let all: Value = c.get(format!("{base}/api/v1/volumes")).send().await.unwrap().json().await.unwrap();
    let by = |n: &str| all["items"].as_array().unwrap().iter().find(|v| v["name"] == n).unwrap().clone();
    assert_eq!(all["count"], 6, "no filter lists everything, as before");
    assert_eq!(by("stormpump.golden")["kind"], "golden");
    assert_eq!(by("pvc-ext4j-1g")["kind"], "blank");
    assert_eq!(by("fedora-44-x86_64")["kind"], "media");
    assert_eq!(by("pvc-db")["kind"], "volume");

    let d = by("pvc-db");
    assert_eq!(d["in_use"], true);
    assert_eq!(d["attachments"][0]["transport"], "nvme-tcp");
    assert_eq!(d["attachments"][0]["nsid"], 3);
    assert_eq!(d["consumer"]["kind"], "PersistentVolumeClaim");
    assert_eq!(d["consumer"]["namespace"], "shop");
    assert_eq!(d["consumer"]["name"], "db");
    let r = by("stormpump");
    assert_eq!(r["in_use"], true, "an adopted boot device is in use");
    assert_eq!(r["attachments"][0]["transport"], "ublk");
    assert_eq!(r["attachments"][0]["device"], "/dev/ublkb0");
    let l = by("left-over");
    assert_eq!(l["in_use"], false);
    assert!(l.get("attachments").is_none() && l.get("consumer").is_none());

    // The single GET says the same.
    let one: Value = c.get(format!("{base}/api/v1/volumes/{}", db.0)).send().await.unwrap().json().await.unwrap();
    assert_eq!(one["kind"], "volume");
    assert_eq!(one["consumer"]["name"], "db");

    // The console's two views, and the rest of the filters.
    assert_eq!(
        names(&c, format!("{base}/api/v1/volumes?kind=volume&in_use=true")).await,
        vec!["pvc-db", "stormpump"],
        "the Volumes view: what running things use"
    );
    assert_eq!(
        names(&c, format!("{base}/api/v1/volumes?kind=image")).await,
        vec!["fedora-44-x86_64", "pvc-ext4j-1g", "stormpump.golden"],
        "the Images view"
    );
    assert_eq!(names(&c, format!("{base}/api/v1/volumes?kind=golden,media")).await, vec!["fedora-44-x86_64", "stormpump.golden"]);
    assert_eq!(names(&c, format!("{base}/api/v1/volumes?kind=all")).await.len(), 6);
    assert_eq!(
        names(&c, format!("{base}/api/v1/volumes?kind=volume&in_use=false")).await,
        vec!["left-over"],
        "what nothing uses"
    );
    let unowned = names(&c, format!("{base}/api/v1/volumes?unowned=true")).await;
    assert!(!unowned.contains(&"pvc-db".to_string()) && unowned.len() == 5, "{unowned:?}");
    let _ = VolumeId(uuid::Uuid::nil());
    server.abort();
}
