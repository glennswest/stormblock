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

/// A spec change refused: its code, reason and words. The :9090 PATCH
/// answers it as a Status; the apiserver's object carries it in
/// `status.specError` (#655).
#[derive(Debug, Clone)]
pub struct Refusal {
    pub code: StatusCode,
    pub reason: &'static str,
    pub message: String,
}

impl Refusal {
    fn new(code: StatusCode, reason: &'static str, message: impl Into<String>) -> Self {
        Refusal { code, reason, message: message.into() }
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        status_error(self.code, self.reason, self.message)
    }
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
            res("replications", "Replication", &["get", "list", "create", "update", "patch", "delete"]),
        ],
    }))
}

// --------------------------------------------------------------- volumes

/// What a Volume object needs from the volume manager, taken under a brief
/// hold (#571): sync getters only, nothing awaited. The object is built after
/// the manager is let go, so nothing else is ever taken under it: the `/v1`
/// handlers take `v1` then the manager, `volumes_in_use` took `ublk_exports`
/// then the manager, and a list that held the manager while it asked for
/// either deadlocked with them (pvetest1: req 1 for an hour, no apiserver).
struct VolumeSnap {
    id: VolumeId,
    handle: Arc<crate::volume::thin::ThinVolumeHandle>,
    fs: Option<Value>,
    namespace: String,
    parent: Option<VolumeId>,
    children: Vec<VolumeId>,
    retention: &'static str,
    /// What it belongs to (#115) and its kind, attachments and consumer
    /// (#138), as the REST listing has them (#649).
    owner: Option<crate::volume::metadata::Owner>,
    usage: super::usage::Usage,
    /// Its grow policy (#690).
    grow: Option<crate::volume::metadata::GrowPolicy>,
}

fn volume_snap(vm: &crate::volume::VolumeManager, id: VolumeId, ctx: &super::usage::Context) -> Option<VolumeSnap> {
    Some(VolumeSnap {
        owner: vm.owner(&id).cloned(),
        grow: vm.grow_policy(&id),
        usage: ctx.usage(vm, &id),
        id,
        handle: vm.get_volume_handle(&id)?,
        fs: vm.fs_info(&id).map(|f| f.json()),
        namespace: vm.namespace(&id).to_string(),
        parent: vm.parent(&id),
        children: vm.children(&id),
        retention: vm.retention(&id).as_str(),
    })
}

/// What the projection reads besides the manager, each lock taken alone and
/// let go before the next (#571).
struct AttachView {
    local_node: String,
}

async fn attach_view(state: &AppState) -> AttachView {
    AttachView { local_node: state.v1.lock().await.local_node.clone() }
}

/// Snapshots of every volume (or the ones asked for), the manager held only
/// to take them.
async fn volume_snaps(state: &AppState, only: Option<VolumeId>, ctx: &super::usage::Context) -> Vec<VolumeSnap> {
    let vm = state.volume_manager.lock().await;
    let mut ids = match only {
        Some(id) => vec![id],
        None => vm.volume_ids(),
    };
    ids.sort_by_key(|i| i.0);
    ids.into_iter().filter_map(|id| volume_snap(&vm, id, ctx)).collect()
}

/// An attachment as the object carries it (#649): camelCase, what the REST
/// listing's `attachments[]` says.
fn attachment_json(a: &super::usage::Attachment) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("transport".into(), json!(a.transport));
    for (k, v) in [("device", a.device.as_ref()), ("mountedAt", a.mounted_at.as_ref()), ("target", a.target.as_ref()), ("state", a.state.as_ref())] {
        if let Some(v) = v {
            m.insert(k.into(), json!(v));
        }
    }
    if let Some(p) = a.port {
        m.insert("port".into(), json!(p));
    }
    if let Some(l) = a.lun {
        m.insert("lun".into(), json!(l));
    }
    if let Some(n) = a.nsid {
        m.insert("nsid".into(), json!(n));
    }
    if !a.remote.is_empty() {
        m.insert("remote".into(), serde_json::to_value(&a.remote).unwrap_or(Value::Null));
    }
    Value::Object(m)
}

/// A label value as Kubernetes takes one: at most 63 of `[A-Za-z0-9._-]`,
/// starting and ending alphanumeric.
fn label_value(s: &str) -> String {
    let v: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).take(63).collect();
    v.trim_matches(|c: char| !c.is_ascii_alphanumeric()).to_string()
}

async fn volume_object(s: &VolumeSnap, view: &AttachView) -> Value {
    let (id, handle) = (s.id, &s.handle);
    let name = handle.name().await;
    let health = handle.health().await;
    let key = id.0.to_string();
    // Every way the volume is served now (#658): the engine's ublk devices
    // (an adopted boot device and a `/v1` attach's too), mounted where, in
    // any mount namespace; its NVMe/TCP and iSCSI exports with who is
    // connected. A list; empty when nothing serves it.
    let attach: Vec<Value> = s
        .usage
        .attachments
        .iter()
        .map(|a| {
            let mut v = attachment_json(a);
            if a.transport == "ublk" {
                if let (Some(o), Some(d)) = (v.as_object_mut(), a.device.as_ref()) {
                    o.insert("deviceHint".into(), json!(d));
                }
            }
            v
        })
        .collect();
    let mut labels = BTreeMap::new();
    labels.insert("storm.io/name".to_string(), name.clone());
    labels.insert("storm.io/node".to_string(), view.local_node.clone());
    labels.insert("storm.io/slab-role".to_string(), handle.placement_role().to_string());
    // The volume's namespace (#408). A label, not `metadata.namespace`: the
    // resource stays cluster-scoped, as discovery says.
    labels.insert("storm.io/namespace".to_string(), s.namespace.clone());
    if let Some(p) = s.parent {
        labels.insert("storm.io/parent".to_string(), p.0.to_string());
    }
    // What it is and what owns it (#649): `kubectl get volumes -l
    // storm.io/owner-kind=Pod`, and `-l '!storm.io/owner-kind'` for the
    // orphans.
    if let Some(k) = s.usage.kind {
        labels.insert("storm.io/kind".to_string(), k.to_string());
    }
    if let Some(o) = &s.owner {
        let v = label_value(&o.kind);
        if !v.is_empty() {
            labels.insert("storm.io/owner-kind".to_string(), v);
        }
    }
    object(
        "Volume",
        &key,
        &key,
        labels,
        json!({
            "name": name,
            "sizeBytes": handle.capacity_bytes(),
            "redundancy": handle.redundancy().spelling(),
            "sealed": handle.is_sealed(),
            // The setting; `status.writable` is whether a write would land,
            // which sealing also decides.
            "access": handle.access().to_string(),
            // Which half of the node's mutable storage this is in. A clone is
            // in its source's half whatever it is named, so the name is not
            // evidence and this is (#88).
            "role": handle.placement_role().to_string(),
            "retention": s.retention,
            "fs": s.fs,
            "namespace": s.namespace,
            // How the engine grows it as its filesystem fills (#690); null:
            // never, the conditions alone say it is filling.
            "grow": s.grow.map(|g| json!({ "max": g.max, "step": g.step })),
        }),
        json!({
            "health": health.state.to_string(),
            "writable": handle.writable(),
            "legsExpected": health.legs_expected,
            "legsMissing": health.legs_missing,
            "unreadable": health.unreadable,
            "failedSlabs": health.failed_slabs.iter().map(|s| s.0.to_string()).collect::<Vec<_>>(),
            "allocatedBytes": handle.allocated().await,
            // What the volume is now (#655): `spec.sizeBytes` is what was
            // asked; this follows the resize.
            "capacityBytes": handle.capacity_bytes(),
            "physicalBytes": handle.physical().await,
            "parent": s.parent.map(|p| p.0.to_string()),
            "children": s.children.iter().map(|c| c.0.to_string()).collect::<Vec<_>>(),
            "attach": attach,
            // #649: the REST listing's #115/#138 fields. `owner` is what a
            // creator recorded (null: nothing claimed it, an orphan for a
            // clone); `consumer` the owner, else the mount a ublk device
            // carries.
            "owner": s.owner,
            "consumer": s.usage.consumer,
            "inUse": s.usage.in_use,
            "attachments": s.usage.attachments.iter().map(attachment_json).collect::<Vec<_>>(),
            "kind": s.usage.kind,
        }),
    )
}

/// The Volume objects, the manager held only for their snapshots (#571).
pub(crate) async fn volume_objects(state: &AppState, only: Option<VolumeId>) -> Vec<Value> {
    // What serves each volume, gathered before the manager is taken (#571).
    let ctx = super::usage::Context::gather(state).await;
    let snaps = volume_snaps(state, only, &ctx).await;
    let view = attach_view(state).await;
    let mut out = Vec::with_capacity(snaps.len());
    for s in &snaps {
        let mut v = volume_object(s, &view).await;
        // Its filesystem's fill and conditions, its grows (#690).
        crate::mgmt::fill::annotate(state, s.id, &mut v);
        out.push(v);
    }
    out
}

async fn all_volumes(state: &AppState) -> Vec<Value> {
    volume_objects(state, None).await
}

/// By uuid, by `spec.name`, or by synonym — the manager first, so a synonym
/// never shadows a volume that answers to the same name.
async fn resolve_volume(state: &AppState, key: &str) -> Option<VolumeId> {
    if let Some(id) = state.volume_manager.lock().await.find_volume(key).await {
        return Some(id);
    }
    super::synonyms::volume_for(state, key).await
}

async fn get_volume(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    let Some(id) = resolve_volume(&state, &name).await else {
        return status_error(StatusCode::NOT_FOUND, "NotFound", format!("volumes \"{name}\" not found"));
    };
    match volume_objects(&state, Some(id)).await.pop() {
        Some(v) => Json(v).into_response(),
        None => status_error(StatusCode::NOT_FOUND, "NotFound", format!("volumes \"{name}\" not found")),
    }
}

#[derive(Deserialize, Default)]
struct VolumePatch {
    #[serde(default)]
    spec: Option<VolumeSpecPatch>,
}

#[derive(Deserialize, Default, Debug)]
pub(crate) struct VolumeSpecPatch {
    #[serde(default)]
    redundancy: Option<String>,
    #[serde(default)]
    sealed: Option<bool>,
    /// `rw` or `ro`, at any point in the volume's life.
    #[serde(default)]
    access: Option<String>,
    /// A verb, not a state: `true` runs a resync now.
    #[serde(default)]
    resync: Option<bool>,
    #[serde(default)]
    retention: Option<String>,
    /// Grow the volume to this many bytes (#655): thin and online, never
    /// smaller, never a sealed one.
    #[serde(default, rename = "sizeBytes")]
    size_bytes: Option<u64>,
    /// The grow policy (#690): `{max, step}` (bytes, or `64G`), `null` to
    /// clear it. Absent leaves it as it is.
    #[serde(default, deserialize_with = "present")]
    grow: Option<Value>,
}

/// A field that is there, `null` included (#690: `grow: null` clears).
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// `spec.grow` as asked: `None` clears the policy.
pub(crate) fn grow_policy_of(v: &Value) -> Result<Option<crate::volume::metadata::GrowPolicy>, String> {
    match v {
        Value::Null => Ok(None),
        Value::Object(m) => {
            if let Some(k) = m.keys().find(|k| !matches!(k.as_str(), "max" | "step")) {
                return Err(format!("spec.grow.{k}: not a field (max, step)"));
            }
            let max = m.get("max").ok_or("spec.grow.max: required (the most the volume grows to)")?;
            crate::volume::metadata::GrowPolicy::parse(max, m.get("step")).map(Some)
        }
        other => Err(format!("spec.grow: {other} is not {{max, step}} or null")),
    }
}

/// Grow a volume through its object (#655): the engine's resize, then the
/// kernel device told (a ublk export), then an ext4 the engine sees mounted
/// grown online. What happened, in words, for `status.lastResize`.
pub(crate) async fn grow_volume(state: &AppState, id: VolumeId, to: u64) -> Result<Value, Refusal> {
    let uuid = id.0;
    let (sealed, from) = {
        let vm = state.volume_manager.lock().await;
        match vm.get_volume_handle(&id) {
            Some(h) => (h.is_sealed(), h.capacity_bytes()),
            None => return Err(Refusal::new(StatusCode::NOT_FOUND, "NotFound", format!("volumes \"{uuid}\" not found"))),
        }
    };
    if to == from {
        return Ok(json!({ "from": from, "to": to, "volume": "already that size" }));
    }
    if to < from {
        return Err(Refusal::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid",
            format!("spec.sizeBytes: {to} is smaller than the volume ({from}): a volume grows, never shrinks (a filesystem on it cannot follow)"),
        ));
    }
    if sealed {
        return Err(Refusal::new(StatusCode::CONFLICT, "Conflict", format!("volume {uuid} is sealed: it takes no writes and is not grown (grow a clone of it)")));
    }
    if state.exports.read().await.iter().any(|e| e.volume_id == uuid) {
        return Err(Refusal::new(StatusCode::CONFLICT, "Conflict", format!("volume {uuid} has iSCSI/NVMe exports: their hosts read its size at connect; detach them first")));
    }
    if crate::mgmt::live_mirror::is_mirrored(state, uuid).await {
        return Err(Refusal::new(StatusCode::CONFLICT, "Conflict", format!("volume {uuid} is being mirrored to a peer; stop the mirror first")));
    }
    if let Err(e) = state.volume_manager.lock().await.resize_volume(id, to).await {
        return Err(Refusal::new(StatusCode::CONFLICT, "Conflict", e.to_string()));
    }
    let key = uuid.to_string();
    let (device, dev_path) = {
        let u = state.ublk_exports.lock().await;
        let p = u.device_path(&key);
        (u.update_size(&key, to), p)
    };
    let device = match device {
        Ok(true) => format!("{} told {to} bytes", dev_path.as_deref().unwrap_or("its ublk device")),
        Ok(false) => "no ublk device here".to_string(),
        Err(e) => {
            tracing::error!("volume {uuid} grew to {to} but its ublk device did not follow: {e} (#655)");
            format!("its ublk device did not follow: {e}")
        }
    };
    let fs = match dev_path.as_deref().and_then(crate::mgmt::ublk_export::mounted_at) {
        Some(mp) => {
            let r = tokio::task::spawn_blocking(move || crate::fs::grow::grow_mounted(&mp, to)).await.unwrap_or_else(|e| Err(e.to_string()));
            match r {
                Ok(s) => s,
                Err(e) => e,
            }
        }
        None => "not mounted where the engine sees it: grown by the service that mounts it".to_string(),
    };
    tracing::warn!("volume {uuid} grown from {from} to {to} bytes through its object: {device}; filesystem: {fs} (#655)");
    Ok(json!({ "from": from, "to": to, "device": device, "filesystem": fs }))
}

/// Apply a spec change to a volume (#655: the :9090 PATCH and a spec
/// edited on the apiserver both come here). Only the fields that are verbs
/// the engine already has. `Some(lastResize)` when it grew.
pub(crate) async fn apply_volume_spec(state: &AppState, id: VolumeId, spec: VolumeSpecPatch) -> Result<Option<Value>, Refusal> {
    // `spec.grow` checked before anything is changed (#690).
    let grow = match &spec.grow {
        Some(g) => Some(grow_policy_of(g).map_err(|e| Refusal::new(StatusCode::UNPROCESSABLE_ENTITY, "Invalid", e))?),
        None => None,
    };
    // First, and with no manager held across it (#571): it takes the
    // exports, the ublk table and the mount table.
    let resized = match spec.size_bytes {
        Some(to) => Some(grow_volume(state, id, to).await?),
        None => None,
    };
    let conflict = |e: String| Refusal::new(StatusCode::CONFLICT, "Conflict", e);
    let mut vm = state.volume_manager.lock().await;
    if let Some(g) = grow {
        vm.set_grow_policy(id, g).map_err(|e| conflict(e.to_string()))?;
        vm.persist().await;
        match g {
            Some(p) => tracing::info!("volume {id}: grow policy set: up to {} bytes, {} a step (#690)", p.max, p.step),
            None => tracing::info!("volume {id}: grow policy cleared (#690)"),
        }
    }
    if let Some(r) = spec.redundancy {
        let policy = crate::volume::RedundancyPolicy::parse(&r)
            .map_err(|e| Refusal::new(StatusCode::UNPROCESSABLE_ENTITY, "Invalid", format!("spec.redundancy: {e}")))?;
        vm.set_redundancy(id, policy).await.map_err(|e| conflict(e.to_string()))?;
    }
    if let Some(sealed) = spec.sealed {
        let r = if sealed { vm.seal_volume(id, None).await } else { vm.unseal_volume(id).await };
        r.map_err(|e| conflict(e.to_string()))?;
    }
    if let Some(a) = spec.access {
        let access = crate::volume::Access::parse(&a)
            .ok_or_else(|| Refusal::new(StatusCode::UNPROCESSABLE_ENTITY, "Invalid", format!("spec.access: {a:?} (use rw or ro)")))?;
        vm.set_access(id, access).await.map_err(|e| conflict(e.to_string()))?;
    }
    if let Some(ret) = spec.retention {
        let r = crate::volume::Retention::parse(&ret)
            .ok_or_else(|| Refusal::new(StatusCode::UNPROCESSABLE_ENTITY, "Invalid", format!("spec.retention: {ret:?}")))?;
        vm.set_retention(id, r).await;
    }
    // A volume the rebuild queue holds is resynced by it.
    if spec.resync == Some(true) && !state.rebuilds.holds(&id) {
        vm.resync_volume(id, false)
            .await
            .map_err(|e| Refusal::new(StatusCode::INTERNAL_SERVER_ERROR, "InternalError", e.to_string()))?;
    }
    Ok(resized)
}

/// `PATCH /apis/storage.storm.io/v1/volumes/{name}` — merge-patch on `spec`.
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
    let resized = match apply_volume_spec(&state, id, spec).await {
        Ok(r) => r,
        Err(r) => return r.into_response(),
    };
    // The object after the manager is let go (#571).
    match volume_objects(&state, Some(id)).await.pop() {
        Some(mut v) => {
            if let Some(r) = resized {
                v["status"]["lastResize"] = r;
            }
            Json(v).into_response()
        }
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
    // The manager alone, before the registry (#571): a hold of the registry
    // that waits on the manager inverts the order a persist takes them in.
    let meta: Vec<_> = state.volume_manager.lock().await.metadata_slabs().to_vec();
    let reg = state.slab_registry.read().await;
    let mut items: Vec<Value> = reg
        .iter()
        .map(|(id, slab)| {
            let key = id.0.to_string();
            let dev = slab.device().id();
            let mut labels = BTreeMap::new();
            labels.insert("storm.io/tier".to_string(), slab.tier().to_string());
            labels.insert("storm.io/drive".to_string(), dev.path.clone());
            labels.insert("storm.io/slab-role".to_string(), slab.role().to_string());
            object(
                "Slab",
                &key,
                &key,
                labels,
                json!({
                    "tier": slab.tier().to_string(),
                    "role": slab.role().to_string(),
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
                    "holdsMetadata": meta.contains(id),
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
    // The system disk too (#133), read-only: `status.system`.
    let drives = state.listed_drives().await;
    let reg = state.slab_registry.read().await;
    let mut items = Vec::with_capacity(drives.len());
    for (d, system) in drives.iter() {
        let id = d.device.id();
        let key = id.uuid.to_string();
        let slabs: Vec<String> = reg
            .iter()
            .filter(|(_, s)| super::drives::slab_is_on(s, &d.device, &d.path))
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
                "system": system,
            }),
        ));
    }
    items.sort_by(|a, b| a["spec"]["path"].as_str().cmp(&b["spec"]["path"].as_str()));
    items
}

async fn find_drive_key(state: &AppState, name: &str) -> Option<String> {
    state
        .listed_drives()
        .await
        .into_iter()
        .find(|(d, _)| d.path == name || d.device.id().uuid.to_string() == name)
        .map(|(d, _)| d.device.id().uuid.to_string())
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
    if let Some(path) = state.system_disk(&name).await {
        return status_error(
            StatusCode::CONFLICT,
            "Conflict",
            format!("{path} is this node's system disk; it is not relabelled or drained through the drive API"),
        );
    }
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
                    let holds_meta = {
                        let vm = state.volume_manager.lock().await;
                        slabs.iter().any(|s| vm.is_metadata_slab(s))
                    };
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
    /// `?unowned=true` (#649, as the REST listing has it): only Volumes no
    /// creator claimed (`status.owner` null).
    #[serde(default)]
    unowned: Option<String>,
}

/// The listing's filters: the label selector, and `?unowned` (#649).
#[derive(Clone, Default)]
struct Filter {
    selector: Option<String>,
    unowned: bool,
}

impl Filter {
    fn of(q: &ListQuery) -> Filter {
        Filter { selector: q.label_selector.clone(), unowned: watch_truthy(&q.unowned) }
    }
    fn keeps(&self, item: &Value) -> bool {
        selected(item, &self.selector) && (!self.unowned || item["kind"] != "Volume" || item["status"]["owner"].is_null())
    }
}

/// `key=value`, `key!=value`, `key` and `!key` (#649), comma-separated, all
/// of them.
fn selected(item: &Value, selector: &Option<String>) -> bool {
    let Some(sel) = selector else { return true };
    let labels = &item["metadata"]["labels"];
    let get = |k: &str| labels.get(k.trim()).and_then(|x| x.as_str());
    sel.split(',').filter(|s| !s.trim().is_empty()).all(|pair| {
        let pair = pair.trim();
        if let Some((k, v)) = pair.split_once("!=") {
            return get(k) != Some(v.trim());
        }
        if let Some((k, v)) = pair.split_once('=') {
            return get(k) == Some(v.trim_start_matches('=').trim());
        }
        match pair.strip_prefix('!') {
            Some(k) => labels.get(k.trim()).is_none(),
            None => labels.get(pair).is_some(),
        }
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
    let filter = Filter::of(&q);
    let items: Vec<Value> = collect(&state, kind).await.into_iter().filter(|i| filter.keeps(i)).collect();
    if !watch_truthy(&q.watch) {
        let rv = fingerprint(&Value::Array(items.clone()));
        return Json(list(kind, items, rv)).into_response();
    }
    let stream = async_stream(state, kind, items, filter);
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
    filter: Filter,
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
            let now: Vec<Value> = collect(&state, kind).await.into_iter().filter(|i| filter.keeps(i)).collect();
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

// ----------------------------------------------------------- replications

/// A typed body, so the object's fields keep their order (#656: the
/// listing's maps are sorted; a status is read `phase` first).
fn typed<T: serde::Serialize>(code: StatusCode, v: &T) -> Response {
    match serde_json::to_vec(v) {
        Ok(b) => (code, [(header::CONTENT_TYPE, "application/json")], b).into_response(),
        Err(e) => status_error(StatusCode::INTERNAL_SERVER_ERROR, "InternalError", e.to_string()),
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ReplicationList<'a> {
    api_version: String,
    kind: &'static str,
    items: Vec<crate::mgmt::replication::Object<'a>>,
}

async fn list_replications(State(state): State<Arc<AppState>>) -> Response {
    let all = crate::mgmt::replication::list(&state);
    typed(StatusCode::OK, &ReplicationList { api_version: api_version(), kind: "ReplicationList", items: all.iter().map(|r| r.object()).collect() })
}

async fn get_replication(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    match crate::mgmt::replication::get(&state, &name) {
        Some(r) => typed(StatusCode::OK, &r.object()),
        None => status_error(StatusCode::NOT_FOUND, "NotFound", format!("replications \"{name}\" not found")),
    }
}

fn spec_of(body: &Value) -> Result<crate::mgmt::replication::Spec, Response> {
    serde_json::from_value(body.get("spec").cloned().unwrap_or(Value::Null))
        .map_err(|e| status_error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid", format!("spec: {e}")))
}

fn put_replication(state: &Arc<AppState>, name: &str, spec: crate::mgmt::replication::Spec, code: StatusCode) -> Response {
    match crate::mgmt::replication::put(state, name, spec) {
        Ok(r) => typed(code, &r.object()),
        Err(e) => status_error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid", e),
    }
}

/// `POST …/replications` (#656): `{metadata: {name}, spec}`; the run starts.
async fn create_replication(State(state): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    let Some(name) = body["metadata"]["name"].as_str().map(str::to_string) else {
        return status_error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid", "metadata.name: required");
    };
    if crate::mgmt::replication::get(&state, &name).is_some() {
        return status_error(StatusCode::CONFLICT, "AlreadyExists", format!("replications \"{name}\" already exists"));
    }
    match spec_of(&body) {
        Ok(spec) => put_replication(&state, &name, spec, StatusCode::CREATED),
        Err(r) => r,
    }
}

/// `PUT …/replications/{name}`: its spec replaced (a new generation, the
/// run follows it), or made.
async fn replace_replication(State(state): State<Arc<AppState>>, Path(name): Path<String>, Json(body): Json<Value>) -> Response {
    match spec_of(&body) {
        Ok(spec) => put_replication(&state, &name, spec, StatusCode::OK),
        Err(r) => r,
    }
}

/// A JSON merge patch (RFC 7386) of `patch` into `into`.
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

/// `PATCH …/replications/{name}`: `{spec: {…}}` merged into its spec.
async fn patch_replication(State(state): State<Arc<AppState>>, Path(name): Path<String>, Json(body): Json<Value>) -> Response {
    let Some(r) = crate::mgmt::replication::get(&state, &name) else {
        return status_error(StatusCode::NOT_FOUND, "NotFound", format!("replications \"{name}\" not found"));
    };
    let mut spec = serde_json::to_value(&r.spec).unwrap_or(Value::Null);
    merge(&mut spec, body.get("spec").unwrap_or(&Value::Null));
    match serde_json::from_value(spec) {
        Ok(spec) => put_replication(&state, &name, spec, StatusCode::OK),
        Err(e) => status_error(StatusCode::UNPROCESSABLE_ENTITY, "Invalid", format!("spec: {e}")),
    }
}

/// `DELETE …/replications/{name}`: the run stops; nothing it copied goes.
async fn delete_replication(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    if crate::mgmt::replication::delete(&state, &name) {
        Json(json!({ "apiVersion": "v1", "kind": "Status", "status": "Success", "details": { "name": name, "kind": "replications" } })).into_response()
    } else {
        status_error(StatusCode::NOT_FOUND, "NotFound", format!("replications \"{name}\" not found"))
    }
}

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
        .route(&format!("{gv}/replications"), get(list_replications).post(create_replication))
        .route(
            &format!("{gv}/replications/{{name}}"),
            get(get_replication).put(replace_replication).patch(patch_replication).delete(delete_replication),
        )
        .with_state(state)
}
