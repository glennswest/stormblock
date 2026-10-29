//! `/api/v1/arrays` — RAID arrays ("sets"): create, assemble, look at, fail
//! and replace members, scrub, delete (#252, #168).

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{ApiError, ListResponse};
use crate::drive::BlockDevice;
use crate::mgmt::config::human_size;
use crate::mgmt::raid_sets;
use crate::mgmt::{AppState, ArrayInfo};
use crate::raid::rebuild::{RebuildConfig, ScrubConfig};
use crate::raid::{ArrayStatus, CreateOptions, RaidArray, RaidArrayId, RaidLevel};

#[derive(Debug, Serialize)]
pub struct ArrayResponse {
    pub id: Uuid,
    /// The set's name (`shelf1-a`), "" when it was given none.
    pub name: String,
    /// The spare pool it takes spares from — its shelf. "" = global only.
    pub pool: String,
    pub level: String,
    pub member_count: usize,
    pub capacity_bytes: u64,
    pub capacity_human: String,
    pub stripe_size: u64,
    pub stripe_human: String,
    /// Bytes of data on each member (after the 1 MiB of superblock and bitmap).
    pub member_data_bytes: u64,
    /// Bumped on every change of state; the newest superblock wins at assembly.
    pub events: u64,
    /// `clean`, `degraded`, `rebuilding`, `failed`; rebuild and scrub progress.
    pub status: ArrayStatus,
    pub members: Vec<MemberResponse>,
    /// The slab this array's storage is (#150).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slab: Option<ArraySlab>,
    /// Volumes on it: pinned to it, or with a leg there.
    pub volumes: Vec<ArrayVolume>,
}

#[derive(Debug, Serialize)]
pub struct ArraySlab {
    pub id: Uuid,
    /// Only volumes pinned to it allocate on it.
    pub dedicated: bool,
    pub role: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    /// Whether it carries the records of its own volumes, so a head that
    /// reassembles the members can adopt it.
    pub self_describing: bool,
    /// Its failure domain (`shelf=…/set=…/drive=raid-…`).
    pub domain: String,
}

#[derive(Debug, Serialize)]
pub struct ArrayVolume {
    pub id: Uuid,
    pub name: String,
    pub pinned: bool,
}

#[derive(Debug, Serialize)]
pub struct MemberResponse {
    /// The slot.
    pub index: usize,
    /// What `DELETE .../members/{member_id}` takes — without it an
    /// orchestrator can add members but never surgically remove one.
    pub uuid: Uuid,
    pub state: String,
    /// "" when the drive is not here (assembled without it).
    pub device_path: String,
    /// The drive, the way stormdrive names it — what tells someone which
    /// bay to pull.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drive: Option<MemberDrive>,
    /// The drive's registration labels (`shelf=…/bay=…`), when it has any.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub labels: String,
    /// For a rebuilding member: bytes of its data rebuilt so far.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rebuilt_bytes: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct MemberDrive {
    pub uuid: Uuid,
    pub serial: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub wwn: String,
    pub model: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateArrayRequest {
    pub level: RaidLevel,
    pub drive_uuids: Vec<Uuid>,
    #[serde(default = "default_stripe_kb")]
    pub stripe_kb: u64,
    /// The array is one consumer's storage (#150): its slab takes only volumes
    /// pinned to it (`array_id` on a volume create), carries their records,
    /// and deleting the array cannot take anyone else's data. Default true;
    /// `false` puts it in the node's general pool — a set on a shelf.
    #[serde(default = "yes")]
    pub dedicated: bool,
    /// The set's name (≤ 64 bytes).
    #[serde(default)]
    pub name: String,
    /// The spare pool (the shelf) its failed members take spares from.
    #[serde(default)]
    pub pool: String,
    /// Drives to make hot spares in `pool` at the same time.
    #[serde(default)]
    pub spares: Vec<Uuid>,
    /// Overwrite drives carrying the superblock of an array this engine does
    /// not hold. Never overrides a drive in use here.
    #[serde(default)]
    pub force: bool,
}

fn yes() -> bool {
    true
}

fn default_stripe_kb() -> u64 {
    64
}

/// The array's slab and the volumes on it, as the volume manager knows them.
async fn slab_view(state: &AppState, id: RaidArrayId) -> (Option<ArraySlab>, Vec<ArrayVolume>) {
    let vm = state.volume_manager.lock().await;
    let Some(slab_id) = vm.array_slab(&id) else { return (None, Vec::new()) };
    let slab = {
        let reg = state.slab_registry.read().await;
        reg.get(&slab_id).map(|s| ArraySlab {
            id: slab_id.0,
            dedicated: s.is_dedicated(),
            role: s.role().to_string(),
            total_bytes: s.total_slots() * s.slot_size(),
            free_bytes: s.free_slots() * s.slot_size(),
            self_describing: s.has_metadata_region(),
            domain: reg.domain_of(&slab_id).to_string(),
        })
    };
    let volumes = vm
        .volumes_on_slab(slab_id)
        .await
        .into_iter()
        .map(|(v, name, pinned)| ArrayVolume { id: v.0, name, pinned })
        .collect();
    (slab, volumes)
}

pub(crate) async fn full_response(state: &AppState, id: RaidArrayId, info: &ArrayInfo) -> ArrayResponse {
    let a = &info.array;
    let labels: Vec<(Uuid, String, String)> = state
        .drives
        .read()
        .await
        .iter()
        .map(|d| (d.device.id().uuid, d.device.id().path.clone(), d.labels.to_string()))
        .collect();
    let details = a.member_details();
    let members = a
        .members_view()
        .into_iter()
        .zip(details)
        .map(|(m, (_, _, _, path))| {
            let drive = m.drive.as_ref().map(|d| MemberDrive {
                uuid: d.uuid,
                serial: d.serial.clone(),
                wwn: d.wwn.clone(),
                model: d.model.clone(),
                path: d.path.clone(),
            });
            let l = labels.iter().find(|(_, p, _)| *p == path).map(|(_, _, l)| l.clone()).unwrap_or_default();
            MemberResponse {
                index: m.slot,
                uuid: m.uuid,
                state: m.state.to_string(),
                device_path: path,
                drive,
                labels: l,
                rebuilt_bytes: (m.state == crate::raid::RaidMemberState::Rebuilding).then_some(m.rebuilt_to),
            }
        })
        .collect();
    let (slab, volumes) = slab_view(state, id).await;
    ArrayResponse {
        id: id.0,
        name: a.name(),
        pool: a.pool(),
        level: a.level().to_string(),
        member_count: a.member_count(),
        capacity_bytes: a.capacity_bytes(),
        capacity_human: human_size(a.capacity_bytes()),
        stripe_size: a.stripe_size(),
        stripe_human: human_size(a.stripe_size()),
        member_data_bytes: a.data_size(),
        events: a.events(),
        status: a.status(),
        members,
        slab,
        volumes,
    }
}

async fn list_arrays(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "arrays", "method" => "list").increment(1);
    let snapshot: Vec<(RaidArrayId, ArrayInfo)> =
        state.arrays.read().await.iter().map(|(id, i)| (*id, i.clone())).collect();
    let mut items: Vec<ArrayResponse> = Vec::new();
    for (id, info) in &snapshot {
        items.push(full_response(&state, *id, info).await);
    }
    items.sort_by(|a, b| a.pool.cmp(&b.pool).then(a.name.cmp(&b.name)).then(a.id.cmp(&b.id)));
    let count = items.len();
    Json(ListResponse { items, count })
}

fn parse_id(id: &str) -> Result<RaidArrayId, Response> {
    id.parse::<Uuid>().map(RaidArrayId).map_err(|_| ApiError::bad_request(format!("invalid UUID: {id}")))
}

async fn array_of(state: &AppState, id: &str) -> Result<(RaidArrayId, ArrayInfo), Response> {
    let id = parse_id(id)?;
    match state.arrays.read().await.get(&id).cloned() {
        Some(info) => Ok((id, info)),
        None => Err(ApiError::not_found(format!("array {id} not found"))),
    }
}

async fn get_array(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "arrays", "method" => "get").increment(1);
    match array_of(&state, &id).await {
        Ok((id, info)) => Json(full_response(&state, id, &info).await).into_response(),
        Err(r) => r,
    }
}

/// The open drives named by uuid, in the order given.
pub(crate) async fn drives_by_uuid(state: &AppState, uuids: &[Uuid]) -> Result<Vec<Arc<dyn BlockDevice>>, Response> {
    let drives = state.drives.read().await;
    let mut out = Vec::new();
    for u in uuids {
        match drives.iter().find(|d| d.device.id().uuid == *u) {
            Some(d) => out.push(d.device.clone()),
            None => return Err(ApiError::not_found(format!("drive {u} not found"))),
        }
    }
    for (i, u) in uuids.iter().enumerate() {
        if uuids[..i].contains(u) {
            return Err(ApiError::bad_request(format!("drive {u} is named twice")));
        }
    }
    Ok(out)
}

/// What stands in the way of using these drives for something new: in use
/// here (#215), or carrying a RAID superblock of an array not held here.
pub(crate) async fn check_free(state: &AppState, devs: &[Arc<dyn BlockDevice>], force: bool) -> Result<(), Response> {
    for d in devs {
        if let Some(why) = raid_sets::drive_in_use(state, d).await {
            return Err(ApiError::conflict(why));
        }
        {
            let reg = state.slab_registry.read().await;
            if reg.iter().any(|(_, s)| s.device().drive_id().path == d.drive_id().path || s.device().id().path == d.id().path) {
                return Err(ApiError::conflict(format!(
                    "drive {} ({}) holds a slab this engine has registered",
                    d.id().uuid,
                    d.id().path
                )));
            }
        }
        if !force {
            match crate::raid::read_superblock(d).await {
                Ok(Some(sb)) if sb.is_spare() => {
                    return Err(ApiError::conflict(format!(
                        "drive {} carries a hot-spare superblock (pool '{}') that is not in this engine's pool; \
                         assemble it (POST /api/v1/arrays/assemble) or pass force",
                        d.id().uuid,
                        sb.pool
                    )))
                }
                Ok(Some(sb)) => {
                    return Err(ApiError::conflict(format!(
                        "drive {} carries slot {} of array {} ('{}'), which this engine does not hold; \
                         assemble it (POST /api/v1/arrays/assemble) or pass force to overwrite it",
                        d.id().uuid,
                        sb.slot.unwrap_or(0),
                        sb.array_uuid,
                        sb.name
                    )))
                }
                Ok(None) => {}
                Err(e) => {
                    return Err(ApiError::conflict(format!(
                        "drive {} has a damaged RAID superblock ({e}); pass force to overwrite it",
                        d.id().uuid
                    )))
                }
            }
        }
    }
    Ok(())
}

/// Make an array, its slab and its spares; register it. Shared with the
/// shelf layout.
pub(crate) async fn build_array(
    state: &Arc<AppState>,
    opts: CreateOptions,
    dedicated: bool,
) -> Result<Arc<RaidArray>, Response> {
    let array = match RaidArray::create_with(opts).await {
        Ok(a) => Arc::new(a),
        Err(e) => return Err(ApiError::bad_request(format!("failed to create array: {e}"))),
    };
    let id = array.array_id();
    // The domain labels go on before the slab is registered, so it is placed
    // under them from the start.
    let domain = raid_sets::set_domain(&array.pool(), &array.name());
    if !domain.is_empty() {
        state.slab_registry.write().await.label_device(&array.id().path, domain);
    }
    {
        let mut vm = state.volume_manager.lock().await;
        if let Err(e) = vm.add_array_slab(id, array.clone() as Arc<dyn BlockDevice>, dedicated).await {
            array.wipe().await;
            return Err(ApiError::internal(e.to_string()));
        }
    }
    raid_sets::register(state, array.clone()).await;
    Ok(array)
}

async fn create_array(State(state): State<Arc<AppState>>, Json(req): Json<CreateArrayRequest>) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "arrays", "method" => "create").increment(1);
    let members = match drives_by_uuid(&state, &req.drive_uuids).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let spares = match drives_by_uuid(&state, &req.spares).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    if let Some(u) = req.spares.iter().find(|u| req.drive_uuids.contains(u)) {
        return ApiError::bad_request(format!("drive {u} is both a member and a spare"));
    }
    // A retried create whose first attempt did go through finds its drives
    // held, and is told so (#215) rather than formatting over the array.
    let all: Vec<Arc<dyn BlockDevice>> = members.iter().chain(spares.iter()).cloned().collect();
    if let Err(r) = check_free(&state, &all, req.force).await {
        return r;
    }
    let opts = CreateOptions {
        level: req.level,
        members,
        stripe_size: Some(req.stripe_kb * 1024),
        name: req.name.clone(),
        pool: req.pool.clone(),
    };
    let array = match build_array(&state, opts, req.dedicated).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    for s in spares {
        if let Err(e) = state.spares.add(s.clone(), &req.pool).await {
            tracing::warn!("spare {}: {e}", s.id().path);
        }
    }
    let id = array.array_id();
    let info = state.arrays.read().await.get(&id).cloned();
    match info {
        Some(info) => (axum::http::StatusCode::CREATED, Json(full_response(&state, id, &info).await)).into_response(),
        None => ApiError::internal("array vanished after creation"),
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct AssembleRequest {
    /// The drives to look at; default every open drive not already in use.
    #[serde(default)]
    pub drive_uuids: Option<Vec<Uuid>>,
}

/// `POST /api/v1/arrays/assemble` — put arrays back together from their
/// drives' superblocks, and take spares into the pool. What startup does for
/// configured drives, for drives registered since (an `nvme-tcp://` leg).
async fn assemble(State(state): State<Arc<AppState>>, body: Option<Json<AssembleRequest>>) -> Response {
    let req = body.map(|b| b.0).unwrap_or_default();
    let devs = match &req.drive_uuids {
        Some(u) => match drives_by_uuid(&state, u).await {
            Ok(d) => d,
            Err(r) => return r,
        },
        None => state.drives.read().await.iter().map(|d| d.device.clone()).collect(),
    };
    let mut free = Vec::new();
    for d in devs {
        if raid_sets::drive_in_use(&state, &d).await.is_none() {
            free.push(d);
        }
    }
    let (report, _) = raid_sets::assemble_and_adopt(&state, &free).await;
    Json(report).into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct DeleteQuery {
    /// Keep the members' superblocks (default: wipe them, so the drives can
    /// be used again).
    #[serde(default)]
    pub keep_superblocks: bool,
}

async fn delete_array(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<DeleteQuery>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "arrays", "method" => "delete").increment(1);
    let (array_id, info) = match array_of(&state, &id).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    // Refused while any volume is on *this* array — pinned to it or with a
    // leg on its slab — and only then (#150).
    if let Err(e) = state.volume_manager.lock().await.remove_array(&array_id).await {
        return ApiError::conflict(format!("cannot delete array {array_id}: {e}"));
    }
    let mut arrays = state.arrays.write().await;
    arrays.remove(&array_id);
    metrics::gauge!("stormblock_arrays_total").set(arrays.len() as f64);
    drop(arrays);
    if q.keep_superblocks {
        info.array.stop();
    } else {
        info.array.wipe().await;
    }
    axum::http::StatusCode::NO_CONTENT.into_response()
}

/// `POST /api/v1/arrays/{id}/grow` (#318, stormstorage#73): a RAID-1 whose
/// legs were each made larger takes the room, and its slab grows into it.
/// Every member is asked its size again (an NVMe/TCP leg is identified again);
/// the new room is zeroed on every leg, the superblocks written with the new
/// size, then the slab grown. 409 when no leg has room past what the array
/// uses (naming each member and its size), when a member is not in service,
/// while it rebuilds or scrubs, or for a parity set. The served volume is
/// then resized as any volume is. A slab left short by a grow cut off after
/// the superblocks is grown by calling it again.
async fn grow_array(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let (array_id, info) = match array_of(&state, &id).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let report = match info.array.grow().await {
        Ok(r) => r,
        Err(crate::raid::RaidError::MemberIo { member_idx, error }) => {
            return ApiError::internal(format!("growing array {array_id}: member {member_idx}: {error}"));
        }
        Err(e) => return ApiError::conflict(format!("array {array_id}: {e}")),
    };
    let slab = state.volume_manager.lock().await.grow_array_slab(&array_id).await;
    let (before, after, short) = match slab {
        Ok(x) => (Some(x.0), Some(x.1), x.2),
        Err(e) if report.grown => return ApiError::internal(format!("array {array_id} grew; its slab did not: {e}")),
        Err(_) => (None, None, None),
    };
    let slab_grew = matches!((before, after), (Some(b), Some(a)) if a > b);
    if !report.grown && !slab_grew {
        let sizes: Vec<String> =
            report.members.iter().map(|(p, s)| format!("{p} {}", human_size(*s))).collect();
        return ApiError::conflict(format!(
            "array {array_id} has nothing to grow into: it uses {} a member past the first MiB, and its members are {}",
            human_size(report.data_size),
            sizes.join(", ")
        ));
    }
    if let Some(i) = state.arrays.write().await.get_mut(&array_id) {
        i.capacity_bytes = info.array.capacity_bytes();
    }
    Json(serde_json::json!({
        "id": array_id,
        "grown": report.grown,
        "was": report.was,
        "data_size": report.data_size,
        "capacity_bytes": info.array.capacity_bytes(),
        "members": report.members.iter().map(|(p, s)| serde_json::json!({"path": p, "bytes": s})).collect::<Vec<_>>(),
        "slab_slots_before": before,
        "slab_slots": after,
        "slab_grown": slab_grew,
        "slab_short": short,
    }))
    .into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct ReleaseQuery {
    /// Withdraw what serves the array's volumes (exports, LUNs, NVMe
    /// namespaces, ublk and vhost devices) instead of refusing.
    #[serde(default)]
    pub force: bool,
}

/// `POST /api/v1/arrays/{id}/release[?force]` (#296, stormstorage#33): let go
/// of an array for another engine to assemble, **writing nothing to any
/// member**: its volumes and its slab dropped from memory (no persist, no
/// slot-table sync, no erase), the set stopped with no superblock, bitmap or
/// flush write (and refusing any I/O from then on), its member drives closed.
/// Then `POST /api/v1/arrays/assemble` on the other engine brings it up as if
/// this one had died. 404 when the array is not held here (so a repeat is a
/// 404); 409 while a consumer is served one of its volumes (unless `force`,
/// which withdraws them), while it rebuilds or scrubs, or for a volume with
/// extents off the array.
async fn release_array(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<ReleaseQuery>,
) -> Response {
    let (array_id, info) = match array_of(&state, &id).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Some(why) = info.array.release_blocker() {
        return ApiError::conflict(format!("cannot release array {array_id}: {why}"));
    }
    let vols = state.volume_manager.lock().await.volumes_of_array(&array_id).await;
    let mut serving = Vec::new();
    for (v, name) in &vols {
        let w = super::what_is_serving(&state, v.0).await;
        if !w.is_empty() {
            serving.push(format!("{name} ({})", w.join(", ")));
        }
    }
    if !serving.is_empty() {
        if !q.force {
            return ApiError::conflict(format!(
                "cannot release array {array_id}: its volumes are served: {}; detach them, or release with force",
                serving.join("; ")
            ));
        }
        for (v, _) in &vols {
            let exports: Vec<Uuid> =
                state.exports.read().await.iter().filter(|e| e.volume_id == v.0).map(|e| e.id).collect();
            for e in exports {
                super::exports::drop_export(&state, e).await;
            }
            #[cfg(feature = "iscsi")]
            {
                let luns: Vec<u64> = state
                    .lun_entries
                    .read()
                    .await
                    .iter()
                    .filter(|(_, e)| matches!(e.backing, crate::mgmt::LunBacking::Volume { volume_id: x } if x == v.0))
                    .map(|(l, _)| *l)
                    .collect();
                for l in luns {
                    super::luns::detach_lun(&state, l).await;
                }
            }
            let stopping = state.ublk_exports.lock().await.remove(&v.0.to_string());
            stopping.settle(crate::mgmt::ublk_export::DETACH_WAIT).await;
            #[cfg(target_os = "linux")]
            {
                let gone = state.vhost_exports.lock().await.detach(v.0);
                crate::mgmt::vhost_export::stop(gone.into_iter().collect()).await;
            }
            super::v1::release_nvme_namespace(&state, &v.0.to_string()).await;
            crate::mgmt::nvme_hosts::detach(&state, v.0, None, crate::mgmt::nvme_hosts::Release::All).await;
        }
        tracing::warn!("array {array_id}: released with force; withdrawn: {}", serving.join("; "));
        // What force does not withdraw (a published release, a StormFS pin)
        // still refuses: those are let go of deliberately, not by a handover.
        let mut left = Vec::new();
        for (v, name) in &vols {
            let w = super::what_is_serving(&state, v.0).await;
            if !w.is_empty() {
                left.push(format!("{name} ({})", w.join(", ")));
            }
        }
        if !left.is_empty() {
            return ApiError::conflict(format!(
                "cannot release array {array_id}: still held after withdrawing what serves it: {}",
                left.join("; ")
            ));
        }
    }
    let released = match state.volume_manager.lock().await.release_array(&array_id).await {
        Ok(r) => r,
        Err(e) => return ApiError::conflict(format!("cannot release array {array_id}: {e}")),
    };
    if let Err(why) = info.array.release() {
        // A rebuild began in between: the volumes are already let go of in
        // memory, written nowhere; the set stays here, stopped.
        info.array.stop();
        tracing::error!("array {array_id}: released its volumes but not the set: {why}");
    }
    let members: Vec<Uuid> = info.array.members_view().iter().filter_map(|m| m.drive.as_ref().map(|d| d.uuid)).collect();
    state.arrays.write().await.remove(&array_id);
    let mut closed = Vec::new();
    {
        let mut drives = state.drives.write().await;
        drives.retain(|d| {
            let member = members.contains(&d.device.id().uuid);
            if member {
                closed.push(d.path.clone());
            }
            !member
        });
    }
    metrics::gauge!("stormblock_arrays_total").set(state.arrays.read().await.len() as f64);
    tracing::info!(
        "array {array_id}: released to another engine (#296): {} volume(s) let go of, {} drive(s) closed, nothing written",
        released.len(),
        closed.len()
    );
    Json(serde_json::json!({
        "released": array_id,
        "volumes": released.iter().map(|(id, n)| serde_json::json!({"id": id.0, "name": n})).collect::<Vec<_>>(),
        "drives_closed": closed,
    }))
    .into_response()
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
    let (_, info) = match array_of(&state, &id).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let device = match drives_by_uuid(&state, &[req.drive_uuid]).await {
        Ok(mut d) => d.remove(0),
        Err(r) => return r,
    };
    if let Some(why) = raid_sets::drive_in_use(&state, &device).await {
        return ApiError::conflict(why);
    }
    match info.array.add_member(device).await {
        Ok(member_uuid) => (axum::http::StatusCode::CREATED, Json(MemberUuidResponse { member_uuid })).into_response(),
        Err(e) => ApiError::bad_request(format!("failed to add member: {e}")),
    }
}

async fn remove_member(
    State(state): State<Arc<AppState>>,
    Path((id, member_id)): Path<(String, String)>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "arrays", "method" => "remove_member").increment(1);
    let (_, info) = match array_of(&state, &id).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let member_uuid = match member_id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return ApiError::bad_request(format!("invalid member UUID: {member_id}")),
    };
    match info.array.remove_member(member_uuid).await {
        Ok(()) => axum::http::StatusCode::NO_CONTENT.into_response(),
        Err(e) => ApiError::bad_request(format!("failed to remove member: {e}")),
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct FailRequest {
    #[serde(default)]
    pub reason: Option<String>,
}

/// `POST /api/v1/arrays/{id}/members/{slot}/fail` — take a member out (to
/// pull its drive, say). A spare takes the slot if the pool has one.
async fn fail_member(
    State(state): State<Arc<AppState>>,
    Path((id, slot)): Path<(String, usize)>,
    body: Option<Json<FailRequest>>,
) -> Response {
    let (array_id, info) = match array_of(&state, &id).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if slot >= info.array.member_count() {
        return ApiError::not_found(format!("array {array_id} has no slot {slot}"));
    }
    let why = body.and_then(|b| b.0.reason).unwrap_or_else(|| "failed by request".into());
    if !info.array.fail_member(slot, &why) {
        return ApiError::conflict(format!(
            "slot {slot} of array {array_id} was not failed: it is already failed, or failing it would lose data"
        ));
    }
    info.array.persist_if_dirty().await;
    Json(full_response(&state, array_id, &info).await).into_response()
}

#[derive(Debug, Deserialize)]
pub struct ReplaceRequest {
    pub drive_uuid: Uuid,
    #[serde(default)]
    pub force: bool,
}

/// `POST /api/v1/arrays/{id}/members/{slot}/replace` — put a drive into a
/// failed slot (it rebuilds in the background), or replace an active one
/// while it serves: the drive is filled from it and takes the slot (#256).
async fn replace_member(
    State(state): State<Arc<AppState>>,
    Path((id, slot)): Path<(String, usize)>,
    Json(req): Json<ReplaceRequest>,
) -> Response {
    let (array_id, info) = match array_of(&state, &id).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let device = match drives_by_uuid(&state, &[req.drive_uuid]).await {
        Ok(mut d) => d.remove(0),
        Err(r) => return r,
    };
    if let Err(r) = check_free(&state, std::slice::from_ref(&device), req.force).await {
        return r;
    }
    // An active slot is replaced while it serves (#256); a failed one is
    // rebuilt onto the drive.
    let active = info.array.member_states().get(slot).map(|(_, st)| *st) == Some(crate::raid::RaidMemberState::Active);
    let res = if active { info.array.replace_proactively(slot, device).await } else { info.array.replace(slot, device).await };
    match res {
        Ok(_) => (axum::http::StatusCode::ACCEPTED, Json(full_response(&state, array_id, &info).await)).into_response(),
        Err(e) => ApiError::bad_request(format!("cannot replace slot {slot}: {e}")),
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct ScrubRequest {
    /// Rewrite parity (or the other mirror legs) where they disagree.
    /// Default false (#160): with no checksums a mismatch is reported, and
    /// rewritten only when asked, as a volume's scrub does.
    #[serde(default)]
    pub repair: Option<bool>,
    /// Member data bytes a second; 0 or absent = unlimited.
    #[serde(default)]
    pub max_bytes_per_sec: u64,
}

async fn start_scrub(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<ScrubRequest>>,
) -> Response {
    let (_, info) = match array_of(&state, &id).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let req = body.map(|b| b.0).unwrap_or_default();
    let p = info.array.start_scrub(ScrubConfig { max_bytes_per_sec: req.max_bytes_per_sec, repair: req.repair.unwrap_or(false) });
    (axum::http::StatusCode::ACCEPTED, Json(p.status())).into_response()
}

async fn get_scrub(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let (_, info) = match array_of(&state, &id).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    match info.array.scrub_progress() {
        Some(p) => Json(p.status()).into_response(),
        None => ApiError::not_found("no scrub has run on this array since the engine started"),
    }
}

async fn cancel_scrub(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let (_, info) = match array_of(&state, &id).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Some(p) = info.array.scrub_progress() {
        p.cancel();
    }
    axum::http::StatusCode::NO_CONTENT.into_response()
}

#[derive(Debug, Deserialize)]
pub struct RebuildSettings {
    /// Member data bytes a second; 0 = unlimited.
    pub max_bytes_per_sec: u64,
}

/// `PUT /api/v1/arrays/{id}/rebuild` — how fast its rebuilds may go.
async fn set_rebuild(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<RebuildSettings>,
) -> Response {
    let (array_id, info) = match array_of(&state, &id).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    info.array.set_rebuild_config(RebuildConfig { max_bytes_per_sec: req.max_bytes_per_sec, batch_bytes: 0 });
    Json(full_response(&state, array_id, &info).await).into_response()
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(list_arrays).post(create_array))
        .route("/assemble", post(assemble))
        .route("/{id}", get(get_array).delete(delete_array))
        .route("/{id}/release", post(release_array))
        .route("/{id}/grow", post(grow_array))
        .route("/{id}/members", post(add_member))
        // `{m}`: a member uuid to remove one from a mirror; a slot number to
        // fail or replace one.
        .route("/{id}/members/{m}", axum::routing::delete(remove_member))
        .route("/{id}/members/{m}/fail", post(fail_member))
        .route("/{id}/members/{m}/replace", post(replace_member))
        .route("/{id}/scrub", get(get_scrub).post(start_scrub).delete(cancel_scrub))
        .route("/{id}/rebuild", axum::routing::put(set_rebuild))
        .with_state(state)
}
