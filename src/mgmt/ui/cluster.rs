//! Cluster page — GET /ui/cluster, GET /ui/cluster/nodes/table
//! Only compiled when feature = "cluster".

use std::sync::Arc;

use askama::Template;
use axum::extract::State;
use axum::response::Response;

use crate::mgmt::AppState;
use crate::mgmt::config::human_size;
use super::shared;

/// Node info for templates.
pub struct NodeRow {
    pub node_id: u64,
    pub hostname: String,
    pub mgmt_addr: String,
    pub status: String,
    pub capacity_human: String,
    pub drives_count: usize,
    pub arrays_count: usize,
    pub volumes_count: usize,
}

#[derive(Template)]
#[template(path = "cluster.html")]
struct ClusterPage {
    active: &'static str,
    enabled: bool,
    local_node_id: u64,
    is_leader: bool,
    node_count: usize,
    online_count: usize,
    leader_display: String,
    nodes: Vec<NodeRow>,
}

#[derive(Template)]
#[template(path = "cluster_nodes_table.html")]
struct ClusterNodesTable {
    nodes: Vec<NodeRow>,
}

pub async fn status_page(State(state): State<Arc<AppState>>) -> Response {
    let cluster = match &state.cluster {
        Some(c) => c,
        None => {
            return shared::render(&ClusterPage {
                active: "cluster",
                enabled: false,
                local_node_id: 0,
                is_leader: false,
                node_count: 0,
                online_count: 0,
                leader_display: "None".to_string(),
                nodes: vec![],
            });
        }
    };

    let leader_id = cluster.leader_id().await;
    let is_leader = cluster.is_leader().await;
    let store = cluster.membership.read().await;
    let node_count = store.node_count();
    let online_count = store.online_count();

    let nodes: Vec<NodeRow> = store
        .list_nodes()
        .iter()
        .map(|(info, status)| NodeRow {
            node_id: info.node_id,
            hostname: info.hostname.clone(),
            mgmt_addr: info.mgmt_addr.clone(),
            status: status.to_string(),
            capacity_human: human_size(info.capacity_bytes),
            drives_count: info.drives_count,
            arrays_count: info.arrays_count,
            volumes_count: info.volumes_count,
        })
        .collect();

    shared::render(&ClusterPage {
        active: "cluster",
        enabled: true,
        local_node_id: cluster.node_id,
        is_leader,
        node_count,
        online_count,
        leader_display: leader_id.map_or("None".to_string(), |id| id.to_string()),
        nodes,
    })
}

pub async fn nodes_table_partial(State(state): State<Arc<AppState>>) -> Response {
    let cluster = match &state.cluster {
        Some(c) => c,
        None => return shared::render(&ClusterNodesTable { nodes: vec![] }),
    };

    let store = cluster.membership.read().await;
    let nodes: Vec<NodeRow> = store
        .list_nodes()
        .iter()
        .map(|(info, status)| NodeRow {
            node_id: info.node_id,
            hostname: info.hostname.clone(),
            mgmt_addr: info.mgmt_addr.clone(),
            status: status.to_string(),
            capacity_human: human_size(info.capacity_bytes),
            drives_count: info.drives_count,
            arrays_count: info.arrays_count,
            volumes_count: info.volumes_count,
        })
        .collect();

    shared::render(&ClusterNodesTable { nodes })
}
