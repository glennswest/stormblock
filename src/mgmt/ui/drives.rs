//! Drives page — GET /ui/drives, GET /ui/drives/table

use std::sync::Arc;

use askama::Template;
use axum::extract::State;
use axum::response::Response;

use crate::mgmt::AppState;
use crate::mgmt::config::human_size;
use super::shared;

/// Drive info for templates.
pub struct DriveRow {
    pub path: String,
    pub model: String,
    pub serial: String,
    pub device_type: String,
    pub capacity_human: String,
    pub block_size: u32,
}

#[derive(Template)]
#[template(path = "drives.html")]
struct DrivesPage {
    active: &'static str,
    drives: Vec<DriveRow>,
}

#[derive(Template)]
#[template(path = "drives_table.html")]
struct DrivesTable {
    drives: Vec<DriveRow>,
}

async fn gather_drives(state: &AppState) -> Vec<DriveRow> {
    let drives = state.drives.read().await;
    drives
        .iter()
        .map(|d| {
            let id = d.device.id();
            DriveRow {
                path: d.path.clone(),
                model: id.model.clone(),
                serial: id.serial.clone(),
                device_type: d.device.device_type().to_string(),
                capacity_human: human_size(d.device.capacity_bytes()),
                block_size: d.device.block_size(),
            }
        })
        .collect()
}

pub async fn list_page(State(state): State<Arc<AppState>>) -> Response {
    let drives = gather_drives(&state).await;
    shared::render(&DrivesPage {
        active: "drives",
        drives,
    })
}

pub async fn table_partial(State(state): State<Arc<AppState>>) -> Response {
    let drives = gather_drives(&state).await;
    shared::render(&DrivesTable { drives })
}
