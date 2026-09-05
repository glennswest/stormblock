//! GET/POST/DELETE /api/v1/exports — volume-to-target export mappings.

use std::path::PathBuf;
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

/// Path of the persisted export table, when a data dir is configured.
fn exports_path(state: &AppState) -> Option<PathBuf> {
    state
        .config
        .management
        .data_dir
        .as_ref()
        .map(|d| PathBuf::from(d).join("exports.json"))
}

/// Write the current export table to disk. Best-effort: a persistence failure
/// must not fail the API call that triggered it.
///
/// An export is the address a consumer was given — a subsystem and a namespace
/// number that something out there has written down, and that firmware booting
/// over NVMe/TCP has baked into its configuration. Losing the table across a
/// restart does not merely forget an API call: it silently stops answering at
/// an address a machine is still dialling.
pub async fn persist_exports(state: &AppState) {
    let Some(path) = exports_path(state) else { return };

    let snapshot: Vec<ExportEntry> = state.exports.read().await.clone();
    let bytes = match serde_json::to_vec_pretty(&snapshot) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("failed to serialize export table: {e}");
            return;
        }
    };

    // Temp file and rename, so a crash mid-write cannot truncate the table
    // that is already there.
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, &bytes) {
        tracing::warn!("failed to write export table to {}: {e}", tmp.display());
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        tracing::warn!("failed to install export table at {}: {e}", path.display());
    }
}

/// Re-wire persisted exports into the running targets.
///
/// **The namespace id is restored, not reassigned.** Handing a volume the next
/// free nsid on restart would be a quiet renumbering, and everything that
/// attached by the old one — a boot command line, a saved configuration, a
/// machine's firmware — would come back pointing at a different volume or at
/// nothing. The number is part of the address, so it is part of the record.
///
/// Individual failures are logged and skipped: one volume that did not come
/// back must not stop the rest from being served.
pub async fn restore_exports(state: &Arc<AppState>) -> usize {
    let Some(path) = exports_path(state) else { return 0 };
    let Ok(bytes) = std::fs::read(&path) else { return 0 };

    let persisted: Vec<ExportEntry> = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("failed to parse {}: {e}", path.display());
            return 0;
        }
    };

    let mut restored = 0usize;
    let mut entries = Vec::with_capacity(persisted.len());

    for mut entry in persisted {
        match entry.protocol {
            #[cfg(feature = "nvmeof")]
            ExportProtocol::Nvmeof => {
                let Some(nsid) = entry.nsid else {
                    tracing::warn!(
                        "export {}: no namespace recorded, cannot restore", entry.id
                    );
                    continue;
                };
                let device = {
                    let vm = state.volume_manager.lock().await;
                    vm.get_volume(&VolumeId(entry.volume_id))
                };
                let Some(device) = device else {
                    tracing::warn!(
                        "export {}: volume {} is not attached, so nsid {nsid} stays unserved",
                        entry.id, entry.volume_id
                    );
                    entry.status = ExportStatus::PendingRestart;
                    entries.push(entry);
                    continue;
                };
                match state.nvmeof_target.read().await.as_ref() {
                    Some(target) => {
                        target.add_namespace_dynamic(nsid, device).await;
                        entry.status = ExportStatus::Active;
                        restored += 1;
                    }
                    None => {
                        tracing::warn!("NVMe-oF target not running; export stays pending");
                        entry.status = ExportStatus::PendingRestart;
                    }
                }
            }
            #[cfg(not(feature = "nvmeof"))]
            ExportProtocol::Nvmeof => {}

            // iSCSI exports ride on the LUN table, which restores itself.
            ExportProtocol::Iscsi => {
                entry.status = ExportStatus::Active;
                restored += 1;
            }
        }
        entries.push(entry);
    }

    {
        let mut exports = state.exports.write().await;
        *exports = entries;
        metrics::gauge!("stormblock_exports_total").set(exports.len() as f64);
    }

    if restored > 0 {
        tracing::info!("restored {restored} export(s) from {}", path.display());
    }
    restored
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
    persist_exports(&state).await;

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

    if !drop_export(&state, uuid).await {
        return ApiError::not_found(format!("export {uuid} not found"));
    }
    axum::http::StatusCode::NO_CONTENT.into_response()
}

/// Take an export out of the table and stop serving it.
///
/// Factored out because tearing an export down is not only something a
/// caller asks for: releasing the volume behind it has to do the same, and an
/// export that outlives its volume is a namespace answering for nothing.
///
/// Returns whether there was one to remove.
pub(crate) async fn drop_export(state: &Arc<AppState>, id: Uuid) -> bool {
    // Take the entry out first so we know what to tear down on the target.
    let removed = {
        let mut exports = state.exports.write().await;
        match exports.iter().position(|e| e.id == id) {
            Some(i) => {
                let e = exports.remove(i);
                metrics::gauge!("stormblock_exports_total").set(exports.len() as f64);
                Some(e)
            }
            None => None,
        }
    };

    let Some(entry) = removed else { return false };

    // An export that outlives its record would keep the volume pinned and its
    // LUN number allocated.
    #[cfg(feature = "iscsi")]
    if let Some(lun) = entry.lun_id {
        super::luns::detach_lun(state, lun).await;
    }
    #[cfg(feature = "nvmeof")]
    if let Some(nsid) = entry.nsid {
        if let Some(target) = state.nvmeof_target.read().await.as_ref() {
            target.remove_namespace(nsid).await;
        }
    }

    persist_exports(state).await;
    true
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list_exports).post(create_export))
        .route("/{id}", get(get_export).delete(delete_export))
        .with_state(state)
}
