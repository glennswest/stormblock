//! #655: the engine's Volume and Replication objects on the cluster's
//! apiserver, and the spec changes asked there. A small apiserver stands in
//! (the CRDs' REST: list, create, get, merge-patch, `/status`, delete).

use crate::common;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};
use stormblock::mgmt::config::{KubeAuthConfig, StormBlockConfig};
use stormblock::mgmt::kube_publish::{pass, Publisher, ENGINE_SPEC};
use stormblock::mgmt::AppState;
use stormblock::volume::{VolumeManager, DEFAULT_EXTENT_SIZE};
use tempfile::TempDir;
use tokio::net::TcpListener;
use uuid::Uuid;

type Store = Arc<Mutex<BTreeMap<(String, String), Value>>>;

fn merge(into: &mut Value, patch: &Value) {
    match (into.as_object_mut(), patch.as_object()) {
        (Some(o), Some(p)) => {
            for (k, v) in p {
                if v.is_null() {
                    o.remove(k);
                } else {
                    merge(o.entry(k.clone()).or_insert(Value::Null), v);
                }
            }
        }
        _ => *into = patch.clone(),
    }
}

fn not_found() -> (StatusCode, Json<Value>) {
    (StatusCode::NOT_FOUND, Json(json!({ "kind": "Status", "code": 404, "message": "not found" })))
}

async fn list(State(s): State<Store>, Path(plural): Path<String>) -> Json<Value> {
    let items: Vec<Value> = s.lock().unwrap().iter().filter(|((p, _), _)| *p == plural).map(|(_, v)| v.clone()).collect();
    Json(json!({ "kind": "List", "items": items }))
}

async fn create(State(s): State<Store>, Path(plural): Path<String>, Json(mut body): Json<Value>) -> (StatusCode, Json<Value>) {
    let name = body["metadata"]["name"].as_str().unwrap().to_string();
    let mut g = s.lock().unwrap();
    if g.contains_key(&(plural.clone(), name.clone())) {
        return (StatusCode::CONFLICT, Json(json!({ "code": 409, "message": "exists" })));
    }
    // The status subresource: a create sets no status.
    body.as_object_mut().unwrap().remove("status");
    g.insert((plural, name), body.clone());
    (StatusCode::CREATED, Json(body))
}

async fn get_one(State(s): State<Store>, Path((plural, name)): Path<(String, String)>) -> (StatusCode, Json<Value>) {
    match s.lock().unwrap().get(&(plural, name)) {
        Some(v) => (StatusCode::OK, Json(v.clone())),
        None => not_found(),
    }
}

async fn patch_main(State(s): State<Store>, Path((plural, name)): Path<(String, String)>, Json(mut p): Json<Value>) -> (StatusCode, Json<Value>) {
    p.as_object_mut().unwrap().remove("status");
    match s.lock().unwrap().get_mut(&(plural, name)) {
        Some(v) => {
            merge(v, &p);
            (StatusCode::OK, Json(v.clone()))
        }
        None => not_found(),
    }
}

async fn patch_status(State(s): State<Store>, Path((plural, name)): Path<(String, String)>, Json(p): Json<Value>) -> (StatusCode, Json<Value>) {
    match s.lock().unwrap().get_mut(&(plural, name)) {
        Some(v) => {
            if v.get("status").is_none() {
                v["status"] = json!({});
            }
            merge(&mut v["status"], &p["status"]);
            (StatusCode::OK, Json(v.clone()))
        }
        None => not_found(),
    }
}

async fn delete_one(State(s): State<Store>, Path((plural, name)): Path<(String, String)>) -> (StatusCode, Json<Value>) {
    match s.lock().unwrap().remove(&(plural, name)) {
        Some(v) => (StatusCode::OK, Json(v)),
        None => not_found(),
    }
}

async fn apiserver() -> (String, Store) {
    let store: Store = Arc::default();
    let g = "/apis/storage.storm.io/v1";
    let app = Router::new()
        .route(&format!("{g}/{{plural}}"), get(list).post(create))
        .route(&format!("{g}/{{plural}}/{{name}}"), get(get_one).patch(patch_main).delete(delete_one))
        .route(&format!("{g}/{{plural}}/{{name}}/status"), axum::routing::patch(patch_status))
        .with_state(store.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    common::wait_for_listener(addr).await;
    (format!("http://{addr}"), store)
}

async fn setup(dir: &TempDir, api_url: &str) -> Arc<AppState> {
    let devices = common::create_file_devices(dir, 1, 2 * 1024 * 1024 * 1024).await;
    let mut vm = VolumeManager::new(DEFAULT_EXTENT_SIZE);
    vm.add_backing_device(stormblock::raid::RaidArrayId(Uuid::new_v4()), devices[0].clone()).await;
    let mut config = StormBlockConfig::default();
    config.management.data_dir = Some(dir.path().to_str().unwrap().to_string());
    config.management.ublk_transport = false;
    config.management.node_name = Some("forge1".into());
    config.management.kubernetes = Some(KubeAuthConfig { api_url: api_url.into(), ca_file: None, token_file: None, cluster_record: None });
    let slab_registry = vm.registry().clone();
    let gem = vm.gem().clone();
    Arc::new(AppState::new(config, vm, slab_registry, gem))
}

fn obj(store: &Store, plural: &str, name: &str) -> Option<Value> {
    store.lock().unwrap().get(&(plural.to_string(), name.to_string())).cloned()
}

fn set_spec(store: &Store, plural: &str, name: &str, k: &str, v: Value) {
    store.lock().unwrap().get_mut(&(plural.to_string(), name.to_string())).unwrap()["spec"][k] = v;
}

/// The Volume published, kept still, grown by a spec asked on the
/// apiserver (the proof's shape on the apiserver's listing), a smaller size
/// refused and put back, the object deleted with the volume; a Replication
/// made on the apiserver for this node runs here and stops when deleted
/// there.
#[tokio::test]
async fn volumes_and_replications_on_the_apiserver() {
    let (api, store) = apiserver().await;
    let dir = TempDir::new().unwrap();
    let state = setup(&dir, &api).await;
    let id = state.volume_manager.lock().await.create_volume_any("registry-data", 4 << 20).await.unwrap();
    let name = id.0.to_string();
    let mut p = Publisher::new();

    let s = pass(&state, &mut p).await.unwrap();
    assert_eq!(s.created, 1, "{s:?}");
    let v = obj(&store, "volumes", &name).expect("published");
    assert_eq!(v["kind"], "Volume");
    assert_eq!(v["metadata"]["labels"]["storm.io/name"], "registry-data");
    assert_eq!(v["metadata"]["labels"]["storm.io/node"], "forge1");
    assert_eq!(v["spec"]["sizeBytes"], 4u64 << 20);
    assert_eq!(v["status"]["capacityBytes"], 4u64 << 20, "{v}");
    let ann: Value = serde_json::from_str(v["metadata"]["annotations"][ENGINE_SPEC].as_str().unwrap()).unwrap();
    assert_eq!(ann["sizeBytes"], 4u64 << 20);

    let s = pass(&state, &mut p).await.unwrap();
    assert_eq!((s.created, s.patched, s.deleted), (0, 0, 0), "nothing changed, nothing written: {s:?}");

    // `sc patch volume <id> -p '{"spec":{"sizeBytes":…}}'` on the apiserver.
    set_spec(&store, "volumes", &name, "sizeBytes", json!(64u64 << 20));
    let s = pass(&state, &mut p).await.unwrap();
    assert_eq!(s.applied, 1, "{s:?}");
    {
        let vm = state.volume_manager.lock().await;
        let h = vm.get_volume_handle(&id).unwrap();
        assert_eq!(stormblock::drive::BlockDevice::capacity_bytes(&*h), 64 << 20, "the engine grew it");
    }
    let v = obj(&store, "volumes", &name).unwrap();
    assert_eq!(v["status"]["capacityBytes"], 64u64 << 20, "{v}");
    assert_eq!(v["status"]["lastResize"]["from"], 4u64 << 20, "{v}");
    let ann: Value = serde_json::from_str(v["metadata"]["annotations"][ENGINE_SPEC].as_str().unwrap()).unwrap();
    assert_eq!(ann["sizeBytes"], 64u64 << 20);
    let listing = serde_json::to_string(&obj(&store, "volumes", &name).unwrap()).unwrap();
    let re = regex_lite(&listing);
    assert!(re, "the proof's shape on the apiserver: {listing}");

    // Smaller: refused, said, and the spec put back.
    set_spec(&store, "volumes", &name, "sizeBytes", json!(8u64 << 20));
    let s = pass(&state, &mut p).await.unwrap();
    assert_eq!(s.refused, 1, "{s:?}");
    let v = obj(&store, "volumes", &name).unwrap();
    assert_eq!(v["spec"]["sizeBytes"], 64u64 << 20, "put back: {v}");
    assert_eq!(v["status"]["specError"]["code"], 422, "{v}");
    assert!(v["status"]["specError"]["message"].as_str().unwrap().contains("never shrinks"), "{v}");

    // Deleting the object on the apiserver deletes nothing: it comes back.
    store.lock().unwrap().remove(&("volumes".to_string(), name.clone()));
    pass(&state, &mut p).await.unwrap();
    assert!(obj(&store, "volumes", &name).is_some(), "written again");
    assert!(state.volume_manager.lock().await.get_volume_handle(&id).is_some());

    // Another node's object is left alone.
    store.lock().unwrap().insert(
        ("volumes".into(), "other".into()),
        json!({ "metadata": { "name": "other", "labels": { "storm.io/node": "server8" } }, "spec": {} }),
    );

    // A Replication made on the apiserver for this node.
    store.lock().unwrap().insert(
        ("replications".into(), "forge".into()),
        json!({ "apiVersion": "storage.storm.io/v1", "kind": "Replication",
                "metadata": { "name": "forge", "labels": { "storm.io/node": "forge1" } },
                "spec": { "from": { "engine": "http://127.0.0.1:1" }, "what": ["goldens"], "mode": "once" } }),
    );
    let s = pass(&state, &mut p).await.unwrap();
    assert_eq!(s.applied, 1, "{s:?}");
    let r = stormblock::mgmt::replication::get(&state, "forge").expect("taken by this engine");
    assert_eq!(r.spec.from.engine.as_deref(), Some("http://127.0.0.1:1"));
    let o = obj(&store, "replications", "forge").unwrap();
    assert!(o["metadata"]["annotations"][ENGINE_SPEC].is_string(), "{o}");
    assert!(o["status"]["phase"].is_string(), "{o}");

    // A spec that cannot run is refused on its status.
    store.lock().unwrap().insert(
        ("replications".into(), "bad".into()),
        json!({ "metadata": { "name": "bad", "labels": { "storm.io/node": "forge1" } }, "spec": { "from": {}, "mode": "sometimes" } }),
    );
    let s = pass(&state, &mut p).await.unwrap();
    assert!(s.refused >= 1, "{s:?}");
    assert!(stormblock::mgmt::replication::get(&state, "bad").is_none());
    let o = obj(&store, "replications", "bad").unwrap();
    assert_eq!(o["status"]["phase"], "Failed", "{o}");
    assert!(o["status"]["specError"]["message"].is_string(), "{o}");
    store.lock().unwrap().remove(&("replications".to_string(), "bad".to_string()));

    // Deleted on the apiserver: deleted here, and not written again.
    store.lock().unwrap().remove(&("replications".to_string(), "forge".to_string()));
    pass(&state, &mut p).await.unwrap();
    assert!(stormblock::mgmt::replication::get(&state, "forge").is_none(), "stopped here");
    pass(&state, &mut p).await.unwrap();
    assert!(obj(&store, "replications", "forge").is_none());

    // The volume deleted here: its object goes; the other node's stays.
    state.volume_manager.lock().await.delete_volume(id).await.unwrap();
    let s = pass(&state, &mut p).await.unwrap();
    assert_eq!(s.deleted, 1, "{s:?}");
    assert!(obj(&store, "volumes", &name).is_none());
    assert!(obj(&store, "volumes", "other").is_some());
}

/// `"storm.io/name":"registry-data".*"sizeBytes":[0-9]{8,}` without a regex
/// crate: the label, then a sizeBytes of eight digits or more after it.
fn regex_lite(text: &str) -> bool {
    let Some(at) = text.find(r#""storm.io/name":"registry-data""#) else { return false };
    text[at..].match_indices(r#""sizeBytes":"#).any(|(i, m)| {
        let rest = &text[at + i + m.len()..];
        rest.chars().take_while(|c| c.is_ascii_digit()).count() >= 8
    })
}
