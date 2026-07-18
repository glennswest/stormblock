//! /v1 — the management surface consumed by stormblock-csi and the wander
//! operator (issues #3, #8, #9, #10; API layer of #5/#6/#7).
//!
//! The normative contract is stormblock-csi's docs/stormblock-api.md; the
//! `MockEngine` there is the executable spec these handlers must match:
//! name-based idempotency, epoch fencing (fence-before-promote CAS), a single
//! bounded dual-attach window, mandatory replica anti-affinity, and the
//! `{code, message, current_epoch?}` error envelope with 404/409/412/507.
//!
//! Volumes whose master lands on this node are backed by real thin volumes
//! through the `VolumeManager` (COW clones via GEM for `source`); replica
//! placement on remote nodes is tracked as control-plane state — the data
//! path for cross-node replication is the engine work tracked in #5/#6/#7.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    extract::{Path, Query, Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::drive::BlockDevice;
use crate::mgmt::AppState;
use crate::volume::VolumeId as EngineVolumeId;

pub type Epoch = u64;

// ---------------------------------------------------------------------------
// Wire types (mirrors stormblock-csi crates/stormblock-client/src/types.rs)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplicaRole {
    Master,
    Slave,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum SyncState {
    InSync,
    Resyncing { progress_pct: f32, lag_bytes: u64 },
    Detached,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Replica {
    pub node: String,
    pub role: ReplicaRole,
    pub sync: SyncState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeHealth {
    Healthy,
    Degraded,
    Faulted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BandwidthClass {
    Low,
    #[default]
    Normal,
    High,
    Unthrottled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaTier {
    pub slaves: u8,
}

impl Default for ReplicaTier {
    fn default() -> Self {
        Self { slaves: 1 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Volume {
    pub id: String,
    pub name: String,
    pub size_bytes: u64,
    pub epoch: Epoch,
    pub replicas: Vec<Replica>,
    pub health: VolumeHealth,
    #[serde(default)]
    pub encrypted: bool,
    #[serde(default)]
    pub qos_class: Option<String>,
    #[serde(default)]
    pub bandwidth_class: BandwidthClass,
}

impl Volume {
    pub fn master_node(&self) -> Option<&str> {
        self.replicas
            .iter()
            .find(|r| r.role == ReplicaRole::Master)
            .map(|r| r.node.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "id")]
pub enum VolumeSource {
    Snapshot(String),
    Volume(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateVolumeRequest {
    pub name: String,
    pub size_bytes: u64,
    #[serde(default)]
    pub master_node: Option<String>,
    #[serde(default)]
    pub excluded_nodes: Vec<String>,
    #[serde(default)]
    pub replica_tier: ReplicaTier,
    #[serde(default)]
    pub bandwidth_class: BandwidthClass,
    #[serde(default)]
    pub qos_class: Option<String>,
    #[serde(default)]
    pub encrypted: bool,
    #[serde(default)]
    pub source: Option<VolumeSource>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub name: String,
    pub source_volume_id: String,
    pub size_bytes: u64,
    pub ready: bool,
    pub created_at_ms: i64,
    #[serde(default)]
    pub group_snapshot_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupSnapshot {
    pub id: String,
    pub name: String,
    pub snapshots: Vec<Snapshot>,
    pub ready: bool,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "transport")]
pub enum AttachInfo {
    NvmeTcp {
        nqn: String,
        addresses: Vec<NvmeAddress>,
    },
    Ublk {
        device_hint: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NvmeAddress {
    pub traddr: String,
    pub trsvcid: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachMode {
    ReadWrite,
    MigrationTarget,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DualAttachWindow {
    pub volume_id: String,
    pub epoch: Epoch,
    pub target_node: String,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DualAttachOutcome {
    Commit,
    Abort,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeCapacity {
    pub node: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    #[serde(default)]
    pub topology: BTreeMap<String, String>,
}

// ---------------------------------------------------------------------------
// Error envelope: {code, message, current_epoch?} + 404/409/412/507 mapping
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum V1Error {
    NotFound(String),
    Conflict(String),
    AlreadyExists(String),
    StaleEpoch(Epoch),
    OutOfSpace(String),
    BadRequest(String),
    Unauthorized,
    Internal(String),
}

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_epoch: Option<Epoch>,
}

impl IntoResponse for V1Error {
    fn into_response(self) -> Response {
        let (status, code, message, current_epoch) = match self {
            V1Error::NotFound(m) => (StatusCode::NOT_FOUND, "not_found", m, None),
            V1Error::Conflict(m) => (StatusCode::CONFLICT, "conflict", m, None),
            V1Error::AlreadyExists(m) => (StatusCode::CONFLICT, "already_exists", m, None),
            V1Error::StaleEpoch(current) => (
                StatusCode::PRECONDITION_FAILED,
                "stale_epoch",
                format!("stale epoch; current is {current}"),
                Some(current),
            ),
            V1Error::OutOfSpace(m) => (StatusCode::INSUFFICIENT_STORAGE, "out_of_space", m, None),
            V1Error::BadRequest(m) => (StatusCode::BAD_REQUEST, "bad_request", m, None),
            V1Error::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing or invalid bearer token".to_string(),
                None,
            ),
            V1Error::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, "internal", m, None),
        };
        (status, Json(ErrorBody { code, message, current_epoch })).into_response()
    }
}

type V1Result<T> = Result<Json<T>, V1Error>;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// One /v1 volume: the wire object plus its local engine binding (the thin
/// volume backing it on this node, when this node holds the master).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeRec {
    pub vol: Volume,
    pub local_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotRec {
    pub snap: Snapshot,
    pub local_id: Option<Uuid>,
}

/// Control-plane state behind /v1. Persisted as JSON under the management
/// data dir so volumes/snapshots survive restart (their data lives in slabs
/// and is rebuilt into GEM independently).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct V1State {
    pub volumes: HashMap<String, VolumeRec>,
    pub snapshots: HashMap<String, SnapshotRec>,
    pub group_snapshots: HashMap<String, GroupSnapshot>,
    /// volume id -> open migration window
    pub dual_attach: HashMap<String, DualAttachWindow>,
    /// volume id -> nodes it is exported to
    pub attachments: HashMap<String, Vec<String>>,
    /// Statically registered peer nodes (test hook / static cluster config).
    /// The local node is always reported live from the slab registry on top
    /// of these.
    pub nodes: BTreeMap<String, NodeCapacity>,
    #[serde(skip)]
    pub local_node: String,
    #[serde(skip)]
    pub local_topology: BTreeMap<String, String>,
    #[serde(skip)]
    persist_path: Option<PathBuf>,
}

impl V1State {
    /// Build the state from config, loading any persisted copy from
    /// `<data_dir>/v1_state.json`.
    pub fn from_config(config: &crate::mgmt::config::StormBlockConfig) -> Self {
        let local_node = config
            .management
            .node_name
            .clone()
            .or_else(|| std::env::var("STORMBLOCK_NODE").ok())
            .or_else(|| std::env::var("HOSTNAME").ok())
            .unwrap_or_else(|| "localhost".to_string());
        let persist_path = config
            .management
            .data_dir
            .as_ref()
            .map(|d| PathBuf::from(d).join("v1_state.json"));

        let mut state = persist_path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice::<V1State>(&bytes).ok())
            .unwrap_or_default();
        state.local_node = local_node;
        state.local_topology = config.management.topology.clone();
        state.persist_path = persist_path;
        state
    }

    /// Register a peer node the engine can place replicas on (test hook /
    /// static multi-node config until cluster membership is wired in).
    pub fn add_node(&mut self, node: &str, free_bytes: u64, topology: BTreeMap<String, String>) {
        self.nodes.insert(
            node.to_string(),
            NodeCapacity {
                node: node.to_string(),
                total_bytes: free_bytes,
                free_bytes,
                topology,
            },
        );
    }

    fn save(&self) {
        let Some(path) = &self.persist_path else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_vec_pretty(self) {
            Ok(bytes) => {
                let tmp = path.with_extension("json.tmp");
                if std::fs::write(&tmp, bytes)
                    .and_then(|_| std::fs::rename(&tmp, path))
                    .is_err()
                {
                    tracing::warn!("failed to persist v1 state to {}", path.display());
                }
            }
            Err(e) => tracing::warn!("failed to serialize v1 state: {e}"),
        }
    }

    fn volume_by_name(&self, name: &str) -> Option<&VolumeRec> {
        self.volumes.values().find(|r| r.vol.name == name)
    }

    /// Drop expired dual-attach windows (engine-enforced auto-abort).
    fn expire_windows(&mut self, now_ms: i64) {
        let expired: Vec<String> = self
            .dual_attach
            .iter()
            .filter(|(_, w)| w.expires_at_ms <= now_ms)
            .map(|(vid, _)| vid.clone())
            .collect();
        for vid in expired {
            if let Some(w) = self.dual_attach.remove(&vid) {
                tracing::info!("dual-attach window on {vid} expired; auto-aborting");
                if let Some(nodes) = self.attachments.get_mut(&vid) {
                    nodes.retain(|n| n != &w.target_node);
                }
            }
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn gen_id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

/// Live capacity of this node: sum over registered slabs.
async fn local_capacity(state: &AppState) -> (u64, u64) {
    let reg = state.slab_registry.lock().await;
    let mut total = 0u64;
    let mut free = 0u64;
    for (_, slab) in reg.iter() {
        total += slab.total_slots() * slab.slot_size();
        free += slab.free_slots() * slab.slot_size();
    }
    (total, free)
}

/// All nodes visible for placement/capacity: static peers plus this node,
/// reported live. A live local report wins unless the node has no slabs and
/// a static entry exists (test setups).
async fn nodes_view(state: &AppState, v1: &V1State) -> BTreeMap<String, NodeCapacity> {
    let mut nodes = v1.nodes.clone();
    let (total, free) = local_capacity(state).await;
    let insert_live = total > 0 || !nodes.contains_key(&v1.local_node);
    if insert_live {
        nodes.insert(
            v1.local_node.clone(),
            NodeCapacity {
                node: v1.local_node.clone(),
                total_bytes: total,
                free_bytes: free,
                topology: v1.local_topology.clone(),
            },
        );
    }
    nodes
}

/// Place a master + N slaves on distinct nodes with room for `size` bytes.
fn pick_nodes(
    nodes: &BTreeMap<String, NodeCapacity>,
    size: u64,
    master_hint: Option<&str>,
    excluded: &[String],
    slaves: u8,
) -> Result<(String, Vec<String>), V1Error> {
    let candidates: Vec<&NodeCapacity> = nodes
        .values()
        .filter(|n| n.free_bytes >= size && !excluded.contains(&n.node))
        .collect();
    let master = match master_hint {
        Some(h) => candidates
            .iter()
            .find(|n| n.node == h)
            .ok_or_else(|| V1Error::OutOfSpace(format!("requested master node {h} unavailable")))?
            .node
            .clone(),
        None => candidates
            .first()
            .ok_or_else(|| V1Error::OutOfSpace("no candidate nodes".into()))?
            .node
            .clone(),
    };
    // Anti-affinity is mandatory: every slave lands on a distinct node.
    let mut slave_nodes = Vec::with_capacity(slaves as usize);
    for n in candidates.iter().filter(|n| n.node != master) {
        if slave_nodes.len() == slaves as usize {
            break;
        }
        slave_nodes.push(n.node.clone());
    }
    if slave_nodes.len() < slaves as usize {
        return Err(V1Error::OutOfSpace(format!(
            "need {} distinct node(s) for slave replicas, found {}",
            slaves,
            slave_nodes.len()
        )));
    }
    Ok((master, slave_nodes))
}

/// Charge/refund statically registered nodes (the local node is live).
fn account_static_nodes(v1: &mut V1State, replicas: &[Replica], size: u64, charge: bool) {
    for r in replicas {
        if let Some(n) = v1.nodes.get_mut(&r.node) {
            if charge {
                n.free_bytes = n.free_bytes.saturating_sub(size);
            } else {
                n.free_bytes = (n.free_bytes + size).min(n.total_bytes);
            }
        }
    }
}

fn attach_info_for(state: &AppState, volume_id: &str) -> AttachInfo {
    let listen = {
        #[cfg(feature = "nvmeof")]
        {
            state
                .config
                .nvmeof
                .as_ref()
                .map(|n| n.listen_addr.clone())
                .unwrap_or_else(|| "0.0.0.0:4420".to_string())
        }
        #[cfg(not(feature = "nvmeof"))]
        {
            let _ = state;
            "0.0.0.0:4420".to_string()
        }
    };
    let (host, port) = match listen.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(4420)),
        None => (listen, 4420),
    };
    let traddr = if host.is_empty() || host == "0.0.0.0" || host == "[::]" || host == "::" {
        // Unspecified listen address: fall back to the management listen host
        // if it is concrete, else loopback.
        let mgmt_host = state
            .config
            .management
            .listen_addr
            .rsplit_once(':')
            .map(|(h, _)| h.to_string())
            .unwrap_or_default();
        if mgmt_host.is_empty() || mgmt_host == "0.0.0.0" || mgmt_host == "[::]" {
            "127.0.0.1".to_string()
        } else {
            mgmt_host
        }
    } else {
        host
    };
    AttachInfo::NvmeTcp {
        nqn: format!("nqn.2026-01.io.stormblock:{volume_id}"),
        addresses: vec![NvmeAddress { traddr, trsvcid: port }],
    }
}

// ---------------------------------------------------------------------------
// Volume handlers
// ---------------------------------------------------------------------------

async fn create_volume(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateVolumeRequest>,
) -> V1Result<Volume> {
    let mut v1 = state.v1.lock().await;
    v1.expire_windows(now_ms());

    // Name-based idempotency: same name + same size → the existing volume.
    if let Some(existing) = v1.volume_by_name(&req.name) {
        if existing.vol.size_bytes == req.size_bytes {
            return Ok(Json(existing.vol.clone()));
        }
        return Err(V1Error::AlreadyExists(format!(
            "volume {} exists with size {}",
            req.name, existing.vol.size_bytes
        )));
    }

    // Source must exist before any allocation happens.
    let source_local: Option<Uuid> = match &req.source {
        Some(VolumeSource::Snapshot(id)) => Some(
            v1.snapshots
                .get(id)
                .ok_or_else(|| V1Error::NotFound(format!("snapshot {id}")))?
                .local_id
                .unwrap_or_default(),
        )
        .filter(|u| !u.is_nil()),
        Some(VolumeSource::Volume(id)) => Some(
            v1.volumes
                .get(id)
                .ok_or_else(|| V1Error::NotFound(format!("volume {id}")))?
                .local_id
                .unwrap_or_default(),
        )
        .filter(|u| !u.is_nil()),
        None => None,
    };

    let nodes = nodes_view(&state, &v1).await;
    let (master, slaves) = pick_nodes(
        &nodes,
        req.size_bytes,
        req.master_node.as_deref(),
        &req.excluded_nodes,
        req.replica_tier.slaves,
    )?;

    // Master on this node: back it with a real thin volume (COW clone of the
    // source when one is bound locally).
    let local_id = if master == v1.local_node {
        let mut vm = state.volume_manager.lock().await;
        let created = match source_local {
            Some(src) => vm.create_snapshot(EngineVolumeId(src), &req.name).await,
            None => vm.create_volume_any(&req.name, req.size_bytes).await,
        };
        match created {
            Ok(id) => {
                // Clones inherit the source size; grow to the request if larger.
                if source_local.is_some() {
                    if let Some(h) = vm.get_volume_handle(&id) {
                        if req.size_bytes > h.capacity_bytes() {
                            let _ = vm.resize_volume(id, req.size_bytes).await;
                        }
                    }
                }
                Some(id.0)
            }
            Err(e) => {
                return Err(V1Error::Internal(format!("backing volume create failed: {e}")))
            }
        }
    } else {
        None
    };

    let mut replicas = vec![Replica {
        node: master,
        role: ReplicaRole::Master,
        sync: SyncState::InSync,
    }];
    for s in slaves {
        replicas.push(Replica {
            node: s,
            role: ReplicaRole::Slave,
            sync: SyncState::InSync,
        });
    }

    let vol = Volume {
        id: gen_id("vol"),
        name: req.name,
        size_bytes: req.size_bytes,
        epoch: 1,
        replicas,
        health: VolumeHealth::Healthy,
        encrypted: req.encrypted,
        qos_class: req.qos_class,
        bandwidth_class: req.bandwidth_class,
    };
    account_static_nodes(&mut v1, &vol.replicas, vol.size_bytes, true);
    v1.volumes.insert(vol.id.clone(), VolumeRec { vol: vol.clone(), local_id });
    v1.save();
    Ok(Json(vol))
}

#[derive(Deserialize)]
struct NameFilter {
    name: Option<String>,
}

async fn list_volumes(
    State(state): State<Arc<AppState>>,
    Query(q): Query<NameFilter>,
) -> V1Result<Vec<Volume>> {
    let v1 = state.v1.lock().await;
    Ok(Json(
        v1.volumes
            .values()
            .filter(|r| q.name.as_deref().map(|n| r.vol.name == n).unwrap_or(true))
            .map(|r| r.vol.clone())
            .collect(),
    ))
}

async fn get_volume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> V1Result<Volume> {
    let v1 = state.v1.lock().await;
    v1.volumes
        .get(&id)
        .map(|r| Json(r.vol.clone()))
        .ok_or_else(|| V1Error::NotFound(format!("volume {id}")))
}

async fn delete_volume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> V1Result<serde_json::Value> {
    let mut v1 = state.v1.lock().await;
    if let Some(rec) = v1.volumes.remove(&id) {
        account_static_nodes(&mut v1, &rec.vol.replicas, rec.vol.size_bytes, false);
        v1.attachments.remove(&id);
        v1.dual_attach.remove(&id);
        if let Some(local) = rec.local_id {
            let mut vm = state.volume_manager.lock().await;
            if let Err(e) = vm.delete_volume(EngineVolumeId(local)).await {
                tracing::warn!("backing volume {local} delete: {e}");
            }
        }
        v1.save();
    }
    // Idempotent: deleting an absent volume succeeds.
    Ok(Json(serde_json::json!({})))
}

#[derive(Deserialize)]
struct ExpandRequest {
    size_bytes: u64,
}

async fn expand_volume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<ExpandRequest>,
) -> V1Result<Volume> {
    let mut v1 = state.v1.lock().await;
    let rec = v1
        .volumes
        .get_mut(&id)
        .ok_or_else(|| V1Error::NotFound(format!("volume {id}")))?;
    // Grow only; shrink requests return the volume unchanged.
    if req.size_bytes >= rec.vol.size_bytes {
        rec.vol.size_bytes = req.size_bytes;
        let local = rec.local_id;
        let vol = rec.vol.clone();
        if let Some(local) = local {
            let mut vm = state.volume_manager.lock().await;
            if let Err(e) = vm.resize_volume(EngineVolumeId(local), req.size_bytes).await {
                tracing::warn!("backing volume {local} resize: {e}");
            }
        }
        v1.save();
        return Ok(Json(vol));
    }
    Ok(Json(rec.vol.clone()))
}

#[derive(Deserialize)]
struct AttachRequest {
    node: String,
    mode: AttachMode,
}

async fn attach_volume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<AttachRequest>,
) -> V1Result<AttachInfo> {
    let mut v1 = state.v1.lock().await;
    v1.expire_windows(now_ms());
    let rec = v1
        .volumes
        .get(&id)
        .ok_or_else(|| V1Error::NotFound(format!("volume {id}")))?;
    match req.mode {
        AttachMode::ReadWrite => {
            // The engine-side gate that makes wrong-node pods harmless.
            if rec.vol.master_node() != Some(req.node.as_str()) {
                return Err(V1Error::Conflict(format!(
                    "read-write attach only on master node {:?}, requested {}",
                    rec.vol.master_node(),
                    req.node
                )));
            }
        }
        AttachMode::MigrationTarget => {
            let ok = v1
                .dual_attach
                .get(&id)
                .map(|w| w.target_node == req.node)
                .unwrap_or(false);
            if !ok {
                return Err(V1Error::Conflict(
                    "migration-target attach requires an open dual-attach window".into(),
                ));
            }
        }
    }
    let entry = v1.attachments.entry(id.clone()).or_default();
    if !entry.contains(&req.node) {
        entry.push(req.node.clone());
    }
    v1.save();
    Ok(Json(attach_info_for(&state, &id)))
}

#[derive(Deserialize)]
struct DetachRequest {
    node: String,
}

async fn detach_volume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<DetachRequest>,
) -> V1Result<serde_json::Value> {
    let mut v1 = state.v1.lock().await;
    if let Some(nodes) = v1.attachments.get_mut(&id) {
        nodes.retain(|n| n != &req.node);
        v1.save();
    }
    // Idempotent: detach replays are no-ops.
    Ok(Json(serde_json::json!({})))
}

// ---------------------------------------------------------------------------
// Placement + prestage (#5 API surface)
// ---------------------------------------------------------------------------

fn apply_placement(
    v1: &mut V1State,
    id: &str,
    master_node: &str,
    slave_node: &str,
) -> Result<Volume, V1Error> {
    if master_node == slave_node {
        return Err(V1Error::Conflict(
            "anti-affinity violation: master and slave on the same node".into(),
        ));
    }
    let rec = v1
        .volumes
        .get_mut(id)
        .ok_or_else(|| V1Error::NotFound(format!("volume {id}")))?;
    if rec.vol.master_node() != Some(master_node) {
        return Err(V1Error::Conflict(format!(
            "placement cannot move the master (current {:?}); use promote",
            rec.vol.master_node()
        )));
    }
    let size = rec.vol.size_bytes;
    rec.vol.replicas.retain(|r| r.role == ReplicaRole::Master);
    rec.vol.replicas.push(Replica {
        node: slave_node.to_string(),
        role: ReplicaRole::Slave,
        // The exposure window: resync progress/lag surfaces here for the
        // wander operator until the new slave catches up.
        sync: SyncState::Resyncing { progress_pct: 0.0, lag_bytes: size },
    });
    rec.vol.health = VolumeHealth::Degraded;
    let vol = rec.vol.clone();
    v1.save();
    Ok(vol)
}

#[derive(Deserialize)]
struct PlacementRequest {
    master_node: String,
    slave_node: String,
}

async fn set_placement(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<PlacementRequest>,
) -> V1Result<Volume> {
    let mut v1 = state.v1.lock().await;
    apply_placement(&mut v1, &id, &req.master_node, &req.slave_node).map(Json)
}

#[derive(Deserialize)]
struct PrestageRequest {
    node: String,
}

async fn prestage_slave(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<PrestageRequest>,
) -> V1Result<Volume> {
    let mut v1 = state.v1.lock().await;
    let master = v1
        .volumes
        .get(&id)
        .ok_or_else(|| V1Error::NotFound(format!("volume {id}")))?
        .vol
        .master_node()
        .map(str::to_string)
        .ok_or_else(|| V1Error::Conflict("volume has no master".into()))?;
    apply_placement(&mut v1, &id, &master, &req.node).map(Json)
}

// ---------------------------------------------------------------------------
// Fence + promote (#6 API surface)
// ---------------------------------------------------------------------------

fn apply_fence(v1: &mut V1State, id: &str, expected_epoch: Epoch) -> Result<Epoch, V1Error> {
    let rec = v1
        .volumes
        .get_mut(id)
        .ok_or_else(|| V1Error::NotFound(format!("volume {id}")))?;
    // CAS on the epoch: two racing tiebreakers cannot both fence.
    if rec.vol.epoch != expected_epoch {
        return Err(V1Error::StaleEpoch(rec.vol.epoch));
    }
    rec.vol.epoch += 1;
    let epoch = rec.vol.epoch;
    v1.save();
    Ok(epoch)
}

fn apply_promote(
    v1: &mut V1State,
    id: &str,
    target_node: &str,
    fenced_epoch: Epoch,
) -> Result<Volume, V1Error> {
    if v1.dual_attach.contains_key(id) {
        return Err(V1Error::Conflict(
            "cannot promote while a dual-attach window is open; close it first".into(),
        ));
    }
    let rec = v1
        .volumes
        .get_mut(id)
        .ok_or_else(|| V1Error::NotFound(format!("volume {id}")))?;
    if rec.vol.epoch != fenced_epoch {
        return Err(V1Error::StaleEpoch(rec.vol.epoch));
    }
    let is_slave = rec
        .vol
        .replicas
        .iter()
        .any(|r| r.node == target_node && r.role == ReplicaRole::Slave);
    if !is_slave {
        return Err(V1Error::Conflict(format!(
            "{target_node} holds no slave replica of {id}"
        )));
    }
    // Old master is already fenced (epoch bumped); demote it out of the pair
    // — restaging a fresh slave is the operator's next step.
    rec.vol.replicas.retain(|r| r.node == target_node);
    rec.vol.replicas[0].role = ReplicaRole::Master;
    rec.vol.replicas[0].sync = SyncState::InSync;
    rec.vol.health = VolumeHealth::Degraded; // single replica until restaged
    let vol = rec.vol.clone();
    v1.attachments.remove(id);
    v1.save();
    Ok(vol)
}

#[derive(Deserialize)]
struct FenceRequest {
    expected_epoch: Epoch,
}

async fn fence_volume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<FenceRequest>,
) -> V1Result<serde_json::Value> {
    let mut v1 = state.v1.lock().await;
    v1.expire_windows(now_ms());
    let epoch = apply_fence(&mut v1, &id, req.expected_epoch)?;
    Ok(Json(serde_json::json!({ "epoch": epoch })))
}

#[derive(Deserialize)]
struct PromoteRequest {
    target_node: String,
    fenced_epoch: Epoch,
}

async fn promote_volume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<PromoteRequest>,
) -> V1Result<Volume> {
    let mut v1 = state.v1.lock().await;
    v1.expire_windows(now_ms());
    apply_promote(&mut v1, &id, &req.target_node, req.fenced_epoch).map(Json)
}

// ---------------------------------------------------------------------------
// Bounded dual-attach (#7 API surface)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct DualAttachRequest {
    target_node: String,
    ttl_secs: u32,
}

async fn open_dual_attach(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<DualAttachRequest>,
) -> V1Result<DualAttachWindow> {
    let mut v1 = state.v1.lock().await;
    v1.expire_windows(now_ms());
    let rec = v1
        .volumes
        .get(&id)
        .ok_or_else(|| V1Error::NotFound(format!("volume {id}")))?;
    if !rec
        .vol
        .replicas
        .iter()
        .any(|r| r.node == req.target_node && r.role == ReplicaRole::Slave)
    {
        return Err(V1Error::Conflict(format!(
            "dual-attach target {} holds no slave replica",
            req.target_node
        )));
    }
    let epoch = rec.vol.epoch;
    if let Some(w) = v1.dual_attach.get(&id) {
        if w.target_node == req.target_node {
            return Ok(Json(w.clone())); // idempotent reopen
        }
        return Err(V1Error::Conflict("dual-attach window already open".into()));
    }
    let window = DualAttachWindow {
        volume_id: id.clone(),
        epoch,
        target_node: req.target_node,
        expires_at_ms: now_ms() + i64::from(req.ttl_secs) * 1000,
    };
    v1.dual_attach.insert(id, window.clone());
    v1.save();
    Ok(Json(window))
}

#[derive(Deserialize)]
struct CloseDualAttachRequest {
    epoch: Epoch,
    outcome: DualAttachOutcome,
}

async fn close_dual_attach(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<CloseDualAttachRequest>,
) -> V1Result<Volume> {
    let mut v1 = state.v1.lock().await;
    v1.expire_windows(now_ms());
    let w = v1
        .dual_attach
        .get(&id)
        .ok_or_else(|| V1Error::NotFound(format!("no dual-attach on {id}")))?;
    if w.epoch != req.epoch {
        return Err(V1Error::StaleEpoch(w.epoch));
    }
    let target = w.target_node.clone();
    v1.dual_attach.remove(&id);
    match req.outcome {
        DualAttachOutcome::Abort => {
            if let Some(nodes) = v1.attachments.get_mut(&id) {
                nodes.retain(|n| n != &target);
            }
            let vol = v1
                .volumes
                .get(&id)
                .ok_or_else(|| V1Error::NotFound(format!("volume {id}")))?
                .vol
                .clone();
            v1.save();
            Ok(Json(vol))
        }
        DualAttachOutcome::Commit => {
            // Cutover: fence the old master, promote the migration target.
            let fenced = apply_fence(&mut v1, &id, req.epoch)?;
            apply_promote(&mut v1, &id, &target, fenced).map(Json)
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshots (#3) + group snapshots (#8)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateSnapshotRequest {
    name: String,
    volume_id: String,
}

async fn create_snapshot(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateSnapshotRequest>,
) -> V1Result<Snapshot> {
    let mut v1 = state.v1.lock().await;
    if let Some(existing) = v1.snapshots.values().find(|s| s.snap.name == req.name) {
        if existing.snap.source_volume_id == req.volume_id {
            return Ok(Json(existing.snap.clone()));
        }
        return Err(V1Error::AlreadyExists(format!(
            "snapshot {} exists for volume {}",
            req.name, existing.snap.source_volume_id
        )));
    }
    let rec = v1
        .volumes
        .get(&req.volume_id)
        .ok_or_else(|| V1Error::NotFound(format!("volume {}", req.volume_id)))?;
    let size = rec.vol.size_bytes;
    let source_local = rec.local_id;

    // COW clone through GEM when the volume is backed on this node.
    let local_id = match source_local {
        Some(src) => {
            let mut vm = state.volume_manager.lock().await;
            match vm.create_snapshot(EngineVolumeId(src), &req.name).await {
                Ok(id) => Some(id.0),
                Err(e) => {
                    return Err(V1Error::Internal(format!("engine snapshot failed: {e}")))
                }
            }
        }
        None => None,
    };

    let snap = Snapshot {
        id: gen_id("snap"),
        name: req.name,
        source_volume_id: req.volume_id,
        size_bytes: size,
        ready: true,
        created_at_ms: now_ms(),
        group_snapshot_id: None,
    };
    v1.snapshots
        .insert(snap.id.clone(), SnapshotRec { snap: snap.clone(), local_id });
    v1.save();
    Ok(Json(snap))
}

#[derive(Deserialize)]
struct SnapshotFilter {
    name: Option<String>,
    source_volume: Option<String>,
}

async fn list_snapshots(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SnapshotFilter>,
) -> V1Result<Vec<Snapshot>> {
    let v1 = state.v1.lock().await;
    Ok(Json(
        v1.snapshots
            .values()
            .filter(|s| q.name.as_deref().map(|n| s.snap.name == n).unwrap_or(true))
            .filter(|s| {
                q.source_volume
                    .as_deref()
                    .map(|v| s.snap.source_volume_id == v)
                    .unwrap_or(true)
            })
            .map(|s| s.snap.clone())
            .collect(),
    ))
}

async fn get_snapshot(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> V1Result<Snapshot> {
    let v1 = state.v1.lock().await;
    v1.snapshots
        .get(&id)
        .map(|s| Json(s.snap.clone()))
        .ok_or_else(|| V1Error::NotFound(format!("snapshot {id}")))
}

async fn delete_snapshot(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> V1Result<serde_json::Value> {
    let mut v1 = state.v1.lock().await;
    if let Some(rec) = v1.snapshots.remove(&id) {
        if let Some(local) = rec.local_id {
            let mut vm = state.volume_manager.lock().await;
            if let Err(e) = vm.delete_volume(EngineVolumeId(local)).await {
                tracing::warn!("backing snapshot {local} delete: {e}");
            }
        }
        v1.save();
    }
    Ok(Json(serde_json::json!({})))
}

#[derive(Deserialize)]
struct CreateGroupSnapshotRequest {
    name: String,
    volume_ids: Vec<String>,
}

async fn create_group_snapshot(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateGroupSnapshotRequest>,
) -> V1Result<GroupSnapshot> {
    let mut v1 = state.v1.lock().await;
    if let Some(existing) = v1.group_snapshots.values().find(|g| g.name == req.name) {
        return Ok(Json(existing.clone())); // idempotent by name
    }
    for id in &req.volume_ids {
        if !v1.volumes.contains_key(id) {
            return Err(V1Error::NotFound(format!("volume {id}")));
        }
    }

    // Engine fence: every locally-backed member is cloned under one held
    // GEM+registry lock — a single consistency point across extent maps.
    let locally_backed: Vec<(EngineVolumeId, String)> = req
        .volume_ids
        .iter()
        .filter_map(|vid| {
            v1.volumes[vid]
                .local_id
                .map(|l| (EngineVolumeId(l), format!("{}-{vid}", req.name)))
        })
        .collect();
    let mut local_snaps: HashMap<String, Uuid> = HashMap::new();
    if !locally_backed.is_empty() {
        let mut vm = state.volume_manager.lock().await;
        match vm.create_snapshots_atomic(&locally_backed).await {
            Ok(ids) => {
                for ((_, name), snap_id) in locally_backed.iter().zip(ids) {
                    local_snaps.insert(name.clone(), snap_id.0);
                }
            }
            Err(e) => {
                return Err(V1Error::Internal(format!("group snapshot fence failed: {e}")))
            }
        }
    }

    let group_id = gen_id("gsnap");
    let created = now_ms();
    let mut snapshots = Vec::with_capacity(req.volume_ids.len());
    for vid in &req.volume_ids {
        let name = format!("{}-{vid}", req.name);
        let snap = Snapshot {
            id: gen_id("snap"),
            name: name.clone(),
            source_volume_id: vid.clone(),
            size_bytes: v1.volumes[vid].vol.size_bytes,
            ready: true,
            created_at_ms: created,
            group_snapshot_id: Some(group_id.clone()),
        };
        v1.snapshots.insert(
            snap.id.clone(),
            SnapshotRec { snap: snap.clone(), local_id: local_snaps.get(&name).copied() },
        );
        snapshots.push(snap);
    }
    let group = GroupSnapshot {
        id: group_id,
        name: req.name,
        snapshots,
        ready: true,
        created_at_ms: created,
    };
    v1.group_snapshots.insert(group.id.clone(), group.clone());
    v1.save();
    Ok(Json(group))
}

async fn list_group_snapshots(
    State(state): State<Arc<AppState>>,
    Query(q): Query<NameFilter>,
) -> V1Result<Vec<GroupSnapshot>> {
    let v1 = state.v1.lock().await;
    Ok(Json(
        v1.group_snapshots
            .values()
            .filter(|g| q.name.as_deref().map(|n| g.name == n).unwrap_or(true))
            .cloned()
            .collect(),
    ))
}

async fn get_group_snapshot(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> V1Result<GroupSnapshot> {
    let v1 = state.v1.lock().await;
    v1.group_snapshots
        .get(&id)
        .cloned()
        .map(Json)
        .ok_or_else(|| V1Error::NotFound(format!("group snapshot {id}")))
}

async fn delete_group_snapshot(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> V1Result<serde_json::Value> {
    let mut v1 = state.v1.lock().await;
    if let Some(g) = v1.group_snapshots.remove(&id) {
        let mut backing = Vec::new();
        for snap in g.snapshots {
            if let Some(rec) = v1.snapshots.remove(&snap.id) {
                if let Some(local) = rec.local_id {
                    backing.push(local);
                }
            }
        }
        if !backing.is_empty() {
            let mut vm = state.volume_manager.lock().await;
            for local in backing {
                if let Err(e) = vm.delete_volume(EngineVolumeId(local)).await {
                    tracing::warn!("backing snapshot {local} delete: {e}");
                }
            }
        }
        v1.save();
    }
    Ok(Json(serde_json::json!({})))
}

// ---------------------------------------------------------------------------
// Capacity + topology (#9)
// ---------------------------------------------------------------------------

async fn list_node_capacities(State(state): State<Arc<AppState>>) -> V1Result<Vec<NodeCapacity>> {
    let v1 = state.v1.lock().await;
    let nodes = nodes_view(&state, &v1).await;
    Ok(Json(nodes.into_values().collect()))
}

async fn get_node_capacity(
    State(state): State<Arc<AppState>>,
    Path(node): Path<String>,
) -> V1Result<NodeCapacity> {
    let v1 = state.v1.lock().await;
    let nodes = nodes_view(&state, &v1).await;
    nodes
        .get(&node)
        .cloned()
        .map(Json)
        .ok_or_else(|| V1Error::NotFound(format!("node {node}")))
}

// ---------------------------------------------------------------------------
// Router + optional bearer auth
// ---------------------------------------------------------------------------

async fn require_bearer(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    if let Some(expected) = &state.config.management.api_token {
        let ok = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|t| t == expected)
            .unwrap_or(false);
        if !ok {
            return V1Error::Unauthorized.into_response();
        }
    }
    next.run(req).await
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/volumes", post(create_volume).get(list_volumes))
        .route("/volumes/{id}", get(get_volume).delete(delete_volume))
        .route("/volumes/{id}/expand", post(expand_volume))
        .route("/volumes/{id}/attach", post(attach_volume))
        .route("/volumes/{id}/detach", post(detach_volume))
        .route("/volumes/{id}/placement", post(set_placement))
        .route("/volumes/{id}/prestage", post(prestage_slave))
        .route("/volumes/{id}/fence", post(fence_volume))
        .route("/volumes/{id}/promote", post(promote_volume))
        .route("/volumes/{id}/dual-attach", post(open_dual_attach))
        .route("/volumes/{id}/dual-attach/close", post(close_dual_attach))
        .route("/snapshots", post(create_snapshot).get(list_snapshots))
        .route("/snapshots/{id}", get(get_snapshot).delete(delete_snapshot))
        .route(
            "/group-snapshots",
            post(create_group_snapshot).get(list_group_snapshots),
        )
        .route(
            "/group-snapshots/{id}",
            get(get_group_snapshot).delete(delete_group_snapshot),
        )
        .route("/nodes/capacity", get(list_node_capacities))
        .route("/nodes/{node}/capacity", get(get_node_capacity))
        .layer(axum::middleware::from_fn_with_state(state.clone(), require_bearer))
        .with_state(state)
}
