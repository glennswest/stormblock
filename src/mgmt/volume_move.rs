//! `/api/v1/volumes/{id}/move` (#426, stormvm#29): move a volume onto a
//! named slab, drive or array, or off one, while it is used, with progress.
//!
//! ```text
//! POST   /api/v1/volumes/{id}/move    {"to": {"drive": "/dev/sdc"}}   start (202)
//!                                     {"except": {"drive": "/dev/sdc"}}
//!                                     {"to": {"array": "<id>"}, "except": {"slab": "<id>"}}
//! GET    /api/v1/volumes/{id}/move    progress: to_move, moved, failed, remaining, state
//! DELETE /api/v1/volumes/{id}/move    cancel; ?release=true also clears the target
//! ```
//!
//! The target ([`crate::volume::target`]) is set before anything moves, so
//! every extent the guest allocates meanwhile already lands inside it, and
//! it stays after the move: new extents keep going there. Each leg outside
//! it is moved as a drive drain moves one (#154): its destination chosen
//! under the read locks, the copy made with only the slot's fence held,
//! every map that names the slot rewritten (a golden's shared extent moves
//! for every holder), the source freed once the map that stopped naming it is
//! on disk. Between moves the disks go back to live I/O first.
//!
//! The volume's ublk device, exports and id stay as they are: only where its
//! bytes are changes.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::drive::slab::SlabId;
use crate::drive::slab_registry::SlabRegistry;
use crate::mgmt::api::ApiError;
use crate::mgmt::AppState;
use crate::placement::domain::FailureDomain;
use crate::placement::{PlacementEngine, PlacementError};
use crate::volume::gem::{GlobalExtentMap, Leg};
use crate::volume::target::{Place, Target};
use crate::volume::{VolumeId, VolumeManager};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveState {
    Running,
    /// Every leg of the volume is inside its target.
    Done,
    /// Some legs could not be moved: `reason` says why, `errors` which.
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct MoveStatus {
    pub volume: uuid::Uuid,
    /// Where the volume is going, as asked.
    pub target: String,
    pub state: MoveState,
    /// Legs (data and parity) outside the target when the move started.
    pub to_move: u64,
    pub moved: u64,
    pub failed: u64,
    /// Legs still outside the target.
    pub remaining: u64,
    pub started_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub errors: Vec<String>,
}

struct Job {
    status: Arc<RwLock<MoveStatus>>,
    cancel: tokio::sync::watch::Sender<bool>,
}

/// The moves this engine has run, by volume: the latest one each.
#[derive(Default)]
pub struct Moves {
    jobs: std::sync::Mutex<HashMap<VolumeId, Job>>,
}

impl Moves {
    pub async fn status(&self, id: &VolumeId) -> Option<MoveStatus> {
        let st = self.jobs.lock().unwrap_or_else(|e| e.into_inner()).get(id).map(|j| j.status.clone());
        match st {
            Some(s) => Some(s.read().await.clone()),
            None => None,
        }
    }

    pub async fn is_running(&self, id: &VolumeId) -> bool {
        matches!(self.status(id).await, Some(s) if s.state == MoveState::Running)
    }

    fn cancel(&self, id: &VolumeId) -> bool {
        match self.jobs.lock().unwrap_or_else(|e| e.into_inner()).get(id) {
            Some(j) => j.cancel.send(true).is_ok(),
            None => false,
        }
    }

    fn forget(&self, id: &VolumeId) {
        self.jobs.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
    }
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Why a name did not resolve.
pub enum Unresolved {
    NotFound(String),
    Invalid(String),
}

impl Unresolved {
    pub fn response(self) -> Response {
        match self {
            Unresolved::NotFound(m) => ApiError::not_found(m),
            Unresolved::Invalid(m) => ApiError::bad_request(m),
        }
    }
}

/// The slabs a slab, drive or array names. A drive is its path, its id, its
/// serial or its WWN, as `GET /api/v1/drives` lists them.
pub async fn resolve(state: &AppState, place: &Place) -> Result<BTreeSet<SlabId>, Unresolved> {
    if let Some(s) = &place.slab {
        let id = s.parse::<uuid::Uuid>().map_err(|_| Unresolved::Invalid(format!("slab {s:?}: a slab id")))?;
        let reg = state.slab_registry.read().await;
        return if reg.get(&SlabId(id)).is_some() {
            Ok([SlabId(id)].into())
        } else {
            Err(Unresolved::NotFound(format!("no slab {s} on this node")))
        };
    }
    if let Some(a) = &place.array {
        let id = a.parse::<uuid::Uuid>().map_err(|_| Unresolved::Invalid(format!("array {a:?}: an array id")))?;
        let slab = state.volume_manager.lock().await.array_slab(&crate::raid::RaidArrayId(id));
        return match slab {
            Some(s) => Ok([s].into()),
            None => Err(Unresolved::NotFound(format!("no array {a} with a slab on this node"))),
        };
    }
    if let Some(d) = &place.drive {
        let found = state.listed_drives().await.into_iter().map(|(d, _)| d).find(|i| {
            let id = i.device.id();
            i.path == *d || id.uuid.to_string() == *d || (!id.serial.is_empty() && id.serial == *d) || (!id.wwn.is_empty() && id.wwn == *d)
        });
        let Some(info) = found else {
            return Err(Unresolved::NotFound(format!("no open drive {d}")));
        };
        let reg = state.slab_registry.read().await;
        return Ok(reg
            .iter()
            .filter(|(_, s)| crate::mgmt::api::drives::slab_is_on(s, &info.device, &info.path))
            .map(|(id, _)| *id)
            .collect());
    }
    Err(Unresolved::Invalid("one of slab, drive or array".into()))
}

/// What a move or a create asks for: `to`, `except`, or both.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TargetRequest {
    #[serde(default)]
    pub to: Option<Place>,
    #[serde(default)]
    pub except: Option<Place>,
}

/// A volume's view of what it may be placed on.
pub struct VolumeShape {
    pub role: crate::drive::slab::SlabRole,
    pub extent_size: u64,
    pub copies: usize,
    pub rung: String,
}

/// Resolve a request into a target the volume can live in, or say why not
/// (409 for a target that cannot hold it).
pub async fn build_target(state: &AppState, req: &TargetRequest, shape: &VolumeShape) -> Result<Target, Response> {
    if req.to.is_none() && req.except.is_none() {
        return Err(ApiError::bad_request("a target needs to, except, or both"));
    }
    let mut t = Target::default();
    if let Some(p) = &req.except {
        p.check("except").map_err(ApiError::bad_request)?;
        t.except = resolve(state, p).await.map_err(Unresolved::response)?;
        t.except_asked = Some(p.clone());
    }
    if let Some(p) = &req.to {
        p.check("to").map_err(ApiError::bad_request)?;
        let named = resolve(state, p).await.map_err(Unresolved::response)?;
        let reg = state.slab_registry.read().await;
        let mut usable = BTreeSet::new();
        let mut why = Vec::new();
        for id in &named {
            if t.except.contains(id) {
                why.push(format!("slab {} is also in except", id.0));
            } else if reg.is_dedicated(id) {
                // A dedicated slab holds only the volumes pinned to it (#150):
                // create the volume with `array_id` instead.
                why.push(format!("slab {} is dedicated to the volumes pinned to its array (#150)", id.0));
            } else if reg.role_of(id) != shape.role {
                why.push(format!("slab {} is in the {} role, the volume in the {}", id.0, reg.role_of(id), shape.role));
            } else if !reg.size_ok(id, shape.extent_size) {
                why.push(format!("slab {}'s slots are not the volume's {}-byte extents", id.0, shape.extent_size));
            } else {
                usable.insert(*id);
            }
        }
        if usable.is_empty() {
            let msg = if named.is_empty() {
                format!("{} carries no slab", p.describe())
            } else {
                format!("{} has no slab this volume can live on: {}", p.describe(), why.join("; "))
            };
            return Err(ApiError::conflict(msg));
        }
        // A redundant volume's legs stay apart (#142): the target must hold as
        // many domains at the volume's rung as it keeps copies.
        let mut picked: Vec<FailureDomain> = Vec::new();
        for id in &usable {
            if !reg.collides(id, &picked, &shape.rung) {
                picked.push(reg.domain_of(id));
            }
        }
        if picked.len() < shape.copies {
            return Err(ApiError::conflict(format!(
                "{} holds {} '{}' domain(s); the volume keeps {} copies apart at that rung",
                p.describe(),
                picked.len(),
                shape.rung,
                shape.copies
            )));
        }
        t.to = Some(usable);
        t.to_asked = Some(p.clone());
    }
    Ok(t)
}

/// The shape of a volume, from its handle.
pub fn shape_of(h: &crate::volume::thin::ThinVolumeHandle) -> VolumeShape {
    let r = h.redundancy();
    VolumeShape { role: h.placement_role(), extent_size: h.extent_size(), copies: r.scheme.copies(), rung: r.spread.clone() }
}

fn data_dir(state: &AppState) -> Option<std::path::PathBuf> {
    state.config.management.data_dir.as_ref().map(std::path::PathBuf::from)
}

/// Set a volume's target (at creation, or by a move).
#[allow(clippy::result_large_err)]
pub fn keep_target(state: &AppState, id: VolumeId, t: Option<Target>) -> Result<(), Response> {
    crate::volume::target::keep(data_dir(state).as_deref(), id, t)
        .map_err(|e| ApiError::internal(format!("keeping the placement target: {e}")))
}

/// What `GET /api/v1/volumes/{id}` says under `target`.
pub fn target_json(id: &VolumeId) -> Option<serde_json::Value> {
    crate::volume::target::of(id).map(|t| {
        serde_json::json!({
            "to": t.to_asked,
            "except": t.except_asked,
            "slabs": t.to.as_ref().map(|s| s.iter().map(|x| x.0).collect::<Vec<_>>()),
            "except_slabs": t.except.iter().map(|x| x.0).collect::<Vec<_>>(),
            "describe": t.describe(),
        })
    })
}

/// `POST /api/v1/volumes/{id}/move`.
pub async fn start(State(state): State<Arc<AppState>>, Path(id): Path<String>, Json(req): Json<TargetRequest>) -> Response {
    let uuid = match crate::mgmt::api::volumes::volume_key(&state, &id).await {
        Ok(v) => v.0,
        Err(r) => return r,
    };
    let vid = VolumeId(uuid);
    let handle = state.volume_manager.lock().await.get_volume_handle(&vid);
    let Some(handle) = handle else {
        return ApiError::not_found(format!("volume {uuid} not found"));
    };
    if let Some(pin) = handle.pinned_slab() {
        return ApiError::conflict(format!(
            "volume {uuid} is pinned to slab {} (#150): it is that array's storage and does not move",
            pin.0
        ));
    }
    if state.volume_moves.is_running(&vid).await {
        return ApiError::conflict(format!("volume {uuid} is already moving; cancel it first"));
    }
    let shape = shape_of(&handle);
    let target = match build_target(&state, &req, &shape).await {
        Ok(t) => t,
        Err(r) => return r,
    };
    // Before anything moves: what the guest allocates meanwhile lands inside.
    if let Err(r) = keep_target(&state, vid, Some(target.clone())) {
        return r;
    }
    let status = Arc::new(RwLock::new(MoveStatus {
        volume: uuid,
        target: target.describe(),
        state: MoveState::Running,
        to_move: 0,
        moved: 0,
        failed: 0,
        remaining: 0,
        started_at: now(),
        finished_at: None,
        reason: None,
        errors: Vec::new(),
    }));
    let (cancel, cancel_rx) = tokio::sync::watch::channel(false);
    {
        let mut jobs = state.volume_moves.jobs.lock().unwrap_or_else(|e| e.into_inner());
        jobs.insert(vid, Job { status: status.clone(), cancel });
    }
    let (gem, reg, vm) = (state.gem.clone(), state.slab_registry.clone(), state.volume_manager.clone());
    let st = status.clone();
    tracing::info!(volume = %vid, "move: started, {}", target.describe());
    tokio::spawn(crate::lockwatch::named(format!("the move of volume {vid}"), async move {
        run(vid, shape, gem, reg, vm, st, cancel_rx).await;
    }));
    // Counted before answering, so the first GET already says how much.
    for _ in 0..50 {
        if status.read().await.to_move > 0 || status.read().await.state != MoveState::Running {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let body = status.read().await.clone();
    (axum::http::StatusCode::ACCEPTED, Json(serde_json::json!({
        "move": body,
        "target": target_json(&vid),
    })))
        .into_response()
}

/// `GET /api/v1/volumes/{id}/move`.
pub async fn get(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let uuid = match crate::mgmt::api::volumes::volume_key(&state, &id).await {
        Ok(v) => v.0,
        Err(r) => return r,
    };
    match state.volume_moves.status(&VolumeId(uuid)).await {
        Some(s) => Json(serde_json::json!({ "move": s, "target": target_json(&VolumeId(uuid)) })).into_response(),
        None => ApiError::not_found(format!("no move of volume {uuid} on this engine since it started")),
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct CancelQuery {
    #[serde(default)]
    pub release: bool,
}

/// `DELETE /api/v1/volumes/{id}/move`: stop moving. What has moved stays
/// moved; the target stays unless `?release=true`, which lets the volume's
/// new extents go anywhere again.
pub async fn cancel(State(state): State<Arc<AppState>>, Path(id): Path<String>, Query(q): Query<CancelQuery>) -> Response {
    let uuid = match crate::mgmt::api::volumes::volume_key(&state, &id).await {
        Ok(v) => v.0,
        Err(r) => return r,
    };
    let vid = VolumeId(uuid);
    let running = state.volume_moves.is_running(&vid).await;
    if running {
        state.volume_moves.cancel(&vid);
        for _ in 0..500 {
            if !state.volume_moves.is_running(&vid).await {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    let had_target = crate::volume::target::of(&vid).is_some();
    if !(running || q.release && had_target) {
        return ApiError::not_found(format!("volume {uuid} is not moving"));
    }
    if q.release {
        if let Err(r) = keep_target(&state, vid, None) {
            return r;
        }
    }
    Json(serde_json::json!({
        "move": state.volume_moves.status(&vid).await,
        "target": target_json(&vid),
        "released": q.release && had_target,
    }))
    .into_response()
}

/// A volume deleted: its target and move go with it.
pub fn forget(state: &AppState, id: VolumeId) {
    state.volume_moves.cancel(&id);
    state.volume_moves.forget(&id);
    if crate::volume::target::of(&id).is_some() {
        let _ = crate::volume::target::keep(data_dir(state).as_deref(), id, None);
    }
}

/// The longest a move waits for live I/O between two moves.
const YIELD_MAX: std::time::Duration = std::time::Duration::from_secs(2);

async fn persist_then_release(
    volumes: &Arc<crate::lockwatch::TrackedMutex<VolumeManager>>,
    registry: &Arc<crate::lockwatch::TrackedRwLock<SlabRegistry>>,
    engine: &PlacementEngine,
) {
    VolumeManager::persist_detached(volumes).await;
    let mut reg = registry.write().await;
    engine.release_owed(&mut reg).await;
}

/// One leg outside the target: a data leg of an extent, or a parity leg of a
/// stripe.
#[derive(Clone, Copy)]
enum Pick {
    Data { vext: u64, leg: Leg },
    Parity { stripe: u64, leg: Leg },
}

/// Legs of the volume outside its target, and the first one not stuck.
fn outside(g: &GlobalExtentMap, reg: &SlabRegistry, id: VolumeId, t: &Target, stuck: &HashSet<Leg>) -> (u64, Option<Pick>) {
    let mut n = 0u64;
    let mut pick = None;
    if let Some(it) = g.volume_extents(&id) {
        for (vext, loc) in it {
            for leg in loc.legs() {
                if !t.allows(&leg.slab_id) {
                    n += 1;
                    if pick.is_none() && !stuck.contains(&leg) {
                        pick = Some(Pick::Data { vext, leg });
                    }
                }
            }
        }
    }
    for (slab, _) in reg.iter() {
        if t.allows(slab) {
            continue;
        }
        for (vol, stripe, grp) in g.slab_parity(*slab) {
            if vol != id {
                continue;
            }
            for leg in grp.legs.iter().filter(|l| l.slab_id == *slab) {
                n += 1;
                if pick.is_none() && !stuck.contains(leg) {
                    pick = Some(Pick::Parity { stripe, leg: *leg });
                }
            }
        }
    }
    (n, pick)
}

/// A slab inside the target for a leg leaving `from`: its role and slot
/// size, off the domains of `others` at `rung`; the one the volume's last
/// leg went to while it takes more (one drive, #153), else the most free.
fn destination(reg: &SlabRegistry, t: &Target, from: SlabId, others: &[FailureDomain], rung: &str, last: Option<SlabId>) -> Option<SlabId> {
    let role = reg.role_of(&from);
    let size = reg.get(&from).map(|s| s.slot_size()).unwrap_or(0);
    let ok = |id: &SlabId| {
        *id != from
            && t.allows(id)
            && reg.takes(id, role, size)
            && !reg.collides(id, others, rung)
    };
    if let Some(l) = last.filter(|l| ok(l)) {
        return Some(l);
    }
    let ids: Vec<SlabId> = match &t.to {
        Some(to) => to.iter().copied().collect(),
        None => reg.iter().map(|(id, _)| *id).collect(),
    };
    ids.into_iter()
        .filter(ok)
        .filter_map(|id| reg.get(&id).map(|s| (id, s.free_slots())))
        .max_by_key(|(_, f)| *f)
        .map(|(id, _)| id)
}

async fn run(
    id: VolumeId,
    shape: VolumeShape,
    gem: Arc<crate::lockwatch::TrackedRwLock<GlobalExtentMap>>,
    registry: Arc<crate::lockwatch::TrackedRwLock<SlabRegistry>>,
    volumes: Arc<crate::lockwatch::TrackedMutex<VolumeManager>>,
    status: Arc<RwLock<MoveStatus>>,
    cancel: tokio::sync::watch::Receiver<bool>,
) {
    let finish = |state: MoveState, reason: Option<String>| {
        let status = status.clone();
        async move {
            let mut st = status.write().await;
            st.state = state;
            st.finished_at = Some(now());
            if st.reason.is_none() {
                st.reason = reason;
            }
            tracing::info!(volume = %id, state = ?state, moved = st.moved, failed = st.failed, remaining = st.remaining, "move: ended");
        }
    };
    // A moved slot is rewritten in every map that names it: all in memory.
    let _pin = match crate::volume::gem::pin_resident(&gem).await {
        Ok(p) => p,
        Err(e) => {
            finish(MoveState::Failed, Some(format!("loading extent maps: {e}"))).await;
            return;
        }
    };
    let engine = PlacementEngine::new();
    let mut stuck: HashSet<Leg> = HashSet::new();
    let mut last: Option<SlabId> = None;
    let affine = shape.copies == 1;
    let mut since_persist = 0u32;
    let mut first = true;
    let mut foreground = crate::volume::thin::FOREGROUND_IO.load(std::sync::atomic::Ordering::Relaxed);
    let mut last_move = std::time::Duration::ZERO;
    loop {
        if *cancel.borrow() {
            persist_then_release(&volumes, &registry, &engine).await;
            let remaining = {
                let g = gem.read().await;
                let r = registry.read().await;
                crate::volume::target::of(&id).map(|t| outside(&g, &r, id, &t, &stuck).0).unwrap_or(0)
            };
            status.write().await.remaining = remaining;
            finish(MoveState::Cancelled, None).await;
            return;
        }
        let Some(target) = crate::volume::target::of(&id) else {
            persist_then_release(&volumes, &registry, &engine).await;
            finish(MoveState::Cancelled, Some("the target was cleared".into())).await;
            return;
        };
        let handle = volumes.lock().await.get_volume_handle(&id);
        let Some(handle) = handle else {
            finish(MoveState::Cancelled, Some("the volume was deleted".into())).await;
            return;
        };
        let (remaining, pick) = {
            let g = gem.read().await;
            let r = registry.read().await;
            outside(&g, &r, id, &target, &stuck)
        };
        {
            let mut st = status.write().await;
            if first {
                st.to_move = remaining;
                first = false;
            }
            st.remaining = remaining;
        }
        let Some(pick) = pick else {
            persist_then_release(&volumes, &registry, &engine).await;
            if remaining == 0 {
                finish(MoveState::Done, None).await;
            } else {
                let reason = status.read().await.errors.first().cloned()
                    .unwrap_or_else(|| format!("{remaining} leg(s) could not be moved"));
                finish(MoveState::Failed, Some(format!("{remaining} leg(s) left outside the target: {reason}"))).await;
            }
            return;
        };
        // Live I/O first, as a drain does (#154).
        if crate::volume::thin::FOREGROUND_IO.load(std::sync::atomic::Ordering::Relaxed) != foreground {
            tokio::time::sleep((last_move / 4).min(YIELD_MAX)).await;
        }
        foreground = crate::volume::thin::FOREGROUND_IO.load(std::sync::atomic::Ordering::Relaxed);
        let started = std::time::Instant::now();
        let (leg, res) = match pick {
            Pick::Data { vext, leg } => {
                if handle.is_failed(leg.slab_id) {
                    // Its bytes are not trusted: a copy would carry them.
                    (leg, Err(PlacementError::Other(format!(
                        "on slab {}, which the volume has stopped trusting: resync it, then move", leg.slab_id.0
                    ))))
                } else {
                    let dest = {
                        let g = gem.read().await;
                        let r = registry.read().await;
                        let others: Vec<FailureDomain> = g
                            .lookup(id, vext)
                            .map(|loc| loc.legs().filter(|l| *l != leg).map(|l| r.domain_of(&l.slab_id)).collect())
                            .unwrap_or_default();
                        destination(&r, &target, leg.slab_id, &others, &shape.rung, affine.then_some(last).flatten())
                    };
                    match dest {
                        None => (leg, Err(PlacementError::NoDestination)),
                        Some(d) => {
                            let fence = crate::volume::fence::exclusive(leg).await;
                            let res = engine.migrate_leg_unlocked(&gem, &registry, id, vext, leg, d, &fence).await;
                            drop(fence);
                            if res.is_ok() {
                                last = Some(d);
                            }
                            (leg, res)
                        }
                    }
                }
            }
            Pick::Parity { stripe, leg } => {
                let mut g = gem.write().await;
                let mut r = registry.write().await;
                let others: Vec<FailureDomain> = match g.lookup_parity(id, stripe) {
                    Some(grp) => {
                        let mut o: Vec<FailureDomain> =
                            grp.legs.iter().filter(|l| **l != leg).map(|l| r.domain_of(&l.slab_id)).collect();
                        for v in grp.members(stripe) {
                            if let Some(loc) = g.lookup(id, v) {
                                o.extend(loc.legs().map(|l| r.domain_of(&l.slab_id)));
                            }
                        }
                        o
                    }
                    None => Vec::new(),
                };
                match destination(&r, &target, leg.slab_id, &others, &shape.rung, None) {
                    None => (leg, Err(PlacementError::NoDestination)),
                    Some(d) => {
                        let res = engine.migrate_parity_leg(&mut g, &mut r, id, stripe, leg.slab_id, Some(d)).await;
                        (leg, res)
                    }
                }
            }
        };
        last_move = started.elapsed();
        match res {
            Ok(_) => {
                status.write().await.moved += 1;
                since_persist += 1;
            }
            // In use by an I/O, or changed meanwhile: the next pass looks again.
            Err(PlacementError::Busy { .. }) | Err(PlacementError::ExtentNotFound { .. }) => {}
            Err(e) => {
                stuck.insert(leg);
                let mut st = status.write().await;
                st.failed += 1;
                if st.errors.len() < 32 {
                    let what = match pick {
                        Pick::Data { vext, .. } => format!("extent {vext}"),
                        Pick::Parity { stripe, .. } => format!("parity of stripe {stripe}"),
                    };
                    let e = match e {
                        PlacementError::NoDestination => format!(
                            "no slab {} has room for it apart from its other legs",
                            target.describe()
                        ),
                        other => other.to_string(),
                    };
                    st.errors.push(format!("{what}: {e}"));
                }
            }
        }
        if since_persist >= 64 {
            persist_then_release(&volumes, &registry, &engine).await;
            since_persist = 0;
        }
        tokio::task::yield_now().await;
    }
}
