//! The management API's credential, end to end over HTTP (#107).
//!
//! What #107 found was not a missing mechanism — `serve::api::require_token`
//! was written, with an admin token and a public-path exemption — but that
//! nothing wired it to the engine's router and nothing minted a token. So a
//! node listened on `0.0.0.0:9090` and answered `POST /api/v1/fstemplates`
//! from anywhere, with no credential, on the surface that can delete a volume
//! and re-point the synonym that decides what a machine boots.
//!
//! These drive the real router over a real socket, because the hole was in
//! the wiring rather than in the check: a unit test of the check passed
//! throughout.

use crate::common;
use std::sync::Arc;

use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::raid::{RaidArray, RaidLevel};
use stormblock::volume::VolumeManager;

use tempfile::TempDir;
use tokio::net::TcpListener;

const SLOT: u64 = 4096;

async fn state_with(dir: &TempDir, config: StormBlockConfig) -> Arc<AppState> {
    let devices = common::create_file_devices(dir, 2, 16 * 1024 * 1024).await;
    let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();
    let array_id = array.array_id();
    let backing: Arc<dyn BlockDevice> = Arc::new(array);
    let mut vm = VolumeManager::new(SLOT);
    vm.add_backing_device(array_id, backing).await;
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
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (format!("http://{addr}"), handle)
}

fn config_with_token(token: &str) -> StormBlockConfig {
    let mut c = StormBlockConfig::default();
    c.management.node_name = Some("n1".to_string());
    c.management.api_token = Some(token.to_string());
    c
}

/// The exact requests from the issue, against a node that has a token.
#[tokio::test]
async fn engine_surface_requires_the_token() {
    let dir = TempDir::new().unwrap();
    let state = state_with(&dir, config_with_token("sekrit")).await;
    let (base, server) = serve(state).await;
    let c = reqwest::Client::new();

    for path in [
        "/api/v1/volumes",
        "/api/v1/slabs",
        "/api/v1/fstemplates",
        "/api/v1/synonyms",
        "/apis/storage.storm.io/v1/volumes",
    ] {
        let s = c.get(format!("{base}{path}")).send().await.unwrap().status().as_u16();
        assert_eq!(s, 401, "{path} answered without a token");

        let s = c
            .get(format!("{base}{path}"))
            .bearer_auth("sekrit")
            .send()
            .await
            .unwrap()
            .status()
            .as_u16();
        assert_ne!(s, 401, "{path} refused the right token");
    }

    // The 422 in the issue: a body that got as far as being parsed is a
    // request that got past authentication.
    let resp = c
        .post(format!("{base}/api/v1/fstemplates"))
        .json(&serde_json::json!({ "nonsense": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401, "a bad body must not reveal that it was read");

    server.abort();
}

/// Health and metrics stay open: one is how a booting node finds an appliance
/// at all, the other is scraped by something that cannot hold a per-node token.
#[tokio::test]
async fn probes_stay_public_and_health_reports_the_mode() {
    let dir = TempDir::new().unwrap();
    let state = state_with(&dir, config_with_token("sekrit")).await;
    let (base, server) = serve(state).await;
    let c = reqwest::Client::new();

    let health = c.get(format!("{base}/api/v1/health")).send().await.unwrap();
    assert_eq!(health.status().as_u16(), 200);
    let body: serde_json::Value = health.json().await.unwrap();
    assert_eq!(body["auth"], "required");
    assert_eq!(body["service"], "stormblock");
    // #322: a node on its own drives is not diskless, and says so openly.
    assert_eq!(body["slabs"]["diskless"], false, "{body}");
    assert_eq!(body["slabs"]["system"], "local", "{body}");
    assert!(body["slabs"]["items"].as_array().is_some_and(|i| !i.is_empty() && i.iter().all(|i| i["source"] == "local")), "{body}");

    server.abort();
}

/// `flow_over_remaining` (#260): absent with no flow-over, then whatever the
/// flow-over last counted, 0 included — open, like the rest of health.
#[tokio::test]
async fn health_reports_what_the_flow_over_has_left() {
    use std::sync::atomic::Ordering;
    let dir = TempDir::new().unwrap();
    let state = state_with(&dir, config_with_token("sekrit")).await;
    let (base, server) = serve(state.clone()).await;
    let c = reqwest::Client::new();
    let get = || async {
        c.get(format!("{base}/api/v1/health")).send().await.unwrap().json::<serde_json::Value>().await.unwrap()
    };

    assert!(get().await.get("flow_over_remaining").is_none(), "no flow-over: left out");
    state.flow_over_remaining.store(1234, Ordering::Relaxed);
    assert_eq!(get().await["flow_over_remaining"], 1234);
    state.flow_over_remaining.store(0, Ordering::Relaxed);
    assert_eq!(get().await["flow_over_remaining"], 0, "finished: 0, not left out");

    server.abort();
}

/// #337: health names a ublk request unanswered for 30 s, open (no token),
/// so a consumer whose fsync never returns can see it is below it.
#[tokio::test]
async fn health_names_a_ublk_request_that_was_never_answered() {
    let dir = TempDir::new().unwrap();
    let state = state_with(&dir, config_with_token("sekrit")).await;
    let (base, server) = serve(state.clone()).await;
    let c = reqwest::Client::new();
    let get = || async {
        c.get(format!("{base}/api/v1/health")).send().await.unwrap().json::<serde_json::Value>().await.unwrap()
    };
    assert!(get().await.get("ublk_stuck").is_none(), "nothing outstanding: left out");
    let t = stormblock::drive::ublk::QueueTrack::new(77, 0, "volume:fastetcd-data".into(), 8);
    t.begin(3, 2 /* UBLK_IO_OP_FLUSH */);
    assert!(get().await.get("ublk_stuck").is_none(), "a flush just taken is not stuck");
    t.backdate(3, 4300);
    let body = get().await;
    assert_eq!(body["status"], "ok");
    let s = &body["ublk_stuck"][0];
    assert_eq!(s["op"], "flush");
    assert_eq!(s["ublk"], "/dev/ublkb77");
    assert_eq!(s["device"], "volume:fastetcd-data");
    assert!(s["secs"].as_f64().unwrap() >= 4300.0);
    t.end(3);
    assert!(get().await.get("ublk_stuck").is_none(), "answered: gone");
    server.abort();
}

#[tokio::test]
async fn health_says_when_the_node_is_open() {
    let dir = TempDir::new().unwrap();
    let mut config = StormBlockConfig::default();
    config.management.node_name = Some("n1".to_string());
    let state = state_with(&dir, config).await;
    let (base, server) = serve(state).await;
    let c = reqwest::Client::new();

    let body: serde_json::Value = c
        .get(format!("{base}/api/v1/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["auth"], "none");

    // ...and it is open, which is the state the fleet is in today.
    let s = c.get(format!("{base}/api/v1/volumes")).send().await.unwrap().status().as_u16();
    assert_eq!(s, 200);

    server.abort();
}

/// A distinct admin token: reads on the ordinary one, deletes only on the
/// admin one.
#[tokio::test]
async fn admin_token_guards_destructive_verbs() {
    let dir = TempDir::new().unwrap();
    let mut config = config_with_token("read");
    config.management.admin_token = Some("root".to_string());
    let state = state_with(&dir, config).await;
    let (base, server) = serve(state).await;
    let c = reqwest::Client::new();

    let id = uuid::Uuid::new_v4();

    let s = c
        .get(format!("{base}/api/v1/volumes"))
        .bearer_auth("read")
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(s, 200);

    // A slab is destructive (#274): the ordinary token is not enough. (An
    // unsealed volume's delete is the ordinary token's since #274.)
    let resp = c
        .delete(format!("{base}/api/v1/slabs/{id}"))
        .bearer_auth("read")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("admin"),
        "the refusal should say which token is wanted: {body}"
    );

    // The admin token is accepted — 404 for a volume that does not exist is
    // the point: it got through.
    let s = c
        .delete(format!("{base}/api/v1/slabs/{id}"))
        .bearer_auth("root")
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    assert_ne!(s, 401);

    server.abort();
}

/// `/v1` is a contract with its own error envelope, and a 401 has to speak it.
#[tokio::test]
async fn v1_keeps_its_error_envelope() {
    let dir = TempDir::new().unwrap();
    let state = state_with(&dir, config_with_token("sekrit")).await;
    let (base, server) = serve(state).await;
    let c = reqwest::Client::new();

    let resp = c.get(format!("{base}/v1/volumes")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 401);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "unauthorized");
    assert!(body["message"].is_string());

    // Everything else answers {error, code}.
    let resp = c.get(format!("{base}/api/v1/volumes")).send().await.unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], 401);
    assert!(body["error"].is_string());

    server.abort();
}
