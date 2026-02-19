//! Cluster module — optional multi-node scaling via Raft consensus.
//!
//! Everything here is behind `#[cfg(feature = "cluster")]`.
//! Single-node operation works without any of this.

pub mod config;
pub mod membership;
pub mod heartbeat;
pub mod raft;
pub mod replication;
pub mod migration;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use openraft::BasicNode;
use tokio::sync::RwLock;

use crate::mgmt::AppState;
use config::ClusterConfig;
use membership::{MembershipStore, NodeInfo};
use raft::{StormRaft, init_raft, raft_rpc_router};

/// Central coordinator for cluster operations.
pub struct ClusterManager {
    pub node_id: u64,
    pub raft: StormRaft,
    pub membership: Arc<RwLock<MembershipStore>>,
    pub config: ClusterConfig,
    heartbeat_handle: Option<tokio::task::JoinHandle<()>>,
}

impl ClusterManager {
    /// Create a new cluster manager.
    pub async fn new(config: ClusterConfig, _state: &Arc<AppState>) -> anyhow::Result<Self> {
        let node_id = config.load_or_create_node_id()?;
        tracing::info!("cluster: node_id = {node_id}");

        // Initialize Raft
        let raft_instance = init_raft(node_id, &config.data_dir).await?;

        // Load or create membership store
        let suspect_threshold = (config.heartbeat_timeout_ms / config.heartbeat_interval_ms) as u32;
        let unreachable_threshold = suspect_threshold + 2;
        let membership = MembershipStore::load(
            &config.membership_path(),
            suspect_threshold.max(2),
            unreachable_threshold.max(4),
        )?;

        Ok(ClusterManager {
            node_id,
            raft: raft_instance,
            membership: Arc::new(RwLock::new(membership)),
            config,
            heartbeat_handle: None,
        })
    }

    /// Start cluster services: heartbeat and optional seed join.
    pub async fn start(&mut self, state: &Arc<AppState>) -> anyhow::Result<()> {
        let local_info = self.build_local_info(state).await;

        // Register ourselves in membership
        {
            let mut store = self.membership.write().await;
            store.add_node(local_info.clone());
        }

        // Start heartbeat
        let hb_handle = heartbeat::start_heartbeat(
            local_info.clone(),
            self.membership.clone(),
            Duration::from_millis(self.config.heartbeat_interval_ms),
            self.config.membership_path(),
        );
        self.heartbeat_handle = Some(hb_handle);

        // If seed nodes are configured and we're a new node, try to join
        if !self.config.seed_nodes.is_empty() {
            for seed in &self.config.seed_nodes {
                if seed == &local_info.mgmt_addr {
                    continue; // Don't join ourselves
                }
                match self.join_cluster(seed, &local_info).await {
                    Ok(()) => {
                        tracing::info!("cluster: joined via seed {seed}");
                        break;
                    }
                    Err(e) => {
                        tracing::warn!("cluster: failed to join via {seed}: {e}");
                    }
                }
            }
        } else {
            // No seeds — initialize as single-node cluster if not already initialized
            self.try_init_single_node().await?;
        }

        tracing::info!("cluster: services started (node_id={})", self.node_id);
        Ok(())
    }

    /// Try to initialize as a single-node Raft cluster.
    async fn try_init_single_node(&self) -> anyhow::Result<()> {
        let mut members = BTreeMap::new();
        members.insert(self.node_id, BasicNode::new(""));
        match self.raft.initialize(members).await {
            Ok(()) => {
                tracing::info!("cluster: initialized as single-node cluster");
                Ok(())
            }
            Err(e) => {
                // Already initialized is fine
                tracing::debug!("cluster: raft init: {e} (may already be initialized)");
                Ok(())
            }
        }
    }

    /// Join an existing cluster via a seed node.
    async fn join_cluster(&self, seed_addr: &str, local_info: &NodeInfo) -> anyhow::Result<()> {
        let client = reqwest::Client::new();
        let url = format!("http://{}/api/v1/cluster/nodes", seed_addr);

        #[derive(serde::Serialize)]
        struct JoinRequest {
            node_id: u64,
            hostname: String,
            mgmt_addr: String,
            capacity_bytes: u64,
        }

        let req = JoinRequest {
            node_id: local_info.node_id,
            hostname: local_info.hostname.clone(),
            mgmt_addr: local_info.mgmt_addr.clone(),
            capacity_bytes: local_info.capacity_bytes,
        };

        let resp = client.post(&url).json(&req).send().await?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("join failed: {body}");
        }
        Ok(())
    }

    /// Check if this node is the Raft leader.
    pub async fn is_leader(&self) -> bool {
        self.raft.current_leader().await == Some(self.node_id)
    }

    /// Get the current Raft leader ID.
    pub async fn leader_id(&self) -> Option<u64> {
        self.raft.current_leader().await
    }

    /// Propose a command through Raft consensus.
    pub async fn propose(
        &self,
        cmd: raft::state::ClusterCommand,
    ) -> anyhow::Result<raft::state::ClusterResponse> {
        let resp = self.raft.client_write(cmd).await
            .map_err(|e| anyhow::anyhow!("raft propose: {e}"))?;
        Ok(resp.data)
    }

    /// Build axum router for Raft RPC endpoints.
    pub fn rpc_router(&self) -> axum::Router {
        raft_rpc_router(self.raft.clone())
    }

    /// Build local NodeInfo from current AppState.
    async fn build_local_info(&self, state: &Arc<AppState>) -> NodeInfo {
        let drives_count = state.drives.read().await.len();
        let arrays_count = state.arrays.read().await.len();
        let volumes = state.volume_manager.lock().await;
        let volumes_count = volumes.list_volumes().await.len();
        let capacity_bytes: u64 = {
            let drives = state.drives.read().await;
            drives.iter().map(|d| d.device.capacity_bytes()).sum()
        };
        let hostname = gethostname().unwrap_or_else(|| "unknown".to_string());

        NodeInfo {
            node_id: self.node_id,
            hostname,
            mgmt_addr: state.config.management.listen_addr.clone(),
            capacity_bytes,
            drives_count,
            arrays_count,
            volumes_count,
        }
    }

    /// Graceful shutdown — notify peers.
    pub async fn shutdown(&mut self) {
        tracing::info!("cluster: shutting down node {}", self.node_id);

        // Mark ourselves as leaving
        {
            let mut store = self.membership.write().await;
            store.mark_leaving(self.node_id);
        }

        // Cancel heartbeat
        if let Some(handle) = self.heartbeat_handle.take() {
            handle.abort();
        }

        // Shutdown Raft
        let _ = self.raft.shutdown().await;
    }
}

/// Get the system hostname.
pub(crate) fn gethostname() -> Option<String> {
    #[cfg(unix)]
    {
        let mut buf = vec![0u8; 256];
        let ret = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut i8, buf.len()) };
        if ret == 0 {
            let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            return String::from_utf8(buf[..nul].to_vec()).ok();
        }
    }
    None
}
