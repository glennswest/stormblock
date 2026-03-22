//! Slab registry — tracks all slabs by ID and tier.
//!
//! The registry is the entry point for finding slabs to allocate from
//! or to read/write existing slots. It indexes slabs by storage tier
//! for placement-aware allocation.

use std::collections::HashMap;

use crate::placement::topology::StorageTier;
use super::slab::{Slab, SlabId};

/// Registry of all slabs known to this node.
pub struct SlabRegistry {
    slabs: HashMap<SlabId, Slab>,
    tier_index: HashMap<StorageTier, Vec<SlabId>>,
}

impl SlabRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        SlabRegistry {
            slabs: HashMap::new(),
            tier_index: HashMap::new(),
        }
    }

    /// Register a slab.
    pub fn add(&mut self, slab: Slab) {
        let id = slab.slab_id();
        let tier = slab.tier();
        self.tier_index.entry(tier).or_default().push(id);
        self.slabs.insert(id, slab);
    }

    /// Remove a slab by ID.
    pub fn remove(&mut self, id: &SlabId) -> Option<Slab> {
        if let Some(slab) = self.slabs.remove(id) {
            let tier = slab.tier();
            if let Some(ids) = self.tier_index.get_mut(&tier) {
                ids.retain(|cid| cid != id);
            }
            Some(slab)
        } else {
            None
        }
    }

    /// Get an immutable reference to a slab.
    pub fn get(&self, id: &SlabId) -> Option<&Slab> {
        self.slabs.get(id)
    }

    /// Get a mutable reference to a slab.
    pub fn get_mut(&mut self, id: &SlabId) -> Option<&mut Slab> {
        self.slabs.get_mut(id)
    }

    /// List all slab IDs for a given tier.
    pub fn by_tier(&self, tier: StorageTier) -> &[SlabId] {
        self.tier_index.get(&tier).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Find the slab on the given tier with the most free slots.
    /// Returns None if no slabs on that tier have free space.
    pub fn best_slab_for_tier(&self, tier: StorageTier) -> Option<SlabId> {
        self.tier_index
            .get(&tier)?
            .iter()
            .filter_map(|id| {
                let c = self.slabs.get(id)?;
                if c.free_slots() > 0 {
                    Some((*id, c.free_slots()))
                } else {
                    None
                }
            })
            .max_by_key(|(_, free)| *free)
            .map(|(id, _)| id)
    }

    /// Find any slab with free space, preferring the given tier order.
    pub fn best_slab(&self, tier_preference: &[StorageTier]) -> Option<SlabId> {
        for tier in tier_preference {
            if let Some(id) = self.best_slab_for_tier(*tier) {
                return Some(id);
            }
        }
        // Fallback: any slab with space
        self.slabs
            .iter()
            .filter(|(_, c)| c.free_slots() > 0)
            .max_by_key(|(_, c)| c.free_slots())
            .map(|(id, _)| *id)
    }

    /// Total number of registered slabs.
    pub fn len(&self) -> usize {
        self.slabs.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.slabs.is_empty()
    }

    /// Iterate over all slabs.
    pub fn iter(&self) -> impl Iterator<Item = (&SlabId, &Slab)> {
        self.slabs.iter()
    }

    /// Iterate mutably over all slabs.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&SlabId, &mut Slab)> {
        self.slabs.iter_mut()
    }

    /// Total free slots across all slabs.
    pub fn total_free_slots(&self) -> u64 {
        self.slabs.values().map(|c| c.free_slots()).sum()
    }

    /// Total slots across all slabs.
    pub fn total_slots(&self) -> u64 {
        self.slabs.values().map(|c| c.total_slots()).sum()
    }
}

impl Default for SlabRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::slab::{Slab, DEFAULT_SLOT_SIZE};
    use crate::drive::filedev::FileDevice;
    use crate::drive::BlockDevice;
    use std::sync::Arc;
    use uuid::Uuid;

    async fn make_slab(size: u64, tier: StorageTier) -> (Slab, String) {
        let dir = std::env::temp_dir().join("stormblock-registry-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("reg-{}.bin", Uuid::new_v4().simple()));
        let path_str = path.to_str().unwrap().to_string();
        let _ = std::fs::remove_file(&path);
        let dev: Arc<dyn BlockDevice> =
            Arc::new(FileDevice::open_with_capacity(&path_str, size).await.unwrap());
        let slab = Slab::format(dev, DEFAULT_SLOT_SIZE, tier).await.unwrap();
        (slab, path_str)
    }

    fn cleanup(paths: &[String]) {
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }

    #[tokio::test]
    async fn registry_add_remove() {
        let (c1, p1) = make_slab(10 * 1024 * 1024, StorageTier::Hot).await;
        let (c2, p2) = make_slab(10 * 1024 * 1024, StorageTier::Cold).await;
        let id1 = c1.slab_id();
        let id2 = c2.slab_id();

        let mut reg = SlabRegistry::new();
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
        let (c_hot, p1) = make_slab(10 * 1024 * 1024, StorageTier::Hot).await;
        let (c_cold, p2) = make_slab(10 * 1024 * 1024, StorageTier::Cold).await;
        let hot_id = c_hot.slab_id();
        let cold_id = c_cold.slab_id();

        let mut reg = SlabRegistry::new();
        reg.add(c_hot);
        reg.add(c_cold);

        assert_eq!(reg.by_tier(StorageTier::Hot).len(), 1);
        assert_eq!(reg.by_tier(StorageTier::Cold).len(), 1);
        assert_eq!(reg.by_tier(StorageTier::Warm).len(), 0);

        let best_hot = reg.best_slab_for_tier(StorageTier::Hot).unwrap();
        assert_eq!(best_hot, hot_id);

        let best_cold = reg.best_slab_for_tier(StorageTier::Cold).unwrap();
        assert_eq!(best_cold, cold_id);

        // Prefer hot, fall back
        let best = reg.best_slab(&[StorageTier::Hot, StorageTier::Cold]).unwrap();
        assert_eq!(best, hot_id);

        cleanup(&[p1, p2]);
    }

    #[tokio::test]
    async fn registry_total_slots() {
        let (c1, p1) = make_slab(10 * 1024 * 1024, StorageTier::Hot).await;
        let (c2, p2) = make_slab(10 * 1024 * 1024, StorageTier::Hot).await;
        let total1 = c1.total_slots();
        let total2 = c2.total_slots();

        let mut reg = SlabRegistry::new();
        reg.add(c1);
        reg.add(c2);

        assert_eq!(reg.total_slots(), total1 + total2);
        assert_eq!(reg.total_free_slots(), total1 + total2);

        cleanup(&[p1, p2]);
    }
}
