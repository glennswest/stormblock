//! `/api/v1/forge` — this node as its site's forge (#272).
//!
//! `PUT` takes the `[nvmeof]` settings (`listen_addr`, `nqn`, and the #210
//! host policy), starts the shared NVMe/TCP target live and keeps them, so
//! every later start of the engine serves it again. `DELETE` turns it off and
//! keeps it off (a node is a forge by default, #287): no new connections, the
//! ones being served finish. `GET` reports `state` (on/off) and `from`
//! (default/persisted/config). Not for a target the command line or
//! `--config` set up: that is answered `409`.

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
        .route("/trust", get(trust_status).put(trust_set).delete(trust_forget))
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

/// `GET /api/v1/forge/trust` (#381): forge's CA and apiserver URL as nodes
/// are handed them, and whether a bootstrap token is set — never the token.
async fn trust_status(State(state): State<Arc<AppState>>) -> Response {
    Json(crate::mgmt::forge_trust::status(&state)).into_response()
}

/// `PUT /api/v1/forge/trust {ca, url?, bootstrap_token}` (#381): forge's
/// stormcert gives its engine the CA and the bootstrap token every booting
/// node is handed. Admin. Kept 0600; this node's own copy rewritten when it
/// is the forge.
async fn trust_set(
    State(state): State<Arc<AppState>>,
    Json(mut t): Json<crate::mgmt::forge_trust::ForgeTrust>,
) -> Response {
    use crate::mgmt::forge_trust;
    if let Err(e) = t.validate() {
        return ApiError::bad_request(e);
    }
    t.bootstrap_token = t.bootstrap_token.trim().to_string();
    t.updated_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    if let Err(e) = forge_trust::keep(&state, &t) {
        return ApiError::internal(format!("keeping forge trust: {e}"));
    }
    let own = forge_trust::write_self(&state).await;
    tracing::info!("forge trust set; this node's own copy {}", if own { "written" } else { "left" });
    Json(forge_trust::status(&state)).into_response()
}

async fn trust_forget(State(state): State<Arc<AppState>>) -> Response {
    use crate::mgmt::forge_trust;
    if let Err(e) = forge_trust::forget(&state) {
        return ApiError::internal(format!("forgetting forge trust: {e}"));
    }
    let dir = forge_trust::node_dir();
    if forge_trust::read_from(&dir).as_deref() == Some("self") {
        forge_trust::remove_dir(&dir);
    }
    Json(forge_trust::status(&state)).into_response()
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
