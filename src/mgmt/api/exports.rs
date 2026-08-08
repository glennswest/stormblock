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
    /// LUN assigned on the iSCSI target — an initiator needs it to address
    /// this volume among the others on the same target (#24).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lun_id: Option<u64>,
    /// Namespace ID assigned on the NVMe-oF target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nsid: Option<u32>,
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
        lun_id: e.lun_id,
        nsid: e.nsid,
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

    // Wire the export into the running target. Both protocols can now do this
    // without a restart — iSCSI via add_lun_dynamic, NVMe-oF via
    // add_namespace_dynamic (#26) — so an export goes straight to active.
    let mut lun_id = None;
    let mut nsid = None;
    let mut status = ExportStatus::PendingRestart;

    match req.protocol {
        #[cfg(feature = "iscsi")]
        ExportProtocol::Iscsi => {
            let backing = crate::mgmt::LunBacking::Volume { volume_id: req.volume_id };
            match super::luns::attach_lun(&state, backing, None, false).await {
                Ok(id) => {
                    lun_id = Some(id);
                    status = ExportStatus::Active;
                }
                Err(e) => {
                    tracing::warn!("export {}: cannot attach LUN: {e}", req.volume_id);
                    return ApiError::internal(format!("failed to attach LUN: {e}"));
                }
            }
        }
        #[cfg(not(feature = "iscsi"))]
        ExportProtocol::Iscsi => return ApiError::internal("iSCSI support not compiled in"),

        #[cfg(feature = "nvmeof")]
        ExportProtocol::Nvmeof => {
            let device = {
                let vm = state.volume_manager.lock().await;
                vm.get_volume(&vol_id)
            };
            let Some(device) = device else {
                return ApiError::not_found(format!("volume {} not found", req.volume_id));
            };
            match state.nvmeof_target.read().await.as_ref() {
                Some(target) => {
                    // NSID 0 is reserved; namespaces start at 1.
                    let used = target.list_namespaces().await;
                    let id = (1u32..).find(|n| !used.contains(n)).unwrap_or(1);
                    target.add_namespace_dynamic(id, device).await;
                    nsid = Some(id);
                    status = ExportStatus::Active;
                }
                None => {
                    tracing::warn!("NVMe-oF target not running; export stays pending");
                }
            }
        }
        #[cfg(not(feature = "nvmeof"))]
        ExportProtocol::Nvmeof => return ApiError::internal("NVMe-oF support not compiled in"),
    }

    let entry = ExportEntry {
        id: Uuid::new_v4(),
        volume_id: req.volume_id,
        protocol: req.protocol,
        target_id,
        status,
        lun_id,
        nsid,
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

    // Take the entry out first so we know what to tear down on the target.
    let removed = {
        let mut exports = state.exports.write().await;
        let idx = exports.iter().position(|e| e.id == uuid);
        match idx {
            Some(i) => {
                let e = exports.remove(i);
                metrics::gauge!("stormblock_exports_total").set(exports.len() as f64);
                Some(e)
            }
            None => None,
        }
    };

    let Some(entry) = removed else {
        return ApiError::not_found(format!("export {uuid} not found"));
    };

    // Stop serving it — an export that outlives its record would keep the
    // volume pinned and its LUN number allocated.
    #[cfg(feature = "iscsi")]
    if let Some(lun) = entry.lun_id {
        super::luns::detach_lun(&state, lun).await;
    }
    #[cfg(feature = "nvmeof")]
    if let Some(nsid) = entry.nsid {
        if let Some(target) = state.nvmeof_target.read().await.as_ref() {
            target.remove_namespace(nsid).await;
        }
    }

    axum::http::StatusCode::NO_CONTENT.into_response()
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list_exports).post(create_export))
        .route("/{id}", get(get_export).delete(delete_export))
        .with_state(state)
}
