//! Volume manager — thin provisioning, COW snapshots, extent allocator.
//!
//! The `VolumeManager` coordinates thin volumes on top of RAID arrays.
//! Each `ThinVolume` implements `BlockDevice`, so target protocols
//! (NVMe-oF, iSCSI) see volumes as plain block devices.

pub mod extent;
pub mod metadata;
pub mod thin;
pub mod snapshot;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::drive::BlockDevice;
use crate::raid::RaidArrayId;

pub use extent::{ExtentAllocator, VolumeId, DEFAULT_EXTENT_SIZE};
pub use metadata::MetadataStore;
pub use thin::{ThinVolume, ThinVolumeHandle, VolumeError};

/// Manages volumes, extent allocation, and snapshots across RAID arrays.
pub struct VolumeManager {
    volumes: HashMap<VolumeId, Arc<ThinVolumeHandle>>,
    allocator: Arc<tokio::sync::Mutex<ExtentAllocator>>,
    backing_devices: HashMap<RaidArrayId, Arc<dyn BlockDevice>>,
    metadata_store: Option<MetadataStore>,
}

impl VolumeManager {
    /// Create a new VolumeManager.
    pub fn new(extent_size: u64) -> Self {
        VolumeManager {
            volumes: HashMap::new(),
            allocator: Arc::new(tokio::sync::Mutex::new(ExtentAllocator::new(extent_size))),
            backing_devices: HashMap::new(),
            metadata_store: None,
        }
    }

    /// Create a VolumeManager with on-disk metadata persistence.
    pub fn with_data_dir(extent_size: u64, data_dir: PathBuf) -> std::io::Result<Self> {
        let store = MetadataStore::new(data_dir)?;
        Ok(VolumeManager {
            volumes: HashMap::new(),
            allocator: Arc::new(tokio::sync::Mutex::new(ExtentAllocator::new(extent_size))),
            backing_devices: HashMap::new(),
            metadata_store: Some(store),
        })
    }

    /// Register a RAID array as a backing device for volumes.
    pub async fn add_backing_device(&mut self, array_id: RaidArrayId, device: Arc<dyn BlockDevice>) {
        let capacity = device.capacity_bytes();
        {
            let mut alloc = self.allocator.lock().await;
            alloc.add_array(array_id, capacity);
        }
        self.backing_devices.insert(array_id, device);
    }

    /// Create a new thin volume on a specific RAID array.
    pub async fn create_volume(
        &mut self,
        name: &str,
        virtual_size: u64,
        array_id: RaidArrayId,
    ) -> Result<VolumeId, VolumeError> {
        let backing = self.backing_devices.get(&array_id)
            .ok_or_else(|| VolumeError::AllocatorError(
                format!("no backing device for array {array_id}")
            ))?
            .clone();

        let vol = ThinVolume::new(
            name.to_string(),
            virtual_size,
            array_id,
            backing,
            self.allocator.clone(),
        );
        let id = vol.id();
        let handle = Arc::new(ThinVolumeHandle::new(vol));
        self.volumes.insert(id, handle);
        self.persist().await;
        Ok(id)
    }

    /// Delete a volume, freeing extents not shared with other volumes.
    pub async fn delete_volume(&mut self, id: VolumeId) -> Result<(), VolumeError> {
        let handle = self.volumes.remove(&id)
            .ok_or(VolumeError::VolumeNotFound(id))?;

        // Collect locks on all remaining volumes to check for shared extents
        let mut other_guards = Vec::new();
        for other_handle in self.volumes.values() {
            other_guards.push(other_handle.lock().await);
        }
        let other_refs: Vec<&ThinVolume> = other_guards.iter().map(|g| &**g).collect();

        let vol = handle.lock().await;
        let mut alloc = self.allocator.lock().await;
        for (_vext_idx, pext) in &vol.extent_map {
            let still_referenced = other_refs.iter().any(|v| {
                v.extent_map.values().any(|other| {
                    other.array_id == pext.array_id && other.offset == pext.offset
                })
            });
            if !still_referenced {
                let ext = extent::Extent {
                    array_id: pext.array_id,
                    offset: pext.offset,
                    length: pext.length,
                };
                alloc.free(&ext);
            }
        }
        drop(alloc);
        drop(vol);
        drop(other_guards);
        self.persist().await;
        Ok(())
    }

    /// Resize a volume to `new_size` bytes.
    pub async fn resize_volume(&mut self, id: VolumeId, new_size: u64) -> Result<(), VolumeError> {
        if new_size == 0 {
            return Err(VolumeError::InvalidSize("size must be > 0".to_string()));
        }
        let handle = self.volumes.get(&id)
            .ok_or(VolumeError::VolumeNotFound(id))?
            .clone();
        handle.resize(new_size).await?;
        self.persist().await;
        Ok(())
    }

    /// Get a volume handle as a `BlockDevice` for target protocols.
    pub fn get_volume(&self, id: &VolumeId) -> Option<Arc<dyn BlockDevice>> {
        self.volumes.get(id).map(|h| h.clone() as Arc<dyn BlockDevice>)
    }

    /// Get a volume handle for management operations.
    pub fn get_volume_handle(&self, id: &VolumeId) -> Option<Arc<ThinVolumeHandle>> {
        self.volumes.get(id).cloned()
    }

    /// Create a snapshot of an existing volume.
    pub async fn create_snapshot(
        &mut self,
        source_id: VolumeId,
        name: &str,
    ) -> Result<VolumeId, VolumeError> {
        let source_handle = self.volumes.get(&source_id)
            .ok_or(VolumeError::VolumeNotFound(source_id))?
            .clone();

        let snap = {
            let mut vol = source_handle.lock().await;
            snapshot::create_snapshot(&mut vol, name)
        };
        let snap_id = snap.id();
        let snap_handle = Arc::new(ThinVolumeHandle::new(snap));
        self.volumes.insert(snap_id, snap_handle);
        self.persist().await;
        Ok(snap_id)
    }

    /// List all volumes: (id, name, virtual_size, allocated).
    pub async fn list_volumes(&self) -> Vec<(VolumeId, String, u64, u64)> {
        let mut list = Vec::with_capacity(self.volumes.len());
        for (id, handle) in &self.volumes {
            let name = handle.name().await;
            let allocated = handle.allocated().await;
            list.push((*id, name, handle.capacity_bytes(), allocated));
        }
        list
    }

    /// Get the allocator for direct inspection.
    pub fn allocator(&self) -> &Arc<tokio::sync::Mutex<ExtentAllocator>> {
        &self.allocator
    }

    /// Persist all volume metadata to disk. No-op if no data_dir configured.
    pub async fn persist(&self) {
        let store = match &self.metadata_store {
            Some(s) => s,
            None => return,
        };

        let alloc = self.allocator.lock().await;
        let extent_size = alloc.extent_size();

        let mut arrays = Vec::new();
        for (array_id, device) in &self.backing_devices {
            arrays.push(metadata::ArrayRecord {
                array_id: *array_id,
                total_capacity: device.capacity_bytes(),
            });
        }

        let mut volumes = Vec::new();
        for (_id, handle) in &self.volumes {
            let vol = handle.lock().await;
            volumes.push(metadata::VolumeRecord {
                id: vol.id,
                name: vol.name.clone(),
                virtual_size: vol.virtual_size,
                array_id: vol.array_id,
                extent_map: vol.extent_map.clone(),
            });
        }
        drop(alloc);

        let meta = metadata::VolumeMetadata {
            extent_size,
            arrays,
            volumes,
        };

        if let Err(e) = store.save(&meta) {
            tracing::error!("Failed to persist volume metadata: {e}");
        }
    }

    /// Restore volumes from persisted metadata. No-op if no data_dir or no metadata file.
    pub async fn restore(&mut self) -> anyhow::Result<()> {
        let store = match &self.metadata_store {
            Some(s) => s,
            None => return Ok(()),
        };

        if !store.exists() {
            tracing::info!("No persisted metadata found, starting fresh");
            return Ok(());
        }

        let meta = store.load()?;

        // Verify arrays exist
        for arec in &meta.arrays {
            if !self.backing_devices.contains_key(&arec.array_id) {
                tracing::warn!(
                    "Persisted array {} not found in current backing devices, skipping its volumes",
                    arec.array_id
                );
            }
        }

        let mut restored = 0u32;
        for vrec in meta.volumes {
            let backing = match self.backing_devices.get(&vrec.array_id) {
                Some(dev) => dev.clone(),
                None => {
                    tracing::warn!(
                        "Skipping volume '{}' ({}): array {} not available",
                        vrec.name, vrec.id, vrec.array_id
                    );
                    continue;
                }
            };

            // Mark extents as allocated in the bitmap
            {
                let mut alloc = self.allocator.lock().await;
                for pext in vrec.extent_map.values() {
                    alloc.mark_allocated(pext.array_id, pext.offset);
                }
            }

            let vol = ThinVolume::restore(
                vrec.id,
                vrec.name.clone(),
                vrec.virtual_size,
                vrec.array_id,
                vrec.extent_map,
                backing,
                self.allocator.clone(),
            );
            let handle = Arc::new(ThinVolumeHandle::new(vol));
            self.volumes.insert(vrec.id, handle);
            restored += 1;
            tracing::info!("Restored volume '{}' ({})", vrec.name, vrec.id);
        }

        tracing::info!("Restored {restored} volume(s) from metadata");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;
    use crate::raid::{RaidArray, RaidLevel};

    async fn create_test_array() -> (RaidArrayId, Arc<dyn BlockDevice>, Vec<String>) {
        let test_id = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::env::temp_dir().join("stormblock-volmgr-test");
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
        let backing: Arc<dyn BlockDevice> = Arc::new(array);
        (array_id, backing, paths)
    }

    fn cleanup(paths: &[String]) {
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }

    #[tokio::test]
    async fn volume_manager_create_and_list() {
        let (array_id, backing, paths) = create_test_array().await;

        let mut mgr = VolumeManager::new(DEFAULT_EXTENT_SIZE);
        mgr.add_backing_device(array_id, backing).await;

        let vol_id = mgr.create_volume("data", 100 * 1024 * 1024, array_id).await.unwrap();
        let list = mgr.list_volumes().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, vol_id);
        assert_eq!(list[0].1, "data");
        assert_eq!(list[0].2, 100 * 1024 * 1024);
        assert_eq!(list[0].3, 0); // No data written yet

        cleanup(&paths);
    }

    #[tokio::test]
    async fn volume_manager_write_read_roundtrip() {
        let (array_id, backing, paths) = create_test_array().await;

        let mut mgr = VolumeManager::new(DEFAULT_EXTENT_SIZE);
        mgr.add_backing_device(array_id, backing).await;

        let vol_id = mgr.create_volume("data", 100 * 1024 * 1024, array_id).await.unwrap();
        let vol = mgr.get_volume(&vol_id).unwrap();

        // Write
        let data = vec![0xDE_u8; 4096];
        vol.write(0, &data).await.unwrap();

        // Read
        let mut buf = vec![0u8; 4096];
        vol.read(0, &mut buf).await.unwrap();
        assert_eq!(buf, data);

        cleanup(&paths);
    }

    #[tokio::test]
    async fn volume_manager_snapshot_roundtrip() {
        let (array_id, backing, paths) = create_test_array().await;

        let mut mgr = VolumeManager::new(4096); // Small extents
        mgr.add_backing_device(array_id, backing).await;

        let vol_id = mgr.create_volume("data", 100 * 1024 * 1024, array_id).await.unwrap();

        // Write data
        let vol = mgr.get_volume(&vol_id).unwrap();
        vol.write(0, &vec![0xAA_u8; 4096]).await.unwrap();

        // Snapshot
        let snap_id = mgr.create_snapshot(vol_id, "snap1").await.unwrap();

        // Write new data to source
        vol.write(0, &vec![0xBB_u8; 4096]).await.unwrap();

        // Source has new data
        let mut src_buf = vec![0u8; 4096];
        vol.read(0, &mut src_buf).await.unwrap();
        assert!(src_buf.iter().all(|&b| b == 0xBB));

        // Snapshot has old data
        let snap = mgr.get_volume(&snap_id).unwrap();
        let mut snap_buf = vec![0u8; 4096];
        snap.read(0, &mut snap_buf).await.unwrap();
        assert!(snap_buf.iter().all(|&b| b == 0xAA));

        cleanup(&paths);
    }

    #[tokio::test]
    async fn volume_manager_delete() {
        let (array_id, backing, paths) = create_test_array().await;

        let mut mgr = VolumeManager::new(DEFAULT_EXTENT_SIZE);
        mgr.add_backing_device(array_id, backing).await;

        let vol_id = mgr.create_volume("to-delete", 50 * 1024 * 1024, array_id).await.unwrap();

        // Write something to allocate extents
        let vol = mgr.get_volume(&vol_id).unwrap();
        vol.write(0, &vec![0xFF_u8; 4096]).await.unwrap();
        drop(vol);

        // Delete
        mgr.delete_volume(vol_id).await.unwrap();
        assert!(mgr.get_volume(&vol_id).is_none());

        // Verify: deleting again should fail
        assert!(mgr.delete_volume(vol_id).await.is_err());

        cleanup(&paths);
    }

    #[tokio::test]
    async fn volume_manager_resize_grow() {
        let (array_id, backing, paths) = create_test_array().await;

        let mut mgr = VolumeManager::new(DEFAULT_EXTENT_SIZE);
        mgr.add_backing_device(array_id, backing).await;

        let vol_id = mgr.create_volume("resize-grow", 50 * 1024 * 1024, array_id).await.unwrap();
        mgr.resize_volume(vol_id, 100 * 1024 * 1024).await.unwrap();

        // Verify new size is reflected
        let list = mgr.list_volumes().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].2, 100 * 1024 * 1024);

        // Write at offset beyond old boundary
        let vol = mgr.get_volume(&vol_id).unwrap();
        let data = vec![0xCD_u8; 4096];
        vol.write(60 * 1024 * 1024, &data).await.unwrap();

        let mut buf = vec![0u8; 4096];
        vol.read(60 * 1024 * 1024, &mut buf).await.unwrap();
        assert_eq!(buf, data);

        cleanup(&paths);
    }

    #[tokio::test]
    async fn volume_manager_resize_shrink() {
        let (array_id, backing, paths) = create_test_array().await;

        let mut mgr = VolumeManager::new(4096); // Small extents
        mgr.add_backing_device(array_id, backing).await;

        let vol_id = mgr.create_volume("resize-shrink", 100 * 1024 * 1024, array_id).await.unwrap();
        let vol = mgr.get_volume(&vol_id).unwrap();

        // Write data at offset 0 (within future boundary)
        let data_low = vec![0xAA_u8; 4096];
        vol.write(0, &data_low).await.unwrap();

        // Write data beyond future boundary
        vol.write(60 * 1024 * 1024, &vec![0xBB_u8; 4096]).await.unwrap();

        // Check extents allocated
        let handle = mgr.get_volume_handle(&vol_id).unwrap();
        let extents_before = handle.extent_count().await;
        assert_eq!(extents_before, 2);

        // Resize down to 50 MB — extent at 60 MB should be freed
        mgr.resize_volume(vol_id, 50 * 1024 * 1024).await.unwrap();

        let extents_after = handle.extent_count().await;
        assert_eq!(extents_after, 1); // Only the extent at offset 0 remains

        // Data at offset 0 still intact
        let mut buf = vec![0u8; 4096];
        vol.read(0, &mut buf).await.unwrap();
        assert_eq!(buf, data_low);

        cleanup(&paths);
    }

    #[tokio::test]
    async fn volume_manager_resize_zero_rejected() {
        let (array_id, backing, paths) = create_test_array().await;

        let mut mgr = VolumeManager::new(DEFAULT_EXTENT_SIZE);
        mgr.add_backing_device(array_id, backing).await;

        let vol_id = mgr.create_volume("no-zero", 50 * 1024 * 1024, array_id).await.unwrap();

        let result = mgr.resize_volume(vol_id, 0).await;
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("size must be > 0"));

        cleanup(&paths);
    }
}
