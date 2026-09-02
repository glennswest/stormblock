//! `/api/v1/synonyms` — a stable name that points at a volume.
//!
//! ```text
//! GET    /api/v1/synonyms                 list (?namespace=)
//! POST   /api/v1/synonyms                 create {namespace?, name, target|volume|uri, label?}
//! GET    /api/v1/synonyms/{ns}/{name}     resolve — the target, and whether it changed
//! PUT    /api/v1/synonyms/{ns}/{name}     re-point at a new version
//! POST   /api/v1/synonyms/{ns}/{name}/rollback   put it back to the previous target
//! DELETE /api/v1/synonyms/{ns}/{name}     drop the name (never the volume)
//! ```
//!
//! Both path shapes work: `/api/v1/synonyms/fedora-43` is
//! `/api/v1/synonyms/default/fedora-43`.
//!
//! **The point of the version.** A name is stable, so something else has to
//! carry "this changed". Every re-point bumps a monotonic `version`, and a
//! client that remembers what it resolved can ask in one call whether that is
//! still the answer:
//!
//! ```text
//! GET /api/v1/synonyms/images/nginx?since=7   → 200 {changed: true, …} or 304
//! GET /api/v1/synonyms/images/nginx
//!     If-None-Match: "7"                      → 304 Not Modified
//! ```
//!
//! The ETag is the version, so an ordinary HTTP client gets the same answer
//! without knowing anything about synonyms. A body is returned with
//! `changed: false` — rather than only a 304 — for `since`, because a caller
//! polling a name usually wants the current target in the same round trip
//! when it *has* moved and a plain answer when it has not.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;

use super::ApiError;
use crate::mgmt::AppState;
use crate::volume::synonym::{self, Synonym, SynonymError, Target, DEFAULT_NAMESPACE};
use crate::volume::VolumeId;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list).post(create))
        .route("/{name}", get(resolve_one).put(repoint).delete(remove))
        .route("/{namespace}/{name}", get(resolve_two).put(repoint_two).delete(remove_two))
        .route("/{name}/rollback", post(rollback_one))
        .route("/{namespace}/{name}/rollback", post(rollback_two))
        .with_state(state)
}

#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    /// Only this namespace.
    #[serde(default)]
    pub namespace: Option<String>,
    /// Only synonyms pointing at this volume — the reverse question, which
    /// is what "may I delete this volume" needs.
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ResolveQuery {
    /// The version the caller last saw. `changed: false` (and a 304 when the
    /// caller asked for one) if the name still means that.
    #[serde(default)]
    pub since: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct CreateRequest {
    #[serde(default)]
    pub namespace: Option<String>,
    pub name: String,
    /// A volume id or an existing volume's name.
    #[serde(default)]
    pub volume: Option<String>,
    /// …or storage another node serves: `nvme-tcp://host:port/<nqn>?nsid=N`.
    #[serde(default)]
    pub uri: Option<String>,
    /// What version the content is, for whoever cares. Never interpreted.
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RepointRequest {
    #[serde(default)]
    pub volume: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

/// The synonym as it goes on the wire, plus what is known about the target
/// right now — a caller resolving a name almost always then asks the volume
/// what it is, and the round trip is free here.
async fn body(state: &AppState, s: &Synonym, changed: Option<bool>) -> serde_json::Value {
    let mut v = json!({
        "namespace": s.namespace,
        "name": s.name,
        "key": synonym::key(&s.namespace, &s.name),
        "version": s.version,
        "target": s.target,
        "label": s.label,
        "description": s.description,
        "created_at": s.created_at,
        "updated_at": s.updated_at,
        "history": s.history,
    });
    if let Some(c) = changed {
        v["changed"] = json!(c);
    }
    if let Some(id) = s.target.volume_id() {
        let vm = state.volume_manager.lock().await;
        match vm.get_volume_handle(&id) {
            Some(h) => {
                v["volume"] = json!({
                    "id": id.0,
                    "name": h.name().await,
                    "size_bytes": h.capacity_bytes(),
                    "sealed": h.is_sealed(),
                    "access": h.access().to_string(),
                    "writable": h.writable(),
                    "role": h.placement_role().to_string(),
                });
            }
            // A name whose volume is gone. Said plainly rather than by
            // omission: a dangling synonym is a failed boot, and the caller
            // that finds it is the one that can fix it.
            None => v["dangling"] = json!(true),
        }
    }
    v
}

fn err(e: SynonymError) -> Response {
    match e {
        SynonymError::NotFound(_) => ApiError::not_found(e.to_string()),
        SynonymError::Exists(_) => ApiError::conflict(e.to_string()),
        SynonymError::InvalidName(_) | SynonymError::NoHistory(_) => {
            ApiError::bad_request(e.to_string())
        }
    }
}

/// A target from `volume` (id or name) or `uri`, exactly one of them.
async fn target_of(
    state: &AppState,
    volume: Option<&str>,
    uri: Option<&str>,
) -> Result<Target, Response> {
    match (volume, uri) {
        (Some(_), Some(_)) => Err(ApiError::bad_request("give volume or uri, not both")),
        (None, None) => Err(ApiError::bad_request("give volume (id or name) or uri")),
        (None, Some(u)) => {
            if u.trim().is_empty() {
                return Err(ApiError::bad_request("uri must not be empty"));
            }
            Ok(Target::Remote { uri: u.to_string() })
        }
        (Some(v), None) => {
            let id = state
                .volume_manager
                .lock()
                .await
                .find_volume(v)
                .await
                .ok_or_else(|| ApiError::not_found(format!("no volume {v}")))?;
            Ok(Target::Volume { id })
        }
    }
}

async fn list(State(state): State<Arc<AppState>>, Query(q): Query<ListQuery>) -> Response {
    let store = state.synonyms.read().await;
    let target = match &q.target {
        Some(t) => match state.volume_manager.lock().await.find_volume(t).await {
            Some(id) => Some(id),
            None => return ApiError::not_found(format!("no volume {t}")),
        },
        None => None,
    };
    let picked: Vec<Synonym> = match target {
        Some(id) => store.pointing_at(&id).into_iter().cloned().collect(),
        None => store.list(q.namespace.as_deref()).into_iter().cloned().collect(),
    };
    drop(store);
    let mut items = Vec::with_capacity(picked.len());
    for s in &picked {
        items.push(body(&state, s, None).await);
    }
    let count = items.len();
    Json(json!({ "items": items, "count": count })).into_response()
}

async fn create(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateRequest>,
) -> Response {
    let target = match target_of(&state, req.volume.as_deref(), req.uri.as_deref()).await {
        Ok(t) => t,
        Err(r) => return r,
    };
    let ns = req.namespace.as_deref().unwrap_or(DEFAULT_NAMESPACE).to_string();
    let made = {
        let mut store = state.synonyms.write().await;
        match store.create(&ns, &req.name, target, req.label.clone(), req.description.clone()) {
            Ok(s) => s.clone(),
            Err(e) => return err(e),
        }
    };
    (StatusCode::CREATED, Json(body(&state, &made, None).await)).into_response()
}

async fn resolve(
    state: Arc<AppState>,
    namespace: &str,
    name: &str,
    since: Option<u64>,
    headers: &HeaderMap,
) -> Response {
    let found = state.synonyms.read().await.get(namespace, name).cloned();
    let Some(s) = found else {
        return ApiError::not_found(format!("no synonym {}", synonym::key(namespace, name)));
    };
    // An `If-None-Match` of the version the caller holds is the HTTP-native
    // spelling of the same question `since` asks.
    let known = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim_matches('"').to_string())
        .and_then(|v| v.parse::<u64>().ok());
    if known == Some(s.version) {
        return (StatusCode::NOT_MODIFIED, etag(s.version)).into_response();
    }
    let changed = since.map(|v| v != s.version);
    (etag(s.version), Json(body(&state, &s, changed).await)).into_response()
}

fn etag(version: u64) -> HeaderMap {
    let mut h = HeaderMap::new();
    if let Ok(v) = axum::http::HeaderValue::from_str(&format!("\"{version}\"")) {
        h.insert(axum::http::header::ETAG, v);
    }
    h
}

async fn resolve_one(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<ResolveQuery>,
    headers: HeaderMap,
) -> Response {
    resolve(state, DEFAULT_NAMESPACE, &name, q.since, &headers).await
}

async fn resolve_two(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    Query(q): Query<ResolveQuery>,
    headers: HeaderMap,
) -> Response {
    resolve(state, &namespace, &name, q.since, &headers).await
}

async fn do_repoint(
    state: Arc<AppState>,
    namespace: &str,
    name: &str,
    req: RepointRequest,
) -> Response {
    let target = match target_of(&state, req.volume.as_deref(), req.uri.as_deref()).await {
        Ok(t) => t,
        Err(r) => return r,
    };
    let moved = {
        let mut store = state.synonyms.write().await;
        match store.repoint(namespace, name, target, req.label.clone()) {
            Ok(s) => s.clone(),
            Err(e) => return err(e),
        }
    };
    (etag(moved.version), Json(body(&state, &moved, Some(true)).await)).into_response()
}

async fn repoint(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(req): Json<RepointRequest>,
) -> Response {
    do_repoint(state, DEFAULT_NAMESPACE, &name, req).await
}

async fn repoint_two(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
    Json(req): Json<RepointRequest>,
) -> Response {
    do_repoint(state, &namespace, &name, req).await
}

async fn do_rollback(state: Arc<AppState>, namespace: &str, name: &str) -> Response {
    let back = {
        let mut store = state.synonyms.write().await;
        match store.rollback(namespace, name) {
            Ok(s) => s.clone(),
            Err(e) => return err(e),
        }
    };
    (etag(back.version), Json(body(&state, &back, Some(true)).await)).into_response()
}

async fn rollback_one(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    do_rollback(state, DEFAULT_NAMESPACE, &name).await
}

async fn rollback_two(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
) -> Response {
    do_rollback(state, &namespace, &name).await
}

async fn do_remove(state: Arc<AppState>, namespace: &str, name: &str) -> Response {
    let mut store = state.synonyms.write().await;
    match store.remove(namespace, name) {
        // Dropping a name never touches the volume it named: the whole point
        // of keeping the binding out of the volume.
        Ok(gone) => Json(json!({
            "namespace": gone.namespace,
            "name": gone.name,
            "removed": true,
            "was": gone.target,
        }))
        .into_response(),
        Err(e) => err(e),
    }
}

async fn remove(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    do_remove(state, DEFAULT_NAMESPACE, &name).await
}

async fn remove_two(
    State(state): State<Arc<AppState>>,
    Path((namespace, name)): Path<(String, String)>,
) -> Response {
    do_remove(state, &namespace, &name).await
}

/// Resolve a synonym to a volume id, for the surfaces that take "an id or a
/// name". `None` when nothing of that name is a synonym, or when it names
/// storage on another node — which is a real answer, not a failure, and the
/// caller has to be able to tell it apart from "no such name".
pub async fn volume_for(state: &AppState, key: &str) -> Option<VolumeId> {
    state.synonyms.read().await.find(key).and_then(|s| s.target.volume_id())
}
