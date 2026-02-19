//! GET /api/v1/drives — drive enumeration.

use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, State},
    routing::get,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use uuid::Uuid;

use super::{ApiError, ListResponse};
use crate::mgmt::AppState;
use crate::mgmt::config::human_size;

#[derive(Debug, Serialize)]
pub struct DriveResponse {
    pub uuid: Uuid,
    pub path: String,
    pub model: String,
    pub serial: String,
    pub device_type: String,
    pub capacity_bytes: u64,
    pub capacity_human: String,
    pub block_size: u32,
}

async fn list_drives(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "drives", "method" => "list").increment(1);
    let drives = state.drives.read().await;
    let items: Vec<DriveResponse> = drives.iter().map(|d| {
        let id = d.device.id();
        DriveResponse {
            uuid: id.uuid,
            path: d.path.clone(),
            model: id.model.clone(),
            serial: id.serial.clone(),
            device_type: d.device.device_type().to_string(),
            capacity_bytes: d.device.capacity_bytes(),
            capacity_human: human_size(d.device.capacity_bytes()),
            block_size: d.device.block_size(),
        }
    }).collect();
    let count = items.len();
    Json(ListResponse { items, count })
}

async fn get_drive(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "drives", "method" => "get").increment(1);
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };

    let drives = state.drives.read().await;
    match drives.iter().find(|d| d.device.id().uuid == uuid) {
        Some(d) => {
            let id = d.device.id();
            let resp = DriveResponse {
                uuid: id.uuid,
                path: d.path.clone(),
                model: id.model.clone(),
                serial: id.serial.clone(),
                device_type: d.device.device_type().to_string(),
                capacity_bytes: d.device.capacity_bytes(),
                capacity_human: human_size(d.device.capacity_bytes()),
                block_size: d.device.block_size(),
            };
            Json(resp).into_response()
        }
        None => ApiError::not_found(format!("drive {uuid} not found")),
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list_drives))
        .route("/{id}", get(get_drive))
        .with_state(state)
}
