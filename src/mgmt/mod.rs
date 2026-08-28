//! Management plane — REST API (axum), Prometheus metrics, config.

pub mod api;
pub mod config;
pub mod metrics;
pub mod discovery;
pub mod ublk_export;
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

use rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

/// Information about an opened drive, stored in AppState.
pub struct DriveInfo {
    pub device: Arc<dyn BlockDevice>,
    pub path: String,
    /// Where the drive is — failure-domain labels given at registration
    /// (#70). Empty when nobody said.
    pub labels: crate::placement::domain::FailureDomain,
}

/// Information about a RAID array, stored in AppState.
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
pub struct AppState {
    pub drives: tokio::sync::RwLock<Vec<DriveInfo>>,
    pub arrays: tokio::sync::RwLock<HashMap<RaidArrayId, ArrayInfo>>,
    /// Behind an `Arc` so work that outlives a request — replenishing a
    /// template's standing clone after a claim — can hold it (#55).
    pub volume_manager: Arc<tokio::sync::Mutex<VolumeManager>>,
    pub exports: tokio::sync::RwLock<Vec<ExportEntry>>,
    pub slab_registry: Arc<tokio::sync::RwLock<SlabRegistry>>,
    pub gem: Arc<tokio::sync::RwLock<GlobalExtentMap>>,
    /// Control-plane state behind the /v1 CSI contract surface.
    pub v1: tokio::sync::Mutex<api::v1::V1State>,
    /// StormFS chunk ownership, map versions and version pins (#49, #50).
    /// Taken **before** the volume manager wherever both are needed.
    #[cfg(feature = "stormfs-data")]
    pub stormfs: tokio::sync::Mutex<api::stormfs::StormFsState>,
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
    /// Latest pool-pressure sample, kept current by the watcher (#18).
    pub pool_pressure: Option<std::sync::Arc<tokio::sync::RwLock<Option<crate::volume::pressure::PressureStatus>>>>,
    pub config: StormBlockConfig,
    /// Node/cluster discovery. `None` when it could not be started (for
    /// example a network without multicast) — the node still serves its own
    /// volumes, it just cannot see peers.
    pub discovery: Option<Arc<discovery::Discovery>>,
    /// Most recent extent-GC pass, when the background collector is running.
    pub last_gc: Option<Arc<tokio::sync::RwLock<Option<crate::volume::gc::GcSummary>>>>,
    #[cfg(feature = "iscsi")]
    pub iscsi_target: tokio::sync::RwLock<Option<Arc<IscsiTarget>>>,
    /// Live NVMe-oF target, so exports can add namespaces at runtime (#26).
    #[cfg(feature = "nvmeof")]
    pub nvmeof_target: tokio::sync::RwLock<Option<Arc<crate::target::nvmeof::NvmeofTarget>>>,
    /// Live LUN table, keyed by LUN ID for O(1) lookup at thousands of
    /// LUNs (#24).
    #[cfg(feature = "iscsi")]
    pub lun_entries: tokio::sync::RwLock<HashMap<u64, LunEntry>>,
    #[cfg(feature = "cluster")]
    pub cluster: Option<Arc<crate::cluster::ClusterManager>>,
}

impl AppState {
    /// This node's name in the /v1 surface and in discovery beacons.
    pub fn local_node_name(&self) -> String {
        self.config
            .management
            .node_name
            .clone()
            .or_else(|| std::env::var("STORMBLOCK_NODE").ok())
            .or_else(|| std::env::var("HOSTNAME").ok())
            .unwrap_or_else(|| "localhost".to_string())
    }

    pub fn new(
        config: StormBlockConfig,
        volume_manager: VolumeManager,
        slab_registry: Arc<tokio::sync::RwLock<SlabRegistry>>,
        gem: Arc<tokio::sync::RwLock<GlobalExtentMap>>,
    ) -> Self {
        AppState {
            drives: tokio::sync::RwLock::new(Vec::new()),
            arrays: tokio::sync::RwLock::new(HashMap::new()),
            volume_manager: Arc::new(tokio::sync::Mutex::new(volume_manager)),
            exports: tokio::sync::RwLock::new(Vec::new()),
            slab_registry,
            gem,
            v1: tokio::sync::Mutex::new(api::v1::V1State::from_config(&config)),
            #[cfg(feature = "stormfs-data")]
            stormfs: tokio::sync::Mutex::new(match config.management.data_dir.as_ref() {
                Some(dir) => api::stormfs::StormFsState::load(std::path::Path::new(dir)),
                None => api::stormfs::StormFsState::default(),
            }),
            fstemplates: Arc::new(tokio::sync::Mutex::new(
                match config.management.data_dir.as_ref() {
                    Some(dir) => crate::fs::TemplateStore::load(std::path::Path::new(dir)),
                    // No data dir means nothing survives a restart anyway; a
                    // template that outlived its store would be unreachable.
                    None => crate::fs::TemplateStore::in_memory(),
                },
            )),
            ublk_exports: tokio::sync::Mutex::new(ublk_export::UblkExportManager::new()),
            moves: tokio::sync::RwLock::new(match config.management.data_dir.as_ref() {
                Some(dir) => api::moves::load(std::path::Path::new(dir)),
                None => HashMap::new(),
            }),
            data_dir: config.management.data_dir.as_ref().map(std::path::PathBuf::from),
            serve: std::sync::OnceLock::new(),
            pool_pressure: None,
            config,
            discovery: None,
            last_gc: None,
            #[cfg(feature = "iscsi")]
            iscsi_target: tokio::sync::RwLock::new(None),
            #[cfg(feature = "nvmeof")]
            nvmeof_target: tokio::sync::RwLock::new(None),
            #[cfg(feature = "iscsi")]
            lun_entries: tokio::sync::RwLock::new(HashMap::new()),
            #[cfg(feature = "cluster")]
            cluster: None,
        }
    }
}

/// Load TLS configuration from PEM cert and key files.
fn load_tls_config(cert_path: &str, key_path: &str) -> anyhow::Result<ServerConfig> {
    let cert_file = std::fs::File::open(cert_path)
        .map_err(|e| anyhow::anyhow!("failed to open TLS cert '{}': {e}", cert_path))?;
    let key_file = std::fs::File::open(key_path)
        .map_err(|e| anyhow::anyhow!("failed to open TLS key '{}': {e}", key_path))?;

    let certs: Vec<_> = rustls_pemfile::certs(&mut std::io::BufReader::new(cert_file))
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("failed to parse TLS certs: {e}"))?;
    if certs.is_empty() {
        anyhow::bail!("no certificates found in {cert_path}");
    }

    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(key_file))
        .map_err(|e| anyhow::anyhow!("failed to parse TLS key: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {key_path}"))?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("invalid TLS configuration: {e}"))?;

    Ok(config)
}

/// Start the management REST API server.
pub async fn start_management_server(state: Arc<AppState>) -> anyhow::Result<()> {
    // Give every sealed template a clone standing by (#55). Spawned: a node
    // that is starting should serve requests now and be fast shortly, not the
    // other way round. Templates restored from disk arrive with their
    // `standing` field, so this only mints what is actually missing — a claim
    // that raced ahead of it, or a template sealed by an older build.
    {
        let state = state.clone();
        tokio::spawn(async move {
            let n = crate::fs::template::ensure_standing_all(
                &state.volume_manager,
                &state.fstemplates,
            )
            .await;
            if n > 0 {
                tracing::info!("{n} template(s) now have a clone standing by");
            }
        });
    }

    let listen_addr = &state.config.management.listen_addr;
    let mut router = api::router(state.clone())
        .merge(metrics::metrics_router(state.clone()));

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
        let tls_config = load_tls_config(cert_path, key_path)?;
        let acceptor = TlsAcceptor::from(Arc::new(tls_config));
        tracing::info!("Management API listening on {listen_addr} (HTTPS)");
        // Readiness asks whether this is listening, and only this code knows.
        // Set through the serving context when there is one — a node that is
        // not serving /serve/v1 has nobody to tell.
        if let Some(ctx) = state.serve.get() {
            ctx.status.set(&ctx.status.mgmt_listening, true);
        }

        loop {
            let (tcp_stream, _peer) = listener.accept().await?;
            let acceptor = acceptor.clone();
            let app = router.clone();
            tokio::spawn(async move {
                let tls_stream = match acceptor.accept(tcp_stream).await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::debug!("TLS handshake failed: {e}");
                        return;
                    }
                };
                let io = hyper_util::rt::TokioIo::new(tls_stream);
                let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let app = app.clone();
                    async move {
                        use tower::Service;
                        let mut svc = app;
                        let req = req.map(axum::body::Body::new);
                        Ok::<_, std::convert::Infallible>(svc.call(req).await.unwrap())
                    }
                });
                let _ = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection(io, service)
                    .await;
            });
        }
    } else {
        tracing::info!("Management API listening on {listen_addr} (HTTP)");
        // Readiness asks whether this is listening, and only this code knows.
        // Set through the serving context when there is one — a node that is
        // not serving /serve/v1 has nobody to tell.
        if let Some(ctx) = state.serve.get() {
            ctx.status.set(&ctx.status.mgmt_listening, true);
        }
        axum::serve(listener, router)
            .await
            .map_err(|e| anyhow::anyhow!("management server error: {e}"))?;
    }

    Ok(())
}
