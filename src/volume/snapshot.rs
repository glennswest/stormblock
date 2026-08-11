//! COW snapshots — extent map cloning via the Global Extent Map.
//!
//! Snapshots share slab slots with their source volume via reference
//! counting. Writes to either the source or snapshot trigger copy-on-write
//! (handled by `ThinVolumeHandle::cow_write`).

use std::collections::HashMap;

use crate::drive::slab::SlabId;
use crate::drive::slab_registry::SlabRegistry;
use crate::volume::extent::VolumeId;
use crate::volume::gem::{ExtentLocation, GlobalExtentMap};
use crate::volume::thin::{ThinVolume, VolumeError};

/// Create a snapshot of a source volume.
///
/// Clones the volume's extent map in the GEM and increments ref_count on
/// all shared slab slots. Returns a new `ThinVolume` that is an independent
/// clone at the point-in-time of the snapshot.
pub async fn create_snapshot(
    source_id: VolumeId,
    name: &str,
    virtual_size: u64,
    slot_size: u64,
    gem: &mut GlobalExtentMap,
    registry: &mut SlabRegistry,
) -> Result<ThinVolume, VolumeError> {
    let snap_id = VolumeId::new();

    // Clone volume map in GEM (bumps ref_count in GEM entries). A volume
    // that has never been written has no GEM map yet — its snapshot is
    // simply empty (extents appear on first allocate-on-write).
    if let Some(cloned) = gem.clone_volume_map(source_id, snap_id) {
        // Increment ref_count on all slab slots (on-disk). Grouped per slab so
        // each one coalesces its slot-table writes by sector — a clone of an
        // N-extent image costs sectors touched, not N round trips.
        let mut by_slab: HashMap<SlabId, Vec<u32>> = HashMap::new();
        for loc in cloned.extents.values() {
            by_slab.entry(loc.slab_id).or_default().push(loc.slot_idx);
        }
        for (slab_id, slots) in by_slab {
            if let Some(slab) = registry.get_mut(&slab_id) {
                slab.inc_ref_batch(&slots).await.map_err(VolumeError::Drive)?;
            }
        }
    }

    // Create the snapshot volume
    let snap = ThinVolume {
        id: snap_id,
        name: name.to_string(),
        virtual_size,
        slot_size,
        purpose: crate::volume::thin::VolumePurpose::Partition,
        device_id: crate::drive::DeviceId {
            uuid: snap_id.0,
            serial: format!("snap-{}", &snap_id.0.simple().to_string()[..8]),
            model: "ThinVolume".to_string(),
            path: format!("volume:{snap_id}"),
        },
    };

    Ok(snap)
}

/// Delete a snapshot, freeing slab slots that are no longer shared.
///
/// Removes the volume from the GEM and decrements ref_count on all slab
/// slots. Slots whose ref_count reaches 0 are freed back to the slab.
pub async fn delete_snapshot(
    snap_id: VolumeId,
    gem: &mut GlobalExtentMap,
    registry: &mut SlabRegistry,
) -> Result<(), VolumeError> {
    // A never-written volume has no GEM map — nothing to free.
    if let Some(vmap) = gem.remove_volume(snap_id) {
        // Grouped per slab so the slot table is written by sector and the
        // header once, rather than twice per extent. Deleting a clone is on
        // the container restart path, so this is the hot direction too.
        let mut by_slab: HashMap<SlabId, Vec<u32>> = HashMap::new();
        for loc in vmap.extents.values() {
            by_slab.entry(loc.slab_id).or_default().push(loc.slot_idx);
        }
        for (slab_id, slots) in by_slab {
            let Some(slab) = registry.get_mut(&slab_id) else {
                // The map named a slab the registry does not have, so these
                // extents cannot be released by anyone. Silence here is how
                // the space went missing with no trace.
                tracing::warn!(
                    volume = %snap_id,
                    slab = %slab_id,
                    extents = slots.len(),
                    "slab not in registry while deleting volume — extents not released"
                );
                continue;
            };
            match slab.dec_ref_batch(&slots).await {
                Ok(outcome) => {
                    if !outcome.rejected.is_empty() {
                        tracing::warn!(
                            volume = %snap_id,
                            slab = %slab_id,
                            leaked = outcome.rejected.len(),
                            freed = outcome.freed,
                            "deleting volume left extents allocated — extent map and slot table diverged"
                        );
                    }
                }
                Err(e) => tracing::warn!(
                    volume = %snap_id,
                    slab = %slab_id,
                    extents = slots.len(),
                    "failed to release extents while deleting volume: {e}"
                ),
            }
        }
    }

    Ok(())
}

/// What a reset actually had to do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResetStats {
    /// Diverged extents whose private copy was released.
    pub freed: usize,
    /// Extents re-pointed at the source's data.
    pub restored: usize,
    /// Extents already identical to the source, so left untouched — the
    /// work this avoids versus delete-and-reclone.
    pub shared: usize,
}

/// Discard a clone's divergence, returning it to its source's contents.
///
/// Delete-and-reclone costs two reference updates for every extent in the
/// image. This touches only the extents the clone actually wrote, so the cost
/// tracks divergence rather than image size — which is the difference between
/// a container restart scaling with the golden image and scaling with what
/// that container happened to change.
///
/// The source is re-shared, not copied, so the clone is immediately COW again.
/// Resetting to a source that has itself moved on yields the source's current
/// contents, which is the intended "start from the golden image" semantic.
pub async fn reset_to_source(
    clone_id: VolumeId,
    source_id: VolumeId,
    gem: &mut GlobalExtentMap,
    registry: &mut SlabRegistry,
) -> Result<ResetStats, VolumeError> {
    let total_before = gem
        .get_volume_map(&clone_id)
        .map(|m| m.extents.len())
        .unwrap_or(0);

    let mut to_free: HashMap<SlabId, Vec<u32>> = HashMap::new();
    let mut to_share: HashMap<SlabId, Vec<u32>> = HashMap::new();
    let mut freed = 0usize;
    let mut restored = 0usize;

    for idx in snapshot_diff(gem, clone_id, source_id) {
        // Drop the clone's private copy, if it made one here.
        if let Some(old) = gem.remove(clone_id, idx) {
            to_free.entry(old.slab_id).or_default().push(old.slot_idx);
            freed += 1;
        }

        // Point back at the source's extent, if it still has one here.
        if let Some(loc) = gem.lookup(source_id, idx).cloned() {
            to_share.entry(loc.slab_id).or_default().push(loc.slot_idx);
            gem.restore_mapping(
                clone_id,
                idx,
                ExtentLocation {
                    slab_id: loc.slab_id,
                    slot_idx: loc.slot_idx,
                    ref_count: loc.ref_count + 1,
                    generation: loc.generation,
                },
            );
            // The source is now shared again, so a write to *it* must copy too.
            gem.inc_extent_ref(source_id, idx);
            restored += 1;
        }
    }

    // Take the new references before releasing the old ones: interrupted
    // halfway that leaks a reference, rather than freeing data still in use.
    for (slab_id, slots) in to_share {
        if let Some(slab) = registry.get_mut(&slab_id) {
            slab.inc_ref_batch(&slots).await.map_err(VolumeError::Drive)?;
        }
    }
    for (slab_id, slots) in to_free {
        if let Some(slab) = registry.get_mut(&slab_id) {
            let outcome = slab.dec_ref_batch(&slots).await.map_err(VolumeError::Drive)?;
            if !outcome.rejected.is_empty() {
                tracing::warn!(
                    clone = %clone_id,
                    slab = %slab_id,
                    leaked = outcome.rejected.len(),
                    "reset left diverged extents allocated"
                );
            }
        }
    }

    Ok(ResetStats {
        freed,
        restored,
        shared: total_before.saturating_sub(freed),
    })
}

/// Compute the diff between two volumes — returns virtual extent indices
/// where the volumes have different physical mappings.
///
/// Useful for incremental backup and cold copy advancement.
pub fn snapshot_diff(
    gem: &GlobalExtentMap,
    a: VolumeId,
    b: VolumeId,
) -> Vec<u64> {
    let a_map = gem.get_volume_map(&a);
    let b_map = gem.get_volume_map(&b);

    let a_keys: std::collections::BTreeSet<u64> = a_map
        .map(|m| m.extents.keys().copied().collect())
        .unwrap_or_default();
    let b_keys: std::collections::BTreeSet<u64> = b_map
        .map(|m| m.extents.keys().copied().collect())
        .unwrap_or_default();

    let mut diff = Vec::new();

    for &idx in a_keys.union(&b_keys) {
        let ea = a_map.and_then(|m| m.extents.get(&idx));
        let eb = b_map.and_then(|m| m.extents.get(&idx));

        match (ea, eb) {
            (Some(la), Some(lb)) => {
                // Both have this extent — differs if pointing to different slab slots
                if la.slab_id != lb.slab_id || la.slot_idx != lb.slot_idx {
                    diff.push(idx);
                }
            }
            _ => {
                // One has it, the other doesn't
                diff.push(idx);
            }
        }
    }

    diff
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::BlockDevice;
    use crate::drive::filedev::FileDevice;
    use crate::drive::slab::Slab;
    use crate::drive::slab_registry::SlabRegistry;
    use crate::placement::topology::StorageTier;
    use crate::raid::{RaidArray, RaidLevel};
    use crate::volume::gem::GlobalExtentMap;
    use crate::volume::thin::{ThinVolumeHandle, PlacementPolicy};
    use std::sync::Arc;

    async fn setup_volume_for_snapshot(
        slot_size: u64,
    ) -> (Arc<ThinVolumeHandle>, Arc<tokio::sync::RwLock<GlobalExtentMap>>, Arc<tokio::sync::RwLock<SlabRegistry>>, Vec<String>) {
        let test_id = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::env::temp_dir().join("stormblock-snap-test");
        std::fs::create_dir_all(&dir).unwrap();

        let mut devices: Vec<Arc<dyn BlockDevice>> = Vec::new();
        let mut paths = Vec::new();
        for i in 0..2 {
            let path = dir.join(format!("{test_id}-member-{i}.bin"));
            let path_str = path.to_str().unwrap().to_string();
            let _ = std::fs::remove_file(&path);
            let dev = FileDevice::open_with_capacity(&path_str, 64 * 1024 * 1024)
                .await
                .unwrap();
            devices.push(Arc::new(dev));
            paths.push(path_str);
        }

        let array = RaidArray::create(RaidLevel::Raid1, devices, None)
            .await
            .unwrap();
        let backing: Arc<dyn BlockDevice> = Arc::new(array);

        let slab = Slab::format(backing, slot_size, StorageTier::Hot)
            .await
            .unwrap();

        let mut registry = SlabRegistry::new();
        registry.add(slab);
        let registry = Arc::new(tokio::sync::RwLock::new(registry));
        let gem = Arc::new(tokio::sync::RwLock::new(GlobalExtentMap::new()));

        let vol = ThinVolume::new("source".to_string(), 128 * 1024 * 1024, slot_size);
        let handle = Arc::new(ThinVolumeHandle::new(
            vol,
            gem.clone(),
            registry.clone(),
            PlacementPolicy::default(),
        ));

        (handle, gem, registry, paths)
    }

    fn cleanup(paths: &[String]) {
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }

    /// A reset must give the clone the source's contents back, free only what
    /// diverged, and leave the source itself untouched.
    #[tokio::test]
    async fn reset_discards_divergence_and_restores_source_data() {
        let slot_size = 4096u64;
        let (source, gem, registry, paths) = setup_volume_for_snapshot(slot_size).await;

        // Golden image: four extents.
        for i in 0..4u64 {
            source.write(i * slot_size, &vec![0xA0 + i as u8; slot_size as usize])
                .await
                .unwrap();
        }
        let source_id = source.volume_id();

        // Clone it.
        let clone_vol = {
            let mut g = gem.write().await;
            let mut r = registry.write().await;
            create_snapshot(source_id, "clone", 128 * 1024 * 1024, slot_size, &mut g, &mut r)
                .await
                .unwrap()
        };
        let clone_id = clone_vol.id();
        let clone = Arc::new(ThinVolumeHandle::new(
            clone_vol, gem.clone(), registry.clone(), PlacementPolicy::default(),
        ));

        let free_after_clone = registry.read().await.total_free_slots();

        // Diverge two of the four extents, and write one the source never had.
        clone.write(0, &vec![0xFF; slot_size as usize]).await.unwrap();
        clone.write(slot_size, &vec![0xEE; slot_size as usize]).await.unwrap();
        clone.write(10 * slot_size, &vec![0xDD; slot_size as usize]).await.unwrap();

        let free_after_divergence = registry.read().await.total_free_slots();
        assert!(free_after_divergence < free_after_clone, "divergence allocates");

        let stats = {
            let mut g = gem.write().await;
            let mut r = registry.write().await;
            reset_to_source(clone_id, source_id, &mut g, &mut r).await.unwrap()
        };

        // Only the three diverged extents cost anything; the two untouched
        // ones were skipped entirely.
        assert_eq!(stats.freed, 3, "three private copies released");
        assert_eq!(stats.restored, 2, "two re-pointed at the source");
        assert!(stats.shared >= 2, "untouched extents left alone");

        // Divergence is gone: the clone reads the golden image again.
        for i in 0..4u64 {
            let mut buf = vec![0u8; slot_size as usize];
            clone.read(i * slot_size, &mut buf).await.unwrap();
            assert!(
                buf.iter().all(|&b| b == 0xA0 + i as u8),
                "extent {i} should read as the source's data"
            );
        }
        // The extent the source never had reads back as zeros.
        let mut buf = vec![0xFF_u8; slot_size as usize];
        clone.read(10 * slot_size, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0), "extent beyond the source is unmapped");

        // The source is untouched.
        for i in 0..4u64 {
            let mut buf = vec![0u8; slot_size as usize];
            source.read(i * slot_size, &mut buf).await.unwrap();
            assert!(buf.iter().all(|&b| b == 0xA0 + i as u8), "source extent {i}");
        }

        // The private copies came back to the slab.
        assert_eq!(
            registry.read().await.total_free_slots(),
            free_after_clone,
            "reset returns exactly what divergence took"
        );

        cleanup(&paths);
    }

    /// After a reset the clone is shared again, so the next write must copy
    /// rather than land in the source's slot.
    #[tokio::test]
    async fn reset_leaves_clone_copy_on_write() {
        let slot_size = 4096u64;
        let (source, gem, registry, paths) = setup_volume_for_snapshot(slot_size).await;

        source.write(0, &vec![0x11; slot_size as usize]).await.unwrap();
        let source_id = source.volume_id();

        let clone_vol = {
            let mut g = gem.write().await;
            let mut r = registry.write().await;
            create_snapshot(source_id, "clone", 128 * 1024 * 1024, slot_size, &mut g, &mut r)
                .await
                .unwrap()
        };
        let clone_id = clone_vol.id();
        let clone = Arc::new(ThinVolumeHandle::new(
            clone_vol, gem.clone(), registry.clone(), PlacementPolicy::default(),
        ));

        clone.write(0, &vec![0x22; slot_size as usize]).await.unwrap();
        {
            let mut g = gem.write().await;
            let mut r = registry.write().await;
            reset_to_source(clone_id, source_id, &mut g, &mut r).await.unwrap();
        }

        // Write again after the reset — this must COW, not scribble on the
        // source, or every container would corrupt the golden image.
        clone.write(0, &vec![0x33; slot_size as usize]).await.unwrap();

        let mut buf = vec![0u8; slot_size as usize];
        source.read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0x11), "source must survive a post-reset write");

        clone.read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0x33), "clone keeps its own new data");

        cleanup(&paths);
    }

    /// Resetting a clone that never diverged is free.
    #[tokio::test]
    async fn reset_of_untouched_clone_is_a_no_op() {
        let slot_size = 4096u64;
        let (source, gem, registry, paths) = setup_volume_for_snapshot(slot_size).await;

        for i in 0..3u64 {
            source.write(i * slot_size, &vec![0x77; slot_size as usize]).await.unwrap();
        }
        let source_id = source.volume_id();

        let clone_vol = {
            let mut g = gem.write().await;
            let mut r = registry.write().await;
            create_snapshot(source_id, "clone", 128 * 1024 * 1024, slot_size, &mut g, &mut r)
                .await
                .unwrap()
        };
        let clone_id = clone_vol.id();
        let free_before = registry.read().await.total_free_slots();

        let stats = {
            let mut g = gem.write().await;
            let mut r = registry.write().await;
            reset_to_source(clone_id, source_id, &mut g, &mut r).await.unwrap()
        };

        assert_eq!(stats.freed, 0);
        assert_eq!(stats.restored, 0);
        assert_eq!(registry.read().await.total_free_slots(), free_before);

        cleanup(&paths);
    }

    #[tokio::test]
    async fn snapshot_preserves_data() {
        let slot_size = 4096u64;
        let (handle, gem, registry, paths) = setup_volume_for_snapshot(slot_size).await;

        // Write data to source
        let data = vec![0xAA_u8; 4096];
        handle.write(0, &data).await.unwrap();

        // Take snapshot
        let source_id = handle.volume_id();
        let snap_vol = {
            let mut gem_guard = gem.write().await;
            let mut reg_guard = registry.write().await;
            create_snapshot(source_id, "snap1", 128 * 1024 * 1024, slot_size, &mut gem_guard, &mut reg_guard)
                .await
                .unwrap()
        };
        let snap_handle = Arc::new(ThinVolumeHandle::new(
            snap_vol,
            gem.clone(),
            registry.clone(),
            PlacementPolicy::default(),
        ));

        // Verify snapshot reads same data
        let mut buf = vec![0u8; 4096];
        snap_handle.read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xAA));

        // Write new data to source
        let new_data = vec![0xBB_u8; 4096];
        handle.write(0, &new_data).await.unwrap();

        // Source should have new data
        let mut src_buf = vec![0u8; 4096];
        handle.read(0, &mut src_buf).await.unwrap();
        assert!(src_buf.iter().all(|&b| b == 0xBB));

        // Snapshot should still have old data
        let mut snap_buf = vec![0u8; 4096];
        snap_handle.read(0, &mut snap_buf).await.unwrap();
        assert!(snap_buf.iter().all(|&b| b == 0xAA));

        cleanup(&paths);
    }

    #[tokio::test]
    async fn snapshot_diff_detects_changes() {
        let slot_size = 4096u64;
        let (handle, gem, registry, paths) = setup_volume_for_snapshot(slot_size).await;

        // Write initial data
        handle.write(0, &vec![0xAA_u8; 4096]).await.unwrap();
        handle.write(4096, &vec![0xBB_u8; 4096]).await.unwrap();

        // Take snapshot
        let source_id = handle.volume_id();
        let snap_vol = {
            let mut gem_guard = gem.write().await;
            let mut reg_guard = registry.write().await;
            create_snapshot(source_id, "snap1", 128 * 1024 * 1024, slot_size, &mut gem_guard, &mut reg_guard)
                .await
                .unwrap()
        };
        let snap_id = snap_vol.id();
        let _snap_handle = Arc::new(ThinVolumeHandle::new(
            snap_vol,
            gem.clone(),
            registry.clone(),
            PlacementPolicy::default(),
        ));

        // Modify source — triggers COW for extent 0
        handle.write(0, &vec![0xCC_u8; 4096]).await.unwrap();

        // Check diff
        let diff = {
            let gem_guard = gem.read().await;
            snapshot_diff(&gem_guard, source_id, snap_id)
        };

        assert!(diff.contains(&0), "extent 0 should be in diff");
        assert!(!diff.contains(&1), "extent 1 should not be in diff");

        cleanup(&paths);
    }

    #[tokio::test]
    async fn snapshot_delete_frees_unshared() {
        let slot_size = 4096u64;
        let (handle, gem, registry, paths) = setup_volume_for_snapshot(slot_size).await;

        handle.write(0, &vec![0xAA_u8; 4096]).await.unwrap();

        let source_id = handle.volume_id();
        let snap_vol = {
            let mut gem_guard = gem.write().await;
            let mut reg_guard = registry.write().await;
            create_snapshot(source_id, "snap1", 128 * 1024 * 1024, slot_size, &mut gem_guard, &mut reg_guard)
                .await
                .unwrap()
        };
        let snap_id = snap_vol.id();

        // Write to source to trigger COW
        handle.write(0, &vec![0xBB_u8; 4096]).await.unwrap();

        // Get free slots before delete
        let free_before = {
            let reg = registry.read().await;
            reg.total_free_slots()
        };

        // Delete snapshot
        {
            let mut gem_guard = gem.write().await;
            let mut reg_guard = registry.write().await;
            delete_snapshot(snap_id, &mut gem_guard, &mut reg_guard).await.unwrap();
        }

        let free_after = {
            let reg = registry.read().await;
            reg.total_free_slots()
        };

        // The old snapshot slot should have been freed (its ref_count dropped to 0)
        assert!(free_after > free_before);
        cleanup(&paths);
    }
}
