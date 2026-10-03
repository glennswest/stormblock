//! `/api/v1/forge` — this node as its site's forge (#272).
//!
//! `PUT` takes the `[nvmeof]` settings (`listen_addr`, `nqn`, and the #210
//! host policy), starts the shared NVMe/TCP target live and keeps them, so
//! every later start of the engine serves it again. `DELETE` turns it off:
//! no new connections, the ones being served finish. `GET` reports what runs
//! and who set it up. Not for a target the command line or `--config` set up:
//! that is answered `409`.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};

use super::ApiError;
use crate::mgmt::config::NvmeofExportConfig;
use crate::mgmt::forge::{self, ForgeError};
use crate::mgmt::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(status).put(enable).delete(disable))
        .with_state(state)
}

fn error(e: ForgeError) -> Response {
    match e {
        ForgeError::Invalid(m) => ApiError::bad_request(m),
        ForgeError::Configured => ApiError::conflict(e.to_string()),
        ForgeError::Io(_) => ApiError::internal(e.to_string()),
    }
}

async fn status(State(state): State<Arc<AppState>>) -> Response {
    Json(forge::status(&state).await).into_response()
}

async fn enable(State(state): State<Arc<AppState>>, Json(settings): Json<NvmeofExportConfig>) -> Response {
    match forge::start(&state, settings, true).await {
        Ok(()) => Json(forge::status(&state).await).into_response(),
        Err(e) => error(e),
    }
}

async fn disable(State(state): State<Arc<AppState>>) -> Response {
    match forge::stop(&state).await {
        Ok(live) => {
            let mut body = forge::status(&state).await;
            body["connections_finishing"] = live.into();
            Json(body).into_response()
        }
        Err(e) => error(e),
    }
}
