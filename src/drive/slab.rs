//! Slab — extent store with fixed-size slots on a block device.
//!
//! Each device (or device region) is formatted as a Slab with a header,
//! a slot table, and a data region of 1 MB slots. Any volume can allocate
//! slots in any slab. This replaces the monolithic DiskPool/VDrive model
//! with organic, per-extent data placement.

use std::collections::HashMap;
use std::sync::Arc;

use bitvec::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{BlockDevice, DriveError, DriveResult};
use crate::placement::topology::StorageTier;
use crate::volume::extent::VolumeId;

/// Slab header magic: "STRMSLAB"
pub const SLAB_MAGIC: [u8; 8] = *b"STRMSLAB";

/// Current slab header version.
pub const SLAB_VERSION: u32 = 1;

/// Default slot size: 1 MB.
pub const DEFAULT_SLOT_SIZE: u64 = 1024 * 1024;

/// Slab header size on disk (4 KB).
const HEADER_SIZE: u64 = 4096;

/// Slot entry size on disk (64 bytes).
const SLOT_ENTRY_SIZE: u64 = 64;

/// Unique identifier for a slab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SlabId(pub Uuid);

impl SlabId {
    pub fn new() -> Self {
        SlabId(Uuid::new_v4())
    }
}

impl Default for SlabId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SlabId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// State of a slot in the slab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SlotState {
    Free = 0,
    Allocated = 1,
    Moving = 2,
}

impl From<u8> for SlotState {
    fn from(v: u8) -> Self {
        match v {
            1 => SlotState::Allocated,
            2 => SlotState::Moving,
            _ => SlotState::Free,
        }
    }
}

/// In-memory representation of a slot.
#[derive(Debug, Clone)]
pub struct Slot {
    pub state: SlotState,
    pub volume_id: VolumeId,
    pub virtual_extent_idx: u64,
    pub ref_count: u32,
    pub generation: u64,
}

impl Slot {
    fn free() -> Self {
        Slot {
            state: SlotState::Free,
            volume_id: VolumeId(Uuid::nil()),
            virtual_extent_idx: 0,
            ref_count: 0,
            generation: 0,
        }
    }

    fn to_bytes(&self) -> [u8; SLOT_ENTRY_SIZE as usize] {
        let mut buf = [0u8; SLOT_ENTRY_SIZE as usize];
        buf[0] = self.state as u8;
        // bytes 1..4 pad
        buf[4..20].copy_from_slice(self.volume_id.0.as_bytes());
        buf[20..28].copy_from_slice(&self.virtual_extent_idx.to_le_bytes());
        buf[28..32].copy_from_slice(&self.ref_count.to_le_bytes());
        buf[32..40].copy_from_slice(&self.generation.to_le_bytes());
        // bytes 40..60 reserved
        let crc = crc32c::crc32c(&buf[..60]);
        buf[60..64].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < SLOT_ENTRY_SIZE as usize {
            return None;
        }
        let stored_crc = u32::from_le_bytes(data[60..64].try_into().unwrap());
        let computed_crc = crc32c::crc32c(&data[..60]);
        if stored_crc != computed_crc {
            return None;
        }

        let state = SlotState::from(data[0]);
        let mut uuid_bytes = [0u8; 16];
        uuid_bytes.copy_from_slice(&data[4..20]);
        let volume_id = VolumeId(Uuid::from_bytes(uuid_bytes));
        let virtual_extent_idx = u64::from_le_bytes(data[20..28].try_into().unwrap());
        let ref_count = u32::from_le_bytes(data[28..32].try_into().unwrap());
        let generation = u64::from_le_bytes(data[32..40].try_into().unwrap());

        Some(Slot {
            state,
            volume_id,
            virtual_extent_idx,
            ref_count,
            generation,
        })
    }
}

/// On-disk slab header (128 bytes used of 4096).
#[derive(Debug, Clone)]
struct SlabHeader {
    slab_uuid: Uuid,
    device_uuid: Uuid,
    slot_size: u64,
    total_slots: u64,
    free_slots: u64,
    data_offset: u64,
    table_offset: u64,
    create_time: u64,
    update_time: u64,
    tier: StorageTier,
    flags: u8,
    #[allow(dead_code)]
    checksum: u32,
}

impl SlabHeader {
    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = vec![0u8; HEADER_SIZE as usize];
        buf[0..8].copy_from_slice(&SLAB_MAGIC);
        buf[8..12].copy_from_slice(&SLAB_VERSION.to_le_bytes());
        buf[12..28].copy_from_slice(self.slab_uuid.as_bytes());
        buf[28..44].copy_from_slice(self.device_uuid.as_bytes());
        buf[44..52].copy_from_slice(&self.slot_size.to_le_bytes());
        buf[52..60].copy_from_slice(&self.total_slots.to_le_bytes());
        buf[60..68].copy_from_slice(&self.free_slots.to_le_bytes());
        buf[68..76].copy_from_slice(&self.data_offset.to_le_bytes());
        buf[76..84].copy_from_slice(&self.table_offset.to_le_bytes());
        buf[84..92].copy_from_slice(&self.create_time.to_le_bytes());
        buf[92..100].copy_from_slice(&self.update_time.to_le_bytes());
        buf[100] = self.tier as u8;
        buf[101] = self.flags;
        // bytes 102..124 reserved
        let crc = crc32c::crc32c(&buf[..124]);
        buf[124..128].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    fn from_bytes(data: &[u8]) -> Result<Self, DriveError> {
        if data.len() < 128 {
            return Err(DriveError::Other(anyhow::anyhow!("slab header too short")));
        }
        if data[0..8] != SLAB_MAGIC {
            return Err(DriveError::Other(anyhow::anyhow!("bad slab magic")));
        }
        let version = u32::from_le_bytes(data[8..12].try_into().unwrap());
        if version != SLAB_VERSION {
            return Err(DriveError::Other(anyhow::anyhow!(
                "slab version {version}, expected {SLAB_VERSION}"
            )));
        }

        let stored_crc = u32::from_le_bytes(data[124..128].try_into().unwrap());
        let computed = crc32c::crc32c(&data[..124]);
        if stored_crc != computed {
            return Err(DriveError::Other(anyhow::anyhow!("slab header CRC mismatch")));
        }

        let mut uuid_bytes = [0u8; 16];
        uuid_bytes.copy_from_slice(&data[12..28]);
        let slab_uuid = Uuid::from_bytes(uuid_bytes);

        let mut dev_bytes = [0u8; 16];
        dev_bytes.copy_from_slice(&data[28..44]);
        let device_uuid = Uuid::from_bytes(dev_bytes);

        let slot_size = u64::from_le_bytes(data[44..52].try_into().unwrap());
        let total_slots = u64::from_le_bytes(data[52..60].try_into().unwrap());
        let free_slots = u64::from_le_bytes(data[60..68].try_into().unwrap());
        let data_offset = u64::from_le_bytes(data[68..76].try_into().unwrap());
        let table_offset = u64::from_le_bytes(data[76..84].try_into().unwrap());
        let create_time = u64::from_le_bytes(data[84..92].try_into().unwrap());
        let update_time = u64::from_le_bytes(data[92..100].try_into().unwrap());
        let tier = match data[100] {
            0 => StorageTier::Hot,
            1 => StorageTier::Warm,
            2 => StorageTier::Cool,
            _ => StorageTier::Cold,
        };
        let flags = data[101];

        Ok(SlabHeader {
            slab_uuid,
            device_uuid,
            slot_size,
            total_slots,
            free_slots,
            data_offset,
            table_offset,
            create_time,
            update_time,
            tier,
            flags,
            checksum: stored_crc,
        })
    }
}

/// A slab manages a device as an extent store with fixed-size slots.
///
/// Any volume can allocate slots in any slab. The slab tracks
/// which volume owns each slot, enabling many-to-many volume-device mapping.
pub struct Slab {
    pub id: SlabId,
    header: SlabHeader,
    device: Arc<dyn BlockDevice>,
    tier: StorageTier,
    free_bitmap: BitVec<u8, Lsb0>,
    slots: Vec<Slot>,
    extent_index: HashMap<(VolumeId, u64), u32>,
    free_count: u64,
}

impl Slab {
    /// Format a device as a new slab.
    pub async fn format(
        device: Arc<dyn BlockDevice>,
        slot_size: u64,
        tier: StorageTier,
    ) -> DriveResult<Self> {
        let capacity = device.capacity_bytes();
        let table_offset = HEADER_SIZE;

        // Calculate how many slots fit: we need header + table + data
        // table_size = total_slots * SLOT_ENTRY_SIZE
        // data_size = total_slots * slot_size
        // capacity >= HEADER_SIZE + total_slots * SLOT_ENTRY_SIZE + total_slots * slot_size
        // capacity - HEADER_SIZE >= total_slots * (SLOT_ENTRY_SIZE + slot_size)
        let usable = capacity.saturating_sub(HEADER_SIZE);
        let per_slot = SLOT_ENTRY_SIZE + slot_size;
        if per_slot == 0 || usable < per_slot {
            return Err(DriveError::Other(anyhow::anyhow!(
                "device too small for slab ({capacity} bytes)"
            )));
        }
        let total_slots = usable / per_slot;
        let table_size = total_slots * SLOT_ENTRY_SIZE;

        // Align data offset to slot_size boundary
        let raw_data_offset = HEADER_SIZE + table_size;
        let data_offset = align_up(raw_data_offset, slot_size);

        // Recalculate: data region must fit
        let data_region = capacity.saturating_sub(data_offset);
        let total_slots = total_slots.min(data_region / slot_size);

        if total_slots == 0 {
            return Err(DriveError::Other(anyhow::anyhow!(
                "device too small for even one slot"
            )));
        }

        let slab_uuid = Uuid::new_v4();
        let device_uuid = device.id().uuid;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let header = SlabHeader {
            slab_uuid,
            device_uuid,
            slot_size,
            total_slots,
            free_slots: total_slots,
            data_offset,
            table_offset,
            create_time: now,
            update_time: now,
            tier,
            flags: 0,
            checksum: 0,
        };

        // Write header
        let header_bytes = header.to_bytes();
        device.write(0, &header_bytes).await?;

        // Write zeroed slot table
        let table_bytes = total_slots as usize * SLOT_ENTRY_SIZE as usize;
        let zero_table = vec![0u8; table_bytes];
        device.write(table_offset, &zero_table).await?;
        device.flush().await?;

        let id = SlabId(slab_uuid);
        let free_bitmap = BitVec::repeat(true, total_slots as usize);
        let slots = vec![Slot::free(); total_slots as usize];

        Ok(Slab {
            id,
            header,
            device,
            tier,
            free_bitmap,
            slots,
            extent_index: HashMap::new(),
            free_count: total_slots,
        })
    }

    /// Open an existing slab from a device.
    pub async fn open(device: Arc<dyn BlockDevice>) -> DriveResult<Self> {
        // Read header
        let mut header_buf = vec![0u8; HEADER_SIZE as usize];
        device.read(0, &mut header_buf).await?;
        let header = SlabHeader::from_bytes(&header_buf)?;

        let total_slots = header.total_slots as usize;
        let table_size = total_slots * SLOT_ENTRY_SIZE as usize;

        // Read slot table
        let mut table_buf = vec![0u8; table_size];
        device.read(header.table_offset, &mut table_buf).await?;

        let mut free_bitmap = BitVec::repeat(true, total_slots);
        let mut slots = Vec::with_capacity(total_slots);
        let mut extent_index = HashMap::new();
        let mut free_count = 0u64;

        for i in 0..total_slots {
            let offset = i * SLOT_ENTRY_SIZE as usize;
            let slot_data = &table_buf[offset..offset + SLOT_ENTRY_SIZE as usize];
            let slot = Slot::from_bytes(slot_data).unwrap_or_else(Slot::free);

            if slot.state != SlotState::Free {
                free_bitmap.set(i, false);
                extent_index.insert(
                    (slot.volume_id, slot.virtual_extent_idx),
                    i as u32,
                );
            } else {
                free_count += 1;
            }
            slots.push(slot);
        }

        let id = SlabId(header.slab_uuid);
        let tier = header.tier;

        Ok(Slab {
            id,
            header,
            device,
            tier,
            free_bitmap,
            slots,
            extent_index,
            free_count,
        })
    }

    /// Allocate a slot for a volume's virtual extent.
    pub async fn allocate(
        &mut self,
        volume_id: VolumeId,
        vext_idx: u64,
    ) -> DriveResult<u32> {
        if self.free_count == 0 {
            return Err(DriveError::Other(anyhow::anyhow!("slab full")));
        }

        // Find first free slot
        let slot_idx = self.free_bitmap.first_one()
            .ok_or_else(|| DriveError::Other(anyhow::anyhow!("bitmap inconsistency")))?;

        self.free_bitmap.set(slot_idx, false);
        self.free_count -= 1;

        self.slots[slot_idx] = Slot {
            state: SlotState::Allocated,
            volume_id,
            virtual_extent_idx: vext_idx,
            ref_count: 1,
            generation: 1,
        };
        self.extent_index.insert((volume_id, vext_idx), slot_idx as u32);

        // Persist slot entry
        self.persist_slot(slot_idx as u32).await?;
        self.persist_header().await?;

        Ok(slot_idx as u32)
    }

    /// Free a slot, returning it to the free pool.
    pub async fn free(&mut self, slot_idx: u32) -> DriveResult<()> {
        let idx = slot_idx as usize;
        if idx >= self.slots.len() {
            return Err(DriveError::Other(anyhow::anyhow!(
                "slot index {slot_idx} out of range"
            )));
        }

        let slot = &self.slots[idx];
        if slot.state == SlotState::Free {
            return Err(DriveError::Other(anyhow::anyhow!(
                "double free of slot {slot_idx}"
            )));
        }

        // Only remove from extent index if it still points to this slot.
        // After COW, a new slot may have been allocated for the same (vol, vext)
        // in this slab, so the index may already point elsewhere.
        let key = (slot.volume_id, slot.virtual_extent_idx);
        if self.extent_index.get(&key) == Some(&slot_idx) {
            self.extent_index.remove(&key);
        }

        self.slots[idx] = Slot::free();
        self.free_bitmap.set(idx, true);
        self.free_count += 1;

        self.persist_slot(slot_idx).await?;
        self.persist_header().await?;

        Ok(())
    }

    /// Read data from a slot at the given offset within the slot.
    pub async fn read_slot(
        &self,
        slot_idx: u32,
        offset_in_slot: u64,
        buf: &mut [u8],
    ) -> DriveResult<usize> {
        let idx = slot_idx as usize;
        if idx >= self.slots.len() {
            return Err(DriveError::Other(anyhow::anyhow!(
                "slot index {slot_idx} out of range"
            )));
        }
        let phys_offset = self.header.data_offset
            + (slot_idx as u64) * self.header.slot_size
            + offset_in_slot;
        self.device.read(phys_offset, buf).await
    }

    /// Write data to a slot at the given offset within the slot.
    pub async fn write_slot(
        &self,
        slot_idx: u32,
        offset_in_slot: u64,
        buf: &[u8],
    ) -> DriveResult<usize> {
        let idx = slot_idx as usize;
        if idx >= self.slots.len() {
            return Err(DriveError::Other(anyhow::anyhow!(
                "slot index {slot_idx} out of range"
            )));
        }
        let phys_offset = self.header.data_offset
            + (slot_idx as u64) * self.header.slot_size
            + offset_in_slot;
        self.device.write(phys_offset, buf).await
    }

    /// Increment the reference count on a slot (for COW snapshots).
    pub async fn inc_ref(&mut self, slot_idx: u32) -> DriveResult<()> {
        let idx = slot_idx as usize;
        if idx >= self.slots.len() {
            return Err(DriveError::Other(anyhow::anyhow!(
                "slot index {slot_idx} out of range"
            )));
        }
        if self.slots[idx].state == SlotState::Free {
            return Err(DriveError::Other(anyhow::anyhow!(
                "cannot inc_ref on free slot {slot_idx}"
            )));
        }
        self.slots[idx].ref_count += 1;
        self.persist_slot(slot_idx).await
    }

    /// Decrement the reference count on a slot. Returns true if freed (hit 0).
    pub async fn dec_ref(&mut self, slot_idx: u32) -> DriveResult<bool> {
        let idx = slot_idx as usize;
        if idx >= self.slots.len() {
            return Err(DriveError::Other(anyhow::anyhow!(
                "slot index {slot_idx} out of range"
            )));
        }
        if self.slots[idx].state == SlotState::Free || self.slots[idx].ref_count == 0 {
            return Err(DriveError::Other(anyhow::anyhow!(
                "cannot dec_ref on free/zero-ref slot {slot_idx}"
            )));
        }
        self.slots[idx].ref_count -= 1;
        if self.slots[idx].ref_count == 0 {
            self.free(slot_idx).await?;
            Ok(true)
        } else {
            self.persist_slot(slot_idx).await?;
            Ok(false)
        }
    }

    /// Find the slot index for a given volume + virtual extent.
    pub fn find_slot(&self, volume_id: VolumeId, vext_idx: u64) -> Option<u32> {
        self.extent_index.get(&(volume_id, vext_idx)).copied()
    }

    /// Get the slot at a given index.
    pub fn get_slot(&self, slot_idx: u32) -> Option<&Slot> {
        self.slots.get(slot_idx as usize)
    }

    /// Slab UUID.
    pub fn slab_id(&self) -> SlabId {
        self.id
    }

    /// Storage tier.
    pub fn tier(&self) -> StorageTier {
        self.tier
    }

    /// Slot size in bytes.
    pub fn slot_size(&self) -> u64 {
        self.header.slot_size
    }

    /// Total number of slots.
    pub fn total_slots(&self) -> u64 {
        self.header.total_slots
    }

    /// Number of free slots.
    pub fn free_slots(&self) -> u64 {
        self.free_count
    }

    /// Number of allocated slots.
    pub fn allocated_slots(&self) -> u64 {
        self.header.total_slots - self.free_count
    }

    /// Get a reference to the underlying device.
    pub fn device(&self) -> &Arc<dyn BlockDevice> {
        &self.device
    }

    /// Get the device and physical offset for a slot + offset within slot.
    /// Useful for extracting I/O target before dropping registry lock.
    pub fn slot_device_and_offset(
        &self,
        slot_idx: u32,
        offset_in_slot: u64,
    ) -> DriveResult<(Arc<dyn BlockDevice>, u64)> {
        let idx = slot_idx as usize;
        if idx >= self.slots.len() {
            return Err(DriveError::Other(anyhow::anyhow!(
                "slot index {slot_idx} out of range"
            )));
        }
        let phys_offset = self.header.data_offset
            + (slot_idx as u64) * self.header.slot_size
            + offset_in_slot;
        Ok((Arc::clone(&self.device), phys_offset))
    }

    /// Persist a single slot entry to disk.
    async fn persist_slot(&self, slot_idx: u32) -> DriveResult<()> {
        let slot = &self.slots[slot_idx as usize];
        let bytes = slot.to_bytes();
        let offset = self.header.table_offset + (slot_idx as u64) * SLOT_ENTRY_SIZE;
        self.device.write(offset, &bytes).await?;
        Ok(())
    }

    /// Persist the header (updates free_slots count).
    async fn persist_header(&mut self) -> DriveResult<()> {
        self.header.free_slots = self.free_count;
        self.header.update_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let bytes = self.header.to_bytes();
        self.device.write(0, &bytes).await?;
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

    async fn create_slab_device(size: u64) -> (Arc<dyn BlockDevice>, String) {
        let dir = std::env::temp_dir().join("stormblock-slab-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("cont-{}.bin", Uuid::new_v4().simple()));
        let path_str = path.to_str().unwrap().to_string();
        let _ = std::fs::remove_file(&path);
        let dev = FileDevice::open_with_capacity(&path_str, size).await.unwrap();
        (Arc::new(dev), path_str)
    }

    fn cleanup(path: &str) {
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn format_and_open_roundtrip() {
        let (dev, path) = create_slab_device(100 * 1024 * 1024).await;
        let cont = Slab::format(dev.clone(), DEFAULT_SLOT_SIZE, StorageTier::Hot)
            .await
            .unwrap();
        let id = cont.id;
        let total = cont.total_slots();
        let free = cont.free_slots();
        assert!(total > 0);
        assert_eq!(total, free);

        // Re-open
        let cont2 = Slab::open(dev).await.unwrap();
        assert_eq!(cont2.id, id);
        assert_eq!(cont2.total_slots(), total);
        assert_eq!(cont2.free_slots(), free);
        assert_eq!(cont2.tier(), StorageTier::Hot);

        cleanup(&path);
    }

    #[tokio::test]
    async fn allocate_and_free() {
        let (dev, path) = create_slab_device(10 * 1024 * 1024).await;
        let mut cont = Slab::format(dev, DEFAULT_SLOT_SIZE, StorageTier::Warm)
            .await
            .unwrap();
        let total = cont.total_slots();
        let vol = VolumeId::new();

        let slot0 = cont.allocate(vol, 0).await.unwrap();
        assert_eq!(cont.free_slots(), total - 1);
        assert_eq!(cont.allocated_slots(), 1);

        let slot1 = cont.allocate(vol, 1).await.unwrap();
        assert_ne!(slot0, slot1);
        assert_eq!(cont.free_slots(), total - 2);

        // Find
        assert_eq!(cont.find_slot(vol, 0), Some(slot0));
        assert_eq!(cont.find_slot(vol, 1), Some(slot1));
        assert_eq!(cont.find_slot(vol, 999), None);

        // Free
        cont.free(slot0).await.unwrap();
        assert_eq!(cont.free_slots(), total - 1);
        assert_eq!(cont.find_slot(vol, 0), None);

        cleanup(&path);
    }

    #[tokio::test]
    async fn read_write_slot() {
        let (dev, path) = create_slab_device(10 * 1024 * 1024).await;
        let mut cont = Slab::format(dev, DEFAULT_SLOT_SIZE, StorageTier::Hot)
            .await
            .unwrap();
        let vol = VolumeId::new();

        let slot = cont.allocate(vol, 0).await.unwrap();

        // Write
        let data = vec![0xDE_u8; 4096];
        cont.write_slot(slot, 0, &data).await.unwrap();

        // Read back
        let mut buf = vec![0u8; 4096];
        cont.read_slot(slot, 0, &mut buf).await.unwrap();
        assert_eq!(buf, data);

        // Write at offset within slot
        let data2 = vec![0xAB_u8; 512];
        cont.write_slot(slot, 8192, &data2).await.unwrap();
        let mut buf2 = vec![0u8; 512];
        cont.read_slot(slot, 8192, &mut buf2).await.unwrap();
        assert_eq!(buf2, data2);

        cleanup(&path);
    }

    #[tokio::test]
    async fn ref_count_inc_dec() {
        let (dev, path) = create_slab_device(10 * 1024 * 1024).await;
        let mut cont = Slab::format(dev, DEFAULT_SLOT_SIZE, StorageTier::Hot)
            .await
            .unwrap();
        let vol = VolumeId::new();
        let total = cont.total_slots();

        let slot = cont.allocate(vol, 0).await.unwrap();
        assert_eq!(cont.get_slot(slot).unwrap().ref_count, 1);

        cont.inc_ref(slot).await.unwrap();
        assert_eq!(cont.get_slot(slot).unwrap().ref_count, 2);

        cont.inc_ref(slot).await.unwrap();
        assert_eq!(cont.get_slot(slot).unwrap().ref_count, 3);

        // dec_ref doesn't free until 0
        let freed = cont.dec_ref(slot).await.unwrap();
        assert!(!freed);
        assert_eq!(cont.get_slot(slot).unwrap().ref_count, 2);

        let freed = cont.dec_ref(slot).await.unwrap();
        assert!(!freed);
        assert_eq!(cont.get_slot(slot).unwrap().ref_count, 1);

        // Final dec_ref frees the slot
        let freed = cont.dec_ref(slot).await.unwrap();
        assert!(freed);
        assert_eq!(cont.free_slots(), total);

        cleanup(&path);
    }

    #[tokio::test]
    async fn bitmap_exhaustion() {
        // Small device: only fits a few slots
        let slot_size = DEFAULT_SLOT_SIZE;
        // 3 MB = header + table + ~2 data slots
        let (dev, path) = create_slab_device(3 * 1024 * 1024).await;
        let mut cont = Slab::format(dev, slot_size, StorageTier::Cold)
            .await
            .unwrap();
        let total = cont.total_slots();
        let vol = VolumeId::new();

        // Allocate all slots
        for i in 0..total {
            cont.allocate(vol, i).await.unwrap();
        }
        assert_eq!(cont.free_slots(), 0);

        // Next allocation should fail
        let result = cont.allocate(vol, total);
        assert!(result.await.is_err());

        cleanup(&path);
    }

    #[tokio::test]
    async fn multi_volume_slots() {
        let (dev, path) = create_slab_device(10 * 1024 * 1024).await;
        let mut cont = Slab::format(dev, DEFAULT_SLOT_SIZE, StorageTier::Warm)
            .await
            .unwrap();

        let vol_a = VolumeId::new();
        let vol_b = VolumeId::new();

        let slot_a0 = cont.allocate(vol_a, 0).await.unwrap();
        let slot_b0 = cont.allocate(vol_b, 0).await.unwrap();
        let slot_a1 = cont.allocate(vol_a, 1).await.unwrap();

        assert_ne!(slot_a0, slot_b0);
        assert_ne!(slot_a0, slot_a1);
        assert_eq!(cont.find_slot(vol_a, 0), Some(slot_a0));
        assert_eq!(cont.find_slot(vol_b, 0), Some(slot_b0));
        assert_eq!(cont.find_slot(vol_a, 1), Some(slot_a1));

        // Free vol_a slot 0, vol_b slot 0 should still be there
        cont.free(slot_a0).await.unwrap();
        assert_eq!(cont.find_slot(vol_a, 0), None);
        assert_eq!(cont.find_slot(vol_b, 0), Some(slot_b0));

        cleanup(&path);
    }

    #[tokio::test]
    async fn persistence_across_reopen() {
        let (dev, path) = create_slab_device(10 * 1024 * 1024).await;
        let vol = VolumeId::new();

        // Format and allocate
        let slot_idx;
        {
            let mut cont = Slab::format(dev.clone(), DEFAULT_SLOT_SIZE, StorageTier::Hot)
                .await
                .unwrap();
            slot_idx = cont.allocate(vol, 42).await.unwrap();
            cont.write_slot(slot_idx, 0, &[0xFF; 4096]).await.unwrap();
            cont.device.flush().await.unwrap();
        }

        // Re-open and verify
        let cont2 = Slab::open(dev).await.unwrap();
        assert_eq!(cont2.find_slot(vol, 42), Some(slot_idx));
        let slot = cont2.get_slot(slot_idx).unwrap();
        assert_eq!(slot.state, SlotState::Allocated);
        assert_eq!(slot.volume_id, vol);
        assert_eq!(slot.virtual_extent_idx, 42);
        assert_eq!(slot.ref_count, 1);

        let mut buf = vec![0u8; 4096];
        cont2.read_slot(slot_idx, 0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xFF));

        cleanup(&path);
    }

    #[test]
    fn slot_entry_roundtrip() {
        let slot = Slot {
            state: SlotState::Allocated,
            volume_id: VolumeId::new(),
            virtual_extent_idx: 42,
            ref_count: 3,
            generation: 7,
        };
        let bytes = slot.to_bytes();
        let decoded = Slot::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.state, SlotState::Allocated);
        assert_eq!(decoded.volume_id, slot.volume_id);
        assert_eq!(decoded.virtual_extent_idx, 42);
        assert_eq!(decoded.ref_count, 3);
        assert_eq!(decoded.generation, 7);
    }

    #[test]
    fn slot_entry_crc_detects_corruption() {
        let slot = Slot {
            state: SlotState::Allocated,
            volume_id: VolumeId::new(),
            virtual_extent_idx: 1,
            ref_count: 1,
            generation: 1,
        };
        let mut bytes = slot.to_bytes();
        bytes[5] ^= 0xFF; // corrupt a byte
        assert!(Slot::from_bytes(&bytes).is_none());
    }

    #[test]
    fn header_roundtrip() {
        let header = SlabHeader {
            slab_uuid: Uuid::new_v4(),
            device_uuid: Uuid::new_v4(),
            slot_size: DEFAULT_SLOT_SIZE,
            total_slots: 100,
            free_slots: 95,
            data_offset: 2 * 1024 * 1024,
            table_offset: HEADER_SIZE,
            create_time: 1234567890,
            update_time: 1234567900,
            tier: StorageTier::Warm,
            flags: 0,
            checksum: 0,
        };
        let bytes = header.to_bytes();
        let decoded = SlabHeader::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.slab_uuid, header.slab_uuid);
        assert_eq!(decoded.device_uuid, header.device_uuid);
        assert_eq!(decoded.slot_size, DEFAULT_SLOT_SIZE);
        assert_eq!(decoded.total_slots, 100);
        assert_eq!(decoded.free_slots, 95);
        assert_eq!(decoded.tier, StorageTier::Warm);
    }
}
