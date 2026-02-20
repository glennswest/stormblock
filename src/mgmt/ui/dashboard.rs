//! Dashboard page — GET /ui/

use std::sync::Arc;

use askama::Template;
use axum::extract::State;
use axum::response::Response;

use crate::mgmt::AppState;
use crate::mgmt::config::human_size;
use super::shared;

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTemplate {
    active: &'static str,
    drive_count: usize,
    total_drive_capacity: String,
    array_count: usize,
    total_array_capacity: String,
    volume_count: usize,
    total_volume_size: String,
    export_count: usize,
    export_summary: String,
    allocated_human: String,
    capacity_pct: u64,
}

pub async fn index(State(state): State<Arc<AppState>>) -> Response {
    let drives = state.drives.read().await;
    let drive_count = drives.len();
    let total_drive_bytes: u64 = drives.iter().map(|d| d.device.capacity_bytes()).sum();
    drop(drives);

    let arrays = state.arrays.read().await;
    let array_count = arrays.len();
    let total_array_bytes: u64 = arrays.values().map(|a| a.capacity_bytes).sum();
    drop(arrays);

    let vm = state.volume_manager.lock().await;
    let vols = vm.list_volumes().await;
    let volume_count = vols.len();
    let total_vol_bytes: u64 = vols.iter().map(|(_, _, vsize, _)| vsize).sum();
    let total_allocated: u64 = vols.iter().map(|(_, _, _, alloc)| alloc).sum();
    drop(vm);

    let exports = state.exports.read().await;
    let export_count = exports.len();
    let iscsi_count = exports.iter().filter(|e| e.protocol == crate::mgmt::ExportProtocol::Iscsi).count();
    let nvmeof_count = exports.iter().filter(|e| e.protocol == crate::mgmt::ExportProtocol::Nvmeof).count();
    drop(exports);

    let export_summary = if export_count == 0 {
        "none".to_string()
    } else {
        let mut parts = Vec::new();
        if iscsi_count > 0 { parts.push(format!("{iscsi_count} iSCSI")); }
        if nvmeof_count > 0 { parts.push(format!("{nvmeof_count} NVMe-oF")); }
        parts.join(", ")
    };

    let capacity_pct = if total_array_bytes > 0 {
        (total_allocated * 100) / total_array_bytes
    } else {
        0
    };

    let tmpl = DashboardTemplate {
        active: "dashboard",
        drive_count,
        total_drive_capacity: human_size(total_drive_bytes),
        array_count,
        total_array_capacity: human_size(total_array_bytes),
        volume_count,
        total_volume_size: human_size(total_vol_bytes),
        export_count,
        export_summary,
        allocated_human: human_size(total_allocated),
        capacity_pct,
    };

    shared::render(&tmpl)
}
