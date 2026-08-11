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
    let items: Vec<VolumeResponse> = vols.iter().map(|(id, name, vsize, allocated)| {
        VolumeResponse {
            id: id.0,
            name: name.clone(),
            virtual_size_bytes: *vsize,
            virtual_size_human: human_size(*vsize),
            allocated_bytes: *allocated,
            allocated_human: human_size(*allocated),
            array_id: None,
            fs_uuid: None,
        }
    }).collect();
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
            let resp = VolumeResponse {
                id: uuid,
                name,
                virtual_size_bytes: vsize,
                virtual_size_human: human_size(vsize),
                allocated_bytes: allocated,
                allocated_human: human_size(allocated),
                array_id: None,
                fs_uuid: None,
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

    // Cloning a template is a snapshot plus a fresh filesystem UUID — no
    // mkfs, no attach. Placement comes from the template's own extents.
    if let Some(key) = req.from_template.as_deref() {
        let (vol_id, fs_uuid, size_bytes) =
            match super::fstemplates::clone_for_volume_api(&state, key, &req.name, size).await {
                Ok(v) => v,
                Err(resp) => return resp,
            };
        let vm = state.volume_manager.lock().await;
        let allocated = match vm.get_volume_handle(&vol_id) {
            Some(h) => h.allocated().await,
            None => 0,
        };
        let resp = VolumeResponse {
            id: vol_id.0,
            name: req.name,
            virtual_size_bytes: size_bytes,
            virtual_size_human: human_size(size_bytes),
            allocated_bytes: allocated,
            allocated_human: human_size(allocated),
            array_id: None,
            fs_uuid,
        };
        metrics::gauge!("stormblock_volumes_total").set(vm.list_volumes().await.len() as f64);
        return (axum::http::StatusCode::CREATED, Json(resp)).into_response();
    }

    let size = match size {
        Some(s) => s,
        None => return ApiError::bad_request("size is required"),
    };
    let array_id = match req.array_id {
        Some(a) => RaidArrayId(a),
        None => return ApiError::bad_request("array_id is required (or use from_template)"),
    };

    // Verify array exists
    {
        let arrays = state.arrays.read().await;
        if !arrays.contains_key(&array_id) {
            return ApiError::not_found(format!("array {} not found", array_id.0));
        }
    }

    let mut vm = state.volume_manager.lock().await;
    match vm.create_volume(&req.name, size, array_id).await {
        Ok(vol_id) => {
            let resp = VolumeResponse {
                id: vol_id.0,
                name: req.name,
                virtual_size_bytes: size,
                virtual_size_human: human_size(size),
                allocated_bytes: 0,
                allocated_human: human_size(0),
                array_id: Some(array_id.0),
                fs_uuid: None,
            };
            metrics::gauge!("stormblock_volumes_total").set(vm.list_volumes().await.len() as f64);
            (axum::http::StatusCode::CREATED, Json(resp)).into_response()
        }
        Err(e) => ApiError::bad_request(format!("failed to create volume: {e}")),
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

    // Check if volume is exported
    {
        let exports = state.exports.read().await;
        if exports.iter().any(|e| e.volume_id == uuid) {
            return ApiError::conflict("cannot delete volume with active exports".to_string());
        }
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

    let mut vm = state.volume_manager.lock().await;
    match vm.create_snapshot(source_id, &req.name).await {
        Ok(snap_id) => {
            let handle = vm.get_volume_handle(&snap_id).unwrap();
            let allocated = handle.allocated().await;
            let vsize = handle.capacity_bytes();
            let resp = VolumeResponse {
                id: snap_id.0,
                name: req.name,
                virtual_size_bytes: vsize,
                virtual_size_human: human_size(vsize),
                allocated_bytes: allocated,
                allocated_human: human_size(allocated),
                array_id: None,
                fs_uuid: None,
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
        Ok(()) => {
            let handle = match vm.get_volume_handle(&vol_id) {
                Some(h) => h,
                None => return ApiError::not_found(format!("volume {uuid} not found")),
            };
            let name = handle.name().await;
            let allocated = handle.allocated().await;
            let vsize = handle.capacity_bytes();
            let resp = VolumeResponse {
                id: uuid,
                name,
                virtual_size_bytes: vsize,
                virtual_size_human: human_size(vsize),
                allocated_bytes: allocated,
                allocated_human: human_size(allocated),
                array_id: None,
                fs_uuid: None,
            };
            Json(resp).into_response()
        }
        Err(e) => ApiError::bad_request(format!("failed to resize volume: {e}")),
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list_volumes).post(create_volume))
        .route("/{id}", get(get_volume).delete(delete_volume))
        .route("/{id}/resize", axum::routing::patch(resize_volume))
        .route("/snapshots", axum::routing::post(create_snapshot))
        .with_state(state)
}
