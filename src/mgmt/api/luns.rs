//! GET/POST/DELETE /api/v1/luns — dynamic iSCSI LUN management.

use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, State},
    routing::get,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Serialize, Deserialize};

use super::{ApiError, ListResponse};
use crate::mgmt::{AppState, LunBacking, LunEntry};
use crate::mgmt::config::parse_size;
use crate::drive::{self, BlockDevice};

#[derive(Debug, Serialize)]
pub struct LunResponse {
    pub lun_id: u64,
    pub backing: LunBacking,
    pub readonly: bool,
    pub capacity_bytes: u64,
    pub block_size: u32,
    pub device_type: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateLunRequest {
    pub lun_id: u64,
    pub backing: LunBacking,
    #[serde(default)]
    pub readonly: bool,
}

fn lun_to_response(entry: &LunEntry) -> LunResponse {
    LunResponse {
        lun_id: entry.lun_id,
        backing: entry.backing.clone(),
        readonly: entry.readonly,
        capacity_bytes: entry.device.capacity_bytes(),
        block_size: entry.device.block_size(),
        device_type: entry.device.device_type().to_string(),
    }
}

async fn list_luns(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "luns", "method" => "list").increment(1);
    let entries = state.lun_entries.read().await;
    let items: Vec<LunResponse> = entries.iter().map(lun_to_response).collect();
    let count = items.len();
    Json(ListResponse { items, count })
}

async fn get_lun(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u64>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "luns", "method" => "get").increment(1);
    let entries = state.lun_entries.read().await;
    match entries.iter().find(|e| e.lun_id == id) {
        Some(e) => Json(lun_to_response(e)).into_response(),
        None => ApiError::not_found(format!("LUN {id} not found")),
    }
}

async fn create_lun(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateLunRequest>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "luns", "method" => "create").increment(1);

    // Check iSCSI target is available
    let iscsi = {
        let guard = state.iscsi_target.read().await;
        match guard.as_ref() {
            Some(t) => t.clone(),
            None => return ApiError::internal("iSCSI target not running"),
        }
    };

    // Check LUN ID not already in use
    {
        let entries = state.lun_entries.read().await;
        if entries.iter().any(|e| e.lun_id == req.lun_id) {
            return ApiError::conflict(format!("LUN {} already exists", req.lun_id));
        }
    }

    // Open the backing device
    let device: Arc<dyn BlockDevice> = match &req.backing {
        LunBacking::File { path, size } => {
            let capacity = match size {
                Some(s) => match parse_size(s) {
                    Ok(sz) => sz,
                    Err(e) => return ApiError::bad_request(format!("invalid size: {e}")),
                },
                None => 0, // open existing file at its current size
            };
            if capacity > 0 {
                match crate::drive::filedev::FileDevice::open_with_capacity(path, capacity).await {
                    Ok(dev) => Arc::new(dev),
                    Err(e) => return ApiError::internal(format!("failed to open file: {e}")),
                }
            } else {
                match crate::drive::filedev::FileDevice::open(path).await {
                    Ok(dev) => Arc::new(dev),
                    Err(e) => return ApiError::internal(format!("failed to open file: {e}")),
                }
            }
        }
        LunBacking::Device { path } => {
            match drive::open_one_drive(path).await {
                Ok(dev) => Arc::from(dev),
                Err(e) => return ApiError::internal(format!("failed to open device: {e}")),
            }
        }
        LunBacking::Raid { array_id } => {
            let arrays = state.arrays.read().await;
            match arrays.get(array_id) {
                Some(info) => info.array.clone() as Arc<dyn BlockDevice>,
                None => return ApiError::not_found(format!("array {} not found", array_id)),
            }
        }
    };

    // Add to iSCSI target
    iscsi.add_lun_dynamic(req.lun_id, device.clone(), req.readonly).await;

    let entry = LunEntry {
        lun_id: req.lun_id,
        backing: req.backing,
        readonly: req.readonly,
        device: device.clone(),
    };
    let resp = lun_to_response(&entry);

    {
        let mut entries = state.lun_entries.write().await;
        entries.push(entry);
        metrics::gauge!("stormblock_luns_total").set(entries.len() as f64);
    }

    tracing::info!("LUN {} created ({}, {}{})",
        req.lun_id,
        resp.device_type,
        crate::mgmt::config::human_size(resp.capacity_bytes),
        if req.readonly { ", readonly" } else { "" },
    );

    (axum::http::StatusCode::CREATED, Json(resp)).into_response()
}

async fn delete_lun(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u64>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "luns", "method" => "delete").increment(1);

    // Remove from iSCSI target
    let removed = {
        let guard = state.iscsi_target.read().await;
        if let Some(iscsi) = guard.as_ref() {
            iscsi.remove_lun(id).await
        } else {
            false
        }
    };

    // Remove from entries
    let mut entries = state.lun_entries.write().await;
    let before = entries.len();
    entries.retain(|e| e.lun_id != id);
    if entries.len() < before {
        metrics::gauge!("stormblock_luns_total").set(entries.len() as f64);
        tracing::info!("LUN {} removed", id);
        axum::http::StatusCode::NO_CONTENT.into_response()
    } else if removed {
        // Was in iSCSI target but not in entries (startup LUN)
        axum::http::StatusCode::NO_CONTENT.into_response()
    } else {
        ApiError::not_found(format!("LUN {id} not found"))
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list_luns).post(create_lun))
        .route("/{id}", get(get_lun).delete(delete_lun))
        .with_state(state)
}
