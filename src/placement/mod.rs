//! Placement engine — extent-level data placement across storage domains.
//!
//! The placement system manages where data lives. Instead of static RAID arrays
//! where volumes are bound to a fixed set of drives, the placement engine
//! treats each extent independently: data flows toward compute (attraction),
//! replicates for safety, tiers by temperature, and adjusts as devices come
//! and go.
//!
//! # Foundation: Snapshot-Fenced Cold Copies
//!
//! The correctness guarantee starts with cold copies. A cold copy converges to
//! a specific **snapshot** — not to live data. Every extent in the cold copy
//! matches the same point in time. This is the invariant that makes everything
//! else safe: you can't have an inconsistent replica.
//!
//! # Data Flow Model
//!
//! ```text
//! Remote iSCSI ──attraction──▶ Local NVMe (hot, close to CPU)
//!                                    │
//!                              safety replication
//!                                    │
//!                                    ▼
//!                              Local SAS (warm)
//!                                    │
//!                               cold backup
//!                                    │
//!                                    ▼
//!                              Remote archive (cold)
//! ```
//!
//! Each flow is a snapshot-fenced replication stream. The live volume is
//! unaware — all I/O goes through the BlockDevice trait (via ublk), and
//! the placement engine handles replication in the background.

pub mod topology;
pub mod cold;

use std::collections::HashMap;
use std::sync::Arc;

use uuid::Uuid;

use crate::drive::BlockDevice;
use crate::volume::extent::VolumeId;
use crate::volume::snapshot::snapshot_diff;
use crate::volume::thin::ThinVolumeHandle;

pub use topology::{StorageTier, Locality, StorageDevice};
pub use cold::{ColdCopy, ReplicationResult, ReplicationError};

/// The placement engine — manages cold copies and replication streams.
///
/// Currently focused on snapshot-fenced cold copies. Future iterations
/// will add: device attraction (hot migration toward compute), organic
/// RAID level transitions, and tiered data flow.
pub struct PlacementEngine {
    /// Available storage devices, indexed by ID.
    devices: HashMap<Uuid, StorageDevice>,
    /// Cold copies, indexed by cold copy ID.
    cold_copies: HashMap<Uuid, ColdCopy>,
    /// Volume → list of cold copy IDs.
    volume_copies: HashMap<VolumeId, Vec<Uuid>>,
}

impl PlacementEngine {
    /// Create a new placement engine.
    pub fn new() -> Self {
        PlacementEngine {
            devices: HashMap::new(),
            cold_copies: HashMap::new(),
            volume_copies: HashMap::new(),
        }
    }

    /// Register a storage device with the placement engine.
    pub fn add_device(&mut self, device: StorageDevice) -> Uuid {
        let id = device.id;
        tracing::info!("placement: registered device {}", device);
        self.devices.insert(id, device);
        id
    }

    /// Remove a storage device.
    pub fn remove_device(&mut self, id: Uuid) -> Option<StorageDevice> {
        let dev = self.devices.remove(&id);
        if let Some(ref d) = dev {
            tracing::info!("placement: removed device {}", d.name);
        }
        dev
    }

    /// List all registered devices.
    pub fn devices(&self) -> impl Iterator<Item = &StorageDevice> {
        self.devices.values()
    }

    /// Create a cold copy of a volume, targeting a snapshot.
    ///
    /// The cold copy will converge to the given snapshot once `replicate()`
    /// is called. The target device must have enough capacity for the volume.
    pub fn create_cold_copy(
        &mut self,
        volume_id: VolumeId,
        target: Arc<dyn BlockDevice>,
        target_snapshot: VolumeId,
        total_extents: u64,
        extent_size: u64,
        tier: StorageTier,
        locality: Locality,
    ) -> Uuid {
        let cold = ColdCopy::new(
            volume_id,
            target,
            target_snapshot,
            total_extents,
            extent_size,
            tier,
            locality,
        );
        let id = cold.id();
        tracing::info!(
            "placement: created cold copy {} for volume {} → snapshot {}",
            id, volume_id, target_snapshot,
        );
        self.volume_copies
            .entry(volume_id)
            .or_default()
            .push(id);
        self.cold_copies.insert(id, cold);
        id
    }

    /// Get a cold copy by ID.
    pub fn get_cold_copy(&self, id: &Uuid) -> Option<&ColdCopy> {
        self.cold_copies.get(id)
    }

    /// Get a mutable cold copy by ID.
    pub fn get_cold_copy_mut(&mut self, id: &Uuid) -> Option<&mut ColdCopy> {
        self.cold_copies.get_mut(id)
    }

    /// List cold copies for a volume.
    pub fn volume_cold_copies(&self, volume_id: &VolumeId) -> Vec<&ColdCopy> {
        self.volume_copies
            .get(volume_id)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| self.cold_copies.get(id))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Replicate a cold copy from its target snapshot.
    ///
    /// Reads unsynced extents from the snapshot and writes them to the
    /// cold copy's target device. Returns when all extents are synced
    /// or the shutdown signal fires.
    pub async fn replicate(
        &mut self,
        cold_copy_id: &Uuid,
        snapshot: &dyn BlockDevice,
        rate_limit: Option<u64>,
        shutdown: &tokio::sync::watch::Receiver<bool>,
    ) -> Result<ReplicationResult, ReplicationError> {
        let cold = self.cold_copies.get_mut(cold_copy_id)
            .ok_or_else(|| ReplicationError::ReadFailed {
                extent_idx: 0,
                error: "cold copy not found".into(),
            })?;

        cold::replicate(snapshot, cold, rate_limit, shutdown).await
    }

    /// Advance a cold copy to a new snapshot using incremental diff.
    ///
    /// Computes which extents changed between the old and new snapshot,
    /// marks only those as needing re-sync, then replicates just the delta.
    pub async fn advance_cold_copy(
        &mut self,
        cold_copy_id: &Uuid,
        old_snapshot: &ThinVolumeHandle,
        new_snapshot: &ThinVolumeHandle,
        new_snapshot_device: &dyn BlockDevice,
        rate_limit: Option<u64>,
        shutdown: &tokio::sync::watch::Receiver<bool>,
    ) -> Result<ReplicationResult, ReplicationError> {
        let old_id = old_snapshot.volume_id();
        let new_snap_id = new_snapshot.volume_id();

        // Compute diff via GEM
        let changed = {
            let gem = new_snapshot.gem().lock().await;
            snapshot_diff(&gem, old_id, new_snap_id)
        };

        tracing::info!(
            "placement: advancing cold copy {}: {} extents changed",
            cold_copy_id, changed.len(),
        );

        // Advance state
        let cold = self.cold_copies.get_mut(cold_copy_id)
            .ok_or_else(|| ReplicationError::ReadFailed {
                extent_idx: 0,
                error: "cold copy not found".into(),
            })?;
        cold.advance_to(new_snap_id, &changed);

        // Replicate only the delta
        cold::replicate(new_snapshot_device, cold, rate_limit, shutdown).await
    }

    /// Remove a cold copy.
    pub fn remove_cold_copy(&mut self, id: &Uuid) -> Option<ColdCopy> {
        if let Some(cold) = self.cold_copies.remove(id) {
            if let Some(ids) = self.volume_copies.get_mut(&cold.volume_id()) {
                ids.retain(|cid| cid != id);
            }
            Some(cold)
        } else {
            None
        }
    }
}

impl Default for PlacementEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;
    use crate::drive::BlockDevice;
    use crate::drive::slab::Slab;
    use crate::drive::slab_registry::SlabRegistry;
    use crate::raid::{RaidArray, RaidLevel};
    use crate::volume::gem::GlobalExtentMap;
    use crate::volume::snapshot::create_snapshot;
    use crate::volume::thin::{ThinVolume, PlacementPolicy};

    async fn setup_test_env(
        slot_size: u64,
    ) -> (Arc<ThinVolumeHandle>, Arc<tokio::sync::Mutex<GlobalExtentMap>>, Arc<tokio::sync::Mutex<SlabRegistry>>, PlacementEngine, Vec<String>) {
        let test_id = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::env::temp_dir().join("stormblock-placement-test");
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

        let vol = ThinVolume::new("source".to_string(), 32 * 1024 * 1024, slot_size);
        let vol_handle = Arc::new(ThinVolumeHandle::new(
            vol,
            gem.clone(),
            registry.clone(),
            PlacementPolicy::default(),
        ));
        let engine = PlacementEngine::new();
        (vol_handle, gem, registry, engine, paths)
    }

    fn cleanup(paths: &[String]) {
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }

    #[tokio::test]
    async fn engine_full_cold_copy_lifecycle() {
        let slot_size = 4096u64;
        let (vol_handle, gem, registry, mut engine, mut paths) = setup_test_env(slot_size).await;

        // Write data
        vol_handle.write(0, &vec![0xAA; 4096]).await.unwrap();
        vol_handle.write(4096, &vec![0xBB; 4096]).await.unwrap();

        // Snapshot
        let source_id = vol_handle.volume_id();
        let snap1 = {
            let mut gem_guard = gem.lock().await;
            let mut reg_guard = registry.lock().await;
            let snap_vol = create_snapshot(source_id, "snap1", 32 * 1024 * 1024, slot_size, &mut gem_guard, &mut reg_guard)
                .await
                .unwrap();
            Arc::new(ThinVolumeHandle::new(
                snap_vol,
                gem.clone(),
                registry.clone(),
                PlacementPolicy::default(),
            ))
        };
        let snap1_id = snap1.volume_id();

        // Create cold copy target
        let cold_path = {
            let test_id = uuid::Uuid::new_v4().simple().to_string();
            let dir = std::env::temp_dir().join("stormblock-placement-test");
            let path = dir.join(format!("{test_id}-cold.bin"));
            let path_str = path.to_str().unwrap().to_string();
            let _ = std::fs::remove_file(&path);
            paths.push(path_str.clone());
            path_str
        };
        let cold_dev = Arc::new(
            FileDevice::open_with_capacity(&cold_path, 32 * 1024 * 1024)
                .await
                .unwrap(),
        ) as Arc<dyn BlockDevice>;

        let total_extents = 32 * 1024 * 1024 / slot_size;
        let vol_id = vol_handle.volume_id();

        let cold_id = engine.create_cold_copy(
            vol_id,
            cold_dev.clone(),
            snap1_id,
            total_extents,
            slot_size,
            StorageTier::Cold,
            Locality::Local,
        );

        // Replicate
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let result = engine
            .replicate(&cold_id, snap1.as_ref(), None, &rx)
            .await
            .unwrap();
        assert!(result.consistent);

        // Verify data
        let mut buf = vec![0u8; 4096];
        cold_dev.read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xAA));
        cold_dev.read(4096, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xBB));

        // Modify live volume
        vol_handle.write(0, &vec![0xDD; 4096]).await.unwrap();

        // Snapshot 2
        let snap2 = {
            let mut gem_guard = gem.lock().await;
            let mut reg_guard = registry.lock().await;
            let snap_vol = create_snapshot(source_id, "snap2", 32 * 1024 * 1024, slot_size, &mut gem_guard, &mut reg_guard)
                .await
                .unwrap();
            Arc::new(ThinVolumeHandle::new(
                snap_vol,
                gem.clone(),
                registry.clone(),
                PlacementPolicy::default(),
            ))
        };

        // Advance cold copy
        let result2 = engine
            .advance_cold_copy(
                &cold_id,
                snap1.as_ref(),
                snap2.as_ref(),
                snap2.as_ref(),
                None,
                &rx,
            )
            .await
            .unwrap();
        assert!(result2.consistent);

        // Verify updated data
        cold_dev.read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xDD));
        // Extent 1 unchanged
        cold_dev.read(4096, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xBB));

        // Check volume_cold_copies
        let copies = engine.volume_cold_copies(&vol_id);
        assert_eq!(copies.len(), 1);
        assert!(copies[0].is_consistent());

        // Remove cold copy
        let removed = engine.remove_cold_copy(&cold_id);
        assert!(removed.is_some());
        assert!(engine.volume_cold_copies(&vol_id).is_empty());

        cleanup(&paths);
    }

    #[test]
    fn engine_device_management() {
        let mut engine = PlacementEngine::new();
        assert_eq!(engine.devices().count(), 0);

        // Can't test with real devices in a unit test, but we can test
        // the HashMap operations
        assert!(engine.remove_device(Uuid::new_v4()).is_none());
    }
}
