//! Where a volume lives (#136, #114): slabs, drives, the state of each leg,
//! and a generation a mirror can ask "changed?" of.

use crate::common;
use std::sync::Arc;

use stormblock::drive::filedev::FileDevice;
use stormblock::drive::partition::PartitionDevice;
use stormblock::drive::slab::Slab;
use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::placement::topology::StorageTier;
use stormblock::volume::redundancy::RedundancyPolicy;
use stormblock::volume::{CreateOptions, VolumeManager};

use tempfile::TempDir;
use tokio::net::TcpListener;

const SLOT: u64 = 4096;

async fn file_slab(dir: &TempDir, tag: &str) -> Slab {
    let path = dir.path().join(format!("{tag}.bin")).to_string_lossy().to_string();
    let dev = FileDevice::open_with_capacity(&path, 8 * 1024 * 1024).await.unwrap();
    Slab::format(Arc::new(dev), SLOT, StorageTier::Hot).await.unwrap()
}

async fn serve(dir: &TempDir, vm: VolumeManager) -> (Arc<AppState>, String, tokio::task::JoinHandle<()>) {
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_string_lossy().to_string());
    config.management.node_name = Some("node-a".into());
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

/// A mirrored volume says which slabs and drives hold it, how many legs
/// each, and the state of each — and a failed or quarantined slab shows.
#[tokio::test]
async fn a_mirrored_volume_says_where_its_legs_are() {
    let dir = TempDir::new().unwrap();
    let mut vm = VolumeManager::new(SLOT);
    vm.add_slab(file_slab(&dir, "a").await).await;
    vm.add_slab(file_slab(&dir, "b").await).await;
    let id = vm
        .create_volume_with("m", 1 << 20, CreateOptions::redundant(RedundancyPolicy::mirror(2)))
        .await
        .unwrap();
    let v = vm.get_volume(&id).unwrap();
    for i in 0..3u64 {
        v.write(i * SLOT, &vec![i as u8 + 1; SLOT as usize]).await.unwrap();
    }
    let (state, base, server) = serve(&dir, vm).await;
    let c = reqwest::Client::new();

    let got: serde_json::Value = c
        .get(format!("{base}/api/v1/volumes/{}", id.0))
        .send().await.unwrap().json().await.unwrap();
    let p = &got["placement"];
    let slabs = p["slabs"].as_array().unwrap();
    assert_eq!(slabs.len(), 2, "{p:#}");
    for s in slabs {
        assert_eq!(s["legs"], 3, "one leg of each of three extents on each slab: {s}");
        assert_eq!(s["bytes"], 3 * SLOT);
        assert_eq!(s["state"], "ok");
        assert_eq!(s["node"], "node-a");
        assert!(s["drive"]["path"].as_str().unwrap().ends_with(".bin"), "{s}");
        assert!(s["domain"].as_str().unwrap().starts_with("drive="), "{s}");
    }
    assert_eq!(p["drives"].as_array().unwrap().len(), 2, "two drives");
    assert_eq!(p["legs"]["policy"], "mirror:2");
    assert_eq!(p["legs"]["expected"], 6);
    assert_eq!(p["legs"]["missing"], 0);
    assert_eq!(p["rebuild"], "none");

    // The listing carries it only when asked.
    let plain: serde_json::Value = c.get(format!("{base}/api/v1/volumes")).send().await.unwrap().json().await.unwrap();
    assert!(plain["items"][0].get("placement").is_none(), "opt-in on the listing");
    let full: serde_json::Value = c
        .get(format!("{base}/api/v1/volumes?placement=true"))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(full["items"][0]["placement"]["slabs"].as_array().unwrap().len(), 2);

    // A leg the volume stops trusting shows as failed, and a rebuild is owed.
    let a = slabs[0]["id"].as_str().unwrap().to_string();
    let a_id = stormblock::drive::slab::SlabId(uuid::Uuid::parse_str(&a).unwrap());
    {
        let vm = state.volume_manager.lock().await;
        vm.get_volume_handle(&id).unwrap().set_failed_slabs([a_id]);
    }
    let got: serde_json::Value = c
        .get(format!("{base}/api/v1/volumes/{}", id.0))
        .send().await.unwrap().json().await.unwrap();
    let p = &got["placement"];
    let failed = p["slabs"].as_array().unwrap().iter().find(|s| s["id"] == a).unwrap();
    assert_eq!(failed["state"], "failed");
    assert_eq!(p["legs"]["missing"], 3);
    assert_eq!(p["rebuild"], "needed");
    assert_eq!(p["legs"]["failed_slabs"][0], a);

    // Quarantined by a health report.
    state.slab_registry.write().await.set_quarantined(
        stormblock::drive::slab::SlabId(uuid::Uuid::parse_str(slabs[1]["id"].as_str().unwrap()).unwrap()),
        true,
    );
    let got: serde_json::Value = c
        .get(format!("{base}/api/v1/volumes/{}", id.0))
        .send().await.unwrap().json().await.unwrap();
    let q = got["placement"]["slabs"].as_array().unwrap().iter().find(|s| s["id"] == slabs[1]["id"]).unwrap().clone();
    assert_eq!(q["state"], "quarantined");
    server.abort();
}

/// A mirror asks "has anything changed since N" instead of re-reading every
/// volume.
#[tokio::test]
async fn the_listing_has_a_generation_to_ask_changed_of() {
    let dir = TempDir::new().unwrap();
    let mut vm = VolumeManager::new(SLOT);
    vm.add_slab(file_slab(&dir, "a").await).await;
    let (state, base, server) = serve(&dir, vm).await;
    let c = reqwest::Client::new();

    let first = c.get(format!("{base}/api/v1/volumes")).send().await.unwrap();
    let etag = first.headers()["etag"].to_str().unwrap().to_string();
    let body: serde_json::Value = first.json().await.unwrap();
    let g = body["generation"].as_u64().unwrap();
    assert_eq!(etag, format!("\"{g}\""));

    let same = c.get(format!("{base}/api/v1/volumes?since={g}")).send().await.unwrap();
    assert_eq!(same.status(), 304);
    let same = c.get(format!("{base}/api/v1/volumes")).header("If-None-Match", &etag).send().await.unwrap();
    assert_eq!(same.status(), 304);

    state.volume_manager.lock().await.create_volume_any("new", 1 << 20).await.unwrap();
    let after = c.get(format!("{base}/api/v1/volumes?since={g}")).send().await.unwrap();
    assert_eq!(after.status(), 200, "a new volume is a change");
    let body: serde_json::Value = after.json().await.unwrap();
    assert!(body["generation"].as_u64().unwrap() > g);
    assert_eq!(body["count"], 1);
    server.abort();
}

/// Two slabs in two partitions of one drive are one failure domain, so a
/// mirror is refused rather than put both copies on one spindle (#136).
#[tokio::test]
async fn two_slabs_on_one_drive_are_one_failure_domain() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("disk.bin").to_string_lossy().to_string();
    let disk: Arc<dyn BlockDevice> = Arc::new(FileDevice::open_with_capacity(&path, 32 * 1024 * 1024).await.unwrap());
    let half = 16 * 1024 * 1024;
    let mut vm = VolumeManager::new(SLOT);
    for start in [0, half] {
        let part = Arc::new(PartitionDevice::new(disk.clone(), start, half).unwrap());
        assert_eq!(part.drive_id().path, path, "a partition is its drive");
        vm.add_slab(Slab::format(part, SLOT, StorageTier::Hot).await.unwrap()).await;
    }
    {
        let reg = vm.registry().read().await;
        let domains: std::collections::HashSet<String> =
            reg.iter().map(|(id, _)| reg.domain_of(id).to_string()).collect();
        assert_eq!(domains.len(), 1, "one drive, one domain: {domains:?}");
    }
    let err = vm
        .create_volume_with("m", 1 << 20, CreateOptions::redundant(RedundancyPolicy::mirror(2)))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("domain"), "{err}");
}

/// #218: what the listing reports without a persist — a failed or
/// quarantined slab, an attach and a detach — moves the generation too, and
/// nothing changing still answers 304.
#[tokio::test]
async fn the_generation_moves_on_attach_detach_and_slab_state() {
    let dir = TempDir::new().unwrap();
    let mut vm = VolumeManager::new(SLOT);
    vm.add_slab(file_slab(&dir, "a").await).await;
    vm.add_slab(file_slab(&dir, "b").await).await;
    let id = vm
        .create_volume_with("m", 1 << 20, CreateOptions::redundant(RedundancyPolicy::mirror(2)))
        .await
        .unwrap();
    vm.get_volume(&id).unwrap().write(0, &vec![1u8; SLOT as usize]).await.unwrap();
    let (state, base, server) = serve(&dir, vm).await;
    let c = reqwest::Client::new();

    let get = |since: u64| {
        let c = c.clone();
        let base = base.clone();
        async move {
            let r = c.get(format!("{base}/api/v1/volumes?since={since}")).send().await.unwrap();
            let status = r.status().as_u16();
            let body: serde_json::Value = if status == 200 { r.json().await.unwrap() } else { serde_json::Value::Null };
            (status, body)
        }
    };
    let gen0 = |b: &serde_json::Value| b["generation"].as_u64().unwrap();
    let (_, body) = get(0).await;
    let mut g = gen0(&body);
    assert_eq!(get(g).await.0, 304, "nothing changed");

    // A slab this volume stops trusting.
    let slabs: Vec<stormblock::drive::slab::SlabId> =
        state.slab_registry.read().await.iter().map(|(id, _)| *id).collect();
    state.volume_manager.lock().await.get_volume_handle(&id).unwrap().set_failed_slabs([slabs[0]]);
    let (st, body) = get(g).await;
    assert_eq!(st, 200, "a failed slab is a change");
    assert!(gen0(&body) > g);
    g = gen0(&body);
    assert_eq!(get(g).await.0, 304, "and then nothing changed");

    // A slab a health report quarantined.
    state.slab_registry.write().await.set_quarantined(slabs[1], true);
    let (st, body) = get(g).await;
    assert_eq!(st, 200, "a quarantine is a change");
    g = gen0(&body);
    assert_eq!(get(g).await.0, 304);

    // An attach to a host's own subsystem (#210): the volume is in use, and
    // the listing says so.
    let sub: stormblock::mgmt::nvme_hosts::HostSubsystem = serde_json::from_value(serde_json::json!({
        "nqn": "nqn.test:host:h1",
        "hosts": [],
        "namespaces": [{"volume": id.0, "nsid": 1}]
    }))
    .unwrap();
    state.nvme_hosts.lock().await.subsystems.insert(sub.nqn.clone(), sub);
    let (st, body) = get(g).await;
    assert_eq!(st, 200, "an attach is a change");
    let item = &body["items"][0];
    assert_eq!(item["in_use"], true, "{item}");
    assert_eq!(item["attachments"][0]["target"], "nqn.test:host:h1", "{item}");
    g = gen0(&body);
    assert_eq!(get(g).await.0, 304);

    // The detach.
    state.nvme_hosts.lock().await.subsystems.clear();
    let (st, body) = get(g).await;
    assert_eq!(st, 200, "a detach is a change");
    assert_eq!(body["items"][0]["in_use"], false);
    assert!(gen0(&body) > g);
    server.abort();
}
