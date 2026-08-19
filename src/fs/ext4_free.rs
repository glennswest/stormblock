//! Read-only ext2/ext3/ext4 layout reader — enough of the on-disk format to
//! find the blocks a filesystem is *not* using.
//!
//! All three are read by one path, because the three structures it touches —
//! the superblock, the group descriptor table and the per-group block bitmaps
//! — are common to all of them. It never walks an inode, so extents (ext4) and
//! indirect blocks (ext2/ext3) never come up. The kind only shows in geometry:
//! a filesystem without `64bit` has 32-byte descriptors, and at 1 KiB blocks
//! `first_data_block` is 1 rather than 0. The engine formats any of the three
//! (`fs` on a template), so all three arrive here.
//!
//! Why this exists (issue #3): thin allocation only ever grew, because the
//! initiator never issues UNMAP. The engine handles UNMAP end to end
//! (`handle_unmap` → `BlockDevice::discard` → GEM free + slab `dec_ref`), but
//! Linux only enables discard when the target advertises VPD page B2h
//! (Logical Block Provisioning), and the engine advertises LBPME in READ
//! CAPACITY(16) without the B2h page — so `sd` lands on `SD_LBP_DISABLE` and
//! never sends one. That is an engine gap, filed upstream; mk must not patch
//! the engine. So mk reclaims from its own side instead: read the ext4 block
//! bitmaps straight off the volume and discard what the filesystem has freed
//! — an offline `fstrim` driven by the target rather than the initiator.
//!
//! Everything here is READ-only and deliberately paranoid: any feature or
//! field it does not fully understand aborts the scan instead of guessing.
//! A wrong answer here discards live data.

use std::sync::Arc;

use crate::drive::BlockDevice;

const SUPERBLOCK_OFFSET: u64 = 1024;
const SUPERBLOCK_LEN: usize = 1024;
const EXT4_MAGIC: u16 = 0xEF53;

const INCOMPAT_RECOVER: u32 = 0x0004;
const INCOMPAT_META_BG: u32 = 0x0010;
const INCOMPAT_64BIT: u32 = 0x0080;

/// Group descriptor flag: the block bitmap on disk is not initialised.
const BG_BLOCK_UNINIT: u16 = 0x0002;

/// State bit: the filesystem was unmounted cleanly.
const STATE_CLEANLY_UNMOUNTED: u16 = 0x0001;

fn le16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// The parts of the superblock the scan needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ext4Layout {
    pub block_size: u64,
    pub blocks_count: u64,
    pub first_data_block: u64,
    pub blocks_per_group: u64,
    pub desc_size: u64,
    pub sixty_four_bit: bool,
    /// First block of the group descriptor table.
    pub gdt_block: u64,
    pub group_count: u64,
    /// Unmounted cleanly and no journal replay pending.
    pub clean: bool,
}

impl Ext4Layout {
    pub fn size_bytes(&self) -> u64 {
        self.blocks_count * self.block_size
    }
}

/// Parse the 1 KiB superblock. `sb` must be the bytes at offset 1024.
pub fn parse_superblock(sb: &[u8]) -> anyhow::Result<Ext4Layout> {
    if sb.len() < SUPERBLOCK_LEN {
        anyhow::bail!("short superblock read ({} bytes)", sb.len());
    }
    let magic = le16(sb, 0x38);
    if magic != EXT4_MAGIC {
        anyhow::bail!("not an ext2/3/4 filesystem (magic {magic:#06x})");
    }

    let log_block_size = le32(sb, 0x18);
    if log_block_size > 6 {
        anyhow::bail!("implausible s_log_block_size {log_block_size}");
    }
    let block_size = 1024u64 << log_block_size;

    let feature_incompat = le32(sb, 0x60);
    if feature_incompat & INCOMPAT_META_BG != 0 {
        anyhow::bail!("META_BG layout is not supported by the mk trim scanner");
    }
    let sixty_four_bit = feature_incompat & INCOMPAT_64BIT != 0;
    let needs_recovery = feature_incompat & INCOMPAT_RECOVER != 0;

    let blocks_count = if sixty_four_bit {
        ((le32(sb, 0x150) as u64) << 32) | le32(sb, 0x04) as u64
    } else {
        le32(sb, 0x04) as u64
    };
    let first_data_block = le32(sb, 0x14) as u64;
    let blocks_per_group = le32(sb, 0x20) as u64;
    if blocks_count == 0 || blocks_per_group == 0 || blocks_count <= first_data_block {
        anyhow::bail!(
            "nonsensical geometry: blocks={blocks_count}, per_group={blocks_per_group}, first={first_data_block}"
        );
    }
    // One block's worth of bits per bitmap — anything else means we have
    // misread the superblock.
    if blocks_per_group > block_size * 8 {
        anyhow::bail!(
            "blocks_per_group {blocks_per_group} exceeds one bitmap block ({} bits)",
            block_size * 8
        );
    }

    let desc_size = if sixty_four_bit {
        let d = le16(sb, 0xFE) as u64;
        if d < 64 || d > block_size || !d.is_power_of_two() {
            anyhow::bail!("bad s_desc_size {d} for a 64bit filesystem");
        }
        d
    } else {
        32
    };

    let state = le16(sb, 0x3A);
    let clean = (state & STATE_CLEANLY_UNMOUNTED) != 0 && !needs_recovery;

    let group_count = (blocks_count - first_data_block).div_ceil(blocks_per_group);
    let gdt_block = first_data_block + 1;

    Ok(Ext4Layout {
        block_size,
        blocks_count,
        first_data_block,
        blocks_per_group,
        desc_size,
        sixty_four_bit,
        gdt_block,
        group_count,
        clean,
    })
}

/// Block number of a group's block bitmap, from its descriptor.
pub fn bitmap_block(desc: &[u8], layout: &Ext4Layout) -> anyhow::Result<u64> {
    if desc.len() < 32 {
        anyhow::bail!("short group descriptor");
    }
    let lo = le32(desc, 0x00) as u64;
    let hi = if layout.sixty_four_bit && desc.len() >= 36 { le32(desc, 0x20) as u64 } else { 0 };
    let block = (hi << 32) | lo;
    if block == 0 || block >= layout.blocks_count {
        anyhow::bail!("group bitmap block {block} out of range (fs has {} blocks)", layout.blocks_count);
    }
    Ok(block)
}

/// True when the descriptor says its block bitmap is not initialised on disk.
/// Such a group is skipped entirely — the bytes there mean nothing.
pub fn block_uninit(desc: &[u8]) -> bool {
    desc.len() >= 20 && (le16(desc, 0x12) & BG_BLOCK_UNINIT) != 0
}

/// Byte ranges (relative to the start of the volume) that the filesystem
/// records as free, derived from one group's block bitmap.
///
/// Bit `i` of the bitmap covers block `group_first_block + i`; a SET bit means
/// in use. Bits past `blocks_in_group` are padding and are ignored.
pub fn free_runs(
    bitmap: &[u8],
    group_first_block: u64,
    blocks_in_group: u64,
    block_size: u64,
) -> Vec<(u64, u64)> {
    let mut runs: Vec<(u64, u64)> = Vec::new();
    let mut run_start: Option<u64> = None;

    for i in 0..blocks_in_group {
        let byte = (i / 8) as usize;
        let free = match bitmap.get(byte) {
            // Missing bitmap bytes are treated as in-use, never as free.
            None => false,
            Some(b) => (b >> (i % 8)) & 1 == 0,
        };
        let block = group_first_block + i;
        if free {
            run_start.get_or_insert(block);
        } else if let Some(start) = run_start.take() {
            runs.push((start * block_size, (block - start) * block_size));
        }
    }
    if let Some(start) = run_start.take() {
        let end = group_first_block + blocks_in_group;
        runs.push((start * block_size, (end - start) * block_size));
    }
    runs
}

/// Read `len` bytes at `offset` honouring the device's alignment requirement.
///
/// `BlockDevice::read` demands a block-aligned offset; the ext4 structures we
/// want are not (the superblock lives at byte 1024). Read the aligned window
/// that contains the range and slice it out.
pub async fn read_at(dev: &Arc<dyn BlockDevice>, offset: u64, len: usize) -> anyhow::Result<Vec<u8>> {
    let bs = dev.block_size().max(1) as u64;
    let start = (offset / bs) * bs;
    let end = (offset + len as u64).div_ceil(bs) * bs;
    if end > dev.capacity_bytes() {
        anyhow::bail!(
            "read {offset}+{len} runs past the end of the volume ({} bytes)",
            dev.capacity_bytes()
        );
    }
    let mut buf = vec![0u8; (end - start) as usize];
    dev.read(start, &mut buf)
        .await
        .map_err(|e| anyhow::anyhow!("reading volume at {start}: {e}"))?;
    let skip = (offset - start) as usize;
    Ok(buf[skip..skip + len].to_vec())
}

/// What an offline trim scan found.
#[derive(Debug, Clone, Default)]
pub struct FreeMap {
    /// Contiguous free byte ranges, volume-relative.
    pub runs: Vec<(u64, u64)>,
    /// Total bytes the filesystem considers free.
    pub free_bytes: u64,
    /// Of those, the bytes that fall in whole slot-aligned slots — the only
    /// ones a discard can actually give back.
    pub reclaimable_bytes: u64,
    /// Groups skipped because their bitmap was uninitialised.
    pub groups_skipped: u64,
    pub groups_scanned: u64,
    pub layout: Option<Ext4Layout>,
}

/// Scan a volume's ext4 block bitmaps.
///
/// `slot_size` is the volume manager's allocation granularity: only whole
/// slots can be returned to the slab, so partial runs are counted but not
/// worth discarding.
pub async fn scan(dev: &Arc<dyn BlockDevice>, slot_size: u64) -> anyhow::Result<FreeMap> {
    let sb = read_at(dev, SUPERBLOCK_OFFSET, SUPERBLOCK_LEN).await?;
    let layout = parse_superblock(&sb)?;
    if layout.size_bytes() > dev.capacity_bytes() {
        anyhow::bail!(
            "filesystem claims {} bytes but the volume is {} — refusing to scan",
            layout.size_bytes(),
            dev.capacity_bytes()
        );
    }

    let mut map = FreeMap { layout: Some(layout.clone()), ..Default::default() };

    // Group descriptor table, read one group at a time to keep the buffer
    // small on a many-group filesystem.
    for g in 0..layout.group_count {
        let desc_off = layout.gdt_block * layout.block_size + g * layout.desc_size;
        let desc = read_at(dev, desc_off, layout.desc_size as usize).await?;
        if block_uninit(&desc) {
            map.groups_skipped += 1;
            continue;
        }
        let bmp_block = bitmap_block(&desc, &layout)?;
        let bitmap = read_at(dev, bmp_block * layout.block_size, layout.block_size as usize).await?;

        let group_first_block = layout.first_data_block + g * layout.blocks_per_group;
        let blocks_in_group =
            layout.blocks_per_group.min(layout.blocks_count.saturating_sub(group_first_block));
        if blocks_in_group == 0 {
            continue;
        }

        for (start, len) in free_runs(&bitmap, group_first_block, blocks_in_group, layout.block_size) {
            map.free_bytes += len;
            map.reclaimable_bytes += aligned_span(start, len, slot_size).map(|(_, l)| l).unwrap_or(0);
            map.runs.push((start, len));
        }
        map.groups_scanned += 1;
    }

    // Merge runs that touch across a group boundary so a slot straddling two
    // groups is still reclaimable.
    map.runs.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(map.runs.len());
    for (start, len) in map.runs.drain(..) {
        match merged.last_mut() {
            Some((ps, pl)) if *ps + *pl == start => *pl += len,
            _ => merged.push((start, len)),
        }
    }
    map.runs = merged;
    map.reclaimable_bytes = map
        .runs
        .iter()
        .filter_map(|(s, l)| aligned_span(*s, *l, slot_size))
        .map(|(_, l)| l)
        .sum();

    Ok(map)
}

/// The whole-slot span inside a free run, or `None` when the run does not
/// contain one. Slot 0 is never offered: the superblock and the group
/// descriptor table live there, and no amount of bitmap arithmetic is worth
/// risking them.
pub fn aligned_span(start: u64, len: u64, slot_size: u64) -> Option<(u64, u64)> {
    if slot_size == 0 {
        return None;
    }
    let first = start.div_ceil(slot_size).max(1) * slot_size;
    let last = ((start + len) / slot_size) * slot_size;
    // NOT `then_some(last - first)`: that argument is evaluated before the
    // guard is consulted, so a run containing no whole aligned slot underflows
    // — a panic in debug, and in release a length near u64::MAX that the guard
    // then usually, but only usually, throws away. This value is fed straight
    // to `discard` (stormblockmk#13, found while porting this reader into
    // stormblock core).
    if last > first {
        Some((first, last - first))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4 KiB-block, 64bit ext4 superblock with 32768 blocks/group.
    fn sample_sb() -> Vec<u8> {
        let mut sb = vec![0u8; SUPERBLOCK_LEN];
        sb[0x04..0x08].copy_from_slice(&65536u32.to_le_bytes()); // blocks_count_lo
        sb[0x14..0x18].copy_from_slice(&0u32.to_le_bytes()); // first_data_block
        sb[0x18..0x1C].copy_from_slice(&2u32.to_le_bytes()); // 1024<<2 = 4096
        sb[0x20..0x24].copy_from_slice(&32768u32.to_le_bytes()); // blocks_per_group
        sb[0x38..0x3A].copy_from_slice(&EXT4_MAGIC.to_le_bytes());
        sb[0x3A..0x3C].copy_from_slice(&STATE_CLEANLY_UNMOUNTED.to_le_bytes());
        sb[0x60..0x64].copy_from_slice(&INCOMPAT_64BIT.to_le_bytes());
        sb[0xFE..0x100].copy_from_slice(&64u16.to_le_bytes()); // desc_size
        sb
    }

    #[test]
    fn parses_a_plain_superblock() {
        let l = parse_superblock(&sample_sb()).unwrap();
        assert_eq!(l.block_size, 4096);
        assert_eq!(l.blocks_count, 65536);
        assert_eq!(l.blocks_per_group, 32768);
        assert_eq!(l.desc_size, 64);
        assert_eq!(l.group_count, 2);
        assert_eq!(l.gdt_block, 1);
        assert!(l.clean);
    }

    /// A 1 KiB-block ext3 superblock: no `64bit`, so 32-byte group
    /// descriptors, and `first_data_block` 1 because the superblock itself
    /// occupies block 1 at this block size. 256 MiB, 8192 blocks per group.
    fn sample_ext3_sb() -> Vec<u8> {
        let mut sb = vec![0u8; SUPERBLOCK_LEN];
        sb[0x04..0x08].copy_from_slice(&262_144u32.to_le_bytes()); // blocks_count_lo
        sb[0x14..0x18].copy_from_slice(&1u32.to_le_bytes()); // first_data_block
        sb[0x18..0x1C].copy_from_slice(&0u32.to_le_bytes()); // 1024<<0 = 1024
        sb[0x20..0x24].copy_from_slice(&8192u32.to_le_bytes()); // blocks_per_group
        sb[0x38..0x3A].copy_from_slice(&EXT4_MAGIC.to_le_bytes());
        sb[0x3A..0x3C].copy_from_slice(&STATE_CLEANLY_UNMOUNTED.to_le_bytes());
        // compat HAS_JOURNAL — an ext3 carries one, and the scanner does not
        // read the compat word at all: only a *pending replay* (incompat
        // RECOVER) makes a filesystem unsafe to scan.
        sb[0x5C..0x60].copy_from_slice(&0x0004u32.to_le_bytes());
        sb
    }

    /// The engine formats ext2/ext3 as well as ext4 (`fs` on a template), and
    /// those carry neither `64bit` nor extents. Nothing here reads an extent
    /// tree — only the superblock, the group descriptors and the block bitmaps,
    /// which are the same three structures in all three — so the scan applies
    /// unchanged. This pins that: the geometry that differs is the descriptor
    /// size and `first_data_block`, and both have to come out right or the GDT
    /// is read at the wrong offset and every bitmap address after it is wrong.
    #[test]
    fn parses_an_ext3_superblock() {
        let l = parse_superblock(&sample_ext3_sb()).unwrap();
        assert_eq!(l.block_size, 1024);
        assert_eq!(l.blocks_count, 262_144);
        assert_eq!(l.blocks_per_group, 8192);
        assert!(!l.sixty_four_bit);
        assert_eq!(l.desc_size, 32, "a non-64bit filesystem has 32-byte descriptors");
        assert_eq!(l.first_data_block, 1, "1 KiB blocks put the superblock in block 1");
        assert_eq!(l.gdt_block, 2);
        assert_eq!(l.group_count, 32);
        assert!(l.clean);

        // A journal that still needs replaying is not scannable, however clean
        // the state word claims to be — the bitmaps on disk predate the replay.
        let mut sb = sample_ext3_sb();
        sb[0x60..0x64].copy_from_slice(&INCOMPAT_RECOVER.to_le_bytes());
        assert!(!parse_superblock(&sb).unwrap().clean);
    }

    /// A 32-byte descriptor has no high half to read: `bg_block_bitmap_hi`
    /// lives at 0x20, one byte past the end of one. Reading it anyway would
    /// take whatever followed in the GDT as the top 32 bits of a bitmap
    /// address, and the scan would then read some other group's bitmap.
    #[test]
    fn a_32_byte_descriptor_has_no_high_half() {
        let layout = parse_superblock(&sample_ext3_sb()).unwrap();
        let mut desc = vec![0u8; 32];
        desc[0x00..0x04].copy_from_slice(&5u32.to_le_bytes()); // bg_block_bitmap_lo
        assert_eq!(bitmap_block(&desc, &layout).unwrap(), 5);

        // The same descriptor with 64bit bytes appended stays a 32-byte read
        // for this filesystem, because the layout says the descriptors are 32.
        assert!(!layout.sixty_four_bit);
    }

    #[test]
    fn rejects_non_ext4_and_unsupported_layouts() {
        let mut sb = sample_sb();
        sb[0x38..0x3A].copy_from_slice(&0u16.to_le_bytes());
        assert!(parse_superblock(&sb).is_err());

        let mut sb = sample_sb();
        sb[0x60..0x64].copy_from_slice(&(INCOMPAT_64BIT | INCOMPAT_META_BG).to_le_bytes());
        assert!(parse_superblock(&sb).is_err());
    }

    #[test]
    fn recovery_pending_is_not_clean() {
        let mut sb = sample_sb();
        sb[0x60..0x64].copy_from_slice(&(INCOMPAT_64BIT | INCOMPAT_RECOVER).to_le_bytes());
        assert!(!parse_superblock(&sb).unwrap().clean);
    }

    #[test]
    fn free_runs_are_lsb_first_and_bounded() {
        // 0b0000_0011 → blocks 0,1 in use; 2..8 free.
        let bitmap = vec![0b0000_0011u8, 0x00];
        let runs = free_runs(&bitmap, 0, 16, 4096);
        assert_eq!(runs, vec![(2 * 4096, 14 * 4096)]);

        // Bits beyond blocks_in_group are ignored.
        let runs = free_runs(&bitmap, 0, 4, 4096);
        assert_eq!(runs, vec![(2 * 4096, 2 * 4096)]);
    }

    #[test]
    fn missing_bitmap_bytes_count_as_in_use() {
        assert_eq!(free_runs(&[], 0, 8, 4096), vec![]);
    }

    #[test]
    fn aligned_span_never_offers_slot_zero() {
        let slot = 4 * 1024 * 1024;
        assert_eq!(aligned_span(0, 16 * slot, slot), Some((slot, 15 * slot)));
        assert_eq!(aligned_span(slot + 1, slot - 2, slot), None);
        assert_eq!(aligned_span(2 * slot, 3 * slot, slot), Some((2 * slot, 3 * slot)));
    }

    /// A run holding no whole aligned slot must yield `None` without
    /// evaluating `last - first` (stormblockmk#13). These cases all have
    /// `last < first`; before the fix each underflowed — a panic in debug, and
    /// in release a length near `u64::MAX` handed towards `discard`, saved only
    /// by the guard that runs afterwards. `cargo test` in **debug** is what
    /// catches this; the release profile wraps silently.
    #[test]
    fn a_run_with_no_whole_slot_never_underflows() {
        let slot = 4 * 1024 * 1024;
        for (start, len) in [
            (slot + 1, slot - 2),      // straddles one boundary, shorter than a slot
            (slot + 1, 1),             // a single byte mid-slot
            (3 * slot - 1, 2),         // spans a boundary by one byte either side
            (slot, 0),                 // empty run on a boundary
            (0, 1),                    // inside slot 0, which is never offered
            (0, slot),                 // exactly slot 0, still never offered
            (u64::MAX / 2 + 1, 3),     // far offset, still no whole slot
        ] {
            assert_eq!(aligned_span(start, len, slot), None, "start={start} len={len}");
        }
    }
}
