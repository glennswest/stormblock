//! Shared runtime context — everything the API handlers and the reconciler
//! both need, in one place with a documented lock order.
//!
//! Lock order, always: `wiring` → `portals` → engine locks
//! (`state.exports`, `state.volume_manager`). Snapshot-and-release is
//! preferred over holding two at once; the reconciler clones the export table
//! rather than reading it under the wiring lock.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::mgmt::{AppState, ExportEntry};
use crate::target::iscsi::IscsiTarget;
use crate::target::nvmeof::NvmeofTarget;
use crate::target::reactor::ReactorPool;

use super::config::ServeConfig;
use super::status::MkStatus;
use super::wiring::{write_atomic, WireProto, WiringTable, Wiring};

/// A running per-export iSCSI target: its own IQN on its own port, with the
/// volume at LUN 0 (issue #2 — the RouterOS initiator cannot select a LUN).
pub struct Portal {
    pub target: Arc<IscsiTarget>,
    pub task: JoinHandle<()>,
    pub port: u16,
    pub iqn: String,
}

/// A running per-export NVMe-oF subsystem: its own NQN on its own port, with
/// the volume as namespace 1. The NVMe counterpart of `Portal` — one
/// subsystem per volume, so `nvme connect` reaches exactly one volume instead
/// of discovering every namespace on a shared subsystem.
pub struct Subsystem {
    pub target: Arc<NvmeofTarget>,
    pub task: JoinHandle<()>,
    pub port: u16,
    pub nqn: String,
}

pub struct ServeContext {
    pub cfg: ServeConfig,
    pub state: Arc<AppState>,
    pub status: Arc<MkStatus>,
    /// The shared multi-LUN iSCSI target — `None` unless
    /// `STORMBLOCKMK_ENABLE_ISCSI` is set, because the legacy stack is not
    /// brought up at all by default.
    pub shared_iscsi: Option<Arc<IscsiTarget>>,
    pub wiring: Mutex<WiringTable>,
    pub portals: Mutex<HashMap<Uuid, Portal>>,
    pub subsystems: Mutex<HashMap<Uuid, Subsystem>>,
    /// The shared reactor pool (sized from cores, issue #8). Per-export
    /// targets run on it too — building a fresh single-core pool per portal
    /// would reintroduce exactly the bottleneck #8 removed.
    pub reactor: Arc<ReactorPool>,
    /// When each draining export's grace period runs out. In-memory only: a
    /// restart legitimately restarts the clock.
    pub drain_deadlines: Mutex<HashMap<Uuid, Instant>>,
    /// Exports already reported as unwireable because their transport is off.
    /// The reconciler runs every 2s; without this the same row would log the
    /// same warning 30 times a minute for the life of the process.
    pub blocked_reported: Mutex<HashSet<Uuid>>,
    /// When each template volume was FIRST seen with no store entry naming it.
    ///
    /// This is what turns "looks like debris right now" into "has been debris
    /// since before the last pass", which is the difference between reaping a
    /// leak and deleting a template someone is formatting: `create` makes the
    /// raw volume before it writes the store entry. In-memory on purpose — a
    /// restart legitimately restarts the clock, and erring towards keeping a
    /// volume costs disk, while erring the other way costs a filesystem.
    pub orphan_first_seen: Mutex<HashMap<Uuid, Instant>>,
    /// When a pending export was first seen naming a volume that does not
    /// exist. Keyed by export id. A row is only withdrawn once it has been in
    /// that state for `orphan_export_grace_secs`, so the normal window between
    /// recording a row and creating its volume is not mistaken for an orphan
    /// (#15).
    pub export_orphan_first_seen: Mutex<HashMap<Uuid, Instant>>,
    pub exports_path: PathBuf,
}

impl ServeContext {
    pub fn new(
        cfg: ServeConfig,
        state: Arc<AppState>,
        status: Arc<MkStatus>,
        shared_iscsi: Option<Arc<IscsiTarget>>,
        reactor: Arc<ReactorPool>,
        wiring: WiringTable,
    ) -> Self {
        let exports_path = PathBuf::from(&cfg.data_dir).join("exports.json");
        ServeContext {
            cfg,
            state,
            status,
            shared_iscsi,
            wiring: Mutex::new(wiring),
            portals: Mutex::new(HashMap::new()),
            subsystems: Mutex::new(HashMap::new()),
            reactor,
            drain_deadlines: Mutex::new(HashMap::new()),
            blocked_reported: Mutex::new(HashSet::new()),
            orphan_first_seen: Mutex::new(HashMap::new()),
            export_orphan_first_seen: Mutex::new(HashMap::new()),
            exports_path,
        }
    }

    /// The IP the per-export targets bind to — same interface as the shared
    /// NVMe-oF portal, so one firewall rule covers the range. Falls back to
    /// the iSCSI bind (then the wildcard) only if that is unparseable.
    pub fn portal_bind_ip(&self) -> IpAddr {
        self.cfg
            .nvmeof_bind
            .parse::<SocketAddr>()
            .or_else(|_| self.cfg.iscsi_bind.parse::<SocketAddr>())
            .map(|s| s.ip())
            .unwrap_or(IpAddr::from([0, 0, 0, 0]))
    }

    /// Is this row's transport actually being served? An iSCSI row while the
    /// legacy stack is off can never be wired — it is blocked, not pending,
    /// and saying so is the difference between "mk is still working on it"
    /// and "nothing will ever happen here".
    pub fn is_blocked(&self, w: &Wiring) -> bool {
        w.protocol == WireProto::Iscsi && !self.cfg.iscsi_enabled
    }

    /// Withdraw every export entry naming `volume_id`, returning their ids.
    ///
    /// This is the step that keeps a wiring row from outliving its volume
    /// (#15). Wiring rows are reconciled against the engine's export table:
    /// a row whose entry is gone drains and is dropped, a row whose entry
    /// remains is retried forever. So deleting a volume without withdrawing
    /// its entry strands the row permanently — `Pending`, un-wireable, and
    /// holding readiness down for good, because nothing in a running mk ever
    /// removes it.
    ///
    /// Withdrawing the entry rather than deleting the wiring row directly is
    /// deliberate: it hands the row to the ordered-teardown path that already
    /// exists, so a consumer still attached is drained rather than having the
    /// LUN pulled out from under it.
    pub async fn withdraw_exports_for_volume(&self, volume_id: Uuid) -> Vec<Uuid> {
        let mut ex = self.state.exports.write().await;
        let withdrawn: Vec<Uuid> =
            ex.iter().filter(|e| e.volume_id == volume_id).map(|e| e.id).collect();
        if !withdrawn.is_empty() {
            ex.retain(|e| e.volume_id != volume_id);
        }
        drop(ex);
        withdrawn
    }

    /// Persist the engine's export table. The engine keeps it in memory only;
    /// mk owns durability for it, atomically.
    pub async fn persist_exports(&self) -> anyhow::Result<()> {
        let entries: Vec<ExportEntry> = self.state.exports.read().await.clone();
        let json = serde_json::to_string_pretty(&entries)?;
        write_atomic(&self.exports_path, json.as_bytes())
    }

    /// Exactly what a consumer needs to attach — the deliverable of issue #2.
    ///
    /// For NVMe (the default transport) that is `address`/`port`/`nqn`/`nsid`:
    /// the volume is namespace 1 of its own subsystem. For a legacy iSCSI row,
    /// `portal`/`iqn`/`lun` describe the dedicated single-volume target, and
    /// `shared_*` the same volume on the multi-LUN target — reported only when
    /// that target is actually being served.
    pub fn attach_params(&self, w: &Wiring) -> serde_json::Value {
        if w.protocol == WireProto::Nvmeof {
            let nqn = w.nqn.clone().unwrap_or_default();
            // No `shared_nqn` here on purpose: the shared subsystem carries no
            // volumes, so pointing a consumer at it would be pointing it at
            // nothing (and, before per-volume NQNs, at everyone else's disks).
            return serde_json::json!({
                "transport": "nvme-tcp",
                "address": self.cfg.advertise_addr,
                "port": w.portal_port,
                "nqn": nqn,
                "nsid": 1,
                "nvme": format!(
                    "nvme connect -t tcp -a {} -s {} -n {}",
                    self.cfg.advertise_addr, w.portal_port, nqn
                ),
                "routeros": format!(
                    "/disk add type=nvme-tcp nvme-tcp-address={} nvme-tcp-port={} nvme-tcp-nqn={}",
                    self.cfg.advertise_addr, w.portal_port, nqn
                ),
            });
        }
        let shared_port = self
            .cfg
            .iscsi_bind
            .parse::<SocketAddr>()
            .map(|s| s.port())
            .unwrap_or(3260);
        let mut v = serde_json::json!({
            "transport": "iscsi",
            "portal": format!("{}:{}", self.cfg.advertise_addr, w.portal_port),
            "address": self.cfg.advertise_addr,
            "port": w.portal_port,
            "iqn": w.iqn,
            "lun": 0,
            "routeros": format!(
                "/disk add type=iscsi iscsi-address={}:{} iscsi-iqn={}",
                self.cfg.advertise_addr, w.portal_port, w.iqn
            ),
        });
        if self.cfg.iscsi_enabled {
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "shared_portal".into(),
                    serde_json::json!(format!("{}:{}", self.cfg.advertise_addr, shared_port)),
                );
                obj.insert("shared_iqn".into(), serde_json::json!(self.cfg.iqn));
                obj.insert("shared_lun".into(), serde_json::json!(w.lun));
            }
        }
        v
    }

    pub fn wiring_json(&self, w: &Wiring) -> serde_json::Value {
        let mut v = serde_json::json!({
            "export_id": w.export_id,
            "volume_id": w.volume_id,
            "protocol": w.protocol.api_name(),
            "state": w.state.as_str(),
            "ephemeral": w.ephemeral,
            "attach": self.attach_params(w),
        });
        // Say plainly why a row is sitting there, rather than leaving a
        // consumer to poll a `pending` that will never advance.
        if self.is_blocked(w) {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("blocked".into(), serde_json::json!(true));
                obj.insert(
                    "blocked_reason".into(),
                    serde_json::json!(
                        "iSCSI is not served — set STORMBLOCKMK_ENABLE_ISCSI=1 to wire this \
                         export, or withdraw it and recreate it with protocol \"nvme-tcp\""
                    ),
                );
            }
        }
        v
    }
}
