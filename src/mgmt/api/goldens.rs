//! `/api/v1/goldens` (#143): a named golden from a stopped VM's disk, and the
//! ticket forge pulls it with. See `mgmt::goldens`.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::ApiError;
use crate::mgmt::auth::Caller;
use crate::mgmt::goldens::{self, GoldenRecord, MakeRequest};
use crate::mgmt::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list).post(make))
        .route("/{name}", get(get_one))
        .route("/{name}/ticket", post(ticket))
        .route("/{name}/content", get(content))
        .with_state(state)
}

/// The URL this node is reached at, as the caller reached it.
fn base(state: &AppState, headers: &HeaderMap) -> String {
    let scheme = if state.config.management.tls_cert.is_some() { "https" } else { "http" };
    match headers.get(header::HOST).and_then(|h| h.to_str().ok()) {
        Some(h) => format!("{scheme}://{h}"),
        None => String::new(),
    }
}

/// The reply: the record, a ticket, and the body to hand forge's import.
fn answer(r: &GoldenRecord, base: &str, secs: Option<u64>, existing: bool) -> serde_json::Value {
    let (t, expires) = goldens::mint_ticket(&r.name, secs);
    let url = format!("{base}/api/v1/goldens/{}/content?ticket={t}", r.name);
    let mut provenance = r.provenance.clone();
    if let Some(s) = &r.source {
        provenance.entry("source_volume".into()).or_insert_with(|| s.name.clone());
    }
    json!({
        "golden": r,
        "existing": existing,
        "ticket": { "url": url, "expires_at": expires },
        // `POST /api/v1/volumes/import` on forge, as is.
        "import": {
            "name": r.name,
            "url": url,
            "format": "raw",
            "sha256": r.sha256,
            "parent": r.parent,
            "provenance": provenance,
        },
    })
}

/// `POST /api/v1/goldens {volume, name, provenance?, force?, ticket_secs?}`.
/// Destructive (#274): the admin token, or a Kubernetes bearer allowed
/// `create` on `storage.storm.io` `goldens`.
async fn make(
    State(state): State<Arc<AppState>>,
    caller: Option<Extension<Caller>>,
    headers: HeaderMap,
    Json(req): Json<MakeRequest>,
) -> Response {
    let source = match super::volumes::volume_key(&state, &req.volume).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let made_by = caller.map(|Extension(c)| c.0);
    let base = base(&state, &headers);
    // On a task of its own: a builder that gives up mid-digest leaves no
    // pending snapshot behind (#141's shape).
    let st = state.clone();
    let job = tokio::spawn(async move { goldens::make(&st, source, &req, made_by).await.map(|r| (r, req.ticket_secs)) });
    match job.await {
        Ok(Ok(((r, existing), secs))) => {
            let code = if existing { StatusCode::OK } else { StatusCode::CREATED };
            (code, Json(answer(&r, &base, secs, existing))).into_response()
        }
        Ok(Err(goldens::MakeError(code, why))) => match code {
            400 => ApiError::bad_request(why),
            404 => ApiError::not_found(why),
            409 => ApiError::conflict(why),
            _ => ApiError::internal(why),
        },
        Err(e) => ApiError::internal(format!("golden task: {e}")),
    }
}

async fn list(State(state): State<Arc<AppState>>) -> Response {
    let items = goldens::load(&state);
    Json(json!({ "count": items.len(), "items": items })).into_response()
}

async fn get_one(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    match goldens::find(&state, &name).await {
        Some(r) => Json(r).into_response(),
        None => ApiError::not_found(format!("no golden {name} here")),
    }
}

#[derive(Debug, Default, Deserialize)]
struct TicketRequest {
    #[serde(default)]
    ticket_secs: Option<u64>,
}

/// `POST /api/v1/goldens/{name}/ticket {ticket_secs?}`: a new ticket (the
/// last one expired, or the engine restarted). Destructive, like the make.
async fn ticket(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Option<Json<TicketRequest>>,
) -> Response {
    let Some(r) = goldens::find(&state, &name).await else {
        return ApiError::not_found(format!("no golden {name} here"));
    };
    let secs = body.and_then(|Json(b)| b.ticket_secs);
    Json(answer(&r, &base(&state, &headers), secs, true)).into_response()
}

#[derive(Debug, Deserialize)]
struct ContentQuery {
    ticket: Option<String>,
}

/// `GET /api/v1/goldens/{name}/content?ticket=…`: the golden's bytes, one
/// `Range` honoured. Open to the ticket alone (forge's import holds no
/// credential for this node); a sealed golden only.
async fn content(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<ContentQuery>,
    headers: HeaderMap,
) -> Response {
    let ok = q.ticket.as_deref().is_some_and(|t| goldens::ticket_reads(t, &name));
    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "no ticket, or not this golden's, or expired: ask for one with POST /api/v1/goldens/{name}/ticket" })),
        )
            .into_response();
    }
    let Some(r) = goldens::find(&state, &name).await else {
        return ApiError::not_found(format!("no golden {name} here"));
    };
    let id = crate::volume::VolumeId(r.volume_id);
    let (dev, sealed) = {
        let vm = state.volume_manager.lock().await;
        (vm.get_volume(&id), vm.is_sealed(&id))
    };
    let Some(dev) = dev else {
        return ApiError::not_found(format!("golden {name}'s volume is gone"));
    };
    if !sealed {
        return ApiError::conflict(format!("golden {name} is not sealed; its content could change under a reader"));
    }
    super::releases::serve_device(dev, &headers, name.clone(), format!("{name}.img"))
}
