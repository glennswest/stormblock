//! /v1 CSI-contract API integration tests.
//!
//! Ports the MockEngine spec tests from stormblock-csi (mock.rs is the
//! executable spec) against the real axum surface, plus engine-backed COW
//! verification the mock can't do.

mod common;

use std::sync::Arc;

use serde_json::{json, Value};
use stormblock::drive::BlockDevice;
use stormblock::mgmt::config::StormBlockConfig;
use stormblock::mgmt::AppState;
use stormblock::raid::{RaidArray, RaidLevel};
use stormblock::volume::VolumeManager;

use tempfile::TempDir;
use tokio::net::TcpListener;

const SLOT: u64 = 4096;

/// Slab-backed state whose local node is "w1".
async fn setup_state(dir: &TempDir) -> Arc<AppState> {
    let devices = common::create_file_devices(dir, 2, 64 * 1024 * 1024).await;
    let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();
    let array_id = array.array_id();
    let backing: Arc<dyn BlockDevice> = Arc::new(array);

    let mut vm = VolumeManager::new(SLOT);
    vm.add_backing_device(array_id, backing).await;

    let mut config = StormBlockConfig::default();
    config.management.node_name = Some("w1".to_string());
    config
        .management
        .topology
        .insert("zone".to_string(), "z1".to_string());

    let slab_registry = vm.registry().clone();
    let gem = vm.gem().clone();
    Arc::new(AppState::new(config, vm, slab_registry, gem))
}

async fn start_server(state: Arc<AppState>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let router = stormblock::mgmt::api::router(state);
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    common::wait_for_listener(addr).await;
    (base_url, handle)
}

fn create_req(name: &str, size: u64, slaves: u8) -> Value {
    json!({
        "name": name,
        "size_bytes": size,
        "replica_tier": { "slaves": slaves },
    })
}

async fn post(client: &reqwest::Client, url: String, body: Value) -> (u16, Value) {
    let resp = client.post(url).json(&body).send().await.unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

#[tokio::test]
async fn v1_create_is_idempotent_by_name() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    let (base, server) = start_server(state).await;
    let c = reqwest::Client::new();

    let (s, a) = post(&c, format!("{base}/v1/volumes"), create_req("pvc-1", 1 << 20, 0)).await;
    assert_eq!(s, 200, "{a}");
    let (s, b) = post(&c, format!("{base}/v1/volumes"), create_req("pvc-1", 1 << 20, 0)).await;
    assert_eq!(s, 200);
    assert_eq!(a["id"], b["id"]);

    // Same name + different size → 409 already_exists envelope.
    let (s, e) = post(&c, format!("{base}/v1/volumes"), create_req("pvc-1", 2 << 20, 0)).await;
    assert_eq!(s, 409);
    assert_eq!(e["code"], "already_exists");

    server.abort();
}

#[tokio::test]
async fn v1_pair_lands_on_two_nodes_and_hint_honored() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    {
        let mut v1 = state.v1.lock().await;
        v1.add_node("w2", 100 << 30, Default::default());
        v1.add_node("w3", 100 << 30, Default::default());
    }
    let (base, server) = start_server(state).await;
    let c = reqwest::Client::new();

    let (s, v) = post(&c, format!("{base}/v1/volumes"), create_req("pvc-1", 1 << 20, 1)).await;
    assert_eq!(s, 200, "{v}");
    let replicas = v["replicas"].as_array().unwrap();
    assert_eq!(replicas.len(), 2);
    let master = replicas.iter().find(|r| r["role"] == "master").unwrap();
    let slave = replicas.iter().find(|r| r["role"] == "slave").unwrap();
    assert_ne!(master["node"], slave["node"]);

    // Master hint honored.
    let mut req = create_req("pvc-2", 1 << 20, 1);
    req["master_node"] = json!("w2");
    let (s, v) = post(&c, format!("{base}/v1/volumes"), req).await;
    assert_eq!(s, 200, "{v}");
    let master = v["replicas"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["role"] == "master")
        .unwrap();
    assert_eq!(master["node"], "w2");

    server.abort();
}

#[tokio::test]
async fn v1_fence_then_promote_bumps_epoch_and_moves_master() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    {
        let mut v1 = state.v1.lock().await;
        v1.add_node("w2", 100 << 30, Default::default());
    }
    let (base, server) = start_server(state).await;
    let c = reqwest::Client::new();

    let (_, v) = post(&c, format!("{base}/v1/volumes"), create_req("pvc-1", 1 << 20, 1)).await;
    let id = v["id"].as_str().unwrap().to_string();
    let epoch = v["epoch"].as_u64().unwrap();
    let slave = v["replicas"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["role"] == "slave")
        .unwrap()["node"]
        .as_str()
        .unwrap()
        .to_string();

    // Promote without fencing → 412 stale_epoch with current_epoch.
    let (s, e) = post(
        &c,
        format!("{base}/v1/volumes/{id}/promote"),
        json!({ "target_node": slave, "fenced_epoch": epoch + 1 }),
    )
    .await;
    assert_eq!(s, 412);
    assert_eq!(e["code"], "stale_epoch");
    assert_eq!(e["current_epoch"].as_u64().unwrap(), epoch);

    // Fence is a CAS: expected epoch matches → bumped.
    let (s, f) = post(
        &c,
        format!("{base}/v1/volumes/{id}/fence"),
        json!({ "expected_epoch": epoch }),
    )
    .await;
    assert_eq!(s, 200);
    let fenced = f["epoch"].as_u64().unwrap();
    assert_eq!(fenced, epoch + 1);

    let (s, p) = post(
        &c,
        format!("{base}/v1/volumes/{id}/promote"),
        json!({ "target_node": slave, "fenced_epoch": fenced }),
    )
    .await;
    assert_eq!(s, 200, "{p}");
    let master = p["replicas"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["role"] == "master")
        .unwrap();
    assert_eq!(master["node"].as_str().unwrap(), slave);
    assert_eq!(p["health"], "degraded");

    // Double-fence with the old epoch fails (zombie tiebreaker guard).
    let (s, e) = post(
        &c,
        format!("{base}/v1/volumes/{id}/fence"),
        json!({ "expected_epoch": epoch }),
    )
    .await;
    assert_eq!(s, 412);
    assert_eq!(e["code"], "stale_epoch");

    server.abort();
}

#[tokio::test]
async fn v1_rw_attach_only_on_master() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    {
        let mut v1 = state.v1.lock().await;
        v1.add_node("w2", 100 << 30, Default::default());
    }
    let (base, server) = start_server(state).await;
    let c = reqwest::Client::new();

    let (_, v) = post(&c, format!("{base}/v1/volumes"), create_req("pvc-1", 1 << 20, 1)).await;
    let id = v["id"].as_str().unwrap();
    let replicas = v["replicas"].as_array().unwrap();
    let master = replicas.iter().find(|r| r["role"] == "master").unwrap()["node"]
        .as_str()
        .unwrap();
    let slave = replicas.iter().find(|r| r["role"] == "slave").unwrap()["node"]
        .as_str()
        .unwrap();

    // Wrong node → 409 (the engine-side wrong-node-pod gate).
    let (s, _) = post(
        &c,
        format!("{base}/v1/volumes/{id}/attach"),
        json!({ "node": slave, "mode": "read_write" }),
    )
    .await;
    assert_eq!(s, 409);

    let (s, info) = post(
        &c,
        format!("{base}/v1/volumes/{id}/attach"),
        json!({ "node": master, "mode": "read_write" }),
    )
    .await;
    assert_eq!(s, 200, "{info}");
    assert_eq!(info["transport"], "nvme_tcp");
    assert!(info["nqn"].as_str().unwrap().contains(id));
    assert!(!info["addresses"].as_array().unwrap().is_empty());

    server.abort();
}

#[tokio::test]
async fn v1_dual_attach_commit_promotes_target() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    {
        let mut v1 = state.v1.lock().await;
        v1.add_node("w2", 100 << 30, Default::default());
    }
    let (base, server) = start_server(state).await;
    let c = reqwest::Client::new();

    let (_, v) = post(&c, format!("{base}/v1/volumes"), create_req("pvc-1", 1 << 20, 1)).await;
    let id = v["id"].as_str().unwrap().to_string();
    let slave = v["replicas"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["role"] == "slave")
        .unwrap()["node"]
        .as_str()
        .unwrap()
        .to_string();

    // Migration-target attach requires the window.
    let (s, _) = post(
        &c,
        format!("{base}/v1/volumes/{id}/attach"),
        json!({ "node": slave, "mode": "migration_target" }),
    )
    .await;
    assert_eq!(s, 409);

    let (s, w) = post(
        &c,
        format!("{base}/v1/volumes/{id}/dual-attach"),
        json!({ "target_node": slave, "ttl_secs": 300 }),
    )
    .await;
    assert_eq!(s, 200, "{w}");
    let epoch = w["epoch"].as_u64().unwrap();

    // Idempotent reopen for the same target.
    let (s, w2) = post(
        &c,
        format!("{base}/v1/volumes/{id}/dual-attach"),
        json!({ "target_node": slave, "ttl_secs": 300 }),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(w["expires_at_ms"], w2["expires_at_ms"]);

    let (s, _) = post(
        &c,
        format!("{base}/v1/volumes/{id}/attach"),
        json!({ "node": slave, "mode": "migration_target" }),
    )
    .await;
    assert_eq!(s, 200);

    // Promotion is blocked while the window is open.
    let (s, _) = post(
        &c,
        format!("{base}/v1/volumes/{id}/promote"),
        json!({ "target_node": slave, "fenced_epoch": epoch }),
    )
    .await;
    assert_eq!(s, 409);

    // Commit = fence + promote: sub-second cutover.
    let (s, after) = post(
        &c,
        format!("{base}/v1/volumes/{id}/dual-attach/close"),
        json!({ "epoch": epoch, "outcome": "commit" }),
    )
    .await;
    assert_eq!(s, 200, "{after}");
    let master = after["replicas"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["role"] == "master")
        .unwrap();
    assert_eq!(master["node"].as_str().unwrap(), slave);
    assert_eq!(after["epoch"].as_u64().unwrap(), epoch + 1);

    server.abort();
}

#[tokio::test]
async fn v1_dual_attach_abort_restores_normal_state() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    {
        let mut v1 = state.v1.lock().await;
        v1.add_node("w2", 100 << 30, Default::default());
    }
    let (base, server) = start_server(state.clone()).await;
    let c = reqwest::Client::new();

    let (_, v) = post(&c, format!("{base}/v1/volumes"), create_req("pvc-1", 1 << 20, 1)).await;
    let id = v["id"].as_str().unwrap().to_string();
    let master = v["replicas"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["role"] == "master")
        .unwrap()["node"]
        .as_str()
        .unwrap()
        .to_string();
    let slave = v["replicas"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["role"] == "slave")
        .unwrap()["node"]
        .as_str()
        .unwrap()
        .to_string();
    let epoch = v["epoch"].as_u64().unwrap();

    let (_, w) = post(
        &c,
        format!("{base}/v1/volumes/{id}/dual-attach"),
        json!({ "target_node": slave, "ttl_secs": 300 }),
    )
    .await;
    let (s, _) = post(
        &c,
        format!("{base}/v1/volumes/{id}/attach"),
        json!({ "node": slave, "mode": "migration_target" }),
    )
    .await;
    assert_eq!(s, 200);

    let (s, after) = post(
        &c,
        format!("{base}/v1/volumes/{id}/dual-attach/close"),
        json!({ "epoch": w["epoch"].as_u64().unwrap(), "outcome": "abort" }),
    )
    .await;
    assert_eq!(s, 200, "{after}");
    let m = after["replicas"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["role"] == "master")
        .unwrap();
    assert_eq!(m["node"].as_str().unwrap(), master);
    assert_eq!(after["epoch"].as_u64().unwrap(), epoch);

    // Target attach dropped.
    let v1 = state.v1.lock().await;
    assert!(!v1
        .attachments
        .get(&id)
        .map(|nodes| nodes.contains(&slave))
        .unwrap_or(false));

    server.abort();
}

#[tokio::test]
async fn v1_prestage_replaces_slave_and_degrades() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    {
        let mut v1 = state.v1.lock().await;
        v1.add_node("w2", 100 << 30, Default::default());
        v1.add_node("w3", 100 << 30, Default::default());
    }
    let (base, server) = start_server(state).await;
    let c = reqwest::Client::new();

    let mut req = create_req("pvc-1", 1 << 20, 1);
    req["master_node"] = json!("w1");
    let (_, v) = post(&c, format!("{base}/v1/volumes"), req).await;
    let id = v["id"].as_str().unwrap();
    let old_slave = v["replicas"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["role"] == "slave")
        .unwrap()["node"]
        .as_str()
        .unwrap();
    let new_slave = if old_slave == "w2" { "w3" } else { "w2" };

    let (s, after) = post(
        &c,
        format!("{base}/v1/volumes/{id}/prestage"),
        json!({ "node": new_slave }),
    )
    .await;
    assert_eq!(s, 200, "{after}");
    assert_eq!(after["health"], "degraded");
    let slave = after["replicas"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["role"] == "slave")
        .unwrap();
    assert_eq!(slave["node"], new_slave);
    // Resync progress/lag exposed: the failover-exposure window.
    assert_eq!(slave["sync"]["state"], "resyncing");
    assert!(slave["sync"]["lag_bytes"].as_u64().unwrap() > 0);

    // Anti-affinity is mandatory.
    let (s, _) = post(
        &c,
        format!("{base}/v1/volumes/{id}/placement"),
        json!({ "master_node": "w1", "slave_node": "w1" }),
    )
    .await;
    assert_eq!(s, 409);

    server.abort();
}

#[tokio::test]
async fn v1_snapshot_clone_flow_with_cow_divergence() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    let (base, server) = start_server(state.clone()).await;
    let c = reqwest::Client::new();

    // Local master ("w1" is this node) → backed by a real thin volume.
    let (s, v) = post(&c, format!("{base}/v1/volumes"), create_req("golden", 1 << 20, 0)).await;
    assert_eq!(s, 200, "{v}");
    let vol_id = v["id"].as_str().unwrap().to_string();

    // Write through the engine binding.
    let backing = {
        let v1 = state.v1.lock().await;
        v1.volumes[&vol_id].local_id.expect("local master is engine-backed")
    };
    let engine_vol = {
        let vm = state.volume_manager.lock().await;
        vm.get_volume(&stormblock::volume::VolumeId(backing)).unwrap()
    };
    engine_vol.write(0, &vec![0xAA_u8; SLOT as usize]).await.unwrap();

    // Snapshot via the API (idempotent by name).
    let (s, snap) = post(
        &c,
        format!("{base}/v1/snapshots"),
        json!({ "name": "golden-snap", "volume_id": vol_id }),
    )
    .await;
    assert_eq!(s, 200, "{snap}");
    let snap_id = snap["id"].as_str().unwrap().to_string();
    let (s, snap2) = post(
        &c,
        format!("{base}/v1/snapshots"),
        json!({ "name": "golden-snap", "volume_id": vol_id }),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(snap["id"], snap2["id"]);

    // Clone from the snapshot (the M2 clone-and-attach disk half).
    let mut clone_req = create_req("agent-fork-1", 1 << 20, 0);
    clone_req["source"] = json!({ "kind": "snapshot", "id": snap_id });
    let (s, clone) = post(&c, format!("{base}/v1/volumes"), clone_req).await;
    assert_eq!(s, 200, "{clone}");
    assert_ne!(clone["id"], v["id"]);

    // Diverge the parent; the clone must keep the frozen data.
    engine_vol.write(0, &vec![0xBB_u8; SLOT as usize]).await.unwrap();
    let clone_backing = {
        let v1 = state.v1.lock().await;
        v1.volumes[clone["id"].as_str().unwrap()].local_id.unwrap()
    };
    let clone_vol = {
        let vm = state.volume_manager.lock().await;
        vm.get_volume(&stormblock::volume::VolumeId(clone_backing)).unwrap()
    };
    let mut buf = vec![0u8; SLOT as usize];
    clone_vol.read(0, &mut buf).await.unwrap();
    assert!(buf.iter().all(|&b| b == 0xAA), "clone must not see parent's new writes");

    // Idempotent snapshot delete.
    let del = |id: String| {
        let c = c.clone();
        let base = base.clone();
        async move {
            c.delete(format!("{base}/v1/snapshots/{id}"))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }
    };
    assert_eq!(del(snap_id.clone()).await, 200);
    assert_eq!(del(snap_id).await, 200);

    server.abort();
}

#[tokio::test]
async fn v1_group_snapshot_is_atomic_and_idempotent() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    let (base, server) = start_server(state).await;
    let c = reqwest::Client::new();

    let (_, a) = post(&c, format!("{base}/v1/volumes"), create_req("data", 1 << 20, 0)).await;
    let (_, b) = post(&c, format!("{base}/v1/volumes"), create_req("wal", 1 << 20, 0)).await;

    let (s, g1) = post(
        &c,
        format!("{base}/v1/group-snapshots"),
        json!({ "name": "backup-1", "volume_ids": [a["id"], b["id"]] }),
    )
    .await;
    assert_eq!(s, 200, "{g1}");
    let snaps = g1["snapshots"].as_array().unwrap();
    assert_eq!(snaps.len(), 2);
    // Single consistency point.
    assert_eq!(snaps[0]["created_at_ms"], snaps[1]["created_at_ms"]);
    assert_eq!(snaps[0]["group_snapshot_id"], g1["id"]);

    // Idempotent by name.
    let (s, g2) = post(
        &c,
        format!("{base}/v1/group-snapshots"),
        json!({ "name": "backup-1", "volume_ids": [a["id"], b["id"]] }),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(g1["id"], g2["id"]);

    // Delete removes member snapshots too.
    let gid = g1["id"].as_str().unwrap();
    let s = c
        .delete(format!("{base}/v1/group-snapshots/{gid}"))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(s, 200);
    let listed: Vec<Value> = c
        .get(format!("{base}/v1/snapshots"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(listed.is_empty());

    server.abort();
}

#[tokio::test]
async fn v1_capacity_topology_and_delete_idempotency() {
    let dir = TempDir::new().unwrap();
    let state = setup_state(&dir).await;
    let (base, server) = start_server(state).await;
    let c = reqwest::Client::new();

    // Local node reported live from the slab registry with topology labels.
    let nodes: Vec<Value> = c
        .get(format!("{base}/v1/nodes/capacity"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let w1 = nodes.iter().find(|n| n["node"] == "w1").expect("local node listed");
    assert!(w1["total_bytes"].as_u64().unwrap() > 0);
    assert!(w1["free_bytes"].as_u64().unwrap() > 0);
    assert_eq!(w1["topology"]["zone"], "z1");

    let one: Value = c
        .get(format!("{base}/v1/nodes/w1/capacity"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(one["node"], "w1");

    let s = c
        .get(format!("{base}/v1/nodes/nope/capacity"))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(s, 404);

    // DELETE of an absent volume succeeds; detach replays are no-ops.
    let s = c
        .delete(format!("{base}/v1/volumes/vol-never-existed"))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(s, 200);
    let (s, _) = post(
        &c,
        format!("{base}/v1/volumes/vol-never-existed/detach"),
        json!({ "node": "w1" }),
    )
    .await;
    assert_eq!(s, 200);

    server.abort();
}

#[tokio::test]
async fn v1_bearer_auth_enforced_when_configured() {
    let dir = TempDir::new().unwrap();
    let devices = common::create_file_devices(&dir, 2, 16 * 1024 * 1024).await;
    let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();
    let array_id = array.array_id();
    let mut vm = VolumeManager::new(SLOT);
    vm.add_backing_device(array_id, Arc::new(array)).await;

    let mut config = StormBlockConfig::default();
    config.management.node_name = Some("w1".to_string());
    config.management.api_token = Some("sekrit".to_string());
    let slab_registry = vm.registry().clone();
    let gem = vm.gem().clone();
    let state = Arc::new(AppState::new(config, vm, slab_registry, gem));
    let (base, server) = start_server(state).await;
    let c = reqwest::Client::new();

    let s = c
        .get(format!("{base}/v1/volumes"))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(s, 401);

    let s = c
        .get(format!("{base}/v1/volumes"))
        .bearer_auth("wrong")
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(s, 401);

    let s = c
        .get(format!("{base}/v1/volumes"))
        .bearer_auth("sekrit")
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(s, 200);

    // Legacy /api/v1 surface stays open (unchanged behavior).
    let s = c
        .get(format!("{base}/api/v1/volumes"))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(s, 200);

    server.abort();
}
