//! Raft consensus — type config, initialization, and RPC handlers.

pub mod store;
pub mod state;
pub mod network;

use std::io::Cursor;
use std::sync::Arc;

use openraft::Config;

use crate::cluster::raft::store::StormStore;
use crate::cluster::raft::state::ClusterCommand;
use crate::cluster::raft::state::ClusterResponse;
use crate::cluster::raft::network::HttpNetworkFactory;

// Type config for our Raft cluster.
openraft::declare_raft_types!(
    pub StormTypeConfig:
        D            = ClusterCommand,
        R            = ClusterResponse,
        NodeId       = u64,
        Node         = openraft::BasicNode,
        Entry        = openraft::Entry<StormTypeConfig>,
        SnapshotData = Cursor<Vec<u8>>,
);

/// The Raft instance type alias.
pub type StormRaft = openraft::Raft<StormTypeConfig>;

/// Initialize a Raft node.
pub async fn init_raft(
    node_id: u64,
    data_dir: &str,
) -> anyhow::Result<StormRaft> {
    let config = Config {
        heartbeat_interval: 500,
        election_timeout_min: 1500,
        election_timeout_max: 3000,
        max_payload_entries: 256,
        snapshot_policy: openraft::SnapshotPolicy::LogsSinceLast(1000),
        ..Default::default()
    };
    let config = Arc::new(config.validate().map_err(|e| anyhow::anyhow!("raft config: {e}"))?);

    let store = StormStore::new(data_dir).await?;
    let (log_store, state_machine) = openraft::storage::Adaptor::new(store);
    let network = HttpNetworkFactory::new();

    let raft = StormRaft::new(node_id, config, network, log_store, state_machine).await?;

    Ok(raft)
}

// --- Axum RPC handlers for inter-node Raft communication ---

use axum::{Router, routing::post, extract::State, Json, http::StatusCode, response::IntoResponse};
use openraft::raft::{
    AppendEntriesRequest,
    VoteRequest,
    InstallSnapshotRequest,
};

/// Shared state for Raft RPC handlers.
#[derive(Clone)]
pub struct RaftRpcState {
    pub raft: StormRaft,
}

/// Build axum router for Raft RPC endpoints.
pub fn raft_rpc_router(raft: StormRaft) -> Router {
    let state = RaftRpcState { raft };
    Router::new()
        .route("/raft/vote", post(handle_vote))
        .route("/raft/append", post(handle_append))
        .route("/raft/snapshot", post(handle_snapshot))
        .with_state(state)
}

async fn handle_vote(
    State(state): State<RaftRpcState>,
    Json(req): Json<VoteRequest<u64>>,
) -> impl IntoResponse {
    match state.raft.vote(req).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

async fn handle_append(
    State(state): State<RaftRpcState>,
    Json(req): Json<AppendEntriesRequest<StormTypeConfig>>,
) -> impl IntoResponse {
    match state.raft.append_entries(req).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

async fn handle_snapshot(
    State(state): State<RaftRpcState>,
    Json(req): Json<InstallSnapshotRequest<StormTypeConfig>>,
) -> impl IntoResponse {
    match state.raft.install_snapshot(req).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}
