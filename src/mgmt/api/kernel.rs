//! `/api/v1/kernel/optional` (#383): the kernel's optional images (`gpu`,
//! `misc`) pulled from forge on day 2. See `mgmt::kernelopt`.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};

use super::ApiError;
use crate::mgmt::kernelopt;
use crate::mgmt::AppState;

/// `POST`: pull these images for the release's kernel. 202 with each one's
/// state (`pulling`, `cloning`), 200 when every one is `present`; an error
/// that made nothing for the request as a whole (no forge, no certificate,
/// forge unreachable).
async fn post_optional(State(state): State<Arc<AppState>>, Json(req): Json<kernelopt::Request>) -> Response {
    match kernelopt::start(&state, req).await {
        Ok(v) => {
            let done = v.iter().all(|s| s.state == "present");
            let failed = v.iter().all(|s| s.state == "failed");
            let code = if done {
                StatusCode::OK
            } else if failed {
                StatusCode::UNPROCESSABLE_ENTITY
            } else {
                StatusCode::ACCEPTED
            };
            (code, Json(serde_json::json!({ "images": v }))).into_response()
        }
        Err((code, msg)) => (
            StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(serde_json::json!({ "code": code, "error": msg })),
        )
            .into_response(),
    }
}

/// `GET`: every image asked for since the engine started, and what this
/// node holds (kept across restarts).
async fn get_optional(State(state): State<Arc<AppState>>) -> Response {
    let installed = state.data_dir.as_deref().map(kernelopt::installed).unwrap_or_default();
    Json(serde_json::json!({ "images": kernelopt::statuses(), "installed": installed })).into_response()
}

async fn get_one(Path(image): Path<String>) -> Response {
    match kernelopt::status(&image) {
        Some(s) => Json(s).into_response(),
        None => ApiError::not_found(format!("no optional image {image} asked for since the engine started")),
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/optional", get(get_optional).post(post_optional))
        .route("/optional/{image}", get(get_one))
        .with_state(state)
}
