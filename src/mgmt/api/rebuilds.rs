//! `/api/v1/rebuilds` — per-volume rebuilds after a drive fails (#146).
//!
//! A drive's health report starts them by itself (`[rebuild] automatic`);
//! this is where to watch them, start one by hand, stop one, and change
//! how much of the drives they may take.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    response::{IntoResponse, Response},
    routing::{get, put},
    Json, Router,
};
use serde::Deserialize;

use super::ApiError;
use crate::mgmt::AppState;
use crate::volume::{HealthState, VolumeId};

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list).post(start))
        .route("/settings", put(settings))
        .route("/{id}", get(one).delete(cancel))
        .with_state(state)
}

/// `GET /api/v1/rebuilds` — settings, what is queued and running, and the
/// jobs (newest first).
async fn list(State(state): State<Arc<AppState>>) -> Response {
    let rb = &state.rebuilds;
    let (queued, running) = rb.counts();
    let jobs = rb.jobs();
    Json(serde_json::json!({
        "settings": rb.settings(),
        "queued": queued,
        "running": running,
        "bytes_copied": rb.bytes_copied(),
        "items": jobs,
        "count": jobs.len(),
    }))
    .into_response()
}

async fn one(State(state): State<Arc<AppState>>, Path(id): Path<u64>) -> Response {
    match state.rebuilds.job(id) {
        Some(j) => Json(j).into_response(),
        None => ApiError::not_found(format!("no rebuild {id}")),
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct StartRequest {
    /// Volumes by id or name. Empty: every redundant volume that is not
    /// healthy.
    #[serde(default)]
    pub volumes: Vec<String>,
}

/// `POST /api/v1/rebuilds` — rebuild named volumes, or every degraded one.
async fn start(State(state): State<Arc<AppState>>, body: Option<Json<StartRequest>>) -> Response {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    let mut ids: Vec<VolumeId> = Vec::new();
    if req.volumes.is_empty() {
        let vm = state.volume_manager.lock().await;
        for (id, ..) in vm.list_volumes().await {
            if let Some(h) = vm.health(&id).await {
                if h.state != HealthState::Healthy {
                    ids.push(id);
                }
            }
        }
    } else {
        for key in &req.volumes {
            let found = state.volume_manager.lock().await.find_volume(key).await;
            match found {
                Some(id) => ids.push(id),
                None => return ApiError::not_found(format!("no volume {key}")),
            }
        }
    }
    let id = state.rebuilds.start("requested".into(), None, ids).await;
    let job = state.rebuilds.job(id);
    (axum::http::StatusCode::ACCEPTED, Json(job)).into_response()
}

/// `DELETE /api/v1/rebuilds/{id}` — stop it. What was rebuilt stays.
async fn cancel(State(state): State<Arc<AppState>>, Path(id): Path<u64>) -> Response {
    if !state.rebuilds.cancel(id) {
        return ApiError::not_found(format!("no rebuild {id}"));
    }
    Json(state.rebuilds.job(id)).into_response()
}

#[derive(Debug, Deserialize)]
pub struct SettingsRequest {
    pub automatic: Option<bool>,
    pub parallel: Option<usize>,
    pub extents_in_flight: Option<usize>,
    /// Bytes per second for every rebuild on the node together; 0 = no limit.
    pub max_bytes_per_sec: Option<u64>,
}

/// `PUT /api/v1/rebuilds/settings` — change them while rebuilds run.
/// Not persisted: `[rebuild]` in the config is what a restart starts from.
async fn settings(State(state): State<Arc<AppState>>, Json(req): Json<SettingsRequest>) -> Response {
    state.rebuilds.set(req.automatic, req.parallel, req.extents_in_flight, req.max_bytes_per_sec);
    Json(state.rebuilds.settings()).into_response()
}
