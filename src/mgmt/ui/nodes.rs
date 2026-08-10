//! Nodes page — GET /ui/nodes, plus cluster create/join/leave.
//!
//! Shows every node discovery can see, which cluster each belongs to, and
//! offers the join actions. Distinct from the Cluster page, which reports
//! Raft consensus state and only exists with the `cluster` feature; this one
//! is always available because discovery is.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Form, State};
use axum::response::Response;
use serde::Deserialize;

use crate::mgmt::AppState;
use crate::mgmt::config::human_size;
use super::shared;

/// One discovered node, shaped for the template.
pub struct NodeRow {
    pub name: String,
    pub mgmt_addr: String,
    pub cluster: String,
    /// True when this row is in the same cluster as us, so it takes part in
    /// placement — the distinction operators actually care about.
    pub in_our_cluster: bool,
    pub is_self: bool,
    pub total_human: String,
    pub free_human: String,
    pub version: String,
    pub status: String,
    pub status_class: &'static str,
}

/// A cluster visible on the network, offered as a join target.
pub struct ClusterRow {
    pub id: String,
    pub name: String,
    pub node_count: usize,
    pub is_ours: bool,
}

#[derive(Template)]
#[template(path = "nodes.html")]
struct NodesPage {
    active: &'static str,
    local_node: String,
    cluster_name: String,
    clustered: bool,
    peer_count: usize,
    nodes: Vec<NodeRow>,
    clusters: Vec<ClusterRow>,
}

#[derive(Template)]
#[template(path = "nodes_table.html")]
struct NodesTable {
    nodes: Vec<NodeRow>,
}

#[derive(Deserialize)]
pub struct CreateForm {
    pub name: String,
}

#[derive(Deserialize)]
pub struct JoinForm {
    pub cluster_name: String,
}

/// Gather everything both the full page and the partial need.
async fn gather(state: &Arc<AppState>) -> (Vec<NodeRow>, Vec<ClusterRow>, String, bool, usize) {
    let local = state.local_node_name();
    let Some(disc) = state.discovery.as_ref() else {
        return (Vec::new(), Vec::new(), String::new(), false, 0);
    };

    let ident = disc.identity().await;
    let ours = ident.cluster_id;
    let discovered = disc.nodes().await;
    let peer_count = disc.cluster_peers().await.len();

    let mut nodes: Vec<NodeRow> = Vec::new();

    // This node first — it never appears in its own beacon table.
    let (ltotal, lfree) = local_capacity(state).await;
    nodes.push(NodeRow {
        name: local.clone(),
        mgmt_addr: state.config.management.listen_addr.clone(),
        cluster: ident.cluster_name.clone().unwrap_or_else(|| "—".into()),
        in_our_cluster: ident.cluster_id.is_some(),
        is_self: true,
        total_human: human_size(ltotal),
        free_human: human_size(lfree),
        version: env!("CARGO_PKG_VERSION").to_string(),
        status: "this node".into(),
        status_class: "badge-info",
    });

    for n in &discovered {
        let same = ours.is_some() && n.beacon.cluster_id == ours;
        let (status, class) = if n.stale {
            ("unreachable", "badge-danger")
        } else if same {
            ("in cluster", "badge-success")
        } else if n.beacon.cluster_id.is_some() {
            ("other cluster", "badge-warning")
        } else {
            ("unclustered", "badge-info")
        };
        nodes.push(NodeRow {
            name: n.beacon.node_name.clone(),
            mgmt_addr: n.beacon.mgmt_addr.clone(),
            cluster: n.beacon.cluster_name.clone().unwrap_or_else(|| "—".into()),
            in_our_cluster: same,
            is_self: false,
            total_human: human_size(n.beacon.total_bytes),
            free_human: human_size(n.beacon.free_bytes),
            version: n.beacon.engine_version.clone(),
            status: status.into(),
            status_class: class,
        });
    }

    // Clusters on the network, for the join picker.
    let mut seen: std::collections::BTreeMap<uuid::Uuid, ClusterRow> = Default::default();
    for n in discovered.iter().filter(|n| !n.stale) {
        if let (Some(id), Some(name)) = (n.beacon.cluster_id, n.beacon.cluster_name.clone()) {
            seen.entry(id)
                .and_modify(|c| c.node_count += 1)
                .or_insert(ClusterRow {
                    id: id.to_string(),
                    name,
                    node_count: 1,
                    is_ours: Some(id) == ours,
                });
        }
    }
    if let (Some(id), Some(name)) = (ident.cluster_id, ident.cluster_name.clone()) {
        seen.entry(id)
            .and_modify(|c| c.node_count += 1)
            .or_insert(ClusterRow { id: id.to_string(), name, node_count: 1, is_ours: true });
    }

    (
        nodes,
        seen.into_values().collect(),
        ident.cluster_name.unwrap_or_default(),
        ident.cluster_id.is_some(),
        peer_count,
    )
}

async fn local_capacity(state: &AppState) -> (u64, u64) {
    let reg = state.slab_registry.read().await;
    let mut total = 0u64;
    let mut free = 0u64;
    for (_, slab) in reg.iter() {
        let s = slab.slot_size();
        total += slab.total_slots() * s;
        free += slab.free_slots() * s;
    }
    (total, free)
}

pub async fn page(State(state): State<Arc<AppState>>) -> Response {
    let (nodes, clusters, cluster_name, clustered, peer_count) = gather(&state).await;
    shared::render(&NodesPage {
        active: "nodes",
        local_node: state.local_node_name(),
        cluster_name,
        clustered,
        peer_count,
        nodes,
        clusters,
    })
}

pub async fn table_partial(State(state): State<Arc<AppState>>) -> Response {
    let (nodes, _, _, _, _) = gather(&state).await;
    shared::render(&NodesTable { nodes })
}

pub async fn create(State(state): State<Arc<AppState>>, Form(form): Form<CreateForm>) -> Response {
    let msg = match state.discovery.as_ref() {
        None => ("Discovery is not running".to_string(), "error"),
        Some(d) if d.identity().await.is_clustered() => {
            ("Already in a cluster — leave it first".to_string(), "error")
        }
        Some(_) if form.name.trim().is_empty() => {
            ("Cluster name must not be empty".to_string(), "error")
        }
        Some(d) => {
            let i = d.create_cluster(form.name.trim()).await;
            (format!("Created cluster '{}'", i.cluster_name.unwrap_or_default()), "success")
        }
    };
    respond(&state, msg).await
}

pub async fn join(State(state): State<Arc<AppState>>, Form(form): Form<JoinForm>) -> Response {
    let msg = match state.discovery.as_ref() {
        None => ("Discovery is not running".to_string(), "error"),
        Some(d) if d.identity().await.is_clustered() => {
            ("Already in a cluster — leave it first".to_string(), "error")
        }
        Some(d) => {
            // Resolve against live beacons, so the UI cannot join a cluster
            // that nothing is actually advertising.
            let target = d.nodes().await.into_iter().filter(|n| !n.stale).find_map(|n| {
                match (n.beacon.cluster_id, n.beacon.cluster_name) {
                    (Some(id), Some(name)) if name == form.cluster_name => Some((id, name)),
                    _ => None,
                }
            });
            match target {
                Some((id, name)) => {
                    d.join_cluster(id, &name).await;
                    (format!("Joined cluster '{name}'"), "success")
                }
                None => (
                    format!("No live node advertising cluster '{}'", form.cluster_name),
                    "error",
                ),
            }
        }
    };
    respond(&state, msg).await
}

pub async fn leave(State(state): State<Arc<AppState>>) -> Response {
    let msg = match state.discovery.as_ref() {
        None => ("Discovery is not running".to_string(), "error"),
        Some(d) => {
            d.leave_cluster().await;
            ("Left the cluster".to_string(), "success")
        }
    };
    respond(&state, msg).await
}

/// Creating, joining and leaving change the controls themselves — a create
/// form becomes a leave button — so swapping only the table would leave stale
/// actions on screen offering to create a cluster this node just joined.
/// Success therefore reloads the page; failures keep it and show a toast, so
/// the message is not lost to the reload.
async fn respond(state: &Arc<AppState>, (msg, level): (String, &str)) -> Response {
    use axum::response::{Html, IntoResponse};

    if level == "success" {
        let mut resp = Html(String::new()).into_response();
        resp.headers_mut()
            .insert("HX-Refresh", axum::http::HeaderValue::from_static("true"));
        return resp;
    }

    let (nodes, _, _, _, _) = gather(state).await;
    let table = NodesTable { nodes };
    let toast = shared::toast_oob(&msg, level);
    Html(format!("{}{}", table.render().unwrap_or_default(), toast)).into_response()
}
