//! `/api/v1/moves` — first-class volume move (#20).
//!
//! ```text
//! POST   /api/v1/moves               start a move (copy + verify; nothing destroyed)
//! GET    /api/v1/moves               list
//! GET    /api/v1/moves/{id}          one
//! POST   /api/v1/moves/{id}/commit   delete the source — after repointing the consumer
//! POST   /api/v1/moves/{id}/abort    delete the target, keep the source
//! ```
//!
//! Two calls, not one, and deliberately so. `POST /api/v1/moves` copies and
//! verifies but destroys nothing, so the source is still there when it returns.
//! Only the caller knows whether whatever was using the volume has been
//! repointed at the new one, so only the caller can say when the old one may
//! go — and until it does, the way back is to point at the source again.
//!
//! This is not `PATCH /volumes/{id}/resize` with a smaller number. That is
//! refused (#19), because shrinking a volume frees the extents past its new end
//! and xfs cannot shrink into that. A move builds a *new, smaller* filesystem
//! and copies the contents, which is the only form of "make this smaller" that
//! keeps the data.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::ApiError;
use crate::mgmt::config::parse_size;
use crate::mgmt::AppState;
use crate::volume::relocate::{self, MoveError, MoveSpec, MoveState};
use crate::volume::VolumeId;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list_moves).post(start_move))
        .route("/{id}", get(get_move))
        .route("/{id}/commit", post(commit_move))
        .route("/{id}/abort", post(abort_move))
        .with_state(state)
}

fn err(e: MoveError) -> Response {
    match e {
        MoveError::NotFound(m) => ApiError::not_found(m),
        MoveError::Conflict(m) => ApiError::conflict(m),
        MoveError::Invalid(m) => ApiError::bad_request(m),
        // A failed copy is the engine's problem, not a malformed request — and
        // the source is intact either way.
        MoveError::Failed(m) => ApiError::internal(m),
    }
}

#[derive(Debug, Deserialize)]
pub struct StartMoveRequest {
    /// Volume to move.
    pub volume_id: Uuid,
    /// Name for the new volume.
    pub target_name: String,
    /// Size of the new volume, as bytes or a human string ("24G").
    pub target_size: String,
    #[serde(default = "yes")]
    pub verify: bool,
}

fn yes() -> bool {
    true
}

async fn start_move(
    State(state): State<Arc<AppState>>,
    Json(req): Json<StartMoveRequest>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "moves", "method" => "start")
        .increment(1);

    let size = match parse_size(&req.target_size) {
        Ok(s) => s,
        Err(e) => {
            return ApiError::bad_request(format!("invalid size '{}': {e}", req.target_size))
        }
    };

    let busy = super::what_is_serving(&state, req.volume_id).await;
    if !busy.is_empty() {
        return ApiError::conflict(format!(
            "volume {} is still served by {} — a move copies a static filesystem, so detach it \
             and unmount it first. Anything written during the copy would not be in the target.",
            req.volume_id,
            busy.join(", ")
        ));
    }

    let spec = MoveSpec {
        source: VolumeId(req.volume_id),
        target_name: req.target_name,
        target_size_bytes: size,
        verify: req.verify,
    };

    // The copy is long and takes no lock of its own, so nothing else on this
    // node stalls behind it.
    match relocate::start(&state.volume_manager, &spec).await {
        Ok(mv) => {
            state.moves.write().await.insert(mv.id, mv.clone());
            persist(&state).await;
            (
                axum::http::StatusCode::CREATED,
                Json(json!({
                    "move": mv.json(),
                    "next": "repoint whatever used the source at the target, then POST \
                             /api/v1/moves/{id}/commit. The source is untouched until you do.",
                })),
            )
                .into_response()
        }
        Err(e) => err(e),
    }
}

async fn list_moves(State(state): State<Arc<AppState>>) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "moves", "method" => "list")
        .increment(1);
    let moves = state.moves.read().await;
    let items: Vec<_> = moves.values().map(|m| m.json()).collect();
    Json(json!({ "items": items, "count": items.len() })).into_response()
}

async fn get_move(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "moves", "method" => "get")
        .increment(1);
    let Ok(uuid) = id.parse::<Uuid>() else {
        return ApiError::bad_request(format!("invalid UUID: {id}"));
    };
    match state.moves.read().await.get(&uuid) {
        Some(m) => Json(m.json()).into_response(),
        None => ApiError::not_found(format!("move {id} not found")),
    }
}

async fn commit_move(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    finish(state, id, true).await
}

async fn abort_move(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    finish(state, id, false).await
}

async fn finish(state: Arc<AppState>, id: String, commit: bool) -> Response {
    metrics::counter!(
        "stormblock_api_requests_total",
        "endpoint" => "moves",
        "method" => if commit { "commit" } else { "abort" },
    )
    .increment(1);

    let Ok(uuid) = id.parse::<Uuid>() else {
        return ApiError::bad_request(format!("invalid UUID: {id}"));
    };
    let Some(mut mv) = state.moves.read().await.get(&uuid).cloned() else {
        return ApiError::not_found(format!("move {id} not found"));
    };

    // Committing deletes the source, so the same offline guard applies again —
    // and this time to the *target*, which the caller has just repointed
    // something at. Re-checked rather than trusted from the start call: the
    // caller has been off doing things in between, and that is the point.
    if commit && mv.state == MoveState::ReadyToCommit {
        let busy = super::what_is_serving(&state, mv.source).await;
        if !busy.is_empty() {
            return ApiError::conflict(format!(
                "the source volume {} is being served again by {} — repoint it at the target \
                 before committing, or the delete takes it out from under a live consumer",
                mv.source,
                busy.join(", ")
            ));
        }
    }

    let outcome = if commit {
        relocate::commit(&state.volume_manager, &mut mv).await
    } else {
        relocate::abort(&state.volume_manager, &mut mv).await
    };

    match outcome {
        Ok(()) => {
            state.moves.write().await.insert(mv.id, mv.clone());
            persist(&state).await;
            Json(mv.json()).into_response()
        }
        Err(e) => err(e),
    }
}

/// Write the move ledger out, so an interrupted move is still nameable after a
/// restart. Without it a crash between copy and commit leaves two volumes and
/// no record of which is which.
async fn persist(state: &AppState) {
    let Some(dir) = &state.data_dir else { return };
    let path = dir.join("moves.json");
    let moves = state.moves.read().await;
    let all: Vec<_> = moves.values().cloned().collect();
    drop(moves);
    let Ok(bytes) = serde_json::to_vec_pretty(&all) else { return };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, &path)).is_err() {
        tracing::warn!("failed to persist moves to {}", path.display());
    }
}

/// Read the move ledger back at startup.
pub fn load(data_dir: &std::path::Path) -> std::collections::HashMap<Uuid, relocate::VolumeMove> {
    let path = data_dir.join("moves.json");
    let Ok(raw) = std::fs::read_to_string(&path) else { return Default::default() };
    match serde_json::from_str::<Vec<relocate::VolumeMove>>(&raw) {
        Ok(list) => list.into_iter().map(|m| (m.id, m)).collect(),
        Err(e) => {
            tracing::error!("corrupt {} ({e}) — starting with no move history", path.display());
            Default::default()
        }
    }
}
