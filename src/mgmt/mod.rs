//! Management plane — REST API (axum), Prometheus metrics, config.

pub mod api;
pub mod auth;
pub mod kubeauth;
pub mod capacity;
pub mod goldens;
pub mod boot_override;
pub mod config;
pub mod metrics;
pub mod discovery;
pub mod ublk_export;
pub mod debug;
pub mod slab_report;
pub mod tls;
pub mod raid_sets;
#[cfg(feature = "nvmeof")]
pub mod nvme_hosts;
#[cfg(feature = "nvmeof")]
pub mod ana;
#[cfg(feature = "nvmeof")]
pub mod forge;
#[cfg(feature = "nvmeof")]
pub mod forge_trust;
#[cfg(feature = "ui")]
pub mod ui;

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Serialize, Deserialize};
use tokio::net::TcpListener;
use uuid::Uuid;

use crate::drive::BlockDevice;
use crate::drive::slab_registry::SlabRegistry;
use crate::raid::{RaidArray, RaidArrayId, RaidLevel};
#[cfg(feature = "iscsi")]
use crate::target::iscsi::IscsiTarget;
use crate::volume::{VolumeManager, GlobalExtentMap};

use config::StormBlockConfig;


/// Information about an opened drive, stored in AppState.
#[derive(Clone)]
pub struct DriveInfo {
    pub device: Arc<dyn BlockDevice>,
    pub path: String,
    /// Where the drive is — failure-domain labels given at registration
    /// (#70). Empty when nobody said.
    pub labels: crate::placement::domain::FailureDomain,
    /// Opened with a DH-HMAC-CHAP secret given for it (#213): reported as
    /// `dhchap: true`. The secret itself is the device's, never kept here.
    pub dhchap: bool,
}

/// Information about a RAID array, stored in AppState.
#[derive(Clone)]
pub struct ArrayInfo {
    pub array: Arc<RaidArray>,
    pub level: RaidLevel,
    pub member_count: usize,
    pub capacity_bytes: u64,
    pub stripe_size: u64,
}

/// Protocol for an export entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportProtocol {
    Iscsi,
    Nvmeof,
}

/// This node's name, as everything that places or attaches a volume sees it.
///
/// Order: the config, then `$STORMBLOCK_NODE`, then `$HOSTNAME`, then the
/// **kernel's** hostname, then `localhost`.
///
/// The kernel's own name is the one that was missing, and its absence is not
/// cosmetic: a workload started by an init system rather than a shell has no
/// `HOSTNAME` in its environment — nothing exports it but a login shell — so a
/// stormblock started as PID 1's child called itself `localhost` on a node
/// named `storm-2c91b3`. Every local attach then failed with "ublk is a local
/// device: this node is \"localhost\", the attach is for \"storm-2c91b3\"",
/// which reads as a transport problem and is an identity problem.
pub fn local_node_name(config: &crate::mgmt::config::StormBlockConfig) -> String {
    config
        .management
        .node_name
        .clone()
        .or_else(|| std::env::var("STORMBLOCK_NODE").ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(kernel_hostname)
        .unwrap_or_else(|| "localhost".to_string())
}

/// What `uname -n` reports, read where the kernel keeps it.
///
/// `/proc/sys/kernel/hostname` rather than `gethostname(2)` so this needs no
/// libc and works the same on every platform that has procfs; a system without
/// one simply falls through to the next answer.
fn kernel_hostname() -> Option<String> {
    let name = std::fs::read_to_string("/proc/sys/kernel/hostname").ok()?;
    let name = name.trim();
    // A container that has not set one reports the empty string or
    // "localhost"; neither is an identity, and taking one would hide the
    // problem rather than fix it.
    if name.is_empty() || name == "localhost" || name == "(none)" {
        return None;
    }
    Some(name.to_string())
}

impl std::fmt::Display for ExportProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportProtocol::Iscsi => write!(f, "iscsi"),
            ExportProtocol::Nvmeof => write!(f, "nvmeof"),
        }
    }
}

/// Status of an export entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportStatus {
    Active,
    PendingRestart,
}

/// A volume-to-target export mapping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportEntry {
    pub id: Uuid,
    pub volume_id: Uuid,
    pub protocol: ExportProtocol,
    pub target_id: String,
    pub status: ExportStatus,
    /// LUN this volume was given on the iSCSI target. An initiator needs it to
    /// address the right volume once more than one is exported (#24).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lun_id: Option<u64>,
    /// Namespace ID on the NVMe-oF target, for `nvmeof` exports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nsid: Option<u32>,
    /// The host this export serves, and its own subsystem (#210). `None`:
    /// the shared subsystem.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_nqn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subsystem: Option<String>,
    /// Made by `/serve/v1` (#217): served on a portal of its own by the
    /// serving layer's reconciler, and by nothing else. Every other export
    /// (`/api/v1/exports`, the UI) is the engine's, on its own listener with
    /// its own host policy, and the reconciler never touches it. An entry
    /// written before this field is recognised by its name
    /// (`serve::wiring::serve_owned`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub serve: bool,
}

/// Backing type for a dynamically-created LUN.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum LunBacking {
    File { path: String, size: Option<String> },
    Device { path: String },
    Raid { array_id: RaidArrayId },
    /// A thin/CoW volume from the GEM, exported as a LUN (#22). This is the
    /// backing the registry model uses — one clone per consumer.
    Volume { volume_id: Uuid },
}

/// A LUN entry tracked by the management plane.
pub struct LunEntry {
    pub lun_id: u64,
    pub backing: LunBacking,
    pub readonly: bool,
    pub device: Arc<dyn BlockDevice>,
}

/// The persisted form of a LUN entry — everything needed to re-open the
/// backing device on startup. The live `Arc<dyn BlockDevice>` is rebuilt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedLun {
    pub lun_id: u64,
    pub backing: LunBacking,
    #[serde(default)]
    pub readonly: bool,
}

/// Shared application state for the management API.
/// What `serve` knows about giving a volume its own subsystem.
#[derive(Clone)]
pub struct PerVolumeServing {
    /// `nqn.2026-08.lo.storm`, to which `:vol-<uuid>` is appended. The volume
    /// GUID *is* the address (#98, #99).
    pub nqn_prefix: String,
    pub portal_base: u16,
    pub portal_span: u16,
    /// The same reactor the rest of the data path runs on — a subsystem
    /// started here must not bring its own thread pool.
    pub reactor: std::sync::Arc<crate::target::reactor::ReactorPool>,
    /// The serving layer, whose one NVMe/TCP listener every per-volume
    /// subsystem is served on (#188): a claim's subsystem goes there too,
    /// never on a listener of its own (#98, #99). Weak: the context holds
    /// this state.
    pub serve: std::sync::Weak<crate::serve::ctx::ServeContext>,
}

/// One volume served as its own subsystem, on the serve listener (#98).
pub struct VolumeSubsystem {
    pub nqn: String,
    pub port: u16,
    /// The listener's target, to withdraw the subsystem from.
    #[cfg(feature = "nvmeof")]
    pub target: Arc<crate::target::nvmeof::NvmeofTarget>,
    /// The subsystem, so who may connect can be changed after it started
    /// (#210).
    #[cfg(feature = "nvmeof")]
    pub sub: Arc<crate::target::nvmeof::Subsystem>,
}

pub struct AppState {
    pub drives: tokio::sync::RwLock<Vec<DriveInfo>>,
    pub arrays: tokio::sync::RwLock<HashMap<RaidArrayId, ArrayInfo>>,
    /// Hot spares for the RAID sets, by pool (#252).
    pub spares: Arc<crate::raid::spares::SparePool>,
    /// Behind an `Arc` so work that outlives a request can hold it.
    pub volume_manager: Arc<crate::lockwatch::TrackedMutex<VolumeManager>>,
    /// Which volumes exist, read without `volume_manager`'s lock (#358).
    pub volume_presence: crate::volume::VolumePresence,
    /// What the volume listing reads without `volume_manager`'s lock (#364).
    pub volume_catalog: crate::volume::catalog::CatalogCell,
    pub exports: tokio::sync::RwLock<Vec<ExportEntry>>,
    pub slab_registry: Arc<crate::lockwatch::TrackedRwLock<SlabRegistry>>,
    pub gem: Arc<crate::lockwatch::TrackedRwLock<GlobalExtentMap>>,
    /// Control-plane state behind the /v1 CSI contract surface.
    pub v1: tokio::sync::Mutex<api::v1::V1State>,
    /// StormFS chunk ownership, map versions and version pins (#49, #50).
    /// Taken **before** the volume manager wherever both are needed.
    #[cfg(feature = "stormfs-data")]
    pub stormfs: tokio::sync::Mutex<api::stormfs::StormFsState>,
    /// Where each volume is served as a subsystem of its own: volume id ->
    /// (NQN, port).
    ///
    /// A per-volume subsystem is self-describing — the NQN contains the volume
    /// uuid, so there is no namespace number to go stale and a deleted volume
    /// stops answering rather than resolving to a stranger. It is also the
    /// only form that can express a volume whose legs are in several places,
    /// which is what NVMe multipath is: one subsystem, several portals
    /// (#98).
    ///
    /// Written by the reconciler and by a claim (`ensure_volume_subsystem`),
    /// both on the serve listener, and read by the claim so a consumer is
    /// told the address that names its volume.
    pub nvme_portals: tokio::sync::RwLock<HashMap<uuid::Uuid, (String, u16)>>,
    /// How to serve a volume as a subsystem of its own, published by `serve`
    /// at startup because the settings live in its config and the API cannot
    /// see them.
    ///
    /// `None` when nothing is serving — the API then has no way to start a
    /// subsystem, and an unnamed claim is answered with no address (#98:
    /// the shared subsystem is never how a claim is served).
    pub per_volume: tokio::sync::RwLock<Option<PerVolumeServing>>,
    /// Subsystems this side started, kept alive. Dropping the handle stops the
    /// listener, so a volume stops answering when its entry goes.
    pub volume_subsystems: tokio::sync::Mutex<HashMap<uuid::Uuid, VolumeSubsystem>>,
    /// How long a freshly claimed clone is protected from being released by
    /// the next claim. See `api::synonyms` — a machine claims twice per boot
    /// and the first clone is still attached when the second arrives.
    ///
    /// Held here rather than read from the environment because a process-wide
    /// setting cannot be varied per instance, and two tests exercising the two
    /// sides of this would clobber each other.
    pub claim_grace: std::time::Duration,
    /// The serving runtime, when this node serves volumes (#60). Unset only
    /// when it was deliberately turned off or could not be built — the router
    /// mounts `/serve/v1` whenever it is here, so no profile has to remember
    /// to.
    ///
    /// A `OnceLock` rather than a plain field because `ServeContext` holds an
    /// `Arc<AppState>` of its own: the state has to exist before the context
    /// can be built, so the context is put back afterwards. That is a
    /// reference cycle and neither side is ever dropped — which is what a
    /// process that serves until it is killed wants anyway, but it is a cycle
    /// and worth saying so.
    pub serve: std::sync::OnceLock<Arc<crate::serve::ctx::ServeContext>>,
    /// Stable names that point at volumes, and can be re-pointed at a new
    /// version. Persisted as `<data_dir>/synonyms.json`; kept apart from the
    /// volume because a volume is extents and a synonym is a binding.
    pub synonyms: tokio::sync::RwLock<crate::volume::SynonymStore>,
    /// Preformatted filesystem templates — mkfs once, clone forever (#38).
    pub fstemplates: Arc<tokio::sync::Mutex<crate::fs::TemplateStore>>,
    /// Live per-volume ublk exports for the local CSI fast path.
    pub ublk_exports: tokio::sync::Mutex<ublk_export::UblkExportManager>,
    /// Volume moves, live and finished (#20). Kept so a move interrupted
    /// between its copy and its commit is still nameable after a restart —
    /// otherwise a crash there leaves two volumes and no record of which is
    /// which.
    pub moves: tokio::sync::RwLock<HashMap<Uuid, crate::volume::relocate::VolumeMove>>,
    /// Where persisted management state lives, when there is anywhere.
    pub data_dir: Option<std::path::PathBuf>,
    /// What a caller must present. Resolved at startup by
    /// `start_management_server` (config, environment, token file, or minted)
    /// and read on every request by `auth::require_token`.
    ///
    /// Seeded from the config alone so that a router built in-process — a
    /// test, an embedder — enforces a token the moment one is configured, and
    /// a `RwLock` rather than a `OnceLock` because startup then replaces it
    /// with the resolved answer, which may include a token no config names.
    auth: std::sync::RwLock<Arc<crate::serve::api::AuthConfig>>,
    /// Pallet name → drives it should be on (#56). Persisted as
    /// `<data_dir>/pallet_mirrors.json`; the drives carry no record of it.
    pub pallet_mirrors: tokio::sync::RwLock<HashMap<String, u8>>,
    /// Extents of this node's volumes still on a flow-over source (the
    /// appliance's slab), for `/api/v1/health` (#260). -1 when this engine
    /// runs no flow-over; 0 once it has finished. An atomic so the open probe
    /// never waits on the extent map the flow-over is holding.
    pub flow_over_remaining: Arc<std::sync::atomic::AtomicI64>,
    /// Why the flow-over stopped short, when it did (#172): the destination
    /// full, or extents that would not move. Said in health until the next
    /// boot.
    pub flow_over_stalled: Arc<std::sync::Mutex<Option<String>>>,
    /// The paths this engine opened its slabs from (`--slab`, or the
    /// handover record's): where a staged release's boot pallet goes is the
    /// one among them that carries a node layout (#122).
    pub slab_paths: tokio::sync::RwLock<Vec<String>>,
    /// The disks those slabs were opened from, as this engine holds them —
    /// on a network-booted node the claimed clone's `nvme-tcp://` namespace
    /// (#314). Not drives: nothing is placed on them through `drives`. The
    /// pallet reads list and verify the pallets on them; no pallet verb
    /// writes to them (a boot clone is shared with the appliance's lineage).
    pub boot_disks: tokio::sync::RwLock<Vec<DriveInfo>>,
    /// Where the slabs are, for health (#322): the last answer and when.
    pub slab_report: std::sync::Mutex<Option<(std::time::Instant, slab_report::SlabReport)>>,
    /// The release being staged on this node, if any (#122).
    pub stage_job: tokio::sync::Mutex<Option<crate::mgmt::api::releases::StageJob>>,
    /// Drives being emptied so they can be pulled (#70 item 3).
    pub drains: tokio::sync::RwLock<crate::drain::Drains>,
    /// Per-volume rebuilds after a drive fails (#146).
    pub rebuilds: Arc<crate::rebuild::Rebuilds>,
    /// Disk images being imported into goldens.
    pub imports: tokio::sync::RwLock<crate::image::import::Imports>,
    /// Latest pool-pressure sample, kept current by the watcher (#18).
    pub pool_pressure: Option<std::sync::Arc<tokio::sync::RwLock<Option<crate::volume::pressure::PressureStatus>>>>,
    pub config: StormBlockConfig,
    /// Node/cluster discovery. `None` when it could not be started (for
    /// example a network without multicast) — the node still serves its own
    /// volumes, it just cannot see peers.
    pub discovery: Option<Arc<discovery::Discovery>>,
    /// The background eraser (#286): overwrites freed slots before they are
    /// reused. Started by [`AppState::start_eraser`].
    pub eraser: Arc<crate::volume::erase::Eraser>,
    /// The apiserver a destructive call's Kubernetes bearer is reviewed
    /// against (#274); `None` refuses such bearers.
    pub kube_auth: Option<Arc<kubeauth::KubeAuth>>,
    /// Where destructive calls are recorded (#274).
    pub audit_log: Option<std::path::PathBuf>,
    /// Most recent extent-GC pass, when the background collector is running.
    pub last_gc: Option<Arc<tokio::sync::RwLock<Option<crate::volume::gc::GcSummary>>>>,
    #[cfg(feature = "iscsi")]
    pub iscsi_target: tokio::sync::RwLock<Option<Arc<IscsiTarget>>>,
    /// Live NVMe-oF target, so exports can add namespaces at runtime (#26).
    #[cfg(feature = "nvmeof")]
    pub nvmeof_target: tokio::sync::RwLock<Option<Arc<crate::target::nvmeof::NvmeofTarget>>>,
    /// Per-host NVMe subsystems and who may reach them (#210).
    #[cfg(feature = "nvmeof")]
    pub nvme_hosts: tokio::sync::Mutex<nvme_hosts::NvmeHosts>,
    /// The volume listing's state generation (#218): (the last fingerprint
    /// of what the listing reports that a persist does not move, how many
    /// times it has changed). The listing's `generation` is the volume
    /// manager's plus that count.
    pub listing_state: std::sync::Mutex<(u64, u64)>,
    /// The `[nvmeof]` settings in force: `--config`'s, or the forge settings
    /// this node keeps (#272). Read through [`AppState::nvmeof_settings`],
    /// never `config.nvmeof`, which is only what the file said at start.
    #[cfg(feature = "nvmeof")]
    pub nvmeof_settings: std::sync::RwLock<Option<config::NvmeofExportConfig>>,
    /// Who set up the shared target, for `/api/v1/forge` (#272).
    #[cfg(feature = "nvmeof")]
    pub forge: tokio::sync::Mutex<forge::Forge>,
    /// Live LUN table, keyed by LUN ID for O(1) lookup at thousands of
    /// LUNs (#24).
    #[cfg(feature = "iscsi")]
    pub lun_entries: tokio::sync::RwLock<HashMap<u64, LunEntry>>,
    /// Writes `luns.json` behind attaches and detaches (#134).
    #[cfg(feature = "iscsi")]
    pub luns_writer: std::sync::OnceLock<Arc<crate::mgmt::api::luns::LunsWriter>>,
    #[cfg(feature = "cluster")]
    pub cluster: Option<Arc<crate::cluster::ClusterManager>>,
}

impl AppState {
    /// Every drive this engine reports (#133): those opened as drives, then
    /// the disks the node's own slabs were opened from (`boot_disks`), which
    /// are its system disk. Each once; `true` marks a system disk.
    pub async fn listed_drives(&self) -> Vec<(DriveInfo, bool)> {
        let mut out: Vec<(DriveInfo, bool)> = self.drives.read().await.iter().map(|d| (d.clone(), false)).collect();
        for d in self.boot_disks.read().await.iter() {
            let uuid = d.device.id().uuid;
            if !out.iter().any(|(o, _)| o.path == d.path || o.device.id().uuid == uuid) {
                out.push((d.clone(), true));
            }
        }
        out
    }

    /// The path of the system disk `id` (uuid or path) names, when it is one
    /// and was not opened as a drive (#133): the drive API reads it and
    /// changes nothing on it.
    pub async fn system_disk(&self, id: &str) -> Option<String> {
        self.listed_drives()
            .await
            .into_iter()
            .find(|(d, system)| *system && (d.path == id || d.device.id().uuid.to_string() == id))
            .map(|(d, _)| d.path)
    }

    /// The `[nvmeof]` settings in force (#272).
    #[cfg(feature = "nvmeof")]
    pub fn nvmeof_settings(&self) -> Option<config::NvmeofExportConfig> {
        self.nvmeof_settings.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// This node's name in the /v1 surface and in discovery beacons.
    pub fn local_node_name(&self) -> String {
        local_node_name(&self.config)
    }

    /// What a caller must present, as of now.
    pub fn auth(&self) -> Arc<crate::serve::api::AuthConfig> {
        // A poisoned lock here would mean a panic while swapping the token at
        // startup; the token itself is still whatever was in there, and
        // failing every request over it would be a worse answer than serving
        // with the credential that is in force.
        match self.auth.read() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    /// Put the resolved credential in force. Called once, at startup, before
    /// the listener binds.
    pub fn set_auth(&self, auth: crate::serve::api::AuthConfig) {
        match self.auth.write() {
            Ok(mut g) => *g = Arc::new(auth),
            Err(p) => *p.into_inner() = Arc::new(auth),
        }
    }

    /// Secure delete (#286): put the node's erase level (`[erase] default`)
    /// on every local slab and start the eraser. The daemon and `adopt-ublk`
    /// call it; a node that never does frees as before #286, and a slot
    /// already marked waits on its slab until an engine that does opens it.
    pub async fn start_eraser(&self) {
        let level = self.config.erase.default;
        self.slab_registry.write().await.set_erase_default(level);
        let pending = self.slab_registry.read().await.erasing_slots();
        tracing::info!("secure delete: freed slots are overwritten ({level}); {pending} waiting");
        self.eraser.spawn();
    }

    /// Whether this node requires a credential — reported by `/api/v1/health`
    /// so a fleet can be asked which of its nodes are open (#107).
    pub fn auth_enforced(&self) -> bool {
        self.auth().api_token.is_some()
    }

    pub fn new(
        config: StormBlockConfig,
        volume_manager: VolumeManager,
        slab_registry: Arc<crate::lockwatch::TrackedRwLock<SlabRegistry>>,
        gem: Arc<crate::lockwatch::TrackedRwLock<GlobalExtentMap>>,
    ) -> Self {
        // NSIDs are never handed out twice, across restarts too (#96): the
        // high-water marks live in the data directory.
        #[cfg(feature = "nvmeof")]
        if let Some(dir) = config.management.data_dir.as_ref() {
            crate::target::nvmeof::nsid::load(std::path::Path::new(dir).join("nsid_high.json"));
        }
        // The node's own rungs sit under every slab's failure domain, so a
        // policy that spreads at `rack` has something to compare (#72).
        if !config.management.topology.is_empty() {
            let chain = crate::placement::domain::FailureDomain::from_labels(
                config.management.topology.iter().map(|(k, v)| (k.clone(), v.clone())),
            );
            match slab_registry.try_write() {
                Ok(mut reg) => reg.set_node_labels(chain),
                Err(_) => tracing::warn!("slab registry busy at startup; node topology labels not applied"),
            }
        }
        let holds = volume_manager.holds();
        let volume_presence = volume_manager.presence();
        // The API's manager: its persists inside a request are made after
        // the request releases it (#364).
        volume_manager.mark_shared();
        volume_manager.publish_catalog();
        let volume_catalog = volume_manager.catalog_cell();
        // Before any target serves: a volume this node was told is not to be
        // used through it must not answer optimized after a restart (#83).
        #[cfg(feature = "nvmeof")]
        ana::load(&config);
        #[cfg(feature = "nvmeof")]
        let nvmeof_settings = config.nvmeof.clone();
        let volume_manager = Arc::new(crate::lockwatch::TrackedMutex::new(volume_manager));
        let rebuilds = crate::rebuild::Rebuilds::new(volume_manager.clone(), &config.rebuild);
        let slab_registry_for_eraser = slab_registry.clone();
        AppState {
            drives: tokio::sync::RwLock::new(Vec::new()),
            arrays: tokio::sync::RwLock::new(HashMap::new()),
            spares: crate::raid::spares::SparePool::new(),
            flow_over_remaining: Arc::new(std::sync::atomic::AtomicI64::new(-1)),
            flow_over_stalled: Arc::new(std::sync::Mutex::new(None)),
            slab_paths: Default::default(),
            boot_disks: tokio::sync::RwLock::new(Vec::new()),
            slab_report: std::sync::Mutex::new(None),
            stage_job: Default::default(),
            volume_manager,
            volume_presence,
            volume_catalog,
            exports: tokio::sync::RwLock::new(Vec::new()),
            slab_registry,
            gem,
            v1: tokio::sync::Mutex::new(api::v1::V1State::from_config(&config)),
            #[cfg(feature = "stormfs-data")]
            stormfs: tokio::sync::Mutex::new(match config.management.data_dir.as_ref() {
                Some(dir) => api::stormfs::StormFsState::load(std::path::Path::new(dir)),
                None => api::stormfs::StormFsState::default(),
            }),
            nvme_portals: tokio::sync::RwLock::new(HashMap::new()),
            per_volume: tokio::sync::RwLock::new(None),
            volume_subsystems: tokio::sync::Mutex::new(HashMap::new()),
            claim_grace: std::time::Duration::from_secs(
                std::env::var("STORMBLOCK_CLAIM_GRACE_SECS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(600),
            ),
            fstemplates: Arc::new(tokio::sync::Mutex::new(
                match config.management.data_dir.as_ref() {
                    Some(dir) => crate::fs::TemplateStore::load(std::path::Path::new(dir)),
                    // No data dir means nothing survives a restart anyway; a
                    // template that outlived its store would be unreachable.
                    None => crate::fs::TemplateStore::in_memory(),
                },
            )),
            synonyms: tokio::sync::RwLock::new(match config.management.data_dir.as_ref() {
                Some(dir) => crate::volume::SynonymStore::load(std::path::Path::new(dir)),
                // A name that does not survive a restart is worse than no
                // name: something would resolve it once and never again.
                None => crate::volume::SynonymStore::in_memory(),
            }),
            ublk_exports: tokio::sync::Mutex::new(ublk_export::UblkExportManager::new().with_holds(holds)),
            moves: tokio::sync::RwLock::new(match config.management.data_dir.as_ref() {
                Some(dir) => api::moves::load(std::path::Path::new(dir)),
                None => HashMap::new(),
            }),
            data_dir: config.management.data_dir.as_ref().map(std::path::PathBuf::from),
            auth: std::sync::RwLock::new(Arc::new(crate::serve::api::AuthConfig {
                api_token: config.management.api_token.clone(),
                admin_token: config.management.admin_token.clone(),
                audit_only: false,
            })),
            pallet_mirrors: tokio::sync::RwLock::new(match config.management.data_dir.as_ref() {
                Some(dir) => api::pallets::load_mirrors(std::path::Path::new(dir)),
                None => HashMap::new(),
            }),
            drains: tokio::sync::RwLock::new(crate::drain::Drains::default()),
            rebuilds,
            imports: tokio::sync::RwLock::new(crate::image::import::Imports::default()),
            serve: std::sync::OnceLock::new(),
            pool_pressure: None,
            kube_auth: kubeauth::KubeAuth::from_config(config.management.kubernetes.as_ref()).map(Arc::new),
            audit_log: config
                .management
                .audit_log
                .clone()
                .map(std::path::PathBuf::from)
                .or_else(|| config.management.data_dir.as_ref().map(|d| std::path::Path::new(d).join("audit.log"))),
            eraser: crate::volume::erase::Eraser::new(
                slab_registry_for_eraser,
                config.management.data_dir.as_ref().map(std::path::PathBuf::from),
            ),
            config,
            discovery: None,
            last_gc: None,
            #[cfg(feature = "iscsi")]
            iscsi_target: tokio::sync::RwLock::new(None),
            #[cfg(feature = "nvmeof")]
            nvmeof_target: tokio::sync::RwLock::new(None),
            #[cfg(feature = "nvmeof")]
            nvme_hosts: tokio::sync::Mutex::new(nvme_hosts::NvmeHosts::default()),
            listing_state: std::sync::Mutex::new((0, 0)),
            #[cfg(feature = "nvmeof")]
            nvmeof_settings: std::sync::RwLock::new(nvmeof_settings),
            #[cfg(feature = "nvmeof")]
            forge: tokio::sync::Mutex::new(forge::Forge::default()),
            #[cfg(feature = "iscsi")]
            lun_entries: tokio::sync::RwLock::new(HashMap::new()),
            #[cfg(feature = "iscsi")]
            luns_writer: std::sync::OnceLock::new(),
            #[cfg(feature = "cluster")]
            cluster: None,
        }
    }
}

/// Serve `router` over TLS on `listener` (#203). Each connection takes the
/// reloader's current acceptor; a client certificate the node CA verified is
/// put on every request of that connection as [`tls::ClientCert`].
pub async fn serve_tls(
    listener: TcpListener,
    router: axum::Router,
    reloader: Arc<tls::Reloader>,
) -> anyhow::Result<()> {
    loop {
        let (tcp_stream, peer) = listener.accept().await?;
        let (acceptor, classifier) = reloader.acceptor();
        let app = router.clone();
        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(tcp_stream).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!("TLS handshake failed: {e}");
                    return;
                }
            };
            let cert = tls::client_cert(&tls_stream, &classifier);
            let io = hyper_util::rt::TokioIo::new(tls_stream);
            let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let app = app.clone();
                let cert = cert.clone();
                async move {
                    use tower::Service;
                    let mut svc = app;
                    let mut req = req.map(axum::body::Body::new);
                    // Never from the client: only what the handshake proved.
                    req.extensions_mut().remove::<tls::ClientCert>();
                    if let Some(c) = cert {
                        req.extensions_mut().insert(c);
                    }
                    req.extensions_mut().insert(axum::extract::ConnectInfo(peer));
                    Ok::<_, std::convert::Infallible>(svc.call(req).await.unwrap())
                }
            });
            let _ = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                .serve_connection(io, service)
                .await;
        });
    }
}

/// Start the management REST API server.
pub async fn start_management_server(state: Arc<AppState>) -> anyhow::Result<()> {
    // Delete the clones #55 kept standing by (#137). A claim mints inline
    // now, and a volume nobody asked for is a volume nobody can account for.
    {
        let state = state.clone();
        tokio::spawn(async move {
            let gone =
                crate::fs::template::retire_standing(&state.volume_manager, &state.fstemplates).await;
            if !gone.is_empty() {
                tracing::info!("retired {} standing clone(s) nobody had claimed", gone.len());
            }
            // And finish the formats a previous run left in the middle (#141).
            let in_use = api::volumes_in_use(&state).await;
            for (name, r) in
                crate::fs::template::resume_formats(&state.volume_manager, &state.fstemplates, &in_use).await
            {
                match r {
                    Ok(()) => tracing::info!("fstemplate {name}: finished the format a previous run left"),
                    Err(e) => tracing::warn!("fstemplate {name}: format not finished ({e}); rolled back — the next claim mints it afresh"),
                }
            }
            // And a ready template whose sealed volume is gone is not ready
            // (#281).
            for (name, why) in
                crate::fs::template::verify_ready(&state.volume_manager, &state.fstemplates).await
            {
                tracing::warn!("fstemplate {name} is broken: {why}; a clone of it is refused until it is minted again");
            }
        });
    }

    let listen_addr = &state.config.management.listen_addr;

    // The stall watchdog and its heartbeat (#269).
    debug::start(state.clone());

    // Who may call this, decided before anything can be called (#107). A
    // failure here — `require_auth` set with nowhere to keep a token — stops
    // the server starting; the one thing it must never do is fall through to
    // serving the whole API to anyone who can reach the port.
    let resolved = auth::resolve(&state.config.management)?;
    auth::log_mode(&resolved, listen_addr, &state.config.management);
    state.set_auth(resolved.auth.clone());
    // Only a token this node was *given* is presented to peers — see
    // `auth::fleet_token`.
    auth::set_fleet_token(match resolved.source {
        auth::Source::Config | auth::Source::Env => resolved.auth.api_token.clone(),
        _ => None,
    });

    // `api::router` carries `/metrics` and the credential check over it.
    let mut router = api::router(state.clone());

    // Mount web UI at /ui when the ui feature is enabled
    #[cfg(feature = "ui")]
    {
        router = router
            .nest("/ui", ui::ui_router(state.clone()))
            .route("/", axum::routing::get(|| async {
                axum::response::Redirect::permanent("/ui/")
            }));
    }

    let listener = TcpListener::bind(listen_addr).await?;

    // Check if TLS is configured
    if let (Some(cert_path), Some(key_path)) = (
        &state.config.management.tls_cert,
        &state.config.management.tls_key,
    ) {
        let reloader = Arc::new(tls::Reloader::new(tls::TlsFiles {
            cert: cert_path.into(),
            key: key_path.into(),
            client_ca: state.config.management.tls_client_ca.as_ref().map(Into::into),
            admin_ca: state.config.management.tls_admin_ca.as_ref().map(Into::into),
            admin_crl: state.config.management.tls_admin_crl.as_ref().map(Into::into),
            admin_names: state.config.management.tls_admin_names.clone(),
        })?);
        if state.config.management.tls_admin_ca.is_some() {
            if state.config.management.tls_admin_names.is_empty() {
                tracing::warn!("tls_admin_ca is set and tls_admin_names lists nobody: no certificate is admin (#379)");
            } else if state.config.management.tls_admin_crl.is_none() {
                tracing::warn!("tls_admin_ca is set with no tls_admin_crl: a certificate forge revokes stays admin here (#379)");
            }
        }
        match &state.config.management.tls_client_ca {
            Some(ca) => tracing::info!(
                "Management API listening on {listen_addr} (HTTPS; a client certificate from {ca} is a credential)"
            ),
            None => tracing::info!("Management API listening on {listen_addr} (HTTPS)"),
        }
        // Readiness asks whether this is listening, and only this code knows.
        // Set through the serving context when there is one — a node that is
        // not serving /serve/v1 has nobody to tell.
        if let Some(ctx) = state.serve.get() {
            ctx.status.set(&ctx.status.mgmt_listening, true);
        }
        serve_tls(listener, router, reloader).await?;
    } else {
        tracing::info!("Management API listening on {listen_addr} (HTTP)");
        // Readiness asks whether this is listening, and only this code knows.
        // Set through the serving context when there is one — a node that is
        // not serving /serve/v1 has nobody to tell.
        if let Some(ctx) = state.serve.get() {
            ctx.status.set(&ctx.status.mgmt_listening, true);
        }
        // The peer's address on every request, for its log line (#365).
        axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>())
            .await
            .map_err(|e| anyhow::anyhow!("management server error: {e}"))?;
    }

    Ok(())
}
