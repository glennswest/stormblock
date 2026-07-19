//! Volume manager — thin provisioning, COW snapshots, slab-based allocation.
//!
//! The `VolumeManager` coordinates thin volumes on top of slab-backed storage.
//! Each `ThinVolume` implements `BlockDevice`, so target protocols
//! (NVMe-oF, iSCSI) see volumes as plain block devices.

pub mod extent;
pub mod gem;
pub mod metadata;
pub mod thin;
pub mod snapshot;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::drive::BlockDevice;
use crate::drive::slab::{Slab, SlabId};
use crate::drive::slab_registry::SlabRegistry;
use crate::placement::topology::StorageTier;
use crate::raid::RaidArrayId;

pub use extent::{ExtentAllocator, VolumeId, DEFAULT_EXTENT_SIZE};
pub use metadata::MetadataStore;
pub use thin::{ThinVolume, ThinVolumeHandle, VolumeError, PlacementPolicy};
pub use gem::GlobalExtentMap;

/// Default slot size for slabs created via add_backing_device.
pub const DEFAULT_SLOT_SIZE: u64 = crate::drive::slab::DEFAULT_SLOT_SIZE;

/// Manages volumes, slab allocation, and snapshots.
pub struct VolumeManager {
    gem: Arc<tokio::sync::Mutex<GlobalExtentMap>>,
    registry: Arc<tokio::sync::Mutex<SlabRegistry>>,
    volumes: HashMap<VolumeId, Arc<ThinVolumeHandle>>,
    /// Legacy mapping: array_id → slab_id (for backward compat with callers
    /// that pass array_id to create_volume).
    array_slabs: HashMap<RaidArrayId, SlabId>,
    slot_size: u64,
    metadata_store: Option<MetadataStore>,
}

impl VolumeManager {
    /// Create a new VolumeManager.
    ///
    /// `slot_size` is the slab slot size (typically 1 MB for production,
    /// smaller values like 4096 for tests).
    pub fn new(slot_size: u64) -> Self {
        VolumeManager {
            gem: Arc::new(tokio::sync::Mutex::new(GlobalExtentMap::new())),
            registry: Arc::new(tokio::sync::Mutex::new(SlabRegistry::new())),
            volumes: HashMap::new(),
            array_slabs: HashMap::new(),
            slot_size,
            metadata_store: None,
        }
    }

    /// Create a VolumeManager with on-disk metadata persistence.
    pub fn with_data_dir(slot_size: u64, data_dir: PathBuf) -> std::io::Result<Self> {
        let store = MetadataStore::new(data_dir)?;
        Ok(VolumeManager {
            gem: Arc::new(tokio::sync::Mutex::new(GlobalExtentMap::new())),
            registry: Arc::new(tokio::sync::Mutex::new(SlabRegistry::new())),
            volumes: HashMap::new(),
            array_slabs: HashMap::new(),
            slot_size,
            metadata_store: Some(store),
        })
    }

    /// Register a RAID array as a backing device for volumes.
    ///
    /// Formats a slab on the device and registers it in the slab registry.
    /// The `array_id` is kept for backward compatibility with callers that
    /// reference arrays by ID.
    pub async fn add_backing_device(
        &mut self,
        array_id: RaidArrayId,
        device: Arc<dyn BlockDevice>,
    ) {
        let slab = match Slab::format(device, self.slot_size, StorageTier::Hot).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to format slab on array {array_id}: {e}");
                return;
            }
        };
        let slab_id = slab.slab_id();
        {
            let mut reg = self.registry.lock().await;
            reg.add(slab);
        }
        self.array_slabs.insert(array_id, slab_id);
        tracing::info!("Registered array {array_id} as slab {}", slab_id.0);
    }

    /// Register a pre-formatted slab directly.
    pub async fn add_slab(&mut self, slab: Slab) {
        let id = slab.slab_id();
        let mut reg = self.registry.lock().await;
        reg.add(slab);
        tracing::info!("Registered slab {}", id.0);
    }

    /// Attach an **existing** slab-formatted device without reformatting.
    ///
    /// Counterpart to `add_backing_device` for the reboot / boot-artifact
    /// path: opens the slab (header + slot table) from the device and
    /// registers it under `array_id`, so `restore()` can resolve volumes
    /// that reference that array. Errors instead of logging — the caller
    /// (initramfs, artifact consumer) must know the attach failed.
    pub async fn open_backing_device(
        &mut self,
        array_id: RaidArrayId,
        device: Arc<dyn BlockDevice>,
    ) -> Result<(), VolumeError> {
        let slab = Slab::open(device).await.map_err(VolumeError::Drive)?;
        if slab.slot_size() != self.slot_size {
            return Err(VolumeError::InvalidSize(format!(
                "slab slot size {} does not match manager slot size {}",
                slab.slot_size(),
                self.slot_size,
            )));
        }
        let slab_id = slab.slab_id();
        {
            let mut reg = self.registry.lock().await;
            reg.add(slab);
        }
        self.array_slabs.insert(array_id, slab_id);
        tracing::info!("Opened array {array_id} as existing slab {}", slab_id.0);
        Ok(())
    }

    /// Create a new thin volume on a specific RAID array.
    ///
    /// The `array_id` parameter maps to a slab for placement preference.
    /// The volume can allocate from any slab if the preferred one is full.
    pub async fn create_volume(
        &mut self,
        name: &str,
        virtual_size: u64,
        array_id: RaidArrayId,
    ) -> Result<VolumeId, VolumeError> {
        if !self.array_slabs.contains_key(&array_id) {
            return Err(VolumeError::AllocatorError(
                format!("no backing device for array {array_id}")
            ));
        }

        let vol = ThinVolume::new(name.to_string(), virtual_size, self.slot_size);
        let id = vol.id();
        let handle = Arc::new(ThinVolumeHandle::new(
            vol,
            self.gem.clone(),
            self.registry.clone(),
            PlacementPolicy::default(),
        ));
        self.volumes.insert(id, handle);
        self.persist().await;
        Ok(id)
    }

    /// Create a new thin volume without binding it to a specific array.
    ///
    /// Slab placement happens at write time via the registry, so the volume
    /// can allocate from any registered slab. Used by the /v1 management
    /// surface where placement is expressed in nodes, not arrays.
    pub async fn create_volume_any(
        &mut self,
        name: &str,
        virtual_size: u64,
    ) -> Result<VolumeId, VolumeError> {
        let vol = ThinVolume::new(name.to_string(), virtual_size, self.slot_size);
        let id = vol.id();
        let handle = Arc::new(ThinVolumeHandle::new(
            vol,
            self.gem.clone(),
            self.registry.clone(),
            PlacementPolicy::default(),
        ));
        self.volumes.insert(id, handle);
        self.persist().await;
        Ok(id)
    }

    /// Snapshot several volumes at a single consistency point.
    ///
    /// Holds the GEM and slab-registry locks across every member clone, so
    /// no write can allocate or COW between the first and last snapshot —
    /// this is the single fence VolumeGroupSnapshot semantics require.
    pub async fn create_snapshots_atomic(
        &mut self,
        sources: &[(VolumeId, String)],
    ) -> Result<Vec<VolumeId>, VolumeError> {
        let mut params = Vec::with_capacity(sources.len());
        for (source_id, name) in sources {
            let handle = self.volumes.get(source_id)
                .ok_or(VolumeError::VolumeNotFound(*source_id))?
                .clone();
            let vol = handle.lock().await;
            params.push((*source_id, name.clone(), vol.virtual_size, vol.slot_size));
        }

        let mut snaps = Vec::with_capacity(sources.len());
        {
            let mut gem = self.gem.lock().await;
            let mut reg = self.registry.lock().await;
            for (source_id, name, virtual_size, slot_size) in &params {
                let snap = snapshot::create_snapshot(
                    *source_id, name, *virtual_size, *slot_size,
                    &mut gem, &mut reg,
                ).await?;
                snaps.push(snap);
            }
        }

        let mut ids = Vec::with_capacity(snaps.len());
        for snap in snaps {
            let snap_id = snap.id();
            let handle = Arc::new(ThinVolumeHandle::new(
                snap,
                self.gem.clone(),
                self.registry.clone(),
                PlacementPolicy::default(),
            ));
            self.volumes.insert(snap_id, handle);
            ids.push(snap_id);
        }
        self.persist().await;
        Ok(ids)
    }

    /// Delete a volume, freeing all slab slots.
    pub async fn delete_volume(&mut self, id: VolumeId) -> Result<(), VolumeError> {
        let _handle = self.volumes.remove(&id)
            .ok_or(VolumeError::VolumeNotFound(id))?;

        // Remove all extents from GEM and dec_ref on slabs
        let mut gem = self.gem.lock().await;
        let mut reg = self.registry.lock().await;
        snapshot::delete_snapshot(id, &mut gem, &mut reg).await?;
        drop(gem);
        drop(reg);

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
        let source_vol = source_handle.lock().await;
        let virtual_size = source_vol.virtual_size;
        let slot_size = source_vol.slot_size;
        drop(source_vol);

        let snap = {
            let mut gem = self.gem.lock().await;
            let mut reg = self.registry.lock().await;
            snapshot::create_snapshot(
                source_id, name, virtual_size, slot_size,
                &mut gem, &mut reg,
            ).await?
        };
        let snap_id = snap.id();
        let snap_handle = Arc::new(ThinVolumeHandle::new(
            snap,
            self.gem.clone(),
            self.registry.clone(),
            PlacementPolicy::default(),
        ));
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

    /// Get the shared GEM.
    pub fn gem(&self) -> &Arc<tokio::sync::Mutex<GlobalExtentMap>> {
        &self.gem
    }

    /// Get the shared SlabRegistry.
    pub fn registry(&self) -> &Arc<tokio::sync::Mutex<SlabRegistry>> {
        &self.registry
    }

    /// Persist all volume metadata to disk, including each volume's extent
    /// map. No-op if no data_dir configured.
    ///
    /// The extent maps are the piece slab slot tables cannot reconstruct: a
    /// COW snapshot's shared slots are recorded under the original writer,
    /// so without this file a snapshot reads as zeros after reattach (#13).
    pub async fn persist(&self) {
        let store = match &self.metadata_store {
            Some(s) => s,
            None => return,
        };

        // Gather per-volume info before taking gem/registry locks so we never
        // hold them across a volume-handle await (I/O paths lock the volume
        // first, then gem/registry).
        let mut vol_info = Vec::with_capacity(self.volumes.len());
        for (id, handle) in &self.volumes {
            vol_info.push((*id, handle.name().await, handle.capacity_bytes()));
        }

        let meta = {
            let gem = self.gem.lock().await;
            let reg = self.registry.lock().await;
            let arrays = self
                .array_slabs
                .iter()
                .map(|(array_id, slab_id)| metadata::ArrayRecord {
                    array_id: *array_id,
                    total_capacity: reg
                        .get(slab_id)
                        .map(|s| s.total_slots() * s.slot_size())
                        .unwrap_or(0),
                })
                .collect();
            let volumes = vol_info
                .into_iter()
                .map(|(id, name, virtual_size)| metadata::VolumeRecord {
                    id,
                    name,
                    virtual_size,
                    array_id: None,
                    extents: gem
                        .get_volume_map(&id)
                        .map(|m| m.extents.clone())
                        .unwrap_or_default(),
                })
                .collect();
            metadata::VolumeMetadata {
                extent_size: self.slot_size,
                arrays,
                volumes,
            }
        };

        if let Err(e) = store.save(&meta) {
            tracing::warn!("Volume metadata persist failed: {e}");
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

        // Rebuild GEM from slab slot tables — authoritative for owned and
        // COW'd slots (written at allocation time, so always at least as new
        // as the metadata file after a crash).
        let mut rebuilt = {
            let reg = self.registry.lock().await;
            GlobalExtentMap::rebuild_from_slabs(reg.iter())
        };

        let mut restored = 0u32;
        for vrec in meta.volumes {
            // Legacy V1 records bind volumes to arrays; skip if that array
            // isn't attached. V2 slab-placed records restore regardless.
            if let Some(array_id) = vrec.array_id {
                if !self.array_slabs.contains_key(&array_id) {
                    tracing::warn!(
                        "Skipping volume '{}' ({}): array {} not available",
                        vrec.name, vrec.id, array_id
                    );
                    continue;
                }
            }

            // Overlay persisted extents the slot tables can't express (a
            // snapshot's shared slots). Slot-table mappings win on conflict;
            // persisted mappings fill the gaps. (#13)
            {
                let reg = self.registry.lock().await;
                for (vext, loc) in &vrec.extents {
                    if rebuilt.lookup(vrec.id, *vext).is_none() {
                        if reg.get(&loc.slab_id).is_some() {
                            rebuilt.restore_mapping(vrec.id, *vext, loc.clone());
                        } else {
                            tracing::warn!(
                                "Volume '{}' extent {vext}: slab {} not attached, mapping dropped",
                                vrec.name, loc.slab_id.0
                            );
                        }
                    }
                }
            }

            let vol = ThinVolume::restore(
                vrec.id,
                vrec.name.clone(),
                vrec.virtual_size,
                self.slot_size,
            );
            let handle = Arc::new(ThinVolumeHandle::new(
                vol,
                self.gem.clone(),
                self.registry.clone(),
                PlacementPolicy::default(),
            ));
            self.volumes.insert(vrec.id, handle);
            restored += 1;
            tracing::info!("Restored volume '{}' ({})", vrec.name, vrec.id);
        }

        *self.gem.lock().await = rebuilt;

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
            let dev = FileDevice::open_with_capacity(&path_str, 64 * 1024 * 1024)
                .await
                .unwrap();
            devices.push(Arc::new(dev));
            paths.push(path_str);
        }

        let array = RaidArray::create(RaidLevel::Raid1, devices, None)
            .await
            .unwrap();
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

        let mut mgr = VolumeManager::new(4096);
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

        let mut mgr = VolumeManager::new(4096);
        mgr.add_backing_device(array_id, backing).await;

        let vol_id = mgr.create_volume("data", 100 * 1024 * 1024, array_id).await.unwrap();
        let vol = mgr.get_volume(&vol_id).unwrap();

        let data = vec![0xDE_u8; 4096];
        vol.write(0, &data).await.unwrap();

        let mut buf = vec![0u8; 4096];
        vol.read(0, &mut buf).await.unwrap();
        assert_eq!(buf, data);

        cleanup(&paths);
    }

    #[tokio::test]
    async fn volume_manager_snapshot_roundtrip() {
        let (array_id, backing, paths) = create_test_array().await;

        let mut mgr = VolumeManager::new(4096);
        mgr.add_backing_device(array_id, backing).await;

        let vol_id = mgr.create_volume("data", 100 * 1024 * 1024, array_id).await.unwrap();
        let vol = mgr.get_volume(&vol_id).unwrap();
        vol.write(0, &vec![0xAA_u8; 4096]).await.unwrap();

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

        let mut mgr = VolumeManager::new(4096);
        mgr.add_backing_device(array_id, backing).await;

        let vol_id = mgr.create_volume("to-delete", 50 * 1024 * 1024, array_id).await.unwrap();
        let vol = mgr.get_volume(&vol_id).unwrap();
        vol.write(0, &vec![0xFF_u8; 4096]).await.unwrap();
        drop(vol);

        mgr.delete_volume(vol_id).await.unwrap();
        assert!(mgr.get_volume(&vol_id).is_none());
        assert!(mgr.delete_volume(vol_id).await.is_err());

        cleanup(&paths);
    }

    #[tokio::test]
    async fn volume_manager_resize_grow() {
        let (array_id, backing, paths) = create_test_array().await;

        let mut mgr = VolumeManager::new(4096);
        mgr.add_backing_device(array_id, backing).await;

        let vol_id = mgr.create_volume("resize-grow", 50 * 1024 * 1024, array_id).await.unwrap();
        mgr.resize_volume(vol_id, 100 * 1024 * 1024).await.unwrap();

        let list = mgr.list_volumes().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].2, 100 * 1024 * 1024);

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

        let mut mgr = VolumeManager::new(4096);
        mgr.add_backing_device(array_id, backing).await;

        let vol_id = mgr.create_volume("resize-shrink", 100 * 1024 * 1024, array_id).await.unwrap();
        let vol = mgr.get_volume(&vol_id).unwrap();

        let data_low = vec![0xAA_u8; 4096];
        vol.write(0, &data_low).await.unwrap();
        vol.write(60 * 1024 * 1024, &vec![0xBB_u8; 4096]).await.unwrap();

        let handle = mgr.get_volume_handle(&vol_id).unwrap();
        let extents_before = handle.extent_count().await;
        assert_eq!(extents_before, 2);

        mgr.resize_volume(vol_id, 50 * 1024 * 1024).await.unwrap();

        let extents_after = handle.extent_count().await;
        assert_eq!(extents_after, 1);

        let mut buf = vec![0u8; 4096];
        vol.read(0, &mut buf).await.unwrap();
        assert_eq!(buf, data_low);

        cleanup(&paths);
    }

    #[tokio::test]
    async fn volume_manager_resize_zero_rejected() {
        let (array_id, backing, paths) = create_test_array().await;

        let mut mgr = VolumeManager::new(4096);
        mgr.add_backing_device(array_id, backing).await;

        let vol_id = mgr.create_volume("no-zero", 50 * 1024 * 1024, array_id).await.unwrap();
        let result = mgr.resize_volume(vol_id, 0).await;
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("size must be > 0"));

        cleanup(&paths);
    }

    /// The boot-artifact / reboot path: build a slab + volume in one manager,
    /// reattach the same backing file in a fresh manager via
    /// open_backing_device (no reformat), restore metadata, read data back.
    #[tokio::test]
    async fn open_backing_device_restores_existing_volume() {
        let test_id = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::env::temp_dir().join("stormblock-volmgr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let backing_path = dir.join(format!("{test_id}-reopen.bin"));
        let backing_str = backing_path.to_str().unwrap().to_string();
        let _ = std::fs::remove_file(&backing_path);
        let meta_dir = dir.join(format!("{test_id}-meta"));

        let array_id = RaidArrayId(uuid::Uuid::new_v4());
        let data: Vec<u8> = (0..2 * 1024 * 1024 + 331).map(|i| (i % 249) as u8).collect();

        // Phase 1: create, write, persist metadata.
        let vol_id = {
            let dev = FileDevice::open_with_capacity(&backing_str, 64 * 1024 * 1024)
                .await
                .unwrap();
            let mut mgr = VolumeManager::with_data_dir(4096, meta_dir.clone()).unwrap();
            mgr.add_backing_device(array_id, Arc::new(dev)).await;
            let vol_id = mgr
                .create_volume("reopen-me", data.len() as u64, array_id)
                .await
                .unwrap();
            let vol = mgr.get_volume(&vol_id).unwrap();
            let mut off = 0usize;
            while off < data.len() {
                let n = vol.write(off as u64, &data[off..]).await.unwrap();
                assert!(n > 0);
                off += n;
            }
            vol.flush().await.unwrap();
            mgr.persist().await;
            vol_id
        };

        // Phase 2: fresh manager, attach WITHOUT reformatting, restore, read.
        let dev = FileDevice::open(&backing_str).await.unwrap();
        let mut mgr = VolumeManager::with_data_dir(4096, meta_dir.clone()).unwrap();
        mgr.open_backing_device(array_id, Arc::new(dev))
            .await
            .unwrap();
        mgr.restore().await.unwrap();

        let vol = mgr
            .get_volume(&vol_id)
            .expect("volume restored from metadata");
        let mut got = vec![0u8; data.len()];
        let mut off = 0usize;
        while off < got.len() {
            let end = got.len();
            let n = vol.read(off as u64, &mut got[off..end]).await.unwrap();
            assert!(n > 0);
            off += n;
        }
        assert_eq!(got, data, "restored volume content differs");

        // Slot-size mismatch must be rejected, not silently misread.
        let dev = FileDevice::open(&backing_str).await.unwrap();
        let mut wrong = VolumeManager::new(8192);
        assert!(wrong
            .open_backing_device(array_id, Arc::new(dev))
            .await
            .is_err());

        let _ = std::fs::remove_file(&backing_path);
        let _ = std::fs::remove_dir_all(&meta_dir);
    }

    /// Issue #13: a COW snapshot must survive detach/reattach with its FULL
    /// content intact — including the shared (never-COW'd) extents that only
    /// exist in the persisted extent map, not in slab slot tables. The parent
    /// diverges after the snapshot, so any mapping confusion shows up as the
    /// snapshot reading the parent's new data (or zeros).
    #[tokio::test]
    async fn snapshot_full_content_survives_reattach() {
        let test_id = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::env::temp_dir().join("stormblock-volmgr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let backing_path = dir.join(format!("{test_id}-snap-reattach.bin"));
        let backing_str = backing_path.to_str().unwrap().to_string();
        let _ = std::fs::remove_file(&backing_path);
        let meta_dir = dir.join(format!("{test_id}-snap-meta"));

        let array_id = RaidArrayId(uuid::Uuid::new_v4());
        // Multiple extents, deterministic per-byte pattern.
        let golden: Vec<u8> = (0..3 * 4096 + 777).map(|i| (i % 251) as u8).collect();

        // Phase 1: create parent, write golden, snapshot, diverge parent.
        let (parent_id, snap_id) = {
            let dev = FileDevice::open_with_capacity(&backing_str, 64 * 1024 * 1024)
                .await
                .unwrap();
            let mut mgr = VolumeManager::with_data_dir(4096, meta_dir.clone()).unwrap();
            mgr.add_backing_device(array_id, Arc::new(dev)).await;
            let parent_id = mgr
                .create_volume("golden", golden.len() as u64, array_id)
                .await
                .unwrap();
            let vol = mgr.get_volume(&parent_id).unwrap();
            let mut off = 0;
            while off < golden.len() {
                off += vol.write(off as u64, &golden[off..]).await.unwrap();
            }
            let snap_id = mgr.create_snapshot(parent_id, "snap-cp-01").await.unwrap();

            // Diverge the parent AFTER the snapshot (COW moves the parent to
            // new slots; the snapshot keeps the originals).
            vol.write(0, &vec![0xEE_u8; 4096]).await.unwrap();
            vol.flush().await.unwrap();
            mgr.persist().await;
            (parent_id, snap_id)
        };

        // Phase 2: fresh manager — attach without reformat, restore, verify.
        let dev = FileDevice::open(&backing_str).await.unwrap();
        let mut mgr = VolumeManager::with_data_dir(4096, meta_dir.clone()).unwrap();
        mgr.open_backing_device(array_id, Arc::new(dev)).await.unwrap();
        mgr.restore().await.unwrap();

        let snap = mgr.get_volume(&snap_id).expect("snapshot restored");
        let mut got = vec![0u8; golden.len()];
        let mut off = 0;
        while off < got.len() {
            let end = got.len();
            let n = snap.read(off as u64, &mut got[off..end]).await.unwrap();
            assert!(n > 0);
            off += n;
        }
        assert_eq!(got, golden, "snapshot content diverged after reattach (#13)");

        // Parent kept its post-snapshot write.
        let parent = mgr.get_volume(&parent_id).expect("parent restored");
        let mut head = vec![0u8; 4096];
        parent.read(0, &mut head).await.unwrap();
        assert!(head.iter().all(|&b| b == 0xEE), "parent lost its divergent write");
        // And the rest of the parent still matches golden.
        let mut tail = vec![0u8; golden.len() - 4096];
        let mut off = 0;
        while off < tail.len() {
            let end = tail.len();
            let n = parent.read((4096 + off) as u64, &mut tail[off..end]).await.unwrap();
            assert!(n > 0);
            off += n;
        }
        assert_eq!(tail, golden[4096..], "parent unshared content corrupted");

        let _ = std::fs::remove_file(&backing_path);
        let _ = std::fs::remove_dir_all(&meta_dir);
    }
}
