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
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::drive::BlockDevice;
use crate::mgmt::ublk_export::{should_offer_ublk, WantTransport};
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
    /// What is attached, each at the epoch it was attached at (#83, #6). A
    /// fence revokes every attachment below the epoch it moves to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
}

/// One attachment of a volume: who, how, and at which epoch (#83, #6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    pub node: String,
    /// The host it is served to, from its own subsystem (#210). Absent: the
    /// node's shared subsystem, or a local ublk device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_nqn: Option<String>,
    pub epoch: Epoch,
    /// `nvme_tcp`, `ublk`, or `none` (nothing served from this node).
    pub transport: String,
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
    /// Where the backing volume lives on this node (#150).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<CreatePlacement>,
    /// The size of the volume's extents, bytes (#156): a StorageClass's
    /// `extentSize`. Absent: the node chooses (8 MiB from 64 GiB where it
    /// has a bulk pool). A clone takes its source's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extent_size_bytes: Option<u64>,
    /// A StorageClass's `redundancy` (#151), spelled as on `/api/v1`:
    /// `mirror`, `mirror:3`, `raid5:4+1`, `raid6:4+2`, optionally `@rung`.
    /// Absent: none. Every leg on a distinct failure domain, or the create
    /// is refused (409).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<String>,
    /// The failure-domain rung the legs differ at (`drive`, `shelf`, `rack`,
    /// …), the same as `@rung` (#151).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spread: Option<String>,
    /// The tier the volume's extents go to first (`hot`, `warm`, `cool`,
    /// `cold`), falling back to the others (#151). Absent: hot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
}

/// `placement` on a `/v1` create.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CreatePlacement {
    /// Carve the volume on this array: every extent on its slab. The array
    /// is on this node, so the master is this node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub array_id: Option<String>,
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
        /// The host this was attached for, when one was named: `nqn` is then
        /// that host's own subsystem, and no other host can connect to it
        /// (#210).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host_nqn: Option<String>,
        /// The DH-HMAC-CHAP secret the host must present
        /// (`nvme connect --dhchap-secret`), when it has one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dhchap_secret: Option<String>,
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
    /// A request this engine cannot honour as asked (#232): 422
    /// `unsupported`, never a success that pretends.
    Unsupported(String),
    /// Kept because it names this contract's 401 body; the check that raises
    /// it is `mgmt::auth::require_token`, which builds the same envelope for
    /// any `/v1` path.
    #[allow(dead_code)]
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
            V1Error::Unsupported(m) => (StatusCode::UNPROCESSABLE_ENTITY, "unsupported", m, None),
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

/// Why `encrypted: true` is refused (#232).
pub const ENCRYPTION_UNSUPPORTED: &str = "encrypted: true is not supported: this engine does not \
    encrypt volumes at rest (stormblock#74 is the design, not built; #232). Create the volume \
    without encryption, or use a storage class that does not ask for it";

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
        // One place decides what this node is called. Two copies of this
        // fallback is how /v1 and the attach path came to disagree about the
        // node's own name.
        let local_node = crate::mgmt::local_node_name(config);
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
        // A volume an earlier engine recorded `encrypted: true` never was
        // (#232). Report what is true; the next persist writes it back.
        for rec in state.volumes.values_mut() {
            if rec.vol.encrypted {
                tracing::warn!(
                    "v1 volume {} ({}) was recorded encrypted: true by an earlier engine; it holds \
                     plaintext (nothing here encrypts, #232) and is reported encrypted: false",
                    rec.vol.name,
                    rec.vol.id
                );
                rec.vol.encrypted = false;
            }
        }
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

    /// Take the dual-attach windows that have expired out of the state:
    /// `(volume, target node)`. [`expire_windows`] aborts them.
    fn take_expired(&mut self, now_ms: i64) -> Vec<(String, String)> {
        let expired: Vec<String> = self
            .dual_attach
            .iter()
            .filter(|(_, w)| w.expires_at_ms <= now_ms)
            .map(|(vid, _)| vid.clone())
            .collect();
        expired
            .into_iter()
            .filter_map(|vid| self.dual_attach.remove(&vid).map(|w| (vid, w.target_node)))
            .collect()
    }
}

/// Abort every dual-attach window that has expired (#195): the target's
/// attachments go the way a `close {outcome: abort}` takes them — records,
/// and the data path behind them (ublk device, namespace).
///
/// Run on every `/v1` call, detach and reads included, and by a timer
/// ([`expiry_loop`]): an expired window must not hold the target's access, or
/// block a promote, until some other call happens by.
async fn expire_windows(state: &AppState, v1: &mut V1State) {
    for (vid, target) in v1.take_expired(now_ms()) {
        tracing::info!("dual-attach window on {vid} expired; auto-aborting");
        drop_node_attachments(state, v1, &vid, &target).await;
    }
}

/// Take away every attachment `node` holds on `id`: its records and the data
/// path behind each (ublk device, namespace), keeping what other nodes share.
async fn drop_node_attachments(state: &AppState, v1: &mut V1State, id: &str, node: &str) {
    if let Some(nodes) = v1.attachments.get_mut(id) {
        nodes.retain(|n| n != node);
    }
    let Some(rec) = v1.volumes.get_mut(id) else {
        v1.save();
        return;
    };
    let local = rec.local_id;
    let (gone, kept): (Vec<Attachment>, Vec<Attachment>) =
        rec.vol.attachments.drain(..).partition(|a| a.node == node);
    rec.vol.attachments = kept.clone();
    let shared = if gone.iter().any(|a| a.transport == "nvme_tcp" && a.host_nqn.is_none())
        && !kept.iter().any(|a| a.transport == "nvme_tcp" && a.host_nqn.is_none())
    {
        v1.nvme_nsids.remove(id)
    } else {
        None
    };
    v1.save();
    for att in &gone {
        revoke_attachment(state, id, local, att, &kept, shared).await;
    }
}

/// Expire dual-attach windows on time (#195), not only when a call happens
/// by. Ends when the state is gone.
async fn expiry_loop(state: std::sync::Weak<AppState>) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let Some(state) = state.upgrade() else { return };
        let mut v1 = state.v1.lock().await;
        if v1.dual_attach.values().any(|w| w.expires_at_ms <= now_ms()) {
            expire_windows(&state, &mut v1).await;
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
pub(crate) async fn nodes_view(state: &AppState, v1: &V1State) -> BTreeMap<String, NodeCapacity> {
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
                    topology: b.topology,
                    topology_chain: b.topology_chain,
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
/// Serve a volume as a subsystem of its own, and remember where.
///
/// The NQN carries the volume GUID, so the address names what it serves:
/// nothing to go stale, a deleted volume stops answering rather than
/// resolving to whatever inherited its namespace number, and the same volume
/// served from several places is the same NQN — which is what NVMe multipath
/// is and the reason a shared subsystem cannot express it (#98, #99).
///
/// Idempotent: a volume already served keeps its address, because handing out
/// a new one would change it under whoever holds the old.
pub async fn ensure_volume_subsystem(
    state: &AppState,
    volume: Uuid,
    access: crate::target::nvmeof::HostAccess,
) -> Option<(String, u16)> {
    if let Some(found) = state.nvme_portals.read().await.get(&volume).cloned() {
        if let Some(s) = state.volume_subsystems.lock().await.get(&volume) {
            s.sub.set_access(access);
        }
        return Some(found);
    }
    let cfg = state.per_volume.read().await.clone()?;
    let ctx = cfg.serve.upgrade()?;
    let device = state
        .volume_manager
        .lock()
        .await
        .get_volume(&EngineVolumeId(volume))?;

    let nqn = format!("{}:vol-{volume}", cfg.nqn_prefix);
    // A subsystem of the serve listener, as every per-volume export is
    // (#188): one listener for every volume, the NQN telling them apart at
    // connect. It had a listener and a port of its own each, from the same
    // range the serve listener binds at (#98, #99).
    let (target, port) = match crate::serve::reconcile::nvme_listener(&ctx).await {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!("volume {volume} not served as its own subsystem: {e}");
            return None;
        }
    };
    // Who may connect, before anything can (#210).
    let sub = target.ensure_subsystem(&nqn, access.clone());
    sub.set_access(access);
    // Namespace 1, always: the volume is the only namespace here, so the
    // number carries no information and nothing can disagree about it.
    // A golden is served write-protected.
    let sealed = state.volume_manager.lock().await.is_sealed(&EngineVolumeId(volume));
    if !sub.add_namespace_at(1, device.clone(), sealed).await && !sub.serves(device.id().uuid).await {
        tracing::warn!("{nqn}: namespace 1 is another volume's; not served");
        return None;
    }
    state.volume_subsystems.lock().await.insert(
        volume,
        crate::mgmt::VolumeSubsystem { nqn: nqn.clone(), port, target: target.clone(), sub },
    );
    state.nvme_portals.write().await.insert(volume, (nqn.clone(), port));
    tracing::info!("volume {volume} served as {nqn} on the serve listener (port {port})");
    Some((nqn, port))
}

pub(crate) async fn ensure_nvme_namespace(
    state: &AppState,
    volume_id: &str,
    local_id: Option<Uuid>,
) -> Option<u32> {
    ensure_nvme_namespace_ro(state, volume_id, local_id, false).await
}

#[cfg(feature = "nvmeof")]
pub(crate) async fn ensure_nvme_namespace_ro(
    state: &AppState,
    volume_id: &str,
    local_id: Option<Uuid>,
    read_only: bool,
) -> Option<u32> {
    let target = state.nvmeof_target.read().await.as_ref().cloned()?;

    // One lock from the check to the record: two attaches of one volume at
    // once must not each add a namespace, and the ID is chosen and taken in
    // one step (`add_namespace_next`) so two volumes never share one (#139).
    let mut v1 = state.v1.lock().await;
    if let Some(nsid) = v1.nvme_nsids.get(volume_id).copied() {
        return Some(nsid);
    }

    let device = state
        .volume_manager
        .lock()
        .await
        .get_volume(&EngineVolumeId(local_id?))?;

    let nsid = target.default_subsystem().add_namespace_next(device, read_only).await;
    v1.nvme_nsids.insert(volume_id.to_string(), nsid);
    v1.save();

    tracing::info!("volume {volume_id} hot-added as NVMe namespace {nsid}");
    Some(nsid)
}

/// Put the `/v1` and `/api/v1` attach records back on the shared subsystem
/// at the NSIDs they were given.
///
/// They were persisted and never restored, so after a restart a record named
/// an NSID nothing served — until the next attach of *another* volume took
/// that NSID, and this volume's consumer read that one. And an attach of a
/// volume already exported added it a second time: two namespaces with one
/// NGUID, which the kernel reports as "duplicate IDs in subsystem" (#210). A
/// record whose volume is gone, whose NSID is taken, or whose volume is a
/// sealed golden (never on the shared subsystem) is dropped, loudly.
#[cfg(feature = "nvmeof")]
pub async fn restore_nvme_nsids(state: &AppState) -> usize {
    let Some(target) = state.nvmeof_target.read().await.as_ref().cloned() else { return 0 };
    let shared = target.default_subsystem();
    let mut v1 = state.v1.lock().await;
    let records: Vec<(String, u32)> = v1.nvme_nsids.iter().map(|(k, v)| (k.clone(), *v)).collect();
    let mut restored = 0;
    let mut changed = false;
    for (key, nsid) in records {
        let local = v1
            .volumes
            .get(&key)
            .and_then(|r| r.local_id)
            .or_else(|| key.parse::<Uuid>().ok());
        let (device, sealed) = match local {
            Some(l) => {
                let vm = state.volume_manager.lock().await;
                (vm.get_volume(&EngineVolumeId(l)), vm.is_sealed(&EngineVolumeId(l)))
            }
            None => (None, false),
        };
        let kept = match device {
            None => {
                tracing::warn!("NVMe namespace {nsid} for {key}: volume gone; record dropped");
                false
            }
            Some(_) if sealed => {
                tracing::warn!(
                    "NVMe namespace {nsid} for {key}: a sealed golden, never served on the shared \
                     subsystem (#210); record dropped — attach it for a host instead"
                );
                false
            }
            Some(dev) => {
                // Already there at this NSID (an export of the same volume)
                // counts as restored.
                if shared.nsid_of(dev.id().uuid).await == Some(nsid) || shared.add_namespace_at(nsid, dev, false).await {
                    restored += 1;
                    true
                } else {
                    tracing::warn!(
                        "NVMe namespace {nsid} for {key}: taken by another volume, or the volume is \
                         already served at another NSID; record dropped (#210)"
                    );
                    false
                }
            }
        };
        if !kept {
            v1.nvme_nsids.remove(&key);
            changed = true;
        }
    }
    if changed {
        v1.save();
    }
    if restored > 0 {
        tracing::info!("restored {restored} attach namespace(s) on {}", shared.nqn());
    }
    restored
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
    withdraw_shared_namespace(state, volume_id, nsid).await;
}

/// Take a volume's namespace off the shared subsystem, its record already
/// gone — unless an export serves it there too.
#[cfg(feature = "nvmeof")]
async fn withdraw_shared_namespace(state: &AppState, volume_id: &str, nsid: u32) {
    // An export may serve the same volume at the same NSID (a subsystem
    // never holds one volume twice): it keeps the namespace.
    if state.exports.read().await.iter().any(|e| e.nsid == Some(nsid) && e.subsystem.is_none()) {
        return;
    }

    if let Some(target) = state.nvmeof_target.read().await.as_ref() {
        target.remove_namespace(nsid).await;
        tracing::info!("volume {volume_id} withdrawn from NVMe namespace {nsid}");
    }
}

/// Where a remote initiator dials this node's NVMe-oF listener.
pub(crate) fn nvme_addresses(state: &AppState, port: Option<u16>) -> Vec<NvmeAddress> {
    match attach_info_for(state, None) {
        AttachInfo::NvmeTcp { mut addresses, .. } => {
            if let Some(p) = port {
                for a in &mut addresses {
                    a.trsvcid = p;
                }
            }
            addresses
        }
        AttachInfo::Ublk { .. } => Vec::new(),
    }
}

pub(crate) fn attach_info_for(state: &AppState, nsid: Option<u32>) -> AttachInfo {
    let listen = {
        #[cfg(feature = "nvmeof")]
        {
            state
                .nvmeof_settings()
                .map(|n| n.listen_addr)
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
                .nvmeof_settings()
                .map(|n| n.nqn)
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
        host_nqn: None,
        dhchap_secret: None,
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
    // Nothing in this engine encrypts (#232): a volume that said it was
    // encrypted held plaintext. Refused until #74's design is built — before
    // the idempotency check too, so an earlier volume of the name is never
    // handed back as the answer to a request for encryption.
    if req.encrypted {
        return Err(V1Error::Unsupported(ENCRYPTION_UNSUPPORTED.to_string()));
    }
    // The claim's policy (#151), checked before anything is allocated.
    let policy = crate::volume::RedundancyPolicy::from_request(req.redundancy.as_deref(), req.spread.as_deref())
        .map_err(|e| V1Error::BadRequest(format!("redundancy: {e}")))?;
    let tier = match req.tier.as_deref() {
        None => crate::placement::topology::StorageTier::Hot,
        Some(t) => super::slabs::parse_tier(t)
            .ok_or_else(|| V1Error::BadRequest(format!("tier {t}: hot, warm, cool or cold")))?,
    };
    let asked_policy = req.redundancy.is_some() || req.spread.is_some();
    if (asked_policy && !policy.is_none() || req.tier.is_some())
        && req.placement.as_ref().is_some_and(|p| p.array_id.is_some())
    {
        return Err(V1Error::BadRequest(
            "a volume carved on an array is that array's storage: no redundancy, spread or tier of its own".into(),
        ));
    }

    let mut v1 = state.v1.lock().await;
    expire_windows(&state, &mut v1).await;

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

    // Pinned to a local array (#150): the master is this node, and the volume
    // is carved on the array rather than cloned or placed.
    let pin_array: Option<crate::raid::RaidArrayId> = match req.placement.as_ref().and_then(|p| p.array_id.as_deref()) {
        None => None,
        Some(a) => {
            let id = a
                .parse::<Uuid>()
                .map_err(|_| V1Error::BadRequest(format!("placement.array_id {a} is not a uuid")))?;
            if req.source.is_some() {
                return Err(V1Error::BadRequest(
                    "placement.array_id carves a new volume on the array; it cannot also be a clone".into(),
                ));
            }
            if req.master_node.as_deref().is_some_and(|m| m != v1.local_node) {
                return Err(V1Error::BadRequest(format!(
                    "array {a} is on {}, not {}", v1.local_node, req.master_node.as_deref().unwrap_or("")
                )));
            }
            if state.volume_manager.lock().await.array_slab(&crate::raid::RaidArrayId(id)).is_none() {
                return Err(V1Error::NotFound(format!("array {a}")));
            }
            Some(crate::raid::RaidArrayId(id))
        }
    };
    let master_node = match pin_array {
        Some(_) => Some(v1.local_node.clone()),
        None => req.master_node.clone(),
    };

    let nodes = nodes_view(&state, &v1).await;
    let (master, slaves) = pick_nodes(
        &nodes,
        req.size_bytes,
        master_node.as_deref(),
        &req.excluded_nodes,
        req.replica_tier.slaves,
    )?;

    // A clone takes its source's policy and tier (#151): a claim that names
    // another is refused, not quietly given the source's.
    if let Some(src) = source_local {
        let vm = state.volume_manager.lock().await;
        if let Some(h) = vm.get_volume_handle(&EngineVolumeId(src)) {
            let theirs = h.redundancy();
            if asked_policy && theirs != policy {
                return Err(V1Error::Conflict(format!(
                    "a clone takes its source's redundancy ({}), not {}",
                    theirs.spelling(),
                    policy.spelling()
                )));
            }
            if req.tier.is_some() && h.preferred_tier() != tier {
                return Err(V1Error::Conflict(format!(
                    "a clone takes its source's tier ({}), not {tier}",
                    h.preferred_tier()
                )));
            }
        }
    }

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
            None => match pin_array {
                Some(a) => state.volume_manager.lock().await.create_volume(&req.name, req.size_bytes, a).await,
                None => {
                    state
                        .volume_manager
                        .lock()
                        .await
                        .create_volume_with(
                            &req.name,
                            req.size_bytes,
                            crate::volume::CreateOptions {
                                redundancy: policy.clone(),
                                placement: crate::volume::PlacementPolicy::preferring(tier),
                                ..Default::default()
                            }
                            .with_extent_size(req.extent_size_bytes),
                        )
                        .await
                }
            },
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
            // Refused the way `/api/v1` refuses it (#151).
            Err(e @ crate::volume::VolumeError::InsufficientDomains { .. }) => return Err(V1Error::Conflict(e.to_string())),
            Err(crate::volume::VolumeError::NoSpace) => {
                return Err(V1Error::OutOfSpace("no slab has room for the volume".into()))
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
        // Refused above when asked for (#232).
        encrypted: false,
        qos_class: req.qos_class,
        bandwidth_class: req.bandwidth_class,
        attachments: Vec::new(),
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
    let mut v1 = state.v1.lock().await;
    expire_windows(&state, &mut v1).await;
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
    let mut v1 = state.v1.lock().await;
    expire_windows(&state, &mut v1).await;
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
    /// `nvme_tcp` or `ublk`; absent is the engine's choice (#149). A caller
    /// attaching on the master's behalf for a remote initiator — a RAID head
    /// assembling legs, a consumer on another machine — sends `nvme_tcp`,
    /// since from the master's own node the engine would otherwise offer a
    /// local ublk device.
    #[serde(default)]
    transport: Option<String>,
    /// The host NQN that will connect. The volume is then served from that
    /// host's own subsystem and no other host sees it (#210). Required for
    /// NVMe/TCP on a node whose shared subsystem admits no host.
    #[serde(default)]
    host_nqn: Option<String>,
    /// Give that host a DH-HMAC-CHAP secret (returned as `dhchap_secret`).
    #[serde(default)]
    dhchap: bool,
    /// The volume's epoch as the caller last saw it (#83, #6): what its last
    /// fence returned. Another epoch is refused (412 `stale_epoch`); so is
    /// leaving it out once the volume has been fenced, or a zombie could
    /// reattach by not saying. Absent at epoch 1: accepted, as before.
    #[serde(default)]
    epoch: Option<Epoch>,
}

/// Refuse an attach at an epoch that is not the volume's (#83, #6).
fn check_attach_epoch(current: Epoch, asked: Option<Epoch>) -> Result<Epoch, V1Error> {
    match asked {
        Some(e) if e != current => Err(V1Error::StaleEpoch(current)),
        None if current > 1 => Err(V1Error::StaleEpoch(current)),
        _ => Ok(current),
    }
}

/// What serves an attachment here, from what the attach answered.
fn attachment_of(node: &str, epoch: Epoch, info: &AttachInfo) -> Attachment {
    let (transport, host_nqn) = match info {
        AttachInfo::Ublk { .. } => ("ublk", None),
        AttachInfo::NvmeTcp { nsid: Some(_), host_nqn, .. } => ("nvme_tcp", host_nqn.clone()),
        AttachInfo::NvmeTcp { nsid: None, .. } => ("none", None),
    };
    Attachment { node: node.to_string(), host_nqn, epoch, transport: transport.to_string() }
}

/// Take an attachment's data path away (#83, #6): its namespace leaves the
/// host's subsystem (or the shared one), or its ublk device goes. A
/// namespace removal returns once nothing in flight on it can still land,
/// so a caller that answers after this answers after the last write.
///
/// `kept` are the attachments that stay: a shared namespace or a ublk
/// device one of them still uses is left alone, as is a host subsystem
/// namespace for a host one of them names. `nsid` is the shared
/// namespace's number, already taken out of the record by the caller (who
/// holds the `/v1` state).
async fn revoke_attachment(
    state: &AppState,
    id: &str,
    local: Option<Uuid>,
    att: &Attachment,
    kept: &[Attachment],
    shared_nsid: Option<u32>,
) {
    let same = |k: &Attachment| k.transport == att.transport && k.host_nqn == att.host_nqn;
    if kept.iter().any(same) {
        return;
    }
    match (att.transport.as_str(), att.host_nqn.as_deref()) {
        ("ublk", _) => {
            state.ublk_exports.lock().await.remove(id);
        }
        #[cfg(feature = "nvmeof")]
        ("nvme_tcp", Some(host)) => {
            if let Some(local) = local {
                crate::mgmt::nvme_hosts::detach(state, local, Some(host), crate::mgmt::nvme_hosts::Release::All)
                    .await;
            }
        }
        #[cfg(feature = "nvmeof")]
        ("nvme_tcp", None) => {
            if let Some(nsid) = shared_nsid {
                withdraw_shared_namespace(state, id, nsid).await;
            }
        }
        _ => {}
    }
    let _ = (local, shared_nsid);
    tracing::info!(
        "volume {id}: attachment of {} ({}{}) at epoch {} revoked",
        att.node,
        att.transport,
        att.host_nqn.as_deref().map(|h| format!(" for {h}")).unwrap_or_default(),
        att.epoch
    );
}

async fn attach_volume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<AttachRequest>,
) -> V1Result<AttachInfo> {
    let node = req.node.clone();
    let asked = req.epoch;
    let epoch = {
        let v1 = state.v1.lock().await;
        let rec = v1.volumes.get(&id).ok_or_else(|| V1Error::NotFound(format!("volume {id}")))?;
        check_attach_epoch(rec.vol.epoch, asked)?
    };
    let Json(info) = attach_data_path(state.clone(), id.clone(), req).await?;
    let att = attachment_of(&node, epoch, &info);

    // A fence may have run while the data path was being set up: then this
    // attachment is below the volume's epoch, the fence did not see it, and
    // it is taken away here instead of answered.
    let mut v1 = state.v1.lock().await;
    let Some(rec) = v1.volumes.get_mut(&id) else {
        return Err(V1Error::NotFound(format!("volume {id}")));
    };
    if rec.vol.epoch != epoch {
        let current = rec.vol.epoch;
        let local = rec.local_id;
        let kept = rec.vol.attachments.clone();
        let shared = if att.transport == "nvme_tcp" && att.host_nqn.is_none()
            && !kept.iter().any(|k| k.transport == "nvme_tcp" && k.host_nqn.is_none())
        {
            v1.nvme_nsids.remove(&id)
        } else {
            None
        };
        if let Some(nodes) = v1.attachments.get_mut(&id) {
            if !kept.iter().any(|k| k.node == node) {
                nodes.retain(|n| n != &node);
            }
        }
        v1.save();
        revoke_attachment(&state, &id, local, &att, &kept, shared).await;
        return Err(V1Error::StaleEpoch(current));
    }
    rec.vol.attachments.retain(|a| !(a.node == att.node && a.host_nqn == att.host_nqn));
    rec.vol.attachments.push(att);
    v1.save();
    Ok(Json(info))
}

async fn attach_data_path(state: Arc<AppState>, id: String, req: AttachRequest) -> V1Result<AttachInfo> {
    let want = WantTransport::parse(req.transport.as_deref()).map_err(V1Error::BadRequest)?;
    let mut v1 = state.v1.lock().await;
    expire_windows(&state, &mut v1).await;
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

    // A transport that was asked for and cannot be given is refused *before*
    // the attachment is recorded, so a refusal leaves nothing behind.
    let ublk_possible = should_offer_ublk(
        state.config.management.ublk_transport,
        &req.node,
        &local_node,
        local_id.is_some(),
    );
    match want {
        WantTransport::Ublk if !ublk_possible => {
            return Err(V1Error::Conflict(format!(
                "ublk is a local device: this node is {local_node:?}, the attach is for {:?}, \
                 the volume is not backed here, or ublk_transport is off",
                req.node
            )));
        }
        WantTransport::NvmeTcp => {
            if let Some(why) = nvme_unavailable(&state, &id, local_id, &local_node).await {
                return Err(V1Error::Conflict(why));
            }
        }
        _ => {}
    }

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
    // `nvme_tcp` skips it: the caller has said the I/O comes over the
    // network (#149).
    if want != WantTransport::NvmeTcp && ublk_possible {
        if let Some(local) = local_id {
            let device = state.volume_manager.lock().await.get_volume(&EngineVolumeId(local));
            if let Some(device) = device {
                if let Some(path) = crate::mgmt::ublk_export::attach(&state, &id, device).await {
                    return Ok(Json(AttachInfo::Ublk { device_hint: path }));
                }
            }
        }
        if want == WantTransport::Ublk {
            forget_attachment(&state, &id, &req.node).await;
            return Err(V1Error::Conflict(
                "ublk was asked for but is not available on this node (kernel ublk_drv not loaded)".into(),
            ));
        }
    }
    // NVMe-oF path: hot-add the volume as a namespace on the shared
    // subsystem. A node that is already connected picks it up from the async
    // event with no Connect at all.
    #[cfg(feature = "nvmeof")]
    {
        let Some(local) = local_id else {
            if want == WantTransport::NvmeTcp {
                forget_attachment(&state, &id, &req.node).await;
                return Err(V1Error::Conflict(format!("volume {id} could not be added as an NVMe-oF namespace")));
            }
            return Ok(Json(attach_info_for(&state, None)));
        };
        match nvme_attach(&state, &id, local, req.host_nqn.as_deref(), req.dhchap, false, None).await {
            Ok(info) => Ok(Json(info)),
            Err(NvmeAttachError::BadRequest(m)) => {
                forget_attachment(&state, &id, &req.node).await;
                Err(V1Error::BadRequest(m))
            }
            Err(NvmeAttachError::Conflict(m)) => {
                forget_attachment(&state, &id, &req.node).await;
                Err(V1Error::Conflict(m))
            }
        }
    }
    #[cfg(not(feature = "nvmeof"))]
    {
        let _ = local_id;
        if want == WantTransport::NvmeTcp {
            forget_attachment(&state, &id, &req.node).await;
            return Err(V1Error::Conflict(format!("volume {id} could not be added as an NVMe-oF namespace")));
        }
        Ok(Json(attach_info_for(&state, None)))
    }
}

/// Why an NVMe/TCP attach was refused.
#[derive(Debug)]
pub(crate) enum NvmeAttachError {
    /// The request must change: name the host.
    BadRequest(String),
    /// The node cannot serve it.
    Conflict(String),
}

/// Serve an engine volume over NVMe/TCP and say where (#210).
///
/// With `host_nqn` the volume goes into that host's own subsystem, which
/// admits that host alone (and, with `dhchap` or the node's
/// `require_dhchap`, only once it proves its secret). Without one it goes
/// on the shared subsystem — refused for a sealed golden, which is never
/// shown to every host the shared subsystem admits, and refused when the
/// shared subsystem admits no host at all, since the coordinates would lead
/// nowhere. A sealed volume is always served write-protected.
#[cfg(feature = "nvmeof")]
pub(crate) async fn nvme_attach(
    state: &AppState,
    key: &str,
    local: Uuid,
    host_nqn: Option<&str>,
    dhchap: bool,
    read_only: bool,
    holder: Option<&str>,
) -> Result<AttachInfo, NvmeAttachError> {
    // No listener: nothing is served, and the answer says so by carrying no
    // NSID — what every caller got before #210.
    let Some(target) = state.nvmeof_target.read().await.as_ref().cloned() else {
        return Ok(attach_info_for(state, None));
    };
    let sealed = state.volume_manager.lock().await.is_sealed(&EngineVolumeId(local));
    let read_only = read_only || sealed;
    if let Some(host) = host_nqn.filter(|h| !h.trim().is_empty()) {
        let a = crate::mgmt::nvme_hosts::attach_for_host(state, local, host, read_only, dhchap, holder)
            .await
            .map_err(NvmeAttachError::BadRequest)?;
        return Ok(AttachInfo::NvmeTcp {
            nqn: a.nqn,
            addresses: nvme_addresses(state, Some(target.advertised().port())),
            nsid: Some(a.nsid),
            host_nqn: Some(a.host_nqn),
            dhchap_secret: a.dhchap_secret,
        });
    }
    if sealed {
        return Err(NvmeAttachError::BadRequest(format!(
            "volume {local} is sealed: a golden is never put on the shared NVMe subsystem, where              every host it admits would see it. Name the host that reads it (host_nqn); it is              served to that host alone, write-protected (#210)"
        )));
    }
    let pol = crate::mgmt::nvme_hosts::policy(state);
    if !pol.shared_reachable() {
        return Err(NvmeAttachError::BadRequest(format!(
            "this node's shared NVMe subsystem ({}) admits no host: name the host that will              connect (host_nqn) and the volume is served from a subsystem of its own (#210)",
            target.default_subsystem().nqn()
        )));
    }
    let nsid = ensure_nvme_namespace_ro(state, key, Some(local), read_only).await.ok_or_else(|| {
        NvmeAttachError::Conflict(format!("volume {key} could not be added as an NVMe-oF namespace"))
    })?;
    Ok(attach_info_for(state, Some(nsid)))
}

/// Why a volume cannot be served over NVMe-oF/TCP from here, if it cannot:
/// the coordinates an `nvme_tcp` attach returns must be ones an initiator
/// can connect to and find the volume behind (#149).
async fn nvme_unavailable(state: &AppState, id: &str, local_id: Option<Uuid>, local_node: &str) -> Option<String> {
    #[cfg(not(feature = "nvmeof"))]
    {
        let _ = (state, id, local_id, local_node);
        return Some("this engine was built without NVMe-oF".into());
    }
    #[cfg(feature = "nvmeof")]
    {
        if state.nvmeof_target.read().await.is_none() {
            return Some(format!(
                "{local_node} serves no NVMe-oF target, so there are no nvme_tcp coordinates to give"
            ));
        }
        match local_id {
            None => Some(format!(
                "volume {id} is not backed on {local_node}: attach it on the node that holds it"
            )),
            Some(l) if state.volume_manager.lock().await.get_volume(&EngineVolumeId(l)).is_none() => {
                Some(format!("volume {id}'s backing volume {l} is gone from {local_node}"))
            }
            Some(_) => None,
        }
    }
}

/// Undo the attachment record of an attach that was refused after it.
async fn forget_attachment(state: &AppState, id: &str, node: &str) {
    let mut v1 = state.v1.lock().await;
    if let Some(nodes) = v1.attachments.get_mut(id) {
        nodes.retain(|n| n != node);
        v1.save();
    }
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
    expire_windows(&state, &mut v1).await;
    let local_node = v1.local_node.clone();
    if let Some(nodes) = v1.attachments.get_mut(&id) {
        nodes.retain(|n| n != &req.node);
    }
    if let Some(rec) = v1.volumes.get_mut(&id) {
        rec.vol.attachments.retain(|a| a.node != req.node);
    }
    v1.save();
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
        let local = state.v1.lock().await.volumes.get(&id).and_then(|r| r.local_id);
        if let Some(local) = local {
            crate::mgmt::nvme_hosts::detach(&state, local, None, crate::mgmt::nvme_hosts::Release::All).await;
        }
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
    rec.vol.attachments.clear();
    let vol = rec.vol.clone();
    v1.attachments.remove(id);
    v1.save();
    Ok(vol)
}

#[derive(Deserialize)]
struct FenceRequest {
    expected_epoch: Epoch,
}

/// Take away every attachment of `id` made below its epoch, except those of
/// `spare` (a dual-attach commit's target, which becomes the master), while
/// the caller holds the `/v1` state — so an attach at the new epoch cannot
/// be recorded, and then revoked, in between (#83, #6). Answers how many.
async fn revoke_below_epoch(
    state: &AppState,
    v1: &mut V1State,
    id: &str,
    spare: Option<&str>,
) -> usize {
    let Some(rec) = v1.volumes.get_mut(id) else { return 0 };
    let epoch = rec.vol.epoch;
    let local = rec.local_id;
    let (stale, kept): (Vec<Attachment>, Vec<Attachment>) = rec
        .vol
        .attachments
        .drain(..)
        .partition(|a| a.epoch < epoch && Some(a.node.as_str()) != spare);
    rec.vol.attachments = kept.clone();
    if stale.is_empty() {
        return 0;
    }
    let shared_stale = stale.iter().any(|a| a.transport == "nvme_tcp" && a.host_nqn.is_none());
    let shared_kept = kept.iter().any(|a| a.transport == "nvme_tcp" && a.host_nqn.is_none());
    let shared = if shared_stale && !shared_kept { v1.nvme_nsids.remove(id) } else { None };
    if let Some(nodes) = v1.attachments.get_mut(id) {
        nodes.retain(|n| kept.iter().any(|k| &k.node == n) || !stale.iter().any(|s| &s.node == n));
    }
    v1.save();
    for att in &stale {
        revoke_attachment(state, id, local, att, &kept, shared).await;
    }
    stale.len()
}

async fn fence_volume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<FenceRequest>,
) -> V1Result<serde_json::Value> {
    let mut v1 = state.v1.lock().await;
    expire_windows(&state, &mut v1).await;
    let epoch = apply_fence(&mut v1, &id, req.expected_epoch)?;
    // Before answering: once the fence has answered, a host attached below
    // it can no longer write (#83, #6).
    let revoked = revoke_below_epoch(&state, &mut v1, &id, None).await;
    Ok(Json(serde_json::json!({ "epoch": epoch, "revoked": revoked })))
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
    expire_windows(&state, &mut v1).await;
    // A promote drops every attachment record (the pair is re-made around
    // the new master): the data path behind each goes with it (#195), or a
    // ublk device or namespace outlives its record, and a later detach
    // finds nothing to tear down.
    let before = v1.volumes.get(&id).map(|r| (r.local_id, r.vol.attachments.clone()));
    let vol = apply_promote(&mut v1, &id, &req.target_node, req.fenced_epoch)?;
    if let Some((local, gone)) = before.filter(|(_, g)| !g.is_empty()) {
        let shared = if gone.iter().any(|a| a.transport == "nvme_tcp" && a.host_nqn.is_none()) {
            v1.nvme_nsids.remove(&id)
        } else {
            None
        };
        v1.save();
        for att in &gone {
            revoke_attachment(&state, &id, local, att, &[], shared).await;
        }
    }
    Ok(Json(vol))
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
    expire_windows(&state, &mut v1).await;
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
    expire_windows(&state, &mut v1).await;
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
            // The migration target's access ends with the window (#83).
            drop_node_attachments(&state, &mut v1, &id, &target).await;
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
            // Cutover: fence the old master — its attachments revoked before
            // anything answers — and promote the migration target, whose
            // attachment stays.
            let fenced = apply_fence(&mut v1, &id, req.epoch)?;
            revoke_below_epoch(&state, &mut v1, &id, Some(target.as_str())).await;
            let kept = v1.volumes.get(&id).map(|r| r.vol.attachments.clone()).unwrap_or_default();
            let mut vol = apply_promote(&mut v1, &id, &target, fenced)?;
            // The target's attachment is now the master's, at the new epoch.
            if let Some(rec) = v1.volumes.get_mut(&id) {
                rec.vol.attachments = kept
                    .into_iter()
                    .map(|a| Attachment { epoch: fenced, ..a })
                    .collect();
                vol = rec.vol.clone();
                let nodes: Vec<String> = rec.vol.attachments.iter().map(|a| a.node.clone()).collect();
                if !nodes.is_empty() {
                    v1.attachments.insert(id.clone(), nodes);
                }
                v1.save();
            }
            Ok(Json(vol))
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshots (#3) + group snapshots (#8)
// ---------------------------------------------------------------------------

/// What a snapshot names as its source: a `/v1` volume, or else an engine
/// volume by id or name (#130). A VM's disks are engine volumes made through
/// `/api/v1` — a clone of a golden, a cidata seed — and a VM snapshot has to
/// be able to name them. The size and, when this node holds the data, the
/// engine id to snapshot.
async fn resolve_snapshot_source(
    state: &AppState,
    v1: &V1State,
    id: &str,
) -> Result<(Option<Uuid>, u64), V1Error> {
    if let Some(rec) = v1.volumes.get(id) {
        return Ok((rec.local_id, rec.vol.size_bytes));
    }
    let vm = state.volume_manager.lock().await;
    let vid = vm
        .find_volume(id)
        .await
        .ok_or_else(|| V1Error::NotFound(format!("volume {id}")))?;
    let size = vm.get_volume(&vid).map(|h| h.capacity_bytes()).unwrap_or(0);
    Ok((Some(vid.0), size))
}

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
    let (source_local, size) = resolve_snapshot_source(&state, &v1, &req.volume_id).await?;

    // COW clone through GEM when the volume is backed on this node — and
    // sealed, because a snapshot *is* a golden (#111): the point-in-time copy
    // a restore clones from, which nothing may write. Left unsealed it was an
    // ordinary engine volume, and anything that attached it read-write could
    // change what the VolumeSnapshot holds.
    let local_id = match source_local {
        Some(src) => {
            let mut vm = state.volume_manager.lock().await;
            match vm.create_snapshot(EngineVolumeId(src), &req.name).await {
                Ok(id) => {
                    if let Err(e) = vm.seal_volume(id, None).await {
                        let _ = vm.delete_volume(id).await;
                        return Err(V1Error::Internal(format!("sealing snapshot: {e}")));
                    }
                    Some(id.0)
                }
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
        // Only what this node holds (#130): a volume mastered elsewhere has
        // nothing here to copy, and `ready` must not say otherwise.
        ready: local_id.is_some(),
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
    // Every member resolved before anything is taken: a `/v1` volume, or an
    // engine volume by id or name (#130).
    let mut members: Vec<(Option<Uuid>, u64)> = Vec::with_capacity(req.volume_ids.len());
    for id in &req.volume_ids {
        members.push(resolve_snapshot_source(&state, &v1, id).await?);
    }

    // Engine fence: every locally-backed member is cloned under one held
    // GEM+registry lock — a single consistency point across extent maps. A
    // snapshot, not a clone: no identity is restamped, so a VM's disks come
    // back as exactly the disks the guest had.
    let locally_backed: Vec<(EngineVolumeId, String)> = req
        .volume_ids
        .iter()
        .zip(&members)
        .filter_map(|(vid, (local, _))| local.map(|l| (EngineVolumeId(l), format!("{}-{vid}", req.name))))
        .collect();
    let mut local_snaps: HashMap<String, Uuid> = HashMap::new();
    if !locally_backed.is_empty() {
        let mut vm = state.volume_manager.lock().await;
        match vm.create_snapshots_atomic(&locally_backed).await {
            Ok(ids) => {
                for ((_, name), snap_id) in locally_backed.iter().zip(ids) {
                    // Sealed after the fence, not inside it: the fence is
                    // what makes the members one point in time, and sealing
                    // changes no extent (#111).
                    if let Err(e) = vm.seal_volume(snap_id, None).await {
                        return Err(V1Error::Internal(format!("sealing group member: {e}")));
                    }
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
    for (vid, (_, size)) in req.volume_ids.iter().zip(&members) {
        let name = format!("{}-{vid}", req.name);
        let snap = Snapshot {
            id: gen_id("snap"),
            name: name.clone(),
            source_volume_id: vid.clone(),
            size_bytes: *size,
            ready: local_snaps.contains_key(&name),
            created_at_ms: created,
            group_snapshot_id: Some(group_id.clone()),
        };
        v1.snapshots.insert(
            snap.id.clone(),
            SnapshotRec { snap: snap.clone(), local_id: local_snaps.get(&name).copied() },
        );
        snapshots.push(snap);
    }
    let ready = snapshots.iter().all(|s| s.ready);
    let group = GroupSnapshot {
        id: group_id,
        name: req.name,
        snapshots,
        ready,
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

// The bearer check that used to live here now covers the whole engine
// (`mgmt::auth::require_token`, #107) — this surface was the only one that had
// it, which is precisely what made `/api/v1` beside it look guarded. It still
// answers 401 in this contract's envelope: the layer picks the body by prefix.

pub fn router(state: Arc<AppState>) -> Router {
    // Dual-attach windows expire on time (#195); a second router over the same
    // state runs a second loop, which finds nothing left to do.
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        rt.spawn(expiry_loop(Arc::downgrade(&state)));
    }
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
                attachments: Vec::new(),
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

/// #149, as stormstorage meets it: an orchestrator on the master's own node
/// attaches a leg for a RAID head somewhere else. With ublk working (faked
/// here: dev has no ublk_drv) the engine answers `ublk` unless the request
/// names `nvme_tcp` — and with it, the coordinates are real: the engine's own
/// `nvme-tcp://` initiator connects to them and finds the volume.
#[cfg(all(test, feature = "nvmeof"))]
mod transport_tests {
    use super::*;
    use crate::drive::filedev::FileDevice;
    use crate::mgmt::config::{NvmeofExportConfig, StormBlockConfig};
    use crate::raid::RaidArrayId;
    use crate::target::nvmeof::{NvmeofConfig, NvmeofTarget};
    use crate::target::reactor::{ReactorConfig, ReactorPool};
    use crate::volume::VolumeManager;

    const NQN: &str = "nqn.2024.io.stormblock:leg-test";
    const MIB: u64 = 1 << 20;

    async fn node(dir: &std::path::Path) -> (Arc<AppState>, std::net::SocketAddr) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut config = StormBlockConfig::default();
        config.management.data_dir = Some(dir.to_string_lossy().to_string());
        config.management.node_name = Some("sno".into());
        config.nvmeof = Some(NvmeofExportConfig {
            listen_addr: addr.to_string(),
            nqn: NQN.into(),
            export_drives: false,
            // This test is about the shared subsystem; a node opens it on
            // purpose (#210).
            allow_any_host: true,
            allowed_hosts: Vec::new(),
            require_dhchap: false,
            boothost_host_nqn: None,
        });
        let mut vm = VolumeManager::new(MIB);
        let dev = FileDevice::open_with_capacity(dir.join("pool.bin").to_str().unwrap(), 128 * MIB).await.unwrap();
        vm.add_backing_device(RaidArrayId(Uuid::new_v4()), Arc::new(dev)).await;
        let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
        let state = Arc::new(AppState::new(config, vm, reg, gem));
        let target = Arc::new(NvmeofTarget::new(NvmeofConfig { listen_addr: addr, nqn: NQN.into(), ..Default::default() }));
        *state.nvmeof_target.write().await = Some(target.clone());
        tokio::spawn(async move {
            let reactor = ReactorPool::new(&ReactorConfig { core_count: 1, pin_cores: false });
            let _ = target.run_with_listener(listener, &reactor).await;
        });
        (state, addr)
    }

    async fn create(state: &Arc<AppState>, name: &str) -> Volume {
        let req: CreateVolumeRequest = serde_json::from_value(serde_json::json!({
            "name": name, "size_bytes": 16 * MIB, "replica_tier": { "slaves": 0 }
        }))
        .unwrap();
        match create_volume(State(state.clone()), Json(req)).await {
            Ok(Json(v)) => v,
            Err(e) => panic!("create: {:?}", e.into_response().status()),
        }
    }

    async fn attach(state: &Arc<AppState>, id: &str, transport: Option<&str>) -> Result<AttachInfo, u16> {
        let node = state.v1.lock().await.local_node.clone();
        let req = AttachRequest { node, mode: AttachMode::ReadWrite, transport: transport.map(String::from), host_nqn: None, dhchap: false, epoch: None };
        match attach_volume(State(state.clone()), Path(id.to_string()), Json(req)).await {
            Ok(Json(info)) => Ok(info),
            Err(e) => Err(e.into_response().status().as_u16()),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nvme_tcp_is_given_even_where_ublk_would_be() {
        let dir = tempfile::tempdir().unwrap();
        let (state, addr) = node(dir.path()).await;
        let leg = create(&state, "leg-0").await;
        // Where ublk works, the engine's own choice for the master's node is
        // a local device — what stormstorage got back and could not use.
        state.ublk_exports.lock().await.insert_fake(&leg.id, "/dev/ublkb9");
        assert_eq!(
            attach(&state, &leg.id, None).await,
            Ok(AttachInfo::Ublk { device_hint: "/dev/ublkb9".into() })
        );

        // Naming the network gets the network.
        let info = attach(&state, &leg.id, Some("nvme_tcp")).await.unwrap();
        let AttachInfo::NvmeTcp { nqn, addresses, nsid, .. } = info else { panic!("{info:?}") };
        assert_eq!(nqn, NQN);
        assert_eq!((addresses[0].traddr.as_str(), addresses[0].trsvcid), ("127.0.0.1", addr.port()));
        let nsid = nsid.expect("a namespace to connect to");
        // The spellings an orchestrator might use all mean the same thing,
        // and a repeat is the same namespace.
        for t in ["nvme-tcp", "nvmeof"] {
            let again = attach(&state, &leg.id, Some(t)).await.unwrap();
            assert!(matches!(again, AttachInfo::NvmeTcp { nsid: Some(n), .. } if n == nsid), "{t}: {again:?}");
        }

        // And the coordinates are real: a RAID head opens them as a drive.
        let uri = format!("nvme-tcp://{}:{}/{nqn}?nsid={nsid}", addresses[0].traddr, addresses[0].trsvcid);
        let remote = crate::drive::open_one_drive(&uri).await.expect("the head connects");
        let pattern: Vec<u8> = (0..MIB as usize).map(|i| (i % 253) as u8).collect();
        remote.write(0, &pattern).await.unwrap();
        remote.flush().await.unwrap();
        let local = state.v1.lock().await.volumes.get(&leg.id).unwrap().local_id.unwrap();
        let backing = state.volume_manager.lock().await.get_volume(&EngineVolumeId(local)).unwrap();
        let mut back = vec![0u8; MIB as usize];
        backing.read(0, &mut back).await.unwrap();
        assert_eq!(back, pattern, "written over nvme_tcp, read from the engine volume");

        // `ublk` insists, and an unknown transport is a bad request.
        assert!(matches!(attach(&state, &leg.id, Some("ublk")).await, Ok(AttachInfo::Ublk { .. })));
        assert_eq!(attach(&state, &leg.id, Some("iscsi")).await, Err(400));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nvme_tcp_with_no_target_says_so_and_records_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = node(dir.path()).await;
        *state.nvmeof_target.write().await = None;
        let v = create(&state, "no-target").await;
        assert_eq!(attach(&state, &v.id, Some("nvme_tcp")).await, Err(409));
        assert!(
            state.v1.lock().await.attachments.get(&v.id).map(|n| n.is_empty()).unwrap_or(true),
            "a refused attach leaves no attachment behind"
        );
        // `ublk` where there is none (dev has no ublk_drv): refused too.
        if !state.ublk_exports.lock().await.available() {
            assert_eq!(attach(&state, &v.id, Some("ublk")).await, Err(409));
            assert!(state.v1.lock().await.attachments.get(&v.id).map(|n| n.is_empty()).unwrap_or(true));
        }
    }
}
