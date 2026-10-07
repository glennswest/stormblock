//! The stock engine mounts the serving surface (#60).
//!
//! `/serve/v1` is layer 2 in `docs/layering.md` — what it takes to serve
//! volumes to something, which is the job rather than a choice a deployment
//! makes differently. It used to exist only where a profile mounted it, so a
//! consumer running against a RouterOS node and an x86 one could list drives
//! on both and create a volume on only one.
//!
//! What this file pins is that the router mounts it whenever a serving
//! context is present, and that the context is built from a config that says
//! nothing about serving.

use crate::common;
use std::sync::Arc;

use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::raid::{RaidArray, RaidLevel};
use stormblock::serve::ctx::ServeContext;
use stormblock::serve::status::MkStatus;
use stormblock::serve::wiring::WiringTable;
use stormblock::target::reactor::{ReactorConfig, ReactorPool};
use stormblock::volume::VolumeManager;

use tempfile::TempDir;
use tokio::net::TcpListener;

const SLOT: u64 = 4096;

/// A node whose config mentions serving only by having a data directory —
/// which is the case #60 is about.
async fn stock_node(dir: &TempDir) -> (Arc<AppState>, StormBlockConfig) {
    let devices = common::create_file_devices(dir, 2, 32 * 1024 * 1024).await;
    let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();
    let array_id = array.array_id();
    let backing: Arc<dyn BlockDevice> = Arc::new(array);

    let mut vm = VolumeManager::new(SLOT);
    vm.add_backing_device(array_id, backing).await;

    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_string_lossy().to_string());

    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    let state = Arc::new(AppState::new(config.clone(), vm, reg, gem));
    (state, config)
}

/// Everything `main.rs` does to bring serving up, minus the spawned loops.
fn attach_serving(state: &Arc<AppState>, config: &StormBlockConfig) {
    let cfg = config
        .serve_config("0.0.0.0:3260", "0.0.0.0:4420")
        .expect("a node with a data dir serves");
    std::fs::create_dir_all(&cfg.data_dir).unwrap();

    let wiring = WiringTable::load(&cfg.data_dir);
    let reactor = Arc::new(ReactorPool::new(&ReactorConfig {
        core_count: 1,
        pin_cores: false,
    }));
    let ctx = Arc::new(ServeContext::new(
        cfg,
        state.clone(),
        Arc::new(MkStatus::new()),
        None,
        reactor,
        wiring,
    ));
    state.serve.set(ctx).ok().expect("serving context set once");
}

async fn start_server(state: Arc<AppState>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    common::wait_for_listener(addr).await;
    format!("http://{addr}")
}

/// The whole issue in one test: the routes a registry calls are reachable
/// from the stock router, not only from a profile that remembered to mount
/// them.
#[tokio::test]
async fn the_stock_router_serves_the_serving_surface() {
    let dir = TempDir::new().unwrap();
    let (state, config) = stock_node(&dir).await;
    attach_serving(&state, &config);
    let base = start_server(state).await;
    let c = reqwest::Client::new();

    for path in ["/serve/v1/ready", "/serve/v1/health", "/serve/v1/status"] {
        let resp = c.get(format!("{base}{path}")).send().await.unwrap();
        assert_ne!(
            resp.status().as_u16(),
            404,
            "{path} must be mounted by the stock engine"
        );
    }

    // The two the registry needs beyond readiness.
    for path in ["/serve/v1/volumes", "/serve/v1/exports"] {
        let resp = c.get(format!("{base}{path}")).send().await.unwrap();
        assert_ne!(resp.status().as_u16(), 404, "{path} must be mounted");
    }
}

/// The deprecated prefix is still served, so a consumer mid-upgrade is not
/// broken by this change. sbregistry v0.7.0 probes `/mk/v1` only as a
/// fallback; the alias goes when mkube follows.
#[tokio::test]
async fn the_legacy_mk_prefix_is_still_answered() {
    let dir = TempDir::new().unwrap();
    let (state, config) = stock_node(&dir).await;
    attach_serving(&state, &config);
    let base = start_server(state).await;
    let c = reqwest::Client::new();

    let resp = c.get(format!("{base}/mk/v1/ready")).send().await.unwrap();
    assert_ne!(resp.status().as_u16(), 404);
}

/// Mounting the serving surface must not disturb the engine surface it is
/// merged into.
#[tokio::test]
async fn the_engine_surface_is_unchanged_alongside_it() {
    let dir = TempDir::new().unwrap();
    let (state, config) = stock_node(&dir).await;
    attach_serving(&state, &config);
    let base = start_server(state).await;
    let c = reqwest::Client::new();

    for path in ["/api/v1/drives", "/api/v1/volumes", "/api/v1/slabs"] {
        let resp = c.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 200, "{path}");
    }
}

/// A node with no serving context serves the engine surface and nothing else
/// — the `serve.enabled = false` case, and the one where there was nowhere to
/// keep the wiring table.
#[tokio::test]
async fn without_a_context_the_serving_surface_is_simply_absent() {
    let dir = TempDir::new().unwrap();
    let (state, _config) = stock_node(&dir).await;
    let base = start_server(state).await;
    let c = reqwest::Client::new();

    let resp = c.get(format!("{base}/serve/v1/ready")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 404);

    let resp = c.get(format!("{base}/api/v1/drives")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200, "the engine surface is untouched");
}

/// #174: 16 exports at once — what a registry pushing images does. Every
/// persist used to write one fixed `exports.tmp`, so concurrent ones renamed
/// it away under each other (`rename exports.tmp -> exports.json: No such
/// file or directory`, a 500). Now every one is a 201, the table on disk
/// names all 16, and no temporary file is left.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_exports_all_persist() {
    let dir = TempDir::new().unwrap();
    let (state, config) = stock_node(&dir).await;
    attach_serving(&state, &config);
    let mut ids = Vec::new();
    {
        let mut vm = state.volume_manager.lock().await;
        for i in 0..16 {
            ids.push(vm.create_volume_any(&format!("c{i}"), 1024 * 1024).await.unwrap().0);
        }
    }
    let base = start_server(state.clone()).await;
    let c = reqwest::Client::new();
    let calls = ids.iter().map(|id| {
        let (c, base, id) = (c.clone(), base.clone(), *id);
        tokio::spawn(async move {
            let r = c
                .post(format!("{base}/serve/v1/exports"))
                .json(&serde_json::json!({"volume_id": id}))
                .send()
                .await
                .unwrap();
            let status = r.status().as_u16();
            (status, r.text().await.unwrap_or_default())
        })
    });
    for call in calls.collect::<Vec<_>>() {
        let (status, body) = call.await.unwrap();
        assert_eq!(status, 201, "{body}");
    }
    let serve_dir = dir.path().join("serve");
    let on_disk: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(serve_dir.join("exports.json")).unwrap()).unwrap();
    for id in &ids {
        assert!(
            on_disk.iter().any(|e| e["volume_id"] == id.to_string()),
            "volume {id}'s export is in the table on disk"
        );
    }
    let tmp: Vec<_> = std::fs::read_dir(&serve_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(tmp.is_empty(), "temporary files left: {tmp:?}");
}

/// #174: a create-and-export whose export cannot be kept answers an error
/// and leaves nothing behind — no volume, no export. A 500 for a volume that
/// exists is how a caller leaks it.
#[tokio::test]
async fn a_volume_whose_export_fails_is_not_left_behind() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let (state, config) = stock_node(&dir).await;
    attach_serving(&state, &config);
    let base = start_server(state.clone()).await;
    let c = reqwest::Client::new();
    let before = state.volume_manager.lock().await.list_volumes().await.len();

    // Nowhere to write the tables: every persist fails.
    let serve_dir = dir.path().join("serve");
    std::fs::set_permissions(&serve_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::File::create(serve_dir.join("probe")).is_ok() {
        eprintln!("SKIP: running as root, a read-only directory is still writable");
        std::fs::set_permissions(&serve_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    let r = c
        .post(format!("{base}/serve/v1/volumes"))
        .json(&serde_json::json!({"name": "leaky", "size_bytes": 1048576, "export": true}))
        .send()
        .await
        .unwrap();
    std::fs::set_permissions(&serve_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(r.status().as_u16(), 500);
    let vm = state.volume_manager.lock().await;
    assert_eq!(vm.list_volumes().await.len(), before, "the volume made for the export is gone");
    assert!(vm.find_volume("leaky").await.is_none());
    drop(vm);
    assert!(state.exports.read().await.is_empty(), "and so is its export");
}
