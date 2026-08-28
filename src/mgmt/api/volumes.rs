//! GET/POST/DELETE /api/v1/volumes — volume management + snapshots.

use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, State},
    routing::get,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Serialize, Deserialize};
use uuid::Uuid;

use super::{ApiError, ListResponse};
use crate::drive::BlockDevice;
use crate::mgmt::AppState;
use crate::mgmt::config::{human_size, parse_size};
use crate::raid::RaidArrayId;
use crate::volume::VolumeId;

#[derive(Debug, Serialize)]
pub struct VolumeResponse {
    pub id: Uuid,
    pub name: String,
    pub virtual_size_bytes: u64,
    pub virtual_size_human: String,
    pub allocated_bytes: u64,
    pub allocated_human: String,
    pub array_id: Option<Uuid>,
    /// Filesystem UUID, for a volume cloned from a preformatted template. Each
    /// clone gets its own, stamped at clone time (#38).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs_uuid: Option<Uuid>,
    /// How the volume is protected: `none`, `mirror:2`, `raid5:4+1`, …
    pub redundancy: String,
    /// `healthy`, `degraded` or `failed` — what the policy asks for versus
    /// what is on trusted slabs.
    pub health: String,
    /// Physical bytes the volume occupies, every leg and parity slot counted.
    pub physical_bytes: u64,
    /// The volume this one was cloned from (#76).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<Uuid>,
    /// Sealed: takes no writes; what clones are taken from.
    pub sealed: bool,
    /// The filesystem on it, when the engine knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs: Option<serde_json::Value>,
}

/// Everything about a volume the response carries beyond name and size.
struct Described {
    redundancy: String,
    health: String,
    physical_bytes: u64,
    parent: Option<Uuid>,
    sealed: bool,
    fs: Option<serde_json::Value>,
    fs_uuid: Option<Uuid>,
}

async fn describe(vm: &crate::volume::VolumeManager, id: &VolumeId) -> Described {
    match vm.get_volume_handle(id) {
        Some(handle) => {
            let h = handle.health().await;
            let fs = vm.fs_info(id);
            Described {
                redundancy: h.redundancy,
                health: h.state.to_string(),
                physical_bytes: handle.physical().await,
                parent: vm.parent(id).map(|p| p.0),
                sealed: handle.is_sealed(),
                fs: fs.map(|f| f.json()),
                fs_uuid: fs.and_then(|f| f.uuid),
            }
        }
        None => Described {
            redundancy: "none".into(),
            health: "healthy".into(),
            physical_bytes: 0,
            parent: None,
            sealed: false,
            fs: None,
            fs_uuid: None,
        },
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateVolumeRequest {
    pub name: String,
    /// Required for a plain volume; optional (grow-only) when cloning a
    /// template, which already knows its size.
    #[serde(default)]
    pub size: Option<String>,
    /// Required for a plain volume. A template clone is placed by the slab
    /// registry, like the source it descends from.
    #[serde(default)]
    pub array_id: Option<Uuid>,
    /// Clone this preformatted filesystem template instead of creating an
    /// empty volume — the mkfs-once path. Template id or name.
    #[serde(default)]
    pub from_template: Option<String>,
    /// Redundancy: `mirror`, `mirror:3`, `raid5:4+1`, `raid6:4+2`, with an
    /// optional `@rung` (`mirror:2@shelf`). A policy is a boundary — the
    /// create is refused when the node cannot place every leg on a distinct
    /// domain. With a policy, `array_id` is not needed: the volume's extents
    /// pick their own slabs.
    #[serde(default)]
    pub redundancy: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RedundancyRequest {
    pub redundancy: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct ResyncQuery {
    /// Also recompute and rewrite every stripe's parity.
    #[serde(default)]
    pub verify: bool,
}

#[derive(Debug, Deserialize)]
pub struct CreateSnapshotRequest {
    pub name: String,
    pub source_volume_id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct ResizeVolumeRequest {
    pub new_size: String,
}

async fn list_volumes(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "volumes", "method" => "list").increment(1);
    let vm = state.volume_manager.lock().await;
    let vols = vm.list_volumes().await;
    let mut items: Vec<VolumeResponse> = Vec::with_capacity(vols.len());
    for (id, name, vsize, allocated) in &vols {
        let d = describe(&vm, id).await;
        items.push(VolumeResponse {
            id: id.0,
            name: name.clone(),
            virtual_size_bytes: *vsize,
            virtual_size_human: human_size(*vsize),
            allocated_bytes: *allocated,
            allocated_human: human_size(*allocated),
            array_id: None,
            fs_uuid: d.fs_uuid,
            redundancy: d.redundancy,
            health: d.health,
            physical_bytes: d.physical_bytes,
            parent: d.parent,
            sealed: d.sealed,
            fs: d.fs,
        });
    }
    let count = items.len();
    Json(ListResponse { items, count })
}

async fn get_volume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "volumes", "method" => "get").increment(1);
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };

    let vol_id = VolumeId(uuid);
    let vm = state.volume_manager.lock().await;
    match vm.get_volume_handle(&vol_id) {
        Some(handle) => {
            let name = handle.name().await;
            let allocated = handle.allocated().await;
            let vsize = handle.capacity_bytes();
            let d = describe(&vm, &vol_id).await;
            let resp = VolumeResponse {
                id: uuid,
                name,
                virtual_size_bytes: vsize,
                virtual_size_human: human_size(vsize),
                allocated_bytes: allocated,
                allocated_human: human_size(allocated),
                array_id: None,
                fs_uuid: d.fs_uuid,
                redundancy: d.redundancy,
                health: d.health,
                physical_bytes: d.physical_bytes,
                parent: d.parent,
                sealed: d.sealed,
                fs: d.fs,
            };
            Json(resp).into_response()
        }
        None => ApiError::not_found(format!("volume {uuid} not found")),
    }
}

async fn create_volume(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateVolumeRequest>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "volumes", "method" => "create").increment(1);

    let size = match super::fstemplates::resolve_size(&req.size, None) {
        Ok(s) => s,
        Err(e) => return ApiError::bad_request(e),
    };

    // Cloning a template — or any sealed volume, by id or name — is a
    // snapshot plus a fresh filesystem UUID: no mkfs, no attach. Placement
    // comes from the source's own extents. One namespace: a template is a
    // volume that has been sealed (#76).
    if let Some(key) = req.from_template.as_deref() {
        let is_template = state.fstemplates.lock().await.find(key).is_some();
        let (vol_id, fs_uuid, size_bytes) = if is_template {
            match super::fstemplates::clone_for_volume_api(&state, key, &req.name, size).await {
                Ok(v) => v,
                Err(resp) => return resp,
            }
        } else {
            let source = { state.volume_manager.lock().await.find_volume(key).await };
            let Some(source) = source else {
                return ApiError::not_found(format!("no fstemplate or volume named {key}"));
            };
            let spec = crate::fs::template::CloneSpec { size_bytes: size, ..crate::fs::template::CloneSpec::new(&req.name) };
            match crate::fs::template::clone_volume(&state.volume_manager, source, &spec).await {
                Ok(c) => (c.volume_id, c.fs_uuid, c.size_bytes),
                Err(e) => return super::fstemplates::err(e),
            }
        };
        let vm = state.volume_manager.lock().await;
        let allocated = match vm.get_volume_handle(&vol_id) {
            Some(h) => h.allocated().await,
            None => 0,
        };
        let d = describe(&vm, &vol_id).await;
        let resp = VolumeResponse {
            id: vol_id.0,
            name: req.name,
            virtual_size_bytes: size_bytes,
            virtual_size_human: human_size(size_bytes),
            allocated_bytes: allocated,
            allocated_human: human_size(allocated),
            array_id: None,
            fs_uuid: fs_uuid.or(d.fs_uuid),
            redundancy: d.redundancy,
            health: d.health,
            physical_bytes: d.physical_bytes,
            parent: d.parent,
            sealed: d.sealed,
            fs: d.fs,
        };
        metrics::gauge!("stormblock_volumes_total").set(vm.list_volumes().await.len() as f64);
        return (axum::http::StatusCode::CREATED, Json(resp)).into_response();
    }

    let size = match size {
        Some(s) => s,
        None => return ApiError::bad_request("size is required"),
    };
    let redundancy = match req.redundancy.as_deref() {
        Some(r) => match crate::volume::RedundancyPolicy::parse(r) {
            Ok(p) => p,
            Err(e) => return ApiError::bad_request(format!("redundancy: {e}")),
        },
        None => crate::volume::RedundancyPolicy::none(),
    };
    let array_id = match req.array_id {
        Some(a) => Some(RaidArrayId(a)),
        None if req.redundancy.is_some() => None,
        None => return ApiError::bad_request(
            "array_id is required (or use from_template, or give a redundancy policy)",
        ),
    };

    // Verify array exists
    if let Some(array_id) = array_id {
        let arrays = state.arrays.read().await;
        if !arrays.contains_key(&array_id) {
            return ApiError::not_found(format!("array {} not found", array_id.0));
        }
    }

    let mut vm = state.volume_manager.lock().await;
    let created = match array_id {
        Some(a) if redundancy.is_none() => vm.create_volume(&req.name, size, a).await,
        _ => vm
            .create_volume_with(&req.name, size, crate::volume::CreateOptions::redundant(redundancy.clone()))
            .await,
    };
    match created {
        Ok(vol_id) => {
            let resp = VolumeResponse {
                id: vol_id.0,
                name: req.name,
                virtual_size_bytes: size,
                virtual_size_human: human_size(size),
                allocated_bytes: 0,
                allocated_human: human_size(0),
                array_id: array_id.map(|a| a.0),
                fs_uuid: None,
                redundancy: redundancy.spelling(),
                health: "healthy".into(),
                physical_bytes: 0,
                parent: None,
                sealed: false,
                fs: None,
            };
            metrics::gauge!("stormblock_volumes_total").set(vm.list_volumes().await.len() as f64);
            (axum::http::StatusCode::CREATED, Json(resp)).into_response()
        }
        Err(e @ crate::volume::VolumeError::InsufficientDomains { .. }) => {
            ApiError::conflict(format!("failed to create volume: {e}"))
        }
        Err(e) => ApiError::bad_request(format!("failed to create volume: {e}")),
    }
}

/// `GET /api/v1/volumes/{id}/health` — the full health report.
async fn volume_health(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };
    let vm = state.volume_manager.lock().await;
    match vm.health(&VolumeId(uuid)).await {
        Some(h) => Json(h).into_response(),
        None => ApiError::not_found(format!("volume {uuid} not found")),
    }
}

/// `PUT /api/v1/volumes/{id}/redundancy` — change the policy. Only
/// none/mirror → mirror is applied in place; the next resync adds or
/// drops legs. Anything involving parity is a re-stripe and is refused.
async fn set_redundancy(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<RedundancyRequest>,
) -> Response {
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };
    let policy = match crate::volume::RedundancyPolicy::parse(&req.redundancy) {
        Ok(p) => p,
        Err(e) => return ApiError::bad_request(format!("redundancy: {e}")),
    };
    let mut vm = state.volume_manager.lock().await;
    match vm.set_redundancy(VolumeId(uuid), policy.clone()).await {
        Ok(()) => Json(serde_json::json!({ "id": uuid, "redundancy": policy.spelling() })).into_response(),
        Err(crate::volume::VolumeError::VolumeNotFound(_)) => ApiError::not_found(format!("volume {uuid} not found")),
        Err(e @ crate::volume::VolumeError::InsufficientDomains { .. }) => ApiError::conflict(e.to_string()),
        Err(e) => ApiError::bad_request(e.to_string()),
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct SealRequest {
    /// Record what is on the volume, when the caller knows and the engine
    /// cannot read it (a filesystem it does not write). Absent: the engine
    /// reads the ext superblock if there is one.
    #[serde(default)]
    pub fs: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    /// Seal even if no filesystem can be read off it.
    #[serde(default)]
    pub force: bool,
}

/// `POST /api/v1/volumes/{id}/seal` — a volume becomes what clones are taken
/// from: no more writes (#76). The ext superblock, if there is one, is read
/// and recorded so every clone can be stamped with its own identity.
async fn seal_volume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<SealRequest>>,
) -> Response {
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };
    let req = body.map(|b| b.0).unwrap_or_default();
    let vol_id = VolumeId(uuid);
    let dev = match state.volume_manager.lock().await.get_volume(&vol_id) {
        Some(d) => d,
        None => return ApiError::not_found(format!("volume {uuid} not found")),
    };
    let existing = state.volume_manager.lock().await.fs_info(&vol_id).cloned();
    let fs = match crate::fs::ext4::read_layout(&dev).await {
        Ok(l) => Some(crate::volume::FsInfo {
            kind: existing.as_ref().map(|f| f.kind.clone()).unwrap_or_else(|| "ext4".into()),
            journal: l.has_journal,
            features: existing.as_ref().and_then(|f| f.features.clone()),
            sixty_four_bit: l.sixty_four_bit,
            metadata_csum: l.metadata_csum,
            csum_seed: l.csum_seed,
            label: req.label.clone().unwrap_or(l.label.clone()),
            uuid: Some(l.uuid),
        }),
        Err(e) => match (&req.fs, req.force, existing) {
            (Some(kind), _, _) => Some(crate::volume::FsInfo {
                kind: kind.clone(), journal: false, features: None, sixty_four_bit: false,
                metadata_csum: false, csum_seed: false, label: req.label.clone().unwrap_or_default(), uuid: None,
            }),
            (None, true, existing) => existing,
            (None, false, _) => {
                return ApiError::conflict(format!(
                    "volume {uuid} has no readable filesystem ({e}); seal it with force=true, or say what is on it"
                ))
            }
        },
    };
    drop(dev);
    let mut vm = state.volume_manager.lock().await;
    match vm.seal_volume(vol_id, fs).await {
        Ok(()) => {
            let d = describe(&vm, &vol_id).await;
            Json(serde_json::json!({ "id": uuid, "sealed": true, "fs": d.fs })).into_response()
        }
        Err(e) => ApiError::internal(e.to_string()),
    }
}

/// `DELETE /api/v1/volumes/{id}/seal` — reopen for writes.
async fn unseal_volume(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };
    let mut vm = state.volume_manager.lock().await;
    match vm.unseal_volume(VolumeId(uuid)).await {
        Ok(()) => Json(serde_json::json!({ "id": uuid, "sealed": false })).into_response(),
        Err(crate::volume::VolumeError::VolumeNotFound(_)) => ApiError::not_found(format!("volume {uuid} not found")),
        Err(e) => ApiError::internal(e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
pub struct CloneRequest {
    pub name: String,
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    /// fsck the clone before handing it out (default true).
    #[serde(default = "default_true")]
    pub verify: bool,
}

fn default_true() -> bool {
    true
}

/// `POST /api/v1/volumes/{id}/clone` — the one answer to "clone this": a
/// snapshot of a sealed volume, stamped with its own filesystem UUID and
/// checked when the source carries a filesystem, lineage recorded (#76).
async fn clone_volume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<CloneRequest>,
) -> Response {
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };
    let size = match super::fstemplates::resolve_size(&req.size, None) {
        Ok(s) => s,
        Err(e) => return ApiError::bad_request(e),
    };
    let mut spec = crate::fs::template::CloneSpec::new(&req.name);
    spec.size_bytes = size;
    spec.label = req.label;
    spec.verify = req.verify;
    match crate::fs::template::clone_volume(&state.volume_manager, VolumeId(uuid), &spec).await {
        Ok(c) => {
            let vm = state.volume_manager.lock().await;
            let d = describe(&vm, &c.volume_id).await;
            let allocated = match vm.get_volume_handle(&c.volume_id) {
                Some(h) => h.allocated().await,
                None => 0,
            };
            let resp = VolumeResponse {
                id: c.volume_id.0,
                name: req.name,
                virtual_size_bytes: c.size_bytes,
                virtual_size_human: human_size(c.size_bytes),
                allocated_bytes: allocated,
                allocated_human: human_size(allocated),
                array_id: None,
                fs_uuid: c.fs_uuid.or(d.fs_uuid),
                redundancy: d.redundancy,
                health: d.health,
                physical_bytes: d.physical_bytes,
                parent: d.parent,
                sealed: d.sealed,
                fs: d.fs,
            };
            (axum::http::StatusCode::CREATED, Json(resp)).into_response()
        }
        Err(e) => super::fstemplates::err(e),
    }
}

/// `GET /api/v1/volumes/{id}/lineage` — the volume, its parent, and so on up;
/// and the volumes cloned directly from it.
async fn volume_lineage(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };
    let vm = state.volume_manager.lock().await;
    let vol_id = VolumeId(uuid);
    if vm.get_volume(&vol_id).is_none() {
        return ApiError::not_found(format!("volume {uuid} not found"));
    }
    let mut ancestors = Vec::new();
    for a in vm.lineage(&vol_id) {
        let present = vm.get_volume(&a).is_some();
        let name = match vm.get_volume_handle(&a) {
            Some(h) => Some(h.name().await),
            None => None,
        };
        ancestors.push(serde_json::json!({ "id": a.0, "name": name, "present": present, "sealed": vm.is_sealed(&a) }));
    }
    let children: Vec<serde_json::Value> = vm.children(&vol_id).iter().map(|c| serde_json::json!({ "id": c.0 })).collect();
    Json(serde_json::json!({ "id": uuid, "lineage": ancestors, "children": children })).into_response()
}

/// `POST /api/v1/volumes/{id}/restripe {"redundancy": "raid5:4+1"}` — change
/// the policy to or from parity by rebuilding the placement. Offline: refused
/// while the volume is exported or attached.
async fn restripe_volume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<RedundancyRequest>,
) -> Response {
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };
    let policy = match crate::volume::RedundancyPolicy::parse(&req.redundancy) {
        Ok(p) => p,
        Err(e) => return ApiError::bad_request(format!("redundancy: {e}")),
    };
    let exported = state.exports.read().await.iter().any(|e| e.volume_id == uuid)
        || state.ublk_exports.lock().await.is_exported(&uuid.to_string());
    if exported {
        return ApiError::conflict(format!(
            "volume {uuid} is exported; a restripe rebuilds its placement offline — detach it first"
        ));
    }
    let mut vm = state.volume_manager.lock().await;
    match vm.restripe(VolumeId(uuid), policy).await {
        Ok(report) => {
            let health = vm.health(&VolumeId(uuid)).await;
            Json(serde_json::json!({ "id": uuid, "report": report, "health": health })).into_response()
        }
        Err(crate::volume::VolumeError::VolumeNotFound(_)) => ApiError::not_found(format!("volume {uuid} not found")),
        Err(e @ crate::volume::VolumeError::InsufficientDomains { .. }) => ApiError::conflict(e.to_string()),
        Err(e) => ApiError::internal(e.to_string()),
    }
}

/// `POST /api/v1/volumes/{id}/resync[?verify=true]` — rebuild missing legs,
/// apply a changed policy, clear the failed set.
async fn resync_volume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<ResyncQuery>,
) -> Response {
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };
    let mut vm = state.volume_manager.lock().await;
    match vm.resync_volume(VolumeId(uuid), q.verify).await {
        Ok(report) => {
            let health = vm.health(&VolumeId(uuid)).await;
            Json(serde_json::json!({ "id": uuid, "report": report, "health": health })).into_response()
        }
        Err(crate::volume::VolumeError::VolumeNotFound(_)) => ApiError::not_found(format!("volume {uuid} not found")),
        Err(e) => ApiError::internal(e.to_string()),
    }
}

async fn delete_volume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "volumes", "method" => "delete").increment(1);
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };

    let vol_id = VolumeId(uuid);

    // Refuse while anything is serving it. This asks the shared question
    // rather than only checking the export table, so a volume backing a live
    // iSCSI LUN, a ublk device, or a StormFS version pin is covered by the
    // same answer the move guard and the template sweep use.
    let busy = super::what_is_serving(&state, uuid).await;
    if !busy.is_empty() {
        return ApiError::conflict(format!(
            "cannot delete volume {uuid}: it is still served by {}",
            busy.join(", ")
        ));
    }

    let mut vm = state.volume_manager.lock().await;
    match vm.delete_volume(vol_id).await {
        Ok(()) => {
            metrics::gauge!("stormblock_volumes_total").set(vm.list_volumes().await.len() as f64);
            axum::http::StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => ApiError::not_found(format!("volume {uuid}: {e}")),
    }
}

async fn create_snapshot(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateSnapshotRequest>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "volumes", "method" => "snapshot").increment(1);

    let source_id = VolumeId(req.source_volume_id);

    // A snapshot of a volume the engine knows carries a filesystem is a
    // clone, and a clone always gets its own identity (#76): two live
    // filesystems must never claim one UUID. A snapshot of anything else is
    // the plain map clone it always was.
    let has_fs = state.volume_manager.lock().await.fs_info(&source_id).is_some();
    let created = if has_fs {
        let mut spec = crate::fs::template::CloneSpec::new(&req.name);
        spec.verify = false;
        crate::fs::template::clone_volume_unsealed_ok(&state.volume_manager, source_id, &spec)
            .await
            .map(|c| c.volume_id)
            .map_err(|e| e.to_string())
    } else {
        state.volume_manager.lock().await.create_snapshot(source_id, &req.name).await.map_err(|e| e.to_string())
    };
    let vm = state.volume_manager.lock().await;
    match created {
        Ok(snap_id) => {
            let handle = vm.get_volume_handle(&snap_id).unwrap();
            let allocated = handle.allocated().await;
            let vsize = handle.capacity_bytes();
            let d = describe(&vm, &snap_id).await;
            let resp = VolumeResponse {
                id: snap_id.0,
                name: req.name,
                virtual_size_bytes: vsize,
                virtual_size_human: human_size(vsize),
                allocated_bytes: allocated,
                allocated_human: human_size(allocated),
                array_id: None,
                fs_uuid: d.fs_uuid,
                redundancy: d.redundancy,
                health: d.health,
                physical_bytes: d.physical_bytes,
                parent: d.parent,
                sealed: d.sealed,
                fs: d.fs,
            };
            metrics::gauge!("stormblock_volumes_total").set(vm.list_volumes().await.len() as f64);
            (axum::http::StatusCode::CREATED, Json(resp)).into_response()
        }
        Err(e) => ApiError::bad_request(format!("failed to create snapshot: {e}")),
    }
}

async fn resize_volume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<ResizeVolumeRequest>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "volumes", "method" => "resize").increment(1);
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };

    let new_size = match parse_size(&req.new_size) {
        Ok(s) => s,
        Err(e) => return ApiError::bad_request(format!("invalid size '{}': {e}", req.new_size)),
    };

    let vol_id = VolumeId(uuid);

    // Check if volume is exported
    {
        let exports = state.exports.read().await;
        if exports.iter().any(|e| e.volume_id == uuid) {
            return ApiError::conflict("cannot resize volume with active exports".to_string());
        }
    }

    let mut vm = state.volume_manager.lock().await;
    match vm.resize_volume(vol_id, new_size).await {
        // Growth only — a shrink frees every extent past the new end and no
        // filesystem above it can follow, so it is a conflict rather than a
        // malformed request (#19).
        Err(crate::volume::VolumeError::ShrinkRefused { current, requested }) => {
            return ApiError::conflict(format!(
                "refusing to shrink volume {uuid} from {current} to {requested} bytes: a \
                 filesystem on it cannot follow, and the extents past the new end would be \
                 freed immediately. Moving a volume to a smaller one is a copy, not a resize."
            ))
        }
        Ok(()) => {
            let handle = match vm.get_volume_handle(&vol_id) {
                Some(h) => h,
                None => return ApiError::not_found(format!("volume {uuid} not found")),
            };
            let name = handle.name().await;
            let allocated = handle.allocated().await;
            let vsize = handle.capacity_bytes();
            let d = describe(&vm, &vol_id).await;
            let resp = VolumeResponse {
                id: uuid,
                name,
                virtual_size_bytes: vsize,
                virtual_size_human: human_size(vsize),
                allocated_bytes: allocated,
                allocated_human: human_size(allocated),
                array_id: None,
                fs_uuid: d.fs_uuid,
                redundancy: d.redundancy,
                health: d.health,
                physical_bytes: d.physical_bytes,
                parent: d.parent,
                sealed: d.sealed,
                fs: d.fs,
            };
            Json(resp).into_response()
        }
        Err(e) => ApiError::bad_request(format!("failed to resize volume: {e}")),
    }
}

/// `POST /api/v1/volumes/{id}/fsck` — check a volume's filesystem, and with
/// `?repair=true` correct what can be corrected.
///
/// Worth having here because of who cannot do it themselves: RouterOS has no
/// fsck and cannot cleanly unmount a network disk, so a volume it left dirty
/// has nowhere else to be repaired. The engine has the volume locally and can
/// check it without an attach.
async fn fsck_volume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "volumes", "method" => "fsck")
        .increment(1);
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };
    let repair = matches!(
        q.get("repair").map(|v| v.as_str()),
        Some("") | Some("1") | Some("true") | Some("yes")
    );

    // The lock is held only long enough to take a handle: a check walks every
    // group, and no other volume operation should queue behind it.
    let dev = {
        let vm = state.volume_manager.lock().await;
        match vm.get_volume(&VolumeId(uuid)) {
            Some(d) => d,
            None => return ApiError::not_found(format!("volume {uuid} not found")),
        }
    };

    // Repairing a volume something else is writing would race that writer.
    if repair {
        let exports = state.exports.read().await;
        if exports.iter().any(|e| e.volume_id == uuid) {
            return ApiError::conflict(
                "volume is exported — detach it before repairing, or run without repair=true",
            );
        }
    }

    let result = if repair {
        crate::fs::ext4::repair(&dev).await
    } else {
        crate::fs::ext4::check(&dev).await
    };

    match result {
        Ok(report) => Json(serde_json::json!({
            "volume_id": uuid,
            "clean": report.is_clean(),
            "repaired": report.repaired_anything(),
            "exit_code": report.exit_code(),
            "inodes_used": report.inodes_used,
            "blocks_used": report.blocks_used,
            "directories": report.directories,
            "problems": report.problems.iter().map(|p| serde_json::json!({
                "pass": p.pass,
                "code": p.code,
                "severity": format!("{:?}", p.severity).to_lowercase(),
                "message": p.message,
                "fixed": p.fixed,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => ApiError::bad_request(format!("volume {uuid}: {e}")),
    }
}

#[derive(Debug, Deserialize)]
pub struct WriteFilesRequest {
    pub files: Vec<super::fstemplates::SeedFileRequest>,
}

/// The volume behind an id, refusing while something else may be writing it.
///
/// A file written under a live mount would be a write the mounted filesystem
/// does not know about — its cached metadata would overwrite ours.
async fn writable_volume(
    state: &AppState,
    uuid: Uuid,
) -> Result<Arc<dyn BlockDevice>, Response> {
    {
        let exports = state.exports.read().await;
        if exports.iter().any(|e| e.volume_id == uuid) {
            return Err(ApiError::conflict(
                "volume is exported — detach it before writing files into it",
            ));
        }
    }
    let vm = state.volume_manager.lock().await;
    vm.get_volume(&VolumeId(uuid))
        .ok_or_else(|| ApiError::not_found(format!("volume {uuid} not found")))
}

/// `POST /api/v1/volumes/{id}/files` — write files into the volume's
/// filesystem in userspace: no mount, no loop device, no attach.
async fn write_volume_files(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<WriteFilesRequest>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "volumes", "method" => "write_files")
        .increment(1);
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };

    let mut files = Vec::with_capacity(req.files.len());
    for f in req.files {
        match f.resolve() {
            Ok(s) => files.push(s),
            Err(e) => return ApiError::bad_request(e),
        }
    }
    if files.is_empty() {
        return ApiError::bad_request("no files given");
    }

    let dev = match writable_volume(&state, uuid).await {
        Ok(d) => d,
        Err(resp) => return resp,
    };

    if let Err(e) = crate::fs::files::write_files(&dev, &files).await {
        return ApiError::bad_request(format!("volume {uuid}: {e}"));
    }

    // Writing metadata is exactly where a filesystem goes quietly wrong, so
    // say whether it still checks out rather than leaving the caller to ask.
    let clean = crate::fs::ext4::check(&dev)
        .await
        .map(|r| r.is_clean())
        .unwrap_or(false);

    Json(serde_json::json!({
        "volume_id": uuid,
        "written": files.iter().map(|f| &f.path).collect::<Vec<_>>(),
        "clean": clean,
    }))
    .into_response()
}

/// `GET /api/v1/volumes/{id}/files?path=/etc/hostname` — read one file, or
/// list a directory when `path` names one.
async fn read_volume_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "volumes", "method" => "read_files")
        .increment(1);
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };
    let path = match q.get("path") {
        Some(p) => p.clone(),
        None => return ApiError::bad_request("path is required"),
    };

    let dev = {
        let vm = state.volume_manager.lock().await;
        match vm.get_volume(&VolumeId(uuid)) {
            Some(d) => d,
            None => return ApiError::not_found(format!("volume {uuid} not found")),
        }
    };

    // A directory reads as a listing; a file reads as its bytes. Base64 so a
    // binary file survives the trip.
    match crate::fs::files::list_dir(&dev, &path).await {
        Ok(entries) => Json(serde_json::json!({
            "volume_id": uuid,
            "path": path,
            "entries": entries.iter().map(|e| serde_json::json!({
                "name": e.name,
                "is_dir": e.is_dir,
                "size": e.size,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(_) => match crate::fs::files::read_file(&dev, &path).await {
            Ok(bytes) => {
                use base64::Engine;
                Json(serde_json::json!({
                    "volume_id": uuid,
                    "path": path,
                    "size": bytes.len(),
                    "contents_base64": base64::engine::general_purpose::STANDARD.encode(&bytes),
                }))
                .into_response()
            }
            Err(e) => ApiError::not_found(format!("volume {uuid}: {e}")),
        },
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list_volumes).post(create_volume))
        .route("/{id}", get(get_volume).delete(delete_volume))
        .route("/{id}/resize", axum::routing::patch(resize_volume))
        .route("/{id}/health", get(volume_health))
        .route("/{id}/redundancy", axum::routing::put(set_redundancy))
        .route("/{id}/resync", axum::routing::post(resync_volume))
        .route("/{id}/restripe", axum::routing::post(restripe_volume))
        .route("/{id}/seal", axum::routing::post(seal_volume).delete(unseal_volume))
        .route("/{id}/clone", axum::routing::post(clone_volume))
        .route("/{id}/lineage", get(volume_lineage))
        .route("/{id}/fsck", axum::routing::post(fsck_volume))
        .route("/{id}/files", get(read_volume_file).post(write_volume_files))
        .route("/snapshots", axum::routing::post(create_snapshot))
        .with_state(state)
}
