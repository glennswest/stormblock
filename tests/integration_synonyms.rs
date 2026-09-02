//! Synonyms — a stable name that points at a volume, re-pointed at will, and
//! a client's way to ask whether what it holds is still current.

mod common;

use std::sync::Arc;

use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::{AppState, ArrayInfo, DriveInfo};
use stormblock::raid::{RaidArray, RaidLevel};
use stormblock::volume::{VolumeManager, DEFAULT_EXTENT_SIZE};

use tempfile::TempDir;
use tokio::net::TcpListener;

async fn start(state: Arc<AppState>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let router = stormblock::mgmt::api::router(state.clone());
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    common::wait_for_listener(addr).await;
    (base_url, handle)
}

/// Two volumes and a management surface over them.
async fn setup(dir: &TempDir) -> (Arc<AppState>, uuid::Uuid, uuid::Uuid) {
    let devices = common::create_file_devices(dir, 2, 64 * 1024 * 1024).await;
    let drive_infos: Vec<DriveInfo> = devices
        .iter()
        .enumerate()
        .map(|(i, d)| DriveInfo {
            device: d.clone(),
            path: format!("/dev/test{i}"),
            labels: Default::default(),
        })
        .collect();
    let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();
    let array_id = array.array_id();
    let level = array.level();
    let member_count = array.member_count();
    let capacity = array.capacity_bytes();
    let stripe_size = array.stripe_size();
    let arc_array = Arc::new(array);
    let backing: Arc<dyn BlockDevice> = arc_array.clone();

    let mut vm = VolumeManager::new(DEFAULT_EXTENT_SIZE);
    vm.add_backing_device(array_id, backing).await;
    let v1 = vm.create_volume("golden-1", 8 * 1024 * 1024, array_id).await.unwrap();
    let v2 = vm.create_volume("golden-2", 8 * 1024 * 1024, array_id).await.unwrap();

    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_str().unwrap().to_string());
    let slab_registry = vm.registry().clone();
    let gem = vm.gem().clone();
    let state = Arc::new(AppState::new(config, vm, slab_registry, gem));
    *state.drives.write().await = drive_infos;
    state.arrays.write().await.insert(
        array_id,
        ArrayInfo { array: arc_array, level, member_count, capacity_bytes: capacity, stripe_size },
    );
    (state, v1.0, v2.0)
}

#[tokio::test]
async fn a_synonym_resolves_re_points_and_says_whether_it_changed() {
    let dir = TempDir::new().unwrap();
    let (state, v1, v2) = setup(&dir).await;
    let (base, server) = start(state.clone()).await;
    let client = reqwest::Client::new();

    // Create by the volume's own name — a caller usually has that, not a uuid.
    let resp = client
        .post(format!("{base}/api/v1/synonyms"))
        .json(&serde_json::json!({"namespace": "images", "name": "fedora", "volume": "golden-1", "label": "43"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["version"], 1);
    assert_eq!(body["target"]["id"], v1.to_string());
    assert_eq!(body["volume"]["name"], "golden-1");

    // Resolving at the version we hold: unchanged, and the ETag agrees.
    let resp = client
        .get(format!("{base}/api/v1/synonyms/images/fedora?since=1"))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["etag"], "\"1\"");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["changed"], false);

    // The HTTP-native spelling of the same question.
    let resp = client
        .get(format!("{base}/api/v1/synonyms/images/fedora"))
        .header("If-None-Match", "\"1\"")
        .send().await.unwrap();
    assert_eq!(resp.status(), 304);

    // Publish a new version of the same name.
    let resp = client
        .put(format!("{base}/api/v1/synonyms/images/fedora"))
        .json(&serde_json::json!({"volume": v2.to_string(), "label": "44"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["version"], 2);
    assert_eq!(body["target"]["id"], v2.to_string());
    assert_eq!(body["label"], "44");
    assert_eq!(body["history"][0]["target"]["id"], v1.to_string());

    // The client that still holds version 1 learns it moved, and gets the new
    // target in the same call.
    let resp = client
        .get(format!("{base}/api/v1/synonyms/images/fedora?since=1"))
        .send().await.unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["changed"], true);
    assert_eq!(body["target"]["id"], v2.to_string());
    assert_eq!(body["volume"]["name"], "golden-2");

    // Stale ETag: not a 304 any more.
    let resp = client
        .get(format!("{base}/api/v1/synonyms/images/fedora"))
        .header("If-None-Match", "\"1\"")
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);

    // Rolling back goes forward in version: undoing a publish is a change.
    let resp = client
        .post(format!("{base}/api/v1/synonyms/images/fedora/rollback"))
        .send().await.unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["version"], 3);
    assert_eq!(body["target"]["id"], v1.to_string());
    assert_eq!(body["label"], "43");

    server.abort();
}

/// A name is not a volume: dropping one leaves the other alone, and deleting
/// a volume something still names is refused.
#[tokio::test]
async fn a_synonym_holds_a_reference_but_owns_nothing() {
    let dir = TempDir::new().unwrap();
    let (state, v1, _v2) = setup(&dir).await;
    let (base, server) = start(state.clone()).await;
    let client = reqwest::Client::new();

    client
        .post(format!("{base}/api/v1/synonyms"))
        .json(&serde_json::json!({"name": "node-root", "volume": v1.to_string()}))
        .send().await.unwrap();

    // Deleting the volume under the name is refused, and the refusal names it.
    let resp = client.delete(format!("{base}/api/v1/volumes/{v1}")).send().await.unwrap();
    assert_eq!(resp.status(), 409);
    let text = resp.text().await.unwrap();
    assert!(text.contains("default/node-root"), "{text}");

    // The reverse question — what points at this volume — is answerable.
    let body: serde_json::Value = client
        .get(format!("{base}/api/v1/synonyms?target={v1}"))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(body["count"], 1);

    // Dropping the name leaves the volume.
    let resp = client.delete(format!("{base}/api/v1/synonyms/node-root")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let resp = client.get(format!("{base}/api/v1/volumes/{v1}")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    // …and now the volume can go.
    let resp = client.delete(format!("{base}/api/v1/volumes/{v1}")).send().await.unwrap();
    assert_eq!(resp.status(), 204);

    server.abort();
}

/// A synonym is usable everywhere a volume is named by id or name — that is
/// the point of it — and it never shadows a real volume.
#[tokio::test]
async fn a_synonym_names_a_volume_at_the_other_doors() {
    let dir = TempDir::new().unwrap();
    let (state, v1, v2) = setup(&dir).await;
    let (base, server) = start(state.clone()).await;
    let client = reqwest::Client::new();

    client
        .post(format!("{base}/api/v1/synonyms"))
        .json(&serde_json::json!({"name": "current", "volume": v1.to_string()}))
        .send().await.unwrap();

    // The access door takes it.
    let resp = client
        .put(format!("{base}/api/v1/volumes/current/access"))
        .json(&serde_json::json!({"access": "ro"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["id"], v1.to_string());
    assert_eq!(body["writable"], false);

    // A synonym whose name collides with a volume's own name does not win.
    client
        .post(format!("{base}/api/v1/synonyms"))
        .json(&serde_json::json!({"name": "golden-1", "volume": v2.to_string()}))
        .send().await.unwrap();
    let body: serde_json::Value = client
        .get(format!("{base}/api/v1/volumes/golden-1/access"))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(body["id"], v1.to_string(), "the volume answers to its own name first");

    server.abort();
}

/// A name survives a restart, or it is not a name.
#[tokio::test]
async fn synonyms_survive_a_restart() {
    let dir = TempDir::new().unwrap();
    let (state, v1, _v2) = setup(&dir).await;
    {
        let (base, server) = start(state.clone()).await;
        let client = reqwest::Client::new();
        client
            .post(format!("{base}/api/v1/synonyms"))
            .json(&serde_json::json!({"namespace": "images", "name": "boot", "volume": v1.to_string()}))
            .send().await.unwrap();
        client
            .put(format!("{base}/api/v1/synonyms/images/boot"))
            .json(&serde_json::json!({"volume": v1.to_string(), "label": "rebuilt"}))
            .send().await.unwrap();
        server.abort();
    }

    // A second engine over the same data dir, holding no volumes of its own.
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_str().unwrap().to_string());
    let fresh = VolumeManager::new(DEFAULT_EXTENT_SIZE);
    let reg = fresh.registry().clone();
    let gem = fresh.gem().clone();
    let second = Arc::new(AppState::new(config, fresh, reg, gem));
    let (base, server) = start(second).await;
    let client = reqwest::Client::new();
    let body: serde_json::Value = client
        .get(format!("{base}/api/v1/synonyms/images/boot"))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(body["target"]["id"], v1.to_string());
    assert_eq!(body["version"], 2, "the version a client holds survives too");
    assert_eq!(body["label"], "rebuilt");
    // The volume itself is not in this engine, and the synonym says so
    // rather than pretending.
    assert_eq!(body["dangling"], true);

    server.abort();
}

/// Storage on another node is a legal target, and resolution says so.
#[tokio::test]
async fn a_synonym_can_point_off_node() {
    let dir = TempDir::new().unwrap();
    let (state, _v1, _v2) = setup(&dir).await;
    let (base, server) = start(state.clone()).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/api/v1/synonyms"))
        .json(&serde_json::json!({
            "name": "far-golden",
            "uri": "nvme-tcp://forge.g16.lo:4420/nqn.2026-09.lo.g16:stormcos?nsid=1"
        }))
        .send().await.unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["target"]["kind"], "remote");
    assert!(body["target"]["uri"].as_str().unwrap().starts_with("nvme-tcp://"));
    // No local volume, and it is not dangling either — it is elsewhere.
    assert!(body.get("volume").is_none());
    assert!(body.get("dangling").is_none());

    // Both at once is a request nobody meant.
    let resp = client
        .post(format!("{base}/api/v1/synonyms"))
        .json(&serde_json::json!({"name": "both", "uri": "nvme-tcp://h:4420/n", "volume": "golden-1"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 400);

    server.abort();
}

/// The rule, at the door where it is cheapest to say: **writes go to a clone,
/// never to the golden.** A read-write attach of a sealed volume is refused
/// before anything boots onto it, and the refusal names the way forward.
#[tokio::test]
async fn a_golden_refuses_a_read_write_attach_and_offers_the_clone() {
    let dir = TempDir::new().unwrap();
    let (state, v1, _v2) = setup(&dir).await;
    let (base, server) = start(state.clone()).await;
    let client = reqwest::Client::new();

    // Unsealed: a rw attach is fine.
    let resp = client
        .post(format!("{base}/api/v1/volumes/{v1}/attach"))
        .json(&serde_json::json!({"transport": "nvme-tcp"}))
        .send().await.unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());

    // Sealed — it is a golden now.
    state
        .volume_manager
        .lock().await
        .seal_volume(stormblock::volume::VolumeId(v1), None)
        .await
        .unwrap();

    let resp = client
        .post(format!("{base}/api/v1/volumes/{v1}/attach"))
        .json(&serde_json::json!({"transport": "nvme-tcp"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 409);
    let text = resp.text().await.unwrap();
    assert!(text.contains("clone"), "the refusal has to say what to do instead: {text}");

    // Reading it is still fine — that is what a golden is for.
    let resp = client
        .post(format!("{base}/api/v1/volumes/{v1}/attach"))
        .json(&serde_json::json!({"transport": "nvme-tcp", "mode": "ro"}))
        .send().await.unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());

    // So is a read-only volume that is not sealed — a different refusal, and
    // a different way out.
    let resp = client
        .put(format!("{base}/api/v1/volumes/{v1}/access"))
        .json(&serde_json::json!({"access": "ro"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);

    server.abort();
}

/// One golden, a name per consumer, each pointing at that consumer's own
/// writable clone — which is what the whole surface is for.
#[tokio::test]
async fn claiming_a_name_hands_out_a_clone_not_the_golden() {
    let dir = TempDir::new().unwrap();
    let (state, v1, _v2) = setup(&dir).await;
    let (base, server) = start(state.clone()).await;
    let client = reqwest::Client::new();

    client
        .post(format!("{base}/api/v1/synonyms"))
        .json(&serde_json::json!({"namespace": "images", "name": "fedora", "volume": v1.to_string()}))
        .send().await.unwrap();

    // An unsealed target is a moving thing: refused until the caller says so.
    let resp = client
        .post(format!("{base}/api/v1/synonyms/images/fedora/claim"))
        .json(&serde_json::json!({"namespace": "tenant-a", "name": "root"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 409);
    assert!(resp.text().await.unwrap().contains("not sealed"));

    state
        .volume_manager
        .lock().await
        .seal_volume(stormblock::volume::VolumeId(v1), None)
        .await
        .unwrap();

    // Two consumers claim the same golden and get different volumes.
    let mut claimed = Vec::new();
    for ns in ["tenant-a", "tenant-b"] {
        let resp = client
            .post(format!("{base}/api/v1/synonyms/images/fedora/claim"))
            .json(&serde_json::json!({"namespace": ns, "name": "root", "verify": false}))
            .send().await.unwrap();
        assert_eq!(resp.status(), 201, "{ns}");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["claimed_from"]["volume"], v1.to_string());
        assert_eq!(body["volume"]["sealed"], false);
        assert_eq!(body["synonym"]["namespace"], ns);
        claimed.push(body["volume"]["id"].as_str().unwrap().to_string());
    }
    assert_ne!(claimed[0], claimed[1], "each consumer writes to its own clone");
    assert_ne!(claimed[0], v1.to_string(), "and never to the golden");

    // A tenant's own name resolves to its own clone, and that clone is
    // writable where the golden is not.
    let body: serde_json::Value = client
        .get(format!("{base}/api/v1/synonyms/tenant-a/root"))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(body["target"]["id"], claimed[0]);
    assert_eq!(body["volume"]["writable"], true);

    let resp = client
        .post(format!("{base}/api/v1/volumes/{}/attach", claimed[0]))
        .json(&serde_json::json!({"transport": "nvme-tcp"}))
        .send().await.unwrap();
    assert!(resp.status().is_success(), "a clone takes a rw attach: {}", resp.status());

    // Claiming again re-points the tenant's own name at its new clone.
    let resp = client
        .post(format!("{base}/api/v1/synonyms/images/fedora/claim"))
        .json(&serde_json::json!({"namespace": "tenant-a", "name": "root", "verify": false}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["synonym"]["version"], 2);
    assert_ne!(body["volume"]["id"].as_str().unwrap(), claimed[0]);

    server.abort();
}
