//! The mk management surface: `/mk/v1`, plus the bearer-auth gate that now
//! covers the *whole* management port.
//!
//! Authentication (issue #6). The engine's own router only guards `/v1`;
//! `/api/v1/{drives,arrays,volumes,exports,slabs,luns}` has no auth at all,
//! and this profile deliberately binds the container IP so sbregistry and
//! mkube can reach it. That left every volume behind a container rootfs or a
//! PVC creatable, snapshottable and deletable by anything on the bridge. mk
//! therefore composes the engine's routers itself instead of calling
//! `start_management_server`, and wraps the lot in one bearer gate — with an
//! optional second token required for the destructive verbs.
//!
//! Everything else here is the consumer-facing half of issues #2, #3, #4 and
//! #7: exact attach parameters, trim and readiness. Filesystem templates
//! moved into the engine in stormblock v8.1.0 and are served on this same
//! port at `/api/v1/fstemplates`, behind this same gate.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{
        header::{AUTHORIZATION, CONTENT_TYPE},
        HeaderValue, Method, StatusCode,
    },
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use futures_util::{StreamExt, TryStreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::drive::BlockDevice;
use crate::mgmt::{ExportEntry, ExportProtocol, ExportStatus};
use crate::fs::template::{self, CloneSpec};
use crate::volume::{VolumeId, DEFAULT_EXTENT_SIZE};

use super::ctx::ServeContext;
use super::netstat;
use super::tarfs;
use super::trim;
use super::wiring::{WireProto, WireState};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// `Debug` so a test may `.unwrap()` an `MkResult`. Without it every
/// `parse_protocol(...).unwrap()` in the test module fails to compile — which
/// is exactly what happened to the tests added in v0.3.0, a version that was
/// tagged but never built.
#[derive(Debug)]
pub struct MkError(StatusCode, String);

impl MkError {
    fn bad(msg: impl Into<String>) -> Self {
        MkError(StatusCode::BAD_REQUEST, msg.into())
    }
    fn not_found(msg: impl Into<String>) -> Self {
        MkError(StatusCode::NOT_FOUND, msg.into())
    }
    fn conflict(msg: impl Into<String>) -> Self {
        MkError(StatusCode::CONFLICT, msg.into())
    }
    fn internal(msg: impl Into<String>) -> Self {
        MkError(StatusCode::INTERNAL_SERVER_ERROR, msg.into())
    }
}

impl IntoResponse for MkError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1, "code": self.0.as_u16() }))).into_response()
    }
}

impl From<anyhow::Error> for MkError {
    fn from(e: anyhow::Error) -> Self {
        MkError::internal(e.to_string())
    }
}

type MkResult = Result<Response, MkError>;

fn ok(v: Value) -> MkResult {
    Ok(Json(v).into_response())
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// Accepted for everything. `None` means authentication is disabled
    /// (`STORMBLOCKMK_INSECURE=1`).
    pub api_token: Option<String>,
    /// When set, destructive verbs require THIS token; the api token is not
    /// enough. When unset, the api token covers them.
    pub admin_token: Option<String>,
}

/// Endpoints reachable without a token: liveness and readiness only, so a
/// supervisor probe never needs a credential.
fn is_public(path: &str) -> bool {
    matches!(
        path,
        "/serve/v1/ready" | "/serve/v1/health" | "/mk/v1/ready" | "/mk/v1/health"
    )
}

/// Verbs that can destroy data. Everything that deletes, plus the ones that
/// mutate irreversibly: applying a trim, sealing a template, collecting
/// extents.
///
/// The two reclaim endpoints spell their preview mode oppositely, so each is
/// read on its own terms: mk's `trim` reports unless `apply` is given, while
/// the engine's `POST /api/v1/slabs/gc` frees unless `dry_run` is given. Both
/// are classified by what the request will actually *do*.
fn is_destructive(method: &Method, path: &str, query: Option<&str>) -> bool {
    if *method == Method::DELETE {
        return true;
    }
    if path.ends_with("/seal") {
        return true;
    }
    if *method == Method::POST && path.ends_with("/tar") {
        // `POST /mk/v1/volumes/{id}/tar` unpacks an archive straight into a
        // volume's filesystem — it overwrites whatever an entry's path already
        // names, and with `whiteouts=true` it *deletes* by design. `GET` on the
        // same path only reads the tree out and stays on the ordinary token.
        return true;
    }
    if *method == Method::POST && path.ends_with("/files") {
        // The engine's `POST /api/v1/volumes/{id}/files` writes into a
        // volume's filesystem in userspace — no mount, no loop device, no
        // export, so none of the guards that normally stand between a caller
        // and a filesystem's contents apply. A file already at that path is
        // replaced. `GET` on the same path reads a file or lists a directory
        // and stays on the ordinary token.
        return true;
    }
    let q = query.unwrap_or("");
    let has = |key: &str| {
        q.split('&').any(|kv| kv == key || kv.starts_with(&format!("{key}=")))
    };
    if path.ends_with("/gc") {
        // Frees by default — only an explicit dry run is non-destructive.
        return !q.split('&').any(|kv| kv == "dry_run=true" || kv == "dry_run=1" || kv == "dry_run");
    }
    if path.ends_with("/trim") {
        // Any mention of `apply` counts: erring towards "needs the admin
        // token" is free, erring the other way discards blocks.
        return has("apply");
    }
    if path.ends_with("/fsck") {
        // The engine's `POST /api/v1/volumes/{id}/fsck` (v8.2.0) is read-only
        // until `repair=true`, which rewrites filesystem metadata in place.
        return has("repair");
    }
    false
}

fn bearer(req: &Request) -> Option<&str> {
    req.headers().get(AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")
}

pub async fn require_token(
    State(auth): State<Arc<AuthConfig>>,
    req: Request,
    next: Next,
) -> Response {
    let Some(expected) = auth.api_token.as_deref() else {
        return next.run(req).await; // explicit insecure mode
    };
    let path = req.uri().path().to_string();
    if is_public(&path) {
        return next.run(req).await;
    }

    let presented = bearer(&req).map(|s| s.to_string());
    let destructive = is_destructive(req.method(), &path, req.uri().query());
    let accepted: Vec<&str> = match (&auth.admin_token, destructive) {
        // A distinct admin token, on a destructive verb: only that token.
        (Some(admin), true) => vec![admin.as_str()],
        (Some(admin), false) => vec![expected, admin.as_str()],
        (None, _) => vec![expected],
    };

    match presented.as_deref() {
        Some(t) if accepted.iter().any(|a| *a == t) => next.run(req).await,
        _ => {
            tracing::warn!("unauthorized {} {}", req.method(), path);
            let msg = if destructive && auth.admin_token.is_some() {
                "admin token required for destructive operations"
            } else {
                "missing or invalid bearer token"
            };
            (StatusCode::UNAUTHORIZED, Json(json!({ "error": msg, "code": 401 })))
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// The serving API.
///
/// Mounted at `/serve/v1`. The old `/mk/v1` prefix is served alongside it and
/// is deprecated: "mk" named the RouterOS profile, and this surface is no
/// longer part of it — a stormos node serves exactly these routes. Both are
/// registered from one route table, so the two prefixes cannot drift; drop
/// the alias once mkube and sbregistry have moved.
pub fn router(ctx: Arc<ServeContext>) -> Router {
    routes(SERVE_PREFIX).merge(routes(LEGACY_PREFIX)).with_state(ctx)
}

/// Where the serving API lives.
pub const SERVE_PREFIX: &str = "/serve/v1";
/// The RouterOS-era prefix, kept until every consumer has moved.
pub const LEGACY_PREFIX: &str = "/mk/v1";

/// Kept for the profile that still calls it by its old name.
pub fn mk_router(ctx: Arc<ServeContext>) -> Router {
    router(ctx)
}

fn routes(p: &str) -> Router<Arc<ServeContext>> {
    Router::new()
        .route(&format!("{p}/health"), get(health))
        .route(&format!("{p}/ready"), get(ready))
        .route(&format!("{p}/status"), get(status))
        .route(&format!("{p}/exports"), get(list_exports).post(create_export))
        .route(&format!("{p}/exports/{{id}}"), get(get_export).delete(delete_export))
        .route(&format!("{p}/volumes"), get(list_volumes).post(create_volume))
        .route(&format!("{p}/volumes/{{id}}"), delete(delete_volume))
        .route(&format!("{p}/volumes/{{id}}/trim"), post(trim_volume))
        // Container layers are hundreds of megabytes and axum's default body
        // cap is 2 MiB. Nothing is buffered here — the body is consumed as a
        // stream — so the cap has no memory to protect and would only reject
        // every real layer.
        .route(
            &format!("{p}/volumes/{{id}}/tar"),
            get(pack_volume).post(unpack_volume).layer(DefaultBodyLimit::disable()),
        )
        // A pre-built filesystem image, written in as blocks. Same reason the
        // body limit goes: a golden is streamed, never buffered.
        .route(
            &format!("{p}/volumes/{{id}}/raw"),
            post(write_raw_volume).layer(DefaultBodyLimit::disable()),
        )
}

fn flag(q: &HashMap<String, String>, key: &str) -> bool {
    q.get(key).map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "")).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Health, readiness, status  (issue #7)
// ---------------------------------------------------------------------------

async fn health() -> Response {
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") })).into_response()
}

/// 200 only when a consumer attaching right now would get what it expects —
/// slab open, metadata restored, targets listening, every persisted export
/// wired. 503 with the list of blockers otherwise.
async fn ready(State(ctx): State<Arc<ServeContext>>) -> Response {
    let body = ctx.status.json();
    let code =
        if ctx.status.ready() { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (code, Json(body)).into_response()
}

async fn status(State(ctx): State<Arc<ServeContext>>) -> MkResult {
    super::reconcile::refresh_counters(&ctx).await;
    let exports: Vec<Value> = {
        let w = ctx.wiring.lock().await;
        w.exports.iter().map(|r| ctx.wiring_json(r)).collect()
    };
    // Templates are the engine's since stormblock v8.1.0 — reported here so
    // one GET still describes the whole instance, but managed through
    // `/api/v1/fstemplates` on this same port.
    let templates: Vec<Value> =
        ctx.state.fstemplates.lock().await.templates.iter().map(|t| t.json()).collect();
    let mut body = ctx.status.json();
    if let Some(obj) = body.as_object_mut() {
        obj.insert("version".into(), json!(env!("CARGO_PKG_VERSION")));
        obj.insert("advertise_addr".into(), json!(ctx.cfg.advertise_addr));
        obj.insert("default_protocol".into(), json!(WireProto::default().api_name()));
        obj.insert("shared_nqn".into(), json!(ctx.cfg.nqn));
        // Only mention the shared iSCSI target when there is one to connect to.
        if ctx.cfg.iscsi_enabled {
            obj.insert("shared_iqn".into(), json!(ctx.cfg.iqn));
        }
        obj.insert("exports".into(), json!(exports));
        obj.insert("fstemplates".into(), json!(templates));
    }
    ok(body)
}

// ---------------------------------------------------------------------------
// Exports  (issues #1, #2, #7)
// ---------------------------------------------------------------------------

async fn list_exports(State(ctx): State<Arc<ServeContext>>) -> MkResult {
    let w = ctx.wiring.lock().await;
    let items: Vec<Value> = w.exports.iter().map(|r| ctx.wiring_json(r)).collect();
    let count = items.len();
    ok(json!({ "items": items, "count": count }))
}

async fn get_export(State(ctx): State<Arc<ServeContext>>, Path(id): Path<String>) -> MkResult {
    let id = id.parse::<Uuid>().map_err(|_| MkError::bad(format!("invalid uuid {id}")))?;
    let w = ctx.wiring.lock().await;
    match w.get(&id) {
        Some(row) => ok(ctx.wiring_json(row)),
        None => Err(MkError::not_found(format!("export {id}"))),
    }
}

/// Map an API protocol name onto a transport.
///
/// **The default is NVMe-TCP**: attach is sub-second where iSCSI measures in
/// seconds, and it is the only transport this profile serves unless
/// `STORMBLOCKMK_ENABLE_ISCSI` is set. `nvme-tcp` is the canonical name (what
/// mkube sends and what attach blocks report); `nvmeof` is accepted as an
/// alias because wiring.json and the engine's export table persist that
/// spelling.
///
/// Asking for iSCSI while the stack is off is a 400, not a silently
/// substituted NVMe export and not a row that sits `pending` forever: the
/// caller is about to hand an initiator an IQN that will never answer.
fn parse_protocol(p: Option<&str>, iscsi_enabled: bool) -> Result<WireProto, MkError> {
    match p {
        None | Some("nvme-tcp") | Some("nvmeof") => Ok(WireProto::Nvmeof),
        Some("iscsi") if iscsi_enabled => Ok(WireProto::Iscsi),
        Some("iscsi") => Err(MkError::bad(
            "iSCSI is not served by this instance — use protocol \"nvme-tcp\" (the default), \
             or start stormblockmk with STORMBLOCKMK_ENABLE_ISCSI=1",
        )),
        Some(other) => Err(MkError::bad(format!(
            "unknown protocol \"{other}\" — expected \"nvme-tcp\" or \"iscsi\""
        ))),
    }
}

#[derive(Debug, Deserialize)]
struct CreateExportRequest {
    volume_id: Uuid,
    /// Export protocol: "nvme-tcp" (default) or "iscsi" (only when the legacy
    /// stack is enabled).
    #[serde(default)]
    protocol: Option<String>,
    /// Delete the volume once the export is withdrawn and drained. Set this
    /// for per-instance clones so they are actually garbage-collected.
    #[serde(default)]
    ephemeral: bool,
}

async fn create_export(
    State(ctx): State<Arc<ServeContext>>,
    Json(req): Json<CreateExportRequest>,
) -> MkResult {
    let proto = parse_protocol(req.protocol.as_deref(), ctx.cfg.iscsi_enabled)?;
    let attach = export_volume(&ctx, req.volume_id, proto, req.ephemeral).await?;
    Ok((StatusCode::CREATED, Json(attach)).into_response())
}

/// Declare an export and pin its wiring in one step, so the LUN id and portal
/// exist before the caller is told about them.
async fn export_volume(
    ctx: &Arc<ServeContext>,
    volume_id: Uuid,
    proto: WireProto,
    ephemeral: bool,
) -> Result<Value, MkError> {
    {
        let vm = ctx.state.volume_manager.lock().await;
        if vm.get_volume(&VolumeId(volume_id)).is_none() {
            return Err(MkError::not_found(format!("volume {volume_id}")));
        }
    }

    // The wiring row and the export entry must become visible together: a
    // reconciler pass that saw the row without the entry would treat it as an
    // orphan and start draining it. Holding the wiring lock across both is
    // what makes that impossible — the reconciler reads the export table
    // under the same lock.
    let export_id = Uuid::new_v4();
    let row = {
        let mut w = ctx.wiring.lock().await;
        let row = w
            .insert(
                export_id,
                volume_id,
                proto,
                None, // mk-declared: the reconciler attaches and records the id
                &ctx.cfg.iqn_prefix,
                &ctx.cfg.nqn_prefix,
                ctx.cfg.portal_base,
                ctx.cfg.portal_span,
                ephemeral,
            )
            .map_err(|e| MkError::conflict(e.to_string()))?;
        let mut ex = ctx.state.exports.write().await;
        ex.push(ExportEntry {
            id: export_id,
            volume_id,
            protocol: match proto {
                WireProto::Iscsi => ExportProtocol::Iscsi,
                WireProto::Nvmeof => ExportProtocol::Nvmeof,
            },
            target_id: match proto {
                WireProto::Iscsi => row.iqn.clone(),
                WireProto::Nvmeof => row.nqn.clone().unwrap_or_default(),
            },
            status: ExportStatus::PendingRestart,
            lun_id: None,
            // The reconciler wires an NVMe export as namespace 1 of its own
            // subsystem — record that here so the entry matches what a
            // consumer will find.
            nsid: match proto {
                WireProto::Iscsi => None,
                WireProto::Nvmeof => Some(1),
            },
        });
        drop(ex);
        w.persist()?;
        row
    };
    ctx.persist_exports().await?;

    // Wire it now rather than at the next tick, so the attach parameters in
    // the response are usable the moment the caller receives them.
    if let Err(e) = super::reconcile::pass(ctx).await {
        tracing::warn!("immediate reconcile after export {export_id}: {e}");
    }

    let w = ctx.wiring.lock().await;
    let row = w.get(&export_id).cloned().unwrap_or(row);
    Ok(ctx.wiring_json(&row))
}

async fn delete_export(
    State(ctx): State<Arc<ServeContext>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> MkResult {
    let id = id.parse::<Uuid>().map_err(|_| MkError::bad(format!("invalid uuid {id}")))?;

    if flag(&q, "delete_volume") {
        let mut w = ctx.wiring.lock().await;
        if let Some(row) = w.get_mut(&id) {
            row.ephemeral = true;
        }
        w.persist()?;
    }

    let removed = {
        let mut ex = ctx.state.exports.write().await;
        let before = ex.len();
        ex.retain(|e| e.id != id);
        before != ex.len()
    };
    if !removed && ctx.wiring.lock().await.get(&id).is_none() {
        return Err(MkError::not_found(format!("export {id}")));
    }
    ctx.persist_exports().await?;

    // Teardown is ordered: the reconciler moves the export to `draining` and
    // only pulls the LUN once the initiator has gone (or the grace period
    // expires), so 202, not 204.
    if let Err(e) = super::reconcile::pass(&ctx).await {
        tracing::warn!("immediate reconcile after delete of export {id}: {e}");
    }
    let state = ctx
        .wiring
        .lock()
        .await
        .get(&id)
        .map(|r| r.state.as_str())
        .unwrap_or("withdrawn");
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "export_id": id,
            "state": state,
            "drain_grace_secs": ctx.cfg.drain_grace_secs,
        })),
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// Volumes  (issues #3, #4, #7)
// ---------------------------------------------------------------------------

async fn list_volumes(State(ctx): State<Arc<ServeContext>>) -> MkResult {
    let vols = { ctx.state.volume_manager.lock().await.list_volumes().await };
    let w = ctx.wiring.lock().await;
    let items: Vec<Value> = vols
        .iter()
        .map(|(id, name, virtual_size, allocated)| {
            let row = w.exports.iter().find(|r| r.volume_id == id.0);
            json!({
                "id": id.0,
                "name": name,
                "virtual_bytes": virtual_size,
                "allocated_bytes": allocated,
                "slot_size": DEFAULT_EXTENT_SIZE,
                "export": row.map(|r| ctx.wiring_json(r)),
            })
        })
        .collect();
    let count = items.len();
    ok(json!({ "items": items, "count": count }))
}

#[derive(Debug, Deserialize)]
struct CreateVolumeRequest {
    name: String,
    #[serde(default)]
    size_bytes: Option<u64>,
    /// Template id or name to clone from — the mkfs-once path.
    #[serde(default)]
    from_template: Option<String>,
    /// Export the new volume immediately and return attach parameters.
    #[serde(default)]
    export: bool,
    /// Export protocol: "nvme-tcp" (default) or "iscsi" (only when the legacy
    /// stack is enabled). Only meaningful with `export: true`.
    #[serde(default)]
    protocol: Option<String>,
    /// Delete the volume when its export is withdrawn.
    #[serde(default)]
    ephemeral: bool,
}

async fn create_volume(
    State(ctx): State<Arc<ServeContext>>,
    Json(req): Json<CreateVolumeRequest>,
) -> MkResult {
    // Validate before creating anything: a bad or unserved protocol must 400
    // without leaving an orphan volume behind.
    let proto = parse_protocol(req.protocol.as_deref(), ctx.cfg.iscsi_enabled)?;
    let volume_id = match &req.from_template {
        // The engine owns cloning as of stormblock v8.1.0. Going through it
        // rather than snapshotting the sealed volume here is what gets every
        // clone its own filesystem UUID (stormblockmk#12): clones of one
        // template used to be byte-identical, so two on a single host collided
        // on mount-by-UUID and in the blkid cache. The stamp has to live in the
        // engine because every consumer clones *through* it — one applied in
        // this layer would miss the clones that never come through this layer.
        Some(key) => {
            let spec = CloneSpec {
                name: req.name.clone(),
                size_bytes: req.size_bytes,
                ..CloneSpec::new(req.name.clone())
            };
            // The locks go in un-held: v8.2.0 takes the volume-manager and
            // template-store mutexes itself, in short windows, so formats and
            // clones no longer queue behind one another. Handing it guards
            // would serialise exactly what that change set out to unblock.
            // Before-picture for the failed-clone cleanup below. Cheap: one
            // listing of ids under a lock mk takes for the clone anyway.
            let before = super::reap::volume_ids(&ctx).await;

            let outcome = template::clone_template(
                &ctx.state.volume_manager,
                &ctx.state.fstemplates,
                key,
                &spec,
            )
            .await;

            let cloned = match outcome {
                Ok(c) => c,
                Err(e) => {
                    // The engine discards a clone that fails verify — but with
                    // `let _ = delete_volume(...)`, so a delete that itself
                    // fails leaves the volume behind while the error says it
                    // was discarded. Anything left carrying the name we asked
                    // for is ours to clean up, and nothing else will: a clone
                    // wears the consumer's name, so the background template
                    // reaper deliberately never considers it.
                    let removed = super::reap::sweep_failed_clone(&ctx, &before, &req.name).await;
                    if !removed.is_empty() {
                        tracing::warn!(
                            "clone of {key} as {} failed and left {} volume(s) behind; removed them",
                            req.name,
                            removed.len()
                        );
                    }
                    return Err(match e {
                        template::TemplateError::NotFound(_) => {
                            MkError::not_found(format!("fstemplate {key}"))
                        }
                        // Cloning an unsealed template, or one with descendants
                        // — the caller's sequencing, not a server fault.
                        template::TemplateError::Conflict(m) => MkError::conflict(m),
                        template::TemplateError::Invalid(m) => MkError::bad(m),
                        other => MkError::internal(other.to_string()),
                    });
                }
            };
            cloned.volume_id.0
        }
        None => {
            let size = req
                .size_bytes
                .ok_or_else(|| MkError::bad("size_bytes is required without from_template"))?;
            if size == 0 {
                return Err(MkError::bad("size_bytes must be > 0"));
            }
            let mut vm = ctx.state.volume_manager.lock().await;
            vm.create_volume_any(&req.name, size)
                .await
                .map_err(|e| MkError::internal(format!("creating volume: {e}")))?
                .0
        }
    };

    let mut body = json!({
        "id": volume_id,
        "name": req.name,
        "from_template": req.from_template,
    });
    if req.export {
        let attach = export_volume(&ctx, volume_id, proto, req.ephemeral).await?;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("export".into(), attach);
        }
    }
    Ok((StatusCode::CREATED, Json(body)).into_response())
}

async fn delete_volume(
    State(ctx): State<Arc<ServeContext>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> MkResult {
    let id = id.parse::<Uuid>().map_err(|_| MkError::bad(format!("invalid uuid {id}")))?;
    let force = flag(&q, "force");

    // Never delete a volume out from under a wired export: that is exactly
    // the "disk vanished underneath the consumer" failure ordered teardown
    // exists to prevent. Withdraw the export first.
    if let Some(row) = ctx.wiring.lock().await.exports.iter().find(|r| r.volume_id == id) {
        if !force {
            return Err(MkError::conflict(format!(
                "volume {id} is exported ({}, state {}) — DELETE the export first, or pass force=true",
                row.export_id,
                row.state.as_str()
            )));
        }
    }

    let mut vm = ctx.state.volume_manager.lock().await;
    vm.delete_volume(VolumeId(id))
        .await
        .map_err(|e| MkError::not_found(format!("deleting volume {id}: {e}")))?;
    drop(vm);

    // The volume is gone, so any export naming it must go too. Leaving the
    // entry behind strands its wiring row `Pending` forever — un-wireable,
    // never cleaned up, and holding readiness down permanently (#15). Only
    // reachable via `force`, since the guard above refuses otherwise.
    let withdrawn = ctx.withdraw_exports_for_volume(id).await;
    if !withdrawn.is_empty() {
        ctx.persist_exports().await?;
        tracing::warn!(
            "volume {id} force-deleted with {} export(s) still declared — withdrew {:?};              the reconciler will drain and drop their wiring rows",
            withdrawn.len(),
            withdrawn
        );
    }
    ok(json!({ "deleted": id, "exports_withdrawn": withdrawn }))
}

/// Refuse to write to a volume an initiator is currently holding.
///
/// Two operations need this and for the same reason: the filesystem's on-disk
/// state is only authoritative while nothing else is changing it. A trim reads
/// free-block bitmaps that a live mount is actively editing; an unpack edits
/// the very structures a live mount has cached and will write back over. Both
/// corrupt the filesystem, silently, some time after the request returns 200.
///
/// An unreadable `/proc/net/tcp` is treated as "sessions may exist" — the
/// cheap wrong answer here destroys data.
async fn no_live_sessions(
    ctx: &Arc<ServeContext>,
    volume_id: Uuid,
    verb: &str,
) -> Result<(), MkError> {
    let row = ctx.wiring.lock().await.exports.iter().find(|r| r.volume_id == volume_id).cloned();
    let Some(row) = row else { return Ok(()) };
    if row.state != WireState::Active {
        return Ok(());
    }
    match netstat::established_on_port(row.portal_port) {
        Some(0) => Ok(()),
        Some(n) => Err(MkError::conflict(format!(
            "volume {volume_id} has {n} attached session(s) on port {} — detach before {verb}",
            row.portal_port
        ))),
        None => Err(MkError::conflict(format!(
            "cannot determine attached sessions (/proc/net/tcp unreadable) — refusing {verb}"
        ))),
    }
}

/// Resolve a volume's block device, or 404.
async fn volume_device(
    ctx: &Arc<ServeContext>,
    volume_id: Uuid,
) -> Result<Arc<dyn BlockDevice>, MkError> {
    let dev = { ctx.state.volume_manager.lock().await.get_volume(&VolumeId(volume_id)) };
    dev.ok_or_else(|| MkError::not_found(format!("volume {volume_id}")))
}

/// Thin allocation for a volume right now, 0 if it has gone. The volume-manager
/// guard is released before the handle is asked, so the count is never taken
/// while the manager is locked.
async fn allocated_bytes(ctx: &Arc<ServeContext>, volume_id: Uuid) -> u64 {
    let handle = { ctx.state.volume_manager.lock().await.get_volume_handle(&VolumeId(volume_id)) };
    match handle {
        Some(h) => h.allocated().await,
        None => 0,
    }
}

/// Unpack a tar archive into a volume's filesystem — no mount, no loop device,
/// no export, no initiator.
///
/// Query: `into` (default `/`), `compression` (`auto` | `none` | `gzip`,
/// default `auto`), `whiteouts` (OCI layer semantics, default off), `force`
/// (skip the live-session guard).
///
/// The body is the archive and is never buffered: it is handed to the walker
/// as a stream, so a multi-hundred-megabyte layer costs a pipe, not RAM.
/// The unit an image is streamed and zero-tested in.
///
/// Thin volumes allocate in slots — 4 MiB on this engine — so testing finer
/// than that saves I/O but not space. A megabyte is small enough that four
/// consecutive zero units leave a slot unallocated, and large enough that a
/// 256 MiB image is 256 writes rather than 65,536.
const RAW_UNIT: usize = 1024 * 1024;

/// Write a pre-built filesystem image straight into a volume.
///
/// # Why this exists alongside `/tar`
///
/// `/tar` unpacks an archive into a filesystem that is already there: it
/// formats nothing and it walks every entry. A golden built on a build box is
/// not an archive — it is a finished filesystem — and putting it on a volume
/// is a block copy. Doing it through `/tar` would mean formatting on the
/// device and then writing the files back one at a time, which is the work
/// the build box already did.
///
/// That is what lets an appliance ship with its goldens pre-made: the device
/// imports bytes it can verify and seals them, with no mkfs, no unpack and no
/// network.
///
/// # Sparse by default
///
/// A golden is mostly holes — 25 MB of program in a 64 MiB filesystem — and a
/// thin volume that is written end to end allocates every slot for zeros
/// nobody will read. Zero units are skipped unless `sparse=0`, so the volume
/// allocates what the image *contains* rather than what it spans. An
/// unwritten slot already reads back as zeros, which is the same thing the
/// formatter relies on.
///
/// # Verifying
///
/// `sha256=<hex>` is checked over the whole stream as it is written, and a
/// mismatch is an error *after* the bytes have landed — deliberately. The
/// volume is left as it is rather than half-reverted, and the caller decides
/// whether to rewrite or discard it; silently keeping an image whose digest
/// did not match is the one option not on offer.
async fn write_raw_volume(
    State(ctx): State<Arc<ServeContext>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    body: Body,
) -> MkResult {
    use sha2::{Digest, Sha256};

    let id = id.parse::<Uuid>().map_err(|_| MkError::bad(format!("invalid uuid {id}")))?;
    if !flag(&q, "force") {
        no_live_sessions(&ctx, id, "writing an image into it").await?;
    }
    let dev = volume_device(&ctx, id).await?;
    let capacity = dev.capacity_bytes();
    let sector = dev.block_size() as usize;
    let sparse = !matches!(q.get("sparse").map(|s| s.as_str()), Some("0") | Some("false"));
    let expect = q.get("sha256").map(|s| s.trim().to_ascii_lowercase());

    let allocated_before = allocated_bytes(&ctx, id).await;

    let mut stream = body.into_data_stream();
    let mut unit = vec![0u8; RAW_UNIT];
    let mut filled = 0usize;
    let mut offset = 0u64;
    let mut written = 0u64;
    let mut skipped = 0u64;
    let mut hasher = Sha256::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| MkError::bad(format!("reading the image: {e}")))?;
        hasher.update(&chunk);
        let mut rest: &[u8] = &chunk;
        while !rest.is_empty() {
            let take = (RAW_UNIT - filled).min(rest.len());
            unit[filled..filled + take].copy_from_slice(&rest[..take]);
            filled += take;
            rest = &rest[take..];
            if filled == RAW_UNIT {
                put_unit(&dev, offset, &unit, capacity, sparse, &mut written, &mut skipped).await?;
                offset += RAW_UNIT as u64;
                filled = 0;
            }
        }
    }

    // The tail, padded to a whole sector: a device will not take less, and the
    // volume already reads as zeros where the padding goes.
    if filled > 0 {
        let end = filled.div_ceil(sector) * sector;
        unit[filled..end].fill(0);
        put_unit(&dev, offset, &unit[..end], capacity, sparse, &mut written, &mut skipped).await?;
        offset += filled as u64;
    }

    dev.flush().await.map_err(|e| MkError::internal(format!("flushing volume {id}: {e}")))?;
    let allocated_after = allocated_bytes(&ctx, id).await;
    super::reconcile::refresh_counters(&ctx).await;

    let digest = format!("sha256:{}", hex::encode(hasher.finalize()));
    tracing::info!(
        "raw import into volume {id}: {offset} byte(s), {written} written, {skipped} skipped as \
         holes; allocation {allocated_before} -> {allocated_after}; {digest}"
    );

    if let Some(want) = expect {
        let want = want.strip_prefix("sha256:").unwrap_or(&want).to_string();
        let got = digest.trim_start_matches("sha256:");
        if want != got {
            return Err(MkError::bad(format!(
                "image digest sha256:{got} does not match the expected sha256:{want} — \
                 {offset} byte(s) were written and the volume is NOT trustworthy; rewrite it \
                 or discard it"
            )));
        }
    }

    ok(json!({
        "volume_id": id,
        "bytes": offset,
        "written": written,
        "skipped": skipped,
        "sparse": sparse,
        "sha256": digest,
        "allocated_before": allocated_before,
        "allocated_after": allocated_after,
    }))
}

/// Write one unit, or skip it when it is a hole.
async fn put_unit(
    dev: &Arc<dyn BlockDevice>,
    offset: u64,
    buf: &[u8],
    capacity: u64,
    sparse: bool,
    written: &mut u64,
    skipped: &mut u64,
) -> Result<(), MkError> {
    if offset + buf.len() as u64 > capacity {
        return Err(MkError::bad(format!(
            "image is larger than volume: {} bytes would write past a {capacity}-byte volume",
            offset + buf.len() as u64
        )));
    }
    if sparse && buf.iter().all(|&b| b == 0) {
        *skipped += buf.len() as u64;
        return Ok(());
    }
    dev.write(offset, buf)
        .await
        .map_err(|e| MkError::internal(format!("writing at {offset}: {e}")))?;
    *written += buf.len() as u64;
    Ok(())
}

async fn unpack_volume(
    State(ctx): State<Arc<ServeContext>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    body: Body,
) -> MkResult {
    let id = id.parse::<Uuid>().map_err(|_| MkError::bad(format!("invalid uuid {id}")))?;
    let compression = tarfs::parse_compression(q.get("compression").map(|s| s.as_str()))
        .map_err(|e| MkError::bad(e))?;
    let into = q.get("into").map(|s| s.as_str()).unwrap_or("/").to_string();
    let whiteouts = flag(&q, "whiteouts");

    if !flag(&q, "force") {
        no_live_sessions(&ctx, id, "unpacking into it").await?;
    }
    let dev = volume_device(&ctx, id).await?;

    // `axum::Error` is not an `io::Error`, so the body stream is re-typed
    // before it can be read as one. Boxing it pinned is what makes the reader
    // `Unpin`, which the walker requires.
    let stream = body.into_data_stream().map_err(|e| std::io::Error::other(e));
    let reader = tokio_util::io::StreamReader::new(Box::pin(stream));

    let allocated_before = allocated_bytes(&ctx, id).await;

    // A filesystem that will not open is the caller's sequencing (an unformatted
    // volume, or one cloned from a template that never sealed), not a server
    // fault — 400 rather than 500.
    let report = tarfs::unpack(&dev, reader, &into, compression, whiteouts)
        .await
        .map_err(|e| MkError::bad(e.to_string()))?;

    let allocated_after = allocated_bytes(&ctx, id).await;

    tracing::info!(
        "unpack volume {id} into {into} (whiteouts={whiteouts}): {} file(s), {} dir(s), {} byte(s), {} removed; allocation {} -> {}",
        report.files,
        report.directories,
        report.bytes,
        report.removed,
        allocated_before,
        allocated_after,
    );
    super::reconcile::refresh_counters(&ctx).await;

    let mut out = tarfs::unpack_json(&report);
    if let Some(obj) = out.as_object_mut() {
        obj.insert("volume_id".into(), json!(id));
        obj.insert("into".into(), json!(into));
        obj.insert("whiteouts".into(), json!(whiteouts));
        obj.insert("allocated_before".into(), json!(allocated_before));
        obj.insert("allocated_after".into(), json!(allocated_after));
    }
    ok(out)
}

/// Pack a subtree of a volume's filesystem out as a tar archive.
///
/// Query: `from` (default `/`), `compression` (`none` | `gzip`, default
/// `none`). Streamed — the archive is produced as the response is read, so
/// neither end holds the whole thing.
async fn pack_volume(
    State(ctx): State<Arc<ServeContext>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> MkResult {
    let id = id.parse::<Uuid>().map_err(|_| MkError::bad(format!("invalid uuid {id}")))?;
    let compression = tarfs::parse_compression(q.get("compression").map(|s| s.as_str()))
        .map_err(|e| MkError::bad(e))?;
    let from = q.get("from").map(|s| s.as_str()).unwrap_or("/").to_string();

    let dev = volume_device(&ctx, id).await?;
    let reader = tarfs::pack_stream(&dev, id, &from, compression)
        .await
        .map_err(|e| MkError::bad(e.to_string()))?;

    let mut resp =
        Body::from_stream(tokio_util::io::ReaderStream::new(reader)).into_response();
    resp.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static(tarfs::content_type(compression)),
    );
    Ok(resp)
}

/// Offline trim. Reports by default; `apply=true` actually discards.
async fn trim_volume(
    State(ctx): State<Arc<ServeContext>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> MkResult {
    let id = id.parse::<Uuid>().map_err(|_| MkError::bad(format!("invalid uuid {id}")))?;
    let apply = flag(&q, "apply");
    let force = flag(&q, "force");

    // Rule 1: never trim underneath a live session.
    if apply && !force {
        no_live_sessions(&ctx, id, "trimming").await?;
    }

    let (dev, handle) = {
        let vm = ctx.state.volume_manager.lock().await;
        (vm.get_volume(&VolumeId(id)), vm.get_volume_handle(&VolumeId(id)))
    };
    let (Some(dev), Some(handle)) = (dev, handle) else {
        return Err(MkError::not_found(format!("volume {id}")));
    };

    let before = handle.allocated().await;
    let mut report = trim::trim(&dev, DEFAULT_EXTENT_SIZE, before, apply, !force)
        .await
        .map_err(|e| MkError::bad(e.to_string()))?;
    report.allocated_after = handle.allocated().await;

    if report.applied {
        tracing::info!(
            "trim volume {id}: discarded {} bytes in {} slots, allocation {} -> {}",
            report.discarded_bytes,
            report.slots,
            report.allocated_before,
            report.allocated_after
        );
        super::reconcile::refresh_counters(&ctx).await;
    }
    ok(report.json())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Liveness and readiness must answer without a token under *both*
    /// prefixes. Miss the new one and every supervisor probe starts failing
    /// closed the moment a consumer moves over.
    #[test]
    fn probes_are_public_under_either_prefix() {
        for p in [SERVE_PREFIX, LEGACY_PREFIX] {
            assert!(is_public(&format!("{p}/ready")), "{p}/ready must be public");
            assert!(is_public(&format!("{p}/health")), "{p}/health must be public");
            assert!(
                !is_public(&format!("{p}/volumes")),
                "{p}/volumes must still need a token"
            );
        }
    }

    /// The alias exists to be removed. If someone changes the prefixes, this
    /// says out loud that two are still being served.
    #[test]
    fn the_legacy_prefix_is_still_served() {
        assert_eq!(SERVE_PREFIX, "/serve/v1");
        assert_eq!(LEGACY_PREFIX, "/mk/v1");
        assert_ne!(SERVE_PREFIX, LEGACY_PREFIX);
    }

    /// A device that records what was written where, so a test can tell the
    /// difference between "wrote zeros" and "left a hole".
    struct Recorder {
        id: crate::drive::DeviceId,
        capacity: u64,
        writes: std::sync::Mutex<Vec<(u64, usize)>>,
    }

    impl Recorder {
        fn new(capacity: u64) -> Arc<Self> {
            Arc::new(Recorder {
                id: crate::drive::DeviceId {
                    uuid: Uuid::new_v4(),
                    serial: "rec".into(),
                    model: "Recorder".into(),
                    path: "mem:rec".into(),
                },
                capacity,
                writes: std::sync::Mutex::new(Vec::new()),
            })
        }
        fn bytes_written(&self) -> usize {
            self.writes.lock().unwrap().iter().map(|(_, n)| n).sum()
        }
    }

    #[async_trait::async_trait]
    impl crate::drive::BlockDevice for Recorder {
        fn id(&self) -> &crate::drive::DeviceId {
            &self.id
        }
        fn capacity_bytes(&self) -> u64 {
            self.capacity
        }
        fn block_size(&self) -> u32 {
            4096
        }
        fn optimal_io_size(&self) -> u32 {
            4096
        }
        fn device_type(&self) -> crate::drive::DriveType {
            crate::drive::DriveType::NVMe
        }
        async fn read(&self, _o: u64, buf: &mut [u8]) -> crate::drive::DriveResult<usize> {
            buf.fill(0);
            Ok(buf.len())
        }
        async fn write(&self, offset: u64, buf: &[u8]) -> crate::drive::DriveResult<usize> {
            self.writes.lock().unwrap().push((offset, buf.len()));
            Ok(buf.len())
        }
        async fn flush(&self) -> crate::drive::DriveResult<()> {
            Ok(())
        }
        async fn discard(&self, _o: u64, _l: u64) -> crate::drive::DriveResult<()> {
            Ok(())
        }
    }

    /// A golden is mostly holes, and a thin volume that is written end to end
    /// allocates every slot for zeros nobody will read. This is the whole
    /// reason the import is sparse.
    #[tokio::test]
    async fn holes_are_skipped_not_written() {
        let rec = Recorder::new(64 * 1024 * 1024);
        let dev: Arc<dyn BlockDevice> = rec.clone();
        let (mut w, mut sk) = (0u64, 0u64);

        let zeros = vec![0u8; RAW_UNIT];
        put_unit(&dev, 0, &zeros, 64 << 20, true, &mut w, &mut sk).await.unwrap();
        let data = vec![7u8; RAW_UNIT];
        put_unit(&dev, RAW_UNIT as u64, &data, 64 << 20, true, &mut w, &mut sk).await.unwrap();

        assert_eq!(sk, RAW_UNIT as u64, "the zero unit should have been skipped");
        assert_eq!(w, RAW_UNIT as u64, "the data unit should have been written");

        assert_eq!(
            rec.bytes_written(),
            RAW_UNIT,
            "only the non-zero unit should have reached the device"
        );
        assert_eq!(
            rec.writes.lock().unwrap()[0].0,
            RAW_UNIT as u64,
            "and it should land at its own offset, not packed down"
        );
    }

    /// `sparse=0` is for a caller who means it: write the zeros too.
    #[tokio::test]
    async fn sparse_can_be_turned_off() {
        let dev: Arc<dyn BlockDevice> = Recorder::new(64 * 1024 * 1024);
        let (mut w, mut sk) = (0u64, 0u64);
        let zeros = vec![0u8; RAW_UNIT];
        put_unit(&dev, 0, &zeros, 64 << 20, false, &mut w, &mut sk).await.unwrap();
        assert_eq!(sk, 0);
        assert_eq!(w, RAW_UNIT as u64);
    }

    /// An image bigger than the volume is the caller's mistake, and it must be
    /// caught before the write rather than after it runs off the end.
    #[tokio::test]
    async fn an_image_larger_than_the_volume_is_refused() {
        let dev: Arc<dyn BlockDevice> = Recorder::new(RAW_UNIT as u64);
        let (mut w, mut sk) = (0u64, 0u64);
        let data = vec![1u8; RAW_UNIT];
        assert!(put_unit(&dev, 0, &data, RAW_UNIT as u64, true, &mut w, &mut sk).await.is_ok());
        assert!(
            put_unit(&dev, RAW_UNIT as u64, &data, RAW_UNIT as u64, true, &mut w, &mut sk)
                .await
                .is_err(),
            "writing past the end of the volume must be refused"
        );
    }

    #[test]
    fn destructive_verbs_are_classified() {
        assert!(is_destructive(&Method::DELETE, "/mk/v1/exports/abc", None));
        assert!(is_destructive(&Method::POST, "/api/v1/fstemplates/abc/seal", None));
        assert!(is_destructive(&Method::POST, "/mk/v1/volumes/abc/trim", Some("apply=true")));
        // The engine's collector frees by default; only a dry run is safe.
        assert!(is_destructive(&Method::POST, "/api/v1/slabs/gc", None));
        assert!(!is_destructive(&Method::POST, "/api/v1/slabs/gc", Some("dry_run=true")));
        // fsck reads until it is asked to repair.
        assert!(is_destructive(&Method::POST, "/api/v1/volumes/abc/fsck", Some("repair=true")));
        assert!(!is_destructive(&Method::POST, "/api/v1/volumes/abc/fsck", None));
        // Writing files into a volume's filesystem replaces what is there;
        // reading one back, or listing a directory, does not.
        assert!(is_destructive(&Method::POST, "/api/v1/volumes/abc/files", None));
        assert!(!is_destructive(&Method::GET, "/api/v1/volumes/abc/files", Some("path=/etc/hostname")));
        // Unpacking an archive overwrites, and with whiteouts it deletes.
        // Packing one out only reads.
        assert!(is_destructive(&Method::POST, "/mk/v1/volumes/abc/tar", None));
        assert!(is_destructive(&Method::POST, "/mk/v1/volumes/abc/tar", Some("whiteouts=true")));
        assert!(!is_destructive(&Method::GET, "/mk/v1/volumes/abc/tar", Some("from=/etc")));
        // The engine's orphan reclamation is a DELETE, so it is already
        // covered by the blanket DELETE rule; its GET inventory is not.
        assert!(is_destructive(&Method::DELETE, "/api/v1/fstemplates/orphans", None));
        assert!(!is_destructive(&Method::GET, "/api/v1/fstemplates/orphans", None));
        assert!(!is_destructive(&Method::POST, "/mk/v1/volumes/abc/trim", None));
        assert!(!is_destructive(&Method::POST, "/mk/v1/volumes/abc/trim", Some("force=true")));
        assert!(!is_destructive(&Method::GET, "/mk/v1/status", None));
        assert!(!is_destructive(&Method::POST, "/mk/v1/volumes", None));
    }

    #[test]
    fn protocol_names_parse() {
        // No protocol stated ⇒ NVMe-TCP, whether or not iSCSI is served.
        assert_eq!(parse_protocol(None, false).unwrap(), WireProto::Nvmeof);
        assert_eq!(parse_protocol(None, true).unwrap(), WireProto::Nvmeof);
        assert_eq!(parse_protocol(Some("nvme-tcp"), false).unwrap(), WireProto::Nvmeof);
        assert_eq!(parse_protocol(Some("nvmeof"), false).unwrap(), WireProto::Nvmeof);
        assert!(parse_protocol(Some("nvme"), false).is_err());
        assert!(parse_protocol(Some(""), false).is_err());
    }

    #[test]
    fn iscsi_is_refused_unless_the_stack_is_enabled() {
        let err = parse_protocol(Some("iscsi"), false).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("STORMBLOCKMK_ENABLE_ISCSI"), "{}", err.1);
        assert_eq!(parse_protocol(Some("iscsi"), true).unwrap(), WireProto::Iscsi);
    }

    #[test]
    fn only_probes_are_public() {
        assert!(is_public("/mk/v1/ready"));
        assert!(is_public("/mk/v1/health"));
        assert!(!is_public("/mk/v1/status"));
        assert!(!is_public("/api/v1/volumes"));
        assert!(!is_public("/metrics"));
    }
}
