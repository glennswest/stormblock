//! Thin volume — virtual size, on-demand extent allocation via slabs.
//!
//! `ThinVolume` implements `BlockDevice`, so target protocols see volumes
//! as plain block devices. Physical storage is allocated on first write
//! (allocate-on-write) from slab slots via the Global Extent Map (GEM).

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use serde::{Serialize, Deserialize};

use crate::drive::{BlockDevice, DeviceId, DriveError, DriveResult, DriveType, SmartData};
use crate::drive::slab::SlabId;
use crate::drive::slab_registry::SlabRegistry;
use crate::placement::topology::StorageTier;
use super::extent::VolumeId;
use super::gem::{ExtentLocation, GlobalExtentMap};

/// A physical extent with reference counting for COW snapshots.
/// Legacy type — kept for metadata V1 compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhysicalExtent {
    pub array_id: crate::raid::RaidArrayId,
    pub offset: u64,
    pub length: u64,
    pub ref_count: u32,
}

/// Volume purpose — how the volume will be used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum VolumePurpose {
    #[default]
    Partition,
    StormFS,
    ObjectStore,
    KeyValue,
    Boot,
}


impl fmt::Display for VolumePurpose {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VolumePurpose::Partition => write!(f, "partition"),
            VolumePurpose::StormFS => write!(f, "stormfs"),
            VolumePurpose::ObjectStore => write!(f, "objstore"),
            VolumePurpose::KeyValue => write!(f, "kv"),
            VolumePurpose::Boot => write!(f, "boot"),
        }
    }
}

/// Placement policy for a volume — controls which slab tiers are preferred.
#[derive(Debug, Clone)]
pub struct PlacementPolicy {
    pub preferred_tier: StorageTier,
    pub tier_fallback: Vec<StorageTier>,
}

impl Default for PlacementPolicy {
    fn default() -> Self {
        PlacementPolicy {
            preferred_tier: StorageTier::Hot,
            tier_fallback: vec![StorageTier::Warm, StorageTier::Cool, StorageTier::Cold],
        }
    }
}

/// Volume manager errors.
#[derive(Debug)]
pub enum VolumeError {
    NoSpace,
    VolumeNotFound(VolumeId),
    InvalidSize(String),
    Drive(DriveError),
    AllocatorError(String),
}

impl fmt::Display for VolumeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VolumeError::NoSpace => write!(f, "no free slots available"),
            VolumeError::VolumeNotFound(id) => write!(f, "volume {id} not found"),
            VolumeError::InvalidSize(msg) => write!(f, "invalid size: {msg}"),
            VolumeError::Drive(e) => write!(f, "drive error: {e}"),
            VolumeError::AllocatorError(msg) => write!(f, "allocator error: {msg}"),
        }
    }
}

impl std::error::Error for VolumeError {}

impl From<DriveError> for VolumeError {
    fn from(e: DriveError) -> Self {
        VolumeError::Drive(e)
    }
}

impl From<VolumeError> for DriveError {
    fn from(e: VolumeError) -> Self {
        DriveError::Other(anyhow::anyhow!("{e}"))
    }
}

/// A thin-provisioned volume backed by slabs via the Global Extent Map.
///
/// Virtual blocks are mapped to slab slots on demand. Implements `BlockDevice`
/// for use by target protocols (NVMe-oF, iSCSI). Storage is allocated from
/// any slab in the registry according to the placement policy.
pub struct ThinVolume {
    pub(crate) id: VolumeId,
    pub(crate) name: String,
    pub(crate) virtual_size: u64,
    pub(crate) slot_size: u64,
    #[allow(dead_code)]
    pub(crate) purpose: VolumePurpose,
    pub(crate) device_id: DeviceId,
}

impl ThinVolume {
    pub fn new(
        name: String,
        virtual_size: u64,
        slot_size: u64,
    ) -> Self {
        let id = VolumeId::new();
        let device_id = DeviceId {
            uuid: id.0,
            serial: format!("vol-{}", &id.0.simple().to_string()[..8]),
            model: "ThinVolume".to_string(),
            path: format!("volume:{id}"),
        };

        ThinVolume {
            id,
            name,
            virtual_size,
            slot_size,
            purpose: VolumePurpose::Partition,
            device_id,
        }
    }

    /// Restore a volume from persisted config (recovery path).
    pub fn restore(
        id: VolumeId,
        name: String,
        virtual_size: u64,
        slot_size: u64,
    ) -> Self {
        let device_id = DeviceId {
            uuid: id.0,
            serial: format!("vol-{}", &id.0.simple().to_string()[..8]),
            model: "ThinVolume".to_string(),
            path: format!("volume:{id}"),
        };

        ThinVolume {
            id,
            name,
            virtual_size,
            slot_size,
            purpose: VolumePurpose::Partition,
            device_id,
        }
    }

    pub fn id(&self) -> VolumeId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn virtual_size(&self) -> u64 {
        self.virtual_size
    }

    pub fn slot_size(&self) -> u64 {
        self.slot_size
    }
}

/// `ThinVolume` wrapped with shared GEM and SlabRegistry references.
///
/// The handle owns Arc references to the GEM and registry, allowing
/// lock-free reads and serialized writes. Implements `BlockDevice`.
pub struct ThinVolumeHandle {
    inner: tokio::sync::Mutex<ThinVolume>,
    device_id: DeviceId,
    virtual_size: AtomicU64,
    id: VolumeId,
    slot_size: u64,
    gem: Arc<tokio::sync::RwLock<GlobalExtentMap>>,
    registry: Arc<tokio::sync::RwLock<SlabRegistry>>,
    placement: PlacementPolicy,
}

impl ThinVolumeHandle {
    pub fn new(
        vol: ThinVolume,
        gem: Arc<tokio::sync::RwLock<GlobalExtentMap>>,
        registry: Arc<tokio::sync::RwLock<SlabRegistry>>,
        placement: PlacementPolicy,
    ) -> Self {
        let device_id = vol.device_id.clone();
        let virtual_size = AtomicU64::new(vol.virtual_size);
        let id = vol.id;
        let slot_size = vol.slot_size;
        ThinVolumeHandle {
            inner: tokio::sync::Mutex::new(vol),
            device_id,
            virtual_size,
            id,
            slot_size,
            gem,
            registry,
            placement,
        }
    }

    /// Resize the volume.
    ///
    /// Growing is instant — allocate-on-write handles new space.
    /// Shrinking frees slab slots beyond the new boundary.
    pub async fn resize(&self, new_size: u64) -> Result<(), VolumeError> {
        if new_size == 0 {
            return Err(VolumeError::InvalidSize("size must be > 0".to_string()));
        }

        let current = self.virtual_size.load(Ordering::Relaxed);
        if new_size == current {
            return Ok(());
        }

        let mut vol = self.inner.lock().await;

        if new_size < current {
            // Shrink: free slots beyond new boundary
            let max_vext_idx = new_size / self.slot_size;

            // Collect extents to remove
            let to_remove: Vec<(u64, ExtentLocation)> = {
                let gem = self.gem.read().await;
                gem.volume_extents(&self.id)
                    .map(|iter| {
                        iter.filter(|(&idx, _)| idx >= max_vext_idx)
                            .map(|(&idx, loc)| (idx, loc.clone()))
                            .collect()
                    })
                    .unwrap_or_default()
            };

            for (vext_idx, loc) in to_remove {
                // Remove from GEM
                {
                    let mut gem = self.gem.write().await;
                    gem.remove(self.id, vext_idx);
                }
                // Dec ref on slab
                {
                    let mut reg = self.registry.write().await;
                    if let Some(slab) = reg.get_mut(&loc.slab_id) {
                        if let Err(e) = slab.dec_ref(loc.slot_idx).await {
                            // The mapping is already gone, so a failure here
                            // strands the slot with no owner. Say so.
                            tracing::warn!(
                                volume = %self.id, slot = loc.slot_idx, extent = vext_idx,
                                "discard could not release extent: {e}"
                            );
                        }
                    }
                }
            }
        }

        vol.virtual_size = new_size;
        self.virtual_size.store(new_size, Ordering::Relaxed);
        Ok(())
    }

    pub fn volume_id(&self) -> VolumeId {
        self.id
    }

    pub async fn name(&self) -> String {
        self.inner.lock().await.name.clone()
    }

    pub async fn allocated(&self) -> u64 {
        let gem = self.gem.read().await;
        gem.get_volume_map(&self.id)
            .map(|m| m.len() as u64 * self.slot_size)
            .unwrap_or(0)
    }

    pub async fn extent_count(&self) -> usize {
        let gem = self.gem.read().await;
        gem.get_volume_map(&self.id).map(|m| m.len()).unwrap_or(0)
    }

    /// Access the inner ThinVolume.
    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, ThinVolume> {
        self.inner.lock().await
    }

    /// Get the shared GEM reference.
    pub fn gem(&self) -> &Arc<tokio::sync::RwLock<GlobalExtentMap>> {
        &self.gem
    }

    /// Get the shared SlabRegistry reference.
    pub fn registry(&self) -> &Arc<tokio::sync::RwLock<SlabRegistry>> {
        &self.registry
    }

    /// Allocate a slot from the best available slab according to placement policy.
    async fn allocate_slot(
        &self,
        registry: &mut SlabRegistry,
        vext_idx: u64,
    ) -> DriveResult<(SlabId, u32)> {
        // Reserved until the caller records the mapping — otherwise the
        // collector cannot tell this slot from a leaked one.
        // Try preferred tier first
        if let Some(slab_id) = registry.best_slab_for_tier(self.placement.preferred_tier) {
            if let Some(slab) = registry.get_mut(&slab_id) {
                if let Ok(slot_idx) = slab.allocate(self.id, vext_idx).await {
                    registry.reserve(slab_id, slot_idx);
                    return Ok((slab_id, slot_idx));
                } // full, try fallback
            }
        }

        // Try fallback tiers
        for &tier in &self.placement.tier_fallback {
            if let Some(slab_id) = registry.best_slab_for_tier(tier) {
                if let Some(slab) = registry.get_mut(&slab_id) {
                    match slab.allocate(self.id, vext_idx).await {
                        Ok(slot_idx) => {
                            registry.reserve(slab_id, slot_idx);
                            return Ok((slab_id, slot_idx));
                        }
                        Err(_) => continue,
                    }
                }
            }
        }

        Err(DriveError::Other(anyhow::anyhow!("no space: all slabs exhausted")))
    }

    /// Allocate a new slot and write data (allocate-on-write path).
    async fn allocate_and_write(
        &self,
        vext_idx: u64,
        off_in_slot: u64,
        buf: &[u8],
    ) -> DriveResult<()> {
        // Allocate slot
        let (slab_id, slot_idx) = {
            let mut reg = self.registry.write().await;
            self.allocate_slot(&mut reg, vext_idx).await?
        };

        // Insert into GEM
        {
            let mut gem = self.gem.write().await;
            gem.insert(self.id, vext_idx, ExtentLocation {
                slab_id,
                slot_idx,
                ref_count: 1,
                generation: 1,
            });
        }
        // Mapped now, so the collector can see it is owned.
        self.registry.write().await.commit(slab_id, slot_idx);

        // Write data
        let (device, phys_offset) = {
            let reg = self.registry.read().await;
            let slab = reg.get(&slab_id).ok_or_else(|| {
                DriveError::Other(anyhow::anyhow!("slab {} not found", slab_id.0))
            })?;
            slab.slot_device_and_offset(slot_idx, off_in_slot)?
        };
        device.write(phys_offset, buf).await?;
        Ok(())
    }

    /// Write into an extent this volume already owns exclusively.
    async fn write_in_place(
        &self,
        loc: &ExtentLocation,
        off_in_slot: u64,
        buf: &[u8],
    ) -> DriveResult<()> {
        let (device, phys_offset) = {
            let reg = self.registry.read().await;
            let slab = reg.get(&loc.slab_id).ok_or_else(|| {
                DriveError::Other(anyhow::anyhow!("slab {} not found", loc.slab_id.0))
            })?;
            slab.slot_device_and_offset(loc.slot_idx, off_in_slot)?
        };
        device.write(phys_offset, buf).await?;
        Ok(())
    }

    /// COW: copy old slot data to new slot, write new data, update GEM, dec_ref old.
    async fn cow_write(
        &self,
        vext_idx: u64,
        off_in_slot: u64,
        buf: &[u8],
        old_loc: &ExtentLocation,
    ) -> DriveResult<()> {
        // Read old slot data
        let mut old_data = vec![0u8; self.slot_size as usize];
        {
            let reg = self.registry.read().await;
            let slab = reg.get(&old_loc.slab_id).ok_or_else(|| {
                DriveError::Other(anyhow::anyhow!("slab {} not found", old_loc.slab_id.0))
            })?;
            // A short copy here is silent data loss: the tail of the slot the
            // clone inherited would read back as whatever the new slot already
            // held. Refusing the write keeps the old mapping, which still has
            // the data.
            let n = slab.read_slot(old_loc.slot_idx, 0, &mut old_data).await?;
            if n != old_data.len() {
                return Err(DriveError::Other(anyhow::anyhow!(
                    "copy-on-write read {n} of {} bytes from slab {} slot {}",
                    old_data.len(), old_loc.slab_id.0, old_loc.slot_idx
                )));
            }
        }

        // Allocate new slot
        let (new_slab_id, new_slot_idx) = {
            let mut reg = self.registry.write().await;
            self.allocate_slot(&mut reg, vext_idx).await?
        };

        // Write old data to new slot, then overlay new data.
        //
        // Failing here leaves the slot allocated but never mapped — a genuine
        // orphan. Drop the reservation on the way out so the collector is
        // free to reclaim it, rather than pinning it for the process lifetime.
        let copied = async {
            let reg = self.registry.read().await;
            let slab = reg.get(&new_slab_id).ok_or_else(|| {
                DriveError::Other(anyhow::anyhow!("slab {} not found", new_slab_id.0))
            })?;
            let n = slab.write_slot(new_slot_idx, 0, &old_data).await?;
            if n != old_data.len() {
                return Err(DriveError::Other(anyhow::anyhow!(
                    "copy-on-write wrote {n} of {} bytes into slab {} slot {new_slot_idx}",
                    old_data.len(), new_slab_id.0
                )));
            }
            slab.write_slot(new_slot_idx, off_in_slot, buf).await
        }
        .await;
        if let Err(e) = copied {
            self.registry.write().await.commit(new_slab_id, new_slot_idx);
            return Err(e);
        }

        // Update GEM
        {
            let mut gem = self.gem.write().await;
            gem.insert(self.id, vext_idx, ExtentLocation {
                slab_id: new_slab_id,
                slot_idx: new_slot_idx,
                ref_count: 1,
                generation: old_loc.generation + 1,
            });
        }

        // Dec ref on old slot
        {
            let mut reg = self.registry.write().await;
            // The new slot is mapped now, so it no longer needs protecting.
            reg.commit(new_slab_id, new_slot_idx);
            if let Some(slab) = reg.get_mut(&old_loc.slab_id) {
                if let Err(e) = slab.dec_ref(old_loc.slot_idx).await {
                    tracing::warn!(
                        volume = %self.id, slot = old_loc.slot_idx,
                        "copy-on-write could not release the shared extent: {e}"
                    );
                }
            }
        }

        Ok(())
    }
}

#[async_trait]
impl BlockDevice for ThinVolumeHandle {
    fn id(&self) -> &DeviceId {
        &self.device_id
    }

    fn capacity_bytes(&self) -> u64 {
        self.virtual_size.load(Ordering::Relaxed)
    }

    fn block_size(&self) -> u32 {
        4096
    }

    fn optimal_io_size(&self) -> u32 {
        4096
    }

    /// Space comes back a whole slab slot at a time — `discard` only frees
    /// fully-covered slots, so a smaller discard reclaims nothing (#25).
    fn discard_granularity(&self) -> u32 {
        self.slot_size.min(u32::MAX as u64) as u32
    }

    fn device_type(&self) -> DriveType {
        DriveType::File
    }

    async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        let buf_len = buf.len() as u64;
        let mut bytes_read = 0u64;
        let mut pos = offset;

        while bytes_read < buf_len {
            let vext_idx = pos / self.slot_size;
            let off_in_slot = pos % self.slot_size;
            let remaining_in_slot = self.slot_size - off_in_slot;
            let remaining_in_buf = buf_len - bytes_read;
            let to_read = remaining_in_slot.min(remaining_in_buf) as usize;

            let buf_start = bytes_read as usize;
            let buf_end = buf_start + to_read;

            // Look up extent in GEM
            let location = {
                let gem = self.gem.read().await;
                gem.lookup(self.id, vext_idx).cloned()
            };

            match location {
                Some(loc) => {
                    // Get device + physical offset from slab
                    let (device, phys_offset) = {
                        let reg = self.registry.read().await;
                        let slab = reg.get(&loc.slab_id).ok_or_else(|| {
                            DriveError::Other(anyhow::anyhow!(
                                "slab {} not found", loc.slab_id.0
                            ))
                        })?;
                        slab.slot_device_and_offset(loc.slot_idx, off_in_slot)?
                    };
                    device.read(phys_offset, &mut buf[buf_start..buf_end]).await?;
                }
                None => {
                    // Unallocated — return zeros
                    buf[buf_start..buf_end].fill(0);
                }
            }

            bytes_read += to_read as u64;
            pos += to_read as u64;
        }

        Ok(bytes_read as usize)
    }

    /// Write, taking the per-volume lock only when the mapping must change.
    ///
    /// A steady-state write lands on an extent this volume already owns
    /// exclusively, which needs no serialisation — holding the volume lock for
    /// the whole call (as this used to) meant every write to a volume queued
    /// behind every other, no matter which extent it touched.
    ///
    /// Allocation and COW do change the mapping, so those re-check the extent
    /// under the lock before acting: two writers can both observe "unmapped"
    /// before either allocates, and without the re-check the second would
    /// allocate a duplicate slot and discard the first writer's data.
    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        let buf_len = buf.len() as u64;
        let mut bytes_written = 0u64;
        let mut pos = offset;

        while bytes_written < buf_len {
            let vext_idx = pos / self.slot_size;
            let off_in_slot = pos % self.slot_size;
            let remaining_in_slot = self.slot_size - off_in_slot;
            let remaining_in_buf = buf_len - bytes_written;
            let to_write = remaining_in_slot.min(remaining_in_buf) as usize;

            let buf_start = bytes_written as usize;
            let buf_end = buf_start + to_write;

            // Look up existing extent in GEM
            let location = {
                let gem = self.gem.read().await;
                gem.lookup(self.id, vext_idx).cloned()
            };

            match location {
                // Exclusively owned: write straight through, no serialisation.
                Some(loc) if loc.ref_count == 1 => {
                    self.write_in_place(&loc, off_in_slot, &buf[buf_start..buf_end]).await?;
                }
                // Shared, or not yet mapped — the mapping is about to change,
                // so serialise per volume and re-read it under the lock.
                _ => {
                    let _vol = self.inner.lock().await;
                    let fresh = {
                        let gem = self.gem.read().await;
                        gem.lookup(self.id, vext_idx).cloned()
                    };
                    match fresh {
                        Some(loc) if loc.ref_count > 1 => {
                            self.cow_write(vext_idx, off_in_slot, &buf[buf_start..buf_end], &loc).await?;
                        }
                        Some(loc) => {
                            // Another writer allocated it, or the sharer went
                            // away, while we waited for the lock.
                            self.write_in_place(&loc, off_in_slot, &buf[buf_start..buf_end]).await?;
                        }
                        None => {
                            self.allocate_and_write(vext_idx, off_in_slot, &buf[buf_start..buf_end]).await?;
                        }
                    }
                }
            }

            bytes_written += to_write as u64;
            pos += to_write as u64;
        }

        Ok(bytes_written as usize)
    }

    async fn flush(&self) -> DriveResult<()> {
        // Collect unique slab IDs for this volume, then flush their devices
        let slab_ids: Vec<SlabId> = {
            let gem = self.gem.read().await;
            gem.volume_extents(&self.id)
                .map(|iter| {
                    iter.map(|(_, loc)| loc.slab_id)
                        .collect::<HashSet<_>>()
                        .into_iter()
                        .collect()
                })
                .unwrap_or_default()
        };

        let reg = self.registry.read().await;
        for slab_id in slab_ids {
            if let Some(slab) = reg.get(&slab_id) {
                slab.device().flush().await?;
            }
        }
        Ok(())
    }

    async fn discard(&self, offset: u64, len: u64) -> DriveResult<()> {
        let _vol = self.inner.lock().await;
        let mut pos = offset;
        let end = offset + len;

        while pos < end {
            let vext_idx = pos / self.slot_size;
            let off_in_slot = pos % self.slot_size;

            // Only discard full slots
            if off_in_slot == 0 && (end - pos) >= self.slot_size {
                let location = {
                    let gem = self.gem.read().await;
                    gem.lookup(self.id, vext_idx).cloned()
                };

                if let Some(loc) = location {
                    {
                        let mut gem = self.gem.write().await;
                        gem.remove(self.id, vext_idx);
                    }
                    {
                        let mut reg = self.registry.write().await;
                        if let Some(slab) = reg.get_mut(&loc.slab_id) {
                            if let Err(e) = slab.dec_ref(loc.slot_idx).await {
                                tracing::warn!(
                                    volume = %self.id, slot = loc.slot_idx, extent = vext_idx,
                                    "shrink could not release extent: {e}"
                                );
                            }
                        }
                    }
                }
            }

            let remaining = self.slot_size - off_in_slot;
            pos += remaining;
        }

        Ok(())
    }

    fn smart_status(&self) -> DriveResult<SmartData> {
        Ok(SmartData { healthy: true, ..Default::default() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;
    use crate::drive::slab::Slab;
    use crate::raid::{RaidArray, RaidLevel};

    async fn setup_test_volume(
        slot_size: u64,
    ) -> (Arc<ThinVolumeHandle>, Vec<String>) {
        let test_id = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::env::temp_dir().join("stormblock-volume-test");
        std::fs::create_dir_all(&dir).unwrap();

        // Create 2 file devices for RAID 1
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

        // Format a slab on the RAID array
        let slab = Slab::format(backing, slot_size, StorageTier::Hot)
            .await
            .unwrap();

        let mut registry = SlabRegistry::new();
        registry.add(slab);
        let registry = Arc::new(tokio::sync::RwLock::new(registry));
        let gem = Arc::new(tokio::sync::RwLock::new(GlobalExtentMap::new()));

        let vol = ThinVolume::new("test-vol".to_string(), 128 * 1024 * 1024, slot_size);
        let handle = Arc::new(ThinVolumeHandle::new(
            vol,
            gem,
            registry,
            PlacementPolicy::default(),
        ));

        (handle, paths)
    }

    fn cleanup(paths: &[String]) {
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }

    #[tokio::test]
    async fn write_allocates_and_read_returns_data() {
        let (handle, paths) = setup_test_volume(4096).await;

        let data = vec![0xAB_u8; 4096];
        let written = handle.write(0, &data).await.unwrap();
        assert_eq!(written, 4096);

        let mut buf = vec![0u8; 4096];
        let read = handle.read(0, &mut buf).await.unwrap();
        assert_eq!(read, 4096);
        assert_eq!(buf, data);

        assert_eq!(handle.extent_count().await, 1);
        assert!(handle.allocated().await > 0);

        cleanup(&paths);
    }

    #[tokio::test]
    async fn read_unallocated_returns_zeros() {
        let (handle, paths) = setup_test_volume(4096).await;

        let mut buf = vec![0xFF_u8; 4096];
        let read = handle.read(0, &mut buf).await.unwrap();
        assert_eq!(read, 4096);
        assert!(buf.iter().all(|&b| b == 0));

        cleanup(&paths);
    }

    #[tokio::test]
    async fn write_at_different_extents() {
        let (handle, paths) = setup_test_volume(4096).await;

        let data_a = vec![0xAA_u8; 4096];
        let data_b = vec![0xBB_u8; 4096];

        handle.write(0, &data_a).await.unwrap();
        handle.write(4096, &data_b).await.unwrap();

        let mut buf = vec![0u8; 4096];
        handle.read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xAA));

        handle.read(4096, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xBB));

        assert_eq!(handle.extent_count().await, 2);
        cleanup(&paths);
    }

    /// Concurrent writers to the same unmapped extent must not each allocate
    /// a slot — the second would leak one and discard the first writer's data.
    /// The narrowed write lock re-checks the mapping before allocating, and
    /// this is what pins that.
    #[tokio::test]
    async fn concurrent_first_writes_to_one_extent_allocate_once() {
        let (handle, paths) = setup_test_volume(4096).await;

        let free_before = handle.registry().read().await.total_free_slots();

        // Eight writers race on the same virtual extent.
        let mut tasks = Vec::new();
        for i in 0..8u8 {
            let h = handle.clone();
            tasks.push(tokio::spawn(async move {
                h.write(0, &vec![0xB0 + i; 4096]).await
            }));
        }
        for t in tasks {
            t.await.unwrap().unwrap();
        }

        assert_eq!(handle.extent_count().await, 1, "exactly one extent mapped");
        assert_eq!(
            handle.registry().read().await.total_free_slots(),
            free_before - 1,
            "exactly one slot consumed"
        );

        // Whichever writer won, the extent holds one of their patterns whole.
        let mut buf = vec![0u8; 4096];
        handle.read(0, &mut buf).await.unwrap();
        let first = buf[0];
        assert!((0xB0..0xB8).contains(&first), "unexpected content {first:#x}");
        assert!(buf.iter().all(|&b| b == first), "extent must not be torn");

        cleanup(&paths);
    }

    /// Writes to different extents must proceed concurrently rather than
    /// queueing behind one another on a volume-wide lock.
    #[tokio::test]
    async fn concurrent_writes_to_distinct_extents_all_land() {
        let (handle, paths) = setup_test_volume(4096).await;

        let mut tasks = Vec::new();
        for i in 0..16u64 {
            let h = handle.clone();
            tasks.push(tokio::spawn(async move {
                h.write(i * 4096, &vec![(0x40 + i) as u8; 4096]).await
            }));
        }
        for t in tasks {
            t.await.unwrap().unwrap();
        }

        assert_eq!(handle.extent_count().await, 16);
        for i in 0..16u64 {
            let mut buf = vec![0u8; 4096];
            handle.read(i * 4096, &mut buf).await.unwrap();
            assert!(
                buf.iter().all(|&b| b == (0x40 + i) as u8),
                "extent {i} content wrong"
            );
        }

        cleanup(&paths);
    }

    /// A single large write must allocate every extent it spans. Reported
    /// allocation stalling behind what was actually written is #33.
    #[tokio::test]
    async fn one_large_write_allocates_every_extent_it_spans() {
        let slot = 1024 * 1024;
        let (handle, paths) = setup_test_volume(slot).await;

        // 16 MB in a single call spans 16 slots.
        let data = vec![0xA7u8; 16 * slot as usize];
        let n = handle.write(0, &data).await.unwrap();
        assert_eq!(n, data.len());

        assert_eq!(handle.extent_count().await, 16, "one extent per slot spanned");
        assert_eq!(handle.allocated().await, 16 * slot, "allocated must track the write");

        // And it must read back whole.
        let mut buf = vec![0u8; data.len()];
        handle.read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xA7));

        cleanup(&paths);
    }

    /// Writing at a high offset must allocate that extent, not silently store
    /// data the map does not know about.
    #[tokio::test]
    async fn write_at_high_offset_allocates_and_reports() {
        let slot = 1024 * 1024;
        let (handle, paths) = setup_test_volume(slot).await;

        let off = 40 * slot;
        handle.write(off, &vec![0xBBu8; slot as usize]).await.unwrap();

        assert_eq!(handle.extent_count().await, 1);
        assert_eq!(handle.allocated().await, slot);

        let mut buf = vec![0u8; slot as usize];
        handle.read(off, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xBB), "data at a high offset must persist");

        cleanup(&paths);
    }

    #[tokio::test]
    async fn flush_works() {
        let (handle, paths) = setup_test_volume(4096).await;
        handle.write(0, &[0xCC_u8; 4096]).await.unwrap();
        handle.flush().await.unwrap();
        cleanup(&paths);
    }

    /// The #25 symptom: allocation must come back down when data is discarded,
    /// and the freed slots must return to the slab, not just leave the GEM.
    #[tokio::test]
    async fn discard_reclaims_extents_and_slab_slots() {
        let (handle, paths) = setup_test_volume(4096).await;

        let free_before = {
            let reg = handle.registry().read().await;
            reg.total_free_slots()
        };

        // Allocate four extents.
        for i in 0..4u64 {
            handle.write(i * 4096, &[0xAB_u8; 4096]).await.unwrap();
        }
        assert_eq!(handle.extent_count().await, 4);
        assert_eq!(handle.allocated().await, 4 * 4096);
        {
            let reg = handle.registry().read().await;
            assert_eq!(reg.total_free_slots(), free_before - 4);
        }

        // Discard the middle two.
        handle.discard(4096, 2 * 4096).await.unwrap();

        assert_eq!(handle.extent_count().await, 2);
        assert_eq!(handle.allocated().await, 2 * 4096);
        {
            let reg = handle.registry().read().await;
            assert_eq!(
                reg.total_free_slots(),
                free_before - 2,
                "discarded slots must return to the slab"
            );
        }

        // Discarded regions read back as zeros (we advertise LBPRZ).
        let mut buf = vec![0xFF_u8; 4096];
        handle.read(4096, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0));

        // Untouched extents still hold their data.
        let mut buf = vec![0u8; 4096];
        handle.read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xAB));

        cleanup(&paths);
    }

    /// A discard smaller than the reclaim granularity must not silently drop
    /// data — it frees nothing and the extent stays readable.
    #[tokio::test]
    async fn partial_discard_frees_nothing() {
        let (handle, paths) = setup_test_volume(65536).await;

        handle.write(0, &[0xCD_u8; 65536]).await.unwrap();
        assert_eq!(handle.extent_count().await, 1);
        assert_eq!(handle.discard_granularity(), 65536);

        // Half a slot — not enough to reclaim.
        handle.discard(0, 32768).await.unwrap();
        assert_eq!(handle.extent_count().await, 1);

        let mut buf = vec![0u8; 65536];
        handle.read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xCD), "partial discard must not lose data");

        cleanup(&paths);
    }
}
