//! FAT, enough of it to build an ESP — FAT16 or FAT32, chosen by size.
//!
//! Firmware needs FAT — that is the whole reason the ESP exists — so an image
//! builder that cannot produce one can only ever assemble half a disk. This is
//! a *writer*, not a filesystem: it formats an empty volume and lays a
//! directory tree into it once, allocating clusters sequentially because
//! nothing has been freed yet. It never deletes, never rewrites and never has
//! to deal with fragmentation.
//!
//! Long names are real VFAT LFN entries. Firmware paths like
//! `/EFI/BOOT/BOOTX64.EFI` fit 8.3, but `loader/entries/stormcos-6.12.0.conf`
//! does not, and a boot loader that cannot read its own config is not a floor
//! to build on.
//!
//! **Both widths exist for a reason.** FAT32 needs 65,525 clusters before it is
//! legally FAT32, which puts a floor of roughly 33 MiB on the volume. El Torito
//! counts a boot image in 512-byte sectors in a *16-bit* field, so an ESP that
//! an optical boot can describe in full has a ceiling of 32 MiB. The two do not
//! overlap: without FAT16 there is no ESP size that satisfies both, and every
//! ISO would ship a filesystem firmware could only half-see.
//!
//! There is a reader too ([`read_tree`]), for one job: an ESP served at one
//! sector size has to be laid onto a drive of another, and a FAT declares its
//! medium's sector size, so it is read out and written again rather than
//! copied (#123).
//!
//! Timestamps are fixed rather than taken from the clock, so building the same
//! tree twice produces the same bytes.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use crate::drive::BlockDevice;
use crate::pallet::PartitionView;

use super::{ImageError, Result};

/// FAT sector size. Not the device's block size: a filesystem an unknown
/// firmware has to read is written in the sector size everything assumes.
/// The logical sector size a FAT declares when nothing else says.
///
/// A FAT's sector size has to match the medium it is read from. Fixed at
/// 512, an ESP written onto a 4096-byte device declared 512-byte sectors and
/// no FAT driver — the kernel's or the firmware's — would mount it. The
/// partition was found and correctly typed, and unreadable (stormcos#31).
const DEFAULT_SECTOR: u32 = 512;
/// FAT32 keeps room for the FSInfo block and the backup boot sector.
const FAT32_RESERVED: u32 = 32;
const NUM_FATS: u32 = 2;
const FSINFO_SECTOR: u32 = 1;
const BACKUP_BOOT_SECTOR: u32 = 6;
/// FAT32 is only FAT32 above this many clusters; below it, the filesystem
/// would be a FAT16 that claims otherwise, and some firmware checks.
const MIN_CLUSTERS: u32 = 65525;
/// FAT16's own legal range, from the same table in the specification.
const FAT16_MIN_CLUSTERS: u32 = 4085;
const FAT16_MAX_CLUSTERS: u32 = 65524;
/// Root directory entries in a FAT16's fixed root area. 512 is the convention,
/// and 512 × 32 B is exactly 32 sectors.
const FAT16_ROOT_ENTRIES: u32 = 512;

/// Which width a volume ended up as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatKind {
    Fat16,
    Fat32,
}

impl FatKind {
    fn eoc(self) -> u32 {
        match self {
            FatKind::Fat16 => 0xFFFF,
            FatKind::Fat32 => 0x0FFF_FFFF,
        }
    }
}

const ATTR_READ_ONLY: u8 = 0x01;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_LFN: u8 = 0x0F;

/// 2026-01-01 00:00:00, so the same tree always builds the same bytes.
const FIXED_DATE: u16 = ((2026 - 1980) << 9) | (1 << 5) | 1;
const FIXED_TIME: u16 = 0;

/// Format an empty volume, FAT16 or FAT32 as its size requires.
pub async fn format(device: Arc<dyn BlockDevice>, label: &str) -> Result<()> {
    let mut fs = Fat32::new(device, label)?;
    fs.finish().await
}

/// Format, then lay a set of files into the root.
///
/// **This is what a cloud-init seed is.** NoCloud looks for a filesystem
/// labelled `cidata` holding `meta-data`, `user-data` and optionally
/// `network-config` — three small files at the root, no directories. Those
/// names do not fit 8.3, so they need the LFN entries this writer already
/// produces; and vfat is one of the two types every cloud-init build accepts,
/// where an ext4 volume with the same label is not universally picked up.
///
/// In memory rather than from a directory, because the seed is generated per
/// VM and staging it on the node's filesystem first would be a temporary file
/// on a path that may be read-only, for bytes that already exist.
pub async fn format_from_files(
    device: Arc<dyn BlockDevice>,
    files: &[(String, Vec<u8>)],
    label: &str,
) -> Result<()> {
    let mut fs = Fat32::new(device, label)?;
    fs.write_files(files).await?;
    fs.finish().await
}

/// Format, then lay `dir`'s contents into it.
pub async fn format_from_dir(
    device: Arc<dyn BlockDevice>,
    dir: &Path,
    label: &str,
) -> Result<()> {
    if !tokio::fs::try_exists(dir).await.unwrap_or(false) {
        return Err(ImageError::Spec(format!(
            "ESP source directory {} does not exist",
            dir.display()
        )));
    }
    let mut fs = Fat32::new(device, label)?;
    fs.write_tree(dir).await?;
    fs.finish().await
}

/// Format, then lay a tree of in-memory files into it.
///
/// `files` are paths from the root, `/`-separated (`EFI/BOOT/BOOTX64.EFI`);
/// `dirs` are directories to create even when nothing is in them. This is
/// how an ESP read off one medium is laid onto another whose sector size
/// differs — see [`read_tree`].
pub async fn format_from_tree(
    device: Arc<dyn BlockDevice>,
    files: &[(String, Vec<u8>)],
    dirs: &[String],
    label: &str,
) -> Result<()> {
    let mut root = MemDir::default();
    for d in dirs {
        root.dir_at(d)?;
    }
    for (path, data) in files {
        let (parent, name) = match path.trim_matches('/').rsplit_once('/') {
            Some((p, n)) => (p, n),
            None => ("", path.trim_matches('/')),
        };
        if name.is_empty() {
            return Err(ImageError::Spec(format!("'{path}' names no file")));
        }
        root.dir_at(parent)?.files.insert(name.to_string(), data.clone());
    }
    let mut fs = Fat32::new(device, label)?;
    let entries = fs.write_mem_children(&root, 0).await?;
    fs.write_root(entries).await?;
    fs.finish().await
}

/// What a FAT volume holds, read back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FatTree {
    /// The volume label from the boot sector, trimmed.
    pub label: String,
    /// Every file, by path from the root, `/`-separated, in directory order.
    pub files: Vec<(String, Vec<u8>)>,
    /// Every directory, by path from the root.
    pub dirs: Vec<String>,
}

/// Read every file and directory out of a FAT16 or FAT32 volume.
///
/// The other half of [`format_from_tree`], and deliberately no more than an
/// ESP needs: long names, subdirectories, both widths. An ESP is formatted in
/// its medium's sector size, so one served at 4096 bytes cannot be copied
/// byte for byte onto a 512-byte drive and still be read by firmware — it
/// has to be read out and laid down again (#123).
///
/// Everything read is checked against the volume's own geometry: a chain
/// that loops, leaves the volume, or ends before a file's size is an error,
/// never a short file.
pub async fn read_tree(device: Arc<dyn BlockDevice>) -> Result<FatTree> {
    let r = FatReader::open(device).await?;
    let mut tree = FatTree { label: r.label.clone(), ..Default::default() };
    let root = r.root_bytes().await?;
    r.walk(&root, "", 0, &mut tree).await?;
    Ok(tree)
}

#[derive(Default)]
struct MemDir {
    dirs: std::collections::BTreeMap<String, MemDir>,
    files: std::collections::BTreeMap<String, Vec<u8>>,
}

impl MemDir {
    fn dir_at(&mut self, path: &str) -> Result<&mut MemDir> {
        let mut at = self;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            if at.files.contains_key(part) {
                return Err(ImageError::Spec(format!("'{part}' is both a file and a directory")));
            }
            at = at.dirs.entry(part.to_string()).or_default();
        }
        Ok(at)
    }
}

struct FatReader {
    view: PartitionView,
    kind: FatKind,
    sector: u64,
    sectors_per_cluster: u64,
    root_dir_sector: u64,
    root_entries: u64,
    root_cluster: u32,
    data_start: u64,
    cluster_count: u32,
    fat: Vec<u8>,
    label: String,
}

impl FatReader {
    async fn open(device: Arc<dyn BlockDevice>) -> Result<FatReader> {
        let view = PartitionView::whole(device);
        let mut b = vec![0u8; 512];
        view.read_at(0, &mut b).await?;
        let bad = |why: &str| ImageError::Other(format!("not a FAT volume: {why}"));
        if b[510] != 0x55 || b[511] != 0xAA {
            return Err(bad("no boot signature"));
        }
        let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]) as u64;
        let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as u64;
        let sector = u16_at(11);
        if !matches!(sector, 512 | 1024 | 2048 | 4096) {
            return Err(bad(&format!("{sector}-byte sectors")));
        }
        let spc = b[13] as u64;
        if spc == 0 || !spc.is_power_of_two() {
            return Err(bad(&format!("{spc} sectors per cluster")));
        }
        let reserved = u16_at(14);
        let nfats = b[16] as u64;
        let root_entries = u16_at(17);
        let total = if u16_at(19) != 0 { u16_at(19) } else { u32_at(32) };
        let fat_sectors = if u16_at(22) != 0 { u16_at(22) } else { u32_at(36) };
        if reserved == 0 || nfats == 0 || fat_sectors == 0 {
            return Err(bad("empty geometry"));
        }
        let root_dir_sectors = (root_entries * 32).div_ceil(sector);
        let root_dir_sector = reserved + nfats * fat_sectors;
        let data_start = root_dir_sector + root_dir_sectors;
        if total <= data_start || total * sector > view.len() {
            return Err(bad("geometry runs past the volume"));
        }
        let cluster_count = ((total - data_start) / spc) as u32;
        let kind = if cluster_count < FAT16_MIN_CLUSTERS {
            return Err(bad("FAT12 is not an ESP this reads"));
        } else if cluster_count <= FAT16_MAX_CLUSTERS {
            FatKind::Fat16
        } else {
            FatKind::Fat32
        };
        let label_at = if kind == FatKind::Fat32 { 71 } else { 43 };
        let label = String::from_utf8_lossy(&b[label_at..label_at + 11]).trim_end().to_string();
        let root_cluster = if kind == FatKind::Fat32 { u32_at(44) as u32 } else { 0 };

        let mut fat = vec![0u8; (fat_sectors * sector) as usize];
        view.read_at(reserved * sector, &mut fat).await?;
        Ok(FatReader {
            view,
            kind,
            sector,
            sectors_per_cluster: spc,
            root_dir_sector,
            root_entries,
            root_cluster,
            data_start,
            cluster_count,
            fat,
            label,
        })
    }

    fn next(&self, c: u32) -> Option<u32> {
        let v = match self.kind {
            FatKind::Fat16 => {
                let o = c as usize * 2;
                let v = u16::from_le_bytes([*self.fat.get(o)?, *self.fat.get(o + 1)?]) as u32;
                if v >= 0xFFF8 {
                    return None;
                }
                v
            }
            FatKind::Fat32 => {
                let o = c as usize * 4;
                let v = u32::from_le_bytes(self.fat.get(o..o + 4)?.try_into().ok()?) & 0x0FFF_FFFF;
                if v >= 0x0FFF_FFF8 {
                    return None;
                }
                v
            }
        };
        Some(v)
    }

    /// The clusters of a chain, checked: every one inside the volume, and no
    /// more of them than the volume has — which is what makes a loop an error
    /// rather than a hang.
    fn chain(&self, first: u32) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        let mut c = first;
        loop {
            if c < 2 || c >= self.cluster_count + 2 || out.len() > self.cluster_count as usize {
                return Err(ImageError::Other(format!("FAT chain from cluster {first} is broken")));
            }
            out.push(c);
            match self.next(c) {
                Some(n) => c = n,
                None => return Ok(out),
            }
        }
    }

    async fn read_chain(&self, first: u32, len: Option<u64>) -> Result<Vec<u8>> {
        let csize = self.sectors_per_cluster * self.sector;
        let clusters = self.chain(first)?;
        let want = len.unwrap_or(clusters.len() as u64 * csize);
        if want > clusters.len() as u64 * csize {
            return Err(ImageError::Other(format!(
                "a file of {want} bytes has a chain of {} clusters",
                clusters.len()
            )));
        }
        let mut out = vec![0u8; (clusters.len() as u64 * csize) as usize];
        for (i, c) in clusters.iter().enumerate() {
            let at = (self.data_start + (*c as u64 - 2) * self.sectors_per_cluster) * self.sector;
            let i = i * csize as usize;
            self.view.read_at(at, &mut out[i..i + csize as usize]).await?;
        }
        out.truncate(want as usize);
        Ok(out)
    }

    async fn root_bytes(&self) -> Result<Vec<u8>> {
        match self.kind {
            FatKind::Fat16 => {
                let mut out = vec![0u8; (self.root_entries * 32) as usize];
                self.view.read_at(self.root_dir_sector * self.sector, &mut out).await?;
                Ok(out)
            }
            FatKind::Fat32 => self.read_chain(self.root_cluster, None).await,
        }
    }

    async fn walk(&self, dir: &[u8], prefix: &str, depth: u32, tree: &mut FatTree) -> Result<()> {
        if depth > 16 {
            return Err(ImageError::Other("directories nested past 16 levels".into()));
        }
        // Long-name pieces seen since the last short entry, by sequence.
        let mut lfn: Vec<(u8, u8, Vec<u16>)> = Vec::new();
        for e in dir.chunks_exact(32) {
            if e[0] == 0x00 {
                break;
            }
            if e[0] == 0xE5 {
                lfn.clear();
                continue;
            }
            let attr = e[11];
            if attr & 0x3F == ATTR_LFN {
                const SPOTS: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
                let units: Vec<u16> = SPOTS
                    .iter()
                    .map(|&o| u16::from_le_bytes([e[o], e[o + 1]]))
                    .take_while(|&u| u != 0x0000 && u != 0xFFFF)
                    .collect();
                lfn.push((e[0] & 0x1F, e[13], units));
                continue;
            }
            if attr & ATTR_VOLUME_ID != 0 {
                lfn.clear();
                continue;
            }
            let short: [u8; 11] = e[0..11].try_into().expect("11 bytes");
            let name = long_name(&lfn, &short).unwrap_or_else(|| short_display(&short, e[12]));
            lfn.clear();
            if name == "." || name == ".." {
                continue;
            }
            let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            let hi = if self.kind == FatKind::Fat32 {
                (u16::from_le_bytes([e[20], e[21]]) as u32) << 16
            } else {
                0
            };
            let first = hi | u16::from_le_bytes([e[26], e[27]]) as u32;
            let size = u32::from_le_bytes([e[28], e[29], e[30], e[31]]) as u64;
            if attr & ATTR_DIRECTORY != 0 {
                tree.dirs.push(path.clone());
                if first != 0 {
                    let bytes = self.read_chain(first, None).await?;
                    Box::pin(self.walk(&bytes, &path, depth + 1, tree)).await?;
                }
            } else {
                let data = if size == 0 { Vec::new() } else { self.read_chain(first, Some(size)).await? };
                tree.files.push((path, data));
            }
        }
        Ok(())
    }
}

/// The long name a run of LFN entries spells, if they belong to this short
/// entry — the checksum is what ties them to it, and a run whose checksum
/// does not match is a stale one some other writer left behind.
fn long_name(lfn: &[(u8, u8, Vec<u16>)], short: &[u8; 11]) -> Option<String> {
    if lfn.is_empty() {
        return None;
    }
    let sum = lfn_checksum(short);
    if lfn.iter().any(|(_, c, _)| *c != sum) {
        return None;
    }
    let mut parts: Vec<&(u8, u8, Vec<u16>)> = lfn.iter().collect();
    parts.sort_by_key(|(seq, ..)| *seq);
    let units: Vec<u16> = parts.iter().flat_map(|(_, _, u)| u.iter().copied()).collect();
    String::from_utf16(&units).ok()
}

/// A short name as a reader shows it, honouring the NT lowercase bits.
fn short_display(short: &[u8; 11], case: u8) -> String {
    let mut stem = String::from_utf8_lossy(&short[0..8]).trim_end().to_string();
    let mut ext = String::from_utf8_lossy(&short[8..11]).trim_end().to_string();
    if stem.starts_with('\u{5}') {
        stem.replace_range(0..1, "\u{e5}");
    }
    if case & 0x08 != 0 {
        stem = stem.to_ascii_lowercase();
    }
    if case & 0x10 != 0 {
        ext = ext.to_ascii_lowercase();
    }
    if ext.is_empty() { stem } else { format!("{stem}.{ext}") }
}

struct Fat32 {
    kind: FatKind,
    view: PartitionView,
    label: String,
    reserved_sectors: u32,
    sectors_per_cluster: u32,
    fat_sectors: u32,
    /// The medium's logical sector size, which this FAT declares.
    sector: u32,
    total_sectors: u32,
    cluster_count: u32,
    /// First data sector — after the FATs, and after the fixed root directory
    /// when there is one.
    data_start: u32,
    /// FAT16 only: where the fixed root directory lives, and how big it is.
    root_dir_sector: u32,
    root_dir_sectors: u32,
    fat: Vec<u32>,
    next_free: u32,
    volume_id: u32,
}

fn fat32_cluster_size(total_sectors: u32, sector: u32) -> u32 {
    // The usual table, then walked down until the cluster count is legal.
    let mb = total_sectors / (1024 * 1024 / sector);
    let mut spc: u32 = match mb {
        0..=260 => 1,
        261..=8192 => 8,
        8193..=16384 => 16,
        16385..=32768 => 32,
        _ => 64,
    };
    while spc > 1 {
        let clusters = (total_sectors - FAT32_RESERVED) / spc;
        if clusters >= MIN_CLUSTERS {
            break;
        }
        spc /= 2;
    }
    spc
}

/// The smallest cluster that keeps a FAT16 under its cluster ceiling — small
/// clusters waste less on the many tiny files an ESP holds.
fn fat16_cluster_size(total_sectors: u32) -> u32 {
    for spc in [1u32, 2, 4, 8, 16, 32, 64] {
        if total_sectors / spc <= FAT16_MAX_CLUSTERS {
            return spc;
        }
    }
    64
}

impl Fat32 {
    fn new(device: Arc<dyn BlockDevice>, label: &str) -> Result<Fat32> {
        let capacity = device.capacity_bytes();
        // A FAT declares the medium's own sector size. Anything else is a
        // filesystem that no reader of that medium will mount.
        let sector = device.block_size();
        let total_sectors = (capacity / sector as u64) as u32;
        if total_sectors < 2048 {
            return Err(ImageError::Spec(format!(
                "{capacity} bytes is too small for a FAT volume"
            )));
        }

        // FAT32 where it is legal, FAT16 below that. The choice is the volume's
        // size and nothing else, so it is the same on every build.
        let kind = if (total_sectors - FAT32_RESERVED) >= MIN_CLUSTERS {
            FatKind::Fat32
        } else {
            FatKind::Fat16
        };

        let (reserved, root_entries) = match kind {
            FatKind::Fat32 => (FAT32_RESERVED, 0),
            FatKind::Fat16 => (1, FAT16_ROOT_ENTRIES),
        };
        let root_dir_sectors = (root_entries * 32).div_ceil(sector);
        let spc = match kind {
            FatKind::Fat32 => fat32_cluster_size(total_sectors, sector),
            FatKind::Fat16 => fat16_cluster_size(total_sectors),
        };
        let bytes_per_fat_entry = match kind {
            FatKind::Fat32 => 4u32,
            FatKind::Fat16 => 2,
        };

        // FAT size and cluster count are mutually dependent; a few passes settle
        // it, and each one only ever shrinks the count.
        let mut fat_sectors = 1;
        for _ in 0..6 {
            let data_sectors = total_sectors
                .saturating_sub(reserved)
                .saturating_sub(NUM_FATS * fat_sectors)
                .saturating_sub(root_dir_sectors);
            let clusters = data_sectors / spc;
            let needed = ((clusters + 2) * bytes_per_fat_entry).div_ceil(sector).max(1);
            if needed == fat_sectors {
                break;
            }
            fat_sectors = needed;
        }
        // The passes can oscillate by one sector instead of settling — at
        // 64 MiB of 512-byte sectors they alternate 1008 / 1009 — and ending
        // on the smaller leaves clusters with no FAT entry. fsck.fat says
        // "129024 clusters but only space for 129022 FAT entries"; firmware
        // is entitled to refuse the volume. Grow until every cluster has one.
        loop {
            let data_sectors = total_sectors
                .saturating_sub(reserved)
                .saturating_sub(NUM_FATS * fat_sectors)
                .saturating_sub(root_dir_sectors);
            let clusters = data_sectors / spc;
            if (clusters + 2) * bytes_per_fat_entry <= fat_sectors * sector {
                break;
            }
            fat_sectors += 1;
        }
        let data_sectors =
            total_sectors - reserved - NUM_FATS * fat_sectors - root_dir_sectors;
        let cluster_count = data_sectors / spc;

        match kind {
            FatKind::Fat32 if cluster_count < MIN_CLUSTERS => {
                return Err(ImageError::Spec(format!(
                    "an ESP of {capacity} bytes holds {cluster_count} clusters, and FAT32 needs \
                     {MIN_CLUSTERS}"
                )))
            }
            FatKind::Fat16 if cluster_count < FAT16_MIN_CLUSTERS => {
                return Err(ImageError::Spec(format!(
                    "an ESP of {capacity} bytes holds {cluster_count} clusters, and FAT16 needs \
                     {FAT16_MIN_CLUSTERS} — give it at least 4M"
                )))
            }
            _ => {}
        }

        let view = PartitionView::whole(device);
        let mut fat = vec![0u32; (cluster_count + 2) as usize];
        fat[0] = match kind {
            FatKind::Fat32 => 0x0FFF_FFF8,
            FatKind::Fat16 => 0xFFF8,
        };
        fat[1] = kind.eoc();
        let next_free = match kind {
            // FAT32's root directory is a cluster chain, and it starts at 2.
            FatKind::Fat32 => {
                fat[2] = kind.eoc();
                3
            }
            // FAT16's root directory is a fixed area outside the data region,
            // so cluster 2 is free for content.
            FatKind::Fat16 => 2,
        };

        Ok(Fat32 {
            sector,
            kind,
            view,
            label: label.to_string(),
            reserved_sectors: reserved,
            sectors_per_cluster: spc,
            fat_sectors,
            total_sectors,
            cluster_count,
            root_dir_sector: reserved + NUM_FATS * fat_sectors,
            root_dir_sectors,
            data_start: reserved + NUM_FATS * fat_sectors + root_dir_sectors,
            fat,
            next_free,
            // Derived from the label so a rebuild of the same tree is
            // byte-identical, rather than from the clock.
            volume_id: crate::pallet::crc32(label.as_bytes()) | 0x0100_0000,
        })
    }

    fn cluster_bytes(&self) -> u64 {
        self.sectors_per_cluster as u64 * self.sector as u64
    }

    fn cluster_offset(&self, cluster: u32) -> u64 {
        (self.data_start as u64 + (cluster as u64 - 2) * self.sectors_per_cluster as u64)
            * self.sector as u64
    }

    fn allocate_chain(&mut self, bytes: u64) -> Result<u32> {
        let n = bytes.div_ceil(self.cluster_bytes()).max(1) as u32;
        if self.next_free + n > self.cluster_count + 2 {
            return Err(ImageError::TooSmall {
                need: bytes,
                have: (self.cluster_count + 2 - self.next_free) as u64 * self.cluster_bytes(),
            });
        }
        let first = self.next_free;
        for i in 0..n {
            let c = first + i;
            self.fat[c as usize] = if i + 1 == n { self.kind.eoc() } else { c + 1 };
        }
        self.next_free += n;
        Ok(first)
    }

    /// Extend an existing chain, for a directory that outgrows one cluster.
    fn extend_chain(&mut self, first: u32, bytes: u64) -> Result<()> {
        let want = bytes.div_ceil(self.cluster_bytes()).max(1) as u32;
        let mut have = 1;
        let mut tail = first;
        while self.fat[tail as usize] != self.kind.eoc() {
            tail = self.fat[tail as usize];
            have += 1;
        }
        while have < want {
            let c = self.next_free;
            if c > self.cluster_count + 1 {
                return Err(ImageError::TooSmall { need: bytes, have: 0 });
            }
            self.next_free += 1;
            self.fat[tail as usize] = c;
            self.fat[c as usize] = self.kind.eoc();
            tail = c;
            have += 1;
        }
        Ok(())
    }

    async fn write_chain(&mut self, first: u32, data: &[u8]) -> Result<()> {
        let mut cluster = first;
        let mut off = 0usize;
        let csize = self.cluster_bytes() as usize;
        while off < data.len() {
            let take = (data.len() - off).min(csize);
            let at = self.cluster_offset(cluster);
            // Whole clusters, so a short tail does not leave another file's
            // bytes visible past the end of this one.
            let mut buf = vec![0u8; csize];
            buf[..take].copy_from_slice(&data[off..off + take]);
            self.view.write_at(at, &buf).await?;
            off += take;
            if off < data.len() {
                cluster = self.fat[cluster as usize];
                if cluster >= self.kind.eoc() {
                    return Err(ImageError::Other("FAT chain ended early".into()));
                }
            }
        }
        Ok(())
    }

    async fn write_file(&mut self, path: &Path) -> Result<(u32, u64)> {
        let data = tokio::fs::read(path).await?;
        if data.is_empty() {
            return Ok((0, 0));
        }
        let first = self.allocate_chain(data.len() as u64)?;
        self.write_chain(first, &data).await?;
        Ok((first, data.len() as u64))
    }

    /// Lay a set of in-memory files into the root of the volume.
    async fn write_files(&mut self, files: &[(String, Vec<u8>)]) -> Result<()> {
        // Sorted, so the same seed twice is the same bytes.
        let mut sorted: Vec<&(String, Vec<u8>)> = files.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));

        let mut used_short = HashSet::new();
        let mut entries = Vec::new();
        for (name, data) in sorted {
            let (first, size) = if data.is_empty() {
                (0u32, 0u64)
            } else {
                let first = self.allocate_chain(data.len() as u64)?;
                self.write_chain(first, data).await?;
                (first, data.len() as u64)
            };
            let short = short_name(name, &mut used_short);
            entries.extend_from_slice(&lfn_entries(name, &short));
            entries.extend_from_slice(&dir_entry(&short, ATTR_ARCHIVE, first, size));
        }
        self.write_root(entries).await
    }

    /// Lay a host directory tree into the root of the volume.
    async fn write_tree(&mut self, dir: &Path) -> Result<()> {
        let entries = self.write_dir_children(dir, None).await?;
        self.write_root(entries).await
    }

    /// Put a set of directory entries in the root, with the volume label.
    async fn write_root(&mut self, entries: Vec<u8>) -> Result<()> {
        let mut bytes = Vec::new();
        // The volume label lives in the root directory as well as in the BPB;
        // some tools only look at one of them — and for a cloud-init seed the
        // label *is* the contract, so both are written.
        bytes.extend_from_slice(&label_entry(&self.label));
        bytes.extend_from_slice(&entries);

        match self.kind {
            FatKind::Fat32 => {
                self.extend_chain(2, bytes.len() as u64)?;
                self.write_chain(2, &bytes).await
            }
            FatKind::Fat16 => {
                // A fixed root area cannot grow, which is the one real limit
                // FAT16 imposes on an ESP. Say so rather than silently drop the
                // entries past the end.
                let capacity = (self.root_dir_sectors * self.sector) as usize;
                if bytes.len() > capacity {
                    return Err(ImageError::Spec(format!(
                        "the root directory of this FAT16 volume holds {} entries and the tree \
                         needs {}: put the files in subdirectories, or make the ESP large enough \
                         to be FAT32",
                        capacity / 32,
                        bytes.len() / 32
                    )));
                }
                let mut area = vec![0u8; capacity];
                area[..bytes.len()].copy_from_slice(&bytes);
                let at = self.root_dir_sector as u64 * self.sector as u64;
                self.view.write_at(at, &area).await?;
                Ok(())
            }
        }
    }

    /// Returns the directory-entry bytes for everything inside `dir`, having
    /// already written the contents of each of them.
    ///
    /// `parent` is the cluster of the directory these are written into, or
    /// `None` for the root — which a `..` entry names as cluster 0.
    async fn write_dir_children(&mut self, dir: &Path, parent: Option<u32>) -> Result<Vec<u8>> {
        let parent = parent.unwrap_or(0);
        let mut names: Vec<(String, std::path::PathBuf, bool)> = Vec::new();
        let mut rd = tokio::fs::read_dir(dir).await?;
        while let Some(e) = rd.next_entry().await? {
            let name = e.file_name().to_string_lossy().into_owned();
            let meta = e.metadata().await?;
            names.push((name, e.path(), meta.is_dir()));
        }
        // Sorted, so an image built twice from the same tree is the same image.
        names.sort_by(|a, b| a.0.cmp(&b.0));

        let mut used_short = HashSet::new();
        let mut out = Vec::new();
        for (name, path, is_dir) in names {
            let (first, size, attr) = if is_dir {
                // The directory's own cluster first, so what is inside it can
                // name it in `..` — pointing every `..` at the root is what
                // fsck.fat reports as "Invalid '..' entry" (#123).
                let cluster = self.allocate_chain(self.cluster_bytes())?;
                let child = Box::pin(self.write_dir_children(&path, Some(cluster))).await?;
                let mut bytes = dot_entries(cluster, parent);
                bytes.extend_from_slice(&child);
                self.extend_chain(cluster, bytes.len() as u64)?;
                self.write_chain(cluster, &bytes).await?;
                (cluster, 0u64, ATTR_DIRECTORY)
            } else {
                let (c, len) = self.write_file(&path).await?;
                (c, len, ATTR_ARCHIVE)
            };
            let short = short_name(&name, &mut used_short);
            out.extend_from_slice(&lfn_entries(&name, &short));
            out.extend_from_slice(&dir_entry(&short, attr, first, size));
        }
        Ok(out)
    }

    /// [`Self::write_dir_children`] for a tree held in memory: the same
    /// order (names sorted, directories and files together), the same entries.
    async fn write_mem_children(&mut self, dir: &MemDir, parent: u32) -> Result<Vec<u8>> {
        let mut names: Vec<(&String, bool)> = dir
            .dirs
            .keys()
            .map(|n| (n, true))
            .chain(dir.files.keys().map(|n| (n, false)))
            .collect();
        names.sort_by(|a, b| a.0.cmp(b.0));

        let mut used_short = HashSet::new();
        let mut out = Vec::new();
        for (name, is_dir) in names {
            let (first, size, attr) = if is_dir {
                let cluster = self.allocate_chain(self.cluster_bytes())?;
                let child = Box::pin(self.write_mem_children(&dir.dirs[name], cluster)).await?;
                let mut bytes = dot_entries(cluster, parent);
                bytes.extend_from_slice(&child);
                self.extend_chain(cluster, bytes.len() as u64)?;
                self.write_chain(cluster, &bytes).await?;
                (cluster, 0u64, ATTR_DIRECTORY)
            } else {
                let data = &dir.files[name];
                if data.is_empty() {
                    (0, 0, ATTR_ARCHIVE)
                } else {
                    let first = self.allocate_chain(data.len() as u64)?;
                    self.write_chain(first, data).await?;
                    (first, data.len() as u64, ATTR_ARCHIVE)
                }
            };
            let short = short_name(name, &mut used_short);
            out.extend_from_slice(&lfn_entries(name, &short));
            out.extend_from_slice(&dir_entry(&short, attr, first, size));
        }
        Ok(out)
    }

    /// Write the boot sector, its backup where the format has one, the FSInfo
    /// block, and both FATs.
    async fn finish(&mut self) -> Result<()> {
        let boot = self.boot_sector();
        self.view.write_at(0, &boot).await?;

        if self.kind == FatKind::Fat32 {
            self.view
                .write_at(BACKUP_BOOT_SECTOR as u64 * self.sector as u64, &boot)
                .await?;

            let mut fsinfo = vec![0u8; self.sector as usize];
            fsinfo[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
            fsinfo[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
            let free = self.cluster_count + 2 - self.next_free;
            fsinfo[488..492].copy_from_slice(&free.to_le_bytes());
            fsinfo[492..496].copy_from_slice(&self.next_free.to_le_bytes());
            fsinfo[508..512].copy_from_slice(&[0x00, 0x00, 0x55, 0xAA]);
            self.view
                .write_at(FSINFO_SECTOR as u64 * self.sector as u64, &fsinfo)
                .await?;
            self.view
                .write_at(
                    (BACKUP_BOOT_SECTOR + FSINFO_SECTOR) as u64 * self.sector as u64,
                    &fsinfo,
                )
                .await?;
        }

        let mut fat_bytes = vec![0u8; (self.fat_sectors * self.sector) as usize];
        for (i, e) in self.fat.iter().enumerate() {
            match self.kind {
                FatKind::Fat32 => {
                    let o = i * 4;
                    if o + 4 > fat_bytes.len() {
                        break;
                    }
                    fat_bytes[o..o + 4].copy_from_slice(&e.to_le_bytes());
                }
                FatKind::Fat16 => {
                    let o = i * 2;
                    if o + 2 > fat_bytes.len() {
                        break;
                    }
                    fat_bytes[o..o + 2].copy_from_slice(&(*e as u16).to_le_bytes());
                }
            }
        }
        for n in 0..NUM_FATS {
            let at = (self.reserved_sectors + n * self.fat_sectors) as u64 * self.sector as u64;
            self.view.write_at(at, &fat_bytes).await?;
        }
        self.view.flush().await?;
        Ok(())
    }

    fn boot_sector(&self) -> Vec<u8> {
        let mut b = vec![0u8; self.sector as usize];
        b[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
        b[3..11].copy_from_slice(b"MSWIN4.1");
        b[11..13].copy_from_slice(&(self.sector as u16).to_le_bytes());
        b[13] = self.sectors_per_cluster as u8;
        b[14..16].copy_from_slice(&(self.reserved_sectors as u16).to_le_bytes());
        b[16] = NUM_FATS as u8;
        b[21] = 0xF8;
        b[24..26].copy_from_slice(&63u16.to_le_bytes());
        b[26..28].copy_from_slice(&255u16.to_le_bytes());

        match self.kind {
            FatKind::Fat32 => {
                b[32..36].copy_from_slice(&self.total_sectors.to_le_bytes());
                b[36..40].copy_from_slice(&self.fat_sectors.to_le_bytes());
                b[44..48].copy_from_slice(&2u32.to_le_bytes()); // root cluster
                b[48..50].copy_from_slice(&(FSINFO_SECTOR as u16).to_le_bytes());
                b[50..52].copy_from_slice(&(BACKUP_BOOT_SECTOR as u16).to_le_bytes());
                b[64] = 0x80;
                b[66] = 0x29;
                b[67..71].copy_from_slice(&self.volume_id.to_le_bytes());
                b[71..82].copy_from_slice(&pad_label(&self.label));
                b[82..90].copy_from_slice(b"FAT32   ");
            }
            FatKind::Fat16 => {
                // Root entry count and the 16-bit FAT size are the fields FAT32
                // zeroes; a reader uses exactly these to tell the two apart.
                b[17..19].copy_from_slice(&(FAT16_ROOT_ENTRIES as u16).to_le_bytes());
                if self.total_sectors <= u16::MAX as u32 {
                    b[19..21].copy_from_slice(&(self.total_sectors as u16).to_le_bytes());
                } else {
                    b[32..36].copy_from_slice(&self.total_sectors.to_le_bytes());
                }
                b[22..24].copy_from_slice(&(self.fat_sectors as u16).to_le_bytes());
                b[36] = 0x80;
                b[38] = 0x29;
                b[39..43].copy_from_slice(&self.volume_id.to_le_bytes());
                b[43..54].copy_from_slice(&pad_label(&self.label));
                b[54..62].copy_from_slice(b"FAT16   ");
            }
        }
        b[510] = 0x55;
        b[511] = 0xAA;
        b
    }
}

fn pad_label(label: &str) -> [u8; 11] {
    let mut out = [b' '; 11];
    for (i, c) in label.bytes().take(11).enumerate() {
        out[i] = c.to_ascii_uppercase();
    }
    out
}

fn label_entry(label: &str) -> [u8; 32] {
    let mut e = [0u8; 32];
    e[0..11].copy_from_slice(&pad_label(label));
    e[11] = ATTR_VOLUME_ID;
    e[22..24].copy_from_slice(&FIXED_TIME.to_le_bytes());
    e[24..26].copy_from_slice(&FIXED_DATE.to_le_bytes());
    e
}

fn dot_entries(this: u32, parent: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    for (name, cluster) in [(b".          ", this), (b"..         ", parent)] {
        let mut e = [0u8; 32];
        e[0..11].copy_from_slice(name);
        e[11] = ATTR_DIRECTORY;
        e[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
        e[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
        e[22..24].copy_from_slice(&FIXED_TIME.to_le_bytes());
        e[24..26].copy_from_slice(&FIXED_DATE.to_le_bytes());
        out.extend_from_slice(&e);
    }
    out
}

fn dir_entry(short: &[u8; 11], attr: u8, first_cluster: u32, size: u64) -> [u8; 32] {
    let mut e = [0u8; 32];
    e[0..11].copy_from_slice(short);
    e[11] = attr;
    e[14..16].copy_from_slice(&FIXED_TIME.to_le_bytes());
    e[16..18].copy_from_slice(&FIXED_DATE.to_le_bytes());
    e[18..20].copy_from_slice(&FIXED_DATE.to_le_bytes());
    e[20..22].copy_from_slice(&((first_cluster >> 16) as u16).to_le_bytes());
    e[22..24].copy_from_slice(&FIXED_TIME.to_le_bytes());
    e[24..26].copy_from_slice(&FIXED_DATE.to_le_bytes());
    e[26..28].copy_from_slice(&(first_cluster as u16).to_le_bytes());
    e[28..32].copy_from_slice(&(size as u32).to_le_bytes());
    e
}

/// The 8.3 name a long name is stored under. Uniqueness is per directory, so
/// the caller carries the set.
fn short_name(name: &str, used: &mut HashSet<[u8; 11]>) -> [u8; 11] {
    // Split on the extension *before* sanitising: the separator is not part of
    // either half, and replacing it first turns every plain 8.3 name into a
    // long one that needs a `~1` it never deserved.
    let (raw_stem, raw_ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, e),
        _ => (name, ""),
    };
    let clean = |s: &str| -> String {
        s.to_ascii_uppercase()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "$%'-_@~`!(){}^#&".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let stem = clean(raw_stem);
    let ext: String = clean(raw_ext).chars().take(3).collect();

    let mut candidate = [b' '; 11];
    let put = |c: &mut [u8; 11], stem: &str, ext: &str| {
        *c = [b' '; 11];
        for (i, ch) in stem.bytes().take(8).enumerate() {
            c[i] = ch;
        }
        for (i, ch) in ext.bytes().take(3).enumerate() {
            c[8 + i] = ch;
        }
    };
    put(&mut candidate, &stem, &ext);

    // A name that already *is* 8.3 keeps it, and then needs no long-name
    // entries at all.
    let fits = stem.len() <= 8
        && raw_ext.len() <= 3
        && name.is_ascii()
        && !name.contains(' ')
        && stem == raw_stem.to_ascii_uppercase()
        && ext == raw_ext.to_ascii_uppercase();
    if fits && !used.contains(&candidate) {
        used.insert(candidate);
        return candidate;
    }

    // ~1, ~2, … until it is unique, which is what every other implementation
    // does and what a reader expects to see.
    for n in 1..=999_999u32 {
        let tag = format!("~{n}");
        let keep = 8usize.saturating_sub(tag.len());
        let short_stem = format!("{}{}", &stem.chars().take(keep).collect::<String>(), tag);
        put(&mut candidate, &short_stem, &ext);
        if !used.contains(&candidate) {
            used.insert(candidate);
            return candidate;
        }
    }
    used.insert(candidate);
    candidate
}

fn lfn_checksum(short: &[u8; 11]) -> u8 {
    let mut sum: u8 = 0;
    for &c in short.iter() {
        sum = sum.rotate_right(1).wrapping_add(c);
    }
    sum
}

/// VFAT long-name entries, written before the short entry and in reverse
/// order, 13 UTF-16 units at a time.
fn lfn_entries(name: &str, short: &[u8; 11]) -> Vec<u8> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let short_only = {
        let stem = String::from_utf8_lossy(&short[0..8]).trim_end().to_string();
        let ext = String::from_utf8_lossy(&short[8..11]).trim_end().to_string();
        let joined = if ext.is_empty() { stem.clone() } else { format!("{stem}.{ext}") };
        joined == name.to_ascii_uppercase() && name.is_ascii()
    };
    if short_only {
        return Vec::new();
    }

    let checksum = lfn_checksum(short);
    let chunks: Vec<&[u16]> = units.chunks(13).collect();
    let mut out = Vec::with_capacity(chunks.len() * 32);
    for (i, chunk) in chunks.iter().enumerate().rev() {
        let mut e = [0u8; 32];
        let seq = (i + 1) as u8;
        e[0] = if i + 1 == chunks.len() { seq | 0x40 } else { seq };
        e[11] = ATTR_LFN;
        e[13] = checksum;
        // Positions of the 13 name units inside the entry, per the LFN layout.
        const SPOTS: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
        for (j, spot) in SPOTS.iter().enumerate() {
            let v = match chunk.get(j) {
                Some(u) => *u,
                // One NUL terminator, then 0xFFFF padding — a reader uses this
                // to find the end of the name.
                None if j == chunk.len() => 0x0000,
                None => 0xFFFF,
            };
            e[*spot..*spot + 2].copy_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&e);
    }
    out
}

/// The smallest volume this can format at all, as FAT16.
pub fn minimum_size() -> u64 {
    (FAT16_MIN_CLUSTERS as u64 + 1 + 32 + 2 * 32) * DEFAULT_SECTOR as u64
}

/// Which width a volume of this size will be formatted as. Callers sizing an
/// ESP for an ISO need to know: El Torito describes a boot image in a 16-bit
/// count of 512-byte sectors, so 32 MiB is the ceiling for optical boot, and
/// FAT32's 65,525-cluster floor puts it just above that.
pub fn kind_for(capacity: u64) -> FatKind {
    let total_sectors = (capacity / DEFAULT_SECTOR as u64) as u32;
    if total_sectors.saturating_sub(FAT32_RESERVED) >= MIN_CLUSTERS {
        FatKind::Fat32
    } else {
        FatKind::Fat16
    }
}

#[allow(dead_code)]
const _: () = {
    // Keep the read-only attribute referenced; ESP files are written plain but
    // the constant documents the layout.
    let _ = ATTR_READ_ONLY;
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;

    async fn volume(dir: &tempfile::TempDir, name: &str, size: u64) -> Arc<dyn BlockDevice> {
        let p = dir.path().join(name);
        Arc::new(
            FileDevice::open_with_capacity(p.to_str().unwrap(), size)
                .await
                .unwrap(),
        )
    }

    /// The two widths do not overlap by accident: FAT32's floor sits just above
    /// El Torito's 32 MiB ceiling, which is why both exist here.
    #[test]
    fn the_width_follows_the_size() {
        assert_eq!(kind_for(16 * 1024 * 1024), FatKind::Fat16);
        assert_eq!(kind_for(32 * 1024 * 1024), FatKind::Fat16);
        assert_eq!(kind_for(64 * 1024 * 1024), FatKind::Fat32);
        assert_eq!(kind_for(512 * 1024 * 1024), FatKind::Fat32);
    }

    #[tokio::test]
    async fn a_fat16_volume_declares_itself_fat16() {
        let dir = tempfile::TempDir::new().unwrap();
        let dev = volume(&dir, "esp16.img", 24 * 1024 * 1024).await;
        format(dev.clone(), "EFI").await.unwrap();

        let mut boot = vec![0u8; 512];
        crate::pallet::PartitionView::whole(dev)
            .read_at(0, &mut boot)
            .await
            .unwrap();
        assert_eq!(&boot[54..62], b"FAT16   ");
        assert_eq!(u16::from_le_bytes([boot[17], boot[18]]), 512, "root entries");
        assert_ne!(u16::from_le_bytes([boot[22], boot[23]]), 0, "16-bit FAT size");
        assert_eq!(&boot[510..512], &[0x55, 0xAA]);
    }

    /// FAT32 needs 65525 clusters, and a cluster is at least one sector — so
    /// how big a volume has to be before it can be FAT32 scales with the
    /// medium's sector size. At 512 bytes a 64 MiB volume clears it; at 4096,
    /// which is what every device here presents, the same 64 MiB has an eighth
    /// as many sectors and is correctly FAT16. This asks for a volume big
    /// enough to be FAT32 at the sector size it will actually be read at.
    #[tokio::test]
    async fn a_fat32_volume_declares_itself_fat32() {
        let dir = tempfile::TempDir::new().unwrap();
        let dev = volume(&dir, "esp32.img", 512 * 1024 * 1024).await;
        format(dev.clone(), "EFI").await.unwrap();

        let mut boot = vec![0u8; 512];
        crate::pallet::PartitionView::whole(dev)
            .read_at(0, &mut boot)
            .await
            .unwrap();
        assert_eq!(&boot[82..90], b"FAT32   ");
        assert_eq!(u16::from_le_bytes([boot[17], boot[18]]), 0, "no fixed root");
        assert_eq!(u16::from_le_bytes([boot[22], boot[23]]), 0, "no 16-bit FAT size");
        assert_eq!(u32::from_le_bytes([boot[44], boot[45], boot[46], boot[47]]), 2);
    }

    #[tokio::test]
    async fn a_tree_lands_in_both_widths() {
        for size in [24 * 1024 * 1024u64, 64 * 1024 * 1024] {
            let dir = tempfile::TempDir::new().unwrap();
            let src = dir.path().join("esp");
            std::fs::create_dir_all(src.join("EFI/BOOT")).unwrap();
            std::fs::write(src.join("EFI/BOOT/BOOTX64.EFI"), vec![0xE1; 100_000]).unwrap();
            std::fs::create_dir_all(src.join("loader/entries")).unwrap();
            std::fs::write(src.join("loader/entries/stormcos-6.12.0.conf"), b"title x\n").unwrap();

            let dev = volume(&dir, "esp.img", size).await;
            format_from_dir(dev, &src, "EFI").await.expect("format");
        }
    }

    /// A name that is already 8.3 keeps it — and then needs no long-name
    /// entries at all, which is what a firmware reading only short names sees.
    #[test]
    fn a_plain_name_is_stored_plainly() {
        let mut used = HashSet::new();
        let short = short_name("BOOTX64.EFI", &mut used);
        assert_eq!(&short, b"BOOTX64 EFI");
        assert!(lfn_entries("BOOTX64.EFI", &short).is_empty());
    }

    #[test]
    fn a_long_name_gets_a_tilde_and_real_lfn_entries() {
        let mut used = HashSet::new();
        let short = short_name("stormcos-6.12.0.conf", &mut used);
        assert_eq!(&short[0..2], b"ST");
        assert!(short.starts_with(b"STORMC~1"), "{:?}", std::str::from_utf8(&short));
        let lfn = lfn_entries("stormcos-6.12.0.conf", &short);
        assert_eq!(lfn.len(), 64, "20 characters needs two entries of 13");
        // Last-in-sequence marker on the first entry written, and every entry
        // carries the short name's checksum so a reader can pair them.
        assert_eq!(lfn[0] & 0x40, 0x40);
        assert_eq!(lfn[11], ATTR_LFN);
        assert_eq!(lfn[13], lfn_checksum(&short));
        assert_eq!(lfn[32 + 13], lfn_checksum(&short));
    }

    #[test]
    fn two_long_names_that_collide_get_different_short_ones() {
        let mut used = HashSet::new();
        let a = short_name("stormcos-6.12.0.conf", &mut used);
        let b = short_name("stormcos-6.13.0.conf", &mut used);
        assert_ne!(a, b);
    }

    /// A cloud-init seed is what this exists for now: three files at the root
    /// of a labelled volume, with names that do not fit 8.3 and therefore need
    /// real long-name entries. NoCloud finds it by the label and reads it by
    /// the names, so both have to be right.
    #[tokio::test]
    async fn a_seed_is_three_files_and_a_label() {
        let dir = tempfile::TempDir::new().unwrap();
        let dev = volume(&dir, "seed.img", 16 << 20).await;
        let files = vec![
            ("user-data".to_string(), b"#cloud-config\nhostname: web1\n".to_vec()),
            ("meta-data".to_string(), b"instance-id: iid-web1\n".to_vec()),
            ("network-config".to_string(), b"version: 2\n".to_vec()),
        ];
        super::format_from_files(dev.clone(), &files, "CIDATA").await.unwrap();

        let mut head = vec![0u8; 512];
        dev.read(0, &mut head).await.unwrap();
        // The label lives in the BPB as well as the root directory, because
        // some tools read only one of them.
        let bpb_label = String::from_utf8_lossy(&head[43..54]).to_string();
        assert!(bpb_label.starts_with("CIDATA"), "{bpb_label:?}");

        // And the long names are really there — searched as UTF-16, which is
        // how an LFN entry stores them.
        let mut whole = vec![0u8; 2 << 20];
        dev.read(0, &mut whole).await.unwrap();
        for short in ["USER-D~1", "META-D~1", "NETWOR~1"] {
            assert!(
                whole.windows(short.len()).any(|w| w == short.as_bytes()),
                "{short} is not in the image; short names present: {:?}",
                whole
                    .windows(8)
                    .filter(|w| w.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'~' || *c == b'-'))
                    .take(6)
                    .map(|w| String::from_utf8_lossy(w).to_string())
                    .collect::<Vec<_>>()
            );
        }
        // The long name itself, in UTF-16 — but only the first five
        // characters, because an LFN entry splits a name across three field
        // ranges (chars 1-5, 6-11, 12-13) and it is never contiguous on disk.
        for name in ["user-data", "meta-data", "network-config"] {
            let head: Vec<u8> = name
                .encode_utf16()
                .take(5)
                .flat_map(|c| c.to_le_bytes())
                .collect();
            assert!(
                whole.windows(head.len()).any(|w| w == head),
                "{name} has no long-name entry — a guest would see only the 8.3 name"
            );
        }
    }
    /// What is read out of an ESP is what was put in — names long and short,
    /// directories, empty files — and it lays down again at another sector
    /// size, which is the whole reason the reader exists (#123).
    #[tokio::test]
    async fn a_tree_reads_back_and_moves_between_sector_sizes() {
        use crate::drive::partition::PartitionDevice;
        let dir = tempfile::TempDir::new().unwrap();
        let files = vec![
            ("EFI/BOOT/BOOTX64.EFI".to_string(), vec![0xE1; 300_000]),
            ("EFI/stormcos/grub.cfg".to_string(), b"set timeout=0\n".to_vec()),
            ("loader/entries/stormcos-6.12.0-200.fc41.conf".to_string(), b"title x\n".to_vec()),
            ("empty".to_string(), Vec::new()),
        ];
        let dirs = vec!["EFI/Linux".to_string()];
        // FAT is case-insensitive, and a name that fits 8.3 is stored in
        // capitals with no long name; compare the way a reader resolves them.
        let sorted = |t: &FatTree| {
            let mut f: Vec<(String, Vec<u8>)> =
                t.files.iter().map(|(p, b)| (p.to_ascii_uppercase(), b.clone())).collect();
            f.sort();
            let mut d: Vec<String> = t.dirs.iter().map(|p| p.to_ascii_uppercase()).collect();
            d.sort();
            (f, d)
        };
        let mut want_files: Vec<(String, Vec<u8>)> =
            files.iter().map(|(p, b)| (p.to_ascii_uppercase(), b.clone())).collect();
        want_files.sort();

        // At 4096, the way an image served over NVMe/TCP carries it: FAT16.
        let big = volume(&dir, "esp4k.img", 32 << 20).await;
        format_from_tree(big.clone(), &files, &dirs, "EFI").await.unwrap();
        let tree = read_tree(big).await.unwrap();
        assert_eq!(tree.label, "EFI");
        let (f, d) = sorted(&tree);
        assert_eq!(f, want_files);
        for want in ["EFI", "EFI/BOOT", "EFI/Linux", "EFI/stormcos", "loader", "loader/entries"] {
            assert!(d.contains(&want.to_ascii_uppercase()), "{want} missing from {d:?}");
        }

        // At 512, the way a local drive is read: 64 MiB is FAT32 there.
        let disk = volume(&dir, "disk.img", 64 << 20).await;
        let small: Arc<dyn BlockDevice> =
            Arc::new(PartitionDevice::with_block_size(disk, 0, 64 << 20, 512).unwrap());
        format_from_tree(small.clone(), &tree.files, &tree.dirs, &tree.label).await.unwrap();
        let mut boot = vec![0u8; 512];
        small.read(0, &mut boot).await.unwrap();
        assert_eq!(u16::from_le_bytes([boot[11], boot[12]]), 512);
        assert_eq!(&boot[82..90], b"FAT32   ");
        let again = read_tree(small).await.unwrap();
        assert_eq!(sorted(&again), sorted(&tree));
    }

    /// Not a FAT: said so, not read as an empty one.
    #[tokio::test]
    async fn a_volume_that_is_not_fat_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let dev = volume(&dir, "zero.img", 8 << 20).await;
        assert!(read_tree(dev).await.is_err());
    }
    /// Every cluster has a FAT entry, at every size an ESP is made in — the
    /// sizing passes used to settle one sector short at 64 MiB of 512-byte
    /// sectors, which fsck.fat found and none of our tests did (#123).
    #[tokio::test]
    async fn every_cluster_has_a_fat_entry() {
        use crate::drive::partition::PartitionDevice;
        let dir = tempfile::TempDir::new().unwrap();
        let disk = volume(&dir, "sizes.img", 600 << 20).await;
        for mib in [24u64, 32, 48, 64, 100, 128, 260, 512] {
            for sector in [512u32, 4096] {
                let len = mib << 20;
                let dev: Arc<dyn BlockDevice> =
                    Arc::new(PartitionDevice::with_block_size(disk.clone(), 0, len, sector).unwrap());
                if format(dev.clone(), "EFI").await.is_err() {
                    continue; // too small for either width at this sector size
                }
                let mut b = vec![0u8; 512];
                dev.read(0, &mut b).await.unwrap();
                let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]) as u64;
                let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as u64;
                let bps = u16_at(11);
                let spc = b[13] as u64;
                let reserved = u16_at(14);
                let root = (u16_at(17) * 32).div_ceil(bps);
                let total = if u16_at(19) != 0 { u16_at(19) } else { u32_at(32) };
                let (fat, width) = if u16_at(22) != 0 { (u16_at(22), 2) } else { (u32_at(36), 4) };
                let clusters = (total - reserved - 2 * fat - root) / spc;
                assert!(
                    (clusters + 2) * width <= fat * bps,
                    "{mib} MiB at {sector}: {clusters} clusters, FAT holds {}",
                    fat * bps / width
                );
            }
        }
    }
    /// A subdirectory's `..` names its parent, and the root as cluster 0 —
    /// what fsck.fat checks and what a reader walking up relies on.
    #[tokio::test]
    async fn dot_dot_names_the_parent() {
        let dir = tempfile::TempDir::new().unwrap();
        let dev = volume(&dir, "dots.img", 32 << 20).await;
        let files = vec![("EFI/BOOT/BOOTX64.EFI".to_string(), vec![1u8; 10])];
        format_from_tree(dev.clone(), &files, &[], "EFI").await.unwrap();
        let r = FatReader::open(dev).await.unwrap();
        let root = r.root_bytes().await.unwrap();
        let cluster_of = |dir: &[u8], name: &[u8; 11]| -> u32 {
            // By the directory bit too: the volume label is also `EFI`.
            let e = dir
                .chunks_exact(32)
                .find(|e| &e[0..11] == name && e[11] & ATTR_DIRECTORY != 0)
                .expect("entry");
            (u16::from_le_bytes([e[20], e[21]]) as u32) << 16 | u16::from_le_bytes([e[26], e[27]]) as u32
        };
        let efi = cluster_of(&root, b"EFI        ");
        let efi_dir = r.read_chain(efi, None).await.unwrap();
        assert_eq!(cluster_of(&efi_dir, b"..         "), 0, "/EFI's parent is the root");
        let boot = cluster_of(&efi_dir, b"BOOT       ");
        let boot_dir = r.read_chain(boot, None).await.unwrap();
        assert_eq!(cluster_of(&boot_dir, b".          "), boot);
        assert_eq!(cluster_of(&boot_dir, b"..         "), efi, "/EFI/BOOT's parent is /EFI");
    }
}


