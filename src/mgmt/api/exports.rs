//! GET/POST/DELETE /api/v1/exports — volume-to-target export mappings.

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
use crate::mgmt::{AppState, ExportEntry, ExportProtocol, ExportStatus};
use crate::volume::VolumeId;

#[derive(Debug, Serialize)]
pub struct ExportResponse {
    pub id: Uuid,
    pub volume_id: Uuid,
    pub protocol: String,
    pub target_id: String,
    pub status: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateExportRequest {
    pub volume_id: Uuid,
    pub protocol: ExportProtocol,
    pub target_id: Option<String>,
}

fn export_to_response(e: &ExportEntry) -> ExportResponse {
    ExportResponse {
        id: e.id,
        volume_id: e.volume_id,
        protocol: e.protocol.to_string(),
        target_id: e.target_id.clone(),
        status: match e.status {
            ExportStatus::Active => "active".to_string(),
            ExportStatus::PendingRestart => "pending_restart".to_string(),
        },
    }
}

async fn list_exports(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "exports", "method" => "list").increment(1);
    let exports = state.exports.read().await;
    let items: Vec<ExportResponse> = exports.iter().map(export_to_response).collect();
    let count = items.len();
    Json(ListResponse { items, count })
}

async fn get_export(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "exports", "method" => "get").increment(1);
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };

    let exports = state.exports.read().await;
    match exports.iter().find(|e| e.id == uuid) {
        Some(e) => Json(export_to_response(e)).into_response(),
        None => ApiError::not_found(format!("export {uuid} not found")),
    }
}

async fn create_export(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateExportRequest>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "exports", "method" => "create").increment(1);

    // Verify volume exists
    let vol_id = VolumeId(req.volume_id);
    {
        let vm = state.volume_manager.lock().await;
        if vm.get_volume(&vol_id).is_none() {
            return ApiError::not_found(format!("volume {} not found", req.volume_id));
        }
    }

    let target_id = req.target_id.unwrap_or_else(|| {
        match req.protocol {
            ExportProtocol::Iscsi => format!("iqn.2024.io.stormblock:{}", req.volume_id),
            ExportProtocol::Nvmeof => format!("nqn.2024.io.stormblock:{}", req.volume_id),
        }
    });

    let entry = ExportEntry {
        id: Uuid::new_v4(),
        volume_id: req.volume_id,
        protocol: req.protocol,
        target_id,
        status: ExportStatus::PendingRestart,
    };

    let resp = export_to_response(&entry);

    {
        let mut exports = state.exports.write().await;
        exports.push(entry);
        metrics::gauge!("stormblock_exports_total").set(exports.len() as f64);
    }

    (axum::http::StatusCode::CREATED, Json(resp)).into_response()
}

async fn delete_export(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "exports", "method" => "delete").increment(1);
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };

    let mut exports = state.exports.write().await;
    let before = exports.len();
    exports.retain(|e| e.id != uuid);
    if exports.len() < before {
        metrics::gauge!("stormblock_exports_total").set(exports.len() as f64);
        axum::http::StatusCode::NO_CONTENT.into_response()
    } else {
        ApiError::not_found(format!("export {uuid} not found"))
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list_exports).post(create_export))
        .route("/{id}", get(get_export).delete(delete_export))
        .with_state(state)
}
