//! ext4 — enough of the on-disk format to *create* an empty filesystem, to
//! read back the flags a consumer acts on, and to re-stamp identity on a clone.
//!
//! Three jobs, all of them storage jobs:
//!
//! 1. **mkfs (empty only).** An empty filesystem needs far less than a general
//!    ext4 implementation: a superblock, a group descriptor table, per-group
//!    bitmaps and inode tables, a root directory, `lost+found`, and optionally
//!    a journal. That is what a blank template is, and it is small enough to
//!    stay pure Rust — the engine takes no C dependency for it. Writing image
//!    *content* into a filesystem is a different problem and stays with the
//!    consumer that owns the content.
//! 2. **Inspect.** `parse_superblock` reads the fields a seal guard has to
//!    check. Checking only `VALID_FS` is what let a template with `ERROR_FS`
//!    set and `RECOVER` pending seal cleanly and then mount read-only on
//!    RouterOS days later (stormblock-registry#10), so `check_sealable`
//!    checks every flag a mount actually acts on.
//! 3. **Stamp.** A CoW clone is byte-identical to its template, filesystem
//!    UUID included, so two clones mounted on one host collide on
//!    mount-by-UUID and in the blkid cache. `stamp_uuid` rewrites it at clone
//!    time — 16 bytes into a block that is being materialised anyway
//!    (stormblockmk#12).
//!
//! **Feature set is deliberately conservative**: `EXTENTS|FILETYPE` incompat,
//! `SPARSE_SUPER|LARGE_FILE|EXTRA_ISIZE` ro_compat. No `metadata_csum`, no
//! `64bit`, no `bigalloc`, no `quota`, no `dir_index`. That is the set
//! verified to mount read-write on RouterOS 7.22.2, and it is also what makes
//! `stamp_uuid` a plain 16-byte patch: with `metadata_csum` the same edit
//! would mean recomputing every group checksum.

use std::sync::Arc;

use uuid::Uuid;

use crate::drive::BlockDevice;

/// The primary superblock always lives 1024 bytes into the filesystem.
pub const SUPERBLOCK_OFFSET: u64 = 1024;
/// On-disk superblock length.
pub const SUPERBLOCK_LEN: usize = 1024;

const EXT4_MAGIC: u16 = 0xEF53;

/// The only block size this writer emits. Readers accept any.
pub const BLOCK_SIZE: u32 = 4096;
const INODE_SIZE: u16 = 256;
/// Inodes 1..=10 are reserved; 11 is `lost+found`.
const FIRST_INODE: u32 = 11;
const ROOT_INODE: u32 = 2;
const JOURNAL_INODE: u32 = 8;
const LOST_FOUND_INODE: u32 = 11;
/// Reserved inodes that are always marked in use (1..=10) plus `lost+found`.
const RESERVED_INODES: u32 = 11;

// s_state bits.
pub const STATE_VALID_FS: u16 = 0x0001;
pub const STATE_ERROR_FS: u16 = 0x0002;
pub const STATE_ORPHAN_RECOVERING: u16 = 0x0004;

// s_feature_compat
const COMPAT_HAS_JOURNAL: u32 = 0x0004;
const COMPAT_EXT_ATTR: u32 = 0x0008;
// s_feature_incompat
const INCOMPAT_FILETYPE: u32 = 0x0002;
const INCOMPAT_RECOVER: u32 = 0x0004;
const INCOMPAT_META_BG: u32 = 0x0010;
const INCOMPAT_EXTENTS: u32 = 0x0040;
const INCOMPAT_64BIT: u32 = 0x0080;
// s_feature_ro_compat
const RO_COMPAT_SPARSE_SUPER: u32 = 0x0001;
const RO_COMPAT_LARGE_FILE: u32 = 0x0002;
const RO_COMPAT_EXTRA_ISIZE: u32 = 0x0040;

/// Group descriptor flag: the block bitmap on disk is not initialised.
const BG_BLOCK_UNINIT: u16 = 0x0002;

/// Extent tree magic (`eh_magic`).
const EXTENT_MAGIC: u16 = 0xF30A;
/// `EXT4_EXTENTS_FL`.
const INODE_FL_EXTENTS: u32 = 0x0008_0000;

/// JBD2 journal superblock magic (big-endian on disk).
const JBD2_MAGIC: u32 = 0xC03B_3998;
/// `JBD2_SUPERBLOCK_V2`.
const JBD2_SUPERBLOCK_V2: u32 = 4;

fn le16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn put16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn be32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_be_bytes());
}

fn unix_now() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as u32
}

/// Groups 0, 1 and the powers of 3, 5 and 7 carry a backup superblock.
pub fn has_backup_super(group: u32) -> bool {
    if group <= 1 {
        return true;
    }
    for base in [3u32, 5, 7] {
        let mut p = base;
        while p < group {
            p = match p.checked_mul(base) {
                Some(v) => v,
                None => break,
            };
        }
        if p == group {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// The parts of the superblock this engine reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ext4Layout {
    pub block_size: u64,
    pub blocks_count: u64,
    pub first_data_block: u64,
    pub blocks_per_group: u64,
    pub inodes_count: u32,
    pub inodes_per_group: u32,
    pub desc_size: u64,
    pub sixty_four_bit: bool,
    /// First block of the group descriptor table.
    pub gdt_block: u64,
    pub group_count: u64,
    /// Raw `s_state`.
    pub state: u16,
    pub feature_compat: u32,
    pub feature_incompat: u32,
    pub feature_ro_compat: u32,
    pub uuid: Uuid,
    pub label: String,
    /// Unmounted cleanly and no journal replay pending.
    pub clean: bool,
}

impl Ext4Layout {
    pub fn size_bytes(&self) -> u64 {
        self.blocks_count * self.block_size
    }

    pub fn has_journal(&self) -> bool {
        self.feature_compat & COMPAT_HAS_JOURNAL != 0
    }

    pub fn needs_recovery(&self) -> bool {
        self.feature_incompat & INCOMPAT_RECOVER != 0
    }

    pub fn has_errors(&self) -> bool {
        self.state & STATE_ERROR_FS != 0
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

    let feature_compat = le32(sb, 0x5C);
    let feature_incompat = le32(sb, 0x60);
    let feature_ro_compat = le32(sb, 0x64);
    if feature_incompat & INCOMPAT_META_BG != 0 {
        anyhow::bail!("META_BG layout is not supported");
    }
    let sixty_four_bit = feature_incompat & INCOMPAT_64BIT != 0;

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
    let clean = (state & STATE_VALID_FS) != 0
        && (state & STATE_ERROR_FS) == 0
        && (state & STATE_ORPHAN_RECOVERING) == 0
        && (feature_incompat & INCOMPAT_RECOVER) == 0;

    let mut uuid_bytes = [0u8; 16];
    uuid_bytes.copy_from_slice(&sb[0x68..0x78]);
    let label_raw = &sb[0x78..0x88];
    let label_end = label_raw.iter().position(|&b| b == 0).unwrap_or(label_raw.len());
    let label = String::from_utf8_lossy(&label_raw[..label_end]).to_string();

    let group_count = (blocks_count - first_data_block).div_ceil(blocks_per_group);
    let gdt_block = first_data_block + 1;

    Ok(Ext4Layout {
        block_size,
        blocks_count,
        first_data_block,
        blocks_per_group,
        inodes_count: le32(sb, 0x00),
        inodes_per_group: le32(sb, 0x28),
        desc_size,
        sixty_four_bit,
        gdt_block,
        group_count,
        state,
        feature_compat,
        feature_incompat,
        feature_ro_compat,
        uuid: Uuid::from_bytes(uuid_bytes),
        label,
        clean,
    })
}

/// Why a filesystem must not be sealed as a template.
///
/// Sealing snapshots a filesystem that thousands of clones will descend from,
/// so the guard checks the flags a *consumer* acts on rather than assuming a
/// clean unmount implies a clean superblock. RouterOS cannot replay a journal:
/// a clone whose superblock says "recovery pending" mounts read-only there,
/// and the failure surfaces in a container long after the build reported
/// success (stormblock-registry#10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SealBlocker {
    /// `VALID_FS` clear — never cleanly unmounted.
    NotCleanlyUnmounted,
    /// `ERROR_FS` set — the kernel recorded an error against this filesystem.
    ErrorFlagSet,
    /// `RECOVER` set — a journal replay is pending.
    RecoveryPending,
    /// `ORPHAN_FS` set — orphan inode cleanup is pending.
    OrphanCleanupPending,
}

impl std::fmt::Display for SealBlocker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SealBlocker::NotCleanlyUnmounted => {
                write!(f, "filesystem was never cleanly unmounted (VALID_FS clear)")
            }
            SealBlocker::ErrorFlagSet => {
                write!(f, "filesystem has errors recorded (ERROR_FS set)")
            }
            SealBlocker::RecoveryPending => {
                write!(f, "journal replay is pending (RECOVER set) — a consumer that cannot replay mounts read-only")
            }
            SealBlocker::OrphanCleanupPending => {
                write!(f, "orphan inode cleanup is pending (ORPHAN_FS set)")
            }
        }
    }
}

/// Every reason this filesystem is not safe to seal. Empty means sealable.
pub fn seal_blockers(layout: &Ext4Layout) -> Vec<SealBlocker> {
    let mut out = Vec::new();
    if layout.state & STATE_VALID_FS == 0 {
        out.push(SealBlocker::NotCleanlyUnmounted);
    }
    if layout.state & STATE_ERROR_FS != 0 {
        out.push(SealBlocker::ErrorFlagSet);
    }
    if layout.feature_incompat & INCOMPAT_RECOVER != 0 {
        out.push(SealBlocker::RecoveryPending);
    }
    if layout.state & STATE_ORPHAN_RECOVERING != 0 {
        out.push(SealBlocker::OrphanCleanupPending);
    }
    out
}

/// `Ok(())` when this filesystem may be sealed as a template.
pub fn check_sealable(layout: &Ext4Layout) -> Result<(), String> {
    let blockers = seal_blockers(layout);
    if blockers.is_empty() {
        return Ok(());
    }
    Err(blockers.iter().map(|b| b.to_string()).collect::<Vec<_>>().join("; "))
}

/// Read `len` bytes at `offset` honouring the device's alignment requirement.
///
/// `BlockDevice::read` demands a block-aligned offset; the structures here are
/// not (the superblock lives at byte 1024). Read the aligned window that
/// contains the range and slice it out.
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

/// Read-modify-write `data` at an arbitrary `offset`, respecting alignment.
pub async fn write_at(dev: &Arc<dyn BlockDevice>, offset: u64, data: &[u8]) -> anyhow::Result<()> {
    let bs = dev.block_size().max(1) as u64;
    let start = (offset / bs) * bs;
    let end = (offset + data.len() as u64).div_ceil(bs) * bs;
    if end > dev.capacity_bytes() {
        anyhow::bail!(
            "write {offset}+{} runs past the end of the volume ({} bytes)",
            data.len(),
            dev.capacity_bytes()
        );
    }
    let mut buf = vec![0u8; (end - start) as usize];
    dev.read(start, &mut buf)
        .await
        .map_err(|e| anyhow::anyhow!("reading volume at {start}: {e}"))?;
    let skip = (offset - start) as usize;
    buf[skip..skip + data.len()].copy_from_slice(data);
    write_all(dev, start, &buf).await
}

/// Write every byte, looping over short writes (a thin volume returns at slot
/// boundaries).
pub async fn write_all(dev: &Arc<dyn BlockDevice>, offset: u64, data: &[u8]) -> anyhow::Result<()> {
    let mut pos = offset;
    let mut rest = data;
    while !rest.is_empty() {
        let n = dev
            .write(pos, rest)
            .await
            .map_err(|e| anyhow::anyhow!("writing volume at {pos}: {e}"))?;
        if n == 0 {
            anyhow::bail!("device accepted 0 bytes at offset {pos}");
        }
        pos += n as u64;
        rest = &rest[n..];
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Stamping
// ---------------------------------------------------------------------------

/// Rewrite the filesystem UUID in the primary superblock, and optionally in
/// every backup.
///
/// This is the piece that can only live in the engine: any layer above clones
/// *through* it, so a UUID stamped anywhere else leaves clones sharing
/// identity (stormblockmk#12). It is one 16-byte patch because the format is
/// deliberately `metadata_csum`-free — otherwise every group checksum would
/// have to be recomputed.
///
/// `backups` costs one copy-on-write extent per backup group, so the default
/// for a clone is primary-only: mount, `blkid` and `mount -U` all read the
/// primary. Backups keep the template's UUID until something rewrites them,
/// which only matters if a filesystem is later rebuilt from a backup
/// superblock.
pub async fn stamp_uuid(
    dev: &Arc<dyn BlockDevice>,
    uuid: Uuid,
    backups: bool,
) -> anyhow::Result<Ext4Layout> {
    let sb = read_at(dev, SUPERBLOCK_OFFSET, SUPERBLOCK_LEN).await?;
    let layout = parse_superblock(&sb)?;

    write_at(dev, SUPERBLOCK_OFFSET + 0x68, uuid.as_bytes()).await?;

    if backups {
        for g in 1..layout.group_count as u32 {
            if !has_backup_super(g) {
                continue;
            }
            let group_start = layout.first_data_block + g as u64 * layout.blocks_per_group;
            let off = group_start * layout.block_size + 0x68;
            if off + 16 > dev.capacity_bytes() {
                break;
            }
            write_at(dev, off, uuid.as_bytes()).await?;
        }
    }

    dev.flush().await.map_err(|e| anyhow::anyhow!("flushing after stamp: {e}"))?;

    Ok(Ext4Layout { uuid, ..layout })
}

/// Rewrite the filesystem label in the primary superblock (16 bytes, NUL-padded).
pub async fn stamp_label(dev: &Arc<dyn BlockDevice>, label: &str) -> anyhow::Result<()> {
    let mut buf = [0u8; 16];
    let bytes = label.as_bytes();
    let n = bytes.len().min(16);
    buf[..n].copy_from_slice(&bytes[..n]);
    write_at(dev, SUPERBLOCK_OFFSET + 0x78, &buf).await?;
    dev.flush().await.map_err(|e| anyhow::anyhow!("flushing after label stamp: {e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Writing (mkfs)
// ---------------------------------------------------------------------------

/// How to lay down a blank ext4.
#[derive(Debug, Clone)]
pub struct Ext4Params {
    /// Volume label (16 bytes on disk, truncated).
    pub label: String,
    /// Filesystem UUID. Templates get one here; clones get a fresh one at
    /// clone time via [`stamp_uuid`].
    pub uuid: Uuid,
    /// Lay down a journal. **Per-template, never a build-time default**:
    /// RouterOS cannot replay one, so a journal that ever goes dirty there
    /// leaves the filesystem read-only permanently; a Linux host or VM wants
    /// the crash consistency. Both variants have to be able to coexist.
    pub journal: bool,
    /// Journal size in filesystem blocks. `None` uses the e2fsprogs default
    /// for the filesystem size.
    pub journal_blocks: Option<u32>,
    /// One inode per this many bytes (mkfs.ext4's `-i`, default 16 KiB).
    pub bytes_per_inode: u64,
    /// Percentage of blocks reserved for root.
    pub reserved_percent: u8,
    /// Trust that the target reads back as zeros, and skip writing blocks that
    /// are all zeros (whole inode tables, the journal body).
    ///
    /// True is correct for a freshly created thin volume and is the whole
    /// reason a template costs kilobytes instead of tens of megabytes of
    /// allocation. Set false when formatting over something that may hold old
    /// data.
    pub assume_blank: bool,
}

impl Default for Ext4Params {
    fn default() -> Self {
        Ext4Params {
            label: String::new(),
            uuid: Uuid::new_v4(),
            journal: false,
            journal_blocks: None,
            bytes_per_inode: 16384,
            reserved_percent: 5,
            assume_blank: true,
        }
    }
}

/// What a format produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ext4Report {
    pub block_size: u32,
    pub blocks: u64,
    pub inodes: u32,
    pub groups: u32,
    pub journal_blocks: u32,
    pub free_blocks: u64,
    pub uuid: Uuid,
    pub label: String,
    /// Bytes actually written to the device — what a thin volume allocates.
    pub bytes_written: u64,
}

/// e2fsprogs' default journal size for a filesystem of `blocks` blocks.
/// `None` means "too small for a journal".
pub fn default_journal_blocks(blocks: u64) -> Option<u32> {
    match blocks {
        b if b < 2048 => None,
        b if b < 32768 => Some(1024),
        b if b < 256 * 1024 => Some(4096),
        b if b < 512 * 1024 => Some(8192),
        b if b < 1024 * 1024 => Some(16384),
        _ => Some(32768),
    }
}

/// One block group's plan.
struct GroupPlan {
    start: u64,
    blocks: u64,
    has_super: bool,
    block_bitmap: u64,
    inode_bitmap: u64,
    inode_table: u64,
    /// End of the fixed metadata prefix — the first usable data block.
    data_start: u64,
    /// Next block a data allocation would take.
    cursor: u64,
    free: u64,
    /// Data runs allocated out of this group, absolute block numbers.
    used_runs: Vec<(u64, u64)>,
}

impl GroupPlan {
    fn alloc(&mut self, n: u64) -> Option<u64> {
        if n == 0 || self.free < n {
            return None;
        }
        let start = self.cursor;
        self.cursor += n;
        self.free -= n;
        self.used_runs.push((start, n));
        Some(start)
    }
}

/// Write a blank ext4 filesystem over the whole device.
pub async fn format(dev: &Arc<dyn BlockDevice>, params: &Ext4Params) -> anyhow::Result<Ext4Report> {
    let bs = BLOCK_SIZE as u64;
    if dev.block_size() as u64 > bs || bs % dev.block_size().max(1) as u64 != 0 {
        anyhow::bail!(
            "device block size {} is not a divisor of the ext4 block size {bs}",
            dev.block_size()
        );
    }
    let total_bytes = dev.capacity_bytes();
    let total_blocks = total_bytes / bs;
    // The smallest filesystem the kernel and RouterOS both accept.
    anyhow::ensure!(
        total_blocks >= 1024,
        "volume too small for ext4: {total_blocks} blocks ({total_bytes} bytes), minimum 1024 blocks (4 MiB)"
    );

    let first_data_block: u64 = 0; // 4 KiB blocks
    let blocks_per_group: u64 = bs * 8; // one bitmap block's worth
    let group_count = (total_blocks - first_data_block).div_ceil(blocks_per_group);
    anyhow::ensure!(group_count <= u32::MAX as u64, "filesystem too large");
    let gdt_blocks = (group_count * 32).div_ceil(bs);

    // Inodes: one per `bytes_per_inode`, rounded so the table fills whole
    // blocks, and never below a floor that keeps a small volume usable.
    let inodes_per_block = bs / INODE_SIZE as u64;
    let wanted = (total_bytes / params.bytes_per_inode.max(1024)).max(1);
    let per_group = (wanted.div_ceil(group_count)).max(inodes_per_block).max(256);
    let inodes_per_group = (per_group.div_ceil(inodes_per_block) * inodes_per_block)
        .min(bs * 8) // one inode bitmap block
        .max(inodes_per_block);
    let inode_table_blocks = inodes_per_group * INODE_SIZE as u64 / bs;
    let total_inodes = inodes_per_group * group_count;
    anyhow::ensure!(total_inodes <= u32::MAX as u64, "too many inodes");

    // --- Per-group layout -------------------------------------------------
    let mut groups: Vec<GroupPlan> = Vec::with_capacity(group_count as usize);
    for g in 0..group_count {
        let start = first_data_block + g * blocks_per_group;
        let blocks = blocks_per_group.min(total_blocks - start);
        let has_super = has_backup_super(g as u32);
        let meta_prefix = if has_super { 1 + gdt_blocks } else { 0 };
        let block_bitmap = start + meta_prefix;
        let inode_bitmap = block_bitmap + 1;
        let inode_table = inode_bitmap + 1;
        let data_start = inode_table + inode_table_blocks;
        anyhow::ensure!(
            data_start <= start + blocks,
            "group {g} metadata ({} blocks) does not fit in {blocks} blocks — volume too small \
             for {inodes_per_group} inodes per group",
            data_start - start
        );
        groups.push(GroupPlan {
            start,
            blocks,
            has_super,
            block_bitmap,
            inode_bitmap,
            inode_table,
            data_start,
            cursor: data_start,
            free: start + blocks - data_start,
            used_runs: Vec::new(),
        });
    }

    // --- Fixed allocations ------------------------------------------------
    let root_block = groups[0]
        .alloc(1)
        .ok_or_else(|| anyhow::anyhow!("no room for the root directory"))?;
    let lost_found_block = groups[0]
        .alloc(1)
        .ok_or_else(|| anyhow::anyhow!("no room for lost+found"))?;

    // Journal: contiguous, in the first group with room for it. A journal
    // bigger than any group's free space is shrunk rather than refused —
    // the point is crash consistency, not a particular size.
    let mut journal_start = 0u64;
    let mut journal_blocks = 0u64;
    if params.journal {
        let want = params
            .journal_blocks
            .map(|b| b as u64)
            .or_else(|| default_journal_blocks(total_blocks).map(|b| b as u64))
            .ok_or_else(|| {
                anyhow::anyhow!("filesystem is too small for a journal ({total_blocks} blocks)")
            })?;
        let largest = groups.iter().map(|g| g.free).max().unwrap_or(0);
        // Leave at least a quarter of the biggest group for data.
        let cap = (largest / 4 * 3).max(0);
        let size = want.min(cap);
        anyhow::ensure!(
            size >= 1024,
            "no group has room for a journal ({size} blocks free of {want} wanted)"
        );
        let g = groups
            .iter_mut()
            .find(|g| g.free >= size)
            .ok_or_else(|| anyhow::anyhow!("no group has {size} contiguous free blocks"))?;
        journal_start = g.alloc(size).expect("checked free above");
        journal_blocks = size;
    }

    let free_blocks: u64 = groups.iter().map(|g| g.free).sum();
    let reserved = total_blocks / 100 * params.reserved_percent.min(50) as u64;

    // --- Inodes -----------------------------------------------------------
    // Root, lost+found and (optionally) the journal all live in group 0's
    // first inode-table block: 256-byte inodes give 16 per 4 KiB block, and
    // the highest inode used here is 11.
    let mut inode_block = vec![0u8; bs as usize];
    write_dir_inode(&mut inode_block, ROOT_INODE, 0o40755, root_block, 3);
    write_dir_inode(&mut inode_block, LOST_FOUND_INODE, 0o40700, lost_found_block, 2);
    let mut jnl_backup = [0u32; 17];
    if journal_blocks > 0 {
        write_journal_inode(
            &mut inode_block,
            journal_start,
            journal_blocks,
            &mut jnl_backup,
        );
    }

    // --- Superblock + GDT -------------------------------------------------
    let sb = build_superblock(
        &SuperblockInput {
            total_blocks,
            total_inodes: total_inodes as u32,
            free_blocks,
            free_inodes: total_inodes as u32 - RESERVED_INODES,
            reserved,
            first_data_block,
            blocks_per_group,
            inodes_per_group: inodes_per_group as u32,
            group_count: group_count as u32,
            journal: journal_blocks > 0,
            jnl_backup,
            journal_size_bytes: journal_blocks * bs,
            params,
        },
        0,
    );
    let gdt = build_gdt(bs, &groups, inodes_per_group as u32, gdt_blocks);

    let mut written = 0u64;
    // Block 0 carries 1024 bytes of boot area then the primary superblock.
    let mut block0 = vec![0u8; bs as usize];
    block0[SUPERBLOCK_OFFSET as usize..SUPERBLOCK_OFFSET as usize + SUPERBLOCK_LEN]
        .copy_from_slice(&sb);
    write_all(dev, 0, &block0).await?;
    written += bs;
    for (i, chunk) in gdt.chunks(bs as usize).enumerate() {
        write_all(dev, (1 + i as u64) * bs, chunk).await?;
        written += bs;
    }

    // --- Per-group metadata ----------------------------------------------
    for (g, plan) in groups.iter().enumerate() {
        if g > 0 && plan.has_super {
            // A backup superblock sits at the *start* of the group's first
            // block (1024 bytes), not at byte 1024 of it.
            let backup = build_superblock(
                &SuperblockInput {
                    total_blocks,
                    total_inodes: total_inodes as u32,
                    free_blocks,
                    free_inodes: total_inodes as u32 - RESERVED_INODES,
                    reserved,
                    first_data_block,
                    blocks_per_group,
                    inodes_per_group: inodes_per_group as u32,
                    group_count: group_count as u32,
                    journal: journal_blocks > 0,
                    jnl_backup,
                    journal_size_bytes: journal_blocks * bs,
                    params,
                },
                g as u16,
            );
            let mut blk = vec![0u8; bs as usize];
            blk[..SUPERBLOCK_LEN].copy_from_slice(&backup);
            write_all(dev, plan.start * bs, &blk).await?;
            written += bs;
            for (i, chunk) in gdt.chunks(bs as usize).enumerate() {
                write_all(dev, (plan.start + 1 + i as u64) * bs, chunk).await?;
                written += bs;
            }
        }

        // Block bitmap: metadata prefix, then this group's data allocations,
        // then padding past the end of the group (e2fsck checks the padding).
        let mut bmp = vec![0u8; bs as usize];
        set_bits(&mut bmp, 0, plan.data_start - plan.start);
        for (start, len) in &plan.used_runs {
            set_bits(&mut bmp, start - plan.start, *len);
        }
        if plan.blocks < blocks_per_group {
            set_bits(&mut bmp, plan.blocks, blocks_per_group - plan.blocks);
        }
        write_all(dev, plan.block_bitmap * bs, &bmp).await?;
        written += bs;

        // Inode bitmap: reserved inodes in group 0, plus padding past
        // inodes_per_group.
        let mut ibmp = vec![0u8; bs as usize];
        if g == 0 {
            set_bits(&mut ibmp, 0, RESERVED_INODES as u64);
        }
        set_bits(&mut ibmp, inodes_per_group, bs * 8 - inodes_per_group);
        write_all(dev, plan.inode_bitmap * bs, &ibmp).await?;
        written += bs;

        // Inode table. Only group 0's first block has content; the rest is
        // zeros, which a blank thin volume already reads back as.
        for i in 0..inode_table_blocks {
            let data: &[u8] = if g == 0 && i == 0 { &inode_block } else { &[] };
            if data.is_empty() {
                if params.assume_blank {
                    continue;
                }
                let zeros = vec![0u8; bs as usize];
                write_all(dev, (plan.inode_table + i) * bs, &zeros).await?;
            } else {
                write_all(dev, (plan.inode_table + i) * bs, data).await?;
            }
            written += bs;
        }
    }

    // --- Directory contents ----------------------------------------------
    write_all(dev, root_block * bs, &build_root_dir(bs)).await?;
    written += bs;
    write_all(dev, lost_found_block * bs, &build_lost_found_dir(bs)).await?;
    written += bs;

    // --- Journal ----------------------------------------------------------
    if journal_blocks > 0 {
        let jsb = build_journal_superblock(bs as u32, journal_blocks as u32, params.uuid);
        write_all(dev, journal_start * bs, &jsb).await?;
        written += bs;
        if !params.assume_blank {
            let zeros = vec![0u8; bs as usize];
            for i in 1..journal_blocks {
                write_all(dev, (journal_start + i) * bs, &zeros).await?;
                written += bs;
            }
        }
    }

    dev.flush().await.map_err(|e| anyhow::anyhow!("flushing after format: {e}"))?;

    tracing::info!(
        blocks = total_blocks,
        inodes = total_inodes,
        groups = group_count,
        journal = journal_blocks,
        label = %params.label,
        "ext4 formatted"
    );

    Ok(Ext4Report {
        block_size: BLOCK_SIZE,
        blocks: total_blocks,
        inodes: total_inodes as u32,
        groups: group_count as u32,
        journal_blocks: journal_blocks as u32,
        free_blocks,
        uuid: params.uuid,
        label: params.label.clone(),
        bytes_written: written,
    })
}

/// Set `count` bits starting at `first` (LSB-first within each byte).
fn set_bits(bitmap: &mut [u8], first: u64, count: u64) {
    for i in first..first + count {
        let byte = (i / 8) as usize;
        if byte >= bitmap.len() {
            break;
        }
        bitmap[byte] |= 1 << (i % 8);
    }
}

struct SuperblockInput<'a> {
    total_blocks: u64,
    total_inodes: u32,
    free_blocks: u64,
    free_inodes: u32,
    reserved: u64,
    first_data_block: u64,
    blocks_per_group: u64,
    inodes_per_group: u32,
    group_count: u32,
    journal: bool,
    jnl_backup: [u32; 17],
    journal_size_bytes: u64,
    params: &'a Ext4Params,
}

fn build_superblock(i: &SuperblockInput<'_>, block_group_nr: u16) -> Vec<u8> {
    let mut sb = vec![0u8; SUPERBLOCK_LEN];
    let now = unix_now();
    // log2(4096/1024) = 2
    let log_block_size = 2u32;

    put32(&mut sb, 0x00, i.total_inodes);
    put32(&mut sb, 0x04, i.total_blocks as u32);
    put32(&mut sb, 0x08, i.reserved as u32);
    put32(&mut sb, 0x0C, i.free_blocks as u32);
    put32(&mut sb, 0x10, i.free_inodes);
    put32(&mut sb, 0x14, i.first_data_block as u32);
    put32(&mut sb, 0x18, log_block_size);
    put32(&mut sb, 0x1C, log_block_size); // log cluster size (no bigalloc)
    put32(&mut sb, 0x20, i.blocks_per_group as u32);
    put32(&mut sb, 0x24, i.blocks_per_group as u32); // clusters per group
    put32(&mut sb, 0x28, i.inodes_per_group);
    put32(&mut sb, 0x2C, 0); // s_mtime — never mounted
    put32(&mut sb, 0x30, now); // s_wtime
    put16(&mut sb, 0x34, 0); // mount count
    put16(&mut sb, 0x36, u16::MAX); // max mount count (-1: no time-based fsck)
    put16(&mut sb, 0x38, EXT4_MAGIC);
    put16(&mut sb, 0x3A, STATE_VALID_FS);
    put16(&mut sb, 0x3C, 1); // errors: continue
    put16(&mut sb, 0x3E, 0); // minor rev
    put32(&mut sb, 0x40, now); // last check
    put32(&mut sb, 0x44, 0); // check interval: none
    put32(&mut sb, 0x48, 0); // creator OS: Linux
    put32(&mut sb, 0x4C, 1); // rev level: dynamic
    put16(&mut sb, 0x50, 0); // default reserved uid
    put16(&mut sb, 0x52, 0); // default reserved gid
    put32(&mut sb, 0x54, FIRST_INODE);
    put16(&mut sb, 0x58, INODE_SIZE);
    put16(&mut sb, 0x5A, block_group_nr);

    let compat = COMPAT_EXT_ATTR | if i.journal { COMPAT_HAS_JOURNAL } else { 0 };
    put32(&mut sb, 0x5C, compat);
    put32(&mut sb, 0x60, INCOMPAT_FILETYPE | INCOMPAT_EXTENTS);
    put32(
        &mut sb,
        0x64,
        RO_COMPAT_SPARSE_SUPER | RO_COMPAT_LARGE_FILE | RO_COMPAT_EXTRA_ISIZE,
    );

    sb[0x68..0x78].copy_from_slice(i.params.uuid.as_bytes());
    let label = i.params.label.as_bytes();
    let n = label.len().min(16);
    sb[0x78..0x78 + n].copy_from_slice(&label[..n]);

    put16(&mut sb, 0xCE, 0); // reserved GDT blocks — no online resize
    if i.journal {
        put32(&mut sb, 0xE0, JOURNAL_INODE);
        sb[0xFD] = 1; // s_jnl_backup_type = JNL_BACKUP_BLOCKS
        for (k, v) in i.jnl_backup.iter().enumerate() {
            put32(&mut sb, 0x10C + k * 4, *v);
        }
        // s_jnl_blocks[16] is the journal inode's i_size.
        put32(&mut sb, 0x10C + 16 * 4, i.journal_size_bytes as u32);
    }
    put16(&mut sb, 0xFE, 32); // s_desc_size — 32-byte descriptors, no 64bit
    put32(&mut sb, 0x108, now); // s_mkfs_time
    put32(&mut sb, 0x150, (i.total_blocks >> 32) as u32);
    put32(&mut sb, 0x154, (i.reserved >> 32) as u32);
    put32(&mut sb, 0x158, (i.free_blocks >> 32) as u32);
    put16(&mut sb, 0x15C, 32); // s_min_extra_isize
    put16(&mut sb, 0x15E, 32); // s_want_extra_isize

    debug_assert!(i.group_count > 0);
    sb
}

fn build_gdt(bs: u64, groups: &[GroupPlan], inodes_per_group: u32, gdt_blocks: u64) -> Vec<u8> {
    let mut gdt = vec![0u8; (gdt_blocks * bs) as usize];
    for (g, plan) in groups.iter().enumerate() {
        let off = g * 32;
        put32(&mut gdt, off, plan.block_bitmap as u32);
        put32(&mut gdt, off + 4, plan.inode_bitmap as u32);
        put32(&mut gdt, off + 8, plan.inode_table as u32);
        put16(&mut gdt, off + 12, plan.free as u16);
        let free_inodes = if g == 0 {
            inodes_per_group - RESERVED_INODES
        } else {
            inodes_per_group
        };
        put16(&mut gdt, off + 14, free_inodes as u16);
        // Directories: root and lost+found, both in group 0.
        put16(&mut gdt, off + 16, if g == 0 { 2 } else { 0 });
        put16(&mut gdt, off + 18, 0); // flags — every bitmap is initialised
    }
    gdt
}

/// Write a single-block directory inode into an inode-table block.
fn write_dir_inode(block: &mut [u8], ino: u32, mode: u16, data_block: u64, links: u16) {
    let off = (ino as usize - 1) * INODE_SIZE as usize;
    let bs = BLOCK_SIZE;
    let now = unix_now();

    put16(block, off, mode);
    put32(block, off + 4, bs); // i_size_lo
    put32(block, off + 8, now); // atime
    put32(block, off + 12, now); // ctime
    put32(block, off + 16, now); // mtime
    put16(block, off + 26, links);
    put32(block, off + 28, (bs / 512) as u32); // i_blocks (512-byte units)
    put32(block, off + 32, INODE_FL_EXTENTS);
    write_inline_extent(block, off, 0, data_block, 1);
    put16(block, off + 128, 32); // i_extra_isize
}

/// The journal is a regular file (inode 8) holding one contiguous extent.
fn write_journal_inode(block: &mut [u8], start: u64, blocks: u64, backup: &mut [u32; 17]) {
    let off = (JOURNAL_INODE as usize - 1) * INODE_SIZE as usize;
    let bs = BLOCK_SIZE as u64;
    let now = unix_now();
    let size = blocks * bs;

    put16(block, off, 0o100600); // regular file, 0600
    put32(block, off + 4, size as u32);
    put32(block, off + 8, now);
    put32(block, off + 12, now);
    put32(block, off + 16, now);
    put16(block, off + 26, 1); // links
    put32(block, off + 28, (size / 512) as u32);
    put32(block, off + 32, INODE_FL_EXTENTS);
    write_inline_extent(block, off, 0, start, blocks as u16);
    put16(block, off + 128, 32);
    put32(block, off + 108, (size >> 32) as u32); // i_size_high

    // s_jnl_blocks backs up i_block[] so a lost journal inode can be rebuilt.
    for (k, slot) in backup.iter_mut().enumerate().take(15) {
        *slot = le32(block, off + 40 + k * 4);
    }
    backup[15] = (size >> 32) as u32;
    backup[16] = size as u32;
}

/// One extent covering `len` blocks from `phys`, inline in the inode body.
fn write_inline_extent(block: &mut [u8], inode_off: usize, logical: u32, phys: u64, len: u16) {
    let ib = inode_off + 40; // i_block[]
    put16(block, ib, EXTENT_MAGIC);
    put16(block, ib + 2, 1); // entries
    put16(block, ib + 4, 4); // max entries inline
    put16(block, ib + 6, 0); // depth
    put32(block, ib + 8, 0); // generation
    put32(block, ib + 12, logical);
    put16(block, ib + 16, len);
    put16(block, ib + 18, (phys >> 32) as u16);
    put32(block, ib + 20, phys as u32);
}

fn dir_entry(buf: &mut [u8], off: usize, ino: u32, rec_len: u16, name: &[u8], file_type: u8) {
    put32(buf, off, ino);
    put16(buf, off + 4, rec_len);
    buf[off + 6] = name.len() as u8;
    buf[off + 7] = file_type;
    buf[off + 8..off + 8 + name.len()].copy_from_slice(name);
}

fn build_root_dir(bs: u64) -> Vec<u8> {
    let mut dir = vec![0u8; bs as usize];
    dir_entry(&mut dir, 0, ROOT_INODE, 12, b".", 2);
    dir_entry(&mut dir, 12, ROOT_INODE, 12, b"..", 2);
    dir_entry(
        &mut dir,
        24,
        LOST_FOUND_INODE,
        (bs - 24) as u16,
        b"lost+found",
        2,
    );
    dir
}

fn build_lost_found_dir(bs: u64) -> Vec<u8> {
    let mut dir = vec![0u8; bs as usize];
    dir_entry(&mut dir, 0, LOST_FOUND_INODE, 12, b".", 2);
    dir_entry(&mut dir, 12, ROOT_INODE, (bs - 12) as u16, b"..", 2);
    dir
}

/// The JBD2 superblock, block 0 of the journal. All fields big-endian.
fn build_journal_superblock(bs: u32, blocks: u32, fs_uuid: Uuid) -> Vec<u8> {
    let mut jsb = vec![0u8; bs as usize];
    be32(&mut jsb, 0x00, JBD2_MAGIC);
    be32(&mut jsb, 0x04, JBD2_SUPERBLOCK_V2);
    be32(&mut jsb, 0x08, 0); // h_sequence
    be32(&mut jsb, 0x0C, bs); // s_blocksize
    be32(&mut jsb, 0x10, blocks); // s_maxlen
    be32(&mut jsb, 0x14, 1); // s_first — block 0 is this superblock
    be32(&mut jsb, 0x18, 1); // s_sequence
    be32(&mut jsb, 0x1C, 0); // s_start — empty journal, nothing to replay
    be32(&mut jsb, 0x20, 0); // s_errno
    jsb[0x30..0x40].copy_from_slice(fs_uuid.as_bytes()); // s_uuid
    be32(&mut jsb, 0x40, 1); // s_nr_users
    jsb
}

/// Read the whole filesystem's free block map, as byte ranges relative to the
/// start of the volume.
///
/// Used to discard what a filesystem has freed but never handed back — an
/// offline `fstrim` driven from the target side, for initiators that do not
/// issue UNMAP. Everything here is read-only and deliberately paranoid: a
/// wrong answer discards live data, so any field it does not fully understand
/// aborts the scan instead of guessing.
pub async fn scan_free(dev: &Arc<dyn BlockDevice>, slot_size: u64) -> anyhow::Result<FreeMap> {
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

    for g in 0..layout.group_count {
        let desc_off = layout.gdt_block * layout.block_size + g * layout.desc_size;
        let desc = read_at(dev, desc_off, layout.desc_size as usize).await?;
        if desc.len() >= 20 && (le16(&desc, 0x12) & BG_BLOCK_UNINIT) != 0 {
            map.groups_skipped += 1;
            continue;
        }
        let lo = le32(&desc, 0x00) as u64;
        let hi = if layout.sixty_four_bit && desc.len() >= 36 { le32(&desc, 0x20) as u64 } else { 0 };
        let bmp_block = (hi << 32) | lo;
        if bmp_block == 0 || bmp_block >= layout.blocks_count {
            anyhow::bail!(
                "group {g} bitmap block {bmp_block} out of range (fs has {} blocks)",
                layout.blocks_count
            );
        }
        let bitmap = read_at(dev, bmp_block * layout.block_size, layout.block_size as usize).await?;

        let group_first_block = layout.first_data_block + g * layout.blocks_per_group;
        let blocks_in_group = layout
            .blocks_per_group
            .min(layout.blocks_count.saturating_sub(group_first_block));
        if blocks_in_group == 0 {
            continue;
        }
        for (start, len) in free_runs(&bitmap, group_first_block, blocks_in_group, layout.block_size)
        {
            map.free_bytes += len;
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

/// What an offline free-space scan found.
#[derive(Debug, Clone, Default)]
pub struct FreeMap {
    /// Contiguous free byte ranges, volume-relative.
    pub runs: Vec<(u64, u64)>,
    /// Total bytes the filesystem considers free.
    pub free_bytes: u64,
    /// Of those, the bytes that fall in whole slot-aligned slots — the only
    /// ones a discard can actually give back.
    pub reclaimable_bytes: u64,
    pub groups_skipped: u64,
    pub groups_scanned: u64,
    pub layout: Option<Ext4Layout>,
}

/// Free byte ranges from one group's block bitmap. Bit `i` covers block
/// `group_first_block + i`; a SET bit means in use. Bits past
/// `blocks_in_group` are padding and are ignored.
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

/// The whole-slot span inside a free run, or `None` when the run does not
/// contain one. Slot 0 is never offered: the superblock and the group
/// descriptor table live there.
pub fn aligned_span(start: u64, len: u64, slot_size: u64) -> Option<(u64, u64)> {
    if slot_size == 0 {
        return None;
    }
    let first = start.div_ceil(slot_size).max(1) * slot_size;
    let last = ((start + len) / slot_size) * slot_size;
    // `then` and not `then_some`: with no whole slot inside the run, `last` is
    // below `first` and the subtraction would underflow before the guard ran.
    (last > first).then(|| (first, last - first))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;

    async fn scratch(name: &str, size: u64) -> (Arc<dyn BlockDevice>, String) {
        let dir = std::env::temp_dir().join("stormblock-ext4-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}-{}.img", Uuid::new_v4().simple()));
        let p = path.to_str().unwrap().to_string();
        let dev = FileDevice::open_with_capacity(&p, size).await.unwrap();
        (Arc::new(dev), p)
    }

    #[test]
    fn backup_supers_land_on_sparse_groups() {
        for g in [0u32, 1, 3, 5, 7, 9, 25, 27, 49, 81, 125, 343] {
            assert!(has_backup_super(g), "group {g} should carry a backup");
        }
        for g in [2u32, 4, 6, 8, 10, 26, 100] {
            assert!(!has_backup_super(g), "group {g} should not carry a backup");
        }
    }

    #[test]
    fn journal_defaults_follow_e2fsprogs() {
        assert_eq!(default_journal_blocks(1000), None);
        assert_eq!(default_journal_blocks(4096), Some(1024));
        assert_eq!(default_journal_blocks(65536), Some(4096));
        assert_eq!(default_journal_blocks(300_000), Some(8192));
        assert_eq!(default_journal_blocks(2_000_000), Some(32768));
    }

    #[tokio::test]
    async fn format_produces_a_parsable_superblock() {
        let (dev, path) = scratch("plain", 64 * 1024 * 1024).await;
        let params = Ext4Params {
            label: "storm".to_string(),
            ..Default::default()
        };
        let report = format(&dev, &params).await.unwrap();
        assert_eq!(report.blocks, 16384);
        assert_eq!(report.groups, 1);
        assert_eq!(report.journal_blocks, 0);

        let sb = read_at(&dev, SUPERBLOCK_OFFSET, SUPERBLOCK_LEN).await.unwrap();
        let l = parse_superblock(&sb).unwrap();
        assert_eq!(l.block_size, 4096);
        assert_eq!(l.blocks_count, 16384);
        assert_eq!(l.label, "storm");
        assert_eq!(l.uuid, params.uuid);
        assert!(l.clean, "a fresh filesystem must be sealable");
        assert!(!l.has_journal());
        // Conservative feature set — none of the flags that would make a
        // UUID stamp a checksum problem, or that RouterOS refuses.
        assert_eq!(l.feature_incompat, INCOMPAT_FILETYPE | INCOMPAT_EXTENTS);
        assert_eq!(l.feature_ro_compat & 0x0400, 0, "metadata_csum must be off");
        assert!(check_sealable(&l).is_ok());

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn journalled_format_sets_the_journal_up() {
        let (dev, path) = scratch("journal", 256 * 1024 * 1024).await;
        let params = Ext4Params { journal: true, ..Default::default() };
        let report = format(&dev, &params).await.unwrap();
        assert_eq!(report.journal_blocks, 4096);

        let sb = read_at(&dev, SUPERBLOCK_OFFSET, SUPERBLOCK_LEN).await.unwrap();
        let l = parse_superblock(&sb).unwrap();
        assert!(l.has_journal());
        assert!(!l.needs_recovery(), "a fresh journal has nothing to replay");
        assert!(l.clean);
        assert_eq!(le32(&sb, 0xE0), JOURNAL_INODE);

        // The JBD2 superblock is where the journal inode says it is.
        let jnl_start = le32(&sb, 0x10C + 5 * 4) as u64; // i_block[5] = extent start_lo
        assert!(jnl_start > 0);
        let jsb = read_at(&dev, jnl_start * 4096, 64).await.unwrap();
        assert_eq!(u32::from_be_bytes(jsb[0..4].try_into().unwrap()), JBD2_MAGIC);
        assert_eq!(u32::from_be_bytes(jsb[4..8].try_into().unwrap()), JBD2_SUPERBLOCK_V2);
        assert_eq!(u32::from_be_bytes(jsb[16..20].try_into().unwrap()), 4096); // s_maxlen
        assert_eq!(&jsb[0x30..0x40], params.uuid.as_bytes());

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn multi_group_format_writes_backup_supers_at_group_start() {
        // 256 MiB = 2 groups; group 1 carries a backup.
        let (dev, path) = scratch("backup", 256 * 1024 * 1024).await;
        format(&dev, &Ext4Params::default()).await.unwrap();

        let backup = read_at(&dev, 32768 * 4096, SUPERBLOCK_LEN).await.unwrap();
        let l = parse_superblock(&backup).unwrap();
        assert_eq!(l.blocks_count, 65536);
        assert_eq!(le16(&backup, 0x5A), 1, "backup must name its own group");

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn blank_volume_format_writes_far_less_than_the_metadata_size() {
        // The point of assume_blank: a 1 GiB template must not cost tens of
        // megabytes of thin allocation just to zero inode tables.
        let (dev, path) = scratch("sparse", 1024 * 1024 * 1024).await;
        let report = format(&dev, &Ext4Params::default()).await.unwrap();
        let inode_table_bytes = report.inodes as u64 * 256;
        assert!(
            report.bytes_written < inode_table_bytes / 4,
            "wrote {} bytes, inode tables alone are {inode_table_bytes}",
            report.bytes_written
        );

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn stamp_uuid_rewrites_identity_without_touching_anything_else() {
        let (dev, path) = scratch("stamp", 256 * 1024 * 1024).await;
        let params = Ext4Params { label: "golden".to_string(), ..Default::default() };
        format(&dev, &params).await.unwrap();

        let fresh = Uuid::new_v4();
        let after = stamp_uuid(&dev, fresh, true).await.unwrap();
        assert_eq!(after.uuid, fresh);

        let sb = read_at(&dev, SUPERBLOCK_OFFSET, SUPERBLOCK_LEN).await.unwrap();
        let l = parse_superblock(&sb).unwrap();
        assert_eq!(l.uuid, fresh);
        assert_eq!(l.label, "golden", "label must survive a UUID stamp");
        assert!(l.clean);

        // Backups were asked for, so group 1 carries the new UUID too.
        let backup = read_at(&dev, 32768 * 4096, SUPERBLOCK_LEN).await.unwrap();
        assert_eq!(parse_superblock(&backup).unwrap().uuid, fresh);

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn stamp_uuid_defaults_to_primary_only() {
        let (dev, path) = scratch("stamp-primary", 256 * 1024 * 1024).await;
        let params = Ext4Params::default();
        format(&dev, &params).await.unwrap();

        let fresh = Uuid::new_v4();
        stamp_uuid(&dev, fresh, false).await.unwrap();

        let backup = read_at(&dev, 32768 * 4096, SUPERBLOCK_LEN).await.unwrap();
        assert_eq!(
            parse_superblock(&backup).unwrap().uuid,
            params.uuid,
            "backups are left alone unless asked for"
        );

        let _ = std::fs::remove_file(path);
    }

    /// stormblock-registry#10: a template whose superblock had ERROR_FS set
    /// and RECOVER pending sealed anyway, because the guard only checked
    /// VALID_FS. It mounted read-only on RouterOS days later.
    #[test]
    fn seal_guard_rejects_every_flag_a_consumer_acts_on() {
        let mut sb = vec![0u8; SUPERBLOCK_LEN];
        put32(&mut sb, 0x04, 65536);
        put32(&mut sb, 0x18, 2);
        put32(&mut sb, 0x20, 32768);
        put16(&mut sb, 0x38, EXT4_MAGIC);
        put16(&mut sb, 0x3A, STATE_VALID_FS);

        assert!(check_sealable(&parse_superblock(&sb).unwrap()).is_ok());

        // VALID_FS set *and* ERROR_FS set — the exact state that shipped.
        let mut bad = sb.clone();
        put16(&mut bad, 0x3A, STATE_VALID_FS | STATE_ERROR_FS);
        put32(&mut bad, 0x60, INCOMPAT_RECOVER);
        let l = parse_superblock(&bad).unwrap();
        assert!(!l.clean);
        let blockers = seal_blockers(&l);
        assert!(blockers.contains(&SealBlocker::ErrorFlagSet));
        assert!(blockers.contains(&SealBlocker::RecoveryPending));

        let mut dirty = sb.clone();
        put16(&mut dirty, 0x3A, 0);
        assert_eq!(
            seal_blockers(&parse_superblock(&dirty).unwrap()),
            vec![SealBlocker::NotCleanlyUnmounted]
        );

        let mut orphan = sb.clone();
        put16(&mut orphan, 0x3A, STATE_VALID_FS | STATE_ORPHAN_RECOVERING);
        assert_eq!(
            seal_blockers(&parse_superblock(&orphan).unwrap()),
            vec![SealBlocker::OrphanCleanupPending]
        );
    }

    #[test]
    fn parse_rejects_non_ext4_and_unsupported_layouts() {
        let mut sb = vec![0u8; SUPERBLOCK_LEN];
        put32(&mut sb, 0x04, 65536);
        put32(&mut sb, 0x18, 2);
        put32(&mut sb, 0x20, 32768);
        put16(&mut sb, 0x38, EXT4_MAGIC);
        assert!(parse_superblock(&sb).is_ok());

        let mut wrong = sb.clone();
        put16(&mut wrong, 0x38, 0);
        assert!(parse_superblock(&wrong).is_err());

        let mut meta_bg = sb.clone();
        put32(&mut meta_bg, 0x60, INCOMPAT_META_BG);
        assert!(parse_superblock(&meta_bg).is_err());
    }

    #[tokio::test]
    async fn scan_free_sees_a_fresh_filesystem_as_mostly_free() {
        let (dev, path) = scratch("scan", 256 * 1024 * 1024).await;
        let report = format(&dev, &Ext4Params::default()).await.unwrap();
        let map = scan_free(&dev, 1024 * 1024).await.unwrap();
        assert_eq!(map.groups_scanned, 2);
        assert_eq!(
            map.free_bytes,
            report.free_blocks * 4096,
            "bitmap free count must agree with the superblock"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn free_runs_are_lsb_first_and_bounded() {
        let bitmap = vec![0b0000_0011u8, 0x00];
        assert_eq!(free_runs(&bitmap, 0, 16, 4096), vec![(2 * 4096, 14 * 4096)]);
        assert_eq!(free_runs(&bitmap, 0, 4, 4096), vec![(2 * 4096, 2 * 4096)]);
        assert_eq!(free_runs(&[], 0, 8, 4096), vec![]);
    }

    #[test]
    fn aligned_span_never_offers_slot_zero() {
        let slot = 4 * 1024 * 1024;
        assert_eq!(aligned_span(0, 16 * slot, slot), Some((slot, 15 * slot)));
        assert_eq!(aligned_span(slot + 1, slot - 2, slot), None);
    }

    #[tokio::test]
    async fn too_small_is_refused_rather_than_half_formatted() {
        let (dev, path) = scratch("tiny", 1024 * 1024).await;
        assert!(format(&dev, &Ext4Params::default()).await.is_err());
        let _ = std::fs::remove_file(path);
    }
}
