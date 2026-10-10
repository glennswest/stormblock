//! Pool admission under per-drive overcommit (#152, stormdrive#13).

use crate::common;
use std::sync::Arc;

use stormblock::drive::filedev::FileDevice;
use stormblock::drive::slab::{Slab, SlabFormat, SlabRole};
use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::placement::topology::StorageTier;
use stormblock::volume::{VolumeManager, DEFAULT_EXTENT_SIZE};
use tempfile::TempDir;
use tokio::net::TcpListener;

const MIB: u64 = 1024 * 1024;

/// A node with a 256 MiB system slab and a 256 MiB data slab.
async fn node(dir: &TempDir, admission: Option<&str>) -> (String, Arc<AppState>) {
    let mut vm = VolumeManager::new(DEFAULT_EXTENT_SIZE);
    for role in [SlabRole::System, SlabRole::Data] {
        let path = dir.path().join(format!("{role:?}.slab"));
        let dev: Arc<dyn BlockDevice> =
            Arc::new(FileDevice::open_with_capacity(path.to_str().unwrap(), 256 * MIB).await.unwrap());
        let fmt = SlabFormat::new(DEFAULT_EXTENT_SIZE, StorageTier::Hot)
            .with_role(role)
            .with_auto_metadata(dev.capacity_bytes());
        vm.add_slab(Slab::format_with(dev, fmt).await.unwrap()).await;
    }
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_str().unwrap().to_string());
    config.capacity.admission = admission.map(str::to_string);
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    let state = Arc::new(AppState::new(config, vm, reg, gem));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    common::wait_for_listener(addr).await;
    (format!("http://{addr}"), state)
}

async fn claim(c: &reqwest::Client, url: &str, name: &str, size: &str, role: &str) -> (u16, serde_json::Value) {
    let r = c
        .post(format!("{url}/api/v1/volumes"))
        .json(&serde_json::json!({ "name": name, "size": size, "role": role }))
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or_default())
}

fn pool<'a>(report: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    report["capacity"]["pools"].as_array().unwrap().iter().find(|p| p["pool"] == name).unwrap()
}

/// Enforced: a data claim past what the data half can promise is refused
/// (507, the volume taken back); the drive's overcommit raises what it can
/// promise and the same claim is admitted; the system half never refuses.
/// Reported: the same claim is admitted.
#[tokio::test]
async fn a_data_claim_past_what_the_pool_can_promise_is_refused_and_overcommit_admits_it() {
    let dir = TempDir::new().unwrap();
    let (url, _state) = node(&dir, Some("enforce")).await;
    let c = reqwest::Client::new();

    assert_eq!(claim(&c, &url, "a", "160M", "data").await.0, 201);
    let (status, body) = claim(&c, &url, "b", "160M", "data").await;
    assert_eq!(status, 507, "{body}");
    assert_eq!(body["code"], "insufficient_capacity", "{body}");
    assert!(body["message"].as_str().unwrap().contains("pool data"), "{body}");
    let vols: serde_json::Value = c.get(format!("{url}/api/v1/volumes")).send().await.unwrap().json().await.unwrap();
    assert!(!vols.to_string().contains("\"b\""), "a refused claim is taken back: {vols}");

    // The system half: counted, never refused.
    assert_eq!(claim(&c, &url, "sys", "1G", "system").await.0, 201);

    let report: serde_json::Value =
        c.get(format!("{url}/api/v1/slabs/pool")).send().await.unwrap().json().await.unwrap();
    assert_eq!(report["capacity"]["admission"], "enforce");
    let data = pool(&report, "data");
    let promisable = data["promisable_bytes"].as_u64().unwrap();
    assert!(promisable < 256 * MIB && promisable > 128 * MIB, "{data}");
    assert_eq!(data["committed_bytes"].as_u64().unwrap(), 160 * MIB, "{data}");
    assert_eq!(data["enforced"], true);
    let sys = pool(&report, "system");
    assert!(sys["committed_bytes"].as_u64().unwrap() >= 1024 * MIB, "{sys}");
    assert!(sys["headroom_bytes"].as_i64().unwrap() < 0, "the system half is promised past it: {sys}");
    assert_eq!(sys["enforced"], false);

    // stormdrive turns overcommit on for the data slab's drive, by path.
    let slabs: serde_json::Value = c.get(format!("{url}/api/v1/slabs")).send().await.unwrap().json().await.unwrap();
    let data_slab = slabs["items"].as_array().unwrap().iter().find(|s| s["role"] == "data").unwrap().clone();
    assert_eq!(data_slab["pool"], "data");
    assert_eq!(data_slab["committed_bytes"].as_u64().unwrap(), 160 * MIB, "{data_slab}");
    let path = data_slab["drive"]["path"].as_str().unwrap().to_string();
    let enc: String = path.bytes().map(|b| format!("%{b:02X}")).collect();
    let r = c
        .put(format!("{url}/api/v1/drives/{enc}/overcommit"))
        .json(&serde_json::json!({ "enabled": true, "ratio": 2.0, "drive": { "path": path, "serial": "", "wwn": "", "uuid": "" } }))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let bad = c
        .put(format!("{url}/api/v1/drives/{enc}/overcommit"))
        .json(&serde_json::json!({ "enabled": true, "ratio": 40.0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status().as_u16(), 400);
    assert!(dir.path().join("overcommit.json").exists(), "kept by the engine");

    assert_eq!(claim(&c, &url, "b", "160M", "data").await.0, 201, "admitted under 2x");
    let report: serde_json::Value =
        c.get(format!("{url}/api/v1/slabs/pool")).send().await.unwrap().json().await.unwrap();
    let data = pool(&report, "data");
    assert_eq!(data["promisable_bytes"].as_u64().unwrap(), 2 * promisable, "{data}");
    assert_eq!(data["committed_bytes"].as_u64().unwrap(), 320 * MIB, "{data}");

    let metrics = c.get(format!("{url}/metrics")).send().await.unwrap().text().await.unwrap();
    assert!(metrics.contains("stormblock_capacity_committed_bytes"), "{metrics}");

    // The default, report: the same claim past the pool is admitted.
    let dir2 = TempDir::new().unwrap();
    let (url2, _s2) = node(&dir2, None).await;
    assert_eq!(claim(&c, &url2, "a", "160M", "data").await.0, 201);
    assert_eq!(claim(&c, &url2, "b", "160M", "data").await.0, 201);
    let report: serde_json::Value =
        c.get(format!("{url2}/api/v1/slabs/pool")).send().await.unwrap().json().await.unwrap();
    assert_eq!(report["capacity"]["admission"], "report");
    assert!(pool(&report, "data")["headroom_bytes"].as_i64().unwrap() < 0);
}
