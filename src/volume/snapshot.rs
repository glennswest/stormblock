//! COW snapshots — extent map cloning with reference counting.
//!
//! Snapshots share physical extents with their source volume via reference
//! counting. Writes to either the source or snapshot trigger copy-on-write
//! (handled by `ThinVolume::cow_extent`).

use crate::volume::extent::{Extent, VolumeId};
use crate::volume::thin::ThinVolume;

/// Create a snapshot of a source volume.
///
/// Clones the extent map and increments ref_count on all shared extents.
/// Returns a new `ThinVolume` that is an independent clone at the point-in-time
/// of the snapshot. Subsequent writes to either volume trigger COW.
pub fn create_snapshot(source: &mut ThinVolume, name: &str) -> ThinVolume {
    // Increment ref_count on all extents in the source
    for pext in source.extent_map.values_mut() {
        pext.ref_count += 1;
    }

    // Clone the extent map for the snapshot
    let snap_extent_map = source.extent_map.clone();

    let id = VolumeId::new();
    let device_id = crate::drive::DeviceId {
        uuid: id.0,
        serial: format!("snap-{}", &id.0.simple().to_string()[..8]),
        model: "ThinVolume".to_string(),
        path: format!("volume:{id}"),
    };

    ThinVolume {
        id,
        name: name.to_string(),
        virtual_size: source.virtual_size,
        allocated: source.allocated,
        extent_map: snap_extent_map,
        array_id: source.array_id,
        backing_device: source.backing_device.clone(),
        allocator: source.allocator.clone(),
        device_id,
    }
}

/// Delete a snapshot, freeing physical extents that are no longer shared.
///
/// Each volume holds its own copy of the extent map with local ref_counts.
/// When a COW write occurs on the source, it gets a new physical extent —
/// the old extent becomes exclusively owned by the snapshot (but the snapshot's
/// local ref_count is stale). So we check: if ref_count == 1, the extent was
/// never shared or the other side already COW'd away. If ref_count > 1,
/// the other volume still shares this exact physical extent — don't free.
///
/// Note: this is a simplified model for Phase 3. A production system would use
/// a centralized ref_count store to avoid stale counts.
pub async fn delete_snapshot(snap: ThinVolume, volumes: &[&ThinVolume]) {
    let mut alloc = snap.allocator.lock().await;
    for (_vext_idx, pext) in &snap.extent_map {
        // Check if any other volume still references this physical extent
        let still_referenced = volumes.iter().any(|v| {
            v.extent_map.values().any(|other| {
                other.array_id == pext.array_id && other.offset == pext.offset
            })
        });
        if !still_referenced {
            let ext = Extent {
                array_id: pext.array_id,
                offset: pext.offset,
                length: pext.length,
            };
            alloc.free(&ext);
        }
    }
}

/// Compute the diff between two volumes — returns virtual extent indices
/// where the volumes have different physical mappings.
///
/// Useful for incremental backup: only transfer blocks that changed since snapshot.
pub fn snapshot_diff(a: &ThinVolume, b: &ThinVolume) -> Vec<u64> {
    let mut diff = Vec::new();

    // Collect all virtual extent indices from both maps
    let a_keys: std::collections::BTreeSet<u64> = a.extent_map.keys().copied().collect();
    let b_keys: std::collections::BTreeSet<u64> = b.extent_map.keys().copied().collect();

    // Union of all keys
    for &idx in a_keys.union(&b_keys) {
        match (a.extent_map.get(&idx), b.extent_map.get(&idx)) {
            (Some(ea), Some(eb)) => {
                // Both have this extent — differs if pointing to different physical locations
                if ea.offset != eb.offset || ea.array_id != eb.array_id {
                    diff.push(idx);
                }
            }
            _ => {
                // One has it, the other doesn't — that's a difference
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
    use crate::raid::{RaidArray, RaidLevel};
    use crate::volume::extent::ExtentAllocator;
    use crate::volume::thin::ThinVolumeHandle;
    use std::sync::Arc;

    async fn setup_volume_for_snapshot(extent_size: u64) -> (ThinVolumeHandle, Vec<String>) {
        let test_id = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::env::temp_dir().join("stormblock-snap-test");
        std::fs::create_dir_all(&dir).unwrap();

        let mut devices: Vec<Arc<dyn BlockDevice>> = Vec::new();
        let mut paths = Vec::new();
        for i in 0..2 {
            let path = dir.join(format!("{test_id}-member-{i}.bin"));
            let path_str = path.to_str().unwrap().to_string();
            let _ = std::fs::remove_file(&path);
            let dev = FileDevice::open_with_capacity(&path_str, 64 * 1024 * 1024).await.unwrap();
            devices.push(Arc::new(dev));
            paths.push(path_str);
        }

        let array = RaidArray::create(RaidLevel::Raid1, devices, None).await.unwrap();
        let array_id = array.array_id();
        let capacity = array.capacity_bytes();
        let backing: Arc<dyn BlockDevice> = Arc::new(array);

        let mut allocator = ExtentAllocator::new(extent_size);
        allocator.add_array(array_id, capacity);
        let allocator = Arc::new(tokio::sync::Mutex::new(allocator));

        let vol = ThinVolume::new(
            "source".to_string(),
            128 * 1024 * 1024,
            array_id,
            backing,
            allocator,
        );

        (ThinVolumeHandle::new(vol), paths)
    }

    fn cleanup(paths: &[String]) {
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }

    #[tokio::test]
    async fn snapshot_preserves_data() {
        let extent_size = 4096u64;
        let (handle, paths) = setup_volume_for_snapshot(extent_size).await;

        // Write data to source
        let data = vec![0xAA_u8; 4096];
        handle.write(0, &data).await.unwrap();

        // Take snapshot
        let snap_handle = {
            let mut vol = handle.lock().await;
            let snap = create_snapshot(&mut vol, "snap1");
            ThinVolumeHandle::new(snap)
        };

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
        let extent_size = 4096u64;
        let (handle, paths) = setup_volume_for_snapshot(extent_size).await;

        // Write initial data
        handle.write(0, &vec![0xAA_u8; 4096]).await.unwrap();
        handle.write(4096, &vec![0xBB_u8; 4096]).await.unwrap();

        // Take snapshot
        let snap_handle = {
            let mut vol = handle.lock().await;
            let snap = create_snapshot(&mut vol, "snap1");
            ThinVolumeHandle::new(snap)
        };

        // No changes yet — but after COW, extent 0 should differ
        handle.write(0, &vec![0xCC_u8; 4096]).await.unwrap();

        // Check diff
        let diff = {
            let src = handle.lock().await;
            let snap = snap_handle.lock().await;
            snapshot_diff(&src, &snap)
        };

        // Extent 0 was modified (COW'd), so it should be in the diff
        assert!(diff.contains(&0));
        // Extent 1 was not modified, should not be in diff
        assert!(!diff.contains(&1));

        cleanup(&paths);
    }

    #[tokio::test]
    async fn snapshot_delete_frees_unshared() {
        let extent_size = 4096u64;
        let (handle, paths) = setup_volume_for_snapshot(extent_size).await;

        handle.write(0, &vec![0xAA_u8; 4096]).await.unwrap();

        // Take snapshot, then COW source so they diverge
        let snap = {
            let mut vol = handle.lock().await;
            create_snapshot(&mut vol, "snap1")
        };

        // Write to source to trigger COW — now source has a new extent for vext 0
        handle.write(0, &vec![0xBB_u8; 4096]).await.unwrap();

        // Source COW'd: it now has a new physical extent for vext 0.
        // The snapshot still points to the old physical extent.
        // delete_snapshot should see that no other volume references the old extent and free it.
        let alloc = snap.allocator.clone();
        let array_id = snap.array_id;
        let free_before = {
            let a = alloc.lock().await;
            a.free_count(&array_id)
        };

        // Pass the source volume so delete_snapshot can check for shared extents
        {
            let src_vol = handle.lock().await;
            delete_snapshot(snap, &[&src_vol]).await;
        }

        let free_after = {
            let a = alloc.lock().await;
            a.free_count(&array_id)
        };

        assert!(free_after > free_before);
        cleanup(&paths);
    }
}
