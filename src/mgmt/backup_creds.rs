//! Per-node backup credentials (#693, stormcos#1027, the owner's B).
//!
//! A node's VM backups go to a store: another node's engine, forge1's by
//! default. The store's admin token never leaves the store. Instead the
//! store's engine mints a bearer for **one node's backups alone**, keeps only
//! its sha256, and can revoke it per node.
//!
//! **What it reaches.** A backup bearer is the principal `backup:<node>`. It
//! reaches the calls a backup and a restore make on a store (stormvm's
//! `stormvm_block::backup::Transfer`, driven by rustkube-node#380), and
//! nothing else:
//!
//! | verb | what for |
//! |---|---|
//! | `GET /v1/volumes/{id or name}` | find a backup's target volume |
//! | `POST /v1/volumes` | a blank target: no `source`, not `secret`, owner kind `VirtualMachineBackup` |
//! | `DELETE /v1/volumes/{id}`, `DELETE /api/v1/volumes/{id}` | a target too small for a grown disk |
//! | `POST /v1/volumes/{id}/attach` | the target served over `nvme_tcp` for the node's ship |
//! | `POST /v1/volumes/{id}/detach` | let go of it |
//! | `GET /v1/group-snapshots` | its own points (another node's are not listed) |
//! | `POST /v1/group-snapshots` | a point, of its own volumes |
//! | `GET`, `DELETE /v1/group-snapshots/{id}` | read or prune a point |
//! | `POST /v1/snapshots/{id}/ship` | a restore: a point's member shipped to the node |
//! | `GET`, `DELETE /v1/ship-jobs/{id}` | that ship's job |
//!
//! **Whose.** Its names carry no node (`<ns>.<vm>.<disk>.backup`), so the
//! scope is by creator, not by name: what a backup bearer makes is recorded
//! as `backup:<node>`'s (`mgmt::creators`, #422), and every call that names
//! an object reaches only one so recorded. A name another holds is refused
//! (409), never handed back. Revoking the bearer leaves the node's backups
//! where they are; a bearer minted again for the node reaches them again.
//!
//! **Seen.** Every call is in the audit log as `backup:<node>`, logged with
//! its node, and counted: `storm_backup_credential_requests_total{node,
//! result}`. The record keeps when each bearer was last used.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path as UrlPath, Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{delete, get},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::AppState;

const FILE: &str = "backup-credentials.json";
/// The kind a backup's volumes on a store are owned by (stormvm's
/// `BACKUP_OWNER_KIND`).
pub const OWNER_KIND: &str = "VirtualMachineBackup";
/// How often a use is written to the record (the time is kept in memory
/// between).
const SAVE_USE_EVERY: u64 = 60;
/// The largest body a backup call's check reads.
const BODY_LIMIT: usize = 1 << 20;

/// The principal a node's backup bearer is.
pub fn principal(node: &str) -> String {
    format!("backup:{node}")
}

/// A point's key in the creator record: a `/v1` group snapshot.
pub fn group_key(id: &str) -> String {
    format!("v1group:{id}")
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn sha256_hex(s: &str) -> String {
    let d = Sha256::digest(s.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// A node name a credential may be minted for: a DNS name, as the node
/// knows itself.
pub fn valid_node(node: &str) -> bool {
    !node.is_empty()
        && node.len() <= 253
        && node.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !l.starts_with('-')
                && !l.ends_with('-')
        })
}

/// One node's credential, as kept: never the bearer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub sha256: String,
    pub created_at: u64,
    /// Who minted it (`admin-token`, `client-cert-admin:…`).
    pub created_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<u64>,
    #[serde(default)]
    pub uses: u64,
}

/// What the list says of one: who holds one, and since and until when.
#[derive(Debug, Clone, Serialize)]
pub struct Holder {
    pub node: String,
    pub principal: String,
    pub created_at: u64,
    pub created_by: String,
    pub last_used_at: Option<u64>,
    pub uses: u64,
    /// The volumes and points it made that are still here.
    pub objects: usize,
}

/// The record, loaded once and written on every change.
#[derive(Default)]
pub struct BackupCreds {
    path: Option<PathBuf>,
    map: BTreeMap<String, Entry>,
    /// When a use was last written, per node.
    saved_use: BTreeMap<String, u64>,
}

impl BackupCreds {
    pub fn load(data_dir: Option<&Path>) -> Self {
        let path = data_dir.map(|d| d.join(FILE));
        let map = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        BackupCreds { path, map, saved_use: BTreeMap::new() }
    }

    /// Mint a bearer for `node`, replacing any it held. Returns the bearer
    /// (the only time it is seen) and whether one was replaced. Refused with
    /// nowhere to keep it: a credential that a restart forgets is one the
    /// node's install-config carries in vain.
    pub fn mint(&mut self, node: &str, by: &str) -> Result<(String, bool), String> {
        if !valid_node(node) {
            return Err(format!("node '{node}' is not a DNS name"));
        }
        if self.path.is_none() {
            return Err("this engine has no data directory to keep a backup credential in".into());
        }
        let token = format!("sbbk-{}", super::auth::mint());
        let entry = Entry { sha256: sha256_hex(&token), created_at: now(), created_by: by.to_string(), last_used_at: None, uses: 0 };
        let replaced = self.map.insert(node.to_string(), entry).is_some();
        self.save()?;
        Ok((token, replaced))
    }

    /// Revoke `node`'s bearer. Whether it held one.
    pub fn revoke(&mut self, node: &str) -> Result<bool, String> {
        if self.map.remove(node).is_none() {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
    }

    /// The node a bearer is the credential of.
    pub fn holder(&self, bearer: &str) -> Option<String> {
        if !bearer.starts_with("sbbk-") {
            return None;
        }
        let h = sha256_hex(bearer);
        self.map.iter().find(|(_, e)| e.sha256 == h).map(|(n, _)| n.clone())
    }

    /// Count a use of `node`'s bearer.
    pub fn used(&mut self, node: &str) {
        let t = now();
        let Some(e) = self.map.get_mut(node) else { return };
        e.last_used_at = Some(t);
        e.uses += 1;
        let last = self.saved_use.get(node).copied().unwrap_or(0);
        if t.saturating_sub(last) >= SAVE_USE_EVERY {
            self.saved_use.insert(node.to_string(), t);
            let _ = self.save();
        }
    }

    pub fn entries(&self) -> &BTreeMap<String, Entry> {
        &self.map
    }

    fn save(&self) -> Result<(), String> {
        let Some(p) = &self.path else { return Ok(()) };
        let b = serde_json::to_vec_pretty(&self.map).map_err(|e| format!("{FILE}: {e}"))?;
        crate::serve::wiring::write_atomic(p, &b).map_err(|e| format!("{}: {e}", p.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

/// A call a backup bearer may make, with the object it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verb {
    GetVolume(String),
    CreateVolume,
    DeleteVolume(String),
    Attach(String),
    Detach(String),
    ListPoints,
    CreatePoint,
    GetPoint(String),
    DeletePoint(String),
    Ship(String),
    GetJob(String),
    CancelJob(String),
}

impl Verb {
    fn reads_body(&self) -> bool {
        matches!(self, Verb::CreateVolume | Verb::CreatePoint | Verb::Attach(_))
    }
}

/// The verb a call is, or `None`: not one a backup bearer may make.
pub fn verb_of(method: &Method, path: &str) -> Option<Verb> {
    let p = path.trim_end_matches('/');
    let seg: Vec<&str> = p.split('/').collect();
    let id = |s: &str| (!s.is_empty()).then(|| s.to_string());
    match (method.as_str(), seg.as_slice()) {
        ("GET", ["", "v1", "volumes", k]) => id(k).map(Verb::GetVolume),
        ("POST", ["", "v1", "volumes"]) => Some(Verb::CreateVolume),
        ("DELETE", ["", "v1", "volumes", k]) | ("DELETE", ["", "api", "v1", "volumes", k]) => id(k).map(Verb::DeleteVolume),
        ("POST", ["", "v1", "volumes", k, "attach"]) => id(k).map(Verb::Attach),
        ("POST", ["", "v1", "volumes", k, "detach"]) => id(k).map(Verb::Detach),
        ("GET", ["", "v1", "group-snapshots"]) => Some(Verb::ListPoints),
        ("POST", ["", "v1", "group-snapshots"]) => Some(Verb::CreatePoint),
        ("GET", ["", "v1", "group-snapshots", k]) => id(k).map(Verb::GetPoint),
        ("DELETE", ["", "v1", "group-snapshots", k]) => id(k).map(Verb::DeletePoint),
        ("POST", ["", "v1", "snapshots", k, "ship"]) => id(k).map(Verb::Ship),
        ("GET", ["", "v1", "ship-jobs", k]) => id(k).map(Verb::GetJob),
        ("DELETE", ["", "v1", "ship-jobs", k]) => id(k).map(Verb::CancelJob),
        _ => None,
    }
}

/// What a backup bearer reaches, as its refusal says it.
pub const REACHES: &str = "a backup credential reaches only its own backups: GET/POST /v1/volumes, \
     DELETE /v1/volumes/{id} (or /api/v1/volumes/{id}), POST /v1/volumes/{id}/attach|detach, \
     GET/POST /v1/group-snapshots, GET/DELETE /v1/group-snapshots/{id}, POST /v1/snapshots/{id}/ship, \
     GET/DELETE /v1/ship-jobs/{id}";

fn answer(path: &str, code: StatusCode, msg: &str) -> Response {
    let v1 = path == "/v1" || path.starts_with("/v1/");
    let body = if v1 {
        let c = match code {
            StatusCode::FORBIDDEN => "forbidden",
            StatusCode::CONFLICT => "conflict",
            StatusCode::BAD_REQUEST => "invalid_argument",
            _ => "error",
        };
        json!({ "code": c, "message": msg })
    } else {
        json!({ "error": msg, "code": code.as_u16() })
    };
    (code, Json(body)).into_response()
}

fn is_mine(state: &AppState, who: &str, key: &str) -> bool {
    state.creators.lock().unwrap_or_else(|e| e.into_inner()).get(key).is_some_and(|c| c.who == who)
}

fn record(state: &AppState, who: &str, key: String) {
    state
        .creators
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .set(key, super::creators::Creator::new(who, BTreeMap::new()));
}

/// The engine volume a `/v1` volume key (id or name) or an engine key is.
async fn engine_volume(state: &AppState, key: &str) -> Option<uuid::Uuid> {
    {
        let v1 = state.v1.lock().await;
        if let Some(r) = v1.volumes.get(key) {
            return r.local_id;
        }
        if let Some(r) = v1.volumes.values().find(|r| r.vol.name == key) {
            return r.local_id;
        }
    }
    state.volume_manager.lock().await.find_volume(key).await.map(|v| v.0)
}

/// The engine volume a `/v1` snapshot (or volume, or engine key) is.
async fn engine_snapshot(state: &AppState, key: &str) -> Option<uuid::Uuid> {
    if let Some(s) = state.v1.lock().await.snapshots.get(key) {
        return s.local_id;
    }
    engine_volume(state, key).await
}

/// May `who` make this call? `Err((status, why))` when not.
async fn check(state: &AppState, who: &str, verb: &Verb, body: &[u8]) -> Result<(), (StatusCode, String)> {
    let not_yours =
        |what: &str| -> Result<(), (StatusCode, String)> { Err((StatusCode::FORBIDDEN, format!("{what} is not {who}'s: {REACHES}"))) };
    let mine_volume = |e: uuid::Uuid| is_mine(state, who, &super::creators::volume(&e));
    match verb {
        Verb::GetVolume(k) | Verb::DeleteVolume(k) | Verb::Detach(k) | Verb::Attach(k) => {
            // An object that is not here is the handler's 404.
            if let Some(e) = engine_volume(state, k).await {
                if !mine_volume(e) {
                    return not_yours(&format!("volume {k}"));
                }
            }
            if let Verb::Attach(_) = verb {
                let v: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
                if v["transport"].as_str() != Some("nvme_tcp") {
                    return Err((
                        StatusCode::FORBIDDEN,
                        format!("{who} attaches a backup volume over nvme_tcp only, for its node's ship"),
                    ));
                }
            }
            Ok(())
        }
        Verb::CreateVolume => {
            let v: serde_json::Value = serde_json::from_slice(body)
                .map_err(|e| (StatusCode::BAD_REQUEST, format!("the create's body is not JSON: {e}")))?;
            if !v["source"].is_null() {
                return Err((StatusCode::FORBIDDEN, format!("{who} makes blank volumes only: no `source`")));
            }
            if v["secret"].as_bool() == Some(true) {
                return Err((StatusCode::FORBIDDEN, format!("{who} makes no secret volume")));
            }
            if v["owner"]["kind"].as_str() != Some(OWNER_KIND) {
                return Err((
                    StatusCode::FORBIDDEN,
                    format!("{who} makes backup volumes only: `owner.kind` must be {OWNER_KIND}"),
                ));
            }
            let name = v["name"].as_str().unwrap_or_default();
            if let Some(e) = engine_volume(state, name).await {
                if !mine_volume(e) {
                    return Err((StatusCode::CONFLICT, format!("volume {name} is here and is not {who}'s")));
                }
            }
            Ok(())
        }
        Verb::CreatePoint => {
            let v: serde_json::Value = serde_json::from_slice(body)
                .map_err(|e| (StatusCode::BAD_REQUEST, format!("the point's body is not JSON: {e}")))?;
            let name = v["name"].as_str().unwrap_or_default().to_string();
            let existing = state.v1.lock().await.group_snapshots.values().find(|g| g.name == name).map(|g| g.id.clone());
            if let Some(id) = existing {
                if !is_mine(state, who, &group_key(&id)) {
                    return Err((StatusCode::CONFLICT, format!("group snapshot {name} is here and is not {who}'s")));
                }
            }
            for m in v["volume_ids"].as_array().into_iter().flatten() {
                let k = m.as_str().unwrap_or_default();
                match engine_volume(state, k).await {
                    Some(e) if mine_volume(e) => {}
                    _ => return not_yours(&format!("volume {k}")),
                }
            }
            Ok(())
        }
        Verb::ListPoints => Ok(()),
        Verb::GetPoint(id) | Verb::DeletePoint(id) => {
            if state.v1.lock().await.group_snapshots.contains_key(id) && !is_mine(state, who, &group_key(id)) {
                return not_yours(&format!("group snapshot {id}"));
            }
            Ok(())
        }
        Verb::Ship(k) => match engine_snapshot(state, k).await {
            Some(e) if !mine_volume(e) => not_yours(&format!("snapshot {k}")),
            _ => Ok(()),
        },
        Verb::GetJob(id) | Verb::CancelJob(id) => match crate::mgmt::ship::get(state, id) {
            Some(j) if !mine_volume(j.snapshot_volume) => not_yours(&format!("ship job {id}")),
            _ => Ok(()),
        },
    }
}

/// After a call that succeeded: record what it made, filter what it lists.
async fn after(state: &AppState, who: &str, verb: &Verb, resp: Response) -> Response {
    if !resp.status().is_success() || !matches!(verb, Verb::CreateVolume | Verb::CreatePoint | Verb::ListPoints) {
        return resp;
    }
    let (parts, b) = resp.into_parts();
    let bytes = match axum::body::to_bytes(b, 64 << 20).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("{who}: reading the reply: {e}");
            return Response::from_parts(parts, Body::empty());
        }
    };
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    match verb {
        Verb::CreateVolume => {
            let id = v["id"].as_str().unwrap_or_default().to_string();
            match engine_volume(state, &id).await {
                Some(e) => record(state, who, super::creators::volume(&e)),
                None => tracing::warn!("{who}: volume {id} made with no data here: not recorded as its"),
            }
            Response::from_parts(parts, Body::from(bytes))
        }
        Verb::CreatePoint => {
            if let Some(id) = v["id"].as_str() {
                record(state, who, group_key(id));
            }
            for s in v["snapshots"].as_array().into_iter().flatten() {
                if let Some(e) = engine_snapshot(state, s["id"].as_str().unwrap_or_default()).await {
                    record(state, who, super::creators::volume(&e));
                }
            }
            Response::from_parts(parts, Body::from(bytes))
        }
        Verb::ListPoints => {
            if let Some(a) = v.as_array_mut() {
                a.retain(|g| is_mine(state, who, &group_key(g["id"].as_str().unwrap_or_default())));
            }
            let body = serde_json::to_vec(&v).unwrap_or_default();
            let mut parts = parts;
            parts.headers.remove(axum::http::header::CONTENT_LENGTH);
            Response::from_parts(parts, Body::from(body))
        }
        _ => Response::from_parts(parts, Body::from(bytes)),
    }
}

/// A call made with `node`'s backup bearer (`mgmt::auth::require_token`).
pub async fn serve(state: &Arc<AppState>, node: String, req: Request, next: Next) -> Response {
    let who = principal(&node);
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let mut rec = super::kubeauth::AuditRecord {
        at: now(),
        who: who.clone(),
        method: method.to_string(),
        path: path.clone(),
        resource: "backup".into(),
        verb: method.as_str().to_ascii_lowercase(),
        target: None,
        decision: String::new(),
        reason: None,
        status: None,
    };
    state.backup_creds.lock().unwrap_or_else(|e| e.into_inner()).used(&node);
    let refuse = |mut rec: super::kubeauth::AuditRecord, code: StatusCode, why: String| {
        rec.decision = "refused".into();
        rec.reason = Some(why.clone());
        rec.status = Some(code.as_u16());
        super::kubeauth::audit(state.audit_log.as_deref(), &rec);
        metrics::counter!("storm_backup_credential_requests_total", "node" => node.clone(), "result" => "refused").increment(1);
        tracing::warn!("{who}: refused {method} {path}: {why}");
        answer(&path, code, &why)
    };
    let Some(verb) = verb_of(&method, &path) else {
        return refuse(rec, StatusCode::FORBIDDEN, REACHES.to_string());
    };
    let (req, body) = if verb.reads_body() {
        let (parts, b) = req.into_parts();
        match axum::body::to_bytes(b, BODY_LIMIT).await {
            Ok(bytes) => (Request::from_parts(parts, Body::from(bytes.clone())), bytes),
            Err(e) => return refuse(rec, StatusCode::BAD_REQUEST, format!("the body: {e}")),
        }
    } else {
        (req, axum::body::Bytes::new())
    };
    if let Err((code, why)) = check(state, &who, &verb, &body).await {
        return refuse(rec, code, why);
    }
    let mut req = req;
    req.extensions_mut().insert(super::auth::Caller(who.clone()));
    let resp = next.run(req).await;
    let resp = after(state, &who, &verb, resp).await;
    rec.decision = "allowed".into();
    rec.status = Some(resp.status().as_u16());
    super::kubeauth::audit(state.audit_log.as_deref(), &rec);
    metrics::counter!("storm_backup_credential_requests_total", "node" => node.clone(), "result" => "allowed").increment(1);
    tracing::info!("{who}: {method} {path} -> {}", resp.status().as_u16());
    resp
}

#[derive(Deserialize)]
struct MintRequest {
    node: String,
}

/// `POST /api/v1/backup-credentials {node}` (the admin's): the bearer, once.
async fn mint(
    State(state): State<Arc<AppState>>,
    caller: Option<axum::Extension<super::auth::Caller>>,
    Json(req): Json<MintRequest>,
) -> Response {
    let by = caller.map(|c| c.0 .0.clone()).unwrap_or_else(|| "open".into());
    let r = state.backup_creds.lock().unwrap_or_else(|e| e.into_inner()).mint(&req.node, &by);
    match r {
        Ok((token, replaced)) => {
            tracing::info!(
                "backup credential {} for node {} by {by} (#693)",
                if replaced { "minted again (the old one no longer works)" } else { "minted" },
                req.node
            );
            (
                StatusCode::CREATED,
                Json(json!({
                    "node": req.node,
                    "principal": principal(&req.node),
                    "token": token,
                    "replaced": replaced,
                })),
            )
                .into_response()
        }
        Err(e) if e.contains("DNS") => answer("/api", StatusCode::BAD_REQUEST, &e),
        Err(e) => answer("/api", StatusCode::CONFLICT, &e),
    }
}

/// `GET /api/v1/backup-credentials`: who holds one, and when it was last used.
async fn list(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let entries: Vec<(String, Entry)> = state
        .backup_creds
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entries()
        .iter()
        .map(|(n, e)| (n.clone(), e.clone()))
        .collect();
    let cr = state.creators.lock().unwrap_or_else(|e| e.into_inner());
    let holders: Vec<Holder> = entries
        .into_iter()
        .map(|(node, e)| Holder {
            principal: principal(&node),
            objects: cr.count_by(&principal(&node)),
            node,
            created_at: e.created_at,
            created_by: e.created_by,
            last_used_at: e.last_used_at,
            uses: e.uses,
        })
        .collect();
    Json(json!({ "items": holders }))
}

/// `DELETE /api/v1/backup-credentials/{node}` (the admin's): revoked at once.
/// The node's backups stay.
async fn revoke(State(state): State<Arc<AppState>>, UrlPath(node): UrlPath<String>) -> Response {
    let r = state.backup_creds.lock().unwrap_or_else(|e| e.into_inner()).revoke(&node);
    match r {
        Ok(true) => {
            tracing::info!("backup credential for node {node} revoked (#693); its backups stay");
            Json(json!({ "node": node, "revoked": true })).into_response()
        }
        Ok(false) => answer("/api", StatusCode::NOT_FOUND, &format!("node {node} holds no backup credential")),
        Err(e) => answer("/api", StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list).post(mint))
        .route("/{node}", delete(revoke))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_verb_list_is_the_backups_and_nothing_else() {
        let m = |s: &str| Method::from_bytes(s.as_bytes()).unwrap();
        let yes = [
            ("GET", "/v1/volumes/default.vm.disk.backup", Verb::GetVolume("default.vm.disk.backup".into())),
            ("POST", "/v1/volumes", Verb::CreateVolume),
            ("DELETE", "/v1/volumes/v1", Verb::DeleteVolume("v1".into())),
            ("DELETE", "/api/v1/volumes/v1", Verb::DeleteVolume("v1".into())),
            ("POST", "/v1/volumes/v1/attach", Verb::Attach("v1".into())),
            ("POST", "/v1/volumes/v1/detach", Verb::Detach("v1".into())),
            ("GET", "/v1/group-snapshots", Verb::ListPoints),
            ("POST", "/v1/group-snapshots/", Verb::CreatePoint),
            ("GET", "/v1/group-snapshots/g1", Verb::GetPoint("g1".into())),
            ("DELETE", "/v1/group-snapshots/g1", Verb::DeletePoint("g1".into())),
            ("POST", "/v1/snapshots/s1/ship", Verb::Ship("s1".into())),
            ("GET", "/v1/ship-jobs/j1", Verb::GetJob("j1".into())),
            ("DELETE", "/v1/ship-jobs/j1", Verb::CancelJob("j1".into())),
        ];
        for (meth, path, want) in yes {
            assert_eq!(verb_of(&m(meth), path), Some(want), "{meth} {path}");
        }
        for (meth, path) in [
            ("GET", "/v1/volumes"),
            ("GET", "/api/v1/volumes"),
            ("GET", "/api/v1/volumes/v1"),
            ("POST", "/api/v1/volumes"),
            ("POST", "/api/v1/volumes/v1/clone"),
            ("PUT", "/api/v1/synonyms/boothost/server8"),
            ("POST", "/v1/volumes/v1/expand"),
            ("POST", "/v1/volumes/v1/promote"),
            ("POST", "/v1/snapshots"),
            ("GET", "/v1/snapshots/s1/changes"),
            ("POST", "/api/v1/backup-credentials"),
            ("DELETE", "/api/v1/backup-credentials/server8"),
            ("GET", "/api/v1/backup-credentials"),
            ("GET", "/metrics"),
            ("DELETE", "/v1/volumes/"),
        ] {
            assert_eq!(verb_of(&m(meth), path), None, "{meth} {path} must not be reached");
        }
    }

    #[test]
    fn a_credential_is_minted_once_kept_as_a_hash_rotated_and_revoked() {
        let d = tempfile::tempdir().unwrap();
        let mut c = BackupCreds::load(Some(d.path()));
        assert!(c.mint("Server8", "admin-token").unwrap_err().contains("DNS"));
        let (t1, replaced) = c.mint("server8", "admin-token").unwrap();
        assert!(!replaced && t1.starts_with("sbbk-"));
        let (t2, _) = c.mint("server9.g16.lo", "admin-token").unwrap();
        assert_eq!(c.holder(&t1).as_deref(), Some("server8"));
        assert_eq!(c.holder(&t2).as_deref(), Some("server9.g16.lo"));
        assert_eq!(c.holder("sbbk-nope"), None);
        let file = std::fs::read_to_string(d.path().join(FILE)).unwrap();
        assert!(!file.contains(&t1) && file.contains(&sha256_hex(&t1)), "only the hash is kept: {file}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(d.path().join(FILE)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        c.used("server8");
        let again = BackupCreds::load(Some(d.path()));
        assert_eq!(again.holder(&t1).as_deref(), Some("server8"), "kept across a restart");
        assert_eq!(again.entries()["server8"].uses, 1);
        // Minted again: the old bearer no longer works.
        let (t3, replaced) = c.mint("server8", "admin-token").unwrap();
        assert!(replaced);
        assert_eq!(c.holder(&t1), None);
        assert_eq!(c.holder(&t3).as_deref(), Some("server8"));
        assert!(c.revoke("server8").unwrap());
        assert!(!c.revoke("server8").unwrap());
        assert_eq!(c.holder(&t3), None);
        assert_eq!(BackupCreds::load(Some(d.path())).holder(&t3), None, "revoked across a restart");
        assert!(BackupCreds::load(None).mint("server8", "a").unwrap_err().contains("data directory"));
    }
}
