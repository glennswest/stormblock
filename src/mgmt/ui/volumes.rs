//! Volumes page — GET /ui/volumes, GET /ui/volumes/table,
//! POST /ui/volumes, POST /ui/volumes/{id}/snapshot, DELETE /ui/volumes/{id}

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Response};
use axum::Form;
use serde::Deserialize;
use uuid::Uuid;

use crate::mgmt::AppState;
use crate::mgmt::config::{human_size, parse_size};
use crate::raid::RaidArrayId;
use crate::volume::VolumeId;
use super::shared::{self, filters};
use super::arrays::ArrayRow;

/// Volume info for templates.
pub struct VolumeRow {
    pub id: Uuid,
    pub name: String,
    pub virtual_size_human: String,
    pub allocated_human: String,
}

#[derive(Template)]
#[template(path = "volumes.html")]
struct VolumesPage {
    active: &'static str,
    volumes: Vec<VolumeRow>,
    arrays: Vec<ArrayRow>,
}

#[derive(Template)]
#[template(path = "volumes_table.html")]
struct VolumesTable {
    volumes: Vec<VolumeRow>,
}

#[derive(Deserialize)]
pub struct CreateVolumeForm {
    pub name: String,
    pub size: String,
    pub array_id: Uuid,
}

#[derive(Deserialize)]
pub struct SnapshotForm {
    pub name: String,
    pub source_volume_id: Uuid,
}

async fn gather_volumes(state: &AppState) -> Vec<VolumeRow> {
    let vm = state.volume_manager.lock().await;
    let vols = vm.list_volumes().await;
    vols.iter()
        .map(|(id, name, vsize, allocated)| VolumeRow {
            id: id.0,
            name: name.clone(),
            virtual_size_human: human_size(*vsize),
            allocated_human: human_size(*allocated),
        })
        .collect()
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
    let volumes = gather_volumes(&state).await;
    let arrays = gather_arrays(&state).await;
    shared::render(&VolumesPage {
        active: "volumes",
        volumes,
        arrays,
    })
}

pub async fn table_partial(State(state): State<Arc<AppState>>) -> Response {
    let volumes = gather_volumes(&state).await;
    shared::render(&VolumesTable { volumes })
}

pub async fn create(
    State(state): State<Arc<AppState>>,
    Form(form): Form<CreateVolumeForm>,
) -> Response {
    let size = match parse_size(&form.size) {
        Ok(s) => s,
        Err(e) => {
            let toast = shared::toast_oob(&format!("Invalid size: {e}"), "error");
            let table = VolumesTable { volumes: gather_volumes(&state).await };
            return Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response();
        }
    };

    let array_id = RaidArrayId(form.array_id);
    {
        let arrays = state.arrays.read().await;
        if !arrays.contains_key(&array_id) {
            let toast = shared::toast_oob("Array not found", "error");
            let table = VolumesTable { volumes: gather_volumes(&state).await };
            return Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response();
        }
    }

    let mut vm = state.volume_manager.lock().await;
    match vm.create_volume(&form.name, size, array_id).await {
        Ok(_vol_id) => {
            metrics::gauge!("stormblock_volumes_total").set(vm.list_volumes().await.len() as f64);
            drop(vm);
            let toast = shared::toast_oob(&format!("Volume '{}' created", form.name), "success");
            let table = VolumesTable { volumes: gather_volumes(&state).await };
            Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response()
        }
        Err(e) => {
            drop(vm);
            let toast = shared::toast_oob(&format!("Failed: {e}"), "error");
            let table = VolumesTable { volumes: gather_volumes(&state).await };
            Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response()
        }
    }
}

pub async fn snapshot(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            let toast = shared::toast_oob("Invalid UUID", "error");
            let table = VolumesTable { volumes: gather_volumes(&state).await };
            return Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response();
        }
    };

    let source_id = VolumeId(uuid);
    let snap_name = format!("snap-{}", &uuid.to_string()[..8]);

    let mut vm = state.volume_manager.lock().await;
    match vm.create_snapshot(source_id, &snap_name).await {
        Ok(_snap_id) => {
            metrics::gauge!("stormblock_volumes_total").set(vm.list_volumes().await.len() as f64);
            drop(vm);
            let toast = shared::toast_oob(&format!("Snapshot '{snap_name}' created"), "success");
            let table = VolumesTable { volumes: gather_volumes(&state).await };
            Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response()
        }
        Err(e) => {
            drop(vm);
            let toast = shared::toast_oob(&format!("Snapshot failed: {e}"), "error");
            let table = VolumesTable { volumes: gather_volumes(&state).await };
            Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response()
        }
    }
}

/// Snapshot from the form (POST /ui/volumes/snapshot with form data).
pub async fn snapshot_form(
    State(state): State<Arc<AppState>>,
    Form(form): Form<SnapshotForm>,
) -> Response {
    let source_id = VolumeId(form.source_volume_id);

    let mut vm = state.volume_manager.lock().await;
    match vm.create_snapshot(source_id, &form.name).await {
        Ok(_snap_id) => {
            metrics::gauge!("stormblock_volumes_total").set(vm.list_volumes().await.len() as f64);
            drop(vm);
            let toast = shared::toast_oob(&format!("Snapshot '{}' created", form.name), "success");
            let table = VolumesTable { volumes: gather_volumes(&state).await };
            Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response()
        }
        Err(e) => {
            drop(vm);
            let toast = shared::toast_oob(&format!("Snapshot failed: {e}"), "error");
            let table = VolumesTable { volumes: gather_volumes(&state).await };
            Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response()
        }
    }
}

pub async fn delete(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let uuid = match id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => {
            let toast = shared::toast_oob("Invalid UUID", "error");
            let table = VolumesTable { volumes: gather_volumes(&state).await };
            return Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response();
        }
    };

    let vol_id = VolumeId(uuid);

    {
        let exports = state.exports.read().await;
        if exports.iter().any(|e| e.volume_id == uuid) {
            let toast = shared::toast_oob("Cannot delete volume with active exports", "error");
            let table = VolumesTable { volumes: gather_volumes(&state).await };
            return Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response();
        }
    }

    let mut vm = state.volume_manager.lock().await;
    match vm.delete_volume(vol_id).await {
        Ok(()) => {
            metrics::gauge!("stormblock_volumes_total").set(vm.list_volumes().await.len() as f64);
            drop(vm);
            let toast = shared::toast_oob("Volume deleted", "success");
            let table = VolumesTable { volumes: gather_volumes(&state).await };
            Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response()
        }
        Err(e) => {
            drop(vm);
            let toast = shared::toast_oob(&format!("Delete failed: {e}"), "error");
            let table = VolumesTable { volumes: gather_volumes(&state).await };
            Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response()
        }
    }
}
