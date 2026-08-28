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
use crate::mgmt::ublk_export::should_offer_ublk;
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

// Serialize as well as Deserialize: the wire-contract fixtures are asserted by
// round-tripping, and a request type that can only be read cannot show that a
// field it stopped reading is still on the wire (#34).
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        /// Namespace ID this volume was hot-added as within `nqn`.
        ///
        /// Volumes share one subsystem so a node connects once and later
        /// attaches cost an async event plus a rescan instead of a fresh
        /// Connect — the node uses this to pick the right namespace out of
        /// the controller it already has.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        nsid: Option<u32>,
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
    /// The same labels as a failure-domain chain, widest rung first
    /// (`site=…/rack=…/node=…`) — what an orchestrator compares at a rung.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub topology_chain: String,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumeRec {
    pub vol: Volume,
    pub local_id: Option<Uuid>,
    /// Engine volume this one was cloned from, when it was created with a
    /// `source`. Reset returns the clone to this volume's contents.
    #[serde(default)]
    pub source_local: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// volume id -> NVMe namespace ID it is hot-added as, so detach can
    /// withdraw the right one.
    #[serde(default)]
    pub nvme_nsids: HashMap<String, u32>,
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
    /// What the on-disk state currently contains, so `save` can journal only
    /// the entries that actually changed. Boxed to keep `V1State` small.
    #[serde(skip)]
    last_persisted: Box<PersistedSnapshot>,
    /// Records appended since the last full snapshot, used to decide when to
    /// compact.
    #[serde(skip)]
    journal_len: usize,
}

/// The persisted subset of `V1State`, kept as the baseline for diffing.
#[derive(Debug, Default, Clone)]
struct PersistedSnapshot {
    volumes: HashMap<String, VolumeRec>,
    snapshots: HashMap<String, SnapshotRec>,
    group_snapshots: HashMap<String, GroupSnapshot>,
    dual_attach: HashMap<String, DualAttachWindow>,
    attachments: HashMap<String, Vec<String>>,
    nvme_nsids: HashMap<String, u32>,
    nodes: BTreeMap<String, NodeCapacity>,
}


/// One persisted change.
///
/// Whole-entity upserts rather than field-level deltas: the entities are
/// small, and it makes replay trivially idempotent — which is what lets
/// compaction be crash-safe (see `compact`).
#[derive(Debug, Clone, Serialize, Deserialize)]
enum Delta {
    /// `None` means the entry was removed.
    Volume(String, Option<VolumeRec>),
    Snapshot(String, Option<SnapshotRec>),
    GroupSnapshot(String, Option<GroupSnapshot>),
    DualAttach(String, Option<DualAttachWindow>),
    Attachments(String, Option<Vec<String>>),
    NvmeNsid(String, Option<u32>),
    Node(String, Option<NodeCapacity>),
}

/// Rewrite the snapshot once the journal has this many records. Bounds both
/// replay time at startup and journal size on disk.
const JOURNAL_COMPACT_THRESHOLD: usize = 512;

/// Diff two maps into whole-entity upserts and removals.
fn diff_map<K, V, F>(old: &HashMap<K, V>, new: &HashMap<K, V>, mut mk: F, out: &mut Vec<Delta>)
where
    K: std::hash::Hash + Eq + Clone + Ord,
    V: PartialEq + Clone,
    F: FnMut(K, Option<V>) -> Delta,
{
    for (k, v) in new {
        if old.get(k) != Some(v) {
            out.push(mk(k.clone(), Some(v.clone())));
        }
    }
    for k in old.keys() {
        if !new.contains_key(k) {
            out.push(mk(k.clone(), None));
        }
    }
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

        // Replay anything journalled since that snapshot. Records are
        // whole-entity upserts applied in order, so a journal that overlaps
        // the snapshot (crash between writing it and dropping the journal)
        // simply re-applies what is already there.
        if let Some(p) = persist_path.as_ref() {
            let jpath = Self::journal_path(p);
            if let Ok(text) = std::fs::read_to_string(&jpath) {
                let mut replayed = 0usize;
                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                    match serde_json::from_str::<Delta>(line) {
                        Ok(d) => { state.apply(d); replayed += 1; }
                        Err(e) => {
                            // A torn final record is expected after a crash
                            // mid-append; everything before it is still good.
                            tracing::warn!("v1 journal: stopping replay at malformed record: {e}");
                            break;
                        }
                    }
                }
                state.journal_len = replayed;
                if replayed > 0 {
                    tracing::info!("v1 state: replayed {replayed} journalled change(s)");
                }
            }
        }

        state.local_node = local_node;
        state.local_topology = config.management.topology.clone();
        state.persist_path = persist_path;
        state.mark_persisted();
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
                topology_chain: crate::placement::domain::FailureDomain::from_labels(
                    topology.iter().map(|(k, v)| (k.clone(), v.clone())),
                )
                .with("node", node)
                .to_string(),
                topology,
            },
        );
    }

    /// Persist whatever changed since the last call.
    ///
    /// Rewriting the whole state on every mutation made every control-plane
    /// operation O(total volumes) — measured at ~0.017 ms per existing
    /// volume, which is ~17 ms per clone at 1000 volumes (#32). This appends
    /// only the entries that actually changed and rewrites the snapshot
    /// occasionally, so the cost tracks the size of the change instead.
    ///
    /// Durability is unchanged: the append is flushed and synced before the
    /// call returns, exactly as the full rewrite was.
    fn save(&mut self) {
        let Some(path) = self.persist_path.clone() else { return };

        let deltas = self.deltas_since_last_write();
        if deltas.is_empty() {
            return;
        }

        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        if self.journal_len + deltas.len() >= JOURNAL_COMPACT_THRESHOLD {
            self.compact(&path);
            return;
        }

        let mut buf = Vec::new();
        for d in &deltas {
            match serde_json::to_vec(d) {
                Ok(mut line) => {
                    line.push(b'\n');
                    buf.extend_from_slice(&line);
                }
                Err(e) => {
                    tracing::warn!("failed to serialize v1 delta: {e}");
                    return;
                }
            }
        }

        let jpath = Self::journal_path(&path);
        let appended = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&jpath)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(&buf)?;
                f.sync_data()
            });

        match appended {
            Ok(()) => {
                self.journal_len += deltas.len();
                self.mark_persisted();
            }
            Err(e) => {
                // Fall back to a full rewrite rather than silently losing the
                // change — correctness beats the optimisation.
                tracing::warn!("v1 journal append failed ({e}), rewriting snapshot");
                self.compact(&path);
            }
        }
    }

    fn journal_path(path: &std::path::Path) -> PathBuf {
        path.with_extension("journal.jsonl")
    }

    /// Write a full snapshot and drop the journal.
    ///
    /// Snapshot first, journal removed second: a crash in between replays
    /// entries already contained in the snapshot, and because every delta is
    /// a whole-entity upsert that is idempotent.
    fn compact(&mut self, path: &std::path::Path) {
        let bytes = match serde_json::to_vec_pretty(self) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("failed to serialize v1 state: {e}");
                return;
            }
        };
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, bytes)
            .and_then(|_| std::fs::rename(&tmp, path))
            .is_err()
        {
            tracing::warn!("failed to persist v1 state to {}", path.display());
            return;
        }
        let _ = std::fs::remove_file(Self::journal_path(path));
        self.journal_len = 0;
        self.mark_persisted();
    }

    /// Entities that differ from what is already on disk.
    fn deltas_since_last_write(&self) -> Vec<Delta> {
        let last = &self.last_persisted;
        let mut out = Vec::new();
        diff_map(&last.volumes, &self.volumes, Delta::Volume, &mut out);
        diff_map(&last.snapshots, &self.snapshots, Delta::Snapshot, &mut out);
        diff_map(&last.group_snapshots, &self.group_snapshots, Delta::GroupSnapshot, &mut out);
        diff_map(&last.dual_attach, &self.dual_attach, Delta::DualAttach, &mut out);
        diff_map(&last.attachments, &self.attachments, Delta::Attachments, &mut out);
        diff_map(&last.nvme_nsids, &self.nvme_nsids, Delta::NvmeNsid, &mut out);

        // nodes is a BTreeMap; same shape, different container.
        for (k, v) in &self.nodes {
            if last.nodes.get(k) != Some(v) {
                out.push(Delta::Node(k.clone(), Some(v.clone())));
            }
        }
        for k in last.nodes.keys() {
            if !self.nodes.contains_key(k) {
                out.push(Delta::Node(k.clone(), None));
            }
        }
        out
    }

    fn mark_persisted(&mut self) {
        self.last_persisted = Box::new(PersistedSnapshot {
            volumes: self.volumes.clone(),
            snapshots: self.snapshots.clone(),
            group_snapshots: self.group_snapshots.clone(),
            dual_attach: self.dual_attach.clone(),
            attachments: self.attachments.clone(),
            nvme_nsids: self.nvme_nsids.clone(),
            nodes: self.nodes.clone(),
        });
    }

    /// Apply one journal record.
    fn apply(&mut self, d: Delta) {
        fn put<K: std::hash::Hash + Eq, V>(m: &mut HashMap<K, V>, k: K, v: Option<V>) {
            match v {
                Some(v) => { m.insert(k, v); }
                None => { m.remove(&k); }
            }
        }
        match d {
            Delta::Volume(k, v) => put(&mut self.volumes, k, v),
            Delta::Snapshot(k, v) => put(&mut self.snapshots, k, v),
            Delta::GroupSnapshot(k, v) => put(&mut self.group_snapshots, k, v),
            Delta::DualAttach(k, v) => put(&mut self.dual_attach, k, v),
            Delta::Attachments(k, v) => put(&mut self.attachments, k, v),
            Delta::NvmeNsid(k, v) => put(&mut self.nvme_nsids, k, v),
            Delta::Node(k, v) => match v {
                Some(v) => { self.nodes.insert(k, v); }
                None => { self.nodes.remove(&k); }
            },
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
    let reg = state.slab_registry.read().await;
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

    // Live peers in this node's cluster, learned from discovery beacons.
    // Stale ones are already filtered out by `cluster_peers`, so a node that
    // has gone quiet stops receiving placements. Statically registered nodes
    // (the test hook) stand behind these and are overridden by them.
    if let Some(disc) = state.discovery.as_ref() {
        for b in disc.cluster_peers().await {
            nodes.insert(
                b.node_name.clone(),
                NodeCapacity {
                    node: b.node_name,
                    total_bytes: b.total_bytes,
                    free_bytes: b.free_bytes,
                    topology: BTreeMap::new(),
                    topology_chain: String::new(),
                },
            );
        }
    }

    let (total, free) = local_capacity(state).await;
    let insert_live = total > 0 || !nodes.contains_key(&v1.local_node);
    if insert_live {
        nodes.insert(
            v1.local_node.clone(),
            NodeCapacity {
                node: v1.local_node.clone(),
                total_bytes: total,
                free_bytes: free,
                topology_chain: crate::placement::domain::FailureDomain::from_labels(
                    v1.local_topology.iter().map(|(k, v)| (k.clone(), v.clone())),
                )
                .with("node", &v1.local_node)
                .to_string(),
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

/// Hot-add a volume as a namespace on the shared subsystem, returning its NSID.
///
/// Reuses the namespace if this volume is already attached, so an attach
/// replay is idempotent and does not leak namespaces. Connected hosts are
/// notified by the target, so no reconnect is needed.
#[cfg(feature = "nvmeof")]
pub(crate) async fn ensure_nvme_namespace(
    state: &AppState,
    volume_id: &str,
    local_id: Option<Uuid>,
) -> Option<u32> {
    let target = state.nvmeof_target.read().await.as_ref().cloned()?;

    if let Some(nsid) = state.v1.lock().await.nvme_nsids.get(volume_id).copied() {
        return Some(nsid);
    }

    let device = state
        .volume_manager
        .lock()
        .await
        .get_volume(&EngineVolumeId(local_id?))?;

    let nsid = target.next_free_nsid().await;
    target.add_namespace_dynamic(nsid, device).await;

    let mut v1 = state.v1.lock().await;
    v1.nvme_nsids.insert(volume_id.to_string(), nsid);
    v1.save();

    tracing::info!("volume {volume_id} hot-added as NVMe namespace {nsid}");
    Some(nsid)
}

/// Withdraw a volume's namespace on detach, so it stops being served and the
/// NSID can be reused.
#[cfg(feature = "nvmeof")]
pub(crate) async fn release_nvme_namespace(state: &AppState, volume_id: &str) {
    let nsid = {
        let mut v1 = state.v1.lock().await;
        let nsid = v1.nvme_nsids.remove(volume_id);
        if nsid.is_some() {
            v1.save();
        }
        nsid
    };
    let Some(nsid) = nsid else { return };

    if let Some(target) = state.nvmeof_target.read().await.as_ref() {
        target.remove_namespace(nsid).await;
        tracing::info!("volume {volume_id} withdrawn from NVMe namespace {nsid}");
    }
}

pub(crate) fn attach_info_for(state: &AppState, nsid: Option<u32>) -> AttachInfo {
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
    // A wildcard listen address tells a remote consumer nothing, so prefer the
    // configured advertised address (#26).
    let traddr = state.config.management.resolve_advertised_host(&host);

    // Volumes share the target's subsystem and are distinguished by NSID.
    // A per-volume NQN would force a Connect per container, which is the
    // overhead the hot-add path exists to avoid.
    let nqn = {
        #[cfg(feature = "nvmeof")]
        {
            state
                .config
                .nvmeof
                .as_ref()
                .map(|n| n.nqn.clone())
                .unwrap_or_else(|| crate::target::nvmeof::NvmeofConfig::default().nqn)
        }
        #[cfg(not(feature = "nvmeof"))]
        {
            "nqn.2024.io.stormblock:default".to_string()
        }
    };

    AttachInfo::NvmeTcp {
        nqn,
        addresses: vec![NvmeAddress { traddr, trsvcid: port }],
        nsid,
    }
}

// ---------------------------------------------------------------------------
// Volume handlers
// ---------------------------------------------------------------------------

/// The `qos_class` taxonomy, agreed with stormblock-csi (its #10, ours #35).
///
/// The wire field stays a string — only the accepted set is pinned. Pinning it
/// on both sides is the point: a class added or renamed on one side surfaces as
/// a rejected create rather than as a string that is carried, stored and never
/// acted on, which is indistinguishable from working until someone looks at
/// what the volume actually got.
pub const QOS_CLASSES: [&str; 4] = ["bronze", "silver", "gold", "platinum"];

/// Reject a `qos_class` outside the agreed taxonomy. Absent stays valid: not
/// asking for a class is not the same as asking for one that does not exist.
fn validate_qos_class(class: Option<&String>) -> Result<(), V1Error> {
    match class {
        None => Ok(()),
        Some(c) if QOS_CLASSES.contains(&c.as_str()) => Ok(()),
        Some(c) => Err(V1Error::BadRequest(format!(
            "unknown qos_class {c:?} (expected one of {})",
            QOS_CLASSES.join(", ")
        ))),
    }
}

async fn create_volume(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateVolumeRequest>,
) -> V1Result<Volume> {
    // Before anything is looked up or allocated, and before the idempotency
    // check: a request naming a class that does not exist is malformed whether
    // or not a volume by that name is already here.
    validate_qos_class(req.qos_class.as_ref())?;

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
        Some(VolumeSource::Volume(id)) => match v1.volumes.get(id) {
            Some(rec) => Some(rec.local_id.unwrap_or_default()).filter(|u| !u.is_nil()),
            // Not a /v1 volume: an engine volume by id or name — a blank the
            // image shipped, a golden sealed through /api/v1 (#78). Same
            // clone underneath, so the source may come from either door.
            None => match state.volume_manager.lock().await.find_volume(id).await {
                Some(v) => Some(v.0),
                None => return Err(V1Error::NotFound(format!("volume {id}"))),
            },
        },
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
        // A source that carries a filesystem is cloned with its own identity
        // (#76); anything else is the plain map clone.
        let has_fs = {
            let vm = state.volume_manager.lock().await;
            source_local.is_some_and(|src| vm.fs_info(&EngineVolumeId(src)).is_some())
        };
        let created = match source_local {
            Some(src) if has_fs => {
                let mut spec = crate::fs::template::CloneSpec::new(&req.name);
                spec.verify = false;
                crate::fs::template::clone_volume_unsealed_ok(&state.volume_manager, EngineVolumeId(src), &spec)
                    .await
                    .map(|c| c.volume_id)
                    .map_err(|e| crate::volume::VolumeError::AllocatorError(e.to_string()))
            }
            Some(src) => state.volume_manager.lock().await.create_snapshot(EngineVolumeId(src), &req.name).await,
            None => state.volume_manager.lock().await.create_volume_any(&req.name, req.size_bytes).await,
        };
        let mut vm = state.volume_manager.lock().await;
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
    v1.volumes.insert(
        vol.id.clone(),
        VolumeRec { vol: vol.clone(), local_id, source_local },
    );
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
    let removed = v1.volumes.remove(&id);
    if let Some(rec) = removed {
        account_static_nodes(&mut v1, &rec.vol.replicas, rec.vol.size_bytes, false);
        v1.attachments.remove(&id);
        v1.dual_attach.remove(&id);
        v1.save();
        drop(v1);

        // Stop serving it before the backing storage goes away — otherwise a
        // deleted COW image leaves a namespace pointing at freed slots, which
        // the container-restart cycle would hit constantly.
        #[cfg(feature = "nvmeof")]
        release_nvme_namespace(&state, &id).await;

        if let Some(local) = rec.local_id {
            state.ublk_exports.lock().await.remove(&id);
            let mut vm = state.volume_manager.lock().await;
            if let Err(e) = vm.delete_volume(EngineVolumeId(local)).await {
                tracing::warn!("backing volume {local} delete: {e}");
            }
        }
    }
    // Idempotent: deleting an absent volume succeeds.
    Ok(Json(serde_json::json!({})))
}

#[derive(Serialize)]
struct ResetResponse {
    /// Diverged extents whose private copy was released.
    freed_extents: usize,
    /// Extents re-pointed at the source's data.
    restored_extents: usize,
    /// Extents already identical to the source, left untouched.
    shared_extents: usize,
}

/// POST /v1/volumes/{id}/reset — discard divergence, back to the source.
///
/// For the clone-per-container model this replaces delete-and-reclone: the
/// volume keeps its identity and attachment while its contents go back to the
/// golden image, and only the extents the container actually wrote are
/// touched.
async fn reset_volume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> V1Result<ResetResponse> {
    let (local_id, source_local, attached) = {
        let v1 = state.v1.lock().await;
        let rec = v1
            .volumes
            .get(&id)
            .ok_or_else(|| V1Error::NotFound(format!("volume {id}")))?;
        let attached = v1
            .attachments
            .get(&id)
            .is_some_and(|nodes| !nodes.is_empty());
        (rec.local_id, rec.source_local, attached)
    };

    // Contents would change underneath a live host, which no filesystem
    // tolerates — the caller resets between runs, not during one.
    if attached {
        return Err(V1Error::Conflict(format!(
            "volume {id} is attached; detach before resetting"
        )));
    }

    let source_local = source_local.ok_or_else(|| {
        V1Error::Conflict(format!("volume {id} was not created from a source"))
    })?;
    let local_id = local_id.ok_or_else(|| {
        V1Error::Conflict(format!("volume {id} has no local backing on this node"))
    })?;

    let mut vm = state.volume_manager.lock().await;
    let stats = vm
        .reset_volume(EngineVolumeId(local_id), EngineVolumeId(source_local))
        .await
        .map_err(|e| V1Error::Internal(format!("reset failed: {e}")))?;

    tracing::info!(
        "volume {id} reset: {} freed, {} restored, {} shared",
        stats.freed, stats.restored, stats.shared
    );

    Ok(Json(ResetResponse {
        freed_extents: stats.freed,
        restored_extents: stats.restored,
        shared_extents: stats.shared,
    }))
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
            } else {
                drop(vm);
                // Layer 3: the kernel device has to learn the new size, or the
                // volume grew and nothing above it can tell (#19). A failure
                // here is loud: the node is about to run `xfs_growfs` against a
                // device that did not move.
                match state.ublk_exports.lock().await.update_size(&id, req.size_bytes) {
                    Ok(true) => tracing::info!("volume {id}: ublk device resized to {}", req.size_bytes),
                    Ok(false) => {}
                    Err(e) => tracing::error!(
                        "volume {id} grew to {} but its ublk device did not follow: {e}",
                        req.size_bytes
                    ),
                }
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
    // Captured before the mutable borrow below; drives the transport choice.
    let local_id = rec.local_id;
    let local_node = v1.local_node.clone();

    let entry = v1.attachments.entry(id.clone()).or_default();
    if !entry.contains(&req.node) {
        entry.push(req.node.clone());
    }
    v1.save();
    drop(v1);

    // Local fast path: when configured and the master is on this node, export
    // the backing device as a local /dev/ublkbN instead of NVMe-oF/TCP. Any
    // miss (disabled, remote node, ublk unavailable) falls through to
    // nvme-tcp, which always works — so this is a pure optimization.
    if should_offer_ublk(
        state.config.management.ublk_transport,
        &req.node,
        &local_node,
        local_id.is_some(),
    ) {
        if let Some(local) = local_id {
            let device = state.volume_manager.lock().await.get_volume(&EngineVolumeId(local));
            if let Some(device) = device {
                if let Some(path) = state.ublk_exports.lock().await.ensure(&id, device) {
                    return Ok(Json(AttachInfo::Ublk { device_hint: path }));
                }
            }
        }
    }
    // NVMe-oF path: hot-add the volume as a namespace on the shared
    // subsystem. A node that is already connected picks it up from the async
    // event with no Connect at all.
    #[cfg(feature = "nvmeof")]
    let nsid = ensure_nvme_namespace(&state, &id, local_id).await;
    #[cfg(not(feature = "nvmeof"))]
    let nsid = None;

    Ok(Json(attach_info_for(&state, nsid)))
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
    let local_node = v1.local_node.clone();
    if let Some(nodes) = v1.attachments.get_mut(&id) {
        nodes.retain(|n| n != &req.node);
        v1.save();
    }
    drop(v1);
    // If the local ublk fast path was serving this node, tear the device down
    // now — its lifetime is the attachment, and the CSI node deliberately
    // never disconnects a ublk device itself.
    if req.node == local_node {
        state.ublk_exports.lock().await.remove(&id);
    }
    // Stop serving the namespace once nothing is attached, so a dropped
    // container's volume does not linger in every connected host's scan.
    let still_attached = state
        .v1
        .lock()
        .await
        .attachments
        .get(&id)
        .is_some_and(|nodes| !nodes.is_empty());
    #[cfg(feature = "nvmeof")]
    if !still_attached {
        release_nvme_namespace(&state, &id).await;
    }
    #[cfg(not(feature = "nvmeof"))]
    let _ = still_attached;
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
        .route("/volumes/{id}/reset", post(reset_volume))
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

#[cfg(test)]
mod persistence_tests {
    use super::*;

    fn state_at(dir: &std::path::Path) -> V1State {
        let mut s = V1State::default();
        s.persist_path = Some(dir.join("v1_state.json"));
        s.mark_persisted();
        s
    }

    fn reload(dir: &std::path::Path) -> V1State {
        let path = dir.join("v1_state.json");
        let mut state = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<V1State>(&b).ok())
            .unwrap_or_default();
        if let Ok(text) = std::fs::read_to_string(V1State::journal_path(&path)) {
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                if let Ok(d) = serde_json::from_str::<Delta>(line) {
                    state.apply(d);
                }
            }
        }
        state
    }

    fn vol(name: &str) -> VolumeRec {
        VolumeRec {
            vol: Volume {
                id: format!("vol-{name}"),
                name: name.to_string(),
                size_bytes: 1 << 20,
                epoch: 1,
                replicas: Vec::new(),
                health: VolumeHealth::Healthy,
                encrypted: false,
                qos_class: None,
                bandwidth_class: BandwidthClass::Normal,
            },
            local_id: None,
            source_local: None,
        }
    }

    /// A save must write only what changed, not the whole state — that is the
    /// entire point of #32.
    #[test]
    fn save_journals_only_the_change() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = state_at(dir.path());

        for i in 0..50 {
            s.volumes.insert(format!("v{i}"), vol(&format!("v{i}")));
        }
        s.save();

        // One more volume: the journal should grow by roughly one record,
        // not by the size of all 51.
        let before = std::fs::metadata(V1State::journal_path(&dir.path().join("v1_state.json")))
            .map(|m| m.len())
            .unwrap_or(0);
        s.volumes.insert("late".into(), vol("late"));
        s.save();
        let after = std::fs::metadata(V1State::journal_path(&dir.path().join("v1_state.json")))
            .unwrap()
            .len();

        let grew = after - before;
        assert!(grew > 0, "the change must be persisted");
        assert!(
            grew < before / 10,
            "one more volume grew the journal by {grew} bytes against {before} for 50 — not O(change)"
        );

        // And a save with nothing changed writes nothing at all.
        let steady = std::fs::metadata(V1State::journal_path(&dir.path().join("v1_state.json")))
            .unwrap().len();
        s.save();
        assert_eq!(
            std::fs::metadata(V1State::journal_path(&dir.path().join("v1_state.json"))).unwrap().len(),
            steady,
            "a no-op save must not write"
        );
    }

    /// Everything written must come back, including removals.
    #[test]
    fn journal_replays_to_the_same_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = state_at(dir.path());

        s.volumes.insert("a".into(), vol("a"));
        s.volumes.insert("b".into(), vol("b"));
        s.save();
        s.attachments.insert("a".into(), vec!["n1".into()]);
        s.nvme_nsids.insert("a".into(), 7);
        s.save();
        s.volumes.remove("b");
        s.save();

        let back = reload(dir.path());
        assert!(back.volumes.contains_key("a"));
        assert!(!back.volumes.contains_key("b"), "removal must survive replay");
        assert_eq!(back.attachments.get("a"), Some(&vec!["n1".to_string()]));
        assert_eq!(back.nvme_nsids.get("a"), Some(&7));
    }

    /// Crossing the threshold rewrites the snapshot and drops the journal,
    /// and the state must be identical either way.
    #[test]
    fn compaction_folds_the_journal_into_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = state_at(dir.path());
        let jpath = V1State::journal_path(&dir.path().join("v1_state.json"));

        for i in 0..(JOURNAL_COMPACT_THRESHOLD + 10) {
            s.volumes.insert(format!("v{i}"), vol(&format!("v{i}")));
            s.save();
        }

        assert!(dir.path().join("v1_state.json").exists(), "snapshot must be written");

        // Compaction fires partway through and the journal legitimately grows
        // again afterwards; what matters is that it was folded in, so the
        // journal holds far fewer records than the number of saves.
        let lines = std::fs::read_to_string(&jpath)
            .map(|t| t.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0);
        assert!(
            lines < JOURNAL_COMPACT_THRESHOLD,
            "journal has {lines} records after {} saves — compaction did not run",
            JOURNAL_COMPACT_THRESHOLD + 10
        );

        // Either way the state must round-trip exactly.
        let back = reload(dir.path());
        assert_eq!(back.volumes.len(), JOURNAL_COMPACT_THRESHOLD + 10);
    }

    /// A crash between writing the snapshot and dropping the journal leaves
    /// both on disk. Replay must be idempotent, or startup would corrupt.
    #[test]
    fn replaying_a_journal_that_overlaps_the_snapshot_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = state_at(dir.path());
        let path = dir.path().join("v1_state.json");

        s.volumes.insert("a".into(), vol("a"));
        s.save();

        // Simulate the crash window: snapshot written, journal still present.
        let journal = std::fs::read(V1State::journal_path(&path)).unwrap();
        s.compact(&path);
        std::fs::write(V1State::journal_path(&path), &journal).unwrap();

        let back = reload(dir.path());
        assert_eq!(back.volumes.len(), 1, "re-applied upsert must not duplicate");
        assert!(back.volumes.contains_key("a"));
    }

    /// A torn final record (crash mid-append) must not discard the good ones
    /// before it.
    #[test]
    fn truncated_journal_record_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = state_at(dir.path());
        let path = dir.path().join("v1_state.json");

        s.volumes.insert("a".into(), vol("a"));
        s.save();

        let jpath = V1State::journal_path(&path);
        let mut text = std::fs::read_to_string(&jpath).unwrap();
        text.push_str("{\"Volume\":[\"b\",{\"vol\":{\"id\":\"vol-b\"");  // torn
        std::fs::write(&jpath, text).unwrap();

        let back = reload(dir.path());
        assert!(back.volumes.contains_key("a"), "records before the tear survive");
        assert!(!back.volumes.contains_key("b"));
    }
}
