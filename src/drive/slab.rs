//! Slab — extent store with fixed-size slots on a block device.
//!
//! Each device (or device region) is formatted as a Slab with a header,
//! a slot table, and a data region of 1 MB slots. Any volume can allocate
//! slots in any slab. This replaces the monolithic DiskPool/VDrive model
//! with organic, per-extent data placement.

use std::collections::{HashMap, HashSet};
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

/// Why a slot in a release batch could not be decremented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecRefReject {
    /// Index past the end of the slot table.
    OutOfRange,
    /// Already free, or already at zero references — the extent map named a
    /// slot the slot table no longer considers owned.
    AlreadyFree,
    /// The same slot appeared earlier in this batch.
    Duplicate,
}

impl std::fmt::Display for DecRefReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecRefReject::OutOfRange => write!(f, "out of range"),
            DecRefReject::AlreadyFree => write!(f, "already free"),
            DecRefReject::Duplicate => write!(f, "duplicate in batch"),
        }
    }
}

/// What a batched reference release actually managed to do.
///
/// Carries the per-slot failures rather than collapsing them into a single
/// error, so a caller can free what it can and still report the divergence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecRefOutcome {
    /// Slots whose last reference went away — space returned to the slab.
    pub freed: usize,
    /// Slots decremented but still referenced by someone else.
    pub retained: usize,
    /// Slots that could not be decremented, and why.
    pub rejected: Vec<(u32, DecRefReject)>,
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

        // Write zeroed slot table (padded to device block_size for alignment)
        let table_bytes = total_slots as usize * SLOT_ENTRY_SIZE as usize;
        let bs = device.block_size() as usize;
        let padded_table = if bs > 1 && table_bytes % bs != 0 {
            table_bytes.div_ceil(bs) * bs
        } else {
            table_bytes
        };
        let zero_table = vec![0u8; padded_table];
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

        // Read slot table (padded to block_size for alignment)
        let bs = device.block_size() as usize;
        let read_size = if bs > 1 && table_size % bs != 0 {
            table_size.div_ceil(bs) * bs
        } else {
            table_size
        };
        let mut table_buf = vec![0u8; read_size];
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

        // Only the slot entry is persisted here. The header's free_slots is
        // derived — `open` recounts it from the slot table — so writing it on
        // every allocation was a second disk round trip under the registry
        // lock for a value that is never read back authoritatively.
        self.persist_slot(slot_idx as u32).await?;

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
        self.discard_slots(&[slot_idx]).await;

        Ok(())
    }

    /// Hand freed slots back to the underlying device.
    ///
    /// Marking a slot free only reclaims it *within* the slab; without this
    /// the backing store keeps every byte it ever wrote, so a
    /// clone-per-container workload grows monotonically however much is
    /// deleted. Contiguous slots are coalesced into one call, which is the
    /// common case when a clone's extents are released together.
    ///
    /// Best-effort: a device that cannot discard is not a failure, the slot is
    /// still free.
    async fn discard_slots(&self, slot_indices: &[u32]) {
        if slot_indices.is_empty() {
            return;
        }
        let slot_size = self.header.slot_size;
        let mut sorted: Vec<u32> = slot_indices.to_vec();
        sorted.sort_unstable();
        sorted.dedup();

        let mut run_start = sorted[0];
        let mut run_end = sorted[0]; // inclusive
        for &idx in &sorted[1..] {
            if idx == run_end + 1 {
                run_end = idx;
            } else {
                self.discard_slot_run(run_start, run_end, slot_size).await;
                run_start = idx;
                run_end = idx;
            }
        }
        self.discard_slot_run(run_start, run_end, slot_size).await;
    }

    async fn discard_slot_run(&self, first: u32, last: u32, slot_size: u64) {
        let offset = self.header.data_offset + (first as u64) * slot_size;
        let len = ((last - first) as u64 + 1) * slot_size;
        if let Err(e) = self.device.discard(offset, len).await {
            tracing::debug!(
                "slab {}: discard of slots {first}..={last} failed: {e}",
                self.id.0
            );
        }
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

    /// Increment reference counts on many slots at once.
    ///
    /// Cloning a volume bumps every extent it shares with its source, so the
    /// per-slot path costs one read-modify-write per extent. This persists the
    /// table by sector instead, which is what keeps clone latency proportional
    /// to sectors touched rather than to image size.
    pub async fn inc_ref_batch(&mut self, slot_indices: &[u32]) -> DriveResult<()> {
        // Validate everything before mutating, so a bad index cannot leave the
        // batch half-applied.
        for &slot_idx in slot_indices {
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
        }

        for &slot_idx in slot_indices {
            self.slots[slot_idx as usize].ref_count += 1;
        }
        self.persist_slots(slot_indices).await
    }

    /// Decrement reference counts on many slots at once, freeing any that
    /// reach zero. Returns the number freed.
    ///
    /// The header is written once at the end rather than once per freed slot.
    pub async fn dec_ref_batch(&mut self, slot_indices: &[u32]) -> DriveResult<DecRefOutcome> {
        let mut out = DecRefOutcome::default();
        let mut seen: HashSet<u32> = HashSet::with_capacity(slot_indices.len());
        let mut touched: Vec<u32> = Vec::with_capacity(slot_indices.len());
        let mut freed_slots: Vec<u32> = Vec::new();

        for &slot_idx in slot_indices {
            let idx = slot_idx as usize;

            // A repeat inside one batch would decrement a slot an earlier pass
            // may already have freed, underflowing ref_count to u32::MAX.
            if !seen.insert(slot_idx) {
                out.rejected.push((slot_idx, DecRefReject::Duplicate));
                continue;
            }
            if idx >= self.slots.len() {
                out.rejected.push((slot_idx, DecRefReject::OutOfRange));
                continue;
            }
            if self.slots[idx].state == SlotState::Free || self.slots[idx].ref_count == 0 {
                out.rejected.push((slot_idx, DecRefReject::AlreadyFree));
                continue;
            }

            self.slots[idx].ref_count -= 1;
            touched.push(slot_idx);
            if self.slots[idx].ref_count > 0 {
                out.retained += 1;
                continue;
            }

            // Last reference: same bookkeeping as `free`, but the write is
            // deferred to the coalesced flush below.
            let slot = &self.slots[idx];
            let key = (slot.volume_id, slot.virtual_extent_idx);
            if self.extent_index.get(&key) == Some(&slot_idx) {
                self.extent_index.remove(&key);
            }
            self.slots[idx] = Slot::free();
            self.free_bitmap.set(idx, true);
            self.free_count += 1;
            freed_slots.push(slot_idx);
            out.freed += 1;
        }

        if !out.rejected.is_empty() {
            // The extent map and the slot table disagree. That is a real
            // accounting fault worth surfacing, but it is not a reason to
            // strand the extents this batch *can* release.
            tracing::warn!(
                slab = %self.slab_id(),
                rejected = out.rejected.len(),
                freed = out.freed,
                retained = out.retained,
                "dec_ref_batch skipped slots: {}",
                out.rejected
                    .iter()
                    .map(|(s, r)| format!("{s}({r})"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }

        if !touched.is_empty() {
            self.persist_slots(&touched).await?;
        }
        if !freed_slots.is_empty() {
            // Give the space back to the device, not just to the slab —
            // otherwise reclaim happens on the single-slot route only and a
            // dropped clone still leaks its backing store.
            self.discard_slots(&freed_slots).await;
        }
        Ok(out)
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
    ///
    /// Slot entries are 64 bytes, but the device may have a larger block size
    /// (e.g., 512 bytes for iSCSI). We do a read-modify-write of the aligned
    /// sector to handle devices that require block-aligned I/O.
    /// Persist several slot entries, coalescing those that share a sector.
    ///
    /// A slot entry is 64 bytes while a sector is 512 or 4096, so 8-64
    /// consecutive entries live in one sector. Clone and delete touch runs of
    /// slots, so grouping turns one read-modify-write per slot into one write
    /// per sector — and a sector the batch fills completely needs no read at
    /// all.
    async fn persist_slots(&self, slot_indices: &[u32]) -> DriveResult<()> {
        if slot_indices.is_empty() {
            return Ok(());
        }
        let bs = self.device.block_size() as u64;

        // Sectors at or below entry size gain nothing from grouping.
        if bs <= SLOT_ENTRY_SIZE {
            for &slot_idx in slot_indices {
                self.persist_slot(slot_idx).await?;
            }
            return Ok(());
        }

        let entries_per_sector = (bs / SLOT_ENTRY_SIZE) as usize;

        let mut by_sector: HashMap<u64, Vec<u32>> = HashMap::new();
        for &slot_idx in slot_indices {
            let entry_offset = self.header.table_offset + (slot_idx as u64) * SLOT_ENTRY_SIZE;
            by_sector
                .entry((entry_offset / bs) * bs)
                .or_default()
                .push(slot_idx);
        }

        for (sector_start, mut idxs) in by_sector {
            idxs.sort_unstable();
            idxs.dedup();

            // Holding one distinct valid slot per entry means the batch
            // overwrites every byte of this sector, so the read can be
            // skipped. Any partial sector — including a final one that runs
            // past the table into the data region — is read first.
            let mut sector = if idxs.len() == entries_per_sector {
                vec![0u8; bs as usize]
            } else {
                let mut buf = vec![0u8; bs as usize];
                self.device.read(sector_start, &mut buf).await?;
                buf
            };

            for &slot_idx in &idxs {
                let entry_offset =
                    self.header.table_offset + (slot_idx as u64) * SLOT_ENTRY_SIZE;
                let off = (entry_offset - sector_start) as usize;
                sector[off..off + SLOT_ENTRY_SIZE as usize]
                    .copy_from_slice(&self.slots[slot_idx as usize].to_bytes());
            }

            self.device.write(sector_start, &sector).await?;
        }

        Ok(())
    }

    async fn persist_slot(&self, slot_idx: u32) -> DriveResult<()> {
        let slot = &self.slots[slot_idx as usize];
        let entry_bytes = slot.to_bytes();
        let entry_offset = self.header.table_offset + (slot_idx as u64) * SLOT_ENTRY_SIZE;
        let bs = self.device.block_size() as u64;

        if bs <= SLOT_ENTRY_SIZE {
            // Block size <= entry size — direct write is fine
            self.device.write(entry_offset, &entry_bytes).await?;
            return Ok(());
        }

        let sector_start = (entry_offset / bs) * bs;
        let entries_per_sector = (bs / SLOT_ENTRY_SIZE) as usize;
        let first_entry = ((sector_start - self.header.table_offset) / SLOT_ENTRY_SIZE) as usize;

        // `self.slots` is the authoritative copy of the table, so a sector that
        // lies wholly inside it can be rebuilt from memory — no read needed.
        // A trailing partial sector overlaps the data region and must still be
        // read first so those bytes are preserved.
        if first_entry + entries_per_sector <= self.slots.len() {
            let mut sector = vec![0u8; bs as usize];
            for i in 0..entries_per_sector {
                let off = i * SLOT_ENTRY_SIZE as usize;
                sector[off..off + SLOT_ENTRY_SIZE as usize]
                    .copy_from_slice(&self.slots[first_entry + i].to_bytes());
            }
            self.device.write(sector_start, &sector).await?;
        } else {
            let offset_in_sector = (entry_offset - sector_start) as usize;
            let mut sector = vec![0u8; bs as usize];
            self.device.read(sector_start, &mut sector).await?;
            sector[offset_in_sector..offset_in_sector + SLOT_ENTRY_SIZE as usize]
                .copy_from_slice(&entry_bytes);
            self.device.write(sector_start, &sector).await?;
        }
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

    /// The batched refcount path must land on disk exactly like the per-slot
    /// path — it coalesces sector writes and skips reads for fully-covered
    /// sectors, both of which could corrupt neighbouring table entries.
    #[tokio::test]
    async fn batched_refcounts_match_disk_after_reopen() {
        let (dev, path) = create_slab_device(64 * 1024 * 1024).await;
        // 64 KB slots so the device holds ~1000 of them: enough for the slot
        // table to span several sectors, which is what the batching groups by.
        let mut slab = Slab::format(dev.clone(), 64 * 1024, StorageTier::Hot)
            .await
            .unwrap();

        // Enough slots to span more than one sector of slot table
        // (4096-byte sector / 64-byte entry = 64 entries per sector).
        let vol = VolumeId::new();
        let mut slots = Vec::new();
        for vext in 0..100u64 {
            slots.push(slab.allocate(vol, vext).await.unwrap());
        }

        // Bump a run that crosses a sector boundary, plus a stray one well
        // past it, so both the coalesced and single-entry paths are used.
        let bumped: Vec<u32> = slots[60..70].to_vec();
        slab.inc_ref_batch(&bumped).await.unwrap();
        slab.inc_ref_batch(&[slots[99]]).await.unwrap();

        let free_before = slab.free_slots();
        drop(slab);

        let reopened = Slab::open(dev.clone()).await.unwrap();
        assert_eq!(reopened.free_slots(), free_before);
        for (i, &s) in slots.iter().enumerate() {
            let expected = if bumped.contains(&s) || i == 99 { 2 } else { 1 };
            assert_eq!(
                reopened.get_slot(s).unwrap().ref_count,
                expected,
                "slot {s} (index {i}) refcount"
            );
            // Neighbours in a rewritten sector must keep their identity.
            assert_eq!(reopened.get_slot(s).unwrap().volume_id, vol);
            assert_eq!(reopened.get_slot(s).unwrap().virtual_extent_idx, i as u64);
        }

        cleanup(&path);
    }

    /// Dropping a clone must return only the slots that hit zero, and leave
    /// the ones its source still holds — the container restart cycle does
    /// this constantly.
    #[tokio::test]
    async fn batched_dec_ref_frees_only_last_reference() {
        let (dev, path) = create_slab_device(64 * 1024 * 1024).await;
        // 64 KB slots so the device holds ~1000 of them: enough for the slot
        // table to span several sectors, which is what the batching groups by.
        let mut slab = Slab::format(dev.clone(), 64 * 1024, StorageTier::Hot)
            .await
            .unwrap();

        let vol = VolumeId::new();
        let mut slots = Vec::new();
        for vext in 0..80u64 {
            slots.push(slab.allocate(vol, vext).await.unwrap());
        }
        let free_after_alloc = slab.free_slots();

        // Clone: every slot now has two references.
        slab.inc_ref_batch(&slots).await.unwrap();

        // Dropping the clone releases one reference from each — none freed.
        let out = slab.dec_ref_batch(&slots).await.unwrap();
        assert_eq!(out.freed, 0, "source still holds every slot");
        assert_eq!(out.retained, slots.len());
        assert!(out.rejected.is_empty());
        assert_eq!(slab.free_slots(), free_after_alloc);

        // Dropping the source too frees them all.
        let out = slab.dec_ref_batch(&slots).await.unwrap();
        assert_eq!(out.freed, slots.len());
        assert_eq!(slab.free_slots(), free_after_alloc + slots.len() as u64);

        drop(slab);
        let reopened = Slab::open(dev.clone()).await.unwrap();
        assert_eq!(reopened.free_slots(), free_after_alloc + slots.len() as u64);
        for &s in &slots {
            assert_eq!(reopened.get_slot(s).unwrap().state, SlotState::Free);
        }

        cleanup(&path);
    }

    /// Rewriting a partially-covered sector must preserve the entries the
    /// batch did not touch.
    #[tokio::test]
    async fn batched_persist_preserves_untouched_neighbours() {
        let (dev, path) = create_slab_device(64 * 1024 * 1024).await;
        // 64 KB slots so the device holds ~1000 of them: enough for the slot
        // table to span several sectors, which is what the batching groups by.
        let mut slab = Slab::format(dev.clone(), 64 * 1024, StorageTier::Hot)
            .await
            .unwrap();

        let vol_a = VolumeId::new();
        let vol_b = VolumeId::new();
        // Interleave two volumes so neighbours in the same sector differ.
        let mut a_slots = Vec::new();
        let mut b_slots = Vec::new();
        for vext in 0..40u64 {
            a_slots.push(slab.allocate(vol_a, vext).await.unwrap());
            b_slots.push(slab.allocate(vol_b, vext).await.unwrap());
        }

        // Touch only vol_a's slots; vol_b's share the same sectors.
        slab.inc_ref_batch(&a_slots).await.unwrap();
        drop(slab);

        let reopened = Slab::open(dev.clone()).await.unwrap();
        for (i, &s) in b_slots.iter().enumerate() {
            let slot = reopened.get_slot(s).unwrap();
            assert_eq!(slot.ref_count, 1, "untouched neighbour {s} refcount");
            assert_eq!(slot.volume_id, vol_b, "untouched neighbour {s} volume");
            assert_eq!(slot.virtual_extent_idx, i as u64);
        }
        for &s in &a_slots {
            assert_eq!(reopened.get_slot(s).unwrap().ref_count, 2);
        }

        cleanup(&path);
    }

    #[tokio::test]
    async fn batched_refcounts_reject_bad_slots_without_mutating() {
        let (dev, path) = create_slab_device(64 * 1024 * 1024).await;
        // 64 KB slots so the device holds ~1000 of them: enough for the slot
        // table to span several sectors, which is what the batching groups by.
        let mut slab = Slab::format(dev.clone(), 64 * 1024, StorageTier::Hot)
            .await
            .unwrap();

        let vol = VolumeId::new();
        let good = slab.allocate(vol, 0).await.unwrap();

        // An out-of-range index anywhere in the batch fails it whole, leaving
        // the valid entries alone rather than half-applied.
        assert!(slab.inc_ref_batch(&[good, u32::MAX]).await.is_err());
        assert_eq!(slab.get_slot(good).unwrap().ref_count, 1);

        // Same for a free slot.
        let free_idx = slab.allocate(vol, 1).await.unwrap();
        slab.free(free_idx).await.unwrap();
        assert!(slab.inc_ref_batch(&[good, free_idx]).await.is_err());
        assert_eq!(slab.get_slot(good).unwrap().ref_count, 1);

        // Release is deliberately *not* all-or-nothing: the stale entry costs
        // itself, and the healthy slot is still released.
        let out = slab.dec_ref_batch(&[good, free_idx]).await.unwrap();
        assert_eq!(out.freed, 1);
        assert_eq!(out.rejected, vec![(free_idx, DecRefReject::AlreadyFree)]);
        assert_eq!(slab.get_slot(good).unwrap().state, SlotState::Free);

        cleanup(&path);
    }

    /// Regression for #37: one stale extent-map entry must not strand every
    /// other extent of the volume being deleted.
    ///
    /// The old `dec_ref_batch` validated the whole batch first and returned
    /// `Err` on the first already-free slot, so nothing was decremented at
    /// all — and the caller discarded that error, leaving the slots allocated
    /// with an owner that no longer existed and no way to reclaim them.
    #[tokio::test]
    async fn one_stale_entry_does_not_strand_the_whole_batch() {
        let (dev, path) = create_slab_device(64 * 1024 * 1024).await;
        let mut slab = Slab::format(dev.clone(), 64 * 1024, StorageTier::Hot)
            .await
            .unwrap();

        let vol = VolumeId::new();
        let mut slots = Vec::new();
        for vext in 0..12u64 {
            slots.push(slab.allocate(vol, vext).await.unwrap());
        }
        let free_before = slab.free_slots();

        // Simulate the divergence: one slot is already free, but the volume's
        // extent map still names it.
        let stale = slots[5];
        slab.free(stale).await.unwrap();

        let out = slab.dec_ref_batch(&slots).await.unwrap();

        assert_eq!(out.rejected, vec![(stale, DecRefReject::AlreadyFree)]);
        assert_eq!(out.freed, slots.len() - 1, "every healthy extent released");
        assert_eq!(
            slab.free_slots(),
            free_before + slots.len() as u64,
            "all 12 slots are back, not zero of them"
        );
        for &s in &slots {
            assert_eq!(slab.get_slot(s).unwrap().state, SlotState::Free);
        }

        cleanup(&path);
    }

    /// A slot repeated inside one batch used to underflow `ref_count` to
    /// `u32::MAX` on the second decrement (panic in debug, silent corruption
    /// in release).
    #[tokio::test]
    async fn duplicate_slot_in_batch_does_not_underflow() {
        let (dev, path) = create_slab_device(64 * 1024 * 1024).await;
        let mut slab = Slab::format(dev.clone(), 64 * 1024, StorageTier::Hot)
            .await
            .unwrap();

        let vol = VolumeId::new();
        let slot = slab.allocate(vol, 0).await.unwrap();
        let free_before = slab.free_slots();

        let out = slab.dec_ref_batch(&[slot, slot, slot]).await.unwrap();

        assert_eq!(out.freed, 1, "released once, not three times");
        assert_eq!(
            out.rejected,
            vec![
                (slot, DecRefReject::Duplicate),
                (slot, DecRefReject::Duplicate)
            ]
        );
        assert_eq!(slab.get_slot(slot).unwrap().ref_count, 0);
        assert_eq!(slab.free_slots(), free_before + 1);

        cleanup(&path);
    }

    /// Counts writes reaching the device, to prove the batching actually
    /// collapses per-slot round trips rather than merely looking tidier.
    struct WriteCounter {
        inner: Arc<dyn BlockDevice>,
        writes: Arc<std::sync::atomic::AtomicUsize>,
        reads: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl BlockDevice for WriteCounter {
        fn id(&self) -> &super::super::DeviceId { self.inner.id() }
        fn capacity_bytes(&self) -> u64 { self.inner.capacity_bytes() }
        fn block_size(&self) -> u32 { self.inner.block_size() }
        fn optimal_io_size(&self) -> u32 { self.inner.optimal_io_size() }
        fn device_type(&self) -> super::super::DriveType { self.inner.device_type() }
        async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.read(offset, buf).await
        }
        async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
            self.writes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.write(offset, buf).await
        }
        async fn flush(&self) -> DriveResult<()> { self.inner.flush().await }
        async fn discard(&self, offset: u64, len: u64) -> DriveResult<()> {
            self.inner.discard(offset, len).await
        }
        fn smart_status(&self) -> DriveResult<super::super::SmartData> {
            self.inner.smart_status()
        }
    }

    /// A clone of an N-extent image must cost sectors touched, not N round
    /// trips — that is the whole point of the batching, and it is what keeps
    /// clone latency flat as VM images grow.
    #[tokio::test]
    async fn batched_refcounts_collapse_device_writes() {
        let (dev, path) = create_slab_device(64 * 1024 * 1024).await;
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counting: Arc<dyn BlockDevice> = Arc::new(WriteCounter {
            inner: dev.clone(),
            writes: writes.clone(),
            reads: reads.clone(),
        });

        let mut slab = Slab::format(counting, 64 * 1024, StorageTier::Hot)
            .await
            .unwrap();

        let vol = VolumeId::new();
        let mut slots = Vec::new();
        for vext in 0..256u64 {
            slots.push(slab.allocate(vol, vext).await.unwrap());
        }

        // 4096-byte sectors hold 64 entries each, so 256 contiguous slots are
        // 4 sectors: 4 writes, and no reads because each is fully covered.
        writes.store(0, std::sync::atomic::Ordering::Relaxed);
        reads.store(0, std::sync::atomic::Ordering::Relaxed);
        slab.inc_ref_batch(&slots).await.unwrap();

        let w = writes.load(std::sync::atomic::Ordering::Relaxed);
        let r = reads.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(w, 4, "256 slots over 64-entry sectors should be 4 writes, got {w}");
        assert_eq!(r, 0, "fully covered sectors need no read, got {r}");

        // Per-slot would have been 256 writes and 256 reads.
        assert!(w < slots.len() / 10);

        cleanup(&path);
    }

    /// Freeing a slot must hand the space back to the *device*, not just mark
    /// it free inside the slab. Without this a clone-per-container workload
    /// grows forever: allocation was measured going 72 -> 116 MB live and
    /// never coming down (#28).
    ///
    /// Measured by allocated blocks (`du`), not apparent size — hole punching
    /// deliberately keeps the length so slab offsets stay valid.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn freeing_slots_punches_holes_in_the_backing_file() {
        use std::os::unix::fs::MetadataExt;

        let (dev, path) = create_slab_device(64 * 1024 * 1024).await;
        let mut slab = Slab::format(dev.clone(), 1024 * 1024, StorageTier::Hot)
            .await
            .unwrap();

        let blocks = |p: &str| std::fs::metadata(p).map(|m| m.blocks()).unwrap_or(0);
        let baseline = blocks(&path);

        // Fill 16 slots with real data so they occupy blocks on disk.
        let vol = VolumeId::new();
        let mut slots = Vec::new();
        let payload = vec![0xA5u8; 1024 * 1024];
        for vext in 0..16u64 {
            let s = slab.allocate(vol, vext).await.unwrap();
            slab.write_slot(s, 0, &payload).await.unwrap();
            slots.push(s);
        }
        slab.device().flush().await.unwrap();
        let filled = blocks(&path);
        assert!(
            filled > baseline,
            "writing should allocate blocks: {baseline} -> {filled}"
        );

        // Release them through the batched path (what dropping a clone uses).
        let out = slab.dec_ref_batch(&slots).await.unwrap();
        assert_eq!(out.freed, slots.len());
        slab.device().flush().await.unwrap();

        let after = blocks(&path);
        assert!(
            after < filled,
            "freed slots must return blocks to the filesystem: {filled} -> {after}"
        );

        // The file keeps its length, so slab offsets remain valid.
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(len, 64 * 1024 * 1024, "punching must not truncate the slab");

        cleanup(&path);
    }

    /// The single-slot route must reclaim too, not only the batched one.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn single_slot_free_also_punches() {
        use std::os::unix::fs::MetadataExt;

        let (dev, path) = create_slab_device(32 * 1024 * 1024).await;
        let mut slab = Slab::format(dev.clone(), 1024 * 1024, StorageTier::Hot)
            .await
            .unwrap();

        let vol = VolumeId::new();
        let s = slab.allocate(vol, 0).await.unwrap();
        slab.write_slot(s, 0, &vec![0x5Au8; 1024 * 1024]).await.unwrap();
        slab.device().flush().await.unwrap();
        let filled = std::fs::metadata(&path).unwrap().blocks();

        slab.free(s).await.unwrap();
        slab.device().flush().await.unwrap();
        let after = std::fs::metadata(&path).unwrap().blocks();

        assert!(after < filled, "free() must discard: {filled} -> {after}");
        cleanup(&path);
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
