//! Management plane — REST API (axum), Prometheus metrics, config.

pub mod api;
pub mod config;
pub mod metrics;

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Serialize, Deserialize};
use tokio::net::TcpListener;
use uuid::Uuid;

use crate::drive::BlockDevice;
use crate::raid::{RaidArray, RaidArrayId, RaidLevel};
use crate::volume::VolumeManager;

use config::StormBlockConfig;

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
    pub config: StormBlockConfig,
}

impl AppState {
    pub fn new(config: StormBlockConfig, volume_manager: VolumeManager) -> Self {
        AppState {
            drives: tokio::sync::RwLock::new(Vec::new()),
            arrays: tokio::sync::RwLock::new(HashMap::new()),
            volume_manager: tokio::sync::Mutex::new(volume_manager),
            exports: tokio::sync::RwLock::new(Vec::new()),
            config,
        }
    }
}

/// Start the management REST API server.
pub async fn start_management_server(state: Arc<AppState>) -> anyhow::Result<()> {
    let listen_addr = &state.config.management.listen_addr;
    let router = api::router(state.clone())
        .merge(metrics::metrics_router());

    let listener = TcpListener::bind(listen_addr).await?;
    tracing::info!("Management API listening on {listen_addr}");

    axum::serve(listener, router)
        .await
        .map_err(|e| anyhow::anyhow!("management server error: {e}"))?;

    Ok(())
}
