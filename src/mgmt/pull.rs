//! A volume pulled from a peer engine over NVMe/TCP, by id (#656).
//!
//! The transport a registry sync stands on (the master, 2026-11-18: "the
//! forge sync is registry to registry … stormblock's part shrinks to the
//! transport primitive: copy a volume from a peer engine over NVMe/TCP by
//! id"). The registry's Replication (stormblock-registry#186) calls it once
//! per image; nothing in it names a forge or a site.
//!
//! `POST /api/v1/volumes/pull {"from": "http://peer:9090", "volume": "<id or
//! name on the peer>"}` (optional: `name`, `namespace`, `parent`, `owner`,
//! `bandwidth_mibps`) answers 202 with a job; `GET /api/v1/volumes/pull`
//! lists the jobs, `GET /api/v1/volumes/pull/{job}` reads one.
//!
//! **What moves** (the master's "thin all the way through"): the peer's
//! `/legs` names the extents the volume maps; the peer serves it read-only to
//! this node's host NQN (#328's attach) and the engine's initiator reads only
//! those extents; the namespace is let go of after, however the copy ends.
//! When the volume's parent is here already (`parent`, a volume here; else
//! the peer parent's name and namespace, sealed here) the copy is a
//! copy-on-write clone of it given only the extents the volume holds of its
//! own, so a clone arrives as a clone of the same golden and never as a
//! flattened copy. Holes stay holes. The job says the virtual size, what the
//! peer holds allocated and what the copy holds here, with the extents
//! copied, shared with the parent and zeroed.
//!
//! **What does not:** a volume that takes writes on the peer (409: it moves
//! by the live mirror, #295, which follows its writes). A volume of the same
//! name and namespace here that is the same (sealed, same size) answers 200,
//! the copy there already; one that differs is a 409, never overwritten.
//!
//! Jobs are kept in `<data_dir>/pull-jobs.json`. A copy a restart cut short
//! has its half-made volume deleted and reads `failed`.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::mgmt::api::ApiError;
use crate::mgmt::replication::{self, Hooks, PeerVol, Pace};
use crate::mgmt::AppState;
use crate::volume::VolumeId;

const FILE: &str = "pull-jobs.json";

#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    /// The peer engine's management API (`http://host:9090`).
    pub from: String,
    /// The volume on the peer: its id, or a name the peer resolves.
    pub volume: String,
    /// The name here (default: the peer's).
    #[serde(default)]
    pub name: Option<String>,
    /// The namespace here (default: the peer's).
    #[serde(default)]
    pub namespace: Option<String>,
    /// The copy of its parent here, when not found by the peer parent's name.
    #[serde(default)]
    pub parent: Option<String>,
    /// Who owns the copy (default: the peer volume's owner).
    #[serde(default)]
    pub owner: Option<crate::volume::metadata::Owner>,
    /// MiB/s, 0 (the default) unlimited.
    #[serde(default)]
    pub bandwidth_mibps: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub id: Uuid,
    /// `copying`, `done` or `failed`.
    pub state: String,
    pub from: String,
    pub peer_volume: String,
    pub name: String,
    pub namespace: String,
    /// The copy here, once it is made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<Uuid>,
    /// Its parent's copy here, when it arrived as a clone of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<Uuid>,
    pub virtual_bytes: u64,
    /// What the peer holds allocated for it.
    pub peer_allocated_bytes: u64,
    /// The bytes to read over NVMe/TCP, and how many have been.
    pub bytes_total: u64,
    pub bytes_done: u64,
    pub extents_copied: u64,
    pub extents_shared: u64,
    pub extents_zeroed: u64,
    /// What the copy holds allocated here, once done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocated_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ------------------------------------------------------------------ store

/// Every engine's jobs by data dir (tests run several engines in a process).
static STORE: Mutex<Option<HashMap<String, BTreeMap<Uuid, Job>>>> = Mutex::new(None);
static FILE_LOCK: Mutex<()> = Mutex::new(());

fn load_file(state: &AppState) -> BTreeMap<Uuid, Job> {
    let Some(d) = state.config.management.data_dir.as_ref() else { return BTreeMap::new() };
    std::fs::read(std::path::Path::new(d).join(FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn with<R>(state: &AppState, f: impl FnOnce(&mut BTreeMap<Uuid, Job>) -> R) -> R {
    let mut g = STORE.lock().unwrap_or_else(|e| e.into_inner());
    let all = g.get_or_insert_with(HashMap::new);
    let k = state.config.management.data_dir.clone().unwrap_or_default();
    if !all.contains_key(&k) {
        all.insert(k.clone(), load_file(state));
    }
    f(all.get_mut(&k).expect("inserted"))
}

fn save(state: &AppState) {
    let Some(d) = state.config.management.data_dir.clone() else { return };
    let _g = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let body = with(state, |m| serde_json::to_vec_pretty(m).unwrap_or_default());
    let p = std::path::Path::new(&d).join(FILE);
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, &p)).is_err() {
        tracing::warn!("pull: {} not written", p.display());
    }
}

fn update(state: &AppState, id: Uuid, f: impl FnOnce(&mut Job)) {
    with(state, |m| {
        if let Some(j) = m.get_mut(&id) {
            f(j);
        }
    });
}

pub fn get(state: &AppState, id: Uuid) -> Option<Job> {
    with(state, |m| m.get(&id).cloned())
}

pub fn list(state: &AppState) -> Vec<Job> {
    with(state, |m| m.values().cloned().collect())
}

// ------------------------------------------------------------------ the peer

/// The peer's answer for one volume, as a listing item reads.
pub fn peer_volume(v: &Value) -> Option<PeerVol> {
    replication::parse_listing(&serde_json::json!({ "items": [v] })).into_iter().next()
}

fn base(s: &str) -> String {
    s.trim().trim_end_matches('/').to_string()
}

async fn ask_volume(c: &crate::http::Client, from: &str, key: &str) -> Result<Option<Value>, (u16, String)> {
    let url = format!("{from}/api/v1/volumes/{}", urlencode(key));
    let r = c.get(&url).send().await.map_err(|e| (502, format!("{url}: {e}")))?;
    match r.status().as_u16() {
        404 => Ok(None),
        s if (200..300).contains(&s) => r.json().await.map(Some).map_err(|e| (502, format!("{url}: {e}"))),
        s => Err((502, format!("{url}: HTTP {s}: {}", r.text().await.unwrap_or_default()))),
    }
}

fn urlencode(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

/// The volume here of that name in that namespace: (id, sealed, size).
async fn here_by_name(state: &AppState, namespace: &str, name: &str) -> Option<(VolumeId, bool, u64)> {
    let vm = state.volume_manager.lock().await;
    for id in vm.volume_ids() {
        if vm.namespace(&id) != namespace {
            continue;
        }
        if let Some(h) = vm.get_volume_handle(&id) {
            if h.name().await == name {
                return Some((id, h.is_sealed(), h.capacity_bytes()));
            }
        }
    }
    None
}

/// What a pull answers before it starts: the job, or the copy already here.
pub enum Started {
    Job(Job),
    Here(Uuid),
}

/// Check the request against the peer and start the copy.
pub async fn start(state: &Arc<AppState>, req: Request) -> Result<Started, (u16, String)> {
    let from = base(&req.from);
    if !(from.starts_with("http://") || from.starts_with("https://")) {
        return Err((422, format!("from {:?}: the peer engine's management API, http(s)://host:port", req.from)));
    }
    if req.volume.trim().is_empty() {
        return Err((422, "volume: the peer's volume id or name".into()));
    }
    if let Some(ns) = req.namespace.as_deref() {
        if !crate::volume::metadata::valid_namespace(ns) {
            return Err((422, format!("namespace {ns:?} is not a DNS-1123 label")));
        }
    }
    let c = replication::client(state).map_err(|e| (500, e))?;
    let Some(pv) = ask_volume(&c, &from, &req.volume).await? else {
        return Err((404, format!("{from} has no volume {}", req.volume)));
    };
    let Some(mut v) = peer_volume(&pv) else {
        return Err((502, format!("{from} answered volume {} without its id, name or size", req.volume)));
    };
    if !v.sealed {
        return Err((
            409,
            format!(
                "{} on {from} is not sealed: it takes writes there, so a copy would not be one moment of it. \
                 A writable volume moves by the live mirror (POST /api/v1/volumes/{{id}}/mirror, #295)",
                v.name
            ),
        ));
    }
    // A blank (a template's sealed volume) stays one here.
    let blanks = replication::get_json(&c, &format!("{from}/api/v1/volumes?kind=blank&namespace={}", urlencode(&v.namespace))).await;
    if let Ok(l) = blanks {
        if l["items"].as_array().is_some_and(|a| a.iter().any(|i| i["id"].as_str() == Some(v.id.as_str()))) {
            v.kind = "blank".into();
        }
    }
    if let Some(n) = req.name.as_deref() {
        if n.trim().is_empty() {
            return Err((422, "name: empty".into()));
        }
        v.name = n.to_string();
    }
    if let Some(ns) = req.namespace.clone() {
        v.namespace = ns;
    }
    if req.owner.is_some() {
        v.owner = req.owner.clone();
    }
    match here_by_name(state, &v.namespace, &v.name).await {
        Some((id, true, size)) if size == v.size => return Ok(Started::Here(id.0)),
        Some((id, _, _)) => {
            return Err((409, format!("{}/{} is here already and is a different volume ({}): not overwritten", v.namespace, v.name, id.0)))
        }
        None => {}
    }
    // Its parent here: as asked, else the peer parent's name, sealed here.
    let mut parent_here: Option<(VolumeId, String)> = None;
    if let Some(pp) = v.parent.clone() {
        match req.parent.as_deref() {
            Some(key) => {
                let id = crate::mgmt::api::volumes::resolve_volume(state, key)
                    .await
                    .map_err(|_| (404, format!("parent {key}: no such volume here")))?;
                if !state.volume_manager.lock().await.is_sealed(&id) {
                    return Err((409, format!("parent {key} is not sealed here: a clone's parent is a golden")));
                }
                parent_here = Some((id, pp));
            }
            None => {
                if let Ok(Some(p)) = ask_volume(&c, &from, &pp).await {
                    if let Some(p) = peer_volume(&p) {
                        if let Some((id, true, _)) = here_by_name(state, &p.namespace, &p.name).await {
                            parent_here = Some((id, pp));
                        }
                    }
                }
            }
        }
    } else if let Some(key) = req.parent.as_deref() {
        return Err((422, format!("parent {key}: {} has no parent on {from}", v.name)));
    }
    let job = Job {
        id: Uuid::new_v4(),
        state: "copying".into(),
        from: from.clone(),
        peer_volume: v.id.clone(),
        name: v.name.clone(),
        namespace: v.namespace.clone(),
        parent: parent_here.as_ref().map(|p| p.0 .0),
        virtual_bytes: v.size,
        peer_allocated_bytes: v.allocated,
        created_at: now(),
        ..Default::default()
    };
    with(state, |m| m.insert(job.id, job.clone()));
    save(state);
    tracing::info!(
        "pull {}: {}/{} ({}) from {from}, {} allocated of {}{} (#656)",
        job.id,
        v.namespace,
        v.name,
        v.id,
        crate::mgmt::config::human_size(v.allocated),
        crate::mgmt::config::human_size(v.size),
        match &parent_here {
            Some((p, _)) => format!(", as a clone of {} here", p.0),
            None => String::new(),
        }
    );
    let state = state.clone();
    let id = job.id;
    tokio::spawn(async move {
        run(&state, id, &c, &from, v, parent_here, req.bandwidth_mibps).await;
    });
    Ok(Started::Job(job))
}

async fn run(
    state: &Arc<AppState>,
    id: Uuid,
    c: &crate::http::Client,
    from: &str,
    v: PeerVol,
    parent_here: Option<(VolumeId, String)>,
    bandwidth: u64,
) {
    let parent = match parent_here {
        Some((local, peer)) => match replication::legs(c, from, &peer).await {
            Ok(e) => Some((local, e)),
            Err(e) => {
                finish(state, id, Err(format!("its parent's extents on the peer: {e}")));
                return;
            }
        },
        None => None,
    };
    let mut pace = Pace::new(bandwidth);
    let planned = |bytes: u64, copied: u64, shared: u64, zeroed: u64| {
        update(state, id, |j| {
            j.bytes_total = bytes;
            j.extents_copied = copied;
            j.extents_shared = shared;
            j.extents_zeroed = zeroed;
        });
        save(state);
    };
    let made = |vid: VolumeId| {
        update(state, id, |j| j.volume = Some(vid.0));
        save(state);
    };
    let moved = |n: u64| update(state, id, |j| j.bytes_done += n);
    let hooks = Hooks { planned: &planned, made: &made, moved: &moved };
    let r = replication::copy_one(state, c, from, &v, parent, &mut pace, &hooks).await;
    match r {
        Ok(done) => {
            let allocated = match state.volume_manager.lock().await.get_volume_handle(&done.id) {
                Some(h) => h.allocated().await,
                None => 0,
            };
            update(state, id, |j| {
                j.volume = Some(done.id.0);
                j.allocated_bytes = Some(allocated);
            });
            finish(state, id, Ok(()));
        }
        Err(e) => {
            update(state, id, |j| j.volume = None);
            finish(state, id, Err(e));
        }
    }
}

fn finish(state: &AppState, id: Uuid, r: Result<(), String>) {
    update(state, id, |j| {
        j.finished_at = Some(now());
        match &r {
            Ok(()) => j.state = "done".into(),
            Err(e) => {
                j.state = "failed".into();
                j.message = Some(e.clone());
            }
        }
    });
    save(state);
    if let Some(j) = get(state, id) {
        match r {
            Ok(()) => tracing::info!(
                "pull {id}: {}/{} here as {}: {} read over NVMe/TCP, {} allocated here of {} virtual ({} on the peer); \
                 extents copied {}, shared with its parent {}, zeroed {} (#656)",
                j.namespace,
                j.name,
                j.volume.map(|v| v.to_string()).unwrap_or_default(),
                crate::mgmt::config::human_size(j.bytes_done),
                crate::mgmt::config::human_size(j.allocated_bytes.unwrap_or(0)),
                crate::mgmt::config::human_size(j.virtual_bytes),
                crate::mgmt::config::human_size(j.peer_allocated_bytes),
                j.extents_copied,
                j.extents_shared,
                j.extents_zeroed,
            ),
            Err(e) => tracing::warn!("pull {id}: {}/{} from {}: {e} (#656)", j.namespace, j.name, j.from),
        }
    }
}

/// At the engine's start: a copy a restart cut short is not left half made.
pub async fn resume(state: &AppState) {
    let cut: Vec<Job> = with(state, |m| m.values().filter(|j| j.state == "copying").cloned().collect());
    for j in cut {
        let mut msg = "the engine restarted during the copy".to_string();
        if let Some(v) = j.volume {
            let mut vm = state.volume_manager.lock().await;
            let vid = VolumeId(v);
            if vm.get_volume(&vid).is_some() && !vm.is_sealed(&vid) {
                match vm.delete_volume(vid).await {
                    Ok(_) => msg.push_str("; its half-made copy was deleted"),
                    Err(e) => msg.push_str(&format!("; its half-made copy {v} could not be deleted: {e}")),
                }
            }
        }
        tracing::warn!("pull {}: {}/{}: {msg} (#656)", j.id, j.namespace, j.name);
        update(state, j.id, |x| {
            x.state = "failed".into();
            x.volume = None;
            x.message = Some(msg);
            x.finished_at = Some(now());
        });
    }
    save(state);
}

// ------------------------------------------------------------------ API

/// `POST /api/v1/volumes/pull`.
pub async fn post(State(state): State<Arc<AppState>>, Json(req): Json<Request>) -> Response {
    match start(&state, req).await {
        Ok(Started::Job(j)) => (axum::http::StatusCode::ACCEPTED, Json(j)).into_response(),
        Ok(Started::Here(id)) => {
            Json(serde_json::json!({ "state": "done", "volume": id, "message": "the same volume is here already" })).into_response()
        }
        Err((code, m)) => match code {
            404 => ApiError::not_found(m),
            409 => ApiError::conflict(m),
            422 => ApiError::unprocessable(m),
            500 => ApiError::internal(m),
            _ => (axum::http::StatusCode::BAD_GATEWAY, Json(serde_json::json!({ "error": m }))).into_response(),
        },
    }
}

/// `GET /api/v1/volumes/pull`.
pub async fn get_all(State(state): State<Arc<AppState>>) -> Response {
    let items = list(&state);
    let count = items.len();
    Json(serde_json::json!({ "items": items, "count": count })).into_response()
}

/// `GET /api/v1/volumes/pull/{job}`.
pub async fn get_one(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let Ok(u) = id.parse::<Uuid>() else { return ApiError::bad_request(format!("invalid job id: {id}")) };
    match get(&state, u) {
        Some(j) => Json(j).into_response(),
        None => ApiError::not_found(format!("pull job {u} not found")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// One volume as the peer's `GET /api/v1/volumes/{id}` answers it.
    #[test]
    fn a_peer_volume_reads_as_a_listing_item() {
        let v = peer_volume(&json!({"id": "7c0ca6cb-49ae-4678-9f5b-984654e60dfb", "name": "golden-x", "namespace": "stormcos",
            "virtual_size_bytes": 16777216, "allocated_bytes": 5242880, "sealed": true, "parent": null, "lba": 4096}))
        .unwrap();
        assert_eq!((v.name.as_str(), v.size, v.allocated, v.sealed, v.parent), ("golden-x", 16777216, 5242880, true, None));
        assert!(peer_volume(&json!({"name": "no id"})).is_none());
    }

    #[test]
    fn names_are_encoded() {
        assert_eq!(urlencode("media/buildbox2:0.3.2"), "media%2Fbuildbox2%3A0.3.2");
        assert_eq!(base(" http://forge:9090/ "), "http://forge:9090");
    }
}
