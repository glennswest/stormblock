//! `/api/v1/volumes/{id}/mirror` (#295): mirror an in-use volume to a peer
//! engine's namespace while it is used, then let go of it. The data path is
//! [`crate::volume::mirror`]; this is the API, and the record that keeps a
//! mirror from vanishing silently across a restart.
//!
//! `POST {"target": "nvme-tcp://T:port/<nqn>?nsid=N"}` connects as this
//! node's host NQN (`[fabric] host_nqn`, else `/etc/nvme/hostnqn`, else the
//! node's) unless the URI names one, checks the namespace is at least the
//! volume's size, and starts the mirror and its copy. `DELETE` lets every
//! write in flight land on both, flushes the target and drops it: the
//! switch-over, and the abort. `GET /api/v1/volumes/{id}` carries `mirror`.
//!
//! `<data_dir>/mirrors.json` names each mirror started and not stopped. One a
//! restart left behind is reported `failed` ("the engine restarted") until a
//! `DELETE` forgets it or a `POST` starts it again.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::drive::BlockDevice;
use crate::mgmt::api::ApiError;
use crate::mgmt::AppState;
use crate::volume::mirror::LiveMirror;
use crate::volume::thin::ThinVolumeHandle;
use crate::volume::VolumeId;

const FILE: &str = "mirrors.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    target: String,
    started_at: u64,
}

fn path(state: &AppState) -> Option<PathBuf> {
    state.config.management.data_dir.as_ref().map(|d| PathBuf::from(d).join(FILE))
}

/// The process-wide lock on the file's read-modify-write.
static FILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn load(state: &AppState) -> BTreeMap<String, Record> {
    path(state)
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn update(state: &AppState, f: impl FnOnce(&mut BTreeMap<String, Record>)) -> anyhow::Result<()> {
    let _g = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(p) = path(state) else { return Ok(()) };
    let mut all = load(state);
    f(&mut all);
    let body = serde_json::to_vec_pretty(&all)?;
    crate::serve::wiring::write_atomic(&p, &body)
}

async fn handle_of(state: &AppState, id: VolumeId) -> Option<Arc<ThinVolumeHandle>> {
    use crate::volume::catalog::VolumeView;
    if let Some(c) = state.volume_catalog.latest() {
        if let Some(h) = c.get_volume_handle(&id) {
            return Some(h);
        }
    }
    state.volume_manager.lock().await.get_volume_handle(&id)
}

/// What `GET /api/v1/volumes/{id}` says under `mirror`: the live one, or a
/// recorded one this process does not hold (failed: the engine restarted).
pub async fn status_of(state: &AppState, id: uuid::Uuid) -> Option<Value> {
    if let Some(m) = handle_of(state, VolumeId(id)).await.and_then(|h| h.live_mirror()) {
        return serde_json::to_value(m.status()).ok();
    }
    load(state).get(&id.to_string()).map(|r| {
        json!({
            "target": r.target,
            "state": "failed",
            "bytes_total": 0,
            "bytes_remaining": 0,
            "message": "the engine restarted; the mirror stopped with it: start it again",
            "started_at": r.started_at,
        })
    })
}

/// Whether a mirror holds this volume (a resize would leave the target
/// behind).
pub async fn is_mirrored(state: &AppState, id: uuid::Uuid) -> bool {
    handle_of(state, VolumeId(id)).await.is_some_and(|h| h.live_mirror().is_some())
}

#[derive(Debug, Deserialize)]
pub struct StartRequest {
    pub target: String,
}

/// `POST /api/v1/volumes/{id}/mirror`.
pub async fn start(state: Arc<AppState>, uuid: uuid::Uuid, req: StartRequest) -> Response {
    use crate::drive::nvmeof_dev::{NvmeTcpSpec, NvmeofDevice};
    let id = VolumeId(uuid);
    let (handle, secret) = {
        let vm = state.volume_manager.lock().await;
        (vm.get_volume_handle(&id), vm.is_secret(&id))
    };
    let Some(handle) = handle else {
        return ApiError::not_found(format!("volume {uuid} not found"));
    };
    // A secret volume's bytes never leave the engine (#74).
    if secret {
        return ApiError::conflict(format!("volume {uuid} is a secret volume: it is never copied off this engine"));
    }
    if let Some(m) = handle.live_mirror() {
        return ApiError::conflict(format!("volume {uuid} is already mirrored to {}; stop it first", m.target()));
    }
    let Some(mut spec) = NvmeTcpSpec::parse(req.target.trim()) else {
        return ApiError::bad_request("target: an nvme-tcp://host:port/<nqn>?nsid=N URI".to_string());
    };
    if spec.host_nqn.is_none() {
        spec.host_nqn = Some(crate::mgmt::peers::host_nqn(&state).await);
    }
    let dev = match NvmeofDevice::connect(&spec).await {
        Ok(d) => Arc::new(d) as Arc<dyn BlockDevice>,
        Err(e) => return ApiError::conflict(format!("connecting to {}: {e}", req.target)),
    };
    let need = handle.capacity_bytes();
    if dev.capacity_bytes() < need {
        return ApiError::conflict(format!(
            "the target is {} bytes; volume {uuid} is {need}: make it at least that size",
            dev.capacity_bytes()
        ));
    }
    let target = req.target.trim().to_string();
    let m = LiveMirror::new(target.clone(), dev, handle.extent_bytes());
    if let Err(e) = update(&state, |all| {
        all.insert(uuid.to_string(), Record { target: target.clone(), started_at: m.status().started_at });
    }) {
        return ApiError::internal(format!("recording the mirror: {e}"));
    }
    let mapped = match handle.start_mirror(m.clone()).await {
        Ok(x) => x,
        Err(e) => {
            let _ = update(&state, |all| {
                all.remove(&uuid.to_string());
            });
            return ApiError::conflict(format!("volume {uuid}: {e}"));
        }
    };
    tracing::info!("volume {uuid}: live mirror to {target} started, {} extent(s) to copy", mapped.len());
    let vol: Arc<dyn BlockDevice> = handle.clone();
    tokio::spawn(m.clone().copy(vol, mapped, need));
    (axum::http::StatusCode::ACCEPTED, Json(m.status())).into_response()
}

/// `DELETE /api/v1/volumes/{id}/mirror`: stop sending, after every write in
/// flight landed on both and the target is flushed. A recorded mirror this
/// process does not hold (a restart) is forgotten.
pub async fn stop(state: Arc<AppState>, uuid: uuid::Uuid) -> Response {
    let handle = handle_of(&state, VolumeId(uuid)).await;
    let stopped = match handle {
        Some(h) => h.stop_mirror().await,
        None => None,
    };
    let recorded = load(&state).contains_key(&uuid.to_string());
    if stopped.is_none() && !recorded {
        return ApiError::not_found(format!("volume {uuid} has no mirror"));
    }
    if let Err(e) = update(&state, |all| {
        all.remove(&uuid.to_string());
    }) {
        return ApiError::internal(format!("forgetting the mirror: {e}"));
    }
    match stopped {
        Some((m, r)) => {
            let st = m.status();
            tracing::info!("volume {uuid}: live mirror to {} stopped ({})", st.target, st.state);
            let mut body = serde_json::to_value(&st).unwrap_or_default();
            body["stopped"] = json!(true);
            body["flushed"] = json!(r.is_ok());
            Json(body).into_response()
        }
        None => Json(json!({ "stopped": true, "state": "failed", "message": "a mirror from before the engine restarted, forgotten" }))
            .into_response(),
    }
}

/// `POST/DELETE /api/v1/volumes/{id}/mirror`, by id or name.
pub async fn post(State(state): State<Arc<AppState>>, Path(id): Path<String>, Json(req): Json<StartRequest>) -> Response {
    match crate::mgmt::api::volumes::volume_key(&state, &id).await {
        Ok(v) => start(state, v.0, req).await,
        Err(r) => r,
    }
}

pub async fn delete(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match crate::mgmt::api::volumes::volume_key(&state, &id).await {
        Ok(v) => stop(state, v.0).await,
        Err(r) => r,
    }
}
