//! Membership store — tracks known nodes, health status, and persistence.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use serde::{Serialize, Deserialize};

/// Health status of a cluster node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    /// Heartbeat OK.
    Online,
    /// Missed 1-2 heartbeats.
    Suspect,
    /// Missed 3+ heartbeats.
    Unreachable,
    /// Graceful removal in progress.
    Leaving,
}

impl std::fmt::Display for NodeStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeStatus::Online => write!(f, "online"),
            NodeStatus::Suspect => write!(f, "suspect"),
            NodeStatus::Unreachable => write!(f, "unreachable"),
            NodeStatus::Leaving => write!(f, "leaving"),
        }
    }
}

/// Identity and metadata of a cluster node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeInfo {
    pub node_id: u64,
    pub hostname: String,
    /// Management API address (e.g. "10.0.0.1:9090").
    pub mgmt_addr: String,
    pub capacity_bytes: u64,
    pub drives_count: usize,
    pub arrays_count: usize,
    pub volumes_count: usize,
}

/// Per-node tracking entry (status + timing).
struct MemberEntry {
    info: NodeInfo,
    status: NodeStatus,
    last_seen: Instant,
    missed_heartbeats: u32,
}

/// Serializable form for persistence (no Instants).
#[derive(Serialize, Deserialize)]
struct PersistedMember {
    info: NodeInfo,
    status: NodeStatus,
}

/// Tracks all known nodes in the cluster with health transitions.
pub struct MembershipStore {
    members: HashMap<u64, MemberEntry>,
    suspect_threshold: u32,
    unreachable_threshold: u32,
}

impl MembershipStore {
    /// Create a new membership store.
    /// `suspect_after` — number of missed heartbeats before marking suspect.
    /// `unreachable_after` — number of missed heartbeats before marking unreachable.
    pub fn new(suspect_after: u32, unreachable_after: u32) -> Self {
        MembershipStore {
            members: HashMap::new(),
            suspect_threshold: suspect_after,
            unreachable_threshold: unreachable_after,
        }
    }

    /// Add or update a node in the store.
    pub fn add_node(&mut self, info: NodeInfo) {
        let id = info.node_id;
        self.members.insert(id, MemberEntry {
            info,
            status: NodeStatus::Online,
            last_seen: Instant::now(),
            missed_heartbeats: 0,
        });
    }

    /// Remove a node from the store.
    pub fn remove_node(&mut self, node_id: u64) -> Option<NodeInfo> {
        self.members.remove(&node_id).map(|e| e.info)
    }

    /// Get a node's info and status.
    pub fn get_node(&self, node_id: u64) -> Option<(&NodeInfo, NodeStatus)> {
        self.members.get(&node_id).map(|e| (&e.info, e.status))
    }

    /// List all nodes with their status.
    pub fn list_nodes(&self) -> Vec<(&NodeInfo, NodeStatus)> {
        self.members.values().map(|e| (&e.info, e.status)).collect()
    }

    /// Total number of known nodes.
    pub fn node_count(&self) -> usize {
        self.members.len()
    }

    /// Count nodes that are online.
    pub fn online_count(&self) -> usize {
        self.members.values().filter(|e| e.status == NodeStatus::Online).count()
    }

    /// Record a successful heartbeat from a node — reset to Online.
    pub fn heartbeat_success(&mut self, node_id: u64, info: NodeInfo) {
        if let Some(entry) = self.members.get_mut(&node_id) {
            entry.info = info;
            entry.status = NodeStatus::Online;
            entry.last_seen = Instant::now();
            entry.missed_heartbeats = 0;
        } else {
            self.add_node(info);
        }
    }

    /// Record a failed heartbeat from a node — increment miss count and transition status.
    pub fn heartbeat_failure(&mut self, node_id: u64) {
        if let Some(entry) = self.members.get_mut(&node_id) {
            entry.missed_heartbeats += 1;
            if entry.missed_heartbeats >= self.unreachable_threshold {
                entry.status = NodeStatus::Unreachable;
            } else if entry.missed_heartbeats >= self.suspect_threshold {
                entry.status = NodeStatus::Suspect;
            }
        }
    }

    /// Mark a node as leaving (graceful removal).
    pub fn mark_leaving(&mut self, node_id: u64) {
        if let Some(entry) = self.members.get_mut(&node_id) {
            entry.status = NodeStatus::Leaving;
        }
    }

    /// Update a node's info (e.g. capacity changed).
    pub fn update_info(&mut self, info: NodeInfo) {
        if let Some(entry) = self.members.get_mut(&info.node_id) {
            entry.info = info;
        }
    }

    /// Persist the membership to a JSON file.
    pub fn persist(&self, path: &Path) -> anyhow::Result<()> {
        let persisted: Vec<PersistedMember> = self.members.values()
            .map(|e| PersistedMember {
                info: e.info.clone(),
                status: e.status,
            })
            .collect();
        let json = serde_json::to_string_pretty(&persisted)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Load membership from a JSON file.
    pub fn load(path: &Path, suspect_after: u32, unreachable_after: u32) -> anyhow::Result<Self> {
        let mut store = Self::new(suspect_after, unreachable_after);
        if !path.exists() {
            return Ok(store);
        }
        let json = std::fs::read_to_string(path)?;
        let persisted: Vec<PersistedMember> = serde_json::from_str(&json)?;
        let now = Instant::now();
        for p in persisted {
            store.members.insert(p.info.node_id, MemberEntry {
                info: p.info,
                status: p.status,
                last_seen: now,
                missed_heartbeats: 0,
            });
        }
        Ok(store)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_info(id: u64) -> NodeInfo {
        NodeInfo {
            node_id: id,
            hostname: format!("node-{id}"),
            mgmt_addr: format!("10.0.0.{id}:9090"),
            capacity_bytes: 1_000_000_000,
            drives_count: 4,
            arrays_count: 1,
            volumes_count: 2,
        }
    }

    #[test]
    fn add_and_list() {
        let mut store = MembershipStore::new(2, 4);
        store.add_node(make_info(1));
        store.add_node(make_info(2));
        assert_eq!(store.node_count(), 2);
        assert_eq!(store.online_count(), 2);
    }

    #[test]
    fn heartbeat_transitions() {
        let mut store = MembershipStore::new(2, 4);
        store.add_node(make_info(1));

        // 1 miss — still online
        store.heartbeat_failure(1);
        assert_eq!(store.get_node(1).unwrap().1, NodeStatus::Online);

        // 2 misses — suspect
        store.heartbeat_failure(1);
        assert_eq!(store.get_node(1).unwrap().1, NodeStatus::Suspect);

        // 3 misses — still suspect
        store.heartbeat_failure(1);
        assert_eq!(store.get_node(1).unwrap().1, NodeStatus::Suspect);

        // 4 misses — unreachable
        store.heartbeat_failure(1);
        assert_eq!(store.get_node(1).unwrap().1, NodeStatus::Unreachable);

        // Heartbeat success — back to online
        store.heartbeat_success(1, make_info(1));
        assert_eq!(store.get_node(1).unwrap().1, NodeStatus::Online);
    }

    #[test]
    fn persist_and_load() {
        let dir = std::env::temp_dir().join("stormblock-membership-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("members.json");

        let mut store = MembershipStore::new(2, 4);
        store.add_node(make_info(1));
        store.add_node(make_info(2));
        store.persist(&path).unwrap();

        let loaded = MembershipStore::load(&path, 2, 4).unwrap();
        assert_eq!(loaded.node_count(), 2);
        assert!(loaded.get_node(1).is_some());
        assert!(loaded.get_node(2).is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_node() {
        let mut store = MembershipStore::new(2, 4);
        store.add_node(make_info(1));
        assert!(store.remove_node(1).is_some());
        assert!(store.get_node(1).is_none());
        assert!(store.remove_node(99).is_none());
    }
}
