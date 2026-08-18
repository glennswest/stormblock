//! `/api/v1/fstemplates` — preformatted filesystem templates (#38).
//!
//! ```text
//! POST   /api/v1/fstemplates              create (formats and seals by default)
//! GET    /api/v1/fstemplates              list
//! GET    /api/v1/fstemplates/{id}         one, by id or name
//! POST   /api/v1/fstemplates/{id}/seal    verify + snapshot an externally formatted template
//! POST   /api/v1/fstemplates/{id}/clone   mint a CoW clone with a fresh filesystem UUID
//! DELETE /api/v1/fstemplates/{id}         remove, with its volumes (?purge=false keeps them)
//! GET    /api/v1/fstemplates/orphans      template volumes no template claims
//! DELETE /api/v1/fstemplates/orphans      delete them
//! ```
//!
//! `POST /api/v1/volumes` also takes `from_template`, which is the same clone
//! path for callers that already speak the volume API.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::ApiError;
use crate::fs::template::{self, CloneSpec, FsKind, TemplateError, TemplateSpec};
use crate::fs::SeedFile;
use crate::mgmt::config::parse_size;
use crate::mgmt::AppState;
use crate::volume::VolumeId;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list_templates).post(create_template))
        // Ahead of `/{id}`: a literal path wins over the capture.
        .route("/orphans", get(list_orphans).delete(reclaim_orphans))
        .route("/{id}", get(get_template).delete(delete_template))
        .route("/{id}/seal", post(seal_template))
        .route("/{id}/clone", post(clone_template))
        .with_state(state)
}

/// Map a lifecycle error onto the status a client can act on.
fn err(e: TemplateError) -> Response {
    match e {
        TemplateError::NotFound(m) => ApiError::not_found(m),
        TemplateError::Exists(m) | TemplateError::Conflict(m) => ApiError::conflict(m),
        // A filesystem that is not safe to seal is a state conflict, not a bad
        // request: the caller's job is to unmount it and retry.
        TemplateError::NotSealable(m) => ApiError::conflict(m),
        TemplateError::Invalid(m) => ApiError::bad_request(m),
        TemplateError::Internal(m) => ApiError::internal(m),
        // The id is the response, not a detail of it. A leaked clone carries a
        // caller-chosen name, so nothing on this side can find it by name
        // later — the caller has to be handed the one thing that identifies
        // it, or the volume is unreclaimable (#48).
        TemplateError::Leaked { volume_id, message } => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": format!("{message}; volume {volume_id} is leaked"),
                "code": 500,
                "leaked_volume_id": volume_id,
            })),
        )
            .into_response(),
    }
}

fn flag(q: &std::collections::HashMap<String, String>, key: &str) -> bool {
    matches!(q.get(key).map(|v| v.as_str()), Some("") | Some("1") | Some("true") | Some("yes"))
}

/// A flag that is on unless the caller explicitly turns it off.
fn flag_default_on(q: &std::collections::HashMap<String, String>, key: &str) -> bool {
    !matches!(q.get(key).map(|v| v.as_str()), Some("0") | Some("false") | Some("no"))
}

#[derive(Debug, Deserialize)]
pub struct CreateTemplateRequest {
    pub name: String,
    /// Size, either as bytes or as a human string ("256M").
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    /// Filesystem to lay down. Only ext4 today.
    #[serde(default)]
    pub fs: Option<String>,
    /// Journal on or off. Absent follows the filesystem kind — ext4 and ext3
    /// have one, ext2 does not. A consumer that cannot replay a journal
    /// (RouterOS) is left read-only the first time one goes dirty, so it is
    /// worth turning off for those and leaving on everywhere else.
    #[serde(default)]
    pub journal: Option<bool>,
    /// A `mke2fs -O`-style feature list applied over the kind's defaults —
    /// `"^64bit"`, `"^metadata_csum,^flex_bg"`, and so on. The `-O` vocabulary
    /// is passed straight through rather than re-invented as flags.
    #[serde(default)]
    pub features: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    /// Files written into the filesystem before it is sealed, so every clone
    /// inherits them. `contents` is plain text; `contents_base64` carries
    /// anything that is not.
    #[serde(default)]
    pub files: Vec<SeedFileRequest>,
    /// Format here and seal in one call. Default true; false leaves the
    /// template `awaiting_format` for an initiator to format over an export.
    #[serde(default = "yes")]
    pub format: bool,
}

fn yes() -> bool {
    true
}

/// One file to place into a template's filesystem.
#[derive(Debug, Deserialize)]
pub struct SeedFileRequest {
    /// Absolute path inside the filesystem. Parents are created.
    pub path: String,
    #[serde(default)]
    pub contents: Option<String>,
    #[serde(default)]
    pub contents_base64: Option<String>,
}

impl SeedFileRequest {
    pub(crate) fn resolve(self) -> Result<SeedFile, String> {
        let bytes = match (self.contents, self.contents_base64) {
            (Some(_), Some(_)) => {
                return Err(format!("{}: give contents or contents_base64, not both", self.path))
            }
            (Some(text), None) => text.into_bytes(),
            (None, Some(b64)) => {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD
                    .decode(b64.as_bytes())
                    .map_err(|e| format!("{}: contents_base64 is not base64: {e}", self.path))?
            }
            (None, None) => Vec::new(),
        };
        Ok(SeedFile { path: self.path, contents: bytes })
    }
}

#[derive(Debug, Deserialize)]
pub struct CloneRequest {
    pub name: String,
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    /// Give the clone its own filesystem UUID. Default true — without it two
    /// clones on one host collide on mount-by-UUID and in the blkid cache.
    #[serde(default = "yes")]
    pub stamp_uuid: bool,
    /// Also rewrite the backup superblocks: one copy-on-write extent per
    /// backup group, so off unless asked for.
    #[serde(default)]
    pub stamp_backups: bool,
    #[serde(default)]
    pub label: Option<String>,
    /// Check the clone before handing it out. Default true.
    #[serde(default = "yes")]
    pub verify: bool,
}

/// Resolve `size` / `size_bytes` into bytes.
pub(crate) fn resolve_size(
    size: &Option<String>,
    size_bytes: Option<u64>,
) -> Result<Option<u64>, String> {
    match (size, size_bytes) {
        (Some(s), _) => parse_size(s).map(Some).map_err(|e| format!("invalid size '{s}': {e}")),
        (None, Some(b)) => Ok(Some(b)),
        (None, None) => Ok(None),
    }
}

async fn list_templates(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "fstemplates", "method" => "list")
        .increment(1);
    let store = state.fstemplates.lock().await;
    let items: Vec<_> = store.templates.iter().map(|t| t.json()).collect();
    let count = items.len();
    Json(json!({ "items": items, "count": count }))
}

async fn get_template(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let store = state.fstemplates.lock().await;
    match store.find(&id) {
        Some(t) => Json(t.json()).into_response(),
        None => ApiError::not_found(format!("fstemplate {id} not found")),
    }
}

async fn create_template(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateTemplateRequest>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "fstemplates", "method" => "create")
        .increment(1);

    let fs: FsKind = match req.fs.as_deref().unwrap_or("ext4").parse() {
        Ok(f) => f,
        Err(e) => return ApiError::bad_request(e),
    };
    let size = match resolve_size(&req.size, req.size_bytes) {
        Ok(Some(s)) => s,
        Ok(None) => return ApiError::bad_request("size (or size_bytes) is required"),
        Err(e) => return ApiError::bad_request(e),
    };

    let mut seed = Vec::with_capacity(req.files.len());
    for f in req.files {
        match f.resolve() {
            Ok(s) => seed.push(s),
            Err(e) => return ApiError::bad_request(e),
        }
    }

    let spec = TemplateSpec {
        name: req.name,
        fs,
        size_bytes: size,
        journal: req.journal,
        label: req.label.unwrap_or_default(),
        features: req.features,
        seed,
        format_in_core: req.format,
    };

    match template::create(&state.volume_manager, &state.fstemplates, &spec).await {
        Ok(t) => {
            let body = if t.state == crate::fs::TemplateState::AwaitingFormat {
                json!({
                    "template": t.json(),
                    "next": "format the raw volume (export it first), then POST /api/v1/fstemplates/{id}/seal",
                })
            } else {
                json!({ "template": t.json() })
            };
            (axum::http::StatusCode::CREATED, Json(body)).into_response()
        }
        Err(e) => err(e),
    }
}

async fn seal_template(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "fstemplates", "method" => "seal")
        .increment(1);
    let force = flag(&q, "force");

    let template_id = {
        let store = state.fstemplates.lock().await;
        match store.find(&id) {
            Some(t) => t.id,
            None => return ApiError::not_found(format!("fstemplate {id} not found")),
        }
    };

    // Guard: an export still standing means something may still be mounted,
    // and a snapshot taken mid-write is exactly the dirty superblock this
    // whole path exists to avoid.
    if !force {
        let raw = {
            let store = state.fstemplates.lock().await;
            store.get(&template_id).and_then(|t| t.raw_volume_id)
        };
        if let Some(raw) = raw {
            let exports = state.exports.read().await;
            if exports.iter().any(|e| e.volume_id == raw) {
                return ApiError::conflict(
                    "the template volume is still exported — detach and remove the export before \
                     sealing (or pass force=true)",
                );
            }
        }
    }

    match template::seal(&state.volume_manager, &state.fstemplates, &template_id, force).await {
        Ok(t) => Json(t.json()).into_response(),
        Err(e) => err(e),
    }
}

async fn clone_template(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<CloneRequest>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "fstemplates", "method" => "clone")
        .increment(1);

    let size = match resolve_size(&req.size, req.size_bytes) {
        Ok(s) => s,
        Err(e) => return ApiError::bad_request(e),
    };
    let spec = CloneSpec {
        name: req.name,
        size_bytes: size,
        stamp_uuid: req.stamp_uuid,
        stamp_backups: req.stamp_backups,
        label: req.label,
        verify: req.verify,
    };

    match template::clone_template(&state.volume_manager, &state.fstemplates, &id, &spec).await {
        Ok(c) => (
            axum::http::StatusCode::CREATED,
            Json(json!({
                "volume_id": c.volume_id.0,
                "name": spec.name,
                "template_id": c.template_id,
                "fs_uuid": c.fs_uuid,
                "size_bytes": c.size_bytes,
                "verified": c.verified,
            })),
        )
            .into_response(),
        Err(e) => err(e),
    }
}

async fn delete_template(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "fstemplates", "method" => "delete")
        .increment(1);
    // Purging is the default. Removing the store entry while leaving the
    // volumes standing is how a node fills with template debris nothing can
    // name afterwards — 186 of 187 volumes on one instance (#47).
    let purge = flag_default_on(&q, "purge");
    let force = flag(&q, "force");

    let template_id = {
        let store = state.fstemplates.lock().await;
        match store.find(&id) {
            Some(t) => t.id,
            None => return ApiError::not_found(format!("fstemplate {id} not found")),
        }
    };

    match template::delete(&state.volume_manager, &state.fstemplates, &template_id, purge, force).await {
        Ok(purged) => Json(json!({ "deleted": template_id, "purged_volumes": purged })).into_response(),
        Err(e) => err(e),
    }
}

async fn list_orphans(State(state): State<Arc<AppState>>) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "fstemplates", "method" => "orphans")
        .increment(1);
    // Never offer to reclaim something this node is serving (#48).
    let in_use = super::volumes_in_use(&state).await;
    let found = template::orphans(&state.volume_manager, &state.fstemplates, &in_use).await;
    let bytes: u64 = found.iter().map(|o| o.allocated_bytes).sum();
    Json(json!({ "orphans": found, "count": found.len(), "allocated_bytes": bytes }))
        .into_response()
}

async fn reclaim_orphans(State(state): State<Arc<AppState>>) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "fstemplates", "method" => "reclaim_orphans")
        .increment(1);
    let in_use = super::volumes_in_use(&state).await;
    let gone = template::reclaim_orphans(&state.volume_manager, &state.fstemplates, &in_use).await;
    let bytes: u64 = gone.iter().map(|o| o.allocated_bytes).sum();
    tracing::info!("reclaimed {} orphaned template volume(s), {bytes} bytes", gone.len());
    Json(json!({ "reclaimed": gone, "count": gone.len(), "allocated_bytes": bytes }))
        .into_response()
}

/// Clone a template on behalf of `POST /api/v1/volumes {from_template}`.
///
/// Kept here so both entry points share one implementation — a clone must
/// always go through the fresh-UUID stamp, whichever door it came in.
pub async fn clone_for_volume_api(
    state: &AppState,
    key: &str,
    name: &str,
    size_bytes: Option<u64>,
) -> Result<(VolumeId, Option<Uuid>, u64), Response> {
    let spec = CloneSpec { size_bytes, ..CloneSpec::new(name) };
    match template::clone_template(&state.volume_manager, &state.fstemplates, key, &spec).await {
        Ok(c) => Ok((c.volume_id, c.fs_uuid, c.size_bytes)),
        Err(e) => Err(err(e)),
    }
}
