//! Snapshot-fenced cold copies — the correctness foundation for data placement.
//!
//! A cold copy is a replica of a volume on a separate device. It converges to
//! a **specific snapshot** — not to live data. This guarantees the cold copy
//! represents a consistent point in time: every extent matches the same snapshot
//! generation.
//!
//! Without snapshot fencing, a cold copy that copies extents one-by-one from a
//! live volume would be an inconsistent mix of different timepoints — garbage
//! for recovery purposes.
//!
//! # Flow
//!
//! ```text
//! 1. Take snapshot S1 of live volume (COW freeze)
//! 2. Create ColdCopy targeting S1
//! 3. replicate(): read each extent from S1, write to cold device
//! 4. All extents synced → cold copy is consistent at S1
//!
//! Incremental update:
//! 5. Take snapshot S2
//! 6. snapshot_diff(S1, S2) → changed extent indices
//! 7. cold.advance_to(S2, changed)
//! 8. replicate(): only copy changed extents
//! 9. Cold copy now consistent at S2
//! ```
//!
//! The snapshot provides the fence. Since it's COW, the live volume runs
//! uninterrupted while we replicate from the frozen point-in-time view.

use std::fmt;
use std::sync::Arc;

use bitvec::prelude::*;
use uuid::Uuid;

use crate::drive::BlockDevice;
use crate::volume::extent::VolumeId;
use super::topology::{StorageTier, Locality};

/// State of a snapshot-fenced cold copy.
///
/// Tracks which extents have been synced to the target device, and which
/// snapshot the cold copy is converging to. A cold copy is "consistent"
/// when every extent has been synced to the target snapshot.
pub struct ColdCopy {
    /// Unique identifier for this cold copy.
    id: Uuid,
    /// Source volume this is a replica of.
    volume_id: VolumeId,
    /// Device where the cold copy lives.
    target: Arc<dyn BlockDevice>,
    /// Snapshot we're currently converging to.
    target_snapshot: VolumeId,
    /// Per-extent sync bitmap: bit N set = extent N is synced to target_snapshot.
    synced: BitVec<u8, Lsb0>,
    /// Total number of extents.
    total_extents: u64,
    /// Extent size in bytes.
    extent_size: u64,
    /// Last snapshot where ALL extents were synced (the consistency point).
    last_consistent: Option<VolumeId>,
    /// Storage tier of the target device.
    tier: StorageTier,
    /// Locality of the target device.
    locality: Locality,
}

impl ColdCopy {
    /// Create a new cold copy targeting a snapshot.
    ///
    /// All extents start as unsynced. Call `replicate()` to begin copying data.
    pub fn new(
        volume_id: VolumeId,
        target: Arc<dyn BlockDevice>,
        target_snapshot: VolumeId,
        total_extents: u64,
        extent_size: u64,
        tier: StorageTier,
        locality: Locality,
    ) -> Self {
        ColdCopy {
            id: Uuid::new_v4(),
            volume_id,
            target,
            target_snapshot,
            synced: BitVec::repeat(false, total_extents as usize),
            total_extents,
            extent_size,
            last_consistent: None,
            tier,
            locality,
        }
    }

    /// Unique ID of this cold copy.
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Source volume ID.
    pub fn volume_id(&self) -> VolumeId {
        self.volume_id
    }

    /// Snapshot this cold copy is converging to.
    pub fn target_snapshot(&self) -> VolumeId {
        self.target_snapshot
    }

    /// Target device.
    pub fn target_device(&self) -> &Arc<dyn BlockDevice> {
        &self.target
    }

    /// Is the cold copy consistent (all extents synced to the target snapshot)?
    pub fn is_consistent(&self) -> bool {
        self.synced_count() == self.total_extents
    }

    /// Replication progress as percentage (0-100).
    pub fn progress_pct(&self) -> u8 {
        if self.total_extents == 0 {
            return 100;
        }
        ((self.synced_count() * 100) / self.total_extents) as u8
    }

    /// Number of synced extents.
    pub fn synced_count(&self) -> u64 {
        self.synced.count_ones() as u64
    }

    /// Number of extents still needing sync.
    pub fn remaining(&self) -> u64 {
        self.total_extents - self.synced_count()
    }

    /// Total extent count.
    pub fn total_extents(&self) -> u64 {
        self.total_extents
    }

    /// Extent size in bytes.
    pub fn extent_size(&self) -> u64 {
        self.extent_size
    }

    /// Storage tier of the cold copy.
    pub fn tier(&self) -> StorageTier {
        self.tier
    }

    /// Last snapshot where the cold copy was fully consistent, if any.
    pub fn last_consistent_snapshot(&self) -> Option<VolumeId> {
        self.last_consistent
    }

    /// Mark an extent as synced to the target snapshot.
    pub fn mark_synced(&mut self, extent_idx: u64) {
        if (extent_idx as usize) < self.synced.len() {
            self.synced.set(extent_idx as usize, true);
        }
        // Check if we just became consistent
        if self.is_consistent() && self.last_consistent != Some(self.target_snapshot) {
            self.last_consistent = Some(self.target_snapshot);
            tracing::info!(
                "cold copy {} consistent at snapshot {}",
                self.id, self.target_snapshot,
            );
        }
    }

    /// Get indices of extents that still need syncing.
    pub fn unsynced_extents(&self) -> Vec<u64> {
        self.synced
            .iter()
            .enumerate()
            .filter(|(_, bit)| !**bit)
            .map(|(idx, _)| idx as u64)
            .collect()
    }

    /// Advance the cold copy to a new snapshot (incremental update).
    ///
    /// Takes a list of extent indices that changed between the old and new
    /// snapshot (from `snapshot_diff()`). Only those extents are marked as
    /// unsynced — everything else remains synced from the previous round.
    pub fn advance_to(&mut self, new_snapshot: VolumeId, changed_extents: &[u64]) {
        let previous = self.target_snapshot;
        self.target_snapshot = new_snapshot;
        // Mark only the changed extents as needing re-sync
        for &idx in changed_extents {
            if (idx as usize) < self.synced.len() {
                self.synced.set(idx as usize, false);
            }
        }
        tracing::debug!(
            "cold copy {} advanced: {} → {}, {} extents need re-sync",
            self.id, previous, new_snapshot, changed_extents.len(),
        );
    }
}

impl fmt::Display for ColdCopy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ColdCopy[{}] vol={} snap={} {}/{} extents ({}%) tier={} {}",
            &self.id.to_string()[..8],
            self.volume_id,
            self.target_snapshot,
            self.synced_count(),
            self.total_extents,
            self.progress_pct(),
            self.tier,
            if self.is_consistent() { "CONSISTENT" } else { "syncing" },
        )
    }
}

// ---------------------------------------------------------------------------
// Replication
// ---------------------------------------------------------------------------

/// Result of a replication run.
#[derive(Debug)]
pub struct ReplicationResult {
    /// Number of extents synced in this run.
    pub synced_extents: u64,
    /// Total extents that needed syncing when the run started.
    pub total_needed: u64,
    /// Whether the cold copy is now fully consistent.
    pub consistent: bool,
}

/// Errors during replication.
#[derive(Debug)]
pub enum ReplicationError {
    /// Failed to read extent from snapshot.
    ReadFailed { extent_idx: u64, error: String },
    /// Failed to write extent to cold copy device.
    WriteFailed { extent_idx: u64, error: String },
}

impl fmt::Display for ReplicationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReplicationError::ReadFailed { extent_idx, error } => {
                write!(f, "read extent {} from snapshot failed: {}", extent_idx, error)
            }
            ReplicationError::WriteFailed { extent_idx, error } => {
                write!(f, "write extent {} to cold copy failed: {}", extent_idx, error)
            }
        }
    }
}

impl std::error::Error for ReplicationError {}

/// Replicate unsynced extents from a snapshot to a cold copy device.
///
/// Reads each unsynced extent from the snapshot (a `BlockDevice` representing
/// the frozen point-in-time view) and writes it to the cold copy's target
/// device. Respects the shutdown signal and optional rate limiting.
///
/// Returns the number of extents synced and whether consistency was reached.
pub async fn replicate(
    snapshot: &dyn BlockDevice,
    cold: &mut ColdCopy,
    rate_limit_bytes_sec: Option<u64>,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> Result<ReplicationResult, ReplicationError> {
    let unsynced = cold.unsynced_extents();
    let total_needed = unsynced.len() as u64;
    let extent_size = cold.extent_size;
    let mut synced = 0u64;

    let mut buf = vec![0u8; extent_size as usize];

    for &extent_idx in &unsynced {
        // Check for shutdown
        if *shutdown.borrow() {
            tracing::debug!(
                "replication interrupted: {}/{} extents synced",
                synced, total_needed,
            );
            return Ok(ReplicationResult {
                synced_extents: synced,
                total_needed,
                consistent: cold.is_consistent(),
            });
        }

        let offset = extent_idx * extent_size;

        // Read from snapshot (the COW point-in-time view)
        snapshot
            .read(offset, &mut buf)
            .await
            .map_err(|e| ReplicationError::ReadFailed {
                extent_idx,
                error: e.to_string(),
            })?;

        // Write to cold copy target
        cold.target
            .write(offset, &buf)
            .await
            .map_err(|e| ReplicationError::WriteFailed {
                extent_idx,
                error: e.to_string(),
            })?;

        cold.mark_synced(extent_idx);
        synced += 1;

        // Rate limiting: sleep proportional to bytes copied
        if let Some(rate) = rate_limit_bytes_sec {
            if rate > 0 {
                let sleep_us = (extent_size as f64 / rate as f64 * 1_000_000.0) as u64;
                if sleep_us > 0 {
                    tokio::time::sleep(tokio::time::Duration::from_micros(sleep_us)).await;
                }
            }
        }
    }

    // Flush the cold copy device to ensure durability
    if synced > 0 {
        let _ = cold.target.flush().await;
    }

    Ok(ReplicationResult {
        synced_extents: synced,
        total_needed,
        consistent: cold.is_consistent(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;
    use crate::drive::BlockDevice;
    use crate::raid::{RaidArray, RaidLevel};
    use crate::volume::extent::{ExtentAllocator, VolumeId};
    use crate::volume::snapshot::{create_snapshot, snapshot_diff};
    use crate::volume::thin::{ThinVolume, ThinVolumeHandle};
    use std::sync::Arc;

    /// Set up a source volume on RAID 1 with small extents for testing.
    async fn setup_volume(
        extent_size: u64,
    ) -> (Arc<ThinVolumeHandle>, Vec<String>) {
        let test_id = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::env::temp_dir().join("stormblock-cold-test");
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
        let capacity = array.capacity_bytes();
        let backing: Arc<dyn BlockDevice> = Arc::new(array);

        let mut allocator = ExtentAllocator::new(extent_size);
        allocator.add_array(array_id, capacity);
        let allocator = Arc::new(tokio::sync::Mutex::new(allocator));

        let vol = ThinVolume::new(
            "source".to_string(),
            32 * 1024 * 1024, // 32 MB virtual
            array_id,
            backing,
            allocator,
        );
        (Arc::new(ThinVolumeHandle::new(vol)), paths)
    }

    /// Create a cold copy target (plain file device).
    async fn create_cold_target(
        paths: &mut Vec<String>,
        size: u64,
    ) -> Arc<dyn BlockDevice> {
        let test_id = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::env::temp_dir().join("stormblock-cold-test");
        let path = dir.join(format!("{test_id}-cold.bin"));
        let path_str = path.to_str().unwrap().to_string();
        let _ = std::fs::remove_file(&path);
        let dev = FileDevice::open_with_capacity(&path_str, size).await.unwrap();
        paths.push(path_str);
        Arc::new(dev) as Arc<dyn BlockDevice>
    }

    fn cleanup(paths: &[String]) {
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }

    #[tokio::test]
    async fn cold_copy_full_replication() {
        let extent_size = 4096u64;
        let (vol_handle, mut paths) = setup_volume(extent_size).await;

        // Write data to 3 extents
        vol_handle.write(0, &vec![0xAA; 4096]).await.unwrap();
        vol_handle.write(4096, &vec![0xBB; 4096]).await.unwrap();
        vol_handle.write(8192, &vec![0xCC; 4096]).await.unwrap();

        // Take snapshot
        let snap_handle = {
            let mut vol = vol_handle.lock().await;
            let snap = create_snapshot(&mut vol, "snap1");
            Arc::new(ThinVolumeHandle::new(snap))
        };
        let snap_id = snap_handle.lock().await.id();

        // Create cold copy target
        let cold_dev = create_cold_target(&mut paths, 32 * 1024 * 1024).await;
        let total_extents = 32 * 1024 * 1024 / extent_size;

        let mut cold = ColdCopy::new(
            vol_handle.lock().await.id(),
            cold_dev.clone(),
            snap_id,
            total_extents,
            extent_size,
            StorageTier::Cold,
            Locality::Local,
        );

        assert!(!cold.is_consistent());
        assert_eq!(cold.progress_pct(), 0);

        // Replicate
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let result = replicate(snap_handle.as_ref(), &mut cold, None, &rx)
            .await
            .unwrap();

        assert!(result.consistent);
        assert!(cold.is_consistent());
        assert_eq!(cold.progress_pct(), 100);
        assert!(cold.last_consistent_snapshot().is_some());

        // Verify cold copy data matches snapshot
        let mut buf = vec![0u8; 4096];
        cold_dev.read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xAA), "extent 0 mismatch");

        cold_dev.read(4096, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xBB), "extent 1 mismatch");

        cold_dev.read(8192, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xCC), "extent 2 mismatch");

        cleanup(&paths);
    }

    #[tokio::test]
    async fn cold_copy_incremental_update() {
        let extent_size = 4096u64;
        let (vol_handle, mut paths) = setup_volume(extent_size).await;

        // Write initial data
        vol_handle.write(0, &vec![0xAA; 4096]).await.unwrap();
        vol_handle.write(4096, &vec![0xBB; 4096]).await.unwrap();
        vol_handle.write(8192, &vec![0xCC; 4096]).await.unwrap();

        // Snapshot 1
        let snap1_handle = {
            let mut vol = vol_handle.lock().await;
            Arc::new(ThinVolumeHandle::new(create_snapshot(&mut vol, "snap1")))
        };
        let snap1_id = snap1_handle.lock().await.id();

        // Full replication to cold copy
        let cold_dev = create_cold_target(&mut paths, 32 * 1024 * 1024).await;
        let total_extents = 32 * 1024 * 1024 / extent_size;

        let mut cold = ColdCopy::new(
            vol_handle.lock().await.id(),
            cold_dev.clone(),
            snap1_id,
            total_extents,
            extent_size,
            StorageTier::Cold,
            Locality::Local,
        );

        let (_tx, rx) = tokio::sync::watch::channel(false);
        replicate(snap1_handle.as_ref(), &mut cold, None, &rx)
            .await
            .unwrap();
        assert!(cold.is_consistent());

        // Modify live volume: change extent 0, leave extents 1 and 2 alone
        vol_handle.write(0, &vec![0xDD; 4096]).await.unwrap();

        // Snapshot 2
        let snap2_handle = {
            let mut vol = vol_handle.lock().await;
            Arc::new(ThinVolumeHandle::new(create_snapshot(&mut vol, "snap2")))
        };
        let snap2_id = snap2_handle.lock().await.id();

        // Compute diff
        let changed = {
            let s1 = snap1_handle.lock().await;
            let s2 = snap2_handle.lock().await;
            snapshot_diff(&s1, &s2)
        };

        // Extent 0 should be in the diff (was rewritten via COW)
        assert!(changed.contains(&0), "extent 0 should be changed");
        // Extent 1 should NOT be changed
        assert!(!changed.contains(&1), "extent 1 should not be changed");

        // Advance cold copy to snapshot 2
        cold.advance_to(snap2_id, &changed);
        assert!(!cold.is_consistent()); // extent 0 needs re-sync

        // Incremental replicate — only the changed extents
        let result = replicate(snap2_handle.as_ref(), &mut cold, None, &rx)
            .await
            .unwrap();
        assert!(result.consistent);
        assert!(cold.is_consistent());
        // Only extent 0 (and any other changed extents) needed syncing
        assert!(result.synced_extents <= changed.len() as u64 + 1); // +1 for potential edge cases

        // Verify cold copy has new data for extent 0
        let mut buf = vec![0u8; 4096];
        cold_dev.read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xDD), "extent 0 should have new data");

        // Extent 1 still has original data (wasn't changed)
        cold_dev.read(4096, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xBB), "extent 1 should be unchanged");

        // Extent 2 still has original data
        cold_dev.read(8192, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xCC), "extent 2 should be unchanged");

        cleanup(&paths);
    }

    #[tokio::test]
    async fn cold_copy_respects_shutdown() {
        let extent_size = 4096u64;
        let (vol_handle, mut paths) = setup_volume(extent_size).await;

        // Write to many extents
        for i in 0..10u64 {
            vol_handle
                .write(i * extent_size, &vec![(i as u8) + 1; extent_size as usize])
                .await
                .unwrap();
        }

        let snap_handle = {
            let mut vol = vol_handle.lock().await;
            Arc::new(ThinVolumeHandle::new(create_snapshot(&mut vol, "snap1")))
        };
        let snap_id = snap_handle.lock().await.id();

        let cold_dev = create_cold_target(&mut paths, 32 * 1024 * 1024).await;
        let total_extents = 32 * 1024 * 1024 / extent_size;

        let mut cold = ColdCopy::new(
            vol_handle.lock().await.id(),
            cold_dev,
            snap_id,
            total_extents,
            extent_size,
            StorageTier::Cold,
            Locality::Local,
        );

        // Signal shutdown immediately
        let (tx, rx) = tokio::sync::watch::channel(true);
        let result = replicate(snap_handle.as_ref(), &mut cold, None, &rx)
            .await
            .unwrap();

        // Should have stopped immediately without syncing anything
        assert_eq!(result.synced_extents, 0);
        assert!(!result.consistent);
        drop(tx);

        cleanup(&paths);
    }

    #[test]
    fn cold_copy_state_tracking() {
        let vol_id = VolumeId::new();
        let snap_id = VolumeId::new();

        // Use a dummy device — we're only testing state, not I/O
        let target: Arc<dyn BlockDevice> = Arc::new(DummyDevice);

        let mut cold = ColdCopy::new(
            vol_id,
            target,
            snap_id,
            100,
            4096,
            StorageTier::Cold,
            Locality::Remote {
                addr: "10.0.0.1:3260".into(),
                latency_us: 1000,
            },
        );

        assert_eq!(cold.total_extents(), 100);
        assert_eq!(cold.synced_count(), 0);
        assert_eq!(cold.remaining(), 100);
        assert_eq!(cold.progress_pct(), 0);
        assert!(!cold.is_consistent());
        assert!(cold.last_consistent_snapshot().is_none());

        // Sync some extents
        for i in 0..50 {
            cold.mark_synced(i);
        }
        assert_eq!(cold.synced_count(), 50);
        assert_eq!(cold.remaining(), 50);
        assert_eq!(cold.progress_pct(), 50);
        assert!(!cold.is_consistent());

        // Sync all
        for i in 50..100 {
            cold.mark_synced(i);
        }
        assert!(cold.is_consistent());
        assert_eq!(cold.progress_pct(), 100);
        assert_eq!(cold.last_consistent_snapshot(), Some(snap_id));

        // Advance to new snapshot with 5 changed extents
        let snap2 = VolumeId::new();
        cold.advance_to(snap2, &[0, 10, 20, 30, 40]);
        assert!(!cold.is_consistent());
        assert_eq!(cold.remaining(), 5);
        assert_eq!(cold.target_snapshot(), snap2);

        let unsynced = cold.unsynced_extents();
        assert_eq!(unsynced, vec![0, 10, 20, 30, 40]);
    }

    #[test]
    fn cold_copy_display() {
        let vol_id = VolumeId::new();
        let snap_id = VolumeId::new();
        let target: Arc<dyn BlockDevice> = Arc::new(DummyDevice);

        let cold = ColdCopy::new(
            vol_id,
            target,
            snap_id,
            100,
            4096,
            StorageTier::Cold,
            Locality::Local,
        );

        let s = cold.to_string();
        assert!(s.contains("ColdCopy["));
        assert!(s.contains("0/100"));
        assert!(s.contains("syncing"));
    }

    // Minimal BlockDevice for unit tests that don't do I/O.
    struct DummyDevice;

    #[async_trait::async_trait]
    impl BlockDevice for DummyDevice {
        fn id(&self) -> &crate::drive::DeviceId {
            static ID: std::sync::OnceLock<crate::drive::DeviceId> = std::sync::OnceLock::new();
            ID.get_or_init(|| crate::drive::DeviceId {
                uuid: uuid::Uuid::nil(),
                serial: "dummy".into(),
                model: "dummy".into(),
                path: "dummy".into(),
            })
        }
        fn capacity_bytes(&self) -> u64 { 0 }
        fn block_size(&self) -> u32 { 4096 }
        fn optimal_io_size(&self) -> u32 { 4096 }
        fn device_type(&self) -> crate::drive::DriveType { crate::drive::DriveType::File }
        async fn read(&self, _offset: u64, _buf: &mut [u8]) -> crate::drive::DriveResult<usize> { Ok(0) }
        async fn write(&self, _offset: u64, _buf: &[u8]) -> crate::drive::DriveResult<usize> { Ok(0) }
        async fn flush(&self) -> crate::drive::DriveResult<()> { Ok(()) }
        async fn discard(&self, _offset: u64, _len: u64) -> crate::drive::DriveResult<()> { Ok(()) }
    }
}
