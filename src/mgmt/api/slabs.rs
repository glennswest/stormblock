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
}

fn default_tier() -> String {
    "hot".to_string()
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

async fn list_slabs(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "slabs", "method" => "list").increment(1);
    let reg = state.slab_registry.lock().await;
    let items: Vec<SlabResponse> = reg.iter()
        .map(|(id, slab)| {
            let slot_size = slab.slot_size();
            let total = slab.total_slots();
            let free = slab.free_slots();
            let allocated = slab.allocated_slots();
            SlabResponse {
                id: id.0.to_string(),
                tier: format!("{}", slab.tier()),
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

    let reg = state.slab_registry.lock().await;
    match reg.get(&slab_id) {
        Some(slab) => {
            let slot_size = slab.slot_size();
            let total = slab.total_slots();
            let free = slab.free_slots();
            let allocated = slab.allocated_slots();
            Json(SlabResponse {
                id: slab_id.0.to_string(),
                tier: format!("{}", slab.tier()),
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

    // Open the device
    let device = match crate::drive::filedev::FileDevice::open(&req.device_path).await {
        Ok(d) => Arc::new(d) as Arc<dyn crate::drive::BlockDevice>,
        Err(e) => return ApiError::bad_request(format!("cannot open device '{}': {e}", req.device_path)),
    };

    match crate::drive::slab::Slab::format(device, slot_size, tier).await {
        Ok(slab) => {
            let slab_id = slab.slab_id();
            let total = slab.total_slots();
            let free = slab.free_slots();
            let allocated = slab.allocated_slots();
            let resp = SlabResponse {
                id: slab_id.0.to_string(),
                tier: format!("{}", slab.tier()),
                slot_size,
                total_slots: total,
                free_slots: free,
                allocated_slots: allocated,
                total_bytes: total * slot_size,
                total_bytes_human: human_size(total * slot_size),
                free_bytes: free * slot_size,
                free_bytes_human: human_size(free * slot_size),
            };
            state.slab_registry.lock().await.add(slab);
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

    let mut reg = state.slab_registry.lock().await;
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

    let reg = state.slab_registry.lock().await;
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

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list_slabs).post(format_slab))
        .route("/{id}", get(get_slab).delete(delete_slab))
        .route("/{id}/slots", get(list_slots))
        .with_state(state)
}
