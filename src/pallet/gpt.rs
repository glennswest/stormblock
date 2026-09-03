//! GPT — the partition table pallets live in.
//!
//! stormblock needs its own reader/writer here rather than a crate because of
//! what it does with it: **activation is an attribute write**. Pallet state —
//! priority, tries left, successful, sealed, read-only — lives in attribute
//! bits 48–63 of the GPT entry, so selecting a different pallet to boot is two
//! entry rewrites and four CRCs, with no data written anywhere. That is the
//! whole reason rollback is cheap and cannot half-happen.
//!
//! Two rules the layout code enforces rather than documents:
//!
//! - **Never alias.** Benched in `stormcos/docs/BOOT.md`: firmware does not
//!   publish a handle for an overlapping entry. [`Gpt::allocate`] therefore
//!   allocates out of measured free runs and [`Gpt::insert`] refuses a range
//!   that touches an existing partition, rather than trusting a caller's
//!   arithmetic.
//! - **Both copies, always.** Every mutation rewrites the primary and the
//!   backup, so a header lost to a bad block is recoverable from the other end
//!   of the disk.

use std::sync::Arc;

use uuid::Uuid;

use crate::drive::{BlockDevice, DriveType};

use super::{crc32, format::PALLET_TYPE_GUID, PalletError, PartitionView, Result};

pub const GPT_SIGNATURE: [u8; 8] = *b"EFI PART";
pub const GPT_REVISION: u32 = 0x0001_0000;
pub const HEADER_SIZE: u32 = 92;
pub const ENTRY_SIZE: u32 = 128;
pub const NUM_ENTRIES: u32 = 128;
pub const NAME_UTF16_LEN: usize = 36;

/// Partitions are aligned to 1 MiB — the alignment every firmware, every
/// erase block and every RAID stripe in practice agrees on.
pub const ALIGN_BYTES: u64 = 1024 * 1024;

/// One GPT partition entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GptEntry {
    pub type_guid: [u8; 16],
    pub unique_guid: [u8; 16],
    pub first_lba: u64,
    /// Inclusive, as GPT stores it.
    pub last_lba: u64,
    pub attributes: u64,
    pub name: String,
}

impl GptEntry {
    pub fn empty() -> GptEntry {
        GptEntry {
            type_guid: [0; 16],
            unique_guid: [0; 16],
            first_lba: 0,
            last_lba: 0,
            attributes: 0,
            name: String::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.type_guid == [0u8; 16]
    }

    pub fn is_pallet(&self) -> bool {
        self.type_guid == PALLET_TYPE_GUID
    }

    pub fn block_count(&self) -> u64 {
        self.last_lba.saturating_sub(self.first_lba) + 1
    }

    pub fn size_bytes(&self, block_size: u32) -> u64 {
        self.block_count() * block_size as u64
    }

    pub fn start_bytes(&self, block_size: u32) -> u64 {
        self.first_lba * block_size as u64
    }

    /// The partition's `UniquePartitionGUID` as a UUID — the identity that is
    /// stable across byte-for-byte copies of the same pallet, and therefore the
    /// handle the lifecycle API uses.
    pub fn uuid(&self) -> Uuid {
        Uuid::from_bytes_le(self.unique_guid)
    }
}

/// The LBA size a GPT on this device should use.
///
/// This is **not** simply the device's block size. A file-backed device
/// reports 4096 because that is the I/O size it prefers, not because it has
/// 4 KiB sectors — and a file is how disk images and ISOs are assembled, where
/// every tool and every firmware assumes 512-byte LBAs. Getting this wrong
/// produces a table that this code can read back happily and nothing else can
/// find at all. A real 4Kn drive reports 4096 because it *is* 4Kn, and there
/// the answer is genuinely 4096.
pub fn default_lba_size(device: &Arc<dyn BlockDevice>) -> u32 {
    match device.device_type() {
        DriveType::File => 512,
        _ => device.block_size(),
    }
}

/// LBA sizes to try when locating a table someone else wrote.
const LBA_CANDIDATES: [u32; 2] = [512, 4096];

/// A GPT, in memory.
#[derive(Debug, Clone)]
pub struct Gpt {
    /// The LBA size this table is expressed in — see [`default_lba_size`].
    pub block_size: u32,
    pub disk_guid: [u8; 16],
    pub first_usable_lba: u64,
    pub last_usable_lba: u64,
    pub alternate_lba: u64,
    pub entries: Vec<GptEntry>,
    /// True when the primary header was unusable and this came off the backup.
    pub recovered_from_backup: bool,
}

fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn rd_u64(b: &[u8], o: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(v)
}

fn wr_u32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn wr_u64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

fn entries_bytes() -> u64 {
    NUM_ENTRIES as u64 * ENTRY_SIZE as u64
}

fn entries_lbas(block_size: u32) -> u64 {
    entries_bytes().div_ceil(block_size as u64)
}

impl Gpt {
    /// An empty table sized to the device, in the LBA size that device wants.
    pub fn create(device: &Arc<dyn BlockDevice>) -> Gpt {
        Gpt::create_with_lba(device, default_lba_size(device))
    }

    /// An empty table in an explicit LBA size — for an image that must be
    /// readable by something with a fixed idea of its sector size.
    pub fn create_with_lba(device: &Arc<dyn BlockDevice>, bs: u32) -> Gpt {
        Gpt::create_for(bs, device.capacity_bytes())
    }

    /// An empty table for a disk of `total_bytes`, with no device in hand —
    /// for a table that is going to be a golden rather than written straight
    /// onto something.
    pub fn create_for(bs: u32, total_bytes: u64) -> Gpt {
        let last_lba = total_bytes / bs as u64 - 1;
        let ent = entries_lbas(bs);
        Gpt {
            block_size: bs,
            disk_guid: Uuid::new_v4().to_bytes_le(),
            first_usable_lba: 2 + ent,
            last_usable_lba: last_lba - 1 - ent,
            alternate_lba: last_lba,
            entries: vec![GptEntry::empty(); NUM_ENTRIES as usize],
            recovered_from_backup: false,
        }
    }

    /// Byte length of the primary half: protective MBR, header, entry array.
    pub fn head_bytes(&self) -> u64 {
        (2 + entries_lbas(self.block_size)) * self.block_size as u64
    }

    /// Byte length of the backup half: entry array, then the header at the
    /// last LBA.
    pub fn tail_bytes(&self) -> u64 {
        (1 + entries_lbas(self.block_size)) * self.block_size as u64
    }

    /// Where the backup half starts, in bytes from the start of the disk.
    pub fn tail_offset(&self) -> u64 {
        (self.alternate_lba - entries_lbas(self.block_size)) * self.block_size as u64
    }

    /// Read the table, preferring the primary and falling back to the backup
    /// when the primary header does not check out.
    ///
    /// The LBA size is discovered rather than assumed: a disk written
    /// elsewhere may be 512 or 4Kn, and the difference is only visible by
    /// finding the header where one of them would put it.
    pub async fn read(device: &Arc<dyn BlockDevice>) -> Result<Gpt> {
        let view = PartitionView::whole(device.clone());
        let preferred = default_lba_size(device);
        let mut sizes = vec![preferred];
        sizes.extend(LBA_CANDIDATES.iter().copied().filter(|c| *c != preferred));

        let mut first_err = None;
        for bs in sizes.iter().copied() {
            if device.capacity_bytes() < 4 * bs as u64 {
                continue;
            }
            let last_lba = device.capacity_bytes() / bs as u64 - 1;
            match Gpt::read_at(&view, bs, 1).await {
                Ok(mut g) => {
                    g.alternate_lba = last_lba;
                    return Ok(g);
                }
                Err(e) => first_err.get_or_insert(e),
            };
            if let Ok(mut g) = Gpt::read_at(&view, bs, last_lba).await {
                g.recovered_from_backup = true;
                g.alternate_lba = last_lba;
                return Ok(g);
            }
        }
        Err(first_err.unwrap_or(PalletError::NotGpt))
    }

    async fn read_at(view: &PartitionView, bs: u32, header_lba: u64) -> Result<Gpt> {
        let mut hdr = vec![0u8; bs as usize];
        view.read_at(header_lba * bs as u64, &mut hdr).await?;
        if hdr[0..8] != GPT_SIGNATURE {
            return Err(PalletError::NotGpt);
        }
        let header_size = rd_u32(&hdr, 12);
        if header_size < HEADER_SIZE || header_size as usize > bs as usize {
            return Err(PalletError::BadGeometry(format!("GPT header size {header_size}")));
        }
        let stored = rd_u32(&hdr, 16);
        let mut check = hdr[..header_size as usize].to_vec();
        wr_u32(&mut check, 16, 0);
        if crc32(&check) != stored {
            return Err(PalletError::BadHeaderCrc);
        }

        let num = rd_u32(&hdr, 80);
        let esz = rd_u32(&hdr, 84);
        if esz < ENTRY_SIZE || num == 0 || num > 4096 {
            return Err(PalletError::BadGeometry(format!(
                "GPT entry array {num} × {esz}"
            )));
        }
        let entry_lba = rd_u64(&hdr, 72);
        let arr_len = (num as u64 * esz as u64) as usize;
        let mut arr = vec![0u8; arr_len];
        view.read_at(entry_lba * bs as u64, &mut arr).await?;
        if crc32(&arr) != rd_u32(&hdr, 88) {
            return Err(PalletError::BadEntryCrc);
        }

        let mut entries = Vec::with_capacity(num as usize);
        for i in 0..num as usize {
            entries.push(parse_entry(&arr[i * esz as usize..i * esz as usize + ENTRY_SIZE as usize]));
        }

        let mut disk_guid = [0u8; 16];
        disk_guid.copy_from_slice(&hdr[56..72]);
        Ok(Gpt {
            block_size: bs,
            disk_guid,
            first_usable_lba: rd_u64(&hdr, 40),
            last_usable_lba: rd_u64(&hdr, 48),
            alternate_lba: rd_u64(&hdr, 32),
            entries,
            recovered_from_backup: false,
        })
    }

    /// Write protective MBR, primary header + entries, and the backup pair.
    pub async fn write(&self, device: &Arc<dyn BlockDevice>) -> Result<()> {
        let view = PartitionView::whole(device.clone());
        let (head, tail) = self.render();
        view.write_at(self.tail_offset(), &tail).await?;
        view.write_at(0, &head).await?;
        view.flush().await?;
        Ok(())
    }

    /// The table as bytes: the primary half, which goes at offset 0, and the
    /// backup half, which goes at [`Gpt::tail_offset`].
    ///
    /// Rendering is separate from writing so that the two halves can be
    /// goldens — the first and last slot of a composed disk — and be shared
    /// by every disk with the same layout rather than written per disk.
    pub fn render(&self) -> (Vec<u8>, Vec<u8>) {
        let bs = self.block_size as u64;
        let ent = entries_lbas(self.block_size);

        let mut arr = vec![0u8; entries_bytes() as usize];
        for (i, e) in self.entries.iter().enumerate().take(NUM_ENTRIES as usize) {
            write_entry(&mut arr[i * ENTRY_SIZE as usize..(i + 1) * ENTRY_SIZE as usize], e);
        }
        let arr_crc = crc32(&arr);

        // Protective MBR — one 0xEE partition covering the disk, so a tool that
        // only understands MBR sees the disk as fully used rather than as free.
        let mut head = vec![0u8; self.head_bytes() as usize];
        let total_lba = self.alternate_lba + 1;
        head[446] = 0x00;
        head[447..450].copy_from_slice(&[0x00, 0x02, 0x00]);
        head[450] = 0xEE;
        head[451..454].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
        wr_u32(&mut head, 454, 1);
        wr_u32(&mut head, 458, (total_lba - 1).min(0xFFFF_FFFF) as u32);
        head[510] = 0x55;
        head[511] = 0xAA;

        let backup_entries_lba = self.alternate_lba - ent;
        let primary = self.header_bytes(1, self.alternate_lba, 2, arr_crc);
        let backup = self.header_bytes(self.alternate_lba, 1, backup_entries_lba, arr_crc);

        head[bs as usize..2 * bs as usize].copy_from_slice(&primary);
        head[2 * bs as usize..2 * bs as usize + arr.len()].copy_from_slice(&arr);

        let mut tail = vec![0u8; self.tail_bytes() as usize];
        tail[..arr.len()].copy_from_slice(&arr);
        let ho = (ent * bs) as usize;
        tail[ho..ho + bs as usize].copy_from_slice(&backup);
        (head, tail)
    }

    fn header_bytes(&self, my_lba: u64, alt_lba: u64, entry_lba: u64, arr_crc: u32) -> Vec<u8> {
        let mut h = vec![0u8; self.block_size as usize];
        h[0..8].copy_from_slice(&GPT_SIGNATURE);
        wr_u32(&mut h, 8, GPT_REVISION);
        wr_u32(&mut h, 12, HEADER_SIZE);
        wr_u64(&mut h, 24, my_lba);
        wr_u64(&mut h, 32, alt_lba);
        wr_u64(&mut h, 40, self.first_usable_lba);
        wr_u64(&mut h, 48, self.last_usable_lba);
        h[56..72].copy_from_slice(&self.disk_guid);
        wr_u64(&mut h, 72, entry_lba);
        wr_u32(&mut h, 80, NUM_ENTRIES);
        wr_u32(&mut h, 84, ENTRY_SIZE);
        wr_u32(&mut h, 88, arr_crc);
        let crc = crc32(&h[..HEADER_SIZE as usize]);
        wr_u32(&mut h, 16, crc);
        h
    }

    /// Every non-empty entry, with its index.
    pub fn partitions(&self) -> impl Iterator<Item = (usize, &GptEntry)> {
        self.entries.iter().enumerate().filter(|(_, e)| !e.is_empty())
    }

    /// Every pallet partition, with its index.
    pub fn pallets(&self) -> impl Iterator<Item = (usize, &GptEntry)> {
        self.entries.iter().enumerate().filter(|(_, e)| e.is_pallet())
    }

    pub fn find_by_uuid(&self, id: Uuid) -> Option<(usize, &GptEntry)> {
        self.partitions().find(|(_, e)| e.uuid() == id)
    }

    /// Free LBA runs between the existing partitions, in disk order.
    pub fn free_runs(&self) -> Vec<(u64, u64)> {
        let mut used: Vec<(u64, u64)> =
            self.partitions().map(|(_, e)| (e.first_lba, e.last_lba)).collect();
        used.sort_unstable();
        let mut runs = Vec::new();
        let mut cursor = self.first_usable_lba;
        for (first, last) in used {
            if first > cursor {
                runs.push((cursor, first - 1));
            }
            cursor = cursor.max(last + 1);
        }
        if cursor <= self.last_usable_lba {
            runs.push((cursor, self.last_usable_lba));
        }
        runs
    }

    /// Largest free run, in bytes — what an out-of-space error reports.
    pub fn largest_free_bytes(&self) -> u64 {
        self.free_runs()
            .into_iter()
            .map(|(a, b)| (b - a + 1) * self.block_size as u64)
            .max()
            .unwrap_or(0)
    }

    fn first_free_slot(&self) -> Option<usize> {
        self.entries.iter().position(|e| e.is_empty())
    }

    /// Place a new partition in the first free run big enough, 1 MiB aligned.
    ///
    /// Returns the entry index. The caller gets a partition that provably
    /// overlaps nothing, because the range came from [`Gpt::free_runs`] rather
    /// than from arithmetic on where the last one ended.
    pub fn allocate(
        &mut self,
        name: &str,
        type_guid: [u8; 16],
        size_bytes: u64,
        attributes: u64,
    ) -> Result<usize> {
        let bs = self.block_size as u64;
        let align = (ALIGN_BYTES / bs).max(1);
        let need = size_bytes.div_ceil(bs);
        let slot = self.first_free_slot().ok_or(PalletError::NoSpace {
            need: size_bytes,
            largest_free: self.largest_free_bytes(),
        })?;

        for (start, end) in self.free_runs() {
            let aligned = start.div_ceil(align) * align;
            if aligned > end {
                continue;
            }
            if end - aligned + 1 >= need {
                self.entries[slot] = GptEntry {
                    type_guid,
                    unique_guid: Uuid::new_v4().to_bytes_le(),
                    first_lba: aligned,
                    last_lba: aligned + need - 1,
                    attributes,
                    name: name.to_string(),
                };
                return Ok(slot);
            }
        }
        Err(PalletError::NoSpace { need: size_bytes, largest_free: self.largest_free_bytes() })
    }

    /// Place a partition at an explicit range, refusing any overlap.
    pub fn insert(&mut self, entry: GptEntry) -> Result<usize> {
        if entry.first_lba < self.first_usable_lba || entry.last_lba > self.last_usable_lba {
            return Err(PalletError::NoSpace {
                need: entry.block_count() * self.block_size as u64,
                largest_free: self.largest_free_bytes(),
            });
        }
        if let Some((_, other)) = self
            .partitions()
            .find(|(_, e)| entry.first_lba <= e.last_lba && e.first_lba <= entry.last_lba)
        {
            return Err(PalletError::Overlaps { with: other.name.clone() });
        }
        let slot = self.first_free_slot().ok_or(PalletError::NoSpace {
            need: entry.block_count() * self.block_size as u64,
            largest_free: self.largest_free_bytes(),
        })?;
        self.entries[slot] = entry;
        Ok(slot)
    }

    /// Clear an entry. The bytes stay on disk; only the reference goes.
    pub fn remove(&mut self, index: usize) -> Result<GptEntry> {
        let e = self
            .entries
            .get(index)
            .filter(|e| !e.is_empty())
            .cloned()
            .ok_or_else(|| PalletError::NotFound(format!("partition {index}")))?;
        self.entries[index] = GptEntry::empty();
        Ok(e)
    }

    /// A byte window onto one partition — the only place a partition-relative
    /// offset becomes an absolute one.
    pub fn view(&self, device: &Arc<dyn BlockDevice>, index: usize) -> Result<PartitionView> {
        let e = self
            .entries
            .get(index)
            .filter(|e| !e.is_empty())
            .ok_or_else(|| PalletError::NotFound(format!("partition {index}")))?;
        Ok(PartitionView::new(
            device.clone(),
            e.start_bytes(self.block_size),
            e.size_bytes(self.block_size),
        ))
    }
}

fn parse_entry(b: &[u8]) -> GptEntry {
    let mut type_guid = [0u8; 16];
    type_guid.copy_from_slice(&b[0..16]);
    let mut unique_guid = [0u8; 16];
    unique_guid.copy_from_slice(&b[16..32]);
    let mut units = Vec::with_capacity(NAME_UTF16_LEN);
    for i in 0..NAME_UTF16_LEN {
        let u = u16::from_le_bytes([b[56 + i * 2], b[57 + i * 2]]);
        if u == 0 {
            break;
        }
        units.push(u);
    }
    GptEntry {
        type_guid,
        unique_guid,
        first_lba: rd_u64(b, 32),
        last_lba: rd_u64(b, 40),
        attributes: rd_u64(b, 48),
        name: String::from_utf16_lossy(&units),
    }
}

fn write_entry(b: &mut [u8], e: &GptEntry) {
    b[0..16].copy_from_slice(&e.type_guid);
    b[16..32].copy_from_slice(&e.unique_guid);
    wr_u64(b, 32, e.first_lba);
    wr_u64(b, 40, e.last_lba);
    wr_u64(b, 48, e.attributes);
    for (i, u) in e.name.encode_utf16().take(NAME_UTF16_LEN).enumerate() {
        b[56 + i * 2..58 + i * 2].copy_from_slice(&u.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(block_size: u32, last_lba: u64) -> Gpt {
        let ent = entries_lbas(block_size);
        Gpt {
            block_size,
            disk_guid: Uuid::new_v4().to_bytes_le(),
            first_usable_lba: 2 + ent,
            last_usable_lba: last_lba - 1 - ent,
            alternate_lba: last_lba,
            entries: vec![GptEntry::empty(); NUM_ENTRIES as usize],
            recovered_from_backup: false,
        }
    }

    /// 128 MiB of 512-byte blocks.
    fn small() -> Gpt {
        table(512, 128 * 1024 * 1024 / 512 - 1)
    }

    #[test]
    fn allocation_aligns_to_a_megabyte_and_never_overlaps() {
        let mut g = small();
        let a = g.allocate("one", PALLET_TYPE_GUID, 3 * 1024 * 1024, 0).unwrap();
        let b = g.allocate("two", PALLET_TYPE_GUID, 3 * 1024 * 1024, 0).unwrap();

        let align = ALIGN_BYTES / 512;
        assert_eq!(g.entries[a].first_lba % align, 0);
        assert_eq!(g.entries[b].first_lba % align, 0);
        assert!(
            g.entries[a].last_lba < g.entries[b].first_lba,
            "firmware does not publish a handle for an overlapping entry"
        );
        assert!(g.entries[a].size_bytes(512) >= 3 * 1024 * 1024);
    }

    #[test]
    fn a_freed_run_is_reused() {
        let mut g = small();
        let a = g.allocate("one", PALLET_TYPE_GUID, 4 * 1024 * 1024, 0).unwrap();
        let first = g.entries[a].first_lba;
        g.allocate("two", PALLET_TYPE_GUID, 4 * 1024 * 1024, 0).unwrap();
        g.remove(a).unwrap();
        let c = g.allocate("three", PALLET_TYPE_GUID, 2 * 1024 * 1024, 0).unwrap();
        assert_eq!(g.entries[c].first_lba, first, "first-fit takes the hole back");
    }

    #[test]
    fn an_overlapping_insert_is_refused() {
        let mut g = small();
        let a = g.allocate("one", PALLET_TYPE_GUID, 4 * 1024 * 1024, 0).unwrap();
        let mut clash = GptEntry::empty();
        clash.type_guid = PALLET_TYPE_GUID;
        clash.unique_guid = Uuid::new_v4().to_bytes_le();
        clash.first_lba = g.entries[a].last_lba;
        clash.last_lba = clash.first_lba + 100;
        clash.name = "clash".into();
        match g.insert(clash) {
            Err(PalletError::Overlaps { with }) => assert_eq!(with, "one"),
            other => panic!("expected an overlap refusal, got {other:?}"),
        }
    }

    #[test]
    fn running_out_of_room_reports_the_largest_run_there_is() {
        let mut g = small();
        let err = g.allocate("huge", PALLET_TYPE_GUID, 1024 * 1024 * 1024, 0).unwrap_err();
        match err {
            PalletError::NoSpace { largest_free, .. } => {
                assert!(largest_free > 100 * 1024 * 1024, "{largest_free}")
            }
            other => panic!("expected NoSpace, got {other:?}"),
        }
    }

    #[test]
    fn an_entry_round_trips_through_its_on_disk_form() {
        let e = GptEntry {
            type_guid: PALLET_TYPE_GUID,
            unique_guid: Uuid::new_v4().to_bytes_le(),
            first_lba: 2048,
            last_lba: 6143,
            attributes: 0x000F_0000_0000_0001,
            name: "stormcos-boot".into(),
        };
        let mut b = [0u8; ENTRY_SIZE as usize];
        write_entry(&mut b, &e);
        assert_eq!(parse_entry(&b), e);
        assert!(e.is_pallet());
        assert_eq!(e.block_count(), 4096);
    }

    #[test]
    fn free_runs_account_for_every_partition_wherever_it_sits() {
        let mut g = table(4096, 32 * 1024 * 1024 / 4096 - 1);
        let a = g.allocate("a", PALLET_TYPE_GUID, 2 * 1024 * 1024, 0).unwrap();
        let b = g.allocate("b", PALLET_TYPE_GUID, 2 * 1024 * 1024, 0).unwrap();
        g.remove(a).unwrap();
        let runs = g.free_runs();
        // The hole before `b`, and everything after it.
        assert_eq!(runs.len(), 2, "{runs:?}");
        assert!(runs[0].1 < g.entries[b].first_lba);
        assert!(runs[1].0 > g.entries[b].last_lba);
    }
}
