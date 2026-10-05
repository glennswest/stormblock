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
