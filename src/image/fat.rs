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
const SECTOR: u32 = 512;
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

struct Fat32 {
    kind: FatKind,
    view: PartitionView,
    label: String,
    reserved_sectors: u32,
    sectors_per_cluster: u32,
    fat_sectors: u32,
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

fn fat32_cluster_size(total_sectors: u32) -> u32 {
    // The usual table, then walked down until the cluster count is legal.
    let mb = total_sectors / (1024 * 1024 / SECTOR);
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
        let total_sectors = (capacity / SECTOR as u64) as u32;
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
        let root_dir_sectors = (root_entries * 32).div_ceil(SECTOR);
        let spc = match kind {
            FatKind::Fat32 => fat32_cluster_size(total_sectors),
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
            let needed = ((clusters + 2) * bytes_per_fat_entry).div_ceil(SECTOR).max(1);
            if needed == fat_sectors {
                break;
            }
            fat_sectors = needed;
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
        self.sectors_per_cluster as u64 * SECTOR as u64
    }

    fn cluster_offset(&self, cluster: u32) -> u64 {
        (self.data_start as u64 + (cluster as u64 - 2) * self.sectors_per_cluster as u64)
            * SECTOR as u64
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

    /// Lay a host directory tree into the root of the volume.
    async fn write_tree(&mut self, dir: &Path) -> Result<()> {
        let entries = self.write_dir_children(dir, None).await?;
        let mut bytes = Vec::new();
        // The volume label lives in the root directory as well as in the BPB;
        // some tools only look at one of them.
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
                let capacity = (self.root_dir_sectors * SECTOR) as usize;
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
                let at = self.root_dir_sector as u64 * SECTOR as u64;
                self.view.write_at(at, &area).await?;
                Ok(())
            }
        }
    }

    /// Returns the directory-entry bytes for everything inside `dir`, having
    /// already written the contents of each of them.
    async fn write_dir_children(&mut self, dir: &Path, parent: Option<u32>) -> Result<Vec<u8>> {
        let _ = parent;
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
                let child = Box::pin(self.write_dir_children(&path, None)).await?;
                let cluster = self.allocate_chain(self.cluster_bytes())?;
                let mut bytes = dot_entries(cluster, 0);
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

    /// Write the boot sector, its backup where the format has one, the FSInfo
    /// block, and both FATs.
    async fn finish(&mut self) -> Result<()> {
        let boot = self.boot_sector();
        self.view.write_at(0, &boot).await?;

        if self.kind == FatKind::Fat32 {
            self.view
                .write_at(BACKUP_BOOT_SECTOR as u64 * SECTOR as u64, &boot)
                .await?;

            let mut fsinfo = vec![0u8; SECTOR as usize];
            fsinfo[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
            fsinfo[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
            let free = self.cluster_count + 2 - self.next_free;
            fsinfo[488..492].copy_from_slice(&free.to_le_bytes());
            fsinfo[492..496].copy_from_slice(&self.next_free.to_le_bytes());
            fsinfo[508..512].copy_from_slice(&[0x00, 0x00, 0x55, 0xAA]);
            self.view
                .write_at(FSINFO_SECTOR as u64 * SECTOR as u64, &fsinfo)
                .await?;
            self.view
                .write_at(
                    (BACKUP_BOOT_SECTOR + FSINFO_SECTOR) as u64 * SECTOR as u64,
                    &fsinfo,
                )
                .await?;
        }

        let mut fat_bytes = vec![0u8; (self.fat_sectors * SECTOR) as usize];
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
            let at = (self.reserved_sectors + n * self.fat_sectors) as u64 * SECTOR as u64;
            self.view.write_at(at, &fat_bytes).await?;
        }
        self.view.flush().await?;
        Ok(())
    }

    fn boot_sector(&self) -> Vec<u8> {
        let mut b = vec![0u8; SECTOR as usize];
        b[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
        b[3..11].copy_from_slice(b"MSWIN4.1");
        b[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
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
    (FAT16_MIN_CLUSTERS as u64 + 1 + 32 + 2 * 32) * SECTOR as u64
}

/// Which width a volume of this size will be formatted as. Callers sizing an
/// ESP for an ISO need to know: El Torito describes a boot image in a 16-bit
/// count of 512-byte sectors, so 32 MiB is the ceiling for optical boot, and
/// FAT32's 65,525-cluster floor puts it just above that.
pub fn kind_for(capacity: u64) -> FatKind {
    let total_sectors = (capacity / SECTOR as u64) as u32;
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

    #[tokio::test]
    async fn a_fat32_volume_declares_itself_fat32() {
        let dir = tempfile::TempDir::new().unwrap();
        let dev = volume(&dir, "esp32.img", 64 * 1024 * 1024).await;
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
}
