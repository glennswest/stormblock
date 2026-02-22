//! Thin volume — virtual size, on-demand extent allocation.
//!
//! `ThinVolume` implements `BlockDevice`, so target protocols see volumes
//! as plain block devices. Physical storage is allocated on first write
//! (allocate-on-write) from the extent allocator.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use serde::{Serialize, Deserialize};

use crate::drive::{BlockDevice, DeviceId, DriveError, DriveResult, DriveType, SmartData};
use crate::raid::RaidArrayId;
use super::extent::{ExtentAllocator, VolumeId};

/// A physical extent with reference counting for COW snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhysicalExtent {
    pub array_id: RaidArrayId,
    pub offset: u64,
    pub length: u64,
    pub ref_count: u32,
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
            VolumeError::NoSpace => write!(f, "no free extents available"),
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

/// A thin-provisioned volume backed by a RAID array.
///
/// Virtual blocks are mapped to physical extents on demand.
/// Implements `BlockDevice` for use by target protocols.
pub struct ThinVolume {
    pub(crate) id: VolumeId,
    pub(crate) name: String,
    pub(crate) virtual_size: u64,
    pub(crate) allocated: u64,
    /// Virtual extent index → physical extent mapping.
    pub(crate) extent_map: BTreeMap<u64, PhysicalExtent>,
    pub(crate) array_id: RaidArrayId,
    pub(crate) backing_device: Arc<dyn BlockDevice>,
    pub(crate) allocator: Arc<tokio::sync::Mutex<ExtentAllocator>>,
    pub(crate) device_id: DeviceId,
}

impl ThinVolume {
    pub fn new(
        name: String,
        virtual_size: u64,
        array_id: RaidArrayId,
        backing_device: Arc<dyn BlockDevice>,
        allocator: Arc<tokio::sync::Mutex<ExtentAllocator>>,
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
            allocated: 0,
            extent_map: BTreeMap::new(),
            array_id,
            backing_device,
            allocator,
            device_id,
        }
    }

    /// Rebuild a volume from persisted metadata (recovery path).
    pub fn restore(
        id: VolumeId,
        name: String,
        virtual_size: u64,
        array_id: RaidArrayId,
        extent_map: BTreeMap<u64, PhysicalExtent>,
        backing_device: Arc<dyn BlockDevice>,
        allocator: Arc<tokio::sync::Mutex<ExtentAllocator>>,
    ) -> Self {
        let allocated: u64 = extent_map.values().map(|e| e.length).sum();
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
            allocated,
            extent_map,
            array_id,
            backing_device,
            allocator,
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

    pub fn allocated(&self) -> u64 {
        self.allocated
    }

    pub fn extent_count(&self) -> usize {
        self.extent_map.len()
    }

    /// Resolve a virtual byte offset to (extent_index, offset_within_extent).
    fn resolve_extent(&self, offset: u64) -> (u64, u64) {
        let extent_size = self.backing_device.optimal_io_size() as u64;
        // We use the allocator's extent size, not optimal_io_size
        // For now, get it from the extent_map entries or use a default
        let es = self.extent_map.values().next()
            .map(|e| e.length)
            .unwrap_or(super::extent::DEFAULT_EXTENT_SIZE);
        let idx = offset / es;
        let off = offset % es;
        let _ = extent_size; // suppress unused
        (idx, off)
    }

    /// Get the extent size from the allocator.
    async fn extent_size(&self) -> u64 {
        let alloc = self.allocator.lock().await;
        alloc.extent_size()
    }

    /// Allocate a new physical extent for the given virtual extent index.
    async fn allocate_extent(&mut self, vext_idx: u64) -> Result<&PhysicalExtent, VolumeError> {
        let mut alloc = self.allocator.lock().await;
        let extents = alloc.allocate(self.array_id, 1)
            .ok_or(VolumeError::NoSpace)?;
        let ext = &extents[0];
        let pext = PhysicalExtent {
            array_id: ext.array_id,
            offset: ext.offset,
            length: ext.length,
            ref_count: 1,
        };
        drop(alloc);

        self.allocated += pext.length;
        self.extent_map.insert(vext_idx, pext);
        Ok(&self.extent_map[&vext_idx])
    }

    /// COW: if extent is shared, copy to a new extent and return it.
    async fn cow_extent(&mut self, vext_idx: u64) -> Result<&PhysicalExtent, VolumeError> {
        let needs_cow = self.extent_map.get(&vext_idx)
            .map(|e| e.ref_count > 1)
            .unwrap_or(false);

        if needs_cow {
            let old = self.extent_map[&vext_idx].clone();
            let extent_size = old.length;

            // Allocate new extent
            let mut alloc = self.allocator.lock().await;
            let new_extents = alloc.allocate(self.array_id, 1)
                .ok_or(VolumeError::NoSpace)?;
            drop(alloc);

            let new_ext = &new_extents[0];

            // Copy data from old to new
            let mut buf = vec![0u8; extent_size as usize];
            self.backing_device.read(old.offset, &mut buf).await?;
            self.backing_device.write(new_ext.offset, &buf).await?;

            // Decrement old ref_count
            let old_entry = self.extent_map.get_mut(&vext_idx).unwrap();
            old_entry.ref_count -= 1;

            // If old ref_count hit 0, free it (shouldn't happen since we checked > 1)
            // Insert new extent
            let pext = PhysicalExtent {
                array_id: new_ext.array_id,
                offset: new_ext.offset,
                length: new_ext.length,
                ref_count: 1,
            };
            self.allocated += pext.length;
            self.extent_map.insert(vext_idx, pext);
        }

        Ok(&self.extent_map[&vext_idx])
    }
}

/// `ThinVolume` wrapped in a Mutex for interior mutability needed by `BlockDevice`.
pub struct ThinVolumeHandle {
    inner: tokio::sync::Mutex<ThinVolume>,
    device_id: DeviceId,
    virtual_size: AtomicU64,
}

impl ThinVolumeHandle {
    pub fn new(vol: ThinVolume) -> Self {
        let device_id = vol.device_id.clone();
        let virtual_size = AtomicU64::new(vol.virtual_size);
        ThinVolumeHandle {
            inner: tokio::sync::Mutex::new(vol),
            device_id,
            virtual_size,
        }
    }

    /// Resize the volume to `new_size` bytes.
    ///
    /// Growing is instant — allocate-on-write handles new space.
    /// Shrinking frees extents beyond the new boundary.
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
            // Shrink: free extents beyond new boundary
            let extent_size = {
                let alloc = vol.allocator.lock().await;
                alloc.extent_size()
            };
            let max_vext_idx = new_size / extent_size;

            // Collect indices to remove (at or beyond boundary)
            let to_remove: Vec<u64> = vol.extent_map.range(max_vext_idx..)
                .map(|(&idx, _)| idx)
                .collect();

            for idx in to_remove {
                if let Some(pext) = vol.extent_map.remove(&idx) {
                    vol.allocated -= pext.length;
                    if pext.ref_count <= 1 {
                        let mut alloc = vol.allocator.lock().await;
                        let ext = super::extent::Extent {
                            array_id: pext.array_id,
                            offset: pext.offset,
                            length: pext.length,
                        };
                        alloc.free(&ext);
                    }
                }
            }
        }

        vol.virtual_size = new_size;
        self.virtual_size.store(new_size, Ordering::Relaxed);
        Ok(())
    }

    pub async fn volume_id(&self) -> VolumeId {
        self.inner.lock().await.id
    }

    pub async fn name(&self) -> String {
        self.inner.lock().await.name.clone()
    }

    pub async fn allocated(&self) -> u64 {
        self.inner.lock().await.allocated
    }

    pub async fn extent_count(&self) -> usize {
        self.inner.lock().await.extent_count()
    }

    /// Access the inner ThinVolume for snapshot operations.
    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, ThinVolume> {
        self.inner.lock().await
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

    fn device_type(&self) -> DriveType {
        DriveType::File // Logical volume
    }

    async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        let vol = self.inner.lock().await;
        let extent_size = vol.extent_map.values().next()
            .map(|e| e.length)
            .unwrap_or({
                let alloc = vol.allocator.lock().await;
                alloc.extent_size()
            });

        let buf_len = buf.len() as u64;
        let mut bytes_read = 0u64;
        let mut pos = offset;

        while bytes_read < buf_len {
            let vext_idx = pos / extent_size;
            let off_in_extent = pos % extent_size;
            let remaining_in_extent = extent_size - off_in_extent;
            let remaining_in_buf = buf_len - bytes_read;
            let to_read = remaining_in_extent.min(remaining_in_buf) as usize;

            let buf_start = bytes_read as usize;
            let buf_end = buf_start + to_read;

            match vol.extent_map.get(&vext_idx) {
                Some(pext) => {
                    let phys_offset = pext.offset + off_in_extent;
                    vol.backing_device.read(phys_offset, &mut buf[buf_start..buf_end]).await?;
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

    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        let mut vol = self.inner.lock().await;
        let extent_size = vol.extent_map.values().next()
            .map(|e| e.length)
            .unwrap_or({
                let alloc = vol.allocator.lock().await;
                alloc.extent_size()
            });

        let buf_len = buf.len() as u64;
        let mut bytes_written = 0u64;
        let mut pos = offset;

        while bytes_written < buf_len {
            let vext_idx = pos / extent_size;
            let off_in_extent = pos % extent_size;
            let remaining_in_extent = extent_size - off_in_extent;
            let remaining_in_buf = buf_len - bytes_written;
            let to_write = remaining_in_extent.min(remaining_in_buf) as usize;

            let buf_start = bytes_written as usize;
            let buf_end = buf_start + to_write;

            // Check if extent exists
            let has_extent = vol.extent_map.contains_key(&vext_idx);
            if has_extent {
                // COW if shared
                vol.cow_extent(vext_idx).await.map_err(VolumeError::from)?;
                let pext = &vol.extent_map[&vext_idx];
                let phys_offset = pext.offset + off_in_extent;
                vol.backing_device.write(phys_offset, &buf[buf_start..buf_end]).await?;
            } else {
                // Allocate new extent
                vol.allocate_extent(vext_idx).await.map_err(VolumeError::from)?;
                let pext = &vol.extent_map[&vext_idx];
                let phys_offset = pext.offset + off_in_extent;
                vol.backing_device.write(phys_offset, &buf[buf_start..buf_end]).await?;
            }

            bytes_written += to_write as u64;
            pos += to_write as u64;
        }

        Ok(bytes_written as usize)
    }

    async fn flush(&self) -> DriveResult<()> {
        let vol = self.inner.lock().await;
        vol.backing_device.flush().await
    }

    async fn discard(&self, offset: u64, len: u64) -> DriveResult<()> {
        let mut vol = self.inner.lock().await;
        let extent_size = vol.extent_map.values().next()
            .map(|e| e.length)
            .unwrap_or({
                let alloc = vol.allocator.lock().await;
                alloc.extent_size()
            });

        let mut pos = offset;
        let end = offset + len;

        while pos < end {
            let vext_idx = pos / extent_size;
            let off_in_extent = pos % extent_size;

            // Only discard full extents
            if off_in_extent == 0 && (end - pos) >= extent_size {
                if let Some(pext) = vol.extent_map.remove(&vext_idx) {
                    vol.allocated -= pext.length;
                    if pext.ref_count <= 1 {
                        let mut alloc = vol.allocator.lock().await;
                        let ext = super::extent::Extent {
                            array_id: pext.array_id,
                            offset: pext.offset,
                            length: pext.length,
                        };
                        alloc.free(&ext);
                    }
                    // If shared, just remove our mapping, don't free the physical extent
                }
            }

            let remaining = extent_size - off_in_extent;
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
    use crate::raid::{RaidArray, RaidLevel, RaidArrayId};
    use crate::volume::extent::{ExtentAllocator, DEFAULT_EXTENT_SIZE};

    async fn setup_test_volume(extent_size: u64) -> (ThinVolumeHandle, Vec<String>) {
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
            "test-vol".to_string(),
            128 * 1024 * 1024, // 128 MB virtual
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
    async fn write_allocates_and_read_returns_data() {
        let (handle, paths) = setup_test_volume(DEFAULT_EXTENT_SIZE).await;

        // Write data
        let data = vec![0xAB_u8; 4096];
        let written = handle.write(0, &data).await.unwrap();
        assert_eq!(written, 4096);

        // Read back
        let mut buf = vec![0u8; 4096];
        let read = handle.read(0, &mut buf).await.unwrap();
        assert_eq!(read, 4096);
        assert_eq!(buf, data);

        // Verify extent was allocated
        assert_eq!(handle.extent_count().await, 1);
        assert!(handle.allocated().await > 0);

        cleanup(&paths);
    }

    #[tokio::test]
    async fn read_unallocated_returns_zeros() {
        let (handle, paths) = setup_test_volume(DEFAULT_EXTENT_SIZE).await;

        let mut buf = vec![0xFF_u8; 4096];
        let read = handle.read(0, &mut buf).await.unwrap();
        assert_eq!(read, 4096);
        assert!(buf.iter().all(|&b| b == 0));

        cleanup(&paths);
    }

    #[tokio::test]
    async fn write_at_different_extents() {
        let extent_size = 4096u64; // Small extents for testing
        let (handle, paths) = setup_test_volume(extent_size).await;

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

    #[tokio::test]
    async fn flush_works() {
        let (handle, paths) = setup_test_volume(DEFAULT_EXTENT_SIZE).await;
        handle.write(0, &[0xCC_u8; 4096]).await.unwrap();
        handle.flush().await.unwrap();
        cleanup(&paths);
    }
}
