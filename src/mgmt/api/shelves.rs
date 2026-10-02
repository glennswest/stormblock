//! `/api/v1/shelves` and `/api/v1/spares` — a shelf laid out as RAID sets
//! with hot spares (#252).
//!
//! A shelf is not a stored object: it is the name its sets and spares carry
//! as their pool, on their own superblocks. `POST /api/v1/shelves` lays one
//! out in a call — N sets of a level, the spares beside them — and `GET`
//! groups what the engine holds by that name.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::arrays::{build_array, check_free, drives_by_uuid, full_response, ArrayResponse};
use super::{ApiError, ListResponse};
use crate::drive::BlockDevice;
use crate::mgmt::config::human_size;
use crate::mgmt::AppState;
use crate::raid::{CreateOptions, RaidLevel};

#[derive(Debug, Deserialize)]
pub struct ShelfRequest {
    /// The shelf's name: each set's spare pool and the `shelf` rung of its
    /// failure domain. Sets are named `<name>-a`, `<name>-b`, ….
    pub name: String,
    /// The shelf's drives, in bay order. The last `spares` become spares; the
    /// rest are dealt into `sets` sets of consecutive drives.
    pub drive_uuids: Vec<Uuid>,
    /// Default RAID-6.
    #[serde(default = "raid6")]
    pub level: RaidLevel,
    /// Default 2.
    #[serde(default = "two")]
    pub sets: usize,
    /// Hot spares, in the shelf's own pool. Default 2.
    #[serde(default = "two")]
    pub spares: usize,
    #[serde(default = "default_stripe_kb")]
    pub stripe_kb: u64,
    /// Overwrite drives carrying the superblock of an array this engine
    /// does not hold.
    #[serde(default)]
    pub force: bool,
}

fn raid6() -> RaidLevel {
    RaidLevel::Raid6
}

fn two() -> usize {
    2
}

fn default_stripe_kb() -> u64 {
    64
}

#[derive(Debug, Serialize)]
pub struct ShelfResponse {
    pub name: String,
    /// The worst of its sets: `clean`, `degraded`, `rebuilding`, `failed`.
    pub state: String,
    pub usable_bytes: u64,
    pub usable_human: String,
    pub sets: Vec<ArrayResponse>,
    pub spares: Vec<SpareResponse>,
}

#[derive(Debug, Serialize)]
pub struct SpareResponse {
    pub uuid: Uuid,
    /// "" = the global pool.
    pub pool: String,
    pub drive_uuid: Uuid,
    pub path: String,
    pub serial: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub wwn: String,
    pub capacity_bytes: u64,
}

fn spare_response(s: &crate::raid::spares::Spare) -> SpareResponse {
    let id = s.device.id();
    let drive = s.device.drive_id();
    SpareResponse {
        uuid: s.uuid,
        pool: s.pool.clone(),
        drive_uuid: id.uuid,
        path: id.path.clone(),
        serial: drive.serial,
        wwn: drive.wwn,
        capacity_bytes: s.device.capacity_bytes(),
    }
}

fn worst(states: impl Iterator<Item = &'static str>) -> &'static str {
    let rank = |s: &str| match s {
        "failed" => 3,
        "degraded" => 2,
        "rebuilding" => 1,
        _ => 0,
    };
    states.max_by_key(|s| rank(s)).unwrap_or("clean")
}

async fn shelf_view(state: &AppState, name: &str) -> Option<ShelfResponse> {
    let arrays: Vec<_> = state
        .arrays
        .read()
        .await
        .iter()
        .filter(|(_, i)| i.array.pool() == name)
        .map(|(id, i)| (*id, i.clone()))
        .collect();
    let spares: Vec<SpareResponse> = state.spares.list().iter().filter(|s| s.pool == name).map(spare_response).collect();
    if arrays.is_empty() && spares.is_empty() {
        return None;
    }
    let mut sets = Vec::new();
    for (id, info) in &arrays {
        sets.push(full_response(state, *id, info).await);
    }
    sets.sort_by(|a, b| a.name.cmp(&b.name));
    let usable: u64 = sets.iter().map(|s| s.capacity_bytes).sum();
    Some(ShelfResponse {
        name: name.to_string(),
        state: worst(sets.iter().map(|s| s.status.state)).to_string(),
        usable_bytes: usable,
        usable_human: human_size(usable),
        sets,
        spares,
    })
}

async fn list_shelves(State(state): State<Arc<AppState>>) -> Response {
    let mut names: Vec<String> = state.arrays.read().await.values().map(|i| i.array.pool()).collect();
    names.extend(state.spares.list().into_iter().map(|s| s.pool));
    names.retain(|n| !n.is_empty());
    names.sort();
    names.dedup();
    let mut items = Vec::new();
    for n in names {
        if let Some(v) = shelf_view(&state, &n).await {
            items.push(v);
        }
    }
    let count = items.len();
    Json(ListResponse { items, count }).into_response()
}

async fn get_shelf(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    match shelf_view(&state, &name).await {
        Some(v) => Json(v).into_response(),
        None => ApiError::not_found(format!("no sets or spares in shelf '{name}'")),
    }
}

/// How `n` drives deal into `sets` sets: as even as they go, the first sets
/// one larger when it does not divide.
pub fn deal(n: usize, sets: usize) -> Vec<usize> {
    (0..sets).map(|i| n / sets + usize::from(i < n % sets)).collect()
}

/// `POST /api/v1/shelves` — lay a shelf out.
async fn create_shelf(State(state): State<Arc<AppState>>, Json(req): Json<ShelfRequest>) -> Response {
    if req.name.is_empty() || req.name.len() > 60 || req.name.contains('/') || req.name.contains('=') {
        return ApiError::bad_request("a shelf name is 1-60 bytes, without '/' or '='");
    }
    if req.sets == 0 {
        return ApiError::bad_request("sets must be at least 1");
    }
    if shelf_view(&state, &req.name).await.is_some() {
        return ApiError::conflict(format!("shelf '{}' already has sets or spares", req.name));
    }
    let n = req.drive_uuids.len();
    if req.spares >= n {
        return ApiError::bad_request(format!("{n} drives cannot hold {} spares and any set", req.spares));
    }
    let sizes = deal(n - req.spares, req.sets);
    let need = req.level.min_members();
    if let Some(small) = sizes.iter().find(|s| **s < need || (req.level == RaidLevel::Raid10 && **s % 2 != 0)) {
        return ApiError::bad_request(format!(
            "{} drives in {} {} set(s) leaves a set of {small}; {} needs at least {need}{}",
            n - req.spares,
            req.sets,
            req.level,
            req.level,
            if req.level == RaidLevel::Raid10 { ", and an even number" } else { "" }
        ));
    }
    let drives = match drives_by_uuid(&state, &req.drive_uuids).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    if let Err(r) = check_free(&state, &drives, req.force).await {
        return r;
    }

    let mut made = Vec::new();
    let mut at = 0;
    for (i, size) in sizes.iter().enumerate() {
        let members: Vec<Arc<dyn BlockDevice>> = drives[at..at + size].to_vec();
        at += size;
        let set_name = format!("{}-{}", req.name, set_letter(i));
        let opts = CreateOptions {
            level: req.level,
            members,
            stripe_size: Some(req.stripe_kb * 1024),
            name: set_name.clone(),
            pool: req.name.clone(),
        };
        match build_array(&state, opts, false).await {
            Ok(a) => made.push(a),
            Err(r) => {
                // Take back what was made, so a retry starts from blank drives.
                for a in &made {
                    let id = a.array_id();
                    let _ = state.volume_manager.lock().await.remove_array(&id).await;
                    state.arrays.write().await.remove(&id);
                    a.wipe().await;
                }
                tracing::error!("shelf '{}': set {set_name} failed; the sets made before it are undone", req.name);
                return r;
            }
        }
    }
    for d in &drives[at..] {
        if let Err(e) = state.spares.add(d.clone(), &req.name).await {
            tracing::warn!("shelf '{}': spare {}: {e}", req.name, d.id().path);
        }
    }
    tracing::info!(
        "shelf '{}': {} {} set(s) of {:?} drives, {} spare(s)",
        req.name,
        req.sets,
        req.level,
        sizes,
        req.spares
    );
    match shelf_view(&state, &req.name).await {
        Some(v) => (axum::http::StatusCode::CREATED, Json(v)).into_response(),
        None => ApiError::internal("shelf vanished after it was laid out"),
    }
}

fn set_letter(i: usize) -> String {
    let letters = b"abcdefghijklmnopqrstuvwxyz";
    if i < 26 {
        (letters[i] as char).to_string()
    } else {
        format!("{}", i + 1)
    }
}

#[derive(Debug, Deserialize)]
pub struct SpareRequest {
    pub drive_uuid: Uuid,
    /// The pool: a shelf's name, or "" (default) for the global pool.
    #[serde(default)]
    pub pool: String,
    #[serde(default)]
    pub force: bool,
}

async fn list_spares(State(state): State<Arc<AppState>>) -> Response {
    let items: Vec<SpareResponse> = state.spares.list().iter().map(spare_response).collect();
    let count = items.len();
    Json(ListResponse { items, count }).into_response()
}

async fn add_spare(State(state): State<Arc<AppState>>, Json(req): Json<SpareRequest>) -> Response {
    let dev = match drives_by_uuid(&state, &[req.drive_uuid]).await {
        Ok(mut d) => d.remove(0),
        Err(r) => return r,
    };
    if let Err(r) = check_free(&state, std::slice::from_ref(&dev), req.force).await {
        return r;
    }
    match state.spares.add(dev, &req.pool).await {
        Ok(uuid) => {
            let s = state.spares.list().into_iter().find(|s| s.uuid == uuid);
            match s {
                Some(s) => (axum::http::StatusCode::CREATED, Json(spare_response(&s))).into_response(),
                None => ApiError::internal("spare vanished (taken by a failed set already?)"),
            }
        }
        Err(e) => ApiError::bad_request(e.to_string()),
    }
}

async fn remove_spare(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let Ok(uuid) = id.parse::<Uuid>() else {
        return ApiError::bad_request(format!("invalid UUID: {id}"));
    };
    match state.spares.remove(uuid).await {
        Ok(_) => axum::http::StatusCode::NO_CONTENT.into_response(),
        Err(e) => ApiError::not_found(e.to_string()),
    }
}

pub fn shelves_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list_shelves).post(create_shelf))
        .route("/{name}", get(get_shelf))
        .with_state(state)
}

pub fn spares_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list_spares).post(add_spare))
        .route("/{id}", axum::routing::delete(remove_spare))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::deal;

    #[test]
    fn deals_evenly() {
        assert_eq!(deal(22, 2), vec![11, 11]);
        assert_eq!(deal(23, 2), vec![12, 11]);
        assert_eq!(deal(16, 3), vec![6, 5, 5]);
    }
}
