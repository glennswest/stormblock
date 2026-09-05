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
    /// `rw` or `ro` — whether the volume is set to take writes. A setting,
    /// changeable at any point in the volume's life, unlike sealing.
    pub access: String,
    /// Whether a write would actually land: not sealed *and* not read-only.
    /// A sealed volume reports `access: "rw", writable: false` — sealing is
    /// the stronger statement and does not disturb the setting underneath.
    pub writable: bool,
    /// Which half of the node's mutable storage the volume lives in:
    /// `system` (replaced wholesale by an install) or `data` (identity and
    /// state, which no install path formats). A clone is in its source's
    /// half whatever it is named, so this is how an operator checks that the
    /// volume they meant to be durable actually is (#88).
    pub role: String,
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
    access: String,
    writable: bool,
    role: String,
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
                access: handle.access().to_string(),
                writable: handle.writable(),
                role: handle.placement_role().to_string(),
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
            access: crate::volume::Access::ReadWrite.to_string(),
            writable: true,
            role: crate::drive::slab::SlabRole::System.to_string(),
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
    /// Which slabs this volume may live in: `system` (goldens, replaced by an
    /// image) or `data` (identity and state, which no install may reformat).
    ///
    /// Absent lets the node decide: system where it has a system slab,
    /// otherwise data. An appliance whose whole job is holding images has
    /// data slabs and nothing else — its content is meant to outlive a
    /// rebuild of the box — and without that every create asked for a system
    /// slab and found none (#93).
    #[serde(default)]
    pub role: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RedundancyRequest {
    pub redundancy: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct DeleteQuery {
    /// Delete even though a synonym points at it, leaving that name
    /// dangling.
    #[serde(default)]
    pub force: bool,
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
            access: d.access.clone(),
            writable: d.writable,
            role: d.role.clone(),
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
                access: d.access.clone(),
                writable: d.writable,
                role: d.role.clone(),
                fs: d.fs,
            };
            Json(resp).into_response()
        }
        None => ApiError::not_found(format!("volume {uuid} not found")),
    }
}

#[derive(Debug, Deserialize)]
pub struct ComposeComponentRequest {
    /// The volume to share in, by id or name. Usually a sealed golden.
    pub volume: String,
    /// Where it starts in the composed volume. Bytes, slot-aligned.
    #[serde(default)]
    pub at: u64,
}

#[derive(Debug, Deserialize)]
pub struct ComposeVolumeRequest {
    pub name: String,
    /// Total size. Omitted takes the end of the last component.
    #[serde(default)]
    pub size: Option<String>,
    pub components: Vec<ComposeComponentRequest>,
}

/// Compose a volume out of other volumes, sharing their extents.
///
/// The result is a disk made *of* its components rather than a copy of them:
/// no bytes are read or written, only the map. What it costs is the map.
#[derive(Debug, Deserialize)]
pub struct RetierRequest {
    /// Where the volume's extents should live: `hot`, `warm`, `cool`, `cold`.
    pub tier: String,
}

/// `POST /api/v1/volumes/{id}/tier` — move a volume's extents to another tier.
///
/// The newest image belongs on the fastest drive and last month's does not.
/// This is how it gets moved: every extent that is not already on a slab of
/// the target tier is migrated to one, **online**, one extent per lock cycle
/// so the volume keeps serving while it moves. Nothing about the volume
/// changes except where its bytes are — same id, same name, same contents,
/// same exports.
///
/// Shared extents move correctly: `migrate_leg` rewrites every map that named
/// the old slot, so a golden and every disk composed from it follow it down
/// together rather than being torn apart.
///
/// Returns as soon as the work is scheduled; the slabs' free counts are how
/// you watch it happen.
async fn retier_volume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<RetierRequest>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "volumes", "method" => "retier").increment(1);

    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };
    let vol_id = VolumeId(uuid);

    let Some(tier) = super::slabs::parse_tier(&req.tier) else {
        return ApiError::bad_request(format!(
            "invalid tier '{}' (hot, warm, cool, cold)", req.tier
        ));
    };

    let role = {
        let vm = state.volume_manager.lock().await;
        match vm.get_volume_handle(&vol_id) {
            Some(h) => h.placement_role(),
            None => return ApiError::not_found(format!("volume {vol_id} not found")),
        }
    };

    // A destination of the right tier *and* the right role. A data volume does
    // not get demoted onto a system slab: the roles mean different things about
    // what an install is allowed to erase.
    let dest = {
        let reg = state.slab_registry.read().await;
        reg.best_slab_for_tier_in_role(tier, role)
    };
    let Some(dest) = dest else {
        return ApiError::conflict(format!(
            "no {} slab in the {role} role to move to", req.tier
        ));
    };

    // What has to move: everything not already there.
    // The work is extent-by-extent and a 32 GB volume is ten thousand of them,
    // so it runs behind the response. The slabs' free counts are how you watch
    // it happen, and "retier complete" is logged when it is.
    let vm_handle = state.volume_manager.clone();
    tokio::spawn(async move {
        let mut vm = vm_handle.lock().await;
        if let Err(e) = vm.retier_volume(vol_id, tier).await {
            tracing::error!(volume = %vol_id, "retier failed: {e}");
        }
    });

    (axum::http::StatusCode::ACCEPTED, Json(serde_json::json!({
        "volume": vol_id.0,
        "tier": tier.to_string(),
        "role": role.to_string(),
        "destination_slab": dest.0,
        "started": true,
    }))).into_response()
}

async fn compose_volume(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ComposeVolumeRequest>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "volumes", "method" => "compose").increment(1);

    if req.components.is_empty() {
        return ApiError::bad_request("a composed volume needs at least one component");
    }

    let size = match super::fstemplates::resolve_size(&req.size, None) {
        Ok(v) => v,
        Err(e) => return ApiError::bad_request(e),
    };

    // Resolve every name before composing, so a typo in the last component
    // does not leave a half-built volume behind.
    let mut placements = Vec::with_capacity(req.components.len());
    {
        let vm = state.volume_manager.lock().await;
        for c in &req.components {
            match vm.find_volume(&c.volume).await {
                Some(id) => placements.push((id, c.at)),
                None => return ApiError::not_found(format!("no volume named {}", c.volume)),
            }
        }
    }

    let mut vm = state.volume_manager.lock().await;
    let id = match vm.compose_volume(&req.name, size, &placements).await {
        Ok(id) => id,
        Err(e) => return ApiError::bad_request(format!("failed to compose volume: {e}")),
    };

    // What it cost is the interesting number, so report it: a composition
    // that shares everything allocates nothing of its own.
    let (virtual_size, allocated) = vm
        .list_volumes()
        .await
        .into_iter()
        .find(|(vid, _, _, _)| *vid == id)
        .map(|(_, _, v, a)| (v, a))
        .unwrap_or((0, 0));

    let resp = VolumeResponse {
        id: id.0,
        name: req.name,
        virtual_size_bytes: virtual_size,
        virtual_size_human: human_size(virtual_size),
        allocated_bytes: allocated,
        allocated_human: human_size(allocated),
        array_id: None,
        fs_uuid: None,
        redundancy: vm.redundancy(&id).map(|r| r.spelling()).unwrap_or_else(|| "none".into()),
        health: "healthy".into(),
        physical_bytes: allocated,
        parent: placements.first().map(|(p, _)| p.0),
        sealed: false,
        access: crate::volume::Access::ReadWrite.to_string(),
        writable: true,
        role: vm
            .get_volume_handle(&id)
            .map(|h| h.placement_role().to_string())
            .unwrap_or_else(|| crate::drive::slab::SlabRole::System.to_string()),
        fs: None,
    };
    metrics::gauge!("stormblock_volumes_total").set(vm.list_volumes().await.len() as f64);
    (axum::http::StatusCode::CREATED, Json(resp)).into_response()
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
            access: d.access.clone(),
            writable: d.writable,
            role: d.role.clone(),
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
    // An array binding is legacy. A volume's extents pick their own slabs, so
    // the only thing a create needs is somewhere to pick from — and demanding
    // an `array_id` for the plain case meant `{"name","size"}`, the request
    // every consumer actually sends, was refused on a node that had storage
    // and had adopted it.
    let array_id = req.array_id.map(RaidArrayId);
    if array_id.is_none() && state.slab_registry.read().await.is_empty() {
        return ApiError::conflict(
            "this node has no slabs to place a volume in: format one \
             (POST /api/v1/slabs) or configure a drive that already carries one",
        );
    }

    // Verify array exists
    if let Some(array_id) = array_id {
        let arrays = state.arrays.read().await;
        if !arrays.contains_key(&array_id) {
            return ApiError::not_found(format!("array {} not found", array_id.0));
        }
    }

    let role = match req.role.as_deref() {
        Some(r) => match crate::drive::slab::SlabRole::parse(r) {
            Some(r) => Some(r),
            None => {
                return ApiError::bad_request(format!("invalid role '{r}' (use system or data)"))
            }
        },
        None => None,
    };

    let mut vm = state.volume_manager.lock().await;
    let created = match array_id {
        Some(a) if redundancy.is_none() && role.is_none() => vm.create_volume(&req.name, size, a).await,
        _ => vm
            .create_volume_with(
                &req.name,
                size,
                crate::volume::CreateOptions::redundant(redundancy.clone()).in_role_opt(role),
            )
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
                access: crate::volume::Access::ReadWrite.to_string(),
                writable: true,
                // What it was actually placed in, which is not always what
                // was asked for: a node with no system slab places in data.
                role: vm.volume_role(&vol_id).unwrap_or_default().to_string(),
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
    // ext4 by superblock; else a partition table or an ISO (a VM disk golden
    // is a whole disk, and sealing it must not need `force`).
    let fs = match crate::fs::disk::probe(&dev).await {
        Some(mut f) => {
            if let Some(l) = &req.label {
                f.label = l.clone();
            }
            if f.kind == "ext4" {
                if let Some(ex) = &existing {
                    f.features = ex.features.clone();
                }
            }
            Some(f)
        }
        None => match (&req.fs, req.force, existing) {
            (Some(kind), _, _) => Some(crate::volume::FsInfo {
                kind: kind.clone(), journal: false, features: None, sixty_four_bit: false,
                metadata_csum: false, csum_seed: false, label: req.label.clone().unwrap_or_default(), uuid: None,
            }),
            (None, true, existing) => existing,
            (None, false, _) => {
                return ApiError::conflict(format!(
                    "volume {uuid} has no readable filesystem or partition table; seal it with force=true, or say what is on it"
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
#[derive(Debug, Deserialize)]
pub struct AccessRequest {
    /// `rw` or `ro` (`read-write`/`read-only` also accepted).
    pub access: String,
}

/// `PUT /api/v1/volumes/{id}/access` — take the volume out of service for
/// writes, or put it back.
///
/// The lifecycle lever sealing is not. A sealed volume is the master copy
/// clones are taken from and is read-only for that reason; this is a setting
/// on an ordinary clone, it moves both ways, and it survives a restart. The
/// response reports both, because "sealed" wins over "rw" and an operator
/// who sets `rw` on a sealed volume should see that it is still not writable
/// rather than be told the setting took.
async fn set_access(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<AccessRequest>,
) -> Response {
    let Some(access) = crate::volume::Access::parse(&req.access) else {
        return ApiError::bad_request(format!(
            "invalid access '{}' (use rw or ro)", req.access
        ));
    };
    let Some(vol_id) = resolve_volume(&state, &id).await else {
        return ApiError::not_found(format!("no volume {id}"));
    };
    let mut vm = state.volume_manager.lock().await;
    match vm.set_access(vol_id, access).await {
        Ok(()) => Json(serde_json::json!({
            "id": vol_id.0,
            "access": access.to_string(),
            "sealed": vm.is_sealed(&vol_id),
            "writable": vm.writable(&vol_id),
        }))
        .into_response(),
        Err(crate::volume::VolumeError::VolumeNotFound(_)) => {
            ApiError::not_found(format!("volume {vol_id} not found"))
        }
        Err(e) => ApiError::internal(e.to_string()),
    }
}

/// `GET /api/v1/volumes/{id}/access` — the setting and whether writes land.
async fn get_access(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let Some(vol_id) = resolve_volume(&state, &id).await else {
        return ApiError::not_found(format!("no volume {id}"));
    };
    let vm = state.volume_manager.lock().await;
    match vm.access(&vol_id) {
        Some(a) => Json(serde_json::json!({
            "id": vol_id.0,
            "access": a.to_string(),
            "sealed": vm.is_sealed(&vol_id),
            "writable": vm.writable(&vol_id),
        }))
        .into_response(),
        None => ApiError::not_found(format!("volume {vol_id} not found")),
    }
}

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
    /// Put the clone in `system` or `data` slabs instead of the source's.
    ///
    /// A copy-on-write clone shares its source's slots, so by default the
    /// clone is in the source's half of the node's storage whatever it is
    /// named — cloning a system golden into something called `…-data` gives a
    /// volume an install replaces. Naming the other role here crosses the
    /// boundary properly, as a real copy that shares nothing (#88).
    #[serde(default)]
    pub role: Option<String>,
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
    // By name as well as by id.
    //
    // A golden is *named* by whatever references it — a VM spec says
    // `dataVolume: { name: fedora-43-x86_64 }`, an image pull writes that
    // name, and nobody carries the uuid around. The volume manager has always
    // resolved either; this door did not, so cloning a golden by the only
    // handle its consumers have came back as "invalid UUID".
    let uuid = match resolve_volume(&state, &id).await {
        Some(u) => u.0,
        None => return ApiError::not_found(format!("no volume {id}")),
    };
    let size = match super::fstemplates::resolve_size(&req.size, None) {
        Ok(s) => s,
        Err(e) => return ApiError::bad_request(e),
    };
    let role = match req.role.as_deref() {
        Some(r) => match crate::drive::slab::SlabRole::parse(r) {
            Some(r) => Some(r),
            None => {
                return ApiError::bad_request(format!("invalid role '{r}' (use system or data)"))
            }
        },
        None => None,
    };
    let mut spec = crate::fs::template::CloneSpec::new(&req.name);
    spec.size_bytes = size;
    spec.label = req.label;
    spec.verify = req.verify;
    spec.role = role;
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
                access: d.access.clone(),
                writable: d.writable,
                role: d.role.clone(),
                fs: d.fs,
            };
            (axum::http::StatusCode::CREATED, Json(resp)).into_response()
        }
        Err(e) => super::fstemplates::err(e),
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct AttachRequest {
    /// `rw` (the default) or `ro`.
    ///
    /// A read-write attach of a golden is refused, and that is the whole
    /// rule: **you write to a clone, never to the master copy.** A sealed
    /// volume is what clones are taken from, so the answer to "I need to
    /// write to it" is always a clone of it, never a way in. Refusing here
    /// rather than at the first write means the caller is told *before* a
    /// guest boots onto storage that will not take its writes.
    ///
    /// `ro` is an assertion about intent, not a transport-level lock: the
    /// engine's own gate is what actually refuses the write, and it answers
    /// "write protected" when it does.
    #[serde(default)]
    pub mode: Option<String>,
    /// The node attaching. Defaults to this node, which is the only one a
    /// local ublk device can be offered to.
    #[serde(default)]
    pub node: Option<String>,
    /// `ublk` (local device; only when this node is the one attaching and
    /// ublk is enabled), `nvme-tcp`, or absent for the engine's choice —
    /// ublk when it can, nvme-tcp otherwise.
    #[serde(default)]
    pub transport: Option<String>,
}

/// `POST /api/v1/volumes/{id}/attach` — a block device for any engine
/// volume (#78). Attach is a property of the volume, like seal, clone and
/// lineage; it does not require the volume to have come through `/v1`.
///
/// Returns the same `AttachInfo` `/v1` returns: `ublk` with a `device_hint`
/// for the local fast path, or `nvme_tcp` with the shared subsystem's NQN,
/// address and this volume's NSID. Idempotent: attaching twice returns the
/// same device or namespace.
///
/// What `/v1` has and this does not — epochs, fencing, the read-write
/// master gate and the dual-attach window — is the CSI contract, and a
/// caller that wants those uses `/v1`. This is the local-node door.
async fn attach_volume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<AttachRequest>>,
) -> Response {
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };
    let req = body.map(|b| b.0).unwrap_or_default();
    let vol_id = VolumeId(uuid);
    let mode = match req.mode.as_deref() {
        None => crate::volume::Access::ReadWrite,
        Some(m) => match crate::volume::Access::parse(m) {
            Some(a) => a,
            None => return ApiError::bad_request(format!("invalid mode '{m}' (use rw or ro)")),
        },
    };
    let device = match state.volume_manager.lock().await.get_volume(&vol_id) {
        Some(d) => d,
        None => return ApiError::not_found(format!("volume {uuid} not found")),
    };

    // Writes go to a clone. A golden is the master copy and takes none, and
    // an attach is the last point where saying so is cheap.
    if mode == crate::volume::Access::ReadWrite {
        let vm = state.volume_manager.lock().await;
        if vm.is_sealed(&vol_id) {
            return ApiError::conflict(format!(
                "volume {uuid} is sealed: a golden takes no writes. Clone it \
                 (POST /api/v1/volumes/{uuid}/clone) and attach the clone, or attach it \
                 with mode=ro"
            ));
        }
        if !vm.writable(&vol_id) {
            return ApiError::conflict(format!(
                "volume {uuid} is read-only: set its access to rw \
                 (PUT /api/v1/volumes/{uuid}/access) or attach it with mode=ro"
            ));
        }
    }
    let local_node = state.v1.lock().await.local_node.clone();
    let node = req.node.clone().unwrap_or_else(|| local_node.clone());
    let want = req.transport.as_deref().map(|t| t.to_ascii_lowercase());
    match want.as_deref() {
        None | Some("ublk") | Some("nvme-tcp") | Some("nvme_tcp") | Some("nvmeof") => {}
        Some(other) => return ApiError::bad_request(format!("transport {other:?}: use ublk or nvme-tcp")),
    }
    let key = uuid.to_string();

    // Local fast path, on the same terms as /v1: enabled, this node, and
    // backed here — which every engine volume is.
    if want.as_deref() != Some("nvme-tcp") && want.as_deref() != Some("nvme_tcp") && want.as_deref() != Some("nvmeof")
        && crate::mgmt::ublk_export::should_offer_ublk(
            state.config.management.ublk_transport,
            &node,
            &local_node,
            true,
        )
    {
        if let Some(path) = state.ublk_exports.lock().await.ensure(&key, device.clone()) {
            return Json(super::v1::AttachInfo::Ublk { device_hint: path }).into_response();
        }
        if want.as_deref() == Some("ublk") {
            return ApiError::conflict(
                "ublk was asked for but is not available on this node (kernel ublk_drv, or ublk_transport is off)",
            );
        }
    } else if want.as_deref() == Some("ublk") {
        return ApiError::conflict(format!(
            "ublk is a local device: this node is {local_node:?}, the attach is for {node:?}, \
             or ublk_transport is off"
        ));
    }

    #[cfg(feature = "nvmeof")]
    let nsid = super::v1::ensure_nvme_namespace(&state, &key, Some(uuid)).await;
    #[cfg(not(feature = "nvmeof"))]
    let nsid = None;
    Json(super::v1::attach_info_for(&state, nsid)).into_response()
}

/// `GET /api/v1/volumes/{id}/attach` — how the volume is being served
/// right now, if it is.
async fn get_attach(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };
    if state.volume_manager.lock().await.get_volume(&VolumeId(uuid)).is_none() {
        return ApiError::not_found(format!("volume {uuid} not found"));
    }
    let key = uuid.to_string();
    if let Some(path) = state.ublk_exports.lock().await.device_path(&key) {
        return Json(serde_json::json!({ "id": uuid, "attached": true, "info": super::v1::AttachInfo::Ublk { device_hint: path } })).into_response();
    }
    let nsid = state.v1.lock().await.nvme_nsids.get(&key).copied();
    match nsid {
        Some(n) => Json(serde_json::json!({ "id": uuid, "attached": true, "info": super::v1::attach_info_for(&state, Some(n)) })).into_response(),
        None => Json(serde_json::json!({ "id": uuid, "attached": false })).into_response(),
    }
}

/// `DELETE /api/v1/volumes/{id}/attach` — stop serving it: the ublk device
/// goes, the NVMe namespace is withdrawn. Idempotent.
async fn detach_volume(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };
    let key = uuid.to_string();
    state.ublk_exports.lock().await.remove(&key);
    #[cfg(feature = "nvmeof")]
    super::v1::release_nvme_namespace(&state, &key).await;
    Json(serde_json::json!({ "id": uuid, "attached": false })).into_response()
}

/// `POST /api/v1/volumes/import {name, file|url, format?, redundancy?, size?, seal?}`
/// — a cloud image, a VM export (qcow2, vmdk, ova) or an ISO becomes a
/// sealed golden. Async: 202 with the job, poll `GET …/import/{id}`.
/// `POST /api/v1/volumes/{id}/cidata` — make this volume a cloud-init seed.
///
/// **The medium is the contract.** NoCloud looks for a filesystem *labelled*
/// `cidata` (or `CIDATA`) holding `meta-data`, `user-data` and optionally
/// `network-config`, and in practice it has to be **vfat or ISO 9660** — an
/// ext4 volume with the same label and the same files is not picked up, which
/// presents inside the guest as `Did not find any data source, searched
/// classes: ()` with a disk sitting right there. That cost a VM boot to find,
/// so the door that makes a seed makes the right kind of one.
///
/// vfat rather than ISO 9660 because this engine already writes FAT with real
/// long-name entries — `meta-data` does not fit 8.3 — and because a seed is
/// per VM and writable, which an ISO is not.
async fn write_cidata(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<CidataRequest>,
) -> Response {
    let Some(vol) = resolve_volume(&state, &id).await else {
        return ApiError::not_found(format!("no volume {id}"));
    };
    let files: Vec<(String, Vec<u8>)> = match req
        .files
        .into_iter()
        .map(|f| f.resolve().map(|s| (leaf(&s.path), s.contents)))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(f) => f,
        Err(e) => return ApiError::bad_request(e),
    };
    if files.is_empty() {
        return ApiError::bad_request("a seed with no files is not a seed");
    }
    let dev = match writable_volume(&state, vol.0).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    // Upper case: both spellings are accepted by cloud-init, and FAT stores a
    // label upper case anyway — writing it that way means what `blkid` reports
    // is what was asked for.
    let label = req.label.unwrap_or_else(|| "CIDATA".to_string()).to_uppercase();
    match crate::image::fat::format_from_files(dev, &files, &label).await {
        Ok(()) => {
            let mut vm = state.volume_manager.lock().await;
            let fs = crate::volume::FsInfo::vfat(&label);
            let _ = vm.set_fs_info(vol, Some(fs)).await;
            Json(serde_json::json!({
                "volume_id": vol.0,
                "label": label,
                "type": "vfat",
                "written": files.iter().map(|(n, _)| n).collect::<Vec<_>>(),
            }))
            .into_response()
        }
        Err(e) => ApiError::bad_request(format!("writing the seed: {e}")),
    }
}

/// The last component of a path — a seed is flat, and a caller that wrote
/// `/user-data` means the file at the root.
fn leaf(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

#[derive(Debug, serde::Deserialize)]
pub struct CidataRequest {
    pub files: Vec<super::fstemplates::SeedFileRequest>,
    /// `CIDATA` unless told otherwise.
    #[serde(default)]
    pub label: Option<String>,
}

/// A volume by id, by name, or by synonym — for the doors that take one in
/// a path.
///
/// The volume manager is asked first, so a synonym can never shadow a real
/// volume: a name means what it has always meant, and a synonym is only
/// consulted for names nothing else answers to.
async fn resolve_volume(state: &Arc<AppState>, key: &str) -> Option<VolumeId> {
    if let Some(id) = state.volume_manager.lock().await.find_volume(key).await {
        return Some(id);
    }
    super::synonyms::volume_for(state, key).await
}

async fn start_import(
    State(state): State<Arc<AppState>>,
    Json(spec): Json<crate::image::import::ImportSpec>,
) -> Response {
    let started = state.imports.write().await.start(state.clone(), spec);
    match started {
        Ok(status) => {
            let s = status.read().await.clone();
            (axum::http::StatusCode::ACCEPTED, Json(s)).into_response()
        }
        Err(e) => ApiError::bad_request(e),
    }
}

async fn list_imports(State(state): State<Arc<AppState>>) -> Response {
    let items = state.imports.read().await.all().await;
    let count = items.len();
    Json(serde_json::json!({ "items": items, "count": count })).into_response()
}

async fn get_import(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };
    match state.imports.read().await.status(&uuid).await {
        Some(s) => Json(s).into_response(),
        None => ApiError::not_found(format!("import {uuid} not found")),
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
    axum::extract::Query(q): axum::extract::Query<DeleteQuery>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "volumes", "method" => "delete").increment(1);
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };

    let vol_id = VolumeId(uuid);

    // A synonym pointing here is a reference held by something that knows
    // this volume only by name. Deleting under it leaves a name that
    // resolves to nothing, and the consumer finds out at its next start —
    // which is a failed boot, and the worst place to learn it. Re-point the
    // name, or say `force=true` and take the dangling synonym knowingly.
    if !q.force {
        let named: Vec<String> = state
            .synonyms
            .read()
            .await
            .pointing_at(&vol_id)
            .iter()
            .map(|s| crate::volume::synonym::key(&s.namespace, &s.name))
            .collect();
        if !named.is_empty() {
            return ApiError::conflict(format!(
                "cannot delete volume {uuid}: the synonym(s) {} point at it — re-point them                  first, or delete with force=true and leave them dangling",
                named.join(", ")
            ));
        }
    }

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
                access: d.access.clone(),
                writable: d.writable,
                role: d.role.clone(),
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
                access: d.access.clone(),
                writable: d.writable,
                role: d.role.clone(),
                fs: d.fs,
            };
            Json(resp).into_response()
        }
        Err(e) => ApiError::bad_request(format!("failed to resize volume: {e}")),
    }
}

/// `POST /api/v1/volumes/{id}/legs/clear` — try a volume's failed legs again.
///
/// A leg marked failed is sticky and persisted, so a marking made for a
/// reason that has since gone away leaves a perfectly readable volume
/// reporting no readable leg, across restarts. This clears the marking and
/// reads the volume to prove it; anything still broken marks itself again.
async fn clear_failed_legs(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "volumes", "method" => "legs_clear")
        .increment(1);
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };
    let mgr = state.volume_manager.lock().await;
    match mgr.clear_failed_legs(VolumeId(uuid)).await {
        Ok(r) => Json(serde_json::json!({
            "volume": uuid.to_string(),
            "cleared": r.cleared,
            "still_failed": r.still_failed,
            "healthy": r.still_failed.is_empty(),
        }))
        .into_response(),
        Err(e) => ApiError::not_found(format!("{e}")),
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

// ------------------------------------------------- composed pallets and disks

#[derive(Debug, Deserialize)]
pub struct ComposePalletMemberRequest {
    pub name: String,
    pub role: String,
    /// `kernel`, `initramfs`, `bootconfig`, `rootimage`, … Defaults to `raw`.
    #[serde(default)]
    pub kind: Option<String>,
    /// A volume, by id or name — shared in by its map, never copied.
    #[serde(default)]
    pub volume: Option<String>,
    /// Or inline text — a kernel command line — written.
    #[serde(default)]
    pub text: Option<String>,
    /// Bytes to digest, when the content ends before the volume does.
    #[serde(default)]
    pub len: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ComposePalletRequest {
    /// The volume's name.
    pub name: String,
    /// The pallet's name. Defaults to `name`.
    #[serde(default)]
    pub pallet: Option<String>,
    /// `boot`, `system`, `kernel`, `kube`, `app`, `runtime`, `data`.
    #[serde(default)]
    pub kind: Option<String>,
    /// Omit to take one past the highest sealed version of this pallet name.
    #[serde(default)]
    pub version: Option<u64>,
    #[serde(default)]
    pub version_label: Option<String>,
    /// LBA size of the disk this pallet will sit in. Defaults to 4096.
    #[serde(default)]
    pub lba: Option<u32>,
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub sealed: Option<bool>,
    #[serde(default)]
    pub read_only: Option<bool>,
    pub members: Vec<ComposePalletMemberRequest>,
}

#[derive(Debug, Serialize)]
pub struct ComposePalletResponse {
    #[serde(flatten)]
    pub volume: VolumeResponse,
    pub pallet: crate::volume::disk::PalletVolumeReport,
}

#[derive(Debug, Deserialize)]
pub struct ComposeDiskPartitionRequest {
    /// The volume that is the partition, by id or name.
    pub volume: String,
    /// GPT name. Defaults to the volume's.
    #[serde(default)]
    pub name: Option<String>,
    /// `esp`, `pallet`, `linux`, `swap`, `basic`, or a GUID. Defaults to
    /// what the volume is.
    #[serde(default)]
    pub r#type: Option<String>,
    /// Pallet selection order. Defaults to 1.
    #[serde(default)]
    pub priority: Option<u8>,
    #[serde(default)]
    pub tries: Option<u8>,
    #[serde(default)]
    pub attributes: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct ComposeDiskRequest {
    pub name: String,
    /// Total size. Omit for the chain plus the GPT.
    #[serde(default)]
    pub size: Option<String>,
    /// LBA size. Defaults to 4096 — what the engine presents a volume at.
    #[serde(default)]
    pub lba: Option<u32>,
    /// Give this disk its own GUID, at the cost of the two GPT slots.
    #[serde(default)]
    pub fresh_guid: Option<bool>,
    #[serde(default)]
    pub role: Option<String>,
    pub partitions: Vec<ComposeDiskPartitionRequest>,
}

#[derive(Debug, Serialize)]
pub struct ComposeDiskResponse {
    #[serde(flatten)]
    pub volume: VolumeResponse,
    pub disk: crate::volume::disk::DiskReport,
}

/// The ordinary volume response for `id`, or none if it went away.
async fn volume_response(vm: &crate::volume::VolumeManager, id: VolumeId) -> Option<VolumeResponse> {
    let handle = vm.get_volume_handle(&id)?;
    let name = handle.name().await;
    let allocated = handle.allocated().await;
    let vsize = handle.capacity_bytes();
    let d = describe(vm, &id).await;
    Some(VolumeResponse {
        id: id.0,
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
        access: d.access,
        writable: d.writable,
        role: d.role,
        fs: d.fs,
    })
}

fn parse_role(role: &Option<String>) -> Result<Option<crate::drive::slab::SlabRole>, Response> {
    match role.as_deref() {
        Some(r) => match crate::drive::slab::SlabRole::parse(r) {
            Some(role) => Ok(Some(role)),
            None => Err(ApiError::bad_request(format!("unknown role '{r}': system or data"))),
        },
        None => Ok(None),
    }
}

/// `POST /api/v1/volumes/compose/pallet` — a pallet as a sealed volume,
/// sharing its members' slots.
async fn compose_pallet(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ComposePalletRequest>,
) -> Response {
    use crate::volume::disk::{MemberSource, PalletMember, PalletVolumeSpec};
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "volumes", "method" => "compose_pallet").increment(1);

    if req.members.is_empty() {
        return ApiError::bad_request("a composed pallet needs at least one member");
    }
    let role = match parse_role(&req.role) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let size = match super::fstemplates::resolve_size(&req.size, None) {
        Ok(v) => v,
        Err(e) => return ApiError::bad_request(e),
    };

    let mut vm = state.volume_manager.lock().await;
    let mut members = Vec::with_capacity(req.members.len());
    for m in &req.members {
        let kind = m
            .kind
            .as_deref()
            .map(crate::pallet::format::parse_member_kind)
            .unwrap_or(crate::pallet::format::MemberKind::Raw);
        let source = match (&m.volume, &m.text) {
            (Some(v), None) => {
                let id = match vm.find_volume(v).await {
                    Some(id) => id,
                    None => return ApiError::not_found(format!("member '{}': no volume named {v}", m.name)),
                };
                let len = match super::fstemplates::resolve_size(&m.len, None) {
                    Ok(v) => v,
                    Err(e) => return ApiError::bad_request(format!("member '{}': {e}", m.name)),
                };
                MemberSource::Volume { id, len }
            }
            (None, Some(t)) => MemberSource::Bytes(t.clone().into_bytes()),
            _ => {
                return ApiError::bad_request(format!(
                    "member '{}': give exactly one of `volume` or `text`",
                    m.name
                ))
            }
        };
        members.push(PalletMember { name: m.name.clone(), role: m.role.clone(), kind, source });
    }

    let mut spec = PalletVolumeSpec::new(
        &req.name,
        req.kind
            .as_deref()
            .map(crate::pallet::format::parse_pallet_kind)
            .unwrap_or(crate::pallet::format::PalletKind::Unspecified),
    );
    spec.pallet = req.pallet.clone();
    spec.version = req.version;
    spec.version_label = req.version_label.clone().unwrap_or_default();
    spec.lba = req.lba.unwrap_or(0);
    spec.size = size;
    spec.role = role;
    spec.sealed = req.sealed.unwrap_or(true);
    spec.read_only = req.read_only.unwrap_or(true);
    spec.members = members;

    let report = match vm.compose_pallet(spec).await {
        Ok(r) => r,
        Err(e) => return ApiError::bad_request(format!("failed to compose pallet: {e}")),
    };
    match volume_response(&vm, VolumeId(report.id)).await {
        Some(volume) => {
            (axum::http::StatusCode::CREATED, Json(ComposePalletResponse { volume, pallet: report })).into_response()
        }
        None => ApiError::internal("composed pallet vanished"),
    }
}

/// `POST /api/v1/volumes/compose/disk` — a bootable disk as a chain of
/// goldens: a GPT whose partitions are the volumes named.
async fn compose_disk(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ComposeDiskRequest>,
) -> Response {
    use crate::volume::disk::{DiskPartition, DiskSpec};
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "volumes", "method" => "compose_disk").increment(1);

    if req.partitions.is_empty() {
        return ApiError::bad_request("a composed disk needs at least one partition");
    }
    let role = match parse_role(&req.role) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let size = match super::fstemplates::resolve_size(&req.size, None) {
        Ok(v) => v,
        Err(e) => return ApiError::bad_request(e),
    };

    let mut vm = state.volume_manager.lock().await;
    let mut partitions = Vec::with_capacity(req.partitions.len());
    for p in &req.partitions {
        let id = match vm.find_volume(&p.volume).await {
            Some(id) => id,
            None => return ApiError::not_found(format!("no volume named {}", p.volume)),
        };
        let type_guid = match p.r#type.as_deref() {
            Some(t) => match crate::image::type_guid::parse(t) {
                Some(g) => Some(g),
                None => return ApiError::bad_request(format!("unknown partition type '{t}'")),
            },
            None => None,
        };
        partitions.push(DiskPartition {
            volume: id,
            name: p.name.clone(),
            type_guid,
            priority: p.priority,
            tries: p.tries,
            attributes: p.attributes,
        });
    }

    let mut spec = DiskSpec::new(&req.name);
    spec.size = size;
    spec.lba = req.lba.unwrap_or(0);
    spec.fresh_guid = req.fresh_guid.unwrap_or(false);
    spec.role = role;
    spec.partitions = partitions;

    let report = match vm.compose_disk(spec).await {
        Ok(r) => r,
        Err(e) => return ApiError::bad_request(format!("failed to compose disk: {e}")),
    };
    match volume_response(&vm, VolumeId(report.id)).await {
        Some(volume) => (axum::http::StatusCode::CREATED, Json(ComposeDiskResponse { volume, disk: report })).into_response(),
        None => ApiError::internal("composed disk vanished"),
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
        .route("/{id}/tier", axum::routing::post(retier_volume))
        .route("/{id}/restripe", axum::routing::post(restripe_volume))
        .route("/{id}/seal", axum::routing::post(seal_volume).delete(unseal_volume))
        .route("/{id}/access", get(get_access).put(set_access))
        .route("/{id}/clone", axum::routing::post(clone_volume))
        .route("/{id}/lineage", get(volume_lineage))
        .route("/{id}/attach", get(get_attach).post(attach_volume).delete(detach_volume))
        .route("/import", get(list_imports).post(start_import))
        .route("/import/{id}", get(get_import))
        .route("/{id}/fsck", axum::routing::post(fsck_volume))
        .route("/{id}/legs/clear", axum::routing::post(clear_failed_legs))
        .route("/{id}/files", get(read_volume_file).post(write_volume_files))
        .route("/{id}/cidata", axum::routing::post(write_cidata))
        .route("/snapshots", axum::routing::post(create_snapshot))
        .route("/compose", axum::routing::post(compose_volume))
        .route("/compose/pallet", axum::routing::post(compose_pallet))
        .route("/compose/disk", axum::routing::post(compose_disk))
        .with_state(state)
}
