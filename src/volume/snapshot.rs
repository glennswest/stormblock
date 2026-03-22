//! COW snapshots — extent map cloning via the Global Extent Map.
//!
//! Snapshots share slab slots with their source volume via reference
//! counting. Writes to either the source or snapshot trigger copy-on-write
//! (handled by `ThinVolumeHandle::cow_write`).

use crate::drive::slab_registry::SlabRegistry;
use crate::volume::extent::VolumeId;
use crate::volume::gem::GlobalExtentMap;
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

    // Clone volume map in GEM (bumps ref_count in GEM entries)
    let cloned = gem.clone_volume_map(source_id, snap_id)
        .ok_or(VolumeError::VolumeNotFound(source_id))?;

    // Increment ref_count on all slab slots (on-disk)
    for loc in cloned.extents.values() {
        if let Some(slab) = registry.get_mut(&loc.slab_id) {
            slab.inc_ref(loc.slot_idx).await
                .map_err(VolumeError::Drive)?;
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
    let vmap = gem.remove_volume(snap_id)
        .ok_or(VolumeError::VolumeNotFound(snap_id))?;

    for loc in vmap.extents.values() {
        if let Some(slab) = registry.get_mut(&loc.slab_id) {
            let _ = slab.dec_ref(loc.slot_idx).await;
        }
    }

    Ok(())
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
    ) -> (Arc<ThinVolumeHandle>, Arc<tokio::sync::Mutex<GlobalExtentMap>>, Arc<tokio::sync::Mutex<SlabRegistry>>, Vec<String>) {
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
        let registry = Arc::new(tokio::sync::Mutex::new(registry));
        let gem = Arc::new(tokio::sync::Mutex::new(GlobalExtentMap::new()));

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
            let mut gem_guard = gem.lock().await;
            let mut reg_guard = registry.lock().await;
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
            let mut gem_guard = gem.lock().await;
            let mut reg_guard = registry.lock().await;
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
            let gem_guard = gem.lock().await;
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
            let mut gem_guard = gem.lock().await;
            let mut reg_guard = registry.lock().await;
            create_snapshot(source_id, "snap1", 128 * 1024 * 1024, slot_size, &mut gem_guard, &mut reg_guard)
                .await
                .unwrap()
        };
        let snap_id = snap_vol.id();

        // Write to source to trigger COW
        handle.write(0, &vec![0xBB_u8; 4096]).await.unwrap();

        // Get free slots before delete
        let free_before = {
            let reg = registry.lock().await;
            reg.total_free_slots()
        };

        // Delete snapshot
        {
            let mut gem_guard = gem.lock().await;
            let mut reg_guard = registry.lock().await;
            delete_snapshot(snap_id, &mut gem_guard, &mut reg_guard).await.unwrap();
        }

        let free_after = {
            let reg = registry.lock().await;
            reg.total_free_slots()
        };

        // The old snapshot slot should have been freed (its ref_count dropped to 0)
        assert!(free_after > free_before);
        cleanup(&paths);
    }
}
