//! Management plane — REST API (axum), Prometheus metrics, config.

pub mod api;
pub mod config;
pub mod metrics;
#[cfg(feature = "ui")]
pub mod ui;

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Serialize, Deserialize};
use tokio::net::TcpListener;
use uuid::Uuid;

use crate::drive::BlockDevice;
use crate::drive::pool::DiskPool;
use crate::raid::{RaidArray, RaidArrayId, RaidLevel};
use crate::volume::VolumeManager;

use config::StormBlockConfig;

use rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

/// Information about an opened drive, stored in AppState.
pub struct DriveInfo {
    pub device: Arc<dyn BlockDevice>,
    pub path: String,
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
}

/// Shared application state for the management API.
pub struct AppState {
    pub drives: tokio::sync::RwLock<Vec<DriveInfo>>,
    pub arrays: tokio::sync::RwLock<HashMap<RaidArrayId, ArrayInfo>>,
    pub volume_manager: tokio::sync::Mutex<VolumeManager>,
    pub exports: tokio::sync::RwLock<Vec<ExportEntry>>,
    pub pools: tokio::sync::RwLock<HashMap<Uuid, DiskPool>>,
    pub config: StormBlockConfig,
    #[cfg(feature = "cluster")]
    pub cluster: Option<Arc<crate::cluster::ClusterManager>>,
}

impl AppState {
    pub fn new(config: StormBlockConfig, volume_manager: VolumeManager) -> Self {
        AppState {
            drives: tokio::sync::RwLock::new(Vec::new()),
            arrays: tokio::sync::RwLock::new(HashMap::new()),
            volume_manager: tokio::sync::Mutex::new(volume_manager),
            exports: tokio::sync::RwLock::new(Vec::new()),
            pools: tokio::sync::RwLock::new(HashMap::new()),
            config,
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
    let listen_addr = &state.config.management.listen_addr;
    let mut router = api::router(state.clone())
        .merge(metrics::metrics_router());

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
        axum::serve(listener, router)
            .await
            .map_err(|e| anyhow::anyhow!("management server error: {e}"))?;
    }

    Ok(())
}
