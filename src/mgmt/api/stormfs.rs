//! `/api/v1/stormfs` — the data-path surface StormFS consumes (#49, #50).
//!
//! ```text
//! POST   /api/v1/stormfs/allocate          take chunks on a named tier
//! POST   /api/v1/stormfs/deallocate        free chunks and their addresses
//! POST   /api/v1/stormfs/trim              free the space, keep the addresses
//! GET    /api/v1/stormfs/extent-map/{vol}  what the block layer actually holds
//! POST   /api/v1/stormfs/commit            versioned compare-and-swap (#50)
//! GET    /api/v1/stormfs/pins              point-in-time holds
//! POST   /api/v1/stormfs/pins              take one
//! DELETE /api/v1/stormfs/pins/{id}         release one
//! ```
//!
//! `docs/stormblock-spec.md` §9.1 has listed these routes since v0.1 and
//! nothing implemented them; what `src/stormfs.rs` does is the opposite
//! direction, announcing this node's volumes to a StormFS metadata server.
//!
//! **Lock order is stormfs → volume manager**, everywhere, including
//! [`super::what_is_serving`]. Two of these handlers need both, and the pin
//! guard means the shared "what is using this volume" answer needs the pin
//! table, so the order has to be stated once rather than discovered.
//!
//! **The persist order inside a commit is load-bearing** and is the reason
//! this handler writes two files rather than one: versions first, then the
//! extent map. See `crate::volume::versioned` for why that way round and not
//! the other.

use std::path::Path as FsPath;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use super::ApiError;
use crate::drive::BlockDevice;
use crate::mgmt::AppState;
use crate::placement::topology::StorageTier;
use crate::volume::chunk::{self, AllocRequest, ChunkExtent, ChunkMap};
use crate::volume::versioned::{self, CommitError, CommitRequest, Pin, PinTable, StagedRange, VersionMap};
use crate::volume::VolumeId;

/// Everything the StormFS surface remembers across a restart.
///
/// One file, one atomic write: the chunk map and the version map are read
/// together by every commit, and splitting them would put a crash between two
/// halves of one answer.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct StormFsState {
    #[serde(default)]
    pub chunks: ChunkMap,
    #[serde(default)]
    pub versions: VersionMap,
    #[serde(default)]
    pub pins: PinTable,
}

impl StormFsState {
    /// Read the state back at startup. A corrupt file is refused loudly and
    /// replaced with an empty one: chunk ownership that reads as "nothing is
    /// owned" would hand a live chunk to a second caller, so it is better for
    /// the operator to see the error than for the node to quietly re-allocate.
    pub fn load(data_dir: &FsPath) -> Self {
        let path = data_dir.join("stormfs.json");
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match serde_json::from_str(&raw) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(
                    "corrupt {} ({e}) — starting with no StormFS chunk state. Chunks StormFS \
                     still holds are not recorded as owned and could be handed out again; \
                     reconcile with GET /api/v1/stormfs/extent-map/{{vol}} before writing.",
                    path.display()
                );
                Self::default()
            }
        }
    }
}

/// Write the StormFS state out. Best-effort at the file level, but callers
/// that also persist the extent map must call this **first** — see the module
/// docs.
async fn persist(state: &AppState, st: &StormFsState) {
    let Some(dir) = &state.data_dir else { return };
    let path = dir.join("stormfs.json");
    let Ok(bytes) = serde_json::to_vec_pretty(st) else {
        tracing::warn!("failed to serialize StormFS state");
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, bytes)
        .and_then(|_| std::fs::rename(&tmp, &path))
        .is_err()
    {
        tracing::warn!("failed to persist StormFS state to {}", path.display());
    }
}

fn parse_tier(s: &str) -> Option<StorageTier> {
    match s.to_lowercase().as_str() {
        "hot" => Some(StorageTier::Hot),
        "warm" => Some(StorageTier::Warm),
        "cool" => Some(StorageTier::Cool),
        "cold" => Some(StorageTier::Cold),
        _ => None,
    }
}

/// Slot size and virtual size of a volume, or a 404.
async fn volume_geometry(state: &AppState, volume: VolumeId) -> Result<(u64, u64), Response> {
    let vm = state.volume_manager.lock().await;
    let Some(handle) = vm.get_volume_handle(&volume) else {
        return Err(ApiError::not_found(format!("volume {} not found", volume.0)));
    };
    let virtual_size = handle.capacity_bytes();
    let slot_size = handle.lock().await.slot_size();
    Ok((slot_size, virtual_size))
}

// ---- allocate -----------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct AllocateRequest {
    /// `hot`, `warm`, `cool` or `cold`.
    #[serde(default = "default_tier")]
    pub tier: String,
    /// Chunk size in bytes. A whole number of slots.
    pub len: u64,
    /// How many chunks. Batched deliberately: a 1 GiB write at a 4 MiB chunk
    /// size is 256 allocations, and one round trip per chunk would put the
    /// round-trip count back in the data path this design exists to keep it
    /// out of.
    #[serde(default = "one")]
    pub count: usize,
    /// Which volume to carve from. Optional only once this node has already
    /// handed StormFS space somewhere.
    pub hint_volume: Option<Uuid>,
    /// Zero the chunks before handing them over. On by default; costs a full
    /// write of each chunk. Turn it off for a chunk about to be overwritten
    /// end to end.
    #[serde(default = "yes")]
    pub zero: bool,
}

fn default_tier() -> String {
    "hot".to_string()
}
fn one() -> usize {
    1
}
fn yes() -> bool {
    true
}

#[derive(Debug, Serialize)]
pub struct ExtentJson {
    pub volume: String,
    pub offset: u64,
    pub len: u64,
}

impl From<&ChunkExtent> for ExtentJson {
    fn from(e: &ChunkExtent) -> Self {
        ExtentJson {
            volume: e.volume.0.to_string(),
            offset: e.offset,
            len: e.len,
        }
    }
}

async fn allocate(State(state): State<Arc<AppState>>, Json(req): Json<AllocateRequest>) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "stormfs", "method" => "allocate")
        .increment(1);

    let Some(tier) = parse_tier(&req.tier) else {
        return ApiError::bad_request(format!(
            "unknown tier '{}' — one of hot, warm, cool, cold",
            req.tier
        ));
    };

    let mut st = state.stormfs.lock().await;

    // Which volume. A hint names one outright; without it, the volume this
    // node has already been carving from. It is never guessed at from the
    // volume list: handing StormFS a boot volume's address space because it
    // happened to be the only one there is not a recoverable mistake.
    let volume = match req.hint_volume {
        Some(v) => VolumeId(v),
        None => {
            let mut known = st.chunks.volumes();
            known.sort_by_key(|v| v.0);
            match known.first() {
                Some(v) => *v,
                None => {
                    return ApiError::bad_request(
                        "no volume to allocate from: name one with hint_volume. This node has \
                         not carved StormFS chunks anywhere yet, and picking a volume for you \
                         could hand out address space something else is using.",
                    )
                }
            }
        }
    };

    let (slot_size, virtual_size) = match volume_geometry(&state, volume).await {
        Ok(g) => g,
        Err(r) => return r,
    };

    let (gem, registry) = {
        let vm = state.volume_manager.lock().await;
        (vm.gem().clone(), vm.registry().clone())
    };

    let alloc = AllocRequest {
        volume,
        virtual_size,
        slot_size,
        tier,
        chunk_len: req.len,
        count: req.count,
        zero: req.zero,
    };

    match chunk::allocate(&mut st.chunks, &gem, &registry, &alloc).await {
        Ok(extents) => {
            let items: Vec<ExtentJson> = extents.iter().map(ExtentJson::from).collect();
            persist(&state, &st).await;
            drop(st);
            state.volume_manager.lock().await.persist().await;
            Json(json!({
                "extents": items,
                "tier": req.tier,
                "count": items.len(),
                "slot_size": slot_size,
            }))
            .into_response()
        }
        Err(e) => chunk_error(e),
    }
}

fn chunk_error(e: chunk::ChunkError) -> Response {
    use chunk::ChunkError::*;
    match e {
        // Out of room is 507, not 400: nothing about the request was wrong.
        TierFull { .. } | NoAddressSpace { .. } => (
            axum::http::StatusCode::INSUFFICIENT_STORAGE,
            Json(json!({ "error": e.to_string(), "code": 507 })),
        )
            .into_response(),
        Drive(_) => ApiError::internal(e.to_string()),
        _ => ApiError::bad_request(e.to_string()),
    }
}

// ---- deallocate / trim --------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ExtentsRequest {
    pub extents: Vec<ExtentRef>,
}

#[derive(Debug, Deserialize)]
pub struct ExtentRef {
    pub volume: Uuid,
    pub offset: u64,
    pub len: u64,
}

async fn deallocate(State(state): State<Arc<AppState>>, Json(req): Json<ExtentsRequest>) -> Response {
    free_extents(state, req, true).await
}

async fn trim(State(state): State<Arc<AppState>>, Json(req): Json<ExtentsRequest>) -> Response {
    free_extents(state, req, false).await
}

/// Free the space under a set of extents; `release_ownership` is what makes
/// it a deallocate rather than a trim.
async fn free_extents(state: Arc<AppState>, req: ExtentsRequest, release_ownership: bool) -> Response {
    metrics::counter!(
        "stormblock_api_requests_total",
        "endpoint" => "stormfs",
        "method" => if release_ownership { "deallocate" } else { "trim" },
    )
    .increment(1);

    if req.extents.is_empty() {
        return ApiError::bad_request("no extents given");
    }

    let mut st = state.stormfs.lock().await;
    let (gem, registry) = {
        let vm = state.volume_manager.lock().await;
        (vm.gem().clone(), vm.registry().clone())
    };

    let mut freed = 0usize;
    let mut already_free = 0usize;
    let mut slots_freed = 0usize;
    let mut bytes_freed = 0u64;
    let mut failures: Vec<String> = Vec::new();

    // One extent at a time, so the counts are per extent as StormFS asked
    // for them rather than per slot. Slots within an extent are still
    // released in one batch per slab.
    for e in &req.extents {
        let volume = VolumeId(e.volume);
        let slot_size = match volume_geometry(&state, volume).await {
            Ok((s, _)) => s,
            // A volume that is gone took its chunks with it. That is the
            // sweeper arriving late, not a failure.
            Err(_) => {
                already_free += 1;
                if release_ownership {
                    st.chunks.forget(volume);
                    st.versions.forget(volume);
                }
                continue;
            }
        };

        let ext = ChunkExtent {
            volume,
            offset: e.offset,
            len: e.len,
        };
        match chunk::free(&mut st.chunks, &gem, &registry, &[ext], slot_size, release_ownership).await {
            Ok(out) => {
                if out.freed > 0 {
                    freed += 1;
                } else {
                    already_free += 1;
                }
                slots_freed += out.freed;
                bytes_freed += out.bytes_freed;
                failures.extend(out.failures);

                if release_ownership && out.freed > 0 {
                    let first = e.offset.div_ceil(slot_size);
                    let last = (e.offset + e.len) / slot_size;
                    st.versions.clear_range(volume, first, last);
                }
            }
            Err(err) => return chunk_error(err),
        }
    }

    persist(&state, &st).await;
    drop(st);
    state.volume_manager.lock().await.persist().await;

    Json(json!({
        "freed": freed,
        "already_free": already_free,
        "slots_freed": slots_freed,
        "bytes_freed": bytes_freed,
        "failures": failures,
    }))
    .into_response()
}

// ---- extent map ---------------------------------------------------------

async fn extent_map(State(state): State<Arc<AppState>>, Path(vol): Path<String>) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "stormfs", "method" => "extent-map")
        .increment(1);

    let Ok(uuid) = vol.parse::<Uuid>() else {
        return ApiError::bad_request(format!("invalid UUID: {vol}"));
    };
    let volume = VolumeId(uuid);

    let st = state.stormfs.lock().await;
    let (slot_size, virtual_size) = match volume_geometry(&state, volume).await {
        Ok(g) => g,
        Err(r) => return r,
    };
    let gem = state.volume_manager.lock().await.gem().clone();

    let mut report = chunk::extent_map(&st.chunks, &gem, volume, slot_size, virtual_size).await;
    // Versions belong in the same answer: fsck reconciling a chunk map wants
    // to know which version each extent is at, and a second round trip to
    // find out would race the first.
    let versions: Vec<u64> = report
        .extents
        .iter()
        .map(|e| st.versions.extent_version(volume, e.offset / slot_size))
        .collect();
    let volume_version = st.versions.volume_version(volume);
    let pins = st.pins.for_volume(volume).len();
    drop(st);

    let extents: Vec<serde_json::Value> = report
        .extents
        .drain(..)
        .zip(versions)
        .map(|(e, version)| {
            let mut v = serde_json::to_value(&e).unwrap_or_else(|_| json!({}));
            if let Some(obj) = v.as_object_mut() {
                obj.insert("version".into(), json!(version));
            }
            v
        })
        .collect();

    Json(json!({
        "volume": report.volume,
        "slot_size": report.slot_size,
        "virtual_size": report.virtual_size,
        "chunks": report.chunks,
        "owned_bytes": report.owned_bytes,
        "allocated_bytes": report.allocated_bytes,
        "unmapped_owned_bytes": report.unmapped_owned_bytes,
        "volume_version": volume_version,
        "pins": pins,
        "extents": extents,
    }))
    .into_response()
}

// ---- commit (#50) -------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CommitJson {
    pub volume: Uuid,
    pub offset: u64,
    pub len: u64,
    /// The version the writer believes the range is at.
    pub expected_version: u64,
    /// Where the writer put the data. Omit to punch the range out — an
    /// all-or-nothing truncate rather than a write.
    pub staged: Option<StagedJson>,
}

#[derive(Debug, Deserialize)]
pub struct StagedJson {
    /// Defaults to the volume being committed into.
    pub volume: Option<Uuid>,
    pub offset: u64,
}

async fn commit(State(state): State<Arc<AppState>>, Json(req): Json<CommitJson>) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "stormfs", "method" => "commit")
        .increment(1);

    let volume = VolumeId(req.volume);
    let mut st = state.stormfs.lock().await;

    let (slot_size, _) = match volume_geometry(&state, volume).await {
        Ok(g) => g,
        Err(r) => return r,
    };
    let (gem, registry) = {
        let vm = state.volume_manager.lock().await;
        (vm.gem().clone(), vm.registry().clone())
    };

    let creq = CommitRequest {
        volume,
        offset: req.offset,
        len: req.len,
        slot_size,
        expected_version: req.expected_version,
        staged: req.staged.as_ref().map(|s| StagedRange {
            volume: s.volume.map(VolumeId).unwrap_or(volume),
            offset: s.offset,
        }),
    };

    match versioned::commit(&mut st.versions, &gem, &registry, &creq).await {
        Ok(out) => {
            // Versions first, then the extent map. A crash between the two
            // leaves the version ahead of the map, which costs a writer one
            // retry; the other order lets a stale writer commit over
            // committed data.
            persist(&state, &st).await;
            drop(st);
            state.volume_manager.lock().await.persist().await;

            Json(json!({
                "version": out.version,
                "extents_swapped": out.extents_swapped,
                "extents_released": out.extents_released,
                "failures": out.failures,
            }))
            .into_response()
        }
        Err(e) => commit_error(e),
    }
}

fn commit_error(e: CommitError) -> Response {
    match e {
        // The one error a writer is expected to see and act on, so it carries
        // the version to retry at rather than making the writer go and ask.
        CommitError::StaleVersion { expected, current } => (
            axum::http::StatusCode::CONFLICT,
            Json(json!({
                "error": CommitError::StaleVersion { expected, current }.to_string(),
                "code": 409,
                "current_version": current,
            })),
        )
            .into_response(),
        CommitError::MixedVersions { low, high } => (
            axum::http::StatusCode::CONFLICT,
            Json(json!({
                "error": CommitError::MixedVersions { low, high }.to_string(),
                "code": 409,
                "current_version": high,
            })),
        )
            .into_response(),
        CommitError::Failed(m) => ApiError::internal(m),
        other => ApiError::bad_request(other.to_string()),
    }
}

// ---- pins (#50) ---------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PinRequest {
    pub volume: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct PinQuery {
    pub volume: Option<Uuid>,
}

fn pin_json(p: &Pin) -> serde_json::Value {
    json!({
        "pin": p.id.to_string(),
        "volume": p.volume.0.to_string(),
        "snapshot": p.snapshot.0.to_string(),
        "version": p.version,
        "created_unix": p.created_unix,
    })
}

async fn take_pin(State(state): State<Arc<AppState>>, Json(req): Json<PinRequest>) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "stormfs", "method" => "pin")
        .increment(1);

    let volume = VolumeId(req.volume);
    let mut st = state.stormfs.lock().await;

    // The version is read and the snapshot taken without releasing the
    // stormfs lock, so no commit can land in between and leave the pin
    // labelled with a version it does not actually hold.
    let version = st.versions.volume_version(volume);
    let id = Uuid::new_v4();
    let name = format!("stormfs-pin-{}", &id.simple().to_string()[..8]);

    let snapshot = {
        let mut vm = state.volume_manager.lock().await;
        if vm.get_volume_handle(&volume).is_none() {
            return ApiError::not_found(format!("volume {} not found", req.volume));
        }
        match vm.create_snapshot(volume, &name).await {
            Ok(s) => s,
            Err(e) => return ApiError::internal(format!("could not pin volume: {e}")),
        }
    };

    let pin = Pin {
        id,
        volume,
        snapshot,
        version,
        created_unix: now_unix(),
    };
    st.pins.insert(pin.clone());
    persist(&state, &st).await;
    drop(st);

    (
        axum::http::StatusCode::CREATED,
        Json(json!({
            "pin": pin.id.to_string(),
            "volume": pin.volume.0.to_string(),
            "snapshot": pin.snapshot.0.to_string(),
            "version": pin.version,
            "created_unix": pin.created_unix,
            "read_through": "the snapshot volume — attach and read it like any other. It holds \
                             the extents this volume had when the pin was taken, and commits \
                             that supersede them decrement rather than free.",
        })),
    )
        .into_response()
}

async fn list_pins(State(state): State<Arc<AppState>>, Query(q): Query<PinQuery>) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "stormfs", "method" => "pins")
        .increment(1);
    let st = state.stormfs.lock().await;
    let items: Vec<serde_json::Value> = match q.volume {
        Some(v) => st.pins.for_volume(VolumeId(v)).iter().map(pin_json).collect(),
        None => st.pins.list().iter().map(pin_json).collect(),
    };
    Json(json!({ "items": items, "count": items.len() })).into_response()
}

async fn release_pin(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "stormfs", "method" => "unpin")
        .increment(1);

    let Ok(uuid) = id.parse::<Uuid>() else {
        return ApiError::bad_request(format!("invalid UUID: {id}"));
    };

    let mut st = state.stormfs.lock().await;
    let Some(pin) = st.pins.remove(&uuid) else {
        // Releasing a pin twice is a success for the same reason freeing an
        // extent twice is: the reader may have crashed mid-release.
        return Json(json!({ "released": false, "already_released": true })).into_response();
    };

    // The pin is out of the table before the snapshot is deleted, so a
    // failure here cannot leave a pin pointing at a volume that is gone.
    let outcome = {
        let mut vm = state.volume_manager.lock().await;
        vm.delete_volume(pin.snapshot).await
    };
    persist(&state, &st).await;
    drop(st);

    match outcome {
        Ok(()) => Json(json!({
            "released": true,
            "snapshot_deleted": pin.snapshot.0.to_string(),
        }))
        .into_response(),
        Err(e) => {
            // The pin is released either way; say plainly that the snapshot
            // volume is still there, since it is now the caller's to remove.
            tracing::error!("pin {uuid} released but its snapshot could not be deleted: {e}");
            Json(json!({
                "released": true,
                "snapshot_leaked": pin.snapshot.0.to_string(),
                "error": e.to_string(),
            }))
            .into_response()
        }
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/allocate", post(allocate))
        .route("/deallocate", post(deallocate))
        .route("/trim", post(trim))
        // axum 0.8 spells path parameters `{vol}`; the spec's `:vol` is the
        // axum 0.6 form the routing table was written against.
        .route("/extent-map/{vol}", get(extent_map))
        .route("/commit", post(commit))
        .route("/pins", get(list_pins).post(take_pin))
        .route("/pins/{id}", axum::routing::delete(release_pin))
        .with_state(state)
}
