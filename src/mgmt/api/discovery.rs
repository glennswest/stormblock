//! GET/POST /api/v1/discovery — discovered nodes and cluster join management.
//!
//! Discovery finds every node on the network; these routes are how an operator
//! decides which of them form a cluster. Creating and joining are recorded
//! locally and advertised by the beacon, so neither depends on a particular
//! node being up at the time.

use std::sync::Arc;

use axum::{
    Router,
    extract::State,
    routing::{get, post},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::ApiError;
use crate::mgmt::AppState;
use crate::mgmt::discovery::{ClusterIdentity, DiscoveredNode};

#[derive(Debug, Serialize)]
pub struct DiscoveryView {
    /// This node's own identity.
    pub local_node: String,
    pub cluster_id: Option<Uuid>,
    pub cluster_name: Option<String>,
    /// Every node heard from, including other clusters and unclustered ones.
    pub nodes: Vec<DiscoveredNode>,
    /// Distinct clusters visible on the network, for a join picker.
    pub clusters: Vec<ClusterSummary>,
    /// Live peers in this node's own cluster — what placement actually uses.
    pub cluster_peer_count: usize,
}

#[derive(Debug, Serialize)]
pub struct ClusterSummary {
    pub cluster_id: Uuid,
    pub cluster_name: String,
    pub node_count: usize,
}

#[derive(Debug, Deserialize)]
pub struct CreateClusterRequest {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct JoinClusterRequest {
    /// Join by cluster id...
    #[serde(default)]
    pub cluster_id: Option<Uuid>,
    /// ...or by name, resolved against what discovery can see.
    #[serde(default)]
    pub cluster_name: Option<String>,
}

async fn view(State(state): State<Arc<AppState>>) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "discovery", "method" => "list")
        .increment(1);

    let Some(disc) = state.discovery.as_ref() else {
        return ApiError::internal("discovery is not running");
    };

    let ident = disc.identity().await;
    let nodes = disc.nodes().await;
    let peers = disc.cluster_peers().await;

    // Summarise the clusters visible on the network so a join picker has
    // something to offer. Stale nodes are left out of the counts.
    let mut clusters: std::collections::BTreeMap<Uuid, ClusterSummary> = Default::default();
    for n in nodes.iter().filter(|n| !n.stale) {
        if let (Some(id), Some(name)) = (n.beacon.cluster_id, n.beacon.cluster_name.clone()) {
            clusters
                .entry(id)
                .and_modify(|c| c.node_count += 1)
                .or_insert(ClusterSummary { cluster_id: id, cluster_name: name, node_count: 1 });
        }
    }
    // Include our own cluster, which no peer beacon accounts for.
    if let (Some(id), Some(name)) = (ident.cluster_id, ident.cluster_name.clone()) {
        clusters
            .entry(id)
            .and_modify(|c| c.node_count += 1)
            .or_insert(ClusterSummary { cluster_id: id, cluster_name: name, node_count: 1 });
    }

    Json(DiscoveryView {
        local_node: state.local_node_name(),
        cluster_id: ident.cluster_id,
        cluster_name: ident.cluster_name,
        cluster_peer_count: peers.len(),
        nodes,
        clusters: clusters.into_values().collect(),
    })
    .into_response()
}

async fn create_cluster(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateClusterRequest>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "discovery", "method" => "create")
        .increment(1);

    let Some(disc) = state.discovery.as_ref() else {
        return ApiError::internal("discovery is not running");
    };
    if req.name.trim().is_empty() {
        return ApiError::bad_request("cluster name must not be empty");
    }
    if disc.identity().await.is_clustered() {
        return ApiError::conflict(
            "this node already belongs to a cluster; leave it first".to_string(),
        );
    }

    let ident = disc.create_cluster(req.name.trim()).await;
    (axum::http::StatusCode::CREATED, Json(ident)).into_response()
}

async fn join_cluster(
    State(state): State<Arc<AppState>>,
    Json(req): Json<JoinClusterRequest>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "discovery", "method" => "join")
        .increment(1);

    let Some(disc) = state.discovery.as_ref() else {
        return ApiError::internal("discovery is not running");
    };
    if disc.identity().await.is_clustered() {
        return ApiError::conflict(
            "this node already belongs to a cluster; leave it first".to_string(),
        );
    }

    // Resolve the target against what discovery can actually see, so a typo
    // cannot strand the node in a cluster of one that merely looks joined.
    let nodes = disc.nodes().await;
    let target = nodes
        .iter()
        .filter(|n| !n.stale)
        .find_map(|n| match (n.beacon.cluster_id, n.beacon.cluster_name.as_ref()) {
            (Some(id), Some(name)) => {
                let by_id = req.cluster_id == Some(id);
                let by_name = req.cluster_name.as_deref() == Some(name.as_str());
                (by_id || by_name).then(|| (id, name.clone()))
            }
            _ => None,
        });

    let Some((id, name)) = target else {
        return ApiError::not_found(
            "no live node advertising that cluster was found on the network".to_string(),
        );
    };

    let ident = disc.join_cluster(id, &name).await;
    Json(ident).into_response()
}

async fn leave_cluster(State(state): State<Arc<AppState>>) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "discovery", "method" => "leave")
        .increment(1);

    let Some(disc) = state.discovery.as_ref() else {
        return ApiError::internal("discovery is not running");
    };
    disc.leave_cluster().await;
    Json(ClusterIdentity::default()).into_response()
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(view))
        .route("/cluster", post(create_cluster))
        .route("/cluster/join", post(join_cluster))
        .route("/cluster/leave", post(leave_cluster))
        .with_state(state)
}
