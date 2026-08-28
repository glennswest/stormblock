//! `/api/v1/drives` — what this node has open, and opening or closing one.
//!
//! **Why open/close is engine-level.** Which drives a node carries is a
//! deployment's business, but *opening one* is mechanism, and every profile
//! needs it identically: `docs/layering.md` puts that here so a second
//! deployment does not fork it. The consumer that made this concrete is the
//! registry, which runs against a RouterOS node and an x86 one and must not
//! learn two APIs to give a pallet a home.
//!
//! It matters more than it looks: the pallet store is rebuilt from
//! `state.drives` on every request, so **a drive that is not open does not
//! exist** to publish onto. Before this route, adding one meant a restart —
//! which takes down every volume the node is serving to add a disk that has
//! nothing to do with them.
//!
//! **Closing refuses to strand data.** A drive carrying a slab is refused,
//! by identity rather than by convention: the slab registry is asked whether
//! any slab's device *is* this device. A profile's "drive 0 is the slab" is a
//! convention; this is a fact.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{ApiError, ListResponse};
use crate::mgmt::config::human_size;
use crate::mgmt::AppState;

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
    /// Failure-domain labels the drive was registered with (#70), as a
    /// chain: `shelf=S/bay=3`. Empty when nobody said where it is.
    pub labels: String,
}

async fn list_drives(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "drives", "method" => "list")
        .increment(1);
    let drives = state.drives.read().await;
    let items: Vec<DriveResponse> = drives
        .iter()
        .map(|d| {
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
                labels: d.labels.to_string(),
            }
        })
        .collect();
    let count = items.len();
    Json(ListResponse { items, count })
}

async fn get_drive(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "drives", "method" => "get")
        .increment(1);
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
                labels: d.labels.to_string(),
            };
            Json(resp).into_response()
        }
        None => ApiError::not_found(format!("drive {uuid} not found")),
    }
}

/// SMART health data response.
#[derive(Debug, Serialize)]
pub struct SmartResponse {
    pub uuid: Uuid,
    pub healthy: bool,
    pub temperature_celsius: Option<u16>,
    pub power_on_hours: Option<u64>,
    pub media_errors: u64,
    pub available_spare_pct: Option<u8>,
}

async fn get_drive_smart(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "drives", "method" => "smart")
        .increment(1);
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };

    let drives = state.drives.read().await;
    match drives.iter().find(|d| d.device.id().uuid == uuid) {
        Some(d) => {
            let smart = d.device.smart_status();
            match smart {
                Ok(data) => {
                    let resp = SmartResponse {
                        uuid,
                        healthy: data.healthy,
                        temperature_celsius: data.temperature_celsius,
                        power_on_hours: data.power_on_hours,
                        media_errors: data.media_errors,
                        available_spare_pct: data.available_spare_pct,
                    };
                    Json(resp).into_response()
                }
                Err(e) => ApiError::internal(format!("failed to read SMART data: {e}")),
            }
        }
        None => ApiError::not_found(format!("drive {uuid} not found")),
    }
}

// ------------------------------------------------------------ open / close

#[derive(Debug, Deserialize)]
pub struct OpenRequest {
    /// A device or a file. What it turns out to be is the drive layer's
    /// problem, which is the point: the same call works on a RouterOS node
    /// where a drive is a file and on an x86 one where it is an NVMe device.
    pub path: String,
    /// Create or extend a *file* to this many bytes before opening it —
    /// sparse, so it costs nothing until written. Omit for something that
    /// already exists, and for anything that is not a file.
    #[serde(default)]
    pub size_bytes: Option<u64>,
    /// Where the drive is: failure-domain labels (`shelf`, `bay`, `hba`,
    /// `rack`, …), as stormdrive resolves them from SES/sysfs (#70). Every
    /// slab on the drive is placed under them — a mirror's legs will not
    /// share a shelf when the policy says `@shelf`.
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
    /// A caller-derived stable identity (uuid5 of the WWID, say). Recorded
    /// as the `drive` rung of the labels, since the device's own uuid is
    /// minted per open (#65).
    #[serde(default)]
    pub uuid: Option<Uuid>,
}

async fn open_drive(State(state): State<Arc<AppState>>, Json(req): Json<OpenRequest>) -> Response {
    {
        // The same file twice would give the pallet allocator two views of one
        // free space, and it would hand out partitions that overlap — caught
        // by the GPT layer, but only after the first is on the disk.
        let drives = state.drives.read().await;
        if let Some(i) = drives.iter().position(|d| d.path == req.path) {
            return ApiError::conflict(format!("{} is already open as drive {i}", req.path));
        }
    }

    let dev: Arc<dyn crate::drive::BlockDevice> = match req.size_bytes {
        Some(bytes) => {
            match crate::drive::filedev::FileDevice::open_with_capacity(&req.path, bytes).await {
                Ok(d) => Arc::new(d),
                Err(e) => return ApiError::bad_request(format!("opening {}: {e}", req.path)),
            }
        }
        None => match crate::drive::open_one_drive(&req.path).await {
            Ok(d) => Arc::from(d),
            Err(e) => return ApiError::bad_request(format!("opening {}: {e}", req.path)),
        },
    };

    let mut labels = crate::placement::domain::FailureDomain::from_labels(req.labels.clone());
    if let Some(u) = req.uuid {
        labels = labels.with("drive", &u.to_string());
    }
    let index = {
        let mut drives = state.drives.write().await;
        drives.push(crate::mgmt::DriveInfo {
            device: dev.clone(),
            path: req.path.clone(),
            labels: labels.clone(),
        });
        drives.len() - 1
    };
    if !labels.is_empty() {
        state.slab_registry.write().await.label_device(&req.path, labels.clone());
    }
    let id = dev.id();
    tracing::info!(
        "drive {index} opened: {} ({}) — {} bytes, block_size={}, type={}",
        req.path,
        id.uuid,
        dev.capacity_bytes(),
        dev.block_size(),
        dev.device_type()
    );
    (
        axum::http::StatusCode::CREATED,
        Json(DriveResponse {
            uuid: id.uuid,
            path: req.path,
            model: id.model.clone(),
            serial: id.serial.clone(),
            device_type: dev.device_type().to_string(),
            capacity_bytes: dev.capacity_bytes(),
            capacity_human: human_size(dev.capacity_bytes()),
            block_size: dev.block_size(),
            labels: labels.to_string(),
        }),
    )
        .into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct CloseQuery {
    /// Close it even though it carries a slab. There is no honest use for
    /// this while volumes are live; it exists so a node with an already-dead
    /// disk can stop listing it.
    #[serde(default)]
    pub force: bool,
}

/// Which slab, if any, lives on this device. Compared by pointer, because two
/// `Arc`s to one device are the same device and two devices over one path are
/// not the same thing at all.
async fn slab_on(state: &AppState, dev: &Arc<dyn crate::drive::BlockDevice>) -> Option<String> {
    let registry = state.slab_registry.read().await;
    let hit = registry
        .iter()
        .find(|(_, slab)| Arc::ptr_eq(slab.device(), dev))
        .map(|(id, _)| id.to_string());
    hit
}

/// `DELETE /api/v1/drives/{id}` — stop serving a drive. `id` is its UUID or
/// its path.
///
/// This removes it from the node's list; it does not tear anything down that
/// is already built on it, which is why a slab-carrying drive is refused.
async fn close_drive(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<CloseQuery>,
) -> Response {
    let found = {
        let drives = state.drives.read().await;
        drives
            .iter()
            .position(|d| d.path == id || d.device.id().uuid.to_string() == id)
            .map(|i| (i, drives[i].device.clone(), drives[i].path.clone()))
    };
    let Some((index, dev, path)) = found else {
        return ApiError::not_found(format!("no open drive {id}"));
    };

    if let Some(slab) = slab_on(&state, &dev).await {
        if !q.force {
            return ApiError::conflict(format!(
                "{path} carries slab {slab}; every volume on it lives there. Closing this drive                  would not move them, so it is refused"
            ));
        }
        tracing::warn!("drive {index} ({path}) carries slab {slab} and is being closed anyway");
    }

    state.drives.write().await.remove(index);
    tracing::info!("drive {index} closed: {path}");
    Json(serde_json::json!({
        "closed": path,
        "was_index": index,
        // Callers hold indices, and the pallet API takes them. Say it rather
        // than let a stale one address someone else's disk.
        "note": "indices after this one shifted down; address drives by path or UUID where you can",
    }))
    .into_response()
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list_drives).post(open_drive))
        .route("/{id}", get(get_drive).delete(close_drive))
        .route("/{id}/smart", get(get_drive_smart))
        .route("/{id}/slabs", get(drive_slabs))
        .route("/{id}/labels", axum::routing::put(set_labels))
        .with_state(state)
}

/// `GET /api/v1/drives/{id}/slabs` — the slabs that live on this drive, by
/// identity of the device (#70 item 2). `id` is the drive's UUID or path.
async fn drive_slabs(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let found = {
        let drives = state.drives.read().await;
        drives
            .iter()
            .find(|d| d.path == id || d.device.id().uuid.to_string() == id)
            .map(|d| (d.device.clone(), d.path.clone(), d.labels.clone()))
    };
    let Some((dev, path, labels)) = found else {
        return ApiError::not_found(format!("no open drive {id}"));
    };
    let registry = state.slab_registry.read().await;
    let items: Vec<serde_json::Value> = registry
        .iter()
        .filter(|(_, slab)| Arc::ptr_eq(slab.device(), &dev) || slab.device().id().path == path)
        .map(|(sid, slab)| {
            serde_json::json!({
                "id": sid.0.to_string(),
                "tier": slab.tier().to_string(),
                "domain": registry.domain_of(sid).to_string(),
                "total_slots": slab.total_slots(),
                "free_slots": slab.free_slots(),
            })
        })
        .collect();
    let count = items.len();
    Json(serde_json::json!({
        "drive": dev.id().uuid,
        "path": path,
        "labels": labels.to_string(),
        "items": items,
        "count": count,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct LabelsRequest {
    pub labels: std::collections::BTreeMap<String, String>,
}

/// `PUT /api/v1/drives/{id}/labels` — (re)label a drive after the fact;
/// every slab on it moves under the new labels.
async fn set_labels(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<LabelsRequest>,
) -> Response {
    let labels = crate::placement::domain::FailureDomain::from_labels(req.labels);
    let path = {
        let mut drives = state.drives.write().await;
        let Some(d) = drives
            .iter_mut()
            .find(|d| d.path == id || d.device.id().uuid.to_string() == id)
        else {
            return ApiError::not_found(format!("no open drive {id}"));
        };
        d.labels = labels.clone();
        d.path.clone()
    };
    state.slab_registry.write().await.label_device(&path, labels.clone());
    Json(serde_json::json!({ "path": path, "labels": labels.to_string() })).into_response()
}
