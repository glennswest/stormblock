//! Slab registry — tracks all slabs by ID and tier.
//!
//! The registry is the entry point for finding slabs to allocate from
//! or to read/write existing slots. It indexes slabs by storage tier
//! for placement-aware allocation.

use std::collections::{HashMap, HashSet};

use crate::placement::domain::FailureDomain;
use crate::placement::topology::StorageTier;
use super::slab::{Slab, SlabId, SlabRole};

/// Registry of all slabs known to this node.
pub struct SlabRegistry {
    slabs: HashMap<SlabId, Slab>,
    tier_index: HashMap<StorageTier, Vec<SlabId>>,
    /// Slots handed out by `allocate` but not yet recorded in the Global
    /// Extent Map.
    ///
    /// Allocation and mapping are not one atomic step: a writer allocates
    /// under the registry lock, releases it to do the data write, and only
    /// then takes the GEM lock to record the mapping. In that window the slot
    /// is `Allocated` with nothing referencing it, which is indistinguishable
    /// from a leak — so garbage collection would free a slot a write is about
    /// to use. Reservations make the difference explicit.
    ///
    /// In-memory only, and deliberately so: after a restart nothing is
    /// in flight, and any slot left stranded by a crash mid-write is a real
    /// orphan that the collector should reclaim.
    in_flight: HashSet<(SlabId, u32)>,
    /// What fails together with each slab. Defaults to the identity of the
    /// device the slab lives on; widened by drive labels when a drive was
    /// registered with them (#70) or set outright.
    domains: HashMap<SlabId, FailureDomain>,
    /// Labels a drive was registered with (#70), by device path: every slab
    /// on that device — now and later — sits under them.
    device_labels: HashMap<String, FailureDomain>,
    /// The node's own rungs (`[management].topology`), under every slab.
    node_labels: FailureDomain,
    /// Slabs that take no new allocations: being drained, or reported
    /// failing by whoever watches the drives (#70).
    quarantined: HashSet<SlabId>,
}

impl SlabRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        SlabRegistry {
            slabs: HashMap::new(),
            tier_index: HashMap::new(),
            in_flight: HashSet::new(),
            domains: HashMap::new(),
            device_labels: HashMap::new(),
            node_labels: FailureDomain::new(),
            quarantined: HashSet::new(),
        }
    }

    /// The chain a slab sits in: its device's identity, under the device's
    /// labels, under the node's.
    fn derive_domain(&self, slab: &Slab) -> FailureDomain {
        let own = FailureDomain::from_device(slab.device().id());
        let own = match self.device_labels.get(&slab.device().id().path) {
            Some(outer) => own.merged_under(outer),
            None => own,
        };
        if self.node_labels.is_empty() { own } else { own.merged_under(&self.node_labels) }
    }

    /// Set the node's rungs (`rack`, `room`, `site`, …) under every slab,
    /// present and future. What `[management].topology` feeds (#72).
    pub fn set_node_labels(&mut self, labels: FailureDomain) {
        self.node_labels = labels;
        let ids: Vec<SlabId> = self.slabs.keys().copied().collect();
        for id in ids {
            let d = self.derive_domain(&self.slabs[&id]);
            self.domains.insert(id, d);
        }
    }

    pub fn node_labels(&self) -> &FailureDomain {
        &self.node_labels
    }

    /// Stop (or resume) handing out slots from a slab. Everything on it stays
    /// readable and writable in place; only *new* placement avoids it.
    pub fn set_quarantined(&mut self, id: SlabId, quarantined: bool) -> bool {
        if !self.slabs.contains_key(&id) {
            return false;
        }
        if quarantined {
            self.quarantined.insert(id);
        } else {
            self.quarantined.remove(&id);
        }
        true
    }

    pub fn is_quarantined(&self, id: &SlabId) -> bool {
        self.quarantined.contains(id)
    }

    pub fn quarantined(&self) -> Vec<SlabId> {
        let mut v: Vec<SlabId> = self.quarantined.iter().copied().collect();
        v.sort_by_key(|s| s.0);
        v
    }

    /// Whether a slab can take a new slot: has room and is not quarantined.
    fn allocatable(&self, id: &SlabId) -> Option<u64> {
        if self.quarantined.contains(id) {
            return None;
        }
        let free = self.slabs.get(id)?.free_slots();
        if free > 0 { Some(free) } else { None }
    }

    /// Say where a device is — `shelf=…/bay=…` from stormdrive, `rack=…`
    /// from an operator — so every slab on it is placed by that. Slabs
    /// already on the device are relabelled; slabs added later inherit it.
    pub fn label_device(&mut self, device_path: &str, labels: FailureDomain) {
        self.device_labels.insert(device_path.to_string(), labels);
        let ids: Vec<SlabId> = self
            .slabs
            .iter()
            .filter(|(_, s)| s.device().id().path == device_path)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let d = self.derive_domain(&self.slabs[&id]);
            self.domains.insert(id, d);
        }
    }

    /// The labels a device was registered with, if any.
    pub fn device_labels(&self, device_path: &str) -> Option<&FailureDomain> {
        self.device_labels.get(device_path)
    }

    /// Slabs whose device is at `device_path`.
    pub fn slabs_on_device(&self, device_path: &str) -> Vec<SlabId> {
        self.slabs
            .iter()
            .filter(|(_, s)| s.device().id().path == device_path)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Mark a freshly allocated slot as in flight, protecting it from
    /// collection until the caller records it in the GEM.
    pub fn reserve(&mut self, slab_id: SlabId, slot_idx: u32) {
        self.in_flight.insert((slab_id, slot_idx));
    }

    /// Release a reservation once the mapping exists (or the write failed and
    /// the slot was given back).
    pub fn commit(&mut self, slab_id: SlabId, slot_idx: u32) {
        self.in_flight.remove(&(slab_id, slot_idx));
    }

    /// Whether a slot is allocated-but-not-yet-mapped.
    pub fn is_reserved(&self, slab_id: SlabId, slot_idx: u32) -> bool {
        self.in_flight.contains(&(slab_id, slot_idx))
    }

    /// How many slots are currently in flight.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    /// Register a slab. Its failure domain is its device's identity until
    /// something says more.
    pub fn add(&mut self, slab: Slab) {
        let id = slab.slab_id();
        let tier = slab.tier();
        let domain = self.derive_domain(&slab);
        self.tier_index.entry(tier).or_default().push(id);
        self.domains.entry(id).or_insert(domain);
        self.slabs.insert(id, slab);
    }

    /// Register a slab with an explicit failure domain.
    pub fn add_in_domain(&mut self, slab: Slab, domain: FailureDomain) {
        let id = slab.slab_id();
        self.domains.insert(id, domain);
        self.add(slab);
    }

    /// Set (replace) a slab's failure domain.
    pub fn set_domain(&mut self, id: SlabId, domain: FailureDomain) -> bool {
        if !self.slabs.contains_key(&id) {
            return false;
        }
        self.domains.insert(id, domain);
        true
    }

    /// The failure domain a slab is in. Empty — *unknown* — only for a slab
    /// the registry never saw added, which should not happen.
    pub fn domain_of(&self, id: &SlabId) -> FailureDomain {
        self.domains.get(id).cloned().unwrap_or_default()
    }

    /// Whether a slab is the same domain at `rung` as any of `taken`. An
    /// empty entry in `taken` — a slab that is no longer registered, so
    /// nobody knows where it was — constrains nothing.
    pub fn collides(&self, id: &SlabId, taken: &[FailureDomain], rung: &str) -> bool {
        let d = self.domain_of(id);
        taken.iter().filter(|t| !t.is_empty()).any(|t| d.same_at(t, rung))
    }

    /// What a slab is for. A slab that is not registered reads as `System`,
    /// which is what every slab formatted before roles existed is.
    pub fn role_of(&self, id: &SlabId) -> SlabRole {
        self.slabs.get(id).map(|s| s.role()).unwrap_or_default()
    }

    /// The slab on `tier` with the most free slots whose domain at `rung`
    /// differs from every one in `taken` — the allocation step of a
    /// redundancy policy. `None` means the policy cannot be met on this
    /// tier, which the caller treats as a boundary, not a hint.
    ///
    /// `role` is a hard filter, not a preference. A system volume that
    /// spilled one copy-on-write extent into the data slab would be silently
    /// orphaned by the next reimage, and a data volume that spilled one into
    /// the system slab would lose that extent to the same reimage — which is
    /// the failure #88 is about, one extent at a time instead of all at once.
    pub fn best_slab_for_tier_apart_from(
        &self,
        tier: StorageTier,
        taken: &[FailureDomain],
        rung: &str,
        role: SlabRole,
    ) -> Option<SlabId> {
        self.tier_index
            .get(&tier)?
            .iter()
            .filter_map(|id| {
                let free = self.allocatable(id)?;
                if self.role_of(id) != role || self.collides(id, taken, rung) {
                    None
                } else {
                    Some((*id, free))
                }
            })
            .max_by_key(|(_, free)| *free)
            .map(|(id, _)| id)
    }

    /// How many distinct domains at `rung` have a slab with free space, on
    /// any tier — what a create checks before promising a policy.
    pub fn distinct_domains_with_space(&self, rung: &str) -> usize {
        self.distinct_domains_with_space_in_role(rung, SlabRole::System)
    }

    /// The same, counting only slabs of one role — the space a volume of
    /// that role can actually reach.
    pub fn distinct_domains_with_space_in_role(&self, rung: &str, role: SlabRole) -> usize {
        let mut seen: Vec<FailureDomain> = Vec::new();
        for id in self.slabs.keys() {
            if self.allocatable(id).is_none() || self.role_of(id) != role {
                continue;
            }
            let d = self.domain_of(id);
            if !seen.iter().any(|s| s.same_at(&d, rung)) {
                seen.push(d);
            }
        }
        seen.len()
    }

    /// Remove a slab by ID.
    pub fn remove(&mut self, id: &SlabId) -> Option<Slab> {
        if let Some(slab) = self.slabs.remove(id) {
            self.domains.remove(id);
            self.quarantined.remove(id);
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

    /// Find the system slab on the given tier with the most free slots.
    /// Returns None if no slabs on that tier have free space.
    pub fn best_slab_for_tier(&self, tier: StorageTier) -> Option<SlabId> {
        self.best_slab_for_tier_in_role(tier, SlabRole::System)
    }

    /// The same, for a named role.
    pub fn best_slab_for_tier_in_role(&self, tier: StorageTier, role: SlabRole) -> Option<SlabId> {
        self.tier_index
            .get(&tier)?
            .iter()
            .filter_map(|id| {
                if self.role_of(id) != role {
                    return None;
                }
                Some((*id, self.allocatable(id)?))
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
        // Fallback: any system slab with space
        self.slabs
            .keys()
            .filter(|id| self.role_of(id) == SlabRole::System)
            .filter_map(|id| Some((*id, self.allocatable(id)?)))
            .max_by_key(|(_, free)| *free)
            .map(|(id, _)| id)
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
