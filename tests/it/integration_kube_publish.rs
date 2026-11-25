//! #655: the engine's Volume and Replication objects on the cluster's
//! apiserver, and the spec changes asked there; #743: Golden objects made
//! here. A small apiserver stands in
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

/// Until the Golden's phase is no longer `Making`: the make runs on a task.
async fn settled(store: &Store, name: &str) -> Value {
    for _ in 0..600 {
        if let Some(o) = obj(store, "goldens", name) {
            if !matches!(o["status"]["phase"].as_str(), None | Some("Making")) {
                return o;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("Golden {name} never left Making: {:?}", obj(store, "goldens", name));
}

fn golden_object(name: &str, node: Option<&str>, spec: Value) -> Value {
    let labels = match node {
        Some(n) => json!({ "storm.io/node": n }),
        None => json!({}),
    };
    json!({ "apiVersion": "storage.storm.io/v1", "kind": "Golden", "metadata": { "name": name, "labels": labels }, "spec": spec })
}

fn put(store: &Store, plural: &str, name: &str, v: Value) {
    store.lock().unwrap().insert((plural.to_string(), name.to_string()), v);
}

/// #743 (stormfn#62): a Golden created on the apiserver for this node is
/// made here from its volume, and its files read out of the golden,
/// bounded, into its status; acted on once a spec, again when the spec
/// changes; a spec that cannot be done, a missing volume, an unlabelled
/// object and another node's are each left as the contract says.
#[tokio::test]
async fn a_golden_asked_on_the_apiserver_is_made_and_its_files_read() {
    use base64::Engine;
    use sha2::Digest;
    use stormblock::fs::{ext4, files::SeedFile};
    let (api, store) = apiserver().await;
    let dir = TempDir::new().unwrap();
    let state = setup(&dir, &api).await;
    let id = state.volume_manager.lock().await.create_volume_any("pvc-shop-checkout-build", 64 << 20).await.unwrap();
    let dev = state.volume_manager.lock().await.get_volume(&id).unwrap();
    ext4::format(&dev, &ext4::Ext4Params::default()).await.unwrap();
    let deps = b"requests==2.32.0\nflask==3.0.0\n".to_vec();
    let mut seed = vec![
        SeedFile::new("/dependencies.txt", deps.clone()),
        SeedFile::new("/big.bin", vec![7u8; 100 * 1024]),
        SeedFile::new("/dir/x", b"x".to_vec()),
    ];
    for i in 0..5 {
        seed.push(SeedFile::new(format!("/f{i}"), vec![i as u8; 60 * 1024]));
    }
    stormblock::fs::files::write_files(&dev, &seed).await.unwrap();
    drop(dev);
    let mut p = Publisher::new();

    let spec = json!({
        "volume": "pvc-shop-checkout-build", "name": "fn-shop-checkout",
        "provenance": { "component": "stormfn", "commit": "0123abc" },
        "files": ["/dependencies.txt", "/big.bin", "/dir", "/missing", "/f0", "/f1", "/f2", "/f3", "/f4"],
    });
    put(&store, "goldens", "fn-shop-checkout-r1", golden_object("fn-shop-checkout-r1", Some("forge1"), spec.clone()));
    let s = pass(&state, &mut p).await.unwrap();
    assert_eq!(s.applied, 1, "{s:?}");
    let o = settled(&store, "fn-shop-checkout-r1").await;
    let st = &o["status"];
    assert_eq!(st["phase"], "Ready", "{o}");
    assert_eq!(st["observedSpec"], spec);
    let gname = st["golden"]["name"].as_str().unwrap();
    assert!(gname.starts_with("golden-fn-shop-checkout-"), "{o}");
    assert_eq!(st["golden"]["sha256"].as_str().unwrap().len(), 64);
    assert_eq!(st["golden"]["existing"], false);
    let gid = Uuid::parse_str(st["golden"]["volumeId"].as_str().unwrap()).unwrap();
    assert_ne!(gid, id.0, "a golden of its own, not the source");

    let f = st["files"].as_array().unwrap();
    assert_eq!(f.len(), 9, "one entry a path, in order: {o}");
    assert_eq!(f[0]["path"], "/dependencies.txt");
    let got = base64::engine::general_purpose::STANDARD.decode(f[0]["contentsBase64"].as_str().unwrap()).unwrap();
    assert_eq!(got, deps);
    assert_eq!(f[0]["sha256"], json!(hex::encode(sha2::Sha256::digest(&deps))));
    assert_eq!(f[0]["size"], deps.len());
    assert!(f[1]["error"].as_str().unwrap().contains("65536 a file"), "over a file's bound, not read: {}", f[1]);
    assert!(f[1].get("contentsBase64").is_none());
    assert!(f[2]["error"].as_str().unwrap().contains("directory"), "{}", f[2]);
    assert!(f[3]["error"].is_string(), "{}", f[3]);
    for e in &f[4..8] {
        assert!(e["contentsBase64"].is_string(), "four 60 KiB files fit the 256 KiB: {e}");
    }
    assert!(f[8]["error"].as_str().unwrap().contains("together"), "the fifth is over the whole bound: {}", f[8]);

    // The golden on this node: sealed, recorded with who asked.
    {
        let vm = state.volume_manager.lock().await;
        let h = vm.get_volume_handle(&stormblock::volume::VolumeId(gid)).expect("the golden is here");
        assert_eq!(h.name().await, gname);
        assert!(vm.is_sealed(&stormblock::volume::VolumeId(gid)), "sealed");
    }
    let rec = stormblock::mgmt::goldens::load(&state).into_iter().find(|r| r.name == gname).expect("recorded");
    assert_eq!(rec.made_by.as_deref(), Some("kubernetes:Golden/fn-shop-checkout-r1"));
    assert_eq!(rec.provenance.get("commit").map(String::as_str), Some("0123abc"));

    // Not made twice for the same spec.
    let s = pass(&state, &mut p).await.unwrap();
    assert_eq!((s.applied, s.refused), (0, 0), "{s:?}");

    // A spec changed there: acted on again; the same content is the same golden.
    store.lock().unwrap().get_mut(&("goldens".to_string(), "fn-shop-checkout-r1".to_string())).unwrap()["spec"]["files"] = json!(["/dependencies.txt"]);
    let s = pass(&state, &mut p).await.unwrap();
    assert_eq!(s.applied, 1, "{s:?}");
    let o = settled(&store, "fn-shop-checkout-r1").await;
    assert_eq!(o["status"]["phase"], "Ready", "{o}");
    assert_eq!(o["status"]["golden"]["name"], gname);
    assert_eq!(o["status"]["golden"]["existing"], true, "{o}");
    assert_eq!(o["status"]["files"].as_array().unwrap().len(), 1);

    // A path that climbs out: refused before anything is made, once.
    put(&store, "goldens", "bad", golden_object("bad", Some("forge1"), json!({ "volume": "pvc-shop-checkout-build", "name": "x", "files": ["/a/../../etc/shadow"] })));
    // A volume this node does not have: failed with its 404.
    put(&store, "goldens", "gone", golden_object("gone", Some("forge1"), json!({ "volume": "pvc-nowhere", "name": "x" })));
    // No node label, and another node's: nobody here touches them.
    put(&store, "goldens", "nolabel", golden_object("nolabel", None, json!({ "volume": "pvc-shop-checkout-build", "name": "x" })));
    put(&store, "goldens", "server8s", golden_object("server8s", Some("server8"), json!({ "volume": "pvc-shop-checkout-build", "name": "x" })));
    let s = pass(&state, &mut p).await.unwrap();
    assert_eq!((s.applied, s.refused), (1, 1), "{s:?}");
    let b = obj(&store, "goldens", "bad").unwrap();
    assert_eq!(b["status"]["phase"], "Failed", "{b}");
    assert_eq!(b["status"]["error"]["code"], 422);
    assert!(b["status"]["error"]["message"].as_str().unwrap().contains(". or .."), "{b}");
    let g = settled(&store, "gone").await;
    assert_eq!(g["status"]["phase"], "Failed", "{g}");
    assert_eq!(g["status"]["error"]["code"], 404, "{g}");
    assert!(obj(&store, "goldens", "nolabel").unwrap().get("status").is_none());
    assert!(obj(&store, "goldens", "server8s").unwrap().get("status").is_none());
    let s = pass(&state, &mut p).await.unwrap();
    assert_eq!((s.applied, s.refused), (0, 0), "a failure as it stands is not asked again: {s:?}");
    let goldens = stormblock::mgmt::goldens::load(&state);
    assert_eq!(goldens.len(), 1, "nothing else was made: {goldens:?}");
}
