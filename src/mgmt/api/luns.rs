//! GET/POST/DELETE /api/v1/luns — dynamic iSCSI LUN management.
//!
//! LUNs are keyed by LUN ID in a map so lookup stays O(1) with thousands of
//! them exported (#24), and the LUN table is persisted to
//! `<data_dir>/luns.json` so exports survive a restart (#22).

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

use super::{ApiError, ListResponse};
use crate::mgmt::{AppState, LunBacking, LunEntry, PersistedLun};
use crate::mgmt::config::parse_size;
use crate::drive::{self, BlockDevice};
use crate::volume::VolumeId;

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
    /// LUN number. Omit to have the next free LUN assigned.
    pub lun_id: Option<u64>,
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

/// Path of the persisted LUN table, when a data dir is configured.
fn luns_path(state: &AppState) -> Option<PathBuf> {
    state
        .config
        .management
        .data_dir
        .as_ref()
        .map(|d| PathBuf::from(d).join("luns.json"))
}

/// Write the current LUN table to disk. Best-effort: a persistence failure
/// must not fail the API call that triggered it.
pub async fn persist_luns(state: &AppState) {
    let Some(path) = luns_path(state) else { return };

    let snapshot: Vec<PersistedLun> = {
        let entries = state.lun_entries.read().await;
        let mut v: Vec<PersistedLun> = entries
            .values()
            .map(|e| PersistedLun {
                lun_id: e.lun_id,
                backing: e.backing.clone(),
                readonly: e.readonly,
            })
            .collect();
        v.sort_by_key(|e| e.lun_id);
        v
    };

    let bytes = match serde_json::to_vec_pretty(&snapshot) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("failed to serialize LUN table: {e}");
            return;
        }
    };

    // Write to a temp file and rename so a crash mid-write cannot truncate
    // the existing table.
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, &bytes) {
        tracing::warn!("failed to write LUN table to {}: {e}", tmp.display());
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        tracing::warn!("failed to install LUN table at {}: {e}", path.display());
    }
}

/// Re-open persisted LUNs and wire them into the running iSCSI target.
///
/// Returns the number of LUNs restored. Individual failures are logged and
/// skipped — one unreachable backing file must not block the rest.
pub async fn restore_luns(state: &Arc<AppState>) -> usize {
    let Some(path) = luns_path(state) else { return 0 };
    let Ok(bytes) = std::fs::read(&path) else { return 0 };

    let persisted: Vec<PersistedLun> = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("failed to parse {}: {e}", path.display());
            return 0;
        }
    };

    let iscsi = state.iscsi_target.read().await.as_ref().cloned();
    let mut restored = 0usize;

    for p in persisted {
        let device = match open_backing(state, &p.backing).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("LUN {}: cannot restore backing: {e}", p.lun_id);
                continue;
            }
        };

        if let Some(ref t) = iscsi {
            t.add_lun_dynamic(p.lun_id, device.clone(), p.readonly).await;
        }
        state.lun_entries.write().await.insert(
            p.lun_id,
            LunEntry {
                lun_id: p.lun_id,
                backing: p.backing,
                readonly: p.readonly,
                device,
            },
        );
        restored += 1;
    }

    if restored > 0 {
        metrics::gauge!("stormblock_luns_total").set(restored as f64);
        tracing::info!("restored {restored} LUN(s) from {}", path.display());
    }
    restored
}

/// Open the block device behind a LUN backing description.
async fn open_backing(
    state: &AppState,
    backing: &LunBacking,
) -> Result<Arc<dyn BlockDevice>, String> {
    match backing {
        LunBacking::File { path, size } => {
            let capacity = match size {
                Some(s) => parse_size(s).map_err(|e| format!("invalid size: {e}"))?,
                None => 0, // open existing file at its current size
            };
            let dev = if capacity > 0 {
                crate::drive::filedev::FileDevice::open_with_capacity(path, capacity).await
            } else {
                crate::drive::filedev::FileDevice::open(path).await
            };
            dev.map(|d| Arc::new(d) as Arc<dyn BlockDevice>)
                .map_err(|e| format!("failed to open file: {e}"))
        }
        LunBacking::Device { path } => drive::open_one_drive(path)
            .await
            .map(Arc::from)
            .map_err(|e| format!("failed to open device: {e}")),
        LunBacking::Raid { array_id } => {
            let arrays = state.arrays.read().await;
            arrays
                .get(array_id)
                .map(|info| info.array.clone() as Arc<dyn BlockDevice>)
                .ok_or_else(|| format!("array {array_id} not found"))
        }
        LunBacking::Volume { volume_id } => {
            let vm = state.volume_manager.lock().await;
            vm.get_volume(&VolumeId(*volume_id))
                .ok_or_else(|| format!("volume {volume_id} not found"))
        }
    }
}

async fn list_luns(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "luns", "method" => "list").increment(1);
    let entries = state.lun_entries.read().await;
    let mut items: Vec<LunResponse> = entries.values().map(lun_to_response).collect();
    items.sort_by_key(|l| l.lun_id);
    let count = items.len();
    Json(ListResponse { items, count })
}

async fn get_lun(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u64>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "luns", "method" => "get").increment(1);
    let entries = state.lun_entries.read().await;
    match entries.get(&id) {
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

    // Resolve the LUN number: explicit, or the next free one.
    let lun_id = {
        let entries = state.lun_entries.read().await;
        match req.lun_id {
            Some(id) => {
                if entries.contains_key(&id) {
                    return ApiError::conflict(format!("LUN {id} already exists"));
                }
                id
            }
            None => (0u64..).find(|id| !entries.contains_key(id)).unwrap_or(0),
        }
    };

    let device = match open_backing(&state, &req.backing).await {
        Ok(d) => d,
        Err(e) => {
            // A missing volume/array is the caller's mistake, not ours.
            return if e.contains("not found") {
                ApiError::not_found(e)
            } else {
                ApiError::internal(e)
            };
        }
    };

    // Add to iSCSI target
    iscsi.add_lun_dynamic(lun_id, device.clone(), req.readonly).await;

    let entry = LunEntry {
        lun_id,
        backing: req.backing,
        readonly: req.readonly,
        device: device.clone(),
    };
    let resp = lun_to_response(&entry);

    {
        let mut entries = state.lun_entries.write().await;
        entries.insert(lun_id, entry);
        metrics::gauge!("stormblock_luns_total").set(entries.len() as f64);
    }
    persist_luns(&state).await;

    tracing::info!("LUN {} created ({}, {}{})",
        lun_id,
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
    let existed = {
        let mut entries = state.lun_entries.write().await;
        let existed = entries.remove(&id).is_some();
        if existed {
            metrics::gauge!("stormblock_luns_total").set(entries.len() as f64);
        }
        existed
    };

    if existed {
        persist_luns(&state).await;
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
