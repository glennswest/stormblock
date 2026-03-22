//! Global Extent Map (GEM) — single source of truth for extent placement.
//!
//! The GEM tracks which slab slot holds each volume's virtual extent.
//! It replaces both the ExtentAllocator's per-array bitmap and ThinVolume's
//! local extent_map with a unified, cross-slab index.
//!
//! Recovery invariant: the GEM is reconstructable from slab slot tables.
//! Each slab's extent table is authoritative for its slots.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::drive::slab::SlabId;
use crate::volume::extent::VolumeId;

/// Location of a single extent in the slab mesh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtentLocation {
    pub slab_id: SlabId,
    pub slot_idx: u32,
    pub ref_count: u32,
    pub generation: u64,
}

/// Per-volume extent map — virtual extent index to physical location.
#[derive(Debug, Clone, Default)]
pub struct VolumeExtentMap {
    pub extents: BTreeMap<u64, ExtentLocation>,
}

impl VolumeExtentMap {
    pub fn new() -> Self {
        VolumeExtentMap {
            extents: BTreeMap::new(),
        }
    }

    /// Number of mapped extents.
    pub fn len(&self) -> usize {
        self.extents.len()
    }

    /// Whether this map has no extents.
    pub fn is_empty(&self) -> bool {
        self.extents.is_empty()
    }
}

/// Global Extent Map — tracks all extent locations across all volumes.
pub struct GlobalExtentMap {
    volumes: HashMap<VolumeId, VolumeExtentMap>,
    reverse: HashMap<(SlabId, u32), (VolumeId, u64)>,
}

impl GlobalExtentMap {
    pub fn new() -> Self {
        GlobalExtentMap {
            volumes: HashMap::new(),
            reverse: HashMap::new(),
        }
    }

    /// Insert or update an extent mapping.
    pub fn insert(
        &mut self,
        volume_id: VolumeId,
        vext_idx: u64,
        location: ExtentLocation,
    ) {
        let key = (location.slab_id, location.slot_idx);

        // Remove old reverse entry if this virtual extent was already mapped
        if let Some(vmap) = self.volumes.get(&volume_id) {
            if let Some(old_loc) = vmap.extents.get(&vext_idx) {
                let old_key = (old_loc.slab_id, old_loc.slot_idx);
                self.reverse.remove(&old_key);
            }
        }

        // Insert forward mapping
        self.volumes
            .entry(volume_id)
            .or_default()
            .extents
            .insert(vext_idx, location);

        // Insert reverse mapping
        self.reverse.insert(key, (volume_id, vext_idx));
    }

    /// Look up where a volume's virtual extent lives.
    pub fn lookup(&self, volume_id: VolumeId, vext_idx: u64) -> Option<&ExtentLocation> {
        self.volumes
            .get(&volume_id)?
            .extents
            .get(&vext_idx)
    }

    /// Remove an extent mapping.
    pub fn remove(&mut self, volume_id: VolumeId, vext_idx: u64) -> Option<ExtentLocation> {
        let vmap = self.volumes.get_mut(&volume_id)?;
        let loc = vmap.extents.remove(&vext_idx)?;
        let key = (loc.slab_id, loc.slot_idx);
        self.reverse.remove(&key);

        // Clean up empty volume map
        if vmap.extents.is_empty() {
            self.volumes.remove(&volume_id);
        }
        Some(loc)
    }

    /// Remove all extents for a volume. Returns the removed extent map.
    pub fn remove_volume(&mut self, volume_id: VolumeId) -> Option<VolumeExtentMap> {
        let vmap = self.volumes.remove(&volume_id)?;
        for loc in vmap.extents.values() {
            let key = (loc.slab_id, loc.slot_idx);
            self.reverse.remove(&key);
        }
        Some(vmap)
    }

    /// Get the volume extent map for a given volume.
    pub fn get_volume_map(&self, volume_id: &VolumeId) -> Option<&VolumeExtentMap> {
        self.volumes.get(volume_id)
    }

    /// Reverse lookup: given a slab+slot, find which volume+extent owns it.
    pub fn reverse_lookup(
        &self,
        slab_id: SlabId,
        slot_idx: u32,
    ) -> Option<(VolumeId, u64)> {
        self.reverse.get(&(slab_id, slot_idx)).copied()
    }

    /// Clone a volume's extent map for snapshot (bumps ref_count in the clone).
    pub fn clone_volume_map(
        &mut self,
        source_id: VolumeId,
        dest_id: VolumeId,
    ) -> Option<VolumeExtentMap> {
        let source_map = self.volumes.get(&source_id)?.clone();

        // Insert cloned mappings for the destination volume.
        // Note: ref_count updates in the actual slabs happen separately.
        let mut dest_map = VolumeExtentMap::new();
        for (&vext_idx, loc) in &source_map.extents {
            let new_loc = ExtentLocation {
                slab_id: loc.slab_id,
                slot_idx: loc.slot_idx,
                ref_count: loc.ref_count + 1,
                generation: loc.generation,
            };
            dest_map.extents.insert(vext_idx, new_loc.clone());

            // Note: reverse map still points to source — that's correct because
            // the slab slot is shared. The reverse map tracks the primary
            // owner. For COW, we track sharing via ref_count.
        }

        // Also update the source's ref_counts in the GEM
        if let Some(src_map) = self.volumes.get_mut(&source_id) {
            for loc in src_map.extents.values_mut() {
                loc.ref_count += 1;
            }
        }

        self.volumes.insert(dest_id, dest_map.clone());
        Some(dest_map)
    }

    /// Number of tracked volumes.
    pub fn volume_count(&self) -> usize {
        self.volumes.len()
    }

    /// Total number of extent mappings across all volumes.
    pub fn total_extents(&self) -> usize {
        self.volumes.values().map(|v| v.extents.len()).sum()
    }

    /// Number of reverse index entries.
    pub fn reverse_entries(&self) -> usize {
        self.reverse.len()
    }

    /// List all volume IDs.
    pub fn volume_ids(&self) -> Vec<VolumeId> {
        self.volumes.keys().copied().collect()
    }

    /// Iterate over all extent locations for a volume.
    pub fn volume_extents(
        &self,
        volume_id: &VolumeId,
    ) -> Option<impl Iterator<Item = (&u64, &ExtentLocation)>> {
        self.volumes.get(volume_id).map(|v| v.extents.iter())
    }

    /// Rebuild the GEM from slab slot tables. This is the recovery path:
    /// scan all slabs, reconstruct the full extent map.
    pub fn rebuild_from_slabs<'a>(
        slabs: impl Iterator<Item = (&'a SlabId, &'a super::super::drive::slab::Slab)>,
    ) -> Self {
        let mut gem = GlobalExtentMap::new();

        for (_, slab) in slabs {
            let cid = slab.slab_id();
            for slot_idx in 0..slab.total_slots() as u32 {
                if let Some(slot) = slab.get_slot(slot_idx) {
                    if slot.state != super::super::drive::slab::SlotState::Free {
                        let loc = ExtentLocation {
                            slab_id: cid,
                            slot_idx,
                            ref_count: slot.ref_count,
                            generation: slot.generation,
                        };
                        gem.insert(slot.volume_id, slot.virtual_extent_idx, loc);
                    }
                }
            }
        }

        gem
    }
}

impl Default for GlobalExtentMap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn cid() -> SlabId {
        SlabId(Uuid::new_v4())
    }

    fn loc(slab_id: SlabId, slot_idx: u32) -> ExtentLocation {
        ExtentLocation {
            slab_id,
            slot_idx,
            ref_count: 1,
            generation: 1,
        }
    }

    #[test]
    fn insert_and_lookup() {
        let mut gem = GlobalExtentMap::new();
        let vol = VolumeId::new();
        let c = cid();

        gem.insert(vol, 0, loc(c, 42));
        gem.insert(vol, 1, loc(c, 43));

        let l0 = gem.lookup(vol, 0).unwrap();
        assert_eq!(l0.slab_id, c);
        assert_eq!(l0.slot_idx, 42);

        let l1 = gem.lookup(vol, 1).unwrap();
        assert_eq!(l1.slot_idx, 43);

        assert!(gem.lookup(vol, 999).is_none());
        assert_eq!(gem.total_extents(), 2);
    }

    #[test]
    fn remove_extent() {
        let mut gem = GlobalExtentMap::new();
        let vol = VolumeId::new();
        let c = cid();

        gem.insert(vol, 0, loc(c, 10));
        gem.insert(vol, 1, loc(c, 11));

        let removed = gem.remove(vol, 0).unwrap();
        assert_eq!(removed.slot_idx, 10);
        assert!(gem.lookup(vol, 0).is_none());
        assert!(gem.lookup(vol, 1).is_some());
        assert_eq!(gem.total_extents(), 1);
    }

    #[test]
    fn remove_volume() {
        let mut gem = GlobalExtentMap::new();
        let vol = VolumeId::new();
        let c = cid();

        gem.insert(vol, 0, loc(c, 0));
        gem.insert(vol, 1, loc(c, 1));
        gem.insert(vol, 2, loc(c, 2));

        let map = gem.remove_volume(vol).unwrap();
        assert_eq!(map.len(), 3);
        assert_eq!(gem.volume_count(), 0);
        assert_eq!(gem.reverse_entries(), 0);
    }

    #[test]
    fn reverse_lookup() {
        let mut gem = GlobalExtentMap::new();
        let vol = VolumeId::new();
        let c = cid();

        gem.insert(vol, 5, loc(c, 99));

        let (v, idx) = gem.reverse_lookup(c, 99).unwrap();
        assert_eq!(v, vol);
        assert_eq!(idx, 5);

        assert!(gem.reverse_lookup(c, 0).is_none());
    }

    #[test]
    fn reverse_index_consistency() {
        let mut gem = GlobalExtentMap::new();
        let vol = VolumeId::new();
        let c1 = cid();
        let c2 = cid();

        gem.insert(vol, 0, loc(c1, 0));
        assert!(gem.reverse_lookup(c1, 0).is_some());

        // Move extent to different slab
        gem.insert(vol, 0, loc(c2, 5));
        assert!(gem.reverse_lookup(c1, 0).is_none());
        assert_eq!(gem.reverse_lookup(c2, 5).unwrap(), (vol, 0));
    }

    #[test]
    fn multi_volume() {
        let mut gem = GlobalExtentMap::new();
        let vol_a = VolumeId::new();
        let vol_b = VolumeId::new();
        let c = cid();

        gem.insert(vol_a, 0, loc(c, 0));
        gem.insert(vol_a, 1, loc(c, 1));
        gem.insert(vol_b, 0, loc(c, 2));
        gem.insert(vol_b, 1, loc(c, 3));

        assert_eq!(gem.volume_count(), 2);
        assert_eq!(gem.total_extents(), 4);

        assert_eq!(gem.lookup(vol_a, 0).unwrap().slot_idx, 0);
        assert_eq!(gem.lookup(vol_b, 0).unwrap().slot_idx, 2);
    }

    #[test]
    fn multi_slab_volume() {
        let mut gem = GlobalExtentMap::new();
        let vol = VolumeId::new();
        let c1 = cid();
        let c2 = cid();

        // Volume spreads across two slabs
        gem.insert(vol, 0, loc(c1, 0));
        gem.insert(vol, 1, loc(c2, 0));
        gem.insert(vol, 2, loc(c1, 1));

        assert_eq!(gem.lookup(vol, 0).unwrap().slab_id, c1);
        assert_eq!(gem.lookup(vol, 1).unwrap().slab_id, c2);
        assert_eq!(gem.lookup(vol, 2).unwrap().slab_id, c1);
    }

    #[test]
    fn clone_volume_map_for_snapshot() {
        let mut gem = GlobalExtentMap::new();
        let source = VolumeId::new();
        let snap = VolumeId::new();
        let c = cid();

        gem.insert(source, 0, loc(c, 10));
        gem.insert(source, 1, loc(c, 11));

        let cloned = gem.clone_volume_map(source, snap).unwrap();
        assert_eq!(cloned.len(), 2);

        // Both volumes now point to the same slots
        let src_loc = gem.lookup(source, 0).unwrap();
        let snap_loc = gem.lookup(snap, 0).unwrap();
        assert_eq!(src_loc.slot_idx, snap_loc.slot_idx);

        // Ref counts bumped
        assert_eq!(src_loc.ref_count, 2);
        assert_eq!(snap_loc.ref_count, 2);

        assert_eq!(gem.volume_count(), 2);
    }

    #[test]
    fn volume_ids_and_extents() {
        let mut gem = GlobalExtentMap::new();
        let vol = VolumeId::new();
        let c = cid();

        gem.insert(vol, 0, loc(c, 0));
        gem.insert(vol, 5, loc(c, 5));

        let ids = gem.volume_ids();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], vol);

        let extents: Vec<_> = gem.volume_extents(&vol).unwrap().collect();
        assert_eq!(extents.len(), 2);
        assert_eq!(*extents[0].0, 0);
        assert_eq!(*extents[1].0, 5);
    }

    #[test]
    fn empty_gem() {
        let gem = GlobalExtentMap::new();
        assert_eq!(gem.volume_count(), 0);
        assert_eq!(gem.total_extents(), 0);
        assert_eq!(gem.reverse_entries(), 0);
        assert!(gem.lookup(VolumeId::new(), 0).is_none());
    }
}
