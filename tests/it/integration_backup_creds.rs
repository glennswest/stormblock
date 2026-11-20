//! Per-node backup credentials on a store, end to end over HTTP (#693,
//! stormcos#1027, the owner's B): minted by the admin only, a bearer that
//! reaches its own node's backups and nothing else, revoked per node.

use crate::common;
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
const MIB: u64 = 1 << 20;

async fn store(dir: &TempDir) -> (String, tokio::task::JoinHandle<()>) {
    let devices = common::create_file_devices(dir, 2, 64 * MIB).await;
    let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();
    let array_id = array.array_id();
    let backing: Arc<dyn BlockDevice> = Arc::new(array);
    let mut vm = VolumeManager::new(SLOT);
    vm.add_backing_device(array_id, backing).await;
    let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
    let data = dir.path().join("engine");
    std::fs::create_dir_all(&data).unwrap();
    let mut c = StormBlockConfig::default();
    c.management.node_name = Some("store".into());
    c.management.api_token = Some("node".into());
    c.management.admin_token = Some("root".into());
    c.management.data_dir = Some(data.display().to_string());
    // The /v1 contract's transport is nvme_tcp (see integration_v1_api).
    c.management.ublk_transport = false;
    c.management.topology.insert("zone".to_string(), "z1".to_string());
    let state = Arc::new(AppState::new(c, vm, reg, gem));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = stormblock::mgmt::api::router(state);
    let h = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (format!("http://{addr}"), h)
}

struct Api {
    c: reqwest::Client,
    base: String,
}

impl Api {
    async fn call(&self, method: &str, path: &str, bearer: &str, body: Option<Value>) -> (u16, Value) {
        let m = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let mut r = self.c.request(m, format!("{}{path}", self.base)).bearer_auth(bearer);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let resp = r.send().await.unwrap();
        let s = resp.status().as_u16();
        let v = resp.json().await.unwrap_or(Value::Null);
        (s, v)
    }
}

fn target(name: &str) -> Value {
    json!({
        "name": name,
        "size_bytes": 8 * MIB,
        "replica_tier": { "slaves": 0 },
        "owner": { "kind": "VirtualMachineBackup", "namespace": "default", "name": "vm" },
    })
}

#[tokio::test]
async fn a_backup_credential_reaches_its_own_nodes_backups_and_nothing_else() {
    let dir = TempDir::new().unwrap();
    let (base, server) = store(&dir).await;
    let api = Api { c: reqwest::Client::new(), base };

    // Minted by the admin only, the bearer answered once.
    let (s, _) = api.call("POST", "/api/v1/backup-credentials", "node", Some(json!({"node": "server8"}))).await;
    assert_eq!(s, 401, "the node token mints no backup credential");
    let (s, v) = api.call("POST", "/api/v1/backup-credentials", "root", Some(json!({"node": "server8"}))).await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(v["principal"], "backup:server8");
    let b8 = v["token"].as_str().unwrap().to_string();
    let (_, v) = api.call("POST", "/api/v1/backup-credentials", "root", Some(json!({"node": "server9"}))).await;
    let b9 = v["token"].as_str().unwrap().to_string();
    let (s, v) = api.call("POST", "/api/v1/backup-credentials", "root", Some(json!({"node": "Bad_Name"}))).await;
    assert_eq!(s, 400, "{v}");

    // A target of its own.
    let (s, v) = api.call("POST", "/v1/volumes", &b8, Some(target("default.vm.root.backup"))).await;
    assert!((200..300).contains(&s), "{s} {v}");
    let vol8 = v["id"].as_str().unwrap().to_string();
    let (s, v) = api.call("GET", "/v1/volumes/default.vm.root.backup", &b8, None).await;
    assert_eq!((s, v["id"].as_str()), (200, Some(vol8.as_str())), "by name: {v}");
    let (s, _) = api.call("GET", &format!("/v1/volumes/{vol8}"), &b8, None).await;
    assert_eq!(s, 200);

    // Only a blank backup volume: an owner of the backup kind, no source,
    // not secret.
    let mut t = target("default.vm.other.backup");
    t["owner"]["kind"] = json!("VirtualMachine");
    let (s, v) = api.call("POST", "/v1/volumes", &b8, Some(t)).await;
    assert_eq!(s, 403, "{v}");
    let mut t = target("default.vm.other.backup");
    t["source"] = json!({ "volume_id": vol8 });
    assert_eq!(api.call("POST", "/v1/volumes", &b8, Some(t)).await.0, 403, "no clone");
    let mut t = target("default.vm.other.backup");
    t["secret"] = json!(true);
    assert_eq!(api.call("POST", "/v1/volumes", &b8, Some(t)).await.0, 403, "no secret volume");

    // Another node's bearer reaches none of it, and takes no name it holds.
    let (s, v) = api.call("GET", &format!("/v1/volumes/{vol8}"), &b9, None).await;
    assert_eq!(s, 403, "{v}");
    assert!(v["message"].as_str().unwrap().contains("backup:server9"), "{v}");
    assert_eq!(api.call("GET", "/v1/volumes/default.vm.root.backup", &b9, None).await.0, 403);
    assert_eq!(api.call("POST", "/v1/volumes", &b9, Some(target("default.vm.root.backup"))).await.0, 409);
    assert_eq!(api.call("DELETE", &format!("/v1/volumes/{vol8}"), &b9, None).await.0, 403);
    assert_eq!(api.call("DELETE", &format!("/api/v1/volumes/{vol8}"), &b9, None).await.0, 403);
    let attach = json!({ "node": "store", "mode": "read_write", "transport": "nvme_tcp", "host_nqn": "nqn.x:server9" });
    assert_eq!(api.call("POST", &format!("/v1/volumes/{vol8}/attach"), &b9, Some(attach)).await.0, 403);
    // Its own, over nvme_tcp only.
    let ublk = json!({ "node": "store", "mode": "read_write", "transport": "ublk" });
    let (s, v) = api.call("POST", &format!("/v1/volumes/{vol8}/attach"), &b8, Some(ublk)).await;
    assert_eq!(s, 403, "{v}");

    // Nor a volume the store made for itself.
    let (s, v) = api.call("POST", "/v1/volumes", "node", Some(json!({"name": "store-own", "size_bytes": 8 * MIB, "replica_tier": {"slaves": 0}}))).await;
    assert!((200..300).contains(&s), "{v}");
    let own = v["id"].as_str().unwrap().to_string();
    assert_eq!(api.call("GET", &format!("/v1/volumes/{own}"), &b8, None).await.0, 403);
    assert_eq!(api.call("DELETE", &format!("/v1/volumes/{own}"), &b8, None).await.0, 403);
    let (s, _) = api.call("POST", "/v1/group-snapshots", &b8, Some(json!({"name": "x", "volume_ids": [own]}))).await;
    assert_eq!(s, 403, "a point of a volume not its own");

    // Points: its own volumes only; the list is its own.
    let (s, v) = api.call("POST", "/v1/group-snapshots", &b8, Some(json!({"name": "default.vm.backup.b1", "volume_ids": [vol8]}))).await;
    assert!((200..300).contains(&s), "{s} {v}");
    let g8 = v["id"].as_str().unwrap().to_string();
    let (_, v) = api.call("POST", "/v1/volumes", &b9, Some(target("default.vm9.root.backup"))).await;
    let vol9 = v["id"].as_str().unwrap().to_string();
    let (s, _) = api.call("POST", "/v1/group-snapshots", &b9, Some(json!({"name": "default.vm9.backup.b1", "volume_ids": [vol8]}))).await;
    assert_eq!(s, 403, "a point of another node's volume");
    let (s, v) = api.call("POST", "/v1/group-snapshots", &b9, Some(json!({"name": "default.vm.backup.b1", "volume_ids": [vol9]}))).await;
    assert_eq!(s, 409, "a point name another holds: {v}");
    let (_, v) = api.call("POST", "/v1/group-snapshots", &b9, Some(json!({"name": "default.vm9.backup.b1", "volume_ids": [vol9]}))).await;
    let g9 = v["id"].as_str().unwrap().to_string();
    let snap9 = v["snapshots"][0]["id"].as_str().unwrap().to_string();
    let names = |v: &Value| v.as_array().unwrap().iter().map(|g| g["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let (s, v) = api.call("GET", "/v1/group-snapshots", &b8, None).await;
    assert_eq!((s, names(&v)), (200, vec!["default.vm.backup.b1".to_string()]));
    let (_, v) = api.call("GET", "/v1/group-snapshots", &b9, None).await;
    assert_eq!(names(&v), vec!["default.vm9.backup.b1".to_string()]);
    let (_, v) = api.call("GET", "/v1/group-snapshots", "node", None).await;
    assert_eq!(v.as_array().unwrap().len(), 2, "the node token sees every point");
    assert_eq!(api.call("GET", &format!("/v1/group-snapshots/{g9}"), &b8, None).await.0, 403);
    assert_eq!(api.call("DELETE", &format!("/v1/group-snapshots/{g9}"), &b8, None).await.0, 403);
    assert_eq!(api.call("GET", &format!("/v1/group-snapshots/{g8}"), &b8, None).await.0, 200);
    // A restore ships a point of its own only.
    let ship = json!({ "target_uri": "nvme-tcp://127.0.0.1:1/nqn.x?nsid=1", "max_bytes_per_sec": 0 });
    let (s, v) = api.call("POST", &format!("/v1/snapshots/{snap9}/ship"), &b8, Some(ship)).await;
    assert_eq!(s, 403, "{v}");

    // Nothing else, whatever it is.
    for (m, p) in [
        ("GET", "/api/v1/volumes"),
        ("GET", "/v1/volumes"),
        ("POST", "/api/v1/backup-credentials"),
        ("GET", "/api/v1/backup-credentials"),
        ("DELETE", "/api/v1/backup-credentials/server9"),
        ("PUT", "/api/v1/synonyms/boothost/server8"),
        ("GET", "/api/v1/slabs"),
        ("POST", "/v1/snapshots"),
    ] {
        let (s, v) = api.call(m, p, &b8, Some(json!({}))).await;
        assert_eq!(s, 403, "{m} {p}: {v}");
    }

    // Prune its own point, and its own volume.
    assert!((200..300).contains(&api.call("DELETE", &format!("/v1/group-snapshots/{g8}"), &b8, None).await.0));
    assert!((200..300).contains(&api.call("DELETE", &format!("/v1/volumes/{vol8}"), &b8, None).await.0));

    // Who holds one, and when it was used: the node token reads it.
    let (s, v) = api.call("GET", "/api/v1/backup-credentials", "node", None).await;
    assert_eq!(s, 200, "{v}");
    let items = v["items"].as_array().unwrap();
    let h9 = items.iter().find(|i| i["node"] == "server9").unwrap();
    assert!(h9["uses"].as_u64().unwrap() >= 4 && h9["last_used_at"].as_u64().is_some(), "{h9}");
    assert!(h9["objects"].as_u64().unwrap() >= 3, "its volume, point and the point's member: {h9}");
    assert!(!v.to_string().contains(&b9), "a bearer is never listed");

    // Revoked per node, by the admin: server8's stops, server9's works.
    assert_eq!(api.call("DELETE", "/api/v1/backup-credentials/server8", "node", None).await.0, 401);
    assert_eq!(api.call("DELETE", "/api/v1/backup-credentials/server8", "root", None).await.0, 200);
    assert_eq!(api.call("DELETE", "/api/v1/backup-credentials/server8", "root", None).await.0, 404);
    assert_eq!(api.call("GET", "/v1/group-snapshots", &b8, None).await.0, 401);
    assert_eq!(api.call("GET", &format!("/v1/group-snapshots/{g9}"), &b9, None).await.0, 200);

    server.abort();
}
