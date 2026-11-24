//! `/api/v1/blobs` and `/api/v1/blobstore` (#732): the engine's
//! content-addressed blob store, for the registry's OCI blobs
//! (stormblock-registry#241). `docs/blobstore.md` is the reference.
//!
//! - `PUT /api/v1/blobs/sha256:<hex>`: the body, hashed as it lands; `201`
//!   made, `200` held already (nothing written), `400` a mismatch (nothing
//!   kept), `507` the pool full (nothing kept).
//! - `GET|HEAD /api/v1/blobs/sha256:<hex>`: the bytes, `Range` honoured
//!   (`206`, `416`); HEAD is the stat (`Content-Length`).
//! - `DELETE /api/v1/blobs/sha256:<hex>`: `200` either way, `deleted` says
//!   whether it was held; a read in flight finishes.
//! - `GET /api/v1/blobs?after=<digest>&limit=<n>`: digests and sizes in
//!   order, `next` when there are more.
//! - upload sessions: `POST /api/v1/blobs/uploads` opens one;
//!   `GET …/uploads/{id}` its length; `PATCH …/uploads/{id}` appends at
//!   `?offset=` or `Content-Range` (the length, or `416`); `PUT
//!   …/uploads/{id}?digest=sha256:<hex>` (with a last chunk or none)
//!   verifies and seals; `DELETE …/uploads/{id}` aborts. `GET …/uploads`
//!   lists them.
//! - `GET /api/v1/blobstore`: used bytes, the pool's free bytes, counts;
//!   `POST|GET /api/v1/blobstore/scrub`: a scrub started, and its report.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use crate::blobs::format::{digest_string, parse_digest};
use crate::blobs::{engine, BlobError, BlobStore, PutOutcome};
use crate::mgmt::AppState;

use super::releases::{parse_range, RangeSpec};

/// A page of the listing, unless the caller asks for less.
const LIST_LIMIT: usize = 1000;
const LIST_MAX: usize = 10_000;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list))
        .route("/uploads", post(open_session).get(list_sessions))
        .route(
            "/uploads/{id}",
            get(session)
                .patch(append)
                .put(commit)
                .delete(abort)
                .layer(DefaultBodyLimit::disable()),
        )
        .route("/{digest}", get(read).put(put).delete(delete).layer(DefaultBodyLimit::disable()))
        .with_state(state)
}

pub fn store_router(state: Arc<AppState>) -> Router {
    Router::new().route("/", get(stats)).route("/scrub", get(scrub_status).post(scrub)).with_state(state)
}

/// A blob store error as its answer: also what an operation that takes a
/// blob as its source answers when the blob is not there (#733).
pub(crate) fn err(e: BlobError) -> Response {
    let code = match &e {
        BlobError::NoSpace(_) => StatusCode::INSUFFICIENT_STORAGE,
        BlobError::Mismatch { .. } | BlobError::Invalid(_) | BlobError::Body(_) => StatusCode::BAD_REQUEST,
        BlobError::NotFound(_) => StatusCode::NOT_FOUND,
        BlobError::BadOffset { .. } => StatusCode::RANGE_NOT_SATISFIABLE,
        BlobError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        BlobError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
    };
    if code.is_server_error() {
        tracing::warn!("blob store: {e}");
    }
    let mut body = json!({ "error": e.to_string(), "code": code.as_u16() });
    let mut resp = match &e {
        BlobError::BadOffset { length, .. } => {
            body["length"] = json!(length);
            let mut r = (code, Json(body)).into_response();
            if *length > 0 {
                if let Ok(v) = HeaderValue::from_str(&format!("0-{}", length - 1)) {
                    r.headers_mut().insert(header::RANGE, v);
                }
            }
            return r;
        }
        BlobError::Mismatch { want, got, .. } => {
            body["want"] = json!(want);
            body["got"] = json!(got);
            (code, Json(body)).into_response()
        }
        _ => (code, Json(body)).into_response(),
    };
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

fn bad(msg: impl Into<String>) -> Response {
    err(BlobError::Invalid(msg.into()))
}

fn not_found(msg: impl Into<String>) -> Response {
    err(BlobError::NotFound(msg.into()))
}

async fn open_store(state: &Arc<AppState>, create: bool) -> Result<Option<Arc<BlobStore>>, Response> {
    engine::store(state, create).await.map_err(err)
}

fn content_length(h: &HeaderMap) -> Option<u64> {
    h.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok())
}

fn digest_header(d: &str) -> HeaderValue {
    HeaderValue::from_str(d).unwrap_or_else(|_| HeaderValue::from_static(""))
}

// ---------------------------------------------------------------------------
// Blobs
// ---------------------------------------------------------------------------

async fn put(State(state): State<Arc<AppState>>, Path(digest): Path<String>, headers: HeaderMap, body: Body) -> Response {
    let d = match parse_digest(&digest) {
        Ok(d) => d,
        Err(e) => return bad(e),
    };
    let store = match open_store(&state, true).await {
        Ok(Some(s)) => s,
        Ok(None) => return err(BlobError::Io("no blob store".into())),
        Err(r) => return r,
    };
    match store.put(d, content_length(&headers), body.into_data_stream()).await {
        Ok(outcome) => {
            let size = store.get(&d).await.map(|p| p.size());
            let code = if outcome == PutOutcome::Created { StatusCode::CREATED } else { StatusCode::OK };
            let name = digest_string(&d);
            let mut r = (code, Json(json!({ "digest": name, "size": size, "outcome": outcome }))).into_response();
            r.headers_mut().insert("docker-content-digest", digest_header(&name));
            r
        }
        Err(e) => err(e),
    }
}

async fn read(State(state): State<Arc<AppState>>, Path(digest): Path<String>, headers: HeaderMap) -> Response {
    let d = match parse_digest(&digest) {
        Ok(d) => d,
        Err(e) => return bad(e),
    };
    let store = match open_store(&state, false).await {
        Ok(Some(s)) => s,
        Ok(None) => return not_found(format!("{digest} is not held")),
        Err(r) => return r,
    };
    let Some(p) = store.get(&d).await else {
        return not_found(format!("{} is not held", digest_string(&d)));
    };
    let size = p.size();
    let (start, end, partial) = match headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        Some(raw) if size > 0 => match parse_range(raw, size) {
            RangeSpec::Whole => (0, size, false),
            RangeSpec::Partial { start, end } => (start, end + 1, true),
            RangeSpec::Unsatisfiable => {
                return (
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    [(header::CONTENT_RANGE, format!("bytes */{size}"))],
                    format!("the blob is {size} bytes"),
                )
                    .into_response()
            }
        },
        _ => (0, size, false),
    };
    let name = digest_string(&d);
    let mut r = Response::new(Body::from_stream(store.stream(p, start, end)));
    *r.status_mut() = if partial { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK };
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(end - start));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(header::ETAG, digest_header(&format!("\"{name}\"")));
    h.insert("docker-content-digest", digest_header(&name));
    if partial {
        if let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{}/{size}", end - 1)) {
            h.insert(header::CONTENT_RANGE, v);
        }
    }
    r
}

async fn delete(State(state): State<Arc<AppState>>, Path(digest): Path<String>) -> Response {
    let d = match parse_digest(&digest) {
        Ok(d) => d,
        Err(e) => return bad(e),
    };
    let store = match open_store(&state, false).await {
        Ok(Some(s)) => s,
        Ok(None) => return Json(json!({ "digest": digest_string(&d), "deleted": false })).into_response(),
        Err(r) => return r,
    };
    match store.delete(&d).await {
        Ok(deleted) => Json(json!({ "digest": digest_string(&d), "deleted": deleted })).into_response(),
        Err(e) => err(e),
    }
}

async fn list(State(state): State<Arc<AppState>>, Query(q): Query<HashMap<String, String>>) -> Response {
    let after = match q.get("after").filter(|a| !a.is_empty()).map(|a| parse_digest(a)) {
        None => None,
        Some(Ok(d)) => Some(d),
        Some(Err(e)) => return bad(format!("after: {e}")),
    };
    let limit = match q.get("limit").map(|l| l.parse::<usize>()) {
        None => LIST_LIMIT,
        Some(Ok(n)) if n > 0 => n.min(LIST_MAX),
        _ => return bad(format!("limit: a number from 1 to {LIST_MAX}")),
    };
    let store = match open_store(&state, false).await {
        Ok(Some(s)) => s,
        Ok(None) => return Json(json!({ "blobs": [], "next": null })).into_response(),
        Err(r) => return r,
    };
    let (items, next) = store.list(after, limit).await;
    let blobs: Vec<_> = items.iter().map(|(d, size)| json!({ "digest": digest_string(d), "size": size })).collect();
    Json(json!({ "blobs": blobs, "next": next.map(|d| digest_string(&d)) })).into_response()
}

// ---------------------------------------------------------------------------
// Upload sessions
// ---------------------------------------------------------------------------

fn session_id(id: &str) -> Result<uuid::Uuid, Response> {
    id.parse().map_err(|_| bad(format!("{id:?} is not an upload session id")))
}

async fn open_session(State(state): State<Arc<AppState>>) -> Response {
    let store = match open_store(&state, true).await {
        Ok(Some(s)) => s,
        Ok(None) => return err(BlobError::Io("no blob store".into())),
        Err(r) => return r,
    };
    match store.open_session().await {
        Ok(id) => {
            let loc = format!("/api/v1/blobs/uploads/{id}");
            let mut r = (StatusCode::CREATED, Json(json!({ "id": id, "length": 0, "location": loc }))).into_response();
            if let Ok(v) = HeaderValue::from_str(&loc) {
                r.headers_mut().insert(header::LOCATION, v);
            }
            r
        }
        Err(e) => err(e),
    }
}

async fn list_sessions(State(state): State<Arc<AppState>>) -> Response {
    match open_store(&state, false).await {
        Ok(Some(s)) => Json(json!({ "sessions": s.sessions().await })).into_response(),
        Ok(None) => Json(json!({ "sessions": [] })).into_response(),
        Err(r) => r,
    }
}

async fn existing(state: &Arc<AppState>, id: &str) -> Result<(Arc<BlobStore>, uuid::Uuid), Response> {
    let id = session_id(id)?;
    match open_store(state, false).await? {
        Some(s) => Ok((s, id)),
        None => Err(not_found(format!("no upload session {id}"))),
    }
}

async fn session(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let (store, id) = match existing(&state, &id).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    match store.session(&id).await {
        Some(s) => session_response(StatusCode::OK, s.id, s.length),
        None => not_found(format!("no upload session {id}")),
    }
}

fn session_response(code: StatusCode, id: uuid::Uuid, length: u64) -> Response {
    let mut r = (code, Json(json!({ "id": id, "length": length }))).into_response();
    if length > 0 {
        if let Ok(v) = HeaderValue::from_str(&format!("0-{}", length - 1)) {
            r.headers_mut().insert(header::RANGE, v);
        }
    }
    r
}

/// Where an append says it starts: `?offset=`, else `Content-Range`
/// (`<from>-<to>`, with or without `bytes `), else wherever the session is.
fn append_offset(q: &HashMap<String, String>, h: &HeaderMap) -> Result<Option<u64>, Response> {
    if let Some(o) = q.get("offset") {
        return o.parse().map(Some).map_err(|_| bad(format!("offset {o:?}: a number of bytes")));
    }
    let Some(raw) = h.get(header::CONTENT_RANGE).and_then(|v| v.to_str().ok()) else {
        return Ok(None);
    };
    let spec = raw.trim().trim_start_matches("bytes").trim().split('/').next().unwrap_or("");
    let from = spec.split('-').next().unwrap_or("").trim();
    from.parse().map(Some).map_err(|_| bad(format!("Content-Range {raw:?}: <from>-<to>")))
}

async fn append(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let (store, id) = match existing(&state, &id).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let offset = match append_offset(&q, &headers) {
        Ok(o) => o,
        Err(r) => return r,
    };
    if let Some(n) = content_length(&headers) {
        if let Err(e) = store.space_for(n).await {
            return err(e);
        }
    }
    match store.append(&id, offset, body.into_data_stream()).await {
        Ok(len) => session_response(StatusCode::ACCEPTED, id, len),
        Err(e) => err(e),
    }
}

async fn commit(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    body: Body,
) -> Response {
    let (store, id) = match existing(&state, &id).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let Some(digest) = q.get("digest") else {
        return bad("digest=sha256:<hex>: what the session's bytes are to be sealed as");
    };
    let d = match parse_digest(digest) {
        Ok(d) => d,
        Err(e) => return bad(e),
    };
    match store.commit(&id, d, Some(body.into_data_stream())).await {
        Ok(outcome) => {
            let size = store.get(&d).await.map(|p| p.size());
            let name = digest_string(&d);
            let loc = format!("/api/v1/blobs/{name}");
            let mut r = (StatusCode::CREATED, Json(json!({ "digest": name, "size": size, "outcome": outcome, "location": loc })))
                .into_response();
            r.headers_mut().insert("docker-content-digest", digest_header(&name));
            if let Ok(v) = HeaderValue::from_str(&loc) {
                r.headers_mut().insert(header::LOCATION, v);
            }
            r
        }
        Err(e) => err(e),
    }
}

async fn abort(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let (store, id) = match existing(&state, &id).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    match store.abort(&id).await {
        Ok(aborted) => Json(json!({ "id": id, "aborted": aborted })).into_response(),
        Err(e) => err(e),
    }
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

async fn stats(State(state): State<Arc<AppState>>) -> Response {
    match open_store(&state, false).await {
        Ok(Some(s)) => {
            let mut v = serde_json::to_value(s.stats().await).unwrap_or_default();
            v["exists"] = json!(true);
            v["scrub"] = serde_json::to_value(s.scrub_status()).unwrap_or_default();
            Json(v).into_response()
        }
        Ok(None) => Json(json!({ "exists": false, "volume_name": engine::VOLUME_NAME })).into_response(),
        Err(r) => r,
    }
}

async fn scrub(State(state): State<Arc<AppState>>) -> Response {
    match open_store(&state, false).await {
        Ok(Some(s)) => {
            let started = s.start_scrub();
            (StatusCode::ACCEPTED, Json(json!({ "started": started, "scrub": s.scrub_status() }))).into_response()
        }
        Ok(None) => not_found("no blob store on this node"),
        Err(r) => r,
    }
}

async fn scrub_status(State(state): State<Arc<AppState>>) -> Response {
    match open_store(&state, false).await {
        Ok(Some(s)) => Json(s.scrub_status()).into_response(),
        Ok(None) => not_found("no blob store on this node"),
        Err(r) => r,
    }
}
