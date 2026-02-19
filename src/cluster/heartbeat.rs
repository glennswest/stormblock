//! Heartbeat — periodic health pings between cluster peers.

use std::sync::Arc;
use std::time::Duration;

use serde::{Serialize, Deserialize};
use tokio::sync::RwLock;

use super::membership::{MembershipStore, NodeInfo};

/// Heartbeat request payload.
#[derive(Debug, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    pub node_id: u64,
    pub hostname: String,
    pub mgmt_addr: String,
    pub capacity_bytes: u64,
    pub drives_count: usize,
    pub arrays_count: usize,
    pub volumes_count: usize,
}

/// Heartbeat response payload.
#[derive(Debug, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    pub node_id: u64,
    pub hostname: String,
    pub status: String,
}

/// Start the heartbeat background task.
/// Periodically pings all known peers and updates their health status.
pub fn start_heartbeat(
    local_info: NodeInfo,
    membership: Arc<RwLock<MembershipStore>>,
    interval: Duration,
    membership_path: std::path::PathBuf,
) -> tokio::task::JoinHandle<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap_or_default();

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;

            // Collect peer addresses
            let peers: Vec<(u64, String)> = {
                let store = membership.read().await;
                store.list_nodes()
                    .iter()
                    .filter(|(info, _)| info.node_id != local_info.node_id)
                    .map(|(info, _)| (info.node_id, info.mgmt_addr.clone()))
                    .collect()
            };

            if peers.is_empty() {
                continue;
            }

            let req = HeartbeatRequest {
                node_id: local_info.node_id,
                hostname: local_info.hostname.clone(),
                mgmt_addr: local_info.mgmt_addr.clone(),
                capacity_bytes: local_info.capacity_bytes,
                drives_count: local_info.drives_count,
                arrays_count: local_info.arrays_count,
                volumes_count: local_info.volumes_count,
            };

            for (peer_id, peer_addr) in &peers {
                let url = format!("http://{}/api/v1/cluster/heartbeat", peer_addr);
                match client.post(&url).json(&req).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        // Parse response to get peer info
                        if let Ok(hb_resp) = resp.json::<HeartbeatResponse>().await {
                            let mut store = membership.write().await;
                            // Build updated info from heartbeat
                            let peer_info = NodeInfo {
                                node_id: *peer_id,
                                hostname: hb_resp.hostname,
                                mgmt_addr: peer_addr.clone(),
                                capacity_bytes: 0, // Updated from peer's own heartbeat to us
                                drives_count: 0,
                                arrays_count: 0,
                                volumes_count: 0,
                            };
                            store.heartbeat_success(*peer_id, peer_info);
                        }
                        metrics::counter!("stormblock_cluster_heartbeat_success_total").increment(1);
                    }
                    Ok(_) | Err(_) => {
                        let mut store = membership.write().await;
                        store.heartbeat_failure(*peer_id);
                        metrics::counter!("stormblock_cluster_heartbeat_failures_total").increment(1);
                        tracing::warn!("heartbeat to node {peer_id} ({peer_addr}) failed");
                    }
                }
            }

            // Update metrics
            {
                let store = membership.read().await;
                metrics::gauge!("stormblock_cluster_nodes_total").set(store.node_count() as f64);
                metrics::gauge!("stormblock_cluster_nodes_online").set(store.online_count() as f64);
            }

            // Persist membership after each round
            {
                let store = membership.read().await;
                if let Err(e) = store.persist(&membership_path) {
                    tracing::warn!("failed to persist membership: {e}");
                }
            }
        }
    })
}
