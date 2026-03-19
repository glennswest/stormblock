//! GET/POST/DELETE /api/v1/arrays — RAID array management.

use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, State},
    routing::get,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Serialize, Deserialize};
use uuid::Uuid;

use super::{ApiError, ListResponse};
use crate::drive::BlockDevice;
use crate::mgmt::AppState;
use crate::mgmt::config::human_size;
use crate::mgmt::ArrayInfo;
use crate::raid::{RaidArray, RaidArrayId, RaidLevel};

#[derive(Debug, Serialize)]
pub struct ArrayResponse {
    pub id: Uuid,
    pub level: String,
    pub member_count: usize,
    pub capacity_bytes: u64,
    pub capacity_human: String,
    pub stripe_size: u64,
    pub stripe_human: String,
    pub members: Vec<MemberResponse>,
}

#[derive(Debug, Serialize)]
pub struct MemberResponse {
    pub index: usize,
    pub state: String,
    pub device_path: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateArrayRequest {
    pub level: RaidLevel,
    pub drive_uuids: Vec<Uuid>,
    #[serde(default = "default_stripe_kb")]
    pub stripe_kb: u64,
}

fn default_stripe_kb() -> u64 {
    64
}

fn array_to_response(id: RaidArrayId, info: &ArrayInfo) -> ArrayResponse {
    let members: Vec<MemberResponse> = info.array.member_states().iter().map(|(idx, state)| {
        MemberResponse {
            index: *idx,
            state: state.to_string(),
            device_path: String::new(), // Drive paths aren't stored on members
        }
    }).collect();

    ArrayResponse {
        id: id.0,
        level: info.level.to_string(),
        member_count: info.member_count,
        capacity_bytes: info.capacity_bytes,
        capacity_human: human_size(info.capacity_bytes),
        stripe_size: info.stripe_size,
        stripe_human: human_size(info.stripe_size),
        members,
    }
}

async fn list_arrays(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "arrays", "method" => "list").increment(1);
    let arrays = state.arrays.read().await;
    let items: Vec<ArrayResponse> = arrays.iter()
        .map(|(id, info)| array_to_response(*id, info))
        .collect();
    let count = items.len();
    Json(ListResponse { items, count })
}

async fn get_array(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "arrays", "method" => "get").increment(1);
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };

    let arrays = state.arrays.read().await;
    let array_id = RaidArrayId(uuid);
    match arrays.get(&array_id) {
        Some(info) => Json(array_to_response(array_id, info)).into_response(),
        None => ApiError::not_found(format!("array {uuid} not found")),
    }
}

async fn create_array(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateArrayRequest>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "arrays", "method" => "create").increment(1);

    // Look up drives by UUID
    let drives_lock = state.drives.read().await;
    let mut member_devices: Vec<Arc<dyn BlockDevice>> = Vec::new();

    for uuid in &req.drive_uuids {
        match drives_lock.iter().find(|d| d.device.id().uuid == *uuid) {
            Some(d) => member_devices.push(d.device.clone()),
            None => return ApiError::not_found(format!("drive {uuid} not found")),
        }
    }
    drop(drives_lock);

    let stripe_size = req.stripe_kb * 1024;

    // Create the RAID array
    let array = match RaidArray::create(req.level, member_devices, Some(stripe_size)).await {
        Ok(a) => a,
        Err(e) => return ApiError::bad_request(format!("failed to create array: {e}")),
    };

    let array_id = array.array_id();
    let level = array.level();
    let member_count = array.member_count();
    let capacity_bytes = array.capacity_bytes();
    let stripe = array.stripe_size();
    let arc_array = Arc::new(array);

    // Register in volume manager
    {
        let mut vm = state.volume_manager.lock().await;
        vm.add_backing_device(array_id, arc_array.clone() as Arc<dyn BlockDevice>).await;
    }

    // Register in state
    let info = ArrayInfo {
        array: arc_array,
        level,
        member_count,
        capacity_bytes,
        stripe_size: stripe,
    };
    let resp = array_to_response(array_id, &info);

    {
        let mut arrays = state.arrays.write().await;
        arrays.insert(array_id, info);
    }

    metrics::gauge!("stormblock_arrays_total").set(state.arrays.read().await.len() as f64);

    (axum::http::StatusCode::CREATED, Json(resp)).into_response()
}

async fn delete_array(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "arrays", "method" => "delete").increment(1);
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };

    let array_id = RaidArrayId(uuid);

    // Check if any volumes reference this array
    {
        let vm = state.volume_manager.lock().await;
        let vols = vm.list_volumes().await;
        // We can't easily check which array a volume belongs to from the public API,
        // so just check if there are any volumes at all when deleting
        if !vols.is_empty() {
            // Check exports for volumes on this array
            // For safety, refuse deletion if there are volumes
            return ApiError::conflict("cannot delete array while volumes exist".to_string());
        }
    }

    let mut arrays = state.arrays.write().await;
    match arrays.remove(&array_id) {
        Some(_) => {
            metrics::gauge!("stormblock_arrays_total").set(arrays.len() as f64);
            axum::http::StatusCode::NO_CONTENT.into_response()
        }
        None => ApiError::not_found(format!("array {uuid} not found")),
    }
}

#[derive(Debug, Deserialize)]
pub struct AddMemberRequest {
    pub drive_uuid: Uuid,
}

#[derive(Debug, Serialize)]
pub struct MemberUuidResponse {
    pub member_uuid: Uuid,
}

async fn add_member(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<AddMemberRequest>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "arrays", "method" => "add_member").increment(1);
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };

    let array_id = RaidArrayId(uuid);

    // Look up the array
    let arrays = state.arrays.read().await;
    let array = match arrays.get(&array_id) {
        Some(info) => info.array.clone(),
        None => return ApiError::not_found(format!("array {uuid} not found")),
    };
    drop(arrays);

    // Look up the drive
    let drives = state.drives.read().await;
    let device = match drives.iter().find(|d| d.device.id().uuid == req.drive_uuid) {
        Some(d) => d.device.clone(),
        None => return ApiError::not_found(format!("drive {} not found", req.drive_uuid)),
    };
    drop(drives);

    match array.add_member(device).await {
        Ok(member_uuid) => {
            (axum::http::StatusCode::CREATED, Json(MemberUuidResponse { member_uuid })).into_response()
        }
        Err(e) => ApiError::bad_request(format!("failed to add member: {e}")),
    }
}

async fn remove_member(
    State(state): State<Arc<AppState>>,
    Path((id, member_id)): Path<(String, String)>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "arrays", "method" => "remove_member").increment(1);
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid UUID: {id}")),
    };
    let member_uuid = match member_id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid member UUID: {member_id}")),
    };

    let array_id = RaidArrayId(uuid);

    let arrays = state.arrays.read().await;
    let array = match arrays.get(&array_id) {
        Some(info) => info.array.clone(),
        None => return ApiError::not_found(format!("array {uuid} not found")),
    };
    drop(arrays);

    match array.remove_member(member_uuid).await {
        Ok(()) => axum::http::StatusCode::NO_CONTENT.into_response(),
        Err(e) => ApiError::bad_request(format!("failed to remove member: {e}")),
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list_arrays).post(create_array))
        .route("/{id}", get(get_array).delete(delete_array))
        .route("/{id}/members", axum::routing::post(add_member))
        .route("/{id}/members/{member_id}", axum::routing::delete(remove_member))
        .with_state(state)
}
