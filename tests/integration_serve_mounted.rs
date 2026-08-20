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

mod common;

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
