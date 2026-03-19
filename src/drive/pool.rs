//! DiskPool — on-disk header, free-space management, VDrive allocation/deletion.
//!
//! A DiskPool manages a physical disk (or file) as a pool of VDrives.
//! The first 1 MB is reserved for the pool header and VDrive table.

use std::sync::Arc;

use serde::{Serialize, Deserialize};
use uuid::Uuid;

use super::vdrive::VDrive;
use super::{BlockDevice, DriveError, DriveResult};

/// Pool header magic: "STRMPOOL"
pub const POOL_MAGIC: [u8; 8] = *b"STRMPOOL";

/// Current pool header version.
pub const POOL_VERSION: u32 = 1;

/// Data starts at 1 MB offset.
pub const POOL_DATA_OFFSET: u64 = 1024 * 1024;

/// Default alignment for VDrive allocations (1 MB).
pub const POOL_ALIGNMENT: u64 = 1024 * 1024;

/// Maximum VDrives per pool.
pub const MAX_VDRIVES: u32 = 256;

/// VDrive table starts at offset 0x200 (512 bytes into the header block).
const VDRIVE_TABLE_OFFSET: u64 = 0x200;

/// Each VDrive entry is 128 bytes on disk.
const VDRIVE_ENTRY_SIZE: usize = 128;

/// State of a VDrive slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VDriveState {
    Free = 0,
    Active = 1,
    InArray = 2,
}

impl From<u8> for VDriveState {
    fn from(v: u8) -> Self {
        match v {
            1 => VDriveState::Active,
            2 => VDriveState::InArray,
            _ => VDriveState::Free,
        }
    }
}

/// On-disk VDrive entry (128 bytes serialized).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VDriveEntry {
    pub uuid: Uuid,
    pub start_offset: u64,
    pub size: u64,
    pub state: VDriveState,
    pub array_uuid: Uuid,
    pub label: String,
    pub create_time: u64,
    pub checksum: u32,
}

impl VDriveEntry {
    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = vec![0u8; VDRIVE_ENTRY_SIZE];
        buf[0..16].copy_from_slice(self.uuid.as_bytes());
        buf[16..24].copy_from_slice(&self.start_offset.to_le_bytes());
        buf[24..32].copy_from_slice(&self.size.to_le_bytes());
        buf[32] = self.state as u8;
        buf[33..49].copy_from_slice(self.array_uuid.as_bytes());
        // Label: up to 63 bytes at offset 49..112
        let label_bytes = self.label.as_bytes();
        let copy_len = label_bytes.len().min(63);
        buf[49..49 + copy_len].copy_from_slice(&label_bytes[..copy_len]);
        buf[112..120].copy_from_slice(&self.create_time.to_le_bytes());
        // Compute CRC32C over bytes 0..120
        let crc = crc32c::crc32c(&buf[..120]);
        buf[120..124].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < VDRIVE_ENTRY_SIZE {
            return None;
        }
        let mut uuid_bytes = [0u8; 16];
        uuid_bytes.copy_from_slice(&data[0..16]);
        let uuid = Uuid::from_bytes(uuid_bytes);

        // All-zero UUID means free slot
        if uuid.is_nil() {
            return None;
        }

        let start_offset = u64::from_le_bytes(data[16..24].try_into().unwrap());
        let size = u64::from_le_bytes(data[24..32].try_into().unwrap());
        let state = VDriveState::from(data[32]);

        let mut arr_bytes = [0u8; 16];
        arr_bytes.copy_from_slice(&data[33..49]);
        let array_uuid = Uuid::from_bytes(arr_bytes);

        // Read label (null-terminated string in bytes 49..112)
        let label_raw = &data[49..112];
        let label_end = label_raw.iter().position(|&b| b == 0).unwrap_or(63);
        let label = String::from_utf8_lossy(&label_raw[..label_end]).to_string();

        let create_time = u64::from_le_bytes(data[112..120].try_into().unwrap());
        let stored_crc = u32::from_le_bytes(data[120..124].try_into().unwrap());

        // Verify checksum
        let computed_crc = crc32c::crc32c(&data[..120]);
        if stored_crc != computed_crc {
            return None;
        }

        Some(VDriveEntry {
            uuid,
            start_offset,
            size,
            state,
            array_uuid,
            label,
            create_time,
            checksum: stored_crc,
        })
    }
}

/// On-disk pool header (first 512 bytes).
#[derive(Debug, Clone)]
pub struct PoolHeader {
    pub pool_uuid: Uuid,
    pub total_capacity: u64,
    pub data_offset: u64,
    pub alignment: u64,
    pub vdrive_count: u32,
    pub max_vdrives: u32,
    pub create_time: u64,
    pub update_time: u64,
}

impl PoolHeader {
    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = vec![0u8; 512];
        buf[0..8].copy_from_slice(&POOL_MAGIC);
        buf[8..12].copy_from_slice(&POOL_VERSION.to_le_bytes());
        buf[12..28].copy_from_slice(self.pool_uuid.as_bytes());
        buf[28..36].copy_from_slice(&self.total_capacity.to_le_bytes());
        buf[36..44].copy_from_slice(&self.data_offset.to_le_bytes());
        buf[44..52].copy_from_slice(&self.alignment.to_le_bytes());
        buf[52..56].copy_from_slice(&self.vdrive_count.to_le_bytes());
        buf[56..60].copy_from_slice(&self.max_vdrives.to_le_bytes());
        buf[60..68].copy_from_slice(&self.create_time.to_le_bytes());
        buf[68..76].copy_from_slice(&self.update_time.to_le_bytes());
        // CRC32C at offset 76..80
        let crc = crc32c::crc32c(&buf[..76]);
        buf[76..80].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    fn from_bytes(data: &[u8]) -> Result<Self, DriveError> {
        if data.len() < 80 {
            return Err(DriveError::Other(anyhow::anyhow!("pool header too short")));
        }
        if &data[0..8] != POOL_MAGIC {
            return Err(DriveError::Other(anyhow::anyhow!("bad pool magic")));
        }
        let version = u32::from_le_bytes(data[8..12].try_into().unwrap());
        if version != POOL_VERSION {
            return Err(DriveError::Other(anyhow::anyhow!(
                "pool version {version}, expected {POOL_VERSION}"
            )));
        }

        let mut uuid_bytes = [0u8; 16];
        uuid_bytes.copy_from_slice(&data[12..28]);
        let pool_uuid = Uuid::from_bytes(uuid_bytes);

        let total_capacity = u64::from_le_bytes(data[28..36].try_into().unwrap());
        let data_offset = u64::from_le_bytes(data[36..44].try_into().unwrap());
        let alignment = u64::from_le_bytes(data[44..52].try_into().unwrap());
        let vdrive_count = u32::from_le_bytes(data[52..56].try_into().unwrap());
        let max_vdrives = u32::from_le_bytes(data[56..60].try_into().unwrap());
        let create_time = u64::from_le_bytes(data[60..68].try_into().unwrap());
        let update_time = u64::from_le_bytes(data[68..76].try_into().unwrap());

        // Verify CRC
        let stored_crc = u32::from_le_bytes(data[76..80].try_into().unwrap());
        let computed = crc32c::crc32c(&data[..76]);
        if stored_crc != computed {
            return Err(DriveError::Other(anyhow::anyhow!("pool header CRC mismatch")));
        }

        Ok(PoolHeader {
            pool_uuid,
            total_capacity,
            data_offset,
            alignment,
            vdrive_count,
            max_vdrives,
            create_time,
            update_time,
        })
    }
}

/// A DiskPool that manages VDrives on a physical device.
pub struct DiskPool {
    pub header: PoolHeader,
    pub entries: Vec<VDriveEntry>,
    device: Arc<dyn BlockDevice>,
    path: String,
}

impl DiskPool {
    /// Format a device as a new DiskPool.
    pub async fn format(device: Arc<dyn BlockDevice>, path: &str) -> DriveResult<Self> {
        let total = device.capacity_bytes();
        if total <= POOL_DATA_OFFSET {
            return Err(DriveError::Other(anyhow::anyhow!(
                "device too small for pool ({} bytes, need > {})",
                total, POOL_DATA_OFFSET
            )));
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let header = PoolHeader {
            pool_uuid: Uuid::new_v4(),
            total_capacity: total,
            data_offset: POOL_DATA_OFFSET,
            alignment: POOL_ALIGNMENT,
            vdrive_count: 0,
            max_vdrives: MAX_VDRIVES,
            create_time: now,
            update_time: now,
        };

        // Write header
        let header_bytes = header.to_bytes();
        device.write(0, &header_bytes).await?;

        // Zero out VDrive table
        let zero_table = vec![0u8; MAX_VDRIVES as usize * VDRIVE_ENTRY_SIZE];
        device.write(VDRIVE_TABLE_OFFSET, &zero_table).await?;
        device.flush().await?;

        Ok(DiskPool {
            header,
            entries: Vec::new(),
            device,
            path: path.to_string(),
        })
    }

    /// Open an existing DiskPool from a device.
    pub async fn open(device: Arc<dyn BlockDevice>, path: &str) -> DriveResult<Self> {
        // Read header
        let mut header_buf = vec![0u8; 512];
        device.read(0, &mut header_buf).await?;
        let header = PoolHeader::from_bytes(&header_buf)?;

        // Read VDrive table
        let table_size = header.max_vdrives as usize * VDRIVE_ENTRY_SIZE;
        let mut table_buf = vec![0u8; table_size];
        device.read(VDRIVE_TABLE_OFFSET, &mut table_buf).await?;

        let mut entries = Vec::new();
        for i in 0..header.max_vdrives as usize {
            let offset = i * VDRIVE_ENTRY_SIZE;
            if let Some(entry) = VDriveEntry::from_bytes(&table_buf[offset..offset + VDRIVE_ENTRY_SIZE]) {
                entries.push(entry);
            }
        }

        Ok(DiskPool {
            header,
            entries,
            device,
            path: path.to_string(),
        })
    }

    /// Device path this pool is on.
    pub fn device_path(&self) -> String {
        self.path.clone()
    }

    /// Number of active VDrives.
    pub fn vdrive_count(&self) -> u32 {
        self.entries.len() as u32
    }

    /// Data offset (where VDrive data begins).
    pub fn data_offset(&self) -> u64 {
        self.header.data_offset
    }

    /// List all VDrive entries.
    pub fn list_vdrives(&self) -> &[VDriveEntry] {
        &self.entries
    }

    /// Pool UUID.
    pub fn pool_uuid(&self) -> Uuid {
        self.header.pool_uuid
    }

    /// Total capacity of the underlying device.
    pub fn total_capacity(&self) -> u64 {
        self.header.total_capacity
    }

    /// Usable data capacity (total - header).
    pub fn data_capacity(&self) -> u64 {
        self.header.total_capacity.saturating_sub(self.header.data_offset)
    }

    /// Total allocated space (sum of all VDrive sizes).
    pub fn allocated(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }

    /// Free space available for new VDrives.
    pub fn free_space(&self) -> u64 {
        self.data_capacity().saturating_sub(self.allocated())
    }

    /// Find the largest contiguous free region.
    pub fn largest_free_region(&self) -> u64 {
        let alignment = self.header.alignment;
        let data_end = self.header.total_capacity;
        let data_start = self.header.data_offset;

        // Build sorted list of allocated regions
        let mut regions: Vec<(u64, u64)> = self.entries.iter()
            .map(|e| (e.start_offset, e.start_offset + e.size))
            .collect();
        regions.sort_by_key(|r| r.0);

        let mut largest = 0u64;
        let mut cursor = data_start;

        for (start, end) in &regions {
            if *start > cursor {
                let gap = start - cursor;
                if gap > largest {
                    largest = gap;
                }
            }
            if *end > cursor {
                cursor = *end;
            }
            // Align cursor
            cursor = align_up(cursor, alignment);
        }

        // Gap after last region
        if data_end > cursor {
            let gap = data_end - cursor;
            if gap > largest {
                largest = gap;
            }
        }

        largest
    }

    /// Allocate a new VDrive with the given size and label.
    /// Uses first-fit allocation with 1 MB alignment.
    pub async fn create_vdrive(&mut self, size: u64, label: &str) -> DriveResult<VDriveEntry> {
        if self.entries.len() >= self.header.max_vdrives as usize {
            return Err(DriveError::Other(anyhow::anyhow!("max VDrives reached")));
        }

        let alignment = self.header.alignment;
        let aligned_size = align_up(size, alignment);
        let data_end = self.header.total_capacity;

        // First-fit: find a gap
        let mut regions: Vec<(u64, u64)> = self.entries.iter()
            .map(|e| (e.start_offset, e.start_offset + e.size))
            .collect();
        regions.sort_by_key(|r| r.0);

        let mut cursor = self.header.data_offset;
        let mut found_offset = None;

        for (start, end) in &regions {
            let aligned_cursor = align_up(cursor, alignment);
            if aligned_cursor + aligned_size <= *start {
                found_offset = Some(aligned_cursor);
                break;
            }
            if *end > cursor {
                cursor = *end;
            }
        }

        if found_offset.is_none() {
            let aligned_cursor = align_up(cursor, alignment);
            if aligned_cursor + aligned_size <= data_end {
                found_offset = Some(aligned_cursor);
            }
        }

        let start_offset = found_offset.ok_or_else(|| {
            DriveError::Other(anyhow::anyhow!(
                "not enough contiguous space for {} byte VDrive (largest free: {} bytes)",
                aligned_size,
                self.largest_free_region()
            ))
        })?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let entry = VDriveEntry {
            uuid: Uuid::new_v4(),
            start_offset,
            size: aligned_size,
            state: VDriveState::Active,
            array_uuid: Uuid::nil(),
            label: label.to_string(),
            create_time: now,
            checksum: 0,
        };

        self.entries.push(entry.clone());
        self.header.vdrive_count = self.entries.len() as u32;
        self.header.update_time = now;

        self.persist().await?;

        Ok(entry)
    }

    /// Delete a VDrive by UUID.
    pub async fn delete_vdrive(&mut self, uuid: Uuid) -> DriveResult<()> {
        let idx = self.entries.iter().position(|e| e.uuid == uuid)
            .ok_or_else(|| DriveError::Other(anyhow::anyhow!("VDrive {} not found", uuid)))?;

        if self.entries[idx].state == VDriveState::InArray {
            return Err(DriveError::Other(anyhow::anyhow!(
                "VDrive {} is in an array — remove from array first", uuid
            )));
        }

        self.entries.remove(idx);
        self.header.vdrive_count = self.entries.len() as u32;
        self.header.update_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        self.persist().await?;
        Ok(())
    }

    /// Get a VDrive entry by UUID.
    pub fn get_vdrive(&self, uuid: &Uuid) -> Option<&VDriveEntry> {
        self.entries.iter().find(|e| e.uuid == *uuid)
    }

    /// Mark a VDrive as being in an array.
    pub async fn set_vdrive_in_array(&mut self, uuid: Uuid, array_uuid: Uuid) -> DriveResult<()> {
        let entry = self.entries.iter_mut().find(|e| e.uuid == uuid)
            .ok_or_else(|| DriveError::Other(anyhow::anyhow!("VDrive {} not found", uuid)))?;
        entry.state = VDriveState::InArray;
        entry.array_uuid = array_uuid;
        self.persist().await
    }

    /// Mark a VDrive as no longer in an array.
    pub async fn set_vdrive_active(&mut self, uuid: Uuid) -> DriveResult<()> {
        let entry = self.entries.iter_mut().find(|e| e.uuid == uuid)
            .ok_or_else(|| DriveError::Other(anyhow::anyhow!("VDrive {} not found", uuid)))?;
        entry.state = VDriveState::Active;
        entry.array_uuid = Uuid::nil();
        self.persist().await
    }

    /// Create a BlockDevice VDrive from an entry.
    pub fn open_vdrive(&self, uuid: &Uuid) -> DriveResult<VDrive> {
        let entry = self.get_vdrive(uuid)
            .ok_or_else(|| DriveError::Other(anyhow::anyhow!("VDrive {} not found", uuid)))?;
        Ok(VDrive::new(
            self.header.pool_uuid,
            self.device.clone(),
            entry.start_offset,
            entry.size,
            entry.label.clone(),
        ))
    }

    /// Get reference to underlying device.
    pub fn device(&self) -> &Arc<dyn BlockDevice> {
        &self.device
    }

    /// Persist header and VDrive table to disk.
    async fn persist(&self) -> DriveResult<()> {
        // Write header
        let header_bytes = self.header.to_bytes();
        self.device.write(0, &header_bytes).await?;

        // Write VDrive table — zero first, then write entries
        let table_size = self.header.max_vdrives as usize * VDRIVE_ENTRY_SIZE;
        let mut table = vec![0u8; table_size];
        for (i, entry) in self.entries.iter().enumerate() {
            let offset = i * VDRIVE_ENTRY_SIZE;
            let entry_bytes = entry.to_bytes();
            table[offset..offset + VDRIVE_ENTRY_SIZE].copy_from_slice(&entry_bytes);
        }
        self.device.write(VDRIVE_TABLE_OFFSET, &table).await?;
        self.device.flush().await?;
        Ok(())
    }
}

/// Align a value up to the given alignment.
fn align_up(value: u64, alignment: u64) -> u64 {
    if alignment == 0 {
        return value;
    }
    let remainder = value % alignment;
    if remainder == 0 {
        value
    } else {
        value + alignment - remainder
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;

    async fn create_pool_device(size: u64) -> (Arc<dyn BlockDevice>, String) {
        let dir = std::env::temp_dir().join("stormblock-pool-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("pool-{}.bin", Uuid::new_v4().simple()));
        let path_str = path.to_str().unwrap().to_string();
        let dev = FileDevice::open_with_capacity(&path_str, size).await.unwrap();
        (Arc::new(dev), path_str)
    }

    #[tokio::test]
    async fn pool_format_and_open() {
        let (dev, path) = create_pool_device(100 * 1024 * 1024).await;
        let pool = DiskPool::format(dev.clone(), &path).await.unwrap();
        let uuid = pool.pool_uuid();
        assert_eq!(pool.entries.len(), 0);
        assert!(pool.free_space() > 0);

        // Re-open
        let pool2 = DiskPool::open(dev, &path).await.unwrap();
        assert_eq!(pool2.pool_uuid(), uuid);
        assert_eq!(pool2.entries.len(), 0);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn pool_create_and_delete_vdrive() {
        let (dev, path) = create_pool_device(100 * 1024 * 1024).await;
        let mut pool = DiskPool::format(dev.clone(), &path).await.unwrap();

        let entry = pool.create_vdrive(10 * 1024 * 1024, "data0").await.unwrap();
        assert_eq!(entry.label, "data0");
        assert_eq!(entry.state, VDriveState::Active);
        assert_eq!(pool.entries.len(), 1);

        // Re-open and verify persistence
        let pool2 = DiskPool::open(dev.clone(), &path).await.unwrap();
        assert_eq!(pool2.entries.len(), 1);
        assert_eq!(pool2.entries[0].uuid, entry.uuid);
        assert_eq!(pool2.entries[0].label, "data0");

        // Delete
        pool.delete_vdrive(entry.uuid).await.unwrap();
        assert_eq!(pool.entries.len(), 0);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn pool_multiple_vdrives() {
        let (dev, path) = create_pool_device(100 * 1024 * 1024).await;
        let mut pool = DiskPool::format(dev, &path).await.unwrap();

        let e1 = pool.create_vdrive(20 * 1024 * 1024, "v1").await.unwrap();
        let e2 = pool.create_vdrive(20 * 1024 * 1024, "v2").await.unwrap();
        let e3 = pool.create_vdrive(20 * 1024 * 1024, "v3").await.unwrap();

        assert_eq!(pool.entries.len(), 3);
        assert!(e2.start_offset > e1.start_offset);
        assert!(e3.start_offset > e2.start_offset);

        // Free space should have decreased
        let free = pool.free_space();
        assert!(free < 100 * 1024 * 1024 - POOL_DATA_OFFSET);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn pool_vdrive_io() {
        let (dev, path) = create_pool_device(100 * 1024 * 1024).await;
        let mut pool = DiskPool::format(dev, &path).await.unwrap();

        let entry = pool.create_vdrive(10 * 1024 * 1024, "io-test").await.unwrap();
        let vdrive = pool.open_vdrive(&entry.uuid).unwrap();

        // Write and read
        let data = vec![0xFE_u8; 4096];
        vdrive.write(0, &data).await.unwrap();
        let mut buf = vec![0u8; 4096];
        vdrive.read(0, &mut buf).await.unwrap();
        assert_eq!(buf, data);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn pool_fragmentation() {
        let (dev, path) = create_pool_device(100 * 1024 * 1024).await;
        let mut pool = DiskPool::format(dev, &path).await.unwrap();

        // Create 3 VDrives, delete the middle one, allocate in the gap
        let e1 = pool.create_vdrive(10 * 1024 * 1024, "a").await.unwrap();
        let e2 = pool.create_vdrive(10 * 1024 * 1024, "b").await.unwrap();
        let _e3 = pool.create_vdrive(10 * 1024 * 1024, "c").await.unwrap();

        pool.delete_vdrive(e2.uuid).await.unwrap();
        assert_eq!(pool.entries.len(), 2);

        // New VDrive should fit in the gap left by e2
        let e4 = pool.create_vdrive(10 * 1024 * 1024, "d").await.unwrap();
        assert_eq!(e4.start_offset, e2.start_offset);
        assert_eq!(pool.entries.len(), 3);

        // VDrive too big for the gap should go at end
        pool.delete_vdrive(e1.uuid).await.unwrap();
        // Now there's a gap at e1 position (10MB) and space after e3
        let _e5 = pool.create_vdrive(5 * 1024 * 1024, "small").await.unwrap();

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn pool_in_array_prevents_delete() {
        let (dev, path) = create_pool_device(100 * 1024 * 1024).await;
        let mut pool = DiskPool::format(dev, &path).await.unwrap();

        let entry = pool.create_vdrive(10 * 1024 * 1024, "locked").await.unwrap();
        pool.set_vdrive_in_array(entry.uuid, Uuid::new_v4()).await.unwrap();

        let result = pool.delete_vdrive(entry.uuid).await;
        assert!(result.is_err());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pool_header_roundtrip() {
        let header = PoolHeader {
            pool_uuid: Uuid::new_v4(),
            total_capacity: 100 * 1024 * 1024,
            data_offset: POOL_DATA_OFFSET,
            alignment: POOL_ALIGNMENT,
            vdrive_count: 2,
            max_vdrives: MAX_VDRIVES,
            create_time: 1234567890,
            update_time: 1234567900,
        };

        let bytes = header.to_bytes();
        let decoded = PoolHeader::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.pool_uuid, header.pool_uuid);
        assert_eq!(decoded.total_capacity, header.total_capacity);
        assert_eq!(decoded.vdrive_count, 2);
    }

    #[test]
    fn vdrive_entry_roundtrip() {
        let entry = VDriveEntry {
            uuid: Uuid::new_v4(),
            start_offset: 1024 * 1024,
            size: 10 * 1024 * 1024,
            state: VDriveState::Active,
            array_uuid: Uuid::nil(),
            label: "test-drive".to_string(),
            create_time: 1234567890,
            checksum: 0,
        };

        let bytes = entry.to_bytes();
        let decoded = VDriveEntry::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.uuid, entry.uuid);
        assert_eq!(decoded.start_offset, entry.start_offset);
        assert_eq!(decoded.size, entry.size);
        assert_eq!(decoded.label, "test-drive");
    }
}
