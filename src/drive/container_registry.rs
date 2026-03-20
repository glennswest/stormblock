//! Container registry — tracks all containers by ID and tier.
//!
//! The registry is the entry point for finding containers to allocate from
//! or to read/write existing slots. It indexes containers by storage tier
//! for placement-aware allocation.

use std::collections::HashMap;

use crate::placement::topology::StorageTier;
use super::container::{Container, ContainerId};

/// Registry of all containers known to this node.
pub struct ContainerRegistry {
    containers: HashMap<ContainerId, Container>,
    tier_index: HashMap<StorageTier, Vec<ContainerId>>,
}

impl ContainerRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        ContainerRegistry {
            containers: HashMap::new(),
            tier_index: HashMap::new(),
        }
    }

    /// Register a container.
    pub fn add(&mut self, container: Container) {
        let id = container.container_id();
        let tier = container.tier();
        self.tier_index.entry(tier).or_default().push(id);
        self.containers.insert(id, container);
    }

    /// Remove a container by ID.
    pub fn remove(&mut self, id: &ContainerId) -> Option<Container> {
        if let Some(container) = self.containers.remove(id) {
            let tier = container.tier();
            if let Some(ids) = self.tier_index.get_mut(&tier) {
                ids.retain(|cid| cid != id);
            }
            Some(container)
        } else {
            None
        }
    }

    /// Get an immutable reference to a container.
    pub fn get(&self, id: &ContainerId) -> Option<&Container> {
        self.containers.get(id)
    }

    /// Get a mutable reference to a container.
    pub fn get_mut(&mut self, id: &ContainerId) -> Option<&mut Container> {
        self.containers.get_mut(id)
    }

    /// List all container IDs for a given tier.
    pub fn by_tier(&self, tier: StorageTier) -> &[ContainerId] {
        self.tier_index.get(&tier).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Find the container on the given tier with the most free slots.
    /// Returns None if no containers on that tier have free space.
    pub fn best_container_for_tier(&self, tier: StorageTier) -> Option<ContainerId> {
        self.tier_index
            .get(&tier)?
            .iter()
            .filter_map(|id| {
                let c = self.containers.get(id)?;
                if c.free_slots() > 0 {
                    Some((*id, c.free_slots()))
                } else {
                    None
                }
            })
            .max_by_key(|(_, free)| *free)
            .map(|(id, _)| id)
    }

    /// Find any container with free space, preferring the given tier order.
    pub fn best_container(&self, tier_preference: &[StorageTier]) -> Option<ContainerId> {
        for tier in tier_preference {
            if let Some(id) = self.best_container_for_tier(*tier) {
                return Some(id);
            }
        }
        // Fallback: any container with space
        self.containers
            .iter()
            .filter(|(_, c)| c.free_slots() > 0)
            .max_by_key(|(_, c)| c.free_slots())
            .map(|(id, _)| *id)
    }

    /// Total number of registered containers.
    pub fn len(&self) -> usize {
        self.containers.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.containers.is_empty()
    }

    /// Iterate over all containers.
    pub fn iter(&self) -> impl Iterator<Item = (&ContainerId, &Container)> {
        self.containers.iter()
    }

    /// Iterate mutably over all containers.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&ContainerId, &mut Container)> {
        self.containers.iter_mut()
    }

    /// Total free slots across all containers.
    pub fn total_free_slots(&self) -> u64 {
        self.containers.values().map(|c| c.free_slots()).sum()
    }

    /// Total slots across all containers.
    pub fn total_slots(&self) -> u64 {
        self.containers.values().map(|c| c.total_slots()).sum()
    }
}

impl Default for ContainerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::container::{Container, DEFAULT_SLOT_SIZE};
    use crate::drive::filedev::FileDevice;
    use crate::drive::BlockDevice;
    use std::sync::Arc;
    use uuid::Uuid;

    async fn make_container(size: u64, tier: StorageTier) -> (Container, String) {
        let dir = std::env::temp_dir().join("stormblock-registry-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("reg-{}.bin", Uuid::new_v4().simple()));
        let path_str = path.to_str().unwrap().to_string();
        let _ = std::fs::remove_file(&path);
        let dev: Arc<dyn BlockDevice> =
            Arc::new(FileDevice::open_with_capacity(&path_str, size).await.unwrap());
        let cont = Container::format(dev, DEFAULT_SLOT_SIZE, tier).await.unwrap();
        (cont, path_str)
    }

    fn cleanup(paths: &[String]) {
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }

    #[tokio::test]
    async fn registry_add_remove() {
        let (c1, p1) = make_container(10 * 1024 * 1024, StorageTier::Hot).await;
        let (c2, p2) = make_container(10 * 1024 * 1024, StorageTier::Cold).await;
        let id1 = c1.container_id();
        let id2 = c2.container_id();

        let mut reg = ContainerRegistry::new();
        assert!(reg.is_empty());

        reg.add(c1);
        reg.add(c2);
        assert_eq!(reg.len(), 2);

        assert!(reg.get(&id1).is_some());
        assert!(reg.get(&id2).is_some());

        let removed = reg.remove(&id1);
        assert!(removed.is_some());
        assert_eq!(reg.len(), 1);
        assert!(reg.get(&id1).is_none());

        cleanup(&[p1, p2]);
    }

    #[tokio::test]
    async fn registry_tier_selection() {
        let (c_hot, p1) = make_container(10 * 1024 * 1024, StorageTier::Hot).await;
        let (c_cold, p2) = make_container(10 * 1024 * 1024, StorageTier::Cold).await;
        let hot_id = c_hot.container_id();
        let cold_id = c_cold.container_id();

        let mut reg = ContainerRegistry::new();
        reg.add(c_hot);
        reg.add(c_cold);

        assert_eq!(reg.by_tier(StorageTier::Hot).len(), 1);
        assert_eq!(reg.by_tier(StorageTier::Cold).len(), 1);
        assert_eq!(reg.by_tier(StorageTier::Warm).len(), 0);

        let best_hot = reg.best_container_for_tier(StorageTier::Hot).unwrap();
        assert_eq!(best_hot, hot_id);

        let best_cold = reg.best_container_for_tier(StorageTier::Cold).unwrap();
        assert_eq!(best_cold, cold_id);

        // Prefer hot, fall back
        let best = reg.best_container(&[StorageTier::Hot, StorageTier::Cold]).unwrap();
        assert_eq!(best, hot_id);

        cleanup(&[p1, p2]);
    }

    #[tokio::test]
    async fn registry_total_slots() {
        let (c1, p1) = make_container(10 * 1024 * 1024, StorageTier::Hot).await;
        let (c2, p2) = make_container(10 * 1024 * 1024, StorageTier::Hot).await;
        let total1 = c1.total_slots();
        let total2 = c2.total_slots();

        let mut reg = ContainerRegistry::new();
        reg.add(c1);
        reg.add(c2);

        assert_eq!(reg.total_slots(), total1 + total2);
        assert_eq!(reg.total_free_slots(), total1 + total2);

        cleanup(&[p1, p2]);
    }
}
