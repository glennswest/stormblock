//! GET/POST/DELETE /api/v1/slabs — Slab extent store management.

use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, State},
    routing::get,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Serialize, Deserialize};

use super::{ApiError, ListResponse};
use crate::drive::slab::SlabId;
use crate::mgmt::AppState;
use crate::mgmt::config::human_size;
use crate::placement::topology::StorageTier;

#[derive(Debug, Serialize)]
pub struct SlabResponse {
    pub id: String,
    pub tier: String,
    /// `system` or `data` — whether an install may reformat this slab (#88).
    pub role: String,
    /// What fails together with this slab: `drive=…`, or the wider chain a
    /// labelled drive gave it. Redundancy policies spread across these.
    pub domain: String,
    pub slot_size: u64,
    pub total_slots: u64,
    pub free_slots: u64,
    pub allocated_slots: u64,
    pub total_bytes: u64,
    pub total_bytes_human: String,
    pub free_bytes: u64,
    pub free_bytes_human: String,
}

#[derive(Debug, Serialize)]
pub struct SlotResponse {
    pub slot_idx: u32,
    pub volume_id: String,
    pub virtual_extent_idx: u64,
    pub ref_count: u32,
    pub generation: u64,
}

#[derive(Debug, Deserialize)]
pub struct FormatSlabRequest {
    pub device_path: String,
    #[serde(default = "default_tier")]
    pub tier: String,
    pub slot_size: Option<u64>,
    /// Failure domain to record for the slab, `rung=value/…`. Defaults to
    /// the device's identity under whatever labels the drive carries.
    #[serde(default)]
    pub domain: Option<String>,
    /// `system` (the default — goldens, replaced by an image) or `data`
    /// (identity and state, which no install path may reformat).
    #[serde(default)]
    pub role: Option<String>,
    /// Bytes reserved in the slab for its own record of what it holds.
    ///
    /// A slab with no metadata region cannot say what volumes are on it, so
    /// the only statement of that lives wherever the engine happened to keep
    /// it — and storage that arrived as a drive has no such place. A `data`
    /// slab therefore reserves one by default: outliving whatever formatted it
    /// is the entire point of the role.
    #[serde(default)]
    pub metadata_bytes: Option<u64>,
}

fn default_tier() -> String {
    "hot".to_string()
}

/// How much of a data slab is set aside for its own volume record. Two
/// copies are kept, so this is the space for both; 4 MiB holds thousands of
/// volumes and costs nothing on a drive measured in terabytes.

pub(crate) fn parse_tier(s: &str) -> Option<StorageTier> {
    match s.to_lowercase().as_str() {
        "hot" => Some(StorageTier::Hot),
        "warm" => Some(StorageTier::Warm),
        "cool" => Some(StorageTier::Cool),
        "cold" => Some(StorageTier::Cold),
        _ => None,
    }
}

async fn list_slabs(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "slabs", "method" => "list").increment(1);
    let reg = state.slab_registry.read().await;
    let items: Vec<SlabResponse> = reg.iter()
        .map(|(id, slab)| {
            let slot_size = slab.slot_size();
            let total = slab.total_slots();
            let free = slab.free_slots();
            let allocated = slab.allocated_slots();
            SlabResponse {
                id: id.0.to_string(),
                tier: format!("{}", slab.tier()),
                role: slab.role().to_string(),
                domain: reg.domain_of(id).to_string(),
                slot_size,
                total_slots: total,
                free_slots: free,
                allocated_slots: allocated,
                total_bytes: total * slot_size,
                total_bytes_human: human_size(total * slot_size),
                free_bytes: free * slot_size,
                free_bytes_human: human_size(free * slot_size),
            }
        })
        .collect();
    let count = items.len();
    Json(ListResponse { items, count })
}

async fn get_slab(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "slabs", "method" => "get").increment(1);
    let uuid = match id.parse::<uuid::Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };
    let slab_id = SlabId(uuid);

    let reg = state.slab_registry.read().await;
    match reg.get(&slab_id) {
        Some(slab) => {
            let slot_size = slab.slot_size();
            let total = slab.total_slots();
            let free = slab.free_slots();
            let allocated = slab.allocated_slots();
            Json(SlabResponse {
                id: slab_id.0.to_string(),
                tier: format!("{}", slab.tier()),
                role: slab.role().to_string(),
                domain: reg.domain_of(&slab_id).to_string(),
                slot_size,
                total_slots: total,
                free_slots: free,
                allocated_slots: allocated,
                total_bytes: total * slot_size,
                total_bytes_human: human_size(total * slot_size),
                free_bytes: free * slot_size,
                free_bytes_human: human_size(free * slot_size),
            }).into_response()
        }
        None => ApiError::not_found(format!("slab {uuid} not found")),
    }
}

async fn format_slab(
    State(state): State<Arc<AppState>>,
    Json(req): Json<FormatSlabRequest>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "slabs", "method" => "format").increment(1);

    let tier = match parse_tier(&req.tier) {
        Some(t) => t,
        None => return ApiError::bad_request(format!("invalid tier '{}' (use hot, warm, cool, cold)", req.tier)),
    };

    let slot_size = req.slot_size.unwrap_or(crate::drive::slab::DEFAULT_SLOT_SIZE);

    let role = match req.role.as_deref() {
        Some(r) => match crate::drive::slab::SlabRole::parse(r) {
            Some(r) => r,
            None => return ApiError::bad_request(format!("invalid role '{r}' (use system or data)")),
        },
        None => crate::drive::slab::SlabRole::System,
    };

    // Formatting destroys what is there, and a data slab holds the one thing
    // on the node nothing can mint again. Refuse unless the caller says, in
    // this request, that a data slab is what it means to write (#88).
    if role != crate::drive::slab::SlabRole::Data {
        if let Ok(existing) = crate::drive::filedev::FileDevice::open(&req.device_path).await {
            let dev = Arc::new(existing) as Arc<dyn crate::drive::BlockDevice>;
            if let Ok(slab) = crate::drive::slab::Slab::open(dev).await {
                if slab.is_data() {
                    return ApiError::conflict(format!(
                        "{} already holds a data slab ({}); pass \"role\": \"data\" to replace it",
                        req.device_path,
                        slab.slab_id().0
                    ));
                }
            }
        }
    }

    // Open the device
    let device = match crate::drive::filedev::FileDevice::open(&req.device_path).await {
        Ok(d) => Arc::new(d) as Arc<dyn crate::drive::BlockDevice>,
        Err(e) => return ApiError::bad_request(format!("cannot open device '{}': {e}", req.device_path)),
    };

    let domain = match req.domain.as_deref() {
        Some(d) => match crate::placement::domain::FailureDomain::parse(d) {
            Ok(d) => Some(d),
            Err(e) => return ApiError::bad_request(e),
        },
        None => None,
    };
    // A data slab keeps its own record unless told otherwise: its reason for
    // existing is to survive the thing that would otherwise hold it.
    //
    // Size that region from the drive, not from a constant. A volume record
    // carries its whole extent map, so what the region has to hold scales
    // with the slots the slab can hand out — an 11 GB volume is some eleven
    // thousand extents on its own. A flat 4 MiB fits a small slab and is
    // several times too small for a large one, and the way that failure
    // presented was not "out of space": writes kept being acknowledged and
    // nothing was durable, so a restart came back with a fraction of its
    // volumes and a published release that had never been on disk.
    let meta_bytes = req.metadata_bytes.unwrap_or(match role {
        crate::drive::slab::SlabRole::Data => {
            crate::drive::slab::auto_metadata_bytes(device.capacity_bytes(), slot_size)
        }
        _ => 0,
    });
    let opts = crate::drive::slab::SlabFormat::new(slot_size, tier)
        .with_role(role)
        .with_metadata(meta_bytes);
    match crate::drive::slab::Slab::format_with(device, opts).await {
        Ok(slab) => {
            let slab_id = slab.slab_id();
            let total = slab.total_slots();
            // A slab with a region for its own record should be keeping one.
            // Registering it without saying so left the record living only in
            // whatever directory the engine happened to have — which for
            // storage that arrives as a drive is nowhere, so its contents did
            // not survive a restart.
            let carries_metadata = slab.has_metadata_region();
            let free = slab.free_slots();
            let allocated = slab.allocated_slots();
            let slab_domain = {
                let mut reg = state.slab_registry.write().await;
                match domain {
                    Some(d) => reg.add_in_domain(slab, d),
                    None => reg.add(slab),
                }
                reg.domain_of(&slab_id).to_string()
            };
            if carries_metadata {
                let mut vm = state.volume_manager.lock().await;
                let mut slabs = vm.metadata_slabs().to_vec();
                if !slabs.contains(&slab_id) {
                    slabs.push(slab_id);
                    vm.persist_to_slabs(slabs);
                }
            }
            let resp = SlabResponse {
                id: slab_id.0.to_string(),
                tier: format!("{}", tier),
                role: role.to_string(),
                domain: slab_domain,
                slot_size,
                total_slots: total,
                free_slots: free,
                allocated_slots: allocated,
                total_bytes: total * slot_size,
                total_bytes_human: human_size(total * slot_size),
                free_bytes: free * slot_size,
                free_bytes_human: human_size(free * slot_size),
            };
            (axum::http::StatusCode::CREATED, Json(resp)).into_response()
        }
        Err(e) => ApiError::internal(format!("failed to format slab: {e}")),
    }
}

async fn delete_slab(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "slabs", "method" => "delete").increment(1);
    let uuid = match id.parse::<uuid::Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };
    let slab_id = SlabId(uuid);

    let mut reg = state.slab_registry.write().await;
    match reg.get(&slab_id) {
        Some(slab) => {
            if slab.allocated_slots() > 0 {
                return ApiError::conflict("cannot delete slab with allocated slots — evacuate first");
            }
        }
        None => return ApiError::not_found(format!("slab {uuid} not found")),
    }
    reg.remove(&slab_id);
    axum::http::StatusCode::NO_CONTENT.into_response()
}

async fn list_slots(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "slots", "method" => "list").increment(1);
    let uuid = match id.parse::<uuid::Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };
    let slab_id = SlabId(uuid);

    let reg = state.slab_registry.read().await;
    match reg.get(&slab_id) {
        Some(slab) => {
            let mut items = Vec::new();
            for idx in 0..slab.total_slots() as u32 {
                if let Some(slot) = slab.get_slot(idx) {
                    if slot.state == crate::drive::slab::SlotState::Free {
                        continue;
                    }
                    items.push(SlotResponse {
                        slot_idx: idx,
                        volume_id: slot.volume_id.0.to_string(),
                        virtual_extent_idx: slot.virtual_extent_idx,
                        ref_count: slot.ref_count,
                        generation: slot.generation,
                    });
                }
            }
            let count = items.len();
            Json(ListResponse { items, count }).into_response()
        }
        None => ApiError::not_found(format!("slab {uuid} not found")),
    }
}

/// Most orphans listed in one response — the counts are always exact, only
/// the sample is bounded.
const MAX_ORPHANS_REPORTED: usize = 100;

#[derive(Debug, Deserialize, Default)]
pub struct GcQuery {
    /// Report what would be freed, without freeing it.
    #[serde(default)]
    pub dry_run: bool,
    /// Cap on slots freed by this pass.
    pub max_reclaim: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct OrphanResponse {
    pub slab_id: String,
    pub slot_idx: u32,
    /// Owner recorded in the slot table — usually a volume that no longer
    /// exists. Informational; liveness is decided by the extent map.
    pub stale_owner: String,
    pub virtual_extent_idx: u64,
    pub ref_count: u32,
}

#[derive(Debug, Serialize)]
pub struct GcRunResponse {
    pub slabs_scanned: usize,
    pub slots_scanned: u64,
    pub live: u64,
    pub in_flight: usize,
    pub orphans_found: usize,
    pub reclaimed: usize,
    pub bytes_reclaimed: u64,
    pub bytes_reclaimed_human: String,
    pub deferred: usize,
    pub dry_run: bool,
    pub orphans: Vec<OrphanResponse>,
    pub orphans_truncated: bool,
}

/// POST /api/v1/slabs/gc — reclaim slab slots no volume maps.
///
/// One pass, run now. The background collector does the same thing on a
/// timer; this is for when an operator wants the space back immediately, or
/// wants to see what a pass would do (`?dry_run=true`).
pub async fn run_gc(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<GcQuery>,
) -> Response {
    let report = crate::volume::gc::run_once(
        &state.gem,
        &state.slab_registry,
        crate::volume::gc::GcOptions {
            dry_run: q.dry_run,
            confirm_against: None,
            max_reclaim: q.max_reclaim,
        },
    )
    .await;

    let truncated = report.orphans.len() > MAX_ORPHANS_REPORTED;
    Json(GcRunResponse {
        slabs_scanned: report.slabs_scanned,
        slots_scanned: report.slots_scanned,
        live: report.live,
        in_flight: report.in_flight,
        orphans_found: report.orphans.len(),
        reclaimed: report.reclaimed,
        bytes_reclaimed: report.bytes_reclaimed,
        bytes_reclaimed_human: human_size(report.bytes_reclaimed),
        deferred: report.deferred,
        dry_run: report.dry_run,
        orphans: report
            .orphans
            .iter()
            .take(MAX_ORPHANS_REPORTED)
            .map(|o| OrphanResponse {
                slab_id: o.slab_id.to_string(),
                slot_idx: o.slot_idx,
                stale_owner: o.volume_id.to_string(),
                virtual_extent_idx: o.virtual_extent_idx,
                ref_count: o.ref_count,
            })
            .collect(),
        orphans_truncated: truncated,
    })
    .into_response()
}

/// GET /api/v1/slabs/gc — what the background collector last found.
pub async fn gc_status(State(state): State<Arc<AppState>>) -> Response {
    let cfg = &state.config.gc;
    let last = match &state.last_gc {
        Some(l) => l.read().await.clone(),
        None => None,
    };
    Json(serde_json::json!({
        "enabled": cfg.enabled,
        "interval_secs": cfg.interval_secs,
        "confirm_passes": cfg.confirm_passes,
        "max_reclaim_per_pass": cfg.max_reclaim_per_pass,
        "dry_run": cfg.dry_run,
        "running": state.last_gc.is_some(),
        "last_pass": last,
    }))
    .into_response()
}

/// `GET /api/v1/slabs/pool` — the pool as a whole: how full it is, and what
/// the pressure watcher is doing about it (#18).
///
/// Per-slab numbers were always available and nothing summed them, so the one
/// question an operator actually asks — is this node about to run out of
/// physical space — had no answer. Thin volumes overcommit, so it cannot be
/// inferred from what the volumes report either.
pub async fn pool_status(State(state): State<Arc<AppState>>) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "slabs", "method" => "pool")
        .increment(1);

    // The watcher's own view when there is one — it carries the decision
    // history. Otherwise sample directly, so the accounting is available on a
    // node that never enabled growth.
    if let Some(cell) = &state.pool_pressure {
        if let Some(status) = cell.read().await.clone() {
            return Json(status).into_response();
        }
    }

    let usage = crate::volume::pressure::PoolUsage::sample(&state.slab_registry).await;
    Json(serde_json::json!({
        "enabled": false,
        "high_water_pct": null,
        "used_pct": usage.used_pct(),
        "under_pressure": false,
        "usage": usage,
        "sources_remaining": 0,
        "slabs_added": 0,
    }))
    .into_response()
}

/// Whether this node's volume record is reaching the disk, and how close each
/// slab is to not being able to hold it.
///
/// Worth asking before trusting that anything created here will come back.
/// The failure this reports is not one a caller ever sees: the volume is
/// created, the write is acknowledged, and only a restart says otherwise.
pub async fn durability(State(state): State<Arc<AppState>>) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "slabs", "method" => "durability")
        .increment(1);

    let mgr = state.volume_manager.lock().await;
    let fault = mgr.durability_fault();
    let slabs = mgr.metadata_pressure().await;
    drop(mgr);

    let short: Vec<&crate::volume::MetadataPressure> =
        slabs.iter().filter(|s| !s.fits).collect();
    Json(serde_json::json!({
        "ok": fault.is_none() && short.is_empty(),
        "fault": fault,
        "slabs": slabs,
        "remedy": if short.is_empty() { serde_json::Value::Null } else {
            serde_json::json!(
                "a slab's metadata region is too small for the record it has to hold; \
                 reformat it with a larger region (the size is chosen from capacity \
                 automatically) or move volumes off it"
            )
        },
    }))
    .into_response()
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list_slabs).post(format_slab))
        // Before /{id}: "gc" is a verb here, not a slab id.
        .route("/gc", get(gc_status).post(run_gc))
        // Likewise "pool": the aggregate over every slab, not one of them.
        .route("/pool", get(pool_status))
        // Likewise "durability": whether the record is actually being written.
        .route("/durability", get(durability))
        .route("/{id}", get(get_slab).delete(delete_slab))
        .route("/{id}/slots", get(list_slots))
        .with_state(state)
}
