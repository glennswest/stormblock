//! Cluster REST API — /api/v1/cluster/* endpoints.

use std::sync::Arc;

use axum::{
    Router, routing::{get, post, delete},
    extract::{State, Path},
    Json, http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Serialize, Deserialize};

use crate::mgmt::AppState;
use crate::cluster::membership::NodeStatus;
use crate::cluster::heartbeat::{HeartbeatRequest, HeartbeatResponse};
use crate::cluster::raft::state::ClusterCommand;

use super::ApiError;

/// Node response for the REST API.
#[derive(Debug, Serialize)]
pub struct NodeResponse {
    pub node_id: u64,
    pub hostname: String,
    pub mgmt_addr: String,
    pub status: String,
    pub capacity_bytes: u64,
    pub drives_count: usize,
    pub arrays_count: usize,
    pub volumes_count: usize,
}

/// Join request body.
#[derive(Debug, Deserialize)]
pub struct JoinRequest {
    pub node_id: u64,
    pub hostname: String,
    pub mgmt_addr: String,
    pub capacity_bytes: u64,
}

/// Cluster status summary.
#[derive(Debug, Serialize)]
pub struct ClusterStatusResponse {
    pub leader_id: Option<u64>,
    pub node_count: usize,
    pub online_count: usize,
    pub local_node_id: u64,
    pub is_leader: bool,
}

/// Build the cluster API router.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/v1/cluster/nodes", get(list_nodes).post(join_node))
        .route("/api/v1/cluster/nodes/{id}", get(get_node).delete(remove_node))
        .route("/api/v1/cluster/status", get(cluster_status))
        .route("/api/v1/cluster/heartbeat", post(handle_heartbeat))
        .with_state(state)
}

/// GET /api/v1/cluster/nodes — list all cluster nodes.
async fn list_nodes(State(state): State<Arc<AppState>>) -> Response {
    let cluster = match &state.cluster {
        Some(c) => c,
        None => return ApiError::bad_request("cluster not enabled"),
    };
    let store = cluster.membership.read().await;
    let nodes: Vec<NodeResponse> = store.list_nodes()
        .iter()
        .map(|(info, status)| NodeResponse {
            node_id: info.node_id,
            hostname: info.hostname.clone(),
            mgmt_addr: info.mgmt_addr.clone(),
            status: status.to_string(),
            capacity_bytes: info.capacity_bytes,
            drives_count: info.drives_count,
            arrays_count: info.arrays_count,
            volumes_count: info.volumes_count,
        })
        .collect();
    let count = nodes.len();
    (StatusCode::OK, Json(super::ListResponse { items: nodes, count })).into_response()
}

/// GET /api/v1/cluster/nodes/{id} — get a single node.
async fn get_node(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u64>,
) -> Response {
    let cluster = match &state.cluster {
        Some(c) => c,
        None => return ApiError::bad_request("cluster not enabled"),
    };
    let store = cluster.membership.read().await;
    match store.get_node(id) {
        Some((info, status)) => {
            let resp = NodeResponse {
                node_id: info.node_id,
                hostname: info.hostname.clone(),
                mgmt_addr: info.mgmt_addr.clone(),
                status: status.to_string(),
                capacity_bytes: info.capacity_bytes,
                drives_count: info.drives_count,
                arrays_count: info.arrays_count,
                volumes_count: info.volumes_count,
            };
            (StatusCode::OK, Json(resp)).into_response()
        }
        None => ApiError::not_found(format!("node {id} not found")),
    }
}

/// POST /api/v1/cluster/nodes — join a node to the cluster.
async fn join_node(
    State(state): State<Arc<AppState>>,
    Json(req): Json<JoinRequest>,
) -> Response {
    let cluster = match &state.cluster {
        Some(c) => c,
        None => return ApiError::bad_request("cluster not enabled"),
    };

    let node_id = req.node_id;
    let hostname = req.hostname;
    let mgmt_addr = req.mgmt_addr;
    let capacity_bytes = req.capacity_bytes;

    // Add to Raft membership and cluster state
    let cmd = ClusterCommand::AddNode {
        node_id,
        hostname: hostname.clone(),
        mgmt_addr: mgmt_addr.clone(),
        capacity_bytes,
    };

    match cluster.propose(cmd).await {
        Ok(_) => {
            // Also add to local membership store
            let info = crate::cluster::membership::NodeInfo {
                node_id,
                hostname,
                mgmt_addr,
                capacity_bytes,
                drives_count: 0,
                arrays_count: 0,
                volumes_count: 0,
            };
            {
                let mut store = cluster.membership.write().await;
                store.add_node(info);
            }

            // Add to Raft voter set
            if let Err(e) = cluster.raft.change_membership(
                std::collections::BTreeSet::from([cluster.node_id, node_id]),
                false,
            ).await {
                tracing::warn!("failed to add node {node_id} to raft membership: {e}");
            }

            (StatusCode::CREATED, Json(serde_json::json!({
                "status": "joined",
                "node_id": node_id,
            }))).into_response()
        }
        Err(e) => ApiError::internal(format!("failed to add node: {e}")),
    }
}

/// DELETE /api/v1/cluster/nodes/{id} — remove a node from the cluster.
async fn remove_node(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u64>,
) -> Response {
    let cluster = match &state.cluster {
        Some(c) => c,
        None => return ApiError::bad_request("cluster not enabled"),
    };

    let cmd = ClusterCommand::RemoveNode { node_id: id };
    match cluster.propose(cmd).await {
        Ok(_) => {
            let mut store = cluster.membership.write().await;
            store.mark_leaving(id);
            store.remove_node(id);
            (StatusCode::OK, Json(serde_json::json!({
                "status": "removed",
                "node_id": id,
            }))).into_response()
        }
        Err(e) => ApiError::internal(format!("failed to remove node: {e}")),
    }
}

/// GET /api/v1/cluster/status — cluster health summary.
async fn cluster_status(State(state): State<Arc<AppState>>) -> Response {
    let cluster = match &state.cluster {
        Some(c) => c,
        None => return ApiError::bad_request("cluster not enabled"),
    };

    let leader_id = cluster.leader_id().await;
    let is_leader = cluster.is_leader().await;
    let store = cluster.membership.read().await;

    let resp = ClusterStatusResponse {
        leader_id,
        node_count: store.node_count(),
        online_count: store.online_count(),
        local_node_id: cluster.node_id,
        is_leader,
    };
    (StatusCode::OK, Json(resp)).into_response()
}

/// POST /api/v1/cluster/heartbeat — receive heartbeat from peer.
async fn handle_heartbeat(
    State(state): State<Arc<AppState>>,
    Json(req): Json<HeartbeatRequest>,
) -> Response {
    let cluster = match &state.cluster {
        Some(c) => c,
        None => return ApiError::bad_request("cluster not enabled"),
    };

    // Update membership with peer's info
    let peer_info = crate::cluster::membership::NodeInfo {
        node_id: req.node_id,
        hostname: req.hostname,
        mgmt_addr: req.mgmt_addr,
        capacity_bytes: req.capacity_bytes,
        drives_count: req.drives_count,
        arrays_count: req.arrays_count,
        volumes_count: req.volumes_count,
    };
    {
        let mut store = cluster.membership.write().await;
        store.heartbeat_success(req.node_id, peer_info);
    }

    // Respond with our info
    let hostname = crate::cluster::gethostname().unwrap_or_else(|| "unknown".to_string());
    let resp = HeartbeatResponse {
        node_id: cluster.node_id,
        hostname,
        status: "online".to_string(),
    };
    (StatusCode::OK, Json(resp)).into_response()
}
