//! Raft state machine — cluster metadata (nodes, volume placement, replication).

use std::collections::HashMap;

use serde::{Serialize, Deserialize};
use uuid::Uuid;

/// Commands applied through Raft consensus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClusterCommand {
    /// Add a node to the cluster.
    AddNode {
        node_id: u64,
        hostname: String,
        mgmt_addr: String,
        capacity_bytes: u64,
    },
    /// Remove a node from the cluster.
    RemoveNode { node_id: u64 },
    /// Update a node's health status.
    UpdateNodeHealth { node_id: u64, status: String },
    /// Assign a volume to a node.
    AssignVolume { volume_id: Uuid, node_id: u64 },
    /// Unassign a volume.
    UnassignVolume { volume_id: Uuid },
    /// Set replication targets for a volume.
    SetReplication { volume_id: Uuid, replica_nodes: Vec<u64> },
}

/// Response from state machine apply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClusterResponse {
    Ok,
    Error(String),
}

/// Node entry in the state machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmNodeInfo {
    pub node_id: u64,
    pub hostname: String,
    pub mgmt_addr: String,
    pub capacity_bytes: u64,
    pub status: String,
}

/// The cluster state machine data.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClusterState {
    pub nodes: HashMap<u64, SmNodeInfo>,
    pub volume_placement: HashMap<Uuid, u64>,
    pub volume_replicas: HashMap<Uuid, Vec<u64>>,
}

impl ClusterState {
    pub fn apply(&mut self, cmd: &ClusterCommand) -> ClusterResponse {
        match cmd {
            ClusterCommand::AddNode { node_id, hostname, mgmt_addr, capacity_bytes } => {
                self.nodes.insert(*node_id, SmNodeInfo {
                    node_id: *node_id,
                    hostname: hostname.clone(),
                    mgmt_addr: mgmt_addr.clone(),
                    capacity_bytes: *capacity_bytes,
                    status: "online".to_string(),
                });
                ClusterResponse::Ok
            }
            ClusterCommand::RemoveNode { node_id } => {
                self.nodes.remove(node_id);
                // Remove volume assignments for this node
                self.volume_placement.retain(|_, n| n != node_id);
                self.volume_replicas.values_mut().for_each(|nodes| {
                    nodes.retain(|n| n != node_id);
                });
                ClusterResponse::Ok
            }
            ClusterCommand::UpdateNodeHealth { node_id, status } => {
                if let Some(node) = self.nodes.get_mut(node_id) {
                    node.status = status.clone();
                    ClusterResponse::Ok
                } else {
                    ClusterResponse::Error(format!("node {node_id} not found"))
                }
            }
            ClusterCommand::AssignVolume { volume_id, node_id } => {
                self.volume_placement.insert(*volume_id, *node_id);
                ClusterResponse::Ok
            }
            ClusterCommand::UnassignVolume { volume_id } => {
                self.volume_placement.remove(volume_id);
                self.volume_replicas.remove(volume_id);
                ClusterResponse::Ok
            }
            ClusterCommand::SetReplication { volume_id, replica_nodes } => {
                self.volume_replicas.insert(*volume_id, replica_nodes.clone());
                ClusterResponse::Ok
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_add_remove_node() {
        let mut state = ClusterState::default();
        state.apply(&ClusterCommand::AddNode {
            node_id: 1,
            hostname: "node-1".into(),
            mgmt_addr: "10.0.0.1:9090".into(),
            capacity_bytes: 1_000_000,
        });
        assert_eq!(state.nodes.len(), 1);
        assert_eq!(state.nodes[&1].hostname, "node-1");

        state.apply(&ClusterCommand::RemoveNode { node_id: 1 });
        assert!(state.nodes.is_empty());
    }

    #[test]
    fn apply_volume_placement() {
        let mut state = ClusterState::default();
        let vol = Uuid::new_v4();
        state.apply(&ClusterCommand::AssignVolume { volume_id: vol, node_id: 1 });
        assert_eq!(state.volume_placement[&vol], 1);

        state.apply(&ClusterCommand::SetReplication {
            volume_id: vol,
            replica_nodes: vec![2, 3],
        });
        assert_eq!(state.volume_replicas[&vol], vec![2, 3]);

        state.apply(&ClusterCommand::UnassignVolume { volume_id: vol });
        assert!(!state.volume_placement.contains_key(&vol));
        assert!(!state.volume_replicas.contains_key(&vol));
    }

    #[test]
    fn remove_node_cleans_volumes() {
        let mut state = ClusterState::default();
        let vol = Uuid::new_v4();
        state.apply(&ClusterCommand::AddNode {
            node_id: 1,
            hostname: "n1".into(),
            mgmt_addr: "10.0.0.1:9090".into(),
            capacity_bytes: 1_000,
        });
        state.apply(&ClusterCommand::AssignVolume { volume_id: vol, node_id: 1 });
        state.apply(&ClusterCommand::SetReplication {
            volume_id: vol,
            replica_nodes: vec![1, 2],
        });
        state.apply(&ClusterCommand::RemoveNode { node_id: 1 });
        assert!(!state.volume_placement.contains_key(&vol));
        assert_eq!(state.volume_replicas[&vol], vec![2]);
    }
}
