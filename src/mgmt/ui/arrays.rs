//! Arrays page — GET /ui/arrays, GET /ui/arrays/table, POST /ui/arrays, DELETE /ui/arrays/{id}

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Response};
use axum::Form;
use serde::Deserialize;
use uuid::Uuid;

use crate::drive::BlockDevice;
use crate::mgmt::AppState;
use crate::mgmt::config::human_size;
use crate::mgmt::ArrayInfo;
use crate::raid::{RaidArray, RaidArrayId, RaidLevel};
use super::shared::{self, filters};

/// Array info for templates.
pub struct ArrayRow {
    pub id: Uuid,
    pub level: String,
    pub member_count: usize,
    pub capacity_human: String,
    pub stripe_human: String,
}

#[derive(Template)]
#[template(path = "arrays.html")]
struct ArraysPage {
    active: &'static str,
    arrays: Vec<ArrayRow>,
}

#[derive(Template)]
#[template(path = "arrays_table.html")]
struct ArraysTable {
    arrays: Vec<ArrayRow>,
}

#[derive(Deserialize)]
pub struct CreateArrayForm {
    pub level: u8,
    pub drive_uuids: String,
    pub stripe_kb: Option<u64>,
}

async fn gather_arrays(state: &AppState) -> Vec<ArrayRow> {
    let arrays = state.arrays.read().await;
    arrays
        .iter()
        .map(|(id, info)| ArrayRow {
            id: id.0,
            level: info.level.to_string(),
            member_count: info.member_count,
            capacity_human: human_size(info.capacity_bytes),
            stripe_human: human_size(info.stripe_size),
        })
        .collect()
}

pub async fn list_page(State(state): State<Arc<AppState>>) -> Response {
    let arrays = gather_arrays(&state).await;
    shared::render(&ArraysPage {
        active: "arrays",
        arrays,
    })
}

pub async fn table_partial(State(state): State<Arc<AppState>>) -> Response {
    let arrays = gather_arrays(&state).await;
    shared::render(&ArraysTable { arrays })
}

pub async fn create(
    State(state): State<Arc<AppState>>,
    Form(form): Form<CreateArrayForm>,
) -> Response {
    let level = match form.level {
        1 => RaidLevel::Raid1,
        5 => RaidLevel::Raid5,
        6 => RaidLevel::Raid6,
        10 => RaidLevel::Raid10,
        _ => {
            let toast = shared::toast_oob("Invalid RAID level", "error");
            let table = ArraysTable { arrays: gather_arrays(&state).await };
            return Html(format!("{}{}", table.render().unwrap_or_default(), toast))
                .into_response();
        }
    };

    // Parse drive UUIDs
    let uuids: Result<Vec<Uuid>, _> = form
        .drive_uuids
        .split(',')
        .map(|s| s.trim().parse::<Uuid>())
        .collect();

    let uuids = match uuids {
        Ok(u) => u,
        Err(_) => {
            let toast = shared::toast_oob("Invalid drive UUID format", "error");
            let table = ArraysTable { arrays: gather_arrays(&state).await };
            return Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response();
        }
    };

    // Look up drives
    let drives_lock = state.drives.read().await;
    let mut member_devices: Vec<Arc<dyn BlockDevice>> = Vec::new();
    for uuid in &uuids {
        match drives_lock.iter().find(|d| d.device.id().uuid == *uuid) {
            Some(d) => member_devices.push(d.device.clone()),
            None => {
                drop(drives_lock);
                let toast = shared::toast_oob(&format!("Drive {uuid} not found"), "error");
                let table = ArraysTable { arrays: gather_arrays(&state).await };
                return Html(format!("{}{}", table.render().unwrap_or_default(), toast))
                    .into_response();
            }
        }
    }
    drop(drives_lock);

    let stripe_size = form.stripe_kb.unwrap_or(64) * 1024;

    let array = match RaidArray::create(level, member_devices, Some(stripe_size)).await {
        Ok(a) => a,
        Err(e) => {
            let toast = shared::toast_oob(&format!("Failed to create array: {e}"), "error");
            let table = ArraysTable { arrays: gather_arrays(&state).await };
            return Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response();
        }
    };

    let array_id = array.array_id();
    let level = array.level();
    let member_count = array.member_count();
    let capacity_bytes = array.capacity_bytes();
    let stripe = array.stripe_size();
    let arc_array = Arc::new(array);

    {
        let mut vm = state.volume_manager.lock().await;
        vm.add_backing_device(array_id, arc_array.clone() as Arc<dyn BlockDevice>)
            .await;
    }

    let info = ArrayInfo {
        array: arc_array,
        level,
        member_count,
        capacity_bytes,
        stripe_size: stripe,
    };
    {
        let mut arrays = state.arrays.write().await;
        arrays.insert(array_id, info);
    }

    metrics::gauge!("stormblock_arrays_total").set(state.arrays.read().await.len() as f64);

    let toast = shared::toast_oob("Array created", "success");
    let table = ArraysTable { arrays: gather_arrays(&state).await };
    Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response()
}

pub async fn delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            let toast = shared::toast_oob("Invalid UUID", "error");
            let table = ArraysTable { arrays: gather_arrays(&state).await };
            return Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response();
        }
    };

    let array_id = RaidArrayId(uuid);

    {
        let vm = state.volume_manager.lock().await;
        let vols = vm.list_volumes().await;
        if !vols.is_empty() {
            let toast = shared::toast_oob("Cannot delete array while volumes exist", "error");
            let table = ArraysTable { arrays: gather_arrays(&state).await };
            return Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response();
        }
    }

    let mut arrays = state.arrays.write().await;
    match arrays.remove(&array_id) {
        Some(_) => {
            metrics::gauge!("stormblock_arrays_total").set(arrays.len() as f64);
            drop(arrays);
            let toast = shared::toast_oob("Array deleted", "success");
            let table = ArraysTable { arrays: gather_arrays(&state).await };
            Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response()
        }
        None => {
            drop(arrays);
            let toast = shared::toast_oob("Array not found", "error");
            let table = ArraysTable { arrays: gather_arrays(&state).await };
            Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response()
        }
    }
}
