//! The scrub's routes (#160).
//!
//! - `GET /api/v1/scrub`: settings, the scrub running, the queue, and every
//!   volume's last pass in short.
//! - `DELETE /api/v1/scrub`: stop the scrub running (its findings so far are
//!   kept; the schedule owes the volume a full pass).
//! - `GET /api/v1/volumes/{id}/scrub`: a volume's last pass in full.
//! - `POST /api/v1/volumes/{id}/scrub`: scrub it next, changing nothing a
//!   majority does not repair.
//! - `POST /api/v1/volumes/{id}/scrub/repair-parity`: the same, and parity
//!   that does not match its data is rewritten from it. Destructive (#274).
//! - `POST /api/v1/volumes/{id}/scrub/repair {extent, from}`: write the
//!   copy on slab `from` over the extent's other legs, an operator's choice
//!   where no majority can say which copy is right. Destructive.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::ApiError;
use crate::mgmt::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new().route("/", get(status).delete(cancel)).with_state(state)
}

async fn status(State(state): State<Arc<AppState>>) -> Response {
    let s = &state.scrubber;
    let present: std::collections::HashSet<_> = state.volume_manager.lock().await.volume_ids().into_iter().collect();
    let mut volumes: Vec<_> = s
        .all()
        .into_iter()
        .filter(|(id, _)| present.contains(id))
        .map(|(id, v)| {
            json!({
                "volume": id.0,
                "started": v.started,
                "finished": v.finished,
                "clean": v.report.clean(),
                "mirror_mismatches": v.report.mirror_mismatches,
                "mirror_repaired": v.report.mirror_repaired,
                "mirror_reported": v.report.mirror_reported,
                "parity_mismatches": v.report.parity_mismatches,
                "parity_rewritten": v.report.parity_rewritten,
                "unreadable": v.report.unreadable,
            })
        })
        .collect();
    volumes.sort_by_key(|v| v["volume"].as_str().unwrap_or_default().to_string());
    Json(json!({
        "settings": state.config.scrub,
        "running": s.running(),
        "queued": s.queued().into_iter().map(|v| v.0).collect::<Vec<_>>(),
        "volumes": volumes,
    }))
    .into_response()
}

async fn cancel(State(state): State<Arc<AppState>>) -> Response {
    if state.scrubber.cancel() {
        Json(json!({"cancelled": true})).into_response()
    } else {
        ApiError::not_found("no scrub is running")
    }
}

/// `GET /api/v1/volumes/{id}/scrub`.
pub async fn volume_scrub(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let vol = match super::volumes::volume_key(&state, &id).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let running = state.scrubber.running().filter(|r| r.volume == vol);
    let queued = state.scrubber.queued().contains(&vol);
    Json(json!({
        "volume": vol.0,
        "running": running,
        "queued": queued,
        "last": state.scrubber.last(&vol),
    }))
    .into_response()
}

/// `POST /api/v1/volumes/{id}/scrub`.
pub async fn start_volume_scrub(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    ask(&state, &id, false).await
}

/// `POST /api/v1/volumes/{id}/scrub/repair-parity`.
pub async fn start_volume_scrub_repairing_parity(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    ask(&state, &id, true).await
}

async fn ask(state: &Arc<AppState>, id: &str, repair_parity: bool) -> Response {
    let vol = match super::volumes::volume_key(state, id).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let redundant = state
        .volume_manager
        .lock()
        .await
        .get_volume_handle(&vol)
        .map(|h| !h.redundancy().is_none());
    match redundant {
        None => return ApiError::not_found(format!("volume {} not found", vol.0)),
        Some(false) => {
            return ApiError::conflict(format!(
                "volume {} has one copy: a scrub compares copies, and there is nothing to compare",
                vol.0
            ))
        }
        Some(true) => {}
    }
    state.scrubber.ask(vol, repair_parity);
    (StatusCode::ACCEPTED, Json(json!({"volume": vol.0, "queued": true, "repair_parity": repair_parity}))).into_response()
}

#[derive(Debug, Deserialize)]
pub struct RepairRequest {
    pub extent: u64,
    /// The slab whose copy is written over the others.
    pub from: uuid::Uuid,
}

/// `POST /api/v1/volumes/{id}/scrub/repair`.
pub async fn repair(State(state): State<Arc<AppState>>, Path(id): Path<String>, Json(req): Json<RepairRequest>) -> Response {
    let vol = match super::volumes::volume_key(&state, &id).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(h) = state.volume_manager.lock().await.get_volume_handle(&vol) else {
        return ApiError::not_found(format!("volume {} not found", vol.0));
    };
    match h.repair_extent_from(req.extent, crate::drive::slab::SlabId(req.from)).await {
        Ok(wrote) => Json(json!({"volume": vol.0, "extent": req.extent, "from": req.from, "legs_written": wrote})).into_response(),
        Err(e) => ApiError::conflict(e),
    }
}
