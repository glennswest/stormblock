//! Secure delete over HTTP (#286): `DELETE /api/v1/volumes/{id}?erase=`,
//! the eraser the daemon starts, and `GET /api/v1/erasures`.

use crate::common;
use std::sync::Arc;
use std::time::Duration;

use stormblock::drive::filedev::FileDevice;
use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::raid::RaidArrayId;
use stormblock::volume::VolumeManager;

use tempfile::TempDir;
use tokio::net::TcpListener;

const MIB: u64 = 1024 * 1024;

fn holds(raw: &[u8], pat: &[u8]) -> bool {
    raw.windows(pat.len()).any(|w| w == pat)
}

#[tokio::test]
async fn a_delete_over_http_overwrites_the_volumes_data_and_records_it() {
    let dir = TempDir::new().unwrap();
    let dev: Arc<dyn BlockDevice> = Arc::new(
        FileDevice::open_with_capacity(dir.path().join("pool.bin").to_str().unwrap(), 64 * MIB)
            .await
            .unwrap(),
    );
    let mut vm = VolumeManager::new(MIB);
    vm.add_backing_device(RaidArrayId(uuid::Uuid::new_v4()), dev.clone()).await;
    let pat = b"HTTP-SECRET-286-";
    let data: Vec<u8> = pat.iter().copied().cycle().take(3 * MIB as usize).collect();
    let id = vm.create_volume_any("secret", 8 * MIB).await.unwrap();
    let other = vm.create_volume_any("other", 8 * MIB).await.unwrap();
    {
        let h = vm.get_volume(&id).unwrap();
        h.write(0, &data).await.unwrap();
        h.flush().await.unwrap();
    }

    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().join("engine").display().to_string());
    std::fs::create_dir_all(dir.path().join("engine")).unwrap();
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    let state = Arc::new(AppState::new(config, vm, reg, gem));
    state.start_eraser().await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    common::wait_for_listener(addr).await;
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    let slabs: serde_json::Value = client.get(format!("{base}/api/v1/slabs")).send().await.unwrap().json().await.unwrap();
    assert_eq!(slabs["items"][0]["erase"], "once", "the node's default: {slabs}");

    let r = client.delete(format!("{base}/api/v1/volumes/{}?erase=dod5", other.0)).send().await.unwrap();
    assert_eq!(r.status(), 400);

    let r = client.delete(format!("{base}/api/v1/volumes/{}?erase=dod3", id.0)).send().await.unwrap();
    assert_eq!(r.status(), 204);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let record = loop {
        let st: serde_json::Value =
            client.get(format!("{base}/api/v1/erasures")).send().await.unwrap().json().await.unwrap();
        if let Some(e) = st["finished"].as_array().and_then(|f| f.first()) {
            assert_eq!(st["pending_slots"], 0);
            assert_eq!(st["default"], "once");
            break e.clone();
        }
        assert!(tokio::time::Instant::now() < deadline, "no erase recorded: {st}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(record["volume"], id.0.to_string());
    assert_eq!(record["level"], "dod3");
    assert_eq!(record["passes"], 3);
    assert_eq!(record["slots"], 3);
    assert_eq!(record["bytes"], 3 * MIB);
    assert_eq!(record["verified"], true);

    let mut all = vec![0u8; dev.capacity_bytes() as usize];
    dev.read(0, &mut all).await.unwrap();
    assert!(!holds(&all, pat), "the deleted volume's data is still on the device");
    assert!(dir.path().join("engine/erasures.json").exists());
    server.abort();
}

/// #313: `?scrub=used` — overwrite once what this delete leaves nobody
/// holding, then free it, and say what was queued. Copy-on-write aware: a
/// clone's delete scrubs its own slots and leaves the golden's shared ones;
/// the golden's delete scrubs those. It raises a node whose default is
/// `none`; a delete without it still answers 204.
#[tokio::test]
async fn scrub_used_overwrites_what_the_last_holder_frees_and_reports_it() {
    let dir = TempDir::new().unwrap();
    let dev: Arc<dyn BlockDevice> = Arc::new(
        FileDevice::open_with_capacity(dir.path().join("pool.bin").to_str().unwrap(), 64 * MIB)
            .await
            .unwrap(),
    );
    let mut vm = VolumeManager::new(MIB);
    vm.add_backing_device(RaidArrayId(uuid::Uuid::new_v4()), dev.clone()).await;
    let golden_pat = b"GOLDEN-BYTES-313";
    let clone_pat = b"CLONE-ONLY-313-!";
    let golden = vm.create_volume_any("golden", 8 * MIB).await.unwrap();
    {
        let h = vm.get_volume(&golden).unwrap();
        let data: Vec<u8> = golden_pat.iter().copied().cycle().take(3 * MIB as usize).collect();
        h.write(0, &data).await.unwrap();
        h.flush().await.unwrap();
    }
    let clone = vm.create_snapshot(golden, "clone").await.unwrap();
    {
        // One extent of its own (copy-on-write); two still shared.
        let h = vm.get_volume(&clone).unwrap();
        let data: Vec<u8> = clone_pat.iter().copied().cycle().take(MIB as usize).collect();
        h.write(0, &data).await.unwrap();
        h.flush().await.unwrap();
    }
    let plain = vm.create_volume_any("plain", 8 * MIB).await.unwrap();
    {
        let h = vm.get_volume(&plain).unwrap();
        h.write(0, &vec![0x5a; MIB as usize]).await.unwrap();
        h.flush().await.unwrap();
    }

    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().join("engine").display().to_string());
    std::fs::create_dir_all(dir.path().join("engine")).unwrap();
    // A node that does not erase by default: scrub=used must raise it.
    config.erase.default = stormblock::drive::erase::EraseLevel::None;
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    let state = Arc::new(AppState::new(config, vm, reg, gem));
    state.start_eraser().await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    common::wait_for_listener(addr).await;
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();
    let on_device = |pat: &'static [u8]| {
        let dev = dev.clone();
        async move {
            let mut all = vec![0u8; dev.capacity_bytes() as usize];
            dev.read(0, &mut all).await.unwrap();
            holds(&all, pat)
        }
    };
    let erased = |vol: uuid::Uuid| {
        let (client, base) = (client.clone(), base.clone());
        async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            loop {
                let st: serde_json::Value =
                    client.get(format!("{base}/api/v1/erasures")).send().await.unwrap().json().await.unwrap();
                let rec = st["finished"]
                    .as_array()
                    .and_then(|f| f.iter().find(|r| r["volume"] == vol.to_string()).cloned());
                if let Some(r) = rec {
                    if st["pending_slots"] == 0 {
                        return r;
                    }
                }
                assert!(tokio::time::Instant::now() < deadline, "{vol} not erased: {st}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };

    // Not a scrub this engine knows.
    let r = client.delete(format!("{base}/api/v1/volumes/{}?scrub=all", clone.0)).send().await.unwrap();
    assert_eq!(r.status(), 400);

    // The clone: its own slot only.
    let r = client.delete(format!("{base}/api/v1/volumes/{}?scrub=used", clone.0)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["deleted"], clone.0.to_string(), "{body}");
    assert_eq!(body["scrub"]["level"], "once", "{body}");
    assert_eq!(body["scrub"]["slots"], 1, "only its copy-on-write slot: {body}");
    assert_eq!(body["scrub"]["bytes"], MIB, "{body}");
    let rec = erased(clone.0).await;
    assert_eq!((rec["level"].as_str(), rec["passes"].as_u64(), rec["slots"].as_u64()), (Some("once"), Some(1), Some(1)));
    assert!(!on_device(clone_pat).await, "the clone's own data is overwritten");
    assert!(on_device(golden_pat).await, "the golden's shared slots are untouched");

    // The golden, now its slots' last holder.
    let r = client.delete(format!("{base}/api/v1/volumes/{}?scrub=used", golden.0)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["scrub"]["slots"], 3, "{body}");
    assert_eq!(body["scrub"]["bytes"], 3 * MIB, "{body}");
    erased(golden.0).await;
    assert!(!on_device(golden_pat).await, "the golden's data is overwritten");

    // Without scrub, the node's default (none here): 204, nothing queued.
    let r = client.delete(format!("{base}/api/v1/volumes/{}", plain.0)).send().await.unwrap();
    assert_eq!(r.status(), 204);
    assert_eq!(state.slab_registry.read().await.erasing_slots(), 0);
    server.abort();
}
