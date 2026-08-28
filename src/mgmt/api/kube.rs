//! Kubernetes-shaped resources, served by the engine itself (#80).
//!
//! `GET /apis/storage.storm.io/v1/volumes` and friends answer in the shape
//! `kubectl`, an aggregated API server and stormview all read —
//! `apiVersion/kind/metadata/spec/status`, `…List` envelopes, API discovery
//! at `/apis` and `/apis/storage.storm.io/v1`, and `?watch=1` as a
//! newline-delimited event stream. Each component serves its own: this is
//! the engine's (`Volume`, `Slab`, `Drive`, `Node`); stormdrive serves the
//! physical `Drive` and `Enclosure`. Nothing here is a second store — every
//! object is a projection of the state the REST API already serves, and
//! the few `spec` fields that can be written map one-to-one onto verbs that
//! already exist.
//!
//! `metadata.name` is the uuid: engine names are not unique (a snapshot can
//! carry any name), and a Kubernetes name has to be. The human name is
//! `spec.name` and the label `storm.io/name`; `get` accepts either.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::drive::BlockDevice;
use crate::mgmt::AppState;
use crate::volume::VolumeId;

pub const GROUP: &str = "storage.storm.io";
pub const VERSION: &str = "v1";

fn api_version() -> String {
    format!("{GROUP}/{VERSION}")
}

/// Kubernetes-style Status error body.
fn status_error(code: StatusCode, reason: &str, message: impl Into<String>) -> Response {
    let message = message.into();
    (
        code,
        Json(json!({
            "apiVersion": "v1", "kind": "Status", "status": "Failure",
            "message": message, "reason": reason, "code": code.as_u16(),
        })),
    )
        .into_response()
}

fn list(kind: &str, items: Vec<Value>, rv: u64) -> Value {
    json!({
        "apiVersion": api_version(),
        "kind": format!("{kind}List"),
        "metadata": { "resourceVersion": rv.to_string() },
        "items": items,
    })
}

fn object(kind: &str, name: &str, uid: &str, labels: BTreeMap<String, String>, spec: Value, status: Value) -> Value {
    json!({
        "apiVersion": api_version(),
        "kind": kind,
        "metadata": { "name": name, "uid": uid, "labels": labels },
        "spec": spec,
        "status": status,
    })
}

/// A resource version that moves whenever anything below moves: a hash of
/// the rendered objects. Coarse, but honest — a watcher is told about a
/// change exactly when the object it would fetch differs.
fn fingerprint(v: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    v.to_string().hash(&mut h);
    h.finish()
}

// ------------------------------------------------------------- discovery

async fn apis() -> Json<Value> {
    Json(json!({
        "kind": "APIGroupList", "apiVersion": "v1",
        "groups": [group_json()],
    }))
}

fn group_json() -> Value {
    json!({
        "name": GROUP,
        "versions": [{ "groupVersion": api_version(), "version": VERSION }],
        "preferredVersion": { "groupVersion": api_version(), "version": VERSION },
    })
}

async fn api_group() -> Json<Value> {
    let mut g = group_json();
    g["kind"] = json!("APIGroup");
    g["apiVersion"] = json!("v1");
    Json(g)
}

async fn api_resources() -> Json<Value> {
    let res = |name: &str, kind: &str, verbs: &[&str]| {
        json!({ "name": name, "singularName": kind.to_lowercase(), "namespaced": false, "kind": kind, "verbs": verbs })
    };
    Json(json!({
        "kind": "APIResourceList", "apiVersion": "v1", "groupVersion": api_version(),
        "resources": [
            res("volumes", "Volume", &["get", "list", "watch", "patch", "delete"]),
            res("slabs", "Slab", &["get", "list", "watch"]),
            res("drives", "Drive", &["get", "list", "watch", "patch"]),
            res("nodes", "Node", &["get", "list", "watch"]),
        ],
    }))
}

// --------------------------------------------------------------- volumes

async fn volume_object(state: &AppState, vm: &crate::volume::VolumeManager, id: VolumeId) -> Option<Value> {
    let handle = vm.get_volume_handle(&id)?;
    let name = handle.name().await;
    let health = handle.health().await;
    let fs = vm.fs_info(&id);
    let key = id.0.to_string();
    let attach = if let Some(path) = state.ublk_exports.lock().await.device_path(&key) {
        json!({ "transport": "ublk", "deviceHint": path })
    } else if let Some(nsid) = state.v1.lock().await.nvme_nsids.get(&key).copied() {
        json!({ "transport": "nvme_tcp", "nsid": nsid })
    } else {
        Value::Null
    };
    let mut labels = BTreeMap::new();
    labels.insert("storm.io/name".to_string(), name.clone());
    labels.insert("storm.io/node".to_string(), state.v1.lock().await.local_node.clone());
    if let Some(p) = vm.parent(&id) {
        labels.insert("storm.io/parent".to_string(), p.0.to_string());
    }
    Some(object(
        "Volume",
        &key,
        &key,
        labels,
        json!({
            "name": name,
            "sizeBytes": handle.capacity_bytes(),
            "redundancy": handle.redundancy().spelling(),
            "sealed": handle.is_sealed(),
            "retention": vm.retention(&id).as_str(),
            "fs": fs.map(|f| f.json()),
        }),
        json!({
            "health": health.state.to_string(),
            "legsExpected": health.legs_expected,
            "legsMissing": health.legs_missing,
            "unreadable": health.unreadable,
            "failedSlabs": health.failed_slabs.iter().map(|s| s.0.to_string()).collect::<Vec<_>>(),
            "allocatedBytes": handle.allocated().await,
            "physicalBytes": handle.physical().await,
            "parent": vm.parent(&id).map(|p| p.0.to_string()),
            "children": vm.children(&id).iter().map(|c| c.0.to_string()).collect::<Vec<_>>(),
            "attach": attach,
        }),
    ))
}

async fn all_volumes(state: &AppState) -> Vec<Value> {
    let vm = state.volume_manager.lock().await;
    let mut ids: Vec<VolumeId> = vm.list_volumes().await.into_iter().map(|(id, ..)| id).collect();
    ids.sort_by_key(|i| i.0);
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(v) = volume_object(state, &vm, id).await {
            out.push(v);
        }
    }
    out
}

async fn resolve_volume(state: &AppState, key: &str) -> Option<VolumeId> {
    state.volume_manager.lock().await.find_volume(key).await
}

async fn get_volume(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    let Some(id) = resolve_volume(&state, &name).await else {
        return status_error(StatusCode::NOT_FOUND, "NotFound", format!("volumes \"{name}\" not found"));
    };
    let vm = state.volume_manager.lock().await;
    match volume_object(&state, &vm, id).await {
        Some(v) => Json(v).into_response(),
        None => status_error(StatusCode::NOT_FOUND, "NotFound", format!("volumes \"{name}\" not found")),
    }
}

#[derive(Deserialize, Default)]
struct VolumePatch {
    #[serde(default)]
    spec: Option<VolumeSpecPatch>,
}

#[derive(Deserialize, Default)]
struct VolumeSpecPatch {
    #[serde(default)]
    redundancy: Option<String>,
    #[serde(default)]
    sealed: Option<bool>,
    /// A verb, not a state: `true` runs a resync now.
    #[serde(default)]
    resync: Option<bool>,
    #[serde(default)]
    retention: Option<String>,
}

/// `PATCH /apis/storage.storm.io/v1/volumes/{name}` — merge-patch on `spec`.
/// Only the fields that are verbs the engine already has.
async fn patch_volume(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(patch): Json<VolumePatch>,
) -> Response {
    let Some(id) = resolve_volume(&state, &name).await else {
        return status_error(StatusCode::NOT_FOUND, "NotFound", format!("volumes \"{name}\" not found"));
    };
    let Some(spec) = patch.spec else {
        return get_volume(State(state), Path(name)).await;
    };
    let mut vm = state.volume_manager.lock().await;
    if let Some(r) = spec.redundancy {
        let policy = match crate::volume::RedundancyPolicy::parse(&r) {
            Ok(p) => p,
            Err(e) => return status_error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid", format!("spec.redundancy: {e}")),
        };
        if let Err(e) = vm.set_redundancy(id, policy).await {
            return status_error(StatusCode::CONFLICT, "Conflict", e.to_string());
        }
    }
    if let Some(sealed) = spec.sealed {
        let r = if sealed { vm.seal_volume(id, None).await } else { vm.unseal_volume(id).await };
        if let Err(e) = r {
            return status_error(StatusCode::CONFLICT, "Conflict", e.to_string());
        }
    }
    if let Some(ret) = spec.retention {
        match crate::volume::Retention::parse(&ret) {
            Some(r) => vm.set_retention(id, r).await,
            None => return status_error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid", format!("spec.retention: {ret:?}")),
        }
    }
    if spec.resync == Some(true) {
        if let Err(e) = vm.resync_volume(id, false).await {
            return status_error(StatusCode::INTERNAL_SERVER_ERROR, "InternalError", e.to_string());
        }
    }
    match volume_object(&state, &vm, id).await {
        Some(v) => Json(v).into_response(),
        None => status_error(StatusCode::NOT_FOUND, "NotFound", format!("volumes \"{name}\" not found")),
    }
}

async fn delete_volume(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    let Some(id) = resolve_volume(&state, &name).await else {
        return status_error(StatusCode::NOT_FOUND, "NotFound", format!("volumes \"{name}\" not found"));
    };
    let exported = state.exports.read().await.iter().any(|e| e.volume_id == id.0)
        || state.ublk_exports.lock().await.is_exported(&id.0.to_string());
    if exported {
        return status_error(StatusCode::CONFLICT, "Conflict", format!("volume {} is exported; detach it first", id));
    }
    let mut vm = state.volume_manager.lock().await;
    match vm.delete_volume(id).await {
        Ok(()) => Json(json!({ "apiVersion": "v1", "kind": "Status", "status": "Success", "details": { "name": id.0.to_string(), "kind": "volumes" } })).into_response(),
        Err(e) => status_error(StatusCode::INTERNAL_SERVER_ERROR, "InternalError", e.to_string()),
    }
}

// ----------------------------------------------------------------- slabs

async fn all_slabs(state: &AppState) -> Vec<Value> {
    let reg = state.slab_registry.read().await;
    let meta = state.volume_manager.lock().await.metadata_slab();
    let mut items: Vec<Value> = reg
        .iter()
        .map(|(id, slab)| {
            let key = id.0.to_string();
            let dev = slab.device().id();
            let mut labels = BTreeMap::new();
            labels.insert("storm.io/tier".to_string(), slab.tier().to_string());
            labels.insert("storm.io/drive".to_string(), dev.path.clone());
            object(
                "Slab",
                &key,
                &key,
                labels,
                json!({
                    "tier": slab.tier().to_string(),
                    "slotSize": slab.slot_size(),
                    "domain": reg.domain_of(id).to_string(),
                    "drive": { "path": dev.path, "serial": dev.serial, "uuid": dev.uuid },
                }),
                json!({
                    "totalSlots": slab.total_slots(),
                    "freeSlots": slab.free_slots(),
                    "allocatedSlots": slab.allocated_slots(),
                    "capacityBytes": slab.total_slots() * slab.slot_size(),
                    "freeBytes": slab.free_slots() * slab.slot_size(),
                    "quarantined": reg.is_quarantined(id),
                    "holdsMetadata": meta == Some(*id),
                }),
            )
        })
        .collect();
    items.sort_by(|a, b| a["metadata"]["name"].as_str().cmp(&b["metadata"]["name"].as_str()));
    items
}

async fn get_slab(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    match all_slabs(&state).await.into_iter().find(|s| s["metadata"]["name"] == name) {
        Some(s) => Json(s).into_response(),
        None => status_error(StatusCode::NOT_FOUND, "NotFound", format!("slabs \"{name}\" not found")),
    }
}

// ---------------------------------------------------------------- drives

async fn all_drives(state: &AppState) -> Vec<Value> {
    let drives = state.drives.read().await;
    let reg = state.slab_registry.read().await;
    let mut items = Vec::with_capacity(drives.len());
    for d in drives.iter() {
        let id = d.device.id();
        let key = id.uuid.to_string();
        let slabs: Vec<String> = reg
            .iter()
            .filter(|(_, s)| Arc::ptr_eq(s.device(), &d.device) || s.device().id().path == d.path)
            .map(|(sid, _)| sid.0.to_string())
            .collect();
        let drain = state.drains.read().await.status(&d.path).await.map(|s| {
            json!({ "state": s.state, "moved": s.moved, "failed": s.failed, "remaining": s.remaining, "errors": s.errors })
        });
        let smart = d.device.smart_status().ok().map(|s| {
            json!({ "healthy": s.healthy, "temperatureC": s.temperature_celsius, "mediaErrors": s.media_errors,
                    "availableSparePct": s.available_spare_pct, "powerOnHours": s.power_on_hours })
        });
        let mut labels = BTreeMap::new();
        labels.insert("storm.io/path".to_string(), d.path.clone());
        for l in &d.labels.chain {
            labels.insert(format!("storm.io/{}", l.rung), l.value.clone());
        }
        items.push(object(
            "Drive",
            &key,
            &key,
            labels,
            json!({
                "path": d.path,
                "labels": d.labels.chain.iter().map(|l| (l.rung.clone(), l.value.clone())).collect::<BTreeMap<_, _>>(),
                "drain": drain.as_ref().is_some_and(|x| x["state"] == "running"),
            }),
            json!({
                "model": id.model, "serial": id.serial,
                "deviceType": d.device.device_type().to_string(),
                "capacityBytes": d.device.capacity_bytes(),
                "blockSize": d.device.block_size(),
                "slabs": slabs,
                "smart": smart,
                "drain": drain,
            }),
        ));
    }
    items.sort_by(|a, b| a["spec"]["path"].as_str().cmp(&b["spec"]["path"].as_str()));
    items
}

async fn find_drive_key(state: &AppState, name: &str) -> Option<String> {
    let drives = state.drives.read().await;
    drives
        .iter()
        .find(|d| d.path == name || d.device.id().uuid.to_string() == name)
        .map(|d| d.device.id().uuid.to_string())
}

async fn get_drive(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    let Some(key) = find_drive_key(&state, &name).await else {
        return status_error(StatusCode::NOT_FOUND, "NotFound", format!("drives \"{name}\" not found"));
    };
    match all_drives(&state).await.into_iter().find(|d| d["metadata"]["name"] == key) {
        Some(d) => Json(d).into_response(),
        None => status_error(StatusCode::NOT_FOUND, "NotFound", format!("drives \"{name}\" not found")),
    }
}

#[derive(Deserialize, Default)]
struct DrivePatch {
    #[serde(default)]
    spec: Option<DriveSpecPatch>,
}

#[derive(Deserialize, Default)]
struct DriveSpecPatch {
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
    /// `true` starts a drain; `false` cancels one.
    #[serde(default)]
    drain: Option<bool>,
}

/// `PATCH /apis/storage.storm.io/v1/drives/{name}` — labels and drain, the
/// two things the engine lets a drive plane say about a drive.
async fn patch_drive(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(patch): Json<DrivePatch>,
) -> Response {
    let found = {
        let drives = state.drives.read().await;
        drives
            .iter()
            .find(|d| d.path == name || d.device.id().uuid.to_string() == name)
            .map(|d| (d.device.clone(), d.path.clone()))
    };
    let Some((dev, path)) = found else {
        return status_error(StatusCode::NOT_FOUND, "NotFound", format!("drives \"{name}\" not found"));
    };
    if let Some(spec) = patch.spec {
        if let Some(labels) = spec.labels {
            let chain = crate::placement::domain::FailureDomain::from_labels(labels);
            {
                let mut drives = state.drives.write().await;
                if let Some(d) = drives.iter_mut().find(|d| d.path == path) {
                    d.labels = chain.clone();
                }
            }
            state.slab_registry.write().await.label_device(&path, chain);
        }
        match spec.drain {
            Some(true) => {
                let running = state.drains.read().await.is_running(&path).await;
                if !running {
                    let slabs: Vec<crate::drive::slab::SlabId> = state
                        .slab_registry
                        .read()
                        .await
                        .iter()
                        .filter(|(_, s)| Arc::ptr_eq(s.device(), &dev) || s.device().id().path == path)
                        .map(|(id, _)| *id)
                        .collect();
                    let holds_meta = state.volume_manager.lock().await.metadata_slab().is_some_and(|m| slabs.contains(&m));
                    if holds_meta {
                        return status_error(StatusCode::CONFLICT, "Conflict", format!("{path} holds the volume metadata slab; move it first"));
                    }
                    if !slabs.is_empty() {
                        state.drains.write().await.start(
                            path.clone(),
                            slabs,
                            state.gem.clone(),
                            state.slab_registry.clone(),
                            state.volume_manager.clone(),
                        );
                    }
                }
            }
            Some(false) => {
                if let Some(d) = state.drains.read().await.get(&path) {
                    d.cancel();
                }
            }
            None => {}
        }
    }
    get_drive(State(state), Path(name)).await
}

// ----------------------------------------------------------------- nodes

async fn all_nodes(state: &AppState) -> Vec<Value> {
    let v1 = state.v1.lock().await;
    let local = v1.local_node.clone();
    let nodes = super::v1::nodes_view(state, &v1).await;
    drop(v1);
    let engine_version = env!("CARGO_PKG_VERSION");
    nodes
        .into_values()
        .map(|n| {
            let mut labels = BTreeMap::new();
            for (k, v) in &n.topology {
                labels.insert(format!("storm.io/{k}"), v.clone());
            }
            labels.insert("storm.io/local".to_string(), (n.node == local).to_string());
            object(
                "Node",
                &n.node,
                &n.node,
                labels,
                json!({ "topology": n.topology, "topologyChain": n.topology_chain }),
                json!({
                    "totalBytes": n.total_bytes,
                    "freeBytes": n.free_bytes,
                    "local": n.node == local,
                    "engineVersion": if n.node == local { Some(engine_version) } else { None },
                }),
            )
        })
        .collect()
}

async fn get_node(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    match all_nodes(&state).await.into_iter().find(|n| n["metadata"]["name"] == name) {
        Some(n) => Json(n).into_response(),
        None => status_error(StatusCode::NOT_FOUND, "NotFound", format!("nodes \"{name}\" not found")),
    }
}

// -------------------------------------------------------- list and watch

#[derive(Deserialize, Default)]
struct ListQuery {
    #[serde(default)]
    watch: Option<String>,
    /// `key=value[,key=value]` over `metadata.labels`.
    #[serde(default, rename = "labelSelector")]
    label_selector: Option<String>,
}

fn selected(item: &Value, selector: &Option<String>) -> bool {
    let Some(sel) = selector else { return true };
    let labels = &item["metadata"]["labels"];
    sel.split(',').filter(|s| !s.trim().is_empty()).all(|pair| match pair.split_once('=') {
        Some((k, v)) => labels.get(k.trim()).and_then(|x| x.as_str()) == Some(v.trim()),
        None => labels.get(pair.trim()).is_some(),
    })
}

async fn collect(state: &AppState, kind: &str) -> Vec<Value> {
    match kind {
        "Volume" => all_volumes(state).await,
        "Slab" => all_slabs(state).await,
        "Drive" => all_drives(state).await,
        "Node" => all_nodes(state).await,
        _ => Vec::new(),
    }
}

fn watch_truthy(w: &Option<String>) -> bool {
    matches!(w.as_deref(), Some("1") | Some("true"))
}

/// A list, or — with `?watch=1` — a stream of `{type, object}` lines:
/// `ADDED` for everything present, then `MODIFIED`/`DELETED`/`ADDED` as the
/// projection changes, sampled every two seconds. Coarse, honest, and
/// enough for a UI and for kubectl.
async fn list_or_watch(state: Arc<AppState>, kind: &'static str, q: ListQuery) -> Response {
    let items: Vec<Value> = collect(&state, kind).await.into_iter().filter(|i| selected(i, &q.label_selector)).collect();
    if !watch_truthy(&q.watch) {
        let rv = fingerprint(&Value::Array(items.clone()));
        return Json(list(kind, items, rv)).into_response();
    }
    let selector = q.label_selector.clone();
    let stream = async_stream(state, kind, items, selector);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::TRANSFER_ENCODING, "chunked")
        .body(Body::from_stream(stream))
        .unwrap()
}

fn async_stream(
    state: Arc<AppState>,
    kind: &'static str,
    initial: Vec<Value>,
    selector: Option<String>,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(64);
    tokio::spawn(async move {
        let mut seen: BTreeMap<String, u64> = BTreeMap::new();
        for it in &initial {
            let name = it["metadata"]["name"].as_str().unwrap_or_default().to_string();
            seen.insert(name, fingerprint(it));
            let line = format!("{}\n", json!({ "type": "ADDED", "object": it }));
            if tx.send(bytes::Bytes::from(line)).await.is_err() {
                return;
            }
        }
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if tx.is_closed() {
                return;
            }
            let now: Vec<Value> = collect(&state, kind).await.into_iter().filter(|i| selected(i, &selector)).collect();
            let mut present: BTreeMap<String, u64> = BTreeMap::new();
            for it in &now {
                let name = it["metadata"]["name"].as_str().unwrap_or_default().to_string();
                let fp = fingerprint(it);
                present.insert(name.clone(), fp);
                let ev = match seen.get(&name) {
                    None => Some("ADDED"),
                    Some(old) if *old != fp => Some("MODIFIED"),
                    _ => None,
                };
                if let Some(t) = ev {
                    let line = format!("{}\n", json!({ "type": t, "object": it }));
                    if tx.send(bytes::Bytes::from(line)).await.is_err() {
                        return;
                    }
                }
            }
            for name in seen.keys() {
                if !present.contains_key(name) {
                    let line = format!("{}\n", json!({ "type": "DELETED", "object": { "apiVersion": api_version(), "kind": kind, "metadata": { "name": name } } }));
                    if tx.send(bytes::Bytes::from(line)).await.is_err() {
                        return;
                    }
                }
            }
            seen = present;
        }
    });
    futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx)).map(Ok)
}

use futures_util::StreamExt as _;

async fn list_volumes(State(state): State<Arc<AppState>>, Query(q): Query<ListQuery>) -> Response {
    list_or_watch(state, "Volume", q).await
}
async fn list_slabs(State(state): State<Arc<AppState>>, Query(q): Query<ListQuery>) -> Response {
    list_or_watch(state, "Slab", q).await
}
async fn list_drives(State(state): State<Arc<AppState>>, Query(q): Query<ListQuery>) -> Response {
    list_or_watch(state, "Drive", q).await
}
async fn list_nodes(State(state): State<Arc<AppState>>, Query(q): Query<ListQuery>) -> Response {
    list_or_watch(state, "Node", q).await
}

pub fn router(state: Arc<AppState>) -> Router {
    let gv = format!("/apis/{GROUP}/{VERSION}");
    Router::new()
        .route("/apis", get(apis))
        .route(&format!("/apis/{GROUP}"), get(api_group))
        .route(&gv, get(api_resources))
        .route(&format!("{gv}/volumes"), get(list_volumes))
        .route(&format!("{gv}/volumes/{{name}}"), get(get_volume).patch(patch_volume).delete(delete_volume))
        .route(&format!("{gv}/slabs"), get(list_slabs))
        .route(&format!("{gv}/slabs/{{name}}"), get(get_slab))
        .route(&format!("{gv}/drives"), get(list_drives))
        .route(&format!("{gv}/drives/{{name}}"), get(get_drive).patch(patch_drive))
        .route(&format!("{gv}/nodes"), get(list_nodes))
        .route(&format!("{gv}/nodes/{{name}}"), get(get_node))
        .with_state(state)
}
