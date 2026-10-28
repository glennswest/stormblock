//! `/api/v1/system` — where this node's system lives (#350).
//!
//! `POST /api/v1/system/move {to, from?, force?}` (admin, destructive) moves
//! the system off a drive onto another, live, in the background: stormdrive's
//! `move-system`. `GET /api/v1/system/move` says how it is going (`idle`,
//! `moving`, `done` with what it did, `failed` with why). One at a time. The
//! drive moved off keeps a pointer to the new one: a boot from it follows the
//! pointer and asks for the BIOS boot order to be set.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use super::ApiError;
use crate::mgmt::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new().route("/move", get(status).post(start)).with_state(state)
}

#[derive(Debug, Deserialize)]
struct MoveRequest {
    /// The drive the system goes to (a device path).
    to: String,
    /// The drive it leaves: needed on a system pair, else the one drive.
    #[serde(default)]
    from: Option<String>,
    /// Lay `to` even when it carries a data slab (someone's data: destroyed).
    #[serde(default)]
    force: bool,
}

async fn status() -> Response {
    Json(crate::pair::move_status()).into_response()
}

async fn start(State(state): State<Arc<AppState>>, Json(req): Json<MoveRequest>) -> Response {
    if req.to.trim().is_empty() {
        return ApiError::bad_request("`to`: the drive the system goes to");
    }
    if !crate::pair::move_begin(req.from.as_deref(), &req.to) {
        return ApiError::conflict("a system move is under way: GET /api/v1/system/move");
    }
    #[cfg(target_os = "linux")]
    {
        let st = state.clone();
        tokio::spawn(crate::lockwatch::named("the system move", async move {
            let r = crate::cli::system_move(&st, req.from.as_deref(), &req.to, req.force).await;
            if let Err(e) = &r {
                println!("WARNING: system move onto {}: {e}", req.to);
                tracing::warn!("system move onto {}: {e}", req.to);
            }
            crate::pair::move_end(r.map_err(|e| e.to_string()));
        }));
        (StatusCode::ACCEPTED, Json(crate::pair::move_status())).into_response()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = state;
        crate::pair::move_end(Err("a system move needs Linux".into()));
        ApiError::internal("a system move needs Linux")
    }
}
