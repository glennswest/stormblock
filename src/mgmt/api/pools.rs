//! GET/POST/DELETE /api/v1/pools — DiskPool and VDrive management.

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
use crate::drive::pool::DiskPool;
use crate::mgmt::AppState;
use crate::mgmt::config::{human_size, parse_size};

#[derive(Debug, Serialize)]
pub struct PoolResponse {
    pub uuid: Uuid,
    pub device_path: String,
    pub total_capacity: u64,
    pub total_capacity_human: String,
    pub data_offset: u64,
    pub vdrive_count: u32,
    pub free_space: u64,
    pub free_space_human: String,
    pub largest_free: u64,
    pub largest_free_human: String,
}

#[derive(Debug, Serialize)]
pub struct VDriveResponse {
    pub uuid: Uuid,
    pub label: String,
    pub start_offset: u64,
    pub size: u64,
    pub size_human: String,
    pub state: String,
    pub array_uuid: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct FormatPoolRequest {
    pub device_path: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateVDriveRequest {
    pub label: String,
    pub size: String,
}

fn pool_to_response(pool: &DiskPool, device_path: &str) -> PoolResponse {
    let free = pool.free_space();
    let largest = pool.largest_free_region();
    PoolResponse {
        uuid: pool.pool_uuid(),
        device_path: device_path.to_string(),
        total_capacity: pool.total_capacity(),
        total_capacity_human: human_size(pool.total_capacity()),
        data_offset: pool.data_offset(),
        vdrive_count: pool.vdrive_count(),
        free_space: free,
        free_space_human: human_size(free),
        largest_free: largest,
        largest_free_human: human_size(largest),
    }
}

fn vdrive_entry_to_response(entry: &crate::drive::pool::VDriveEntry) -> VDriveResponse {
    VDriveResponse {
        uuid: entry.uuid,
        label: entry.label.clone(),
        start_offset: entry.start_offset,
        size: entry.size,
        size_human: human_size(entry.size),
        state: format!("{:?}", entry.state),
        array_uuid: if entry.array_uuid == Uuid::nil() { None } else { Some(entry.array_uuid) },
    }
}

async fn list_pools(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "pools", "method" => "list").increment(1);
    let pools = state.pools.read().await;
    let items: Vec<PoolResponse> = pools.iter()
        .map(|(_uuid, pool)| pool_to_response(pool, &pool.device_path()))
        .collect();
    let count = items.len();
    Json(ListResponse { items, count })
}

async fn get_pool(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "pools", "method" => "get").increment(1);
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };

    let pools = state.pools.read().await;
    match pools.get(&uuid) {
        Some(pool) => Json(pool_to_response(pool, &pool.device_path())).into_response(),
        None => ApiError::not_found(format!("pool {uuid} not found")),
    }
}

async fn format_pool(
    State(state): State<Arc<AppState>>,
    Json(req): Json<FormatPoolRequest>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "pools", "method" => "format").increment(1);

    // Open the device
    let device = match crate::drive::filedev::FileDevice::open(&req.device_path).await {
        Ok(d) => Arc::new(d) as Arc<dyn crate::drive::BlockDevice>,
        Err(e) => return ApiError::bad_request(format!("cannot open device '{}': {e}", req.device_path)),
    };

    match DiskPool::format(device, &req.device_path).await {
        Ok(pool) => {
            let uuid = pool.pool_uuid();
            let resp = pool_to_response(&pool, &req.device_path);
            state.pools.write().await.insert(uuid, pool);
            (axum::http::StatusCode::CREATED, Json(resp)).into_response()
        }
        Err(e) => ApiError::internal(format!("failed to format pool: {e}")),
    }
}

async fn delete_pool(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "pools", "method" => "delete").increment(1);
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };

    let mut pools = state.pools.write().await;
    if let Some(pool) = pools.get(&uuid) {
        if pool.vdrive_count() > 0 {
            return ApiError::conflict("cannot delete pool with existing VDrives — delete them first");
        }
    }
    match pools.remove(&uuid) {
        Some(_) => axum::http::StatusCode::NO_CONTENT.into_response(),
        None => ApiError::not_found(format!("pool {uuid} not found")),
    }
}

async fn list_vdrives(
    State(state): State<Arc<AppState>>,
    Path(pool_id): Path<String>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "vdrives", "method" => "list").increment(1);
    let uuid = match pool_id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {pool_id}")),
    };

    let pools = state.pools.read().await;
    match pools.get(&uuid) {
        Some(pool) => {
            let entries = pool.list_vdrives();
            let items: Vec<VDriveResponse> = entries.iter()
                .map(|e| vdrive_entry_to_response(e))
                .collect();
            let count = items.len();
            Json(ListResponse { items, count }).into_response()
        }
        None => ApiError::not_found(format!("pool {uuid} not found")),
    }
}

async fn create_vdrive(
    State(state): State<Arc<AppState>>,
    Path(pool_id): Path<String>,
    Json(req): Json<CreateVDriveRequest>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "vdrives", "method" => "create").increment(1);
    let uuid = match pool_id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {pool_id}")),
    };

    let size = match parse_size(&req.size) {
        Ok(s) => s,
        Err(e) => return ApiError::bad_request(format!("invalid size '{}': {e}", req.size)),
    };

    let mut pools = state.pools.write().await;
    match pools.get_mut(&uuid) {
        Some(pool) => {
            match pool.create_vdrive(size, &req.label).await {
                Ok(entry) => {
                    let resp = vdrive_entry_to_response(&entry);
                    (axum::http::StatusCode::CREATED, Json(resp)).into_response()
                }
                Err(e) => ApiError::bad_request(format!("failed to create VDrive: {e}")),
            }
        }
        None => ApiError::not_found(format!("pool {uuid} not found")),
    }
}

async fn delete_vdrive(
    State(state): State<Arc<AppState>>,
    Path((pool_id, vdrive_id)): Path<(String, String)>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "vdrives", "method" => "delete").increment(1);
    let pool_uuid = match pool_id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid pool UUID: {pool_id}")),
    };
    let vdrive_uuid = match vdrive_id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid VDrive UUID: {vdrive_id}")),
    };

    let mut pools = state.pools.write().await;
    match pools.get_mut(&pool_uuid) {
        Some(pool) => {
            match pool.delete_vdrive(vdrive_uuid).await {
                Ok(()) => axum::http::StatusCode::NO_CONTENT.into_response(),
                Err(e) => ApiError::conflict(format!("cannot delete VDrive: {e}")),
            }
        }
        None => ApiError::not_found(format!("pool {pool_uuid} not found")),
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list_pools).post(format_pool))
        .route("/{id}", get(get_pool).delete(delete_pool))
        .route("/{pool_id}/vdrives", get(list_vdrives).post(create_vdrive))
        .route("/{pool_id}/vdrives/{vdrive_id}", axum::routing::delete(delete_vdrive))
        .with_state(state)
}
