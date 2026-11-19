//! `/api/v1/stormdist` (#442): goldens exported for stormdist ISOs, the
//! forge's backup inventory, and restores from an ISO. See `mgmt::stormdist`.
//!
//! - `GET /goldens/{volume}`: a sealed golden's name, component, commit,
//!   size, device sha256 and sorted allocated ranges (a recipe `Input`
//!   without its `image`); its bytes are `content` (`GET` with `Range`).
//! - `GET /forge`: retention roots and signed release catalogues, as
//!   stormdist's `Forge` (a backup's `FORGE.toml`).
//! - `PUT /forge/roots {roots}`, `POST /forge/releases {release,
//!   signature_hex, key?}`: record them (admin: the auth class of every
//!   non-GET here).
//! - `POST /restores {iso, base?, key, mode?, flavour?, current?}`: 202 and
//!   a job; `GET /restores`, `GET /restores/{id}`.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::ApiError;
use crate::mgmt::stormdist::{self as sd, Catalogue, RestoreRequest};
use crate::mgmt::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/goldens/{volume}", get(export))
        .route("/forge", get(forge))
        .route("/forge/roots", put(set_roots))
        .route("/forge/releases", post(add_release))
        .route("/restores", get(list).post(restore))
        .route("/restores/{id}", get(get_job))
        .with_state(state)
}

async fn export(State(state): State<Arc<AppState>>, Path(volume): Path<String>) -> Response {
    let id = match super::volumes::volume_key(&state, &volume).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    match sd::export(&state, id).await {
        Ok(e) => Json(e).into_response(),
        Err((404, why)) => ApiError::not_found(why),
        Err((409, why)) => ApiError::conflict(why),
        Err((_, why)) => ApiError::internal(why),
    }
}

async fn forge(State(state): State<Arc<AppState>>) -> Response {
    match sd::load_inventory(&state) {
        Ok(inv) => {
            // A root that is no longer a volume here is said, not dropped.
            let vm = state.volume_manager.lock().await;
            let mut missing = Vec::new();
            for r in &inv.roots {
                if vm.find_volume(r).await.is_none() {
                    missing.push(r.clone());
                }
            }
            drop(vm);
            Json(json!({ "forge": inv.forge(), "missing_roots": missing })).into_response()
        }
        Err(e) => ApiError::internal(format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct RootsRequest {
    roots: BTreeSet<String>,
}

async fn set_roots(State(state): State<Arc<AppState>>, Json(req): Json<RootsRequest>) -> Response {
    for r in &req.roots {
        if stormdist::format::valid_name(r).is_err() {
            return ApiError::bad_request(format!("{r:?} is not a golden name stormdist takes"));
        }
    }
    match sd::set_roots(&state, req.roots) {
        Ok(inv) => Json(inv).into_response(),
        Err(e) => ApiError::internal(format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct ReleaseRequest {
    release: String,
    signature_hex: String,
    /// A pinned key to check the signature with; without it only its shape.
    #[serde(default)]
    key: Option<String>,
}

async fn add_release(State(state): State<Arc<AppState>>, Json(req): Json<ReleaseRequest>) -> Response {
    let key = match req.key.as_deref().map(sd::parse_key).transpose() {
        Ok(k) => k,
        Err(e) => return ApiError::bad_request(format!("{e:#}")),
    };
    let c = Catalogue { release: req.release, signature_hex: req.signature_hex };
    let rel = match sd::check_catalogue(&c, key.as_ref()) {
        Ok(r) => r,
        Err(e) => return ApiError::bad_request(format!("{e:#}")),
    };
    match sd::record_inventory(&state, [], [c]) {
        Ok(inv) => Json(json!({ "version": rel.version, "releases": inv.releases.len() })).into_response(),
        Err(e) => ApiError::internal(format!("{e:#}")),
    }
}

async fn restore(State(state): State<Arc<AppState>>, Json(req): Json<RestoreRequest>) -> Response {
    match sd::start(state, req) {
        Ok(job) => (StatusCode::ACCEPTED, Json(job)).into_response(),
        Err(e) => ApiError::bad_request(format!("{e:#}")),
    }
}

async fn list() -> Response {
    let jobs = sd::list_jobs();
    Json(json!({ "count": jobs.len(), "items": jobs })).into_response()
}

async fn get_job(Path(id): Path<String>) -> Response {
    match sd::job(&id) {
        Some(j) => Json(j).into_response(),
        None => ApiError::not_found(format!("no restore job {id}")),
    }
}
