//! `/api/v1/boothost` — the machines that boot from this node, by name (#199).
//!
//! ```text
//! GET  /api/v1/boothost                   every host: name, aliases, former names,
//!                                         assignment, host golden
//! GET  /api/v1/boothost?unnamed=1         only machines that booted the default
//!                                         and are still called mac-<hex> (#200)
//! GET  /api/v1/boothost/{name|alias}      one host, found by its name or an alias
//! PUT  /api/v1/boothost/{name}            {aliases: [...]} — replace its aliases
//! POST /api/v1/boothost/{name}/rename     {to, keep_alias?} — rename it, keeping
//!                                         its assignment, golden and clones
//! ```
//!
//! A machine is known by its DNS name. Its SMBIOS serial and its MACs are
//! aliases: a claim of `boothost/<alias>` is a claim of the host it belongs
//! to, so an agent that still claims by serial boots the same image. Two
//! hosts never share an alias, and nothing becomes one by itself — MicroCloud
//! nodes share a chassis serial, so an operator (or stormcentral) says which
//! machine is which. Every route here needs the token; the claim itself stays
//! at `/api/v1/synonyms/boothost/<name>/claim` and is unchanged.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;

use super::synonyms::{body, err, BOOTHOST_NS, HOSTGOLDEN_NS};
use super::ApiError;
use crate::mgmt::AppState;
use crate::volume::synonym::{self, Host};

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list))
        .route("/{name}", get(get_one).put(set_aliases))
        .route("/{name}/rename", post(rename))
        .with_state(state)
}

#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    /// `1`/`true`: only the machines nobody has named yet — booted the
    /// default under their MAC and still called `mac-<hex>` (#200).
    #[serde(default)]
    pub unnamed: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AliasesRequest {
    pub aliases: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct RenameRequest {
    pub to: String,
    /// Keep the old name as an alias, so a machine still claiming by it
    /// boots as before (default true).
    #[serde(default = "yes")]
    pub keep_alias: bool,
}

fn yes() -> bool {
    true
}

/// A host as it goes on the wire, with its assignment and golden as the
/// synonyms API shows them.
async fn view(state: &AppState, h: &Host) -> serde_json::Value {
    let (assignment, golden) = {
        let store = state.synonyms.read().await;
        (store.get(BOOTHOST_NS, &h.name).cloned(), store.get(HOSTGOLDEN_NS, &h.name).cloned())
    };
    let assignment = match &assignment {
        Some(s) => body(state, s, None).await,
        None => serde_json::Value::Null,
    };
    let golden = match &golden {
        Some(s) => body(state, s, None).await,
        None => serde_json::Value::Null,
    };
    json!({
        "name": h.name,
        "aliases": h.aliases,
        "former_names": h.former_names,
        "provisional": synonym::is_provisional(&h.name),
        // What its boot agent does before it claims (#148).
        "intent": h.intent.as_str(),
        "install_claim": h.install_claim.map(|v| v.0),
        "assignment": assignment,
        "host_golden": golden,
        "created_at": h.created_at,
        "updated_at": h.updated_at,
    })
}

async fn list(State(state): State<Arc<AppState>>, Query(q): Query<ListQuery>) -> Response {
    let unnamed = match q.unnamed.as_deref().map(str::to_ascii_lowercase).as_deref() {
        None | Some("0") | Some("false") => false,
        Some("" | "1" | "true") => true,
        Some(other) => return ApiError::bad_request(format!("unnamed={other}: expected 1 or 0")),
    };
    let hosts = state.synonyms.read().await.hosts();
    let mut items = Vec::with_capacity(hosts.len());
    for h in hosts.iter().filter(|h| !unnamed || synonym::is_provisional(&h.name)) {
        items.push(view(&state, h).await);
    }
    let count = items.len();
    Json(json!({ "items": items, "count": count })).into_response()
}

async fn get_one(State(state): State<Arc<AppState>>, Path(key): Path<String>) -> Response {
    let found = {
        let store = state.synonyms.read().await;
        store.host_of(&key).and_then(|n| store.host(&n))
    };
    let Some(h) = found else {
        return ApiError::not_found(format!("no host {key} (by name or alias)"));
    };
    let mut v = view(&state, &h).await;
    if h.name != key {
        v["resolved_from"] = json!(key);
    }
    Json(v).into_response()
}

async fn set_aliases(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(req): Json<AliasesRequest>,
) -> Response {
    let set = state.synonyms.write().await.set_aliases(&name, &req.aliases);
    match set {
        Ok(h) => {
            tracing::info!(host = %h.name, aliases = ?h.aliases, "boot host aliases set");
            Json(view(&state, &h).await).into_response()
        }
        Err(e) => err(e),
    }
}

async fn rename(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(req): Json<RenameRequest>,
) -> Response {
    let renamed = state.synonyms.write().await.rename_host(&name, &req.to, req.keep_alias);
    match renamed {
        Ok(h) => {
            tracing::info!(
                from = %name, to = %h.name,
                "boot host renamed; {} and its golden moved with their history",
                synonym::key(BOOTHOST_NS, &h.name)
            );
            (StatusCode::OK, Json(view(&state, &h).await)).into_response()
        }
        Err(e) => err(e),
    }
}
