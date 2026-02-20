//! Exports page — GET /ui/exports, GET /ui/exports/table,
//! POST /ui/exports, DELETE /ui/exports/{id}

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Response};
use axum::Form;
use serde::Deserialize;
use uuid::Uuid;

use crate::mgmt::{AppState, ExportEntry, ExportProtocol, ExportStatus};
use crate::mgmt::config::human_size;
use crate::volume::VolumeId;
use super::shared::{self, filters};

/// Volume info for the export form dropdown.
pub struct VolumeOption {
    pub id: Uuid,
    pub name: String,
    pub virtual_size_human: String,
}

/// Export info for templates.
pub struct ExportRow {
    pub id: Uuid,
    pub volume_id: Uuid,
    pub protocol: String,
    pub target_id: String,
    pub status: String,
}

#[derive(Template)]
#[template(path = "exports.html")]
struct ExportsPage {
    active: &'static str,
    exports: Vec<ExportRow>,
    volumes: Vec<VolumeOption>,
}

#[derive(Template)]
#[template(path = "exports_table.html")]
struct ExportsTable {
    exports: Vec<ExportRow>,
}

#[derive(Deserialize)]
pub struct CreateExportForm {
    pub volume_id: Uuid,
    pub protocol: String,
    pub target_id: Option<String>,
}

async fn gather_exports(state: &AppState) -> Vec<ExportRow> {
    let exports = state.exports.read().await;
    exports
        .iter()
        .map(|e| ExportRow {
            id: e.id,
            volume_id: e.volume_id,
            protocol: e.protocol.to_string(),
            target_id: e.target_id.clone(),
            status: match e.status {
                ExportStatus::Active => "active".to_string(),
                ExportStatus::PendingRestart => "pending_restart".to_string(),
            },
        })
        .collect()
}

async fn gather_volume_options(state: &AppState) -> Vec<VolumeOption> {
    let vm = state.volume_manager.lock().await;
    let vols = vm.list_volumes().await;
    vols.iter()
        .map(|(id, name, vsize, _)| VolumeOption {
            id: id.0,
            name: name.clone(),
            virtual_size_human: human_size(*vsize),
        })
        .collect()
}

pub async fn list_page(State(state): State<Arc<AppState>>) -> Response {
    let exports = gather_exports(&state).await;
    let volumes = gather_volume_options(&state).await;
    shared::render(&ExportsPage {
        active: "exports",
        exports,
        volumes,
    })
}

pub async fn table_partial(State(state): State<Arc<AppState>>) -> Response {
    let exports = gather_exports(&state).await;
    shared::render(&ExportsTable { exports })
}

pub async fn create(
    State(state): State<Arc<AppState>>,
    Form(form): Form<CreateExportForm>,
) -> Response {
    let protocol = match form.protocol.as_str() {
        "iscsi" => ExportProtocol::Iscsi,
        "nvmeof" => ExportProtocol::Nvmeof,
        _ => {
            let toast = shared::toast_oob("Invalid protocol", "error");
            let table = ExportsTable { exports: gather_exports(&state).await };
            return Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response();
        }
    };

    // Verify volume exists
    let vol_id = VolumeId(form.volume_id);
    {
        let vm = state.volume_manager.lock().await;
        if vm.get_volume(&vol_id).is_none() {
            let toast = shared::toast_oob("Volume not found", "error");
            let table = ExportsTable { exports: gather_exports(&state).await };
            return Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response();
        }
    }

    let target_id = form
        .target_id
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| match protocol {
            ExportProtocol::Iscsi => format!("iqn.2024.io.stormblock:{}", form.volume_id),
            ExportProtocol::Nvmeof => format!("nqn.2024.io.stormblock:{}", form.volume_id),
        });

    let entry = ExportEntry {
        id: Uuid::new_v4(),
        volume_id: form.volume_id,
        protocol,
        target_id,
        status: ExportStatus::PendingRestart,
    };

    {
        let mut exports = state.exports.write().await;
        exports.push(entry);
        metrics::gauge!("stormblock_exports_total").set(exports.len() as f64);
    }

    let toast = shared::toast_oob("Export created", "success");
    let table = ExportsTable { exports: gather_exports(&state).await };
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
            let table = ExportsTable { exports: gather_exports(&state).await };
            return Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response();
        }
    };

    let mut exports = state.exports.write().await;
    let before = exports.len();
    exports.retain(|e| e.id != uuid);
    if exports.len() < before {
        metrics::gauge!("stormblock_exports_total").set(exports.len() as f64);
        drop(exports);
        let toast = shared::toast_oob("Export deleted", "success");
        let table = ExportsTable { exports: gather_exports(&state).await };
        Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response()
    } else {
        drop(exports);
        let toast = shared::toast_oob("Export not found", "error");
        let table = ExportsTable { exports: gather_exports(&state).await };
        Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response()
    }
}
