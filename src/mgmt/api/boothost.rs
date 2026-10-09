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
//! PUT  /api/v1/boothost/{name}/tpm        {tpm: required|none} — the machine's
//!                                         TPM mark (#216); admin only
//! DELETE /api/v1/boothost/{name}/tpm      clear it (reads as none); admin only
//! GET  /api/v1/boothost/{name}/attestation the boot chain of its last claim
//!                                         and its TPM mark, for stormcert
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
    routing::{get, post, put},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;

use super::synonyms::{body, err, BOOTHOST_NS, HOSTGOLDEN_NS};
use super::ApiError;
use crate::mgmt::AppState;
use crate::volume::synonym::{self, Host, TpmMark};
use crate::volume::VolumeId;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list))
        .route("/{name}", get(get_one).put(set_aliases))
        .route("/{name}/rename", post(rename))
        .route("/{name}/tpm", put(set_tpm).delete(clear_tpm))
        .route("/{name}/attestation", get(attestation))
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
        // The boot override a claim last acted on and has not reported (#354).
        "boot_override": h.boot_override,
        // How far the last install got (#220): laid, then booted.
        "install": h.install.as_ref().map(|p| p.json()),
        // What its attestation must carry (#216), and its last boot claim.
        "tpm": tpm_word(h),
        "tpm_set_at": h.tpm_set_at,
        "last_claim": h.last_claim.as_ref().map(|c| c.json()),
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

/// The mark as it goes on the wire: `required`, `none`, or `unset` (which
/// stormcert treats as `none`).
fn tpm_word(h: &Host) -> &'static str {
    h.tpm.map(|t| t.as_str()).unwrap_or("unset")
}

#[derive(Debug, Deserialize)]
pub struct TpmRequest {
    pub tpm: String,
}

/// `PUT /api/v1/boothost/{name}/tpm` — set by the platform or an admin as the
/// machine joins the fleet (#216). Destructive in the auth sense: `none`
/// downgrades the machine's attestation, so the node token cannot set it.
async fn set_tpm(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(req): Json<TpmRequest>,
) -> Response {
    let mark: TpmMark = match req.tpm.parse() {
        Ok(m) => m,
        Err(e) => return ApiError::bad_request(e),
    };
    mark_tpm(&state, &name, Some(mark)).await
}

/// `DELETE /api/v1/boothost/{name}/tpm` — unset: the boot chain suffices.
async fn clear_tpm(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    mark_tpm(&state, &name, None).await
}

async fn mark_tpm(state: &AppState, name: &str, mark: Option<TpmMark>) -> Response {
    let set = state.synonyms.write().await.set_tpm(name, mark);
    match set {
        Ok(h) => {
            tracing::info!(host = %h.name, tpm = tpm_word(&h), "boot host TPM mark set");
            Json(view(state, &h).await).into_response()
        }
        Err(e) => err(e),
    }
}

/// One volume of the chain, as the engine sees it now.
async fn link(state: &AppState, id: VolumeId) -> (serde_json::Value, Option<(bool, Option<VolumeId>)>) {
    let vm = state.volume_manager.lock().await;
    let Some(handle) = vm.get_volume_handle(&id) else {
        return (json!({ "id": id.0, "present": false }), None);
    };
    let sealed = vm.is_sealed(&id);
    let parent = vm.parent(&id);
    let owner = vm.owner(&id).cloned();
    let name = handle.name().await;
    (
        json!({
            "id": id.0,
            "name": name,
            "present": true,
            "sealed": sealed,
            "parent": parent.map(|p| p.0),
            "owner": owner,
        }),
        Some((sealed, parent)),
    )
}

/// `GET /api/v1/boothost/{name}/attestation` — boot-chain evidence for
/// stormcert (#216, stormcert#23).
///
/// What it states: the boot clone the engine last handed this machine, the
/// host NQNs that clone was served to, and the chain from it to the golden
/// `boothost/<name>` assigns, each link checked now against the volumes the
/// engine holds. It also carries the machine's TPM mark, so the boot claim and
/// the mark come from one record the machine cannot write.
///
/// By the host's **name** only: an alias (a serial, a MAC) is a 404 naming the
/// host, because the name is what a certificate's `system:node:<name>` is
/// matched against, and a lookup that resolved aliases would attest a
/// different machine than the one asked about.
///
/// What it does not prove: that the machine asking for a certificate is the
/// one that claimed. Until a claim is bound to the host itself (stormcos#35)
/// the tag is the binding, and anything that can reach the claim can claim
/// as a machine (it gets that machine's image, and moves this record).
async fn attestation(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    let (host, assignment) = {
        let store = state.synonyms.read().await;
        let resolved = store.host_of(&name);
        match resolved {
            Some(h) if h == name => (store.host(&h), store.get(super::synonyms::BOOTHOST_NS, &h).cloned()),
            Some(h) => {
                return ApiError::not_found(format!(
                    "{name} is an alias of host {h}: attestation is asked for by the host's name"
                ))
            }
            None => (None, None),
        }
    };
    let Some(h) = host else {
        return ApiError::not_found(format!("no host {name}"));
    };
    let tpm = tpm_word(&h);
    let requires = if h.tpm == Some(TpmMark::Required) { "tpm_quote" } else { "boot_chain" };
    let Some(c) = h.last_claim.clone() else {
        return Json(json!({
            "host": h.name,
            "tpm": tpm,
            "requires": requires,
            "claimed": false,
            "chain": "none",
            "problems": ["this machine has not claimed a boot clone since the engine began recording claims"],
        }))
        .into_response();
    };

    let (clone, clone_s) = link(&state, c.clone).await;
    let (host_golden, hg_s) = link(&state, c.host_golden).await;
    let (golden, g_s) = link(&state, c.golden).await;
    let mut problems: Vec<String> = Vec::new();
    match clone_s {
        None => problems.push(format!("the boot clone {} is gone (released, or the machine claimed again)", c.clone)),
        Some((true, _)) => problems.push(format!("the boot clone {} is sealed", c.clone)),
        Some((false, p)) if p != Some(c.host_golden) => {
            problems.push(format!("the boot clone {} is not a clone of the host golden {}", c.clone, c.host_golden))
        }
        _ => {}
    }
    match hg_s {
        None => problems.push(format!("the host golden {} is gone", c.host_golden)),
        Some((false, _)) => problems.push(format!("the host golden {} is not sealed", c.host_golden)),
        Some((true, p)) if p != Some(c.golden) => {
            problems.push(format!("the host golden {} is not a clone of the golden {}", c.host_golden, c.golden))
        }
        _ => {}
    }
    match g_s {
        None => problems.push(format!("the golden {} is gone", c.golden)),
        Some((false, _)) => problems.push(format!("the golden {} is not sealed", c.golden)),
        _ => {}
    }
    // The assignment still naming the golden says the claim is of what the
    // platform assigns now, not of an image the machine has since moved off.
    let assigned_now = assignment.as_ref().and_then(|a| a.target.volume_id());
    let current = assigned_now == Some(c.golden);
    // A golden published here as a release carries the digest its publisher
    // recorded; absent, the engine says so rather than computing one.
    let release = super::releases::load(&state)
        .await
        .into_iter()
        .find(|r| r.volume_id == c.golden.0)
        .map(|r| json!({
            "version": r.version,
            "digest": r.digest,
            "created_unix": r.created_unix,
        }));
    let mut golden = golden;
    golden["synonym"] = json!(synonym::key(super::synonyms::BOOTHOST_NS, &h.name));
    golden["assignment_version"] = json!(c.assignment_version);
    golden["assigned_now"] = json!(current);
    golden["label"] = json!(assignment.as_ref().and_then(|a| a.label.clone()));
    golden["digest"] = json!(release.as_ref().and_then(|r| r["digest"].as_str().map(str::to_string)));
    golden["release"] = release.unwrap_or(serde_json::Value::Null);
    let mut clone = clone;
    clone["claimed_at"] = json!(c.claimed_at);
    clone["claimed_as"] = json!(c.claimed_as);
    Json(json!({
        "host": h.name,
        "aliases": h.aliases,
        "tpm": tpm,
        "tpm_set_at": h.tpm_set_at,
        "requires": requires,
        "claimed": true,
        "host_nqns": c.host_nqns,
        "clone": clone,
        "host_golden": host_golden,
        "golden": golden,
        "chain": if problems.is_empty() { "intact" } else { "broken" },
        "problems": problems,
    }))
    .into_response()
}
