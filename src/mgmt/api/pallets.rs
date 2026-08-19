//! `/api/v1/pallets` — the pallet lifecycle over REST (#51, #52).
//!
//! The store is built from the drives the node already has open, on every
//! request. That is deliberate: pallet state lives in the GPT and in each
//! pallet's superblock, never in a cache here, so there is nothing that can
//! disagree with the disk — including after a drive is moved between nodes.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{ApiError, ListResponse};
use crate::mgmt::AppState;
use crate::pallet::format::{MemberKind, PalletKind};
use crate::pallet::manager::{PublishSpec, RecomposeSpec};
use crate::pallet::{
    BytesContent, MemberSpec, PalletBrowser, PalletError, PalletLocation, PalletManager,
    PalletStore,
};

// ------------------------------------------------------------------ helpers

async fn store(state: &AppState) -> PalletStore {
    let mut s = PalletStore::default();
    for d in state.drives.read().await.iter() {
        s.add_drive(d.path.clone(), d.device.clone());
    }
    s
}

async fn manager(state: &AppState) -> PalletManager {
    PalletManager::new(store(state).await)
}

fn err(e: PalletError) -> Response {
    match e {
        PalletError::NotFound(_) => ApiError::not_found(e.to_string()),
        PalletError::Refused(_) | PalletError::Overlaps { .. } => ApiError::conflict(e.to_string()),
        PalletError::NoSpace { .. } => ApiError::conflict(e.to_string()),
        PalletError::TooLong { .. } | PalletError::BadGeometry(_) => {
            ApiError::bad_request(e.to_string())
        }
        other => ApiError::internal(other.to_string()),
    }
}

#[derive(Debug, Deserialize)]
pub struct KindQuery {
    /// `boot`, `system`, `kernel`, `kube`, `app`, `runtime`, `data`.
    pub kind: Option<String>,
}

impl KindQuery {
    fn parsed(&self) -> Option<PalletKind> {
        self.kind.as_deref().map(PalletKind::parse)
    }
}

#[derive(Debug, Deserialize)]
pub struct ForceQuery {
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Serialize)]
pub struct PalletResponse {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub version: u64,
    pub version_label: String,
    pub drive: String,
    pub drive_index: usize,
    /// Absent for a whole-drive pallet, which has no GPT entry.
    pub partition: Option<usize>,
    pub whole_drive: bool,
    pub start_bytes: u64,
    pub size_bytes: u64,
    pub used_bytes: u64,
    pub member_count: usize,
    pub priority: u8,
    pub tries_left: u8,
    pub successful: bool,
    pub sealed: bool,
    pub read_only: bool,
    pub readable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unreadable_reason: Option<String>,
}

impl From<&PalletLocation> for PalletResponse {
    fn from(l: &PalletLocation) -> Self {
        let reason = match &l.state {
            crate::pallet::PalletState::Unreadable { reason } => Some(reason.clone()),
            crate::pallet::PalletState::Readable => None,
        };
        PalletResponse {
            id: l.id.to_string(),
            name: l.name.clone(),
            kind: l.kind.to_string(),
            version: l.version,
            version_label: l.version_label.clone(),
            drive: l.drive.clone(),
            drive_index: l.drive_index,
            partition: (!l.is_whole_drive()).then_some(l.entry_index),
            whole_drive: l.is_whole_drive(),
            start_bytes: l.start_bytes,
            size_bytes: l.size_bytes,
            used_bytes: l.used_bytes,
            member_count: l.member_count,
            priority: l.attributes.priority,
            tries_left: l.attributes.tries_left,
            successful: l.attributes.successful,
            sealed: l.attributes.sealed,
            read_only: l.attributes.read_only,
            readable: l.is_readable(),
            unreadable_reason: reason,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct MemberResponse {
    pub name: String,
    pub role: String,
    pub kind: String,
    pub byte_len: u64,
    pub digest: String,
    pub sealed: bool,
    pub read_only: bool,
}

#[derive(Debug, Serialize)]
pub struct PalletDetail {
    #[serde(flatten)]
    pub pallet: PalletResponse,
    pub members: Vec<MemberResponse>,
}

// -------------------------------------------------------------------- reads

async fn list(State(state): State<Arc<AppState>>, Query(q): Query<KindQuery>) -> Response {
    let kind = q.parsed();
    let items: Vec<PalletResponse> = manager(&state)
        .await
        .list()
        .await
        .iter()
        .filter(|p| kind.is_none() || Some(p.kind) == kind)
        .map(PalletResponse::from)
        .collect();
    let count = items.len();
    Json(ListResponse { items, count }).into_response()
}

/// What is selected, what could take over, and what will not be used.
async fn status(State(state): State<Arc<AppState>>, Query(q): Query<KindQuery>) -> Response {
    #[derive(Serialize)]
    struct Failed {
        #[serde(flatten)]
        pallet: PalletResponse,
        reason: String,
    }
    #[derive(Serialize)]
    struct StatusResponse {
        active: Option<PalletResponse>,
        available: Vec<PalletResponse>,
        failed: Vec<Failed>,
    }

    let s = manager(&state).await.status(q.parsed()).await;
    Json(StatusResponse {
        active: s.active.as_ref().map(PalletResponse::from),
        available: s.available.iter().map(PalletResponse::from).collect(),
        failed: s
            .failed
            .iter()
            .map(|f| Failed {
                pallet: PalletResponse::from(&f.location),
                reason: f.reason.clone(),
            })
            .collect(),
    })
    .into_response()
}

/// The order a boot-time consumer would try them in — the read-only half of
/// the policy, answerable without touching a thing.
async fn chain(State(state): State<Arc<AppState>>, Query(q): Query<KindQuery>) -> Response {
    let browser = PalletBrowser::new(store(&state).await);
    let items: Vec<PalletResponse> =
        browser.chain(q.parsed()).await.iter().map(PalletResponse::from).collect();
    let count = items.len();
    Json(ListResponse { items, count }).into_response()
}

async fn get_one(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let Ok(id) = Uuid::parse_str(&id) else {
        return ApiError::bad_request("pallet id must be a UUID");
    };
    let mgr = manager(&state).await;
    let loc = match mgr.get(id).await {
        Ok(l) => l,
        Err(e) => return err(e),
    };
    let members = match mgr.store().open(&loc).await {
        Ok(p) => p
            .members()
            .iter()
            .map(|m| MemberResponse {
                name: m.name.clone(),
                role: m.role.clone(),
                kind: m.kind.to_string(),
                byte_len: m.byte_len,
                digest: m.digest_hex(),
                sealed: m.is_sealed(),
                read_only: m.is_read_only(),
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    Json(PalletDetail { pallet: PalletResponse::from(&loc), members }).into_response()
}

async fn verify(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let Ok(id) = Uuid::parse_str(&id) else {
        return ApiError::bad_request("pallet id must be a UUID");
    };
    match manager(&state).await.verify(id).await {
        Ok(r) => Json(r).into_response(),
        Err(e) => err(e),
    }
}

// ------------------------------------------------------------------- writes

#[derive(Debug, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum MemberSource {
    /// A volume in this engine — a sealed golden clone, read straight out of
    /// the GEM with nothing staged in between.
    Volume { volume_id: String, byte_len: Option<u64> },
    /// A file on the node.
    File { path: String },
    /// Content in the request, base64. For small members: a kernel command
    /// line, a boot config.
    Inline { base64: String },
}

#[derive(Debug, Deserialize)]
pub struct MemberRequest {
    pub name: String,
    pub role: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(flatten)]
    pub source: MemberSource,
}

#[derive(Debug, Deserialize)]
pub struct PublishRequest {
    pub name: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub version: Option<u64>,
    #[serde(default)]
    pub version_label: Option<String>,
    pub members: Vec<MemberRequest>,
    /// Drive path or index. Defaults to the first drive with room.
    #[serde(default)]
    pub drive: Option<String>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    #[serde(default = "yes")]
    pub read_only: bool,
    #[serde(default = "yes")]
    pub sealed: bool,
    #[serde(default)]
    pub activate: bool,
    #[serde(default)]
    pub tries: Option<u8>,
}

fn yes() -> bool {
    true
}

async fn build_members(
    state: &AppState,
    reqs: Vec<MemberRequest>,
) -> Result<Vec<MemberSpec>, Response> {
    let mut out = Vec::with_capacity(reqs.len());
    for r in reqs {
        let kind = r.kind.as_deref().map(MemberKind::parse).unwrap_or(MemberKind::Raw);
        let spec = match r.source {
            MemberSource::Volume { volume_id, byte_len } => {
                let Ok(vid) = Uuid::parse_str(&volume_id) else {
                    return Err(ApiError::bad_request("volume_id must be a UUID"));
                };
                let vm = state.volume_manager.lock().await;
                let Some(dev) = vm.get_volume(&crate::volume::VolumeId(vid)) else {
                    return Err(ApiError::not_found(format!("volume {vid} not found")));
                };
                let len = byte_len.unwrap_or_else(|| dev.capacity_bytes());
                crate::pallet::manager::volume_member(r.name, r.role, kind, dev, len)
            }
            MemberSource::File { path } => {
                match crate::pallet::manager::file_member(r.name, r.role, kind, path).await {
                    Ok(s) => s,
                    Err(e) => return Err(err(e)),
                }
            }
            MemberSource::Inline { base64 } => {
                use base64::Engine;
                let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&base64) else {
                    return Err(ApiError::bad_request("inline content is not valid base64"));
                };
                MemberSpec::new(r.name, r.role, kind, Arc::new(BytesContent(bytes)))
            }
        };
        out.push(spec);
    }
    Ok(out)
}

async fn publish(State(state): State<Arc<AppState>>, Json(req): Json<PublishRequest>) -> Response {
    let mgr = manager(&state).await;
    let drive = match req.drive.as_deref() {
        Some(d) => match mgr.store().drive_index_of(d) {
            Ok(i) => Some(i),
            Err(e) => return err(e),
        },
        None => None,
    };
    let members = match build_members(&state, req.members).await {
        Ok(m) => m,
        Err(r) => return r,
    };

    let mut spec = PublishSpec::new(
        req.name,
        req.kind.as_deref().map(PalletKind::parse).unwrap_or(PalletKind::Unspecified),
    );
    spec.version = req.version;
    spec.version_label = req.version_label.unwrap_or_default();
    spec.members = members;
    spec.drive = drive;
    spec.size_bytes = req.size_bytes;
    spec.read_only = req.read_only;
    spec.sealed = req.sealed;
    spec.activate = req.activate;
    if let Some(t) = req.tries {
        spec.tries = t;
    }

    match mgr.publish(spec).await {
        Ok(loc) => (axum::http::StatusCode::CREATED, Json(PalletResponse::from(&loc))).into_response(),
        Err(e) => err(e),
    }
}

#[derive(Debug, Deserialize)]
pub struct RecomposeRequest {
    #[serde(default)]
    pub add: Vec<MemberRequest>,
    #[serde(default)]
    pub remove: Vec<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub version: Option<u64>,
    #[serde(default)]
    pub version_label: Option<String>,
    #[serde(default)]
    pub drive: Option<String>,
    #[serde(default)]
    pub activate: bool,
}

async fn recompose(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<RecomposeRequest>,
) -> Response {
    let Ok(id) = Uuid::parse_str(&id) else {
        return ApiError::bad_request("pallet id must be a UUID");
    };
    let mgr = manager(&state).await;
    let drive = match req.drive.as_deref() {
        Some(d) => match mgr.store().drive_index_of(d) {
            Ok(i) => Some(i),
            Err(e) => return err(e),
        },
        None => None,
    };
    let add = match build_members(&state, req.add).await {
        Ok(m) => m,
        Err(r) => return r,
    };
    let spec = RecomposeSpec {
        add,
        remove: req.remove,
        version: req.version,
        version_label: req.version_label,
        kind: req.kind.as_deref().map(PalletKind::parse),
        name: req.name,
        drive,
        size_bytes: None,
        activate: req.activate,
    };
    match mgr.recompose(id, spec).await {
        Ok(loc) => (axum::http::StatusCode::CREATED, Json(PalletResponse::from(&loc))).into_response(),
        Err(e) => err(e),
    }
}

macro_rules! with_id {
    ($id:expr) => {
        match Uuid::parse_str(&$id) {
            Ok(v) => v,
            Err(_) => return ApiError::bad_request("pallet id must be a UUID"),
        }
    };
}

async fn activate(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let id = with_id!(id);
    match manager(&state).await.activate(id).await {
        Ok(loc) => Json(PalletResponse::from(&loc)).into_response(),
        Err(e) => err(e),
    }
}

async fn mark_successful(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let id = with_id!(id);
    match manager(&state).await.mark_successful(id).await {
        Ok(loc) => Json(PalletResponse::from(&loc)).into_response(),
        Err(e) => err(e),
    }
}

async fn rollback(State(state): State<Arc<AppState>>, Query(q): Query<KindQuery>) -> Response {
    match manager(&state).await.rollback(q.parsed()).await {
        Ok(loc) => Json(PalletResponse::from(&loc)).into_response(),
        Err(e) => err(e),
    }
}

#[derive(Debug, Deserialize)]
pub struct FlagRequest {
    pub value: bool,
    #[serde(default)]
    pub force: bool,
}

async fn set_read_only(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<FlagRequest>,
) -> Response {
    let id = with_id!(id);
    match manager(&state).await.set_read_only(id, req.value, req.force).await {
        Ok(loc) => Json(PalletResponse::from(&loc)).into_response(),
        Err(e) => err(e),
    }
}

async fn set_sealed(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<FlagRequest>,
) -> Response {
    let id = with_id!(id);
    match manager(&state).await.set_sealed(id, req.value).await {
        Ok(loc) => Json(PalletResponse::from(&loc)).into_response(),
        Err(e) => err(e),
    }
}

#[derive(Debug, Deserialize)]
pub struct DriveRequest {
    /// Destination drive, by path or index.
    pub drive: String,
}

async fn move_pallet(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<DriveRequest>,
) -> Response {
    let id = with_id!(id);
    let mgr = manager(&state).await;
    let dest = match mgr.store().drive_index_of(&req.drive) {
        Ok(i) => i,
        Err(e) => return err(e),
    };
    match mgr.move_pallet(id, dest).await {
        Ok(loc) => Json(PalletResponse::from(&loc)).into_response(),
        Err(e) => err(e),
    }
}

async fn copy_pallet(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<DriveRequest>,
) -> Response {
    let id = with_id!(id);
    let mgr = manager(&state).await;
    let dest = match mgr.store().drive_index_of(&req.drive) {
        Ok(i) => i,
        Err(e) => return err(e),
    };
    match mgr.copy_pallet(id, dest).await {
        Ok(loc) => Json(PalletResponse::from(&loc)).into_response(),
        Err(e) => err(e),
    }
}

#[derive(Debug, Deserialize)]
pub struct MemberMoveRequest {
    /// Destination pallet.
    pub into: String,
    #[serde(default)]
    pub activate: bool,
}

async fn move_member(
    State(state): State<Arc<AppState>>,
    Path((id, member)): Path<(String, String)>,
    Json(req): Json<MemberMoveRequest>,
) -> Response {
    let id = with_id!(id);
    let into = with_id!(req.into);
    #[derive(Serialize)]
    struct MovedResponse {
        destination: PalletResponse,
        source: PalletResponse,
    }
    match manager(&state).await.move_member(id, &member, into, req.activate).await {
        Ok((dest, src)) => Json(MovedResponse {
            destination: PalletResponse::from(&dest),
            source: PalletResponse::from(&src),
        })
        .into_response(),
        Err(e) => err(e),
    }
}

async fn copy_member(
    State(state): State<Arc<AppState>>,
    Path((id, member)): Path<(String, String)>,
    Json(req): Json<MemberMoveRequest>,
) -> Response {
    let id = with_id!(id);
    let into = with_id!(req.into);
    match manager(&state).await.copy_member(id, &member, into, req.activate).await {
        Ok(loc) => Json(PalletResponse::from(&loc)).into_response(),
        Err(e) => err(e),
    }
}

async fn delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<ForceQuery>,
) -> Response {
    let id = with_id!(id);
    match manager(&state).await.delete(id, q.force).await {
        Ok(loc) => Json(PalletResponse::from(&loc)).into_response(),
        Err(e) => err(e),
    }
}

#[derive(Debug, Deserialize)]
pub struct PruneRequest {
    pub name: String,
    #[serde(default = "keep_default")]
    pub keep: usize,
}

fn keep_default() -> usize {
    2
}

async fn prune(State(state): State<Arc<AppState>>, Json(req): Json<PruneRequest>) -> Response {
    match manager(&state).await.prune(&req.name, req.keep).await {
        Ok(removed) => {
            let items: Vec<PalletResponse> = removed.iter().map(PalletResponse::from).collect();
            let count = items.len();
            Json(ListResponse { items, count }).into_response()
        }
        Err(e) => err(e),
    }
}

#[derive(Debug, Deserialize)]
pub struct InitRequest {
    /// Drive to write a fresh GPT onto, by path or index.
    pub drive: String,
    #[serde(default)]
    pub force: bool,
}

async fn init_gpt(State(state): State<Arc<AppState>>, Json(req): Json<InitRequest>) -> Response {
    let mgr = manager(&state).await;
    let idx = match mgr.store().drive_index_of(&req.drive) {
        Ok(i) => i,
        Err(e) => return err(e),
    };
    match mgr.init_gpt(idx, req.force).await {
        Ok(()) => Json(serde_json::json!({ "drive": req.drive, "gpt": "initialized" }))
            .into_response(),
        Err(e) => err(e),
    }
}

#[derive(Debug, Deserialize)]
pub struct AdoptRequest {
    pub from: String,
    pub to: String,
}

/// Migrate a pre-subdivision whole-drive pallet onto a partitioned drive.
async fn adopt(State(state): State<Arc<AppState>>, Json(req): Json<AdoptRequest>) -> Response {
    let mgr = manager(&state).await;
    let (from, to) = match (
        mgr.store().drive_index_of(&req.from),
        mgr.store().drive_index_of(&req.to),
    ) {
        (Ok(f), Ok(t)) => (f, t),
        (Err(e), _) | (_, Err(e)) => return err(e),
    };
    match mgr.adopt_whole_drive(from, to).await {
        Ok(loc) => (axum::http::StatusCode::CREATED, Json(PalletResponse::from(&loc))).into_response(),
        Err(e) => err(e),
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list).post(publish))
        // Verbs before "/{id}": none of these are pallet ids.
        .route("/status", get(status))
        .route("/chain", get(chain))
        .route("/rollback", post(rollback))
        .route("/prune", post(prune))
        .route("/gpt", post(init_gpt))
        .route("/adopt", post(adopt))
        .route("/{id}", get(get_one).delete(delete))
        .route("/{id}/verify", post(verify))
        .route("/{id}/activate", post(activate))
        .route("/{id}/successful", post(mark_successful))
        .route("/{id}/read-only", post(set_read_only))
        .route("/{id}/sealed", post(set_sealed))
        .route("/{id}/move", post(move_pallet))
        .route("/{id}/copy", post(copy_pallet))
        .route("/{id}/recompose", post(recompose))
        .route("/{id}/members/{member}/move", post(move_member))
        .route("/{id}/members/{member}/copy", post(copy_member))
        .with_state(state)
}
