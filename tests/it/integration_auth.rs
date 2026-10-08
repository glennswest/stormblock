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

/// #283: `/debug` stays open, but without the token it shows route families,
/// not paths; task and thread dumps are the admin's alone (#365), and a task
/// dump is taken once for many callers.
#[tokio::test]
async fn debug_is_open_but_an_open_caller_sees_no_paths_and_cannot_force_dumps() {
    use tokio::io::AsyncWriteExt;
    let dir = TempDir::new().unwrap();
    let state = state_with(&dir, config_with_token("sekrit")).await;
    let (base, server) = serve(state).await;
    let c = reqwest::Client::new();

    // A request in flight whose path names something: its body never
    // finishes arriving, so it waits in the handler.
    let secret = "3f1c0de5-0283-4000-8000-5ec2e7da7a00";
    let mut held = tokio::net::TcpStream::connect(base.trim_start_matches("http://")).await.unwrap();
    held.write_all(
        format!(
            "POST /api/v1/volumes/{secret}/clone HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer sekrit\r\n\
             Content-Type: application/json\r\nContent-Length: 100\r\n\r\n{{\"name\":"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let open = c.get(format!("{base}/debug/stalls")).send().await.unwrap();
    assert_eq!(open.status(), 200, "open, as #269 made it");
    let open = open.text().await.unwrap();
    assert!(!open.contains(secret), "the open view names no volume: {open}");
    assert!(open.contains("POST /api/v1/volumes/…"), "only its route family: {open}");
    assert!(open.contains("need the node token"), "and says what it leaves out: {open}");

    let full = c.get(format!("{base}/debug/stalls")).bearer_auth("sekrit").send().await.unwrap().text().await.unwrap();
    assert!(full.contains(&format!("/api/v1/volumes/{secret}/clone")), "the token sees the path: {full}");
    drop(held);

    // Dumps are the admin's, on demand (#365): nobody else can force one.
    for p in ["/debug/tasks", "/debug/threads"] {
        let r = c.get(format!("{base}{p}")).send().await.unwrap();
        assert_eq!(r.status(), 401, "{p} without a token");
    }
    // Many callers at once, then again at once: one dump answers them all.
    let before = stormblock::mgmt::debug::TASK_DUMPS.load(std::sync::atomic::Ordering::Relaxed);
    let calls: Vec<_> = (0..12).map(|_| c.get(format!("{base}/debug/tasks")).bearer_auth("sekrit").send()).collect();
    for r in futures_util::future::join_all(calls).await {
        assert_eq!(r.unwrap().status(), 200);
    }
    let after = stormblock::mgmt::debug::TASK_DUMPS.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(after - before, 1, "one task dump for twelve callers");
    let t = c.get(format!("{base}/debug/threads")).bearer_auth("sekrit").send().await.unwrap().text().await.unwrap();
    assert!(t.contains("thread(s)"), "{t}");
    server.abort();
}

/// #365: who holds a lock and who waits on it is named. A background task
/// holds the volume manager; `GET /api/v1/volumes` waits on it; `/debug/locks`
/// names the holder, how long, and the waiting request (by route family in
/// the open view), and the request ends with its lock wait counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lock_holder_and_its_waiters_are_named() {
    let dir = TempDir::new().unwrap();
    let state = state_with(&dir, config_with_token("sekrit")).await;
    let (base, server) = serve(state.clone()).await;
    let c = reqwest::Client::new();

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let vm = state.volume_manager.clone();
    let holder = tokio::spawn(stormblock::lockwatch::named("template build pvc-16t", async move {
        let _g = vm.lock().await;
        let _ = rx.await;
    }));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let c2 = c.clone();
    let b2 = base.clone();
    let waiting = tokio::spawn(async move {
        c2.get(format!("{b2}/api/v1/volumes/3f1c0de5-0283-4000-8000-5ec2e7da7a00")).bearer_auth("sekrit").send().await
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let full = c.get(format!("{base}/debug/locks")).bearer_auth("sekrit").send().await.unwrap().text().await.unwrap();
    assert!(full.contains("the volume manager: held"), "{full}");
    assert!(full.contains("by template build pvc-16t"), "the holder is named: {full}");
    assert!(full.contains("1 waiting") && full.contains("GET /api/v1/volumes/3f1c0de5"), "the waiter is named: {full}");
    let open = c.get(format!("{base}/debug/locks")).send().await.unwrap().text().await.unwrap();
    assert!(open.contains("by template build pvc-16t"), "{open}");
    assert!(!open.contains("3f1c0de5"), "the open view names no volume: {open}");

    tx.send(()).unwrap();
    holder.await.unwrap();
    let r = waiting.await.unwrap().unwrap();
    assert!(r.status().as_u16() == 404 || r.status().is_success(), "{}", r.status());
    let after = c.get(format!("{base}/debug/locks")).bearer_auth("sekrit").send().await.unwrap().text().await.unwrap();
    assert!(!after.contains("template build"), "a released lock leaves nothing behind: {after}");
    server.abort();
}
