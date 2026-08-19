//! Output formats — the same image, in whatever the consumer can take.
//!
//! Every format here is a *conversion of the finished raw image*, so there is
//! one builder and one layout, and a qcow2 and a VHD of the same build are the
//! same bytes described differently. The sparse formats skip all-zero clusters,
//! which is most of a freshly built image: the slab is empty and the pallets
//! are laid out with headroom.
//!
//! The exception is [`ImageFormat::Iso`], which is not a container around the
//! raw image but a filesystem in its own right with the partitions appended
//! behind it — see [`super::iso`].

use std::path::{Path, PathBuf};

use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use super::{ImageError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    /// A plain disk image. Sparse on any filesystem that supports holes.
    Raw,
    /// QEMU / KVM / Proxmox.
    Qcow2,
    /// Hyper-V and Azure. Fixed-size: raw plus a 512-byte footer.
    Vhd,
    /// VMware, monolithic sparse.
    Vmdk,
    /// Bootable optical image, with the partitions appended so the same file
    /// also works written to a USB stick.
    Iso,
}

impl ImageFormat {
    pub fn parse(s: &str) -> Option<ImageFormat> {
        match s.to_ascii_lowercase().as_str() {
            "raw" | "img" | "bin" => Some(ImageFormat::Raw),
            "qcow2" | "qcow" => Some(ImageFormat::Qcow2),
            "vhd" | "vpc" => Some(ImageFormat::Vhd),
            "vmdk" => Some(ImageFormat::Vmdk),
            "iso" => Some(ImageFormat::Iso),
            _ => None,
        }
    }

    /// Guess from an output path, so `--out disk.qcow2` needs no `--format`.
    pub fn from_path(p: &Path) -> Option<ImageFormat> {
        p.extension().and_then(|e| e.to_str()).and_then(ImageFormat::parse)
    }

    pub fn extension(&self) -> &'static str {
        match self {
            ImageFormat::Raw => "img",
            ImageFormat::Qcow2 => "qcow2",
            ImageFormat::Vhd => "vhd",
            ImageFormat::Vmdk => "vmdk",
            ImageFormat::Iso => "iso",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ImageFormat::Raw => "raw",
            ImageFormat::Qcow2 => "qcow2",
            ImageFormat::Vhd => "vhd",
            ImageFormat::Vmdk => "vmdk",
            ImageFormat::Iso => "iso",
        }
    }

    pub const ALL: [ImageFormat; 5] = [
        ImageFormat::Raw,
        ImageFormat::Qcow2,
        ImageFormat::Vhd,
        ImageFormat::Vmdk,
        ImageFormat::Iso,
    ];
}

impl std::fmt::Display for ImageFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Convert a raw image into `format` at `out`.
pub async fn convert(raw: &Path, out: &Path, format: ImageFormat) -> Result<PathBuf> {
    match format {
        ImageFormat::Raw => {
            if raw != out {
                tokio::fs::copy(raw, out).await?;
            }
            Ok(out.to_path_buf())
        }
        ImageFormat::Qcow2 => {
            to_qcow2(raw, out).await?;
            Ok(out.to_path_buf())
        }
        ImageFormat::Vhd => {
            to_vhd(raw, out).await?;
            Ok(out.to_path_buf())
        }
        ImageFormat::Vmdk => {
            to_vmdk(raw, out).await?;
            Ok(out.to_path_buf())
        }
        ImageFormat::Iso => super::iso::from_image(raw, out).await,
    }
}

const CLUSTER_BITS: u32 = 16;
const CLUSTER: u64 = 1 << CLUSTER_BITS;

fn all_zero(b: &[u8]) -> bool {
    b.iter().all(|&x| x == 0)
}

// ------------------------------------------------------------------- qcow2

/// qcow2 v3, sparse.
///
/// Written in one pass over a plan built in a first pass, because every offset
/// in the file — L1, L2, refcounts — has to be known before the first byte
/// goes down.
pub async fn to_qcow2(raw: &Path, out: &Path) -> Result<()> {
    let mut src = tokio::fs::File::open(raw).await?;
    let virtual_size = src.metadata().await?.len();
    let total_clusters = virtual_size.div_ceil(CLUSTER);

    // Pass 1: which clusters carry anything.
    let mut used = vec![false; total_clusters as usize];
    let mut buf = vec![0u8; CLUSTER as usize];
    for (i, slot) in used.iter_mut().enumerate() {
        let want = ((virtual_size - i as u64 * CLUSTER).min(CLUSTER)) as usize;
        src.read_exact(&mut buf[..want]).await?;
        *slot = !all_zero(&buf[..want]);
    }

    let l2_entries = CLUSTER / 8;
    let l1_size = total_clusters.div_ceil(l2_entries).max(1);
    let l1_clusters = (l1_size * 8).div_ceil(CLUSTER);
    // An L2 table exists only where at least one of its clusters is used.
    let mut l2_needed = vec![false; l1_size as usize];
    for (i, u) in used.iter().enumerate() {
        if *u {
            l2_needed[i / l2_entries as usize] = true;
        }
    }
    let l2_count = l2_needed.iter().filter(|x| **x).count() as u64;
    let data_count = used.iter().filter(|x| **x).count() as u64;

    // Refcount blocks cover CLUSTER/2 clusters each (16-bit refcounts), and
    // adding them can push the file over a boundary that needs another one —
    // so settle it rather than guess.
    let per_rb = CLUSTER / 2;
    let mut rb_count = 1u64;
    let mut file_clusters;
    loop {
        file_clusters = 1 + 1 + rb_count + l1_clusters + l2_count + data_count;
        let need = file_clusters.div_ceil(per_rb).max(1);
        if need == rb_count {
            break;
        }
        rb_count = need;
    }

    let refcount_table_off = CLUSTER;
    let rb_off = refcount_table_off + CLUSTER;
    let l1_off = rb_off + rb_count * CLUSTER;
    let l2_off = l1_off + l1_clusters * CLUSTER;
    let data_off = l2_off + l2_count * CLUSTER;

    // Header, big-endian throughout.
    let mut header = vec![0u8; CLUSTER as usize];
    header[0..4].copy_from_slice(b"QFI\xfb");
    header[4..8].copy_from_slice(&3u32.to_be_bytes());
    header[20..24].copy_from_slice(&CLUSTER_BITS.to_be_bytes());
    header[24..32].copy_from_slice(&virtual_size.to_be_bytes());
    header[36..40].copy_from_slice(&(l1_size as u32).to_be_bytes());
    header[40..48].copy_from_slice(&l1_off.to_be_bytes());
    header[48..56].copy_from_slice(&refcount_table_off.to_be_bytes());
    header[56..60].copy_from_slice(&1u32.to_be_bytes());
    header[100..104].copy_from_slice(&4u32.to_be_bytes()); // 16-bit refcounts
    header[104..108].copy_from_slice(&112u32.to_be_bytes());

    // Refcount table, then the blocks it points at.
    let mut rct = vec![0u8; CLUSTER as usize];
    for i in 0..rb_count {
        let off = rb_off + i * CLUSTER;
        rct[(i * 8) as usize..(i * 8 + 8) as usize].copy_from_slice(&off.to_be_bytes());
    }
    let mut rbs = vec![0u8; (rb_count * CLUSTER) as usize];
    for c in 0..file_clusters {
        let o = (c * 2) as usize;
        if o + 2 <= rbs.len() {
            rbs[o..o + 2].copy_from_slice(&1u16.to_be_bytes());
        }
    }

    // L1 and L2. Bit 63 marks an entry as "allocated and not compressed".
    const COPIED: u64 = 1 << 63;
    let mut l1 = vec![0u8; (l1_clusters * CLUSTER) as usize];
    let mut l2 = vec![0u8; (l2_count * CLUSTER) as usize];
    let mut next_l2 = 0u64;
    let mut next_data = 0u64;
    let mut l2_index_of = vec![u64::MAX; l1_size as usize];
    for (i, need) in l2_needed.iter().enumerate() {
        if !need {
            continue;
        }
        l2_index_of[i] = next_l2;
        let off = l2_off + next_l2 * CLUSTER;
        l1[i * 8..i * 8 + 8].copy_from_slice(&(off | COPIED).to_be_bytes());
        next_l2 += 1;
    }
    for (i, u) in used.iter().enumerate() {
        if !*u {
            continue;
        }
        let table = l2_index_of[i / l2_entries as usize];
        let slot = (i as u64 % l2_entries) as usize;
        let at = (table * CLUSTER) as usize + slot * 8;
        let off = data_off + next_data * CLUSTER;
        l2[at..at + 8].copy_from_slice(&(off | COPIED).to_be_bytes());
        next_data += 1;
    }

    let mut dst = tokio::fs::File::create(out).await?;
    dst.write_all(&header).await?;
    dst.write_all(&rct).await?;
    dst.write_all(&rbs).await?;
    dst.write_all(&l1).await?;
    dst.write_all(&l2).await?;

    // Pass 2: the data clusters, in image order.
    src.seek(std::io::SeekFrom::Start(0)).await?;
    for (i, u) in used.iter().enumerate() {
        let want = ((virtual_size - i as u64 * CLUSTER).min(CLUSTER)) as usize;
        src.read_exact(&mut buf[..want]).await?;
        if *u {
            // A short tail is padded: a qcow2 cluster is always whole.
            for b in buf.iter_mut().take(CLUSTER as usize).skip(want) {
                *b = 0;
            }
            dst.write_all(&buf).await?;
        }
    }
    dst.flush().await?;
    Ok(())
}

// --------------------------------------------------------------------- VHD

/// Fixed-size VHD: the raw image, then a 512-byte footer.
///
/// Fixed rather than dynamic because that is what Azure requires, and because
/// a format whose whole content is "the raw image" cannot be wrong about it.
pub async fn to_vhd(raw: &Path, out: &Path) -> Result<()> {
    let size = tokio::fs::metadata(raw).await?.len();
    if size % 512 != 0 {
        return Err(ImageError::Spec(format!(
            "a VHD's size must be a whole number of sectors; this image is {size} bytes"
        )));
    }
    tokio::fs::copy(raw, out).await?;
    let mut f = tokio::fs::OpenOptions::new().append(true).open(out).await?;
    f.write_all(&vhd_footer(size)).await?;
    f.flush().await?;
    Ok(())
}

fn vhd_footer(size: u64) -> [u8; 512] {
    let mut f = [0u8; 512];
    f[0..8].copy_from_slice(b"conectix");
    f[8..12].copy_from_slice(&2u32.to_be_bytes()); // features: reserved bit
    f[12..16].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    f[16..24].copy_from_slice(&u64::MAX.to_be_bytes()); // no dynamic header
    // Fixed timestamp: the same image should convert to the same bytes.
    f[24..28].copy_from_slice(&0u32.to_be_bytes());
    f[28..32].copy_from_slice(b"strm");
    f[32..36].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    f[36..40].copy_from_slice(b"Wi2k");
    f[40..48].copy_from_slice(&size.to_be_bytes());
    f[48..56].copy_from_slice(&size.to_be_bytes());
    let (c, h, s) = chs(size / 512);
    f[56..58].copy_from_slice(&c.to_be_bytes());
    f[58] = h;
    f[59] = s;
    f[60..64].copy_from_slice(&2u32.to_be_bytes()); // fixed
    // Identity derived from the size, so it is stable across rebuilds.
    let id = uuid::Uuid::from_u128(0x5354_4F52_4D42_4C4F_434B_0000_0000_0000u128 | size as u128);
    f[68..84].copy_from_slice(id.as_bytes());
    let sum: u32 = f.iter().map(|&b| b as u32).sum();
    f[64..68].copy_from_slice(&(!sum).to_be_bytes());
    f
}

/// The CHS translation the VHD spec spells out.
fn chs(total_sectors: u64) -> (u16, u8, u8) {
    let ts = total_sectors.min(65535 * 16 * 255);
    let (mut spt, mut heads, mut cth);
    if ts >= 65535 * 16 * 63 {
        spt = 255;
        heads = 16;
        cth = ts / spt;
    } else {
        spt = 17;
        cth = ts / spt;
        heads = ((cth + 1023) / 1024).max(4);
        if heads > 16 {
            spt = 31;
            heads = 16;
            cth = ts / spt;
        }
        if cth >= heads * 1024 {
            spt = 63;
            heads = 16;
            cth = ts / spt;
        }
    }
    ((cth / heads) as u16, heads as u8, spt as u8)
}

// -------------------------------------------------------------------- VMDK

const VMDK_GRAIN_SECTORS: u64 = 128; // 64 KiB
const VMDK_GTES_PER_GT: u64 = 512;

/// Monolithic sparse VMDK — one file, which is what every consumer of a VMDK
/// actually wants to be handed.
pub async fn to_vmdk(raw: &Path, out: &Path) -> Result<()> {
    let mut src = tokio::fs::File::open(raw).await?;
    let size = src.metadata().await?.len();
    let capacity_sectors = size.div_ceil(512);
    let grain_bytes = VMDK_GRAIN_SECTORS * 512;
    let grains = size.div_ceil(grain_bytes);
    let gt_count = grains.div_ceil(VMDK_GTES_PER_GT).max(1);

    let mut used = vec![false; grains as usize];
    let mut buf = vec![0u8; grain_bytes as usize];
    for (i, slot) in used.iter_mut().enumerate() {
        let want = ((size - i as u64 * grain_bytes).min(grain_bytes)) as usize;
        src.read_exact(&mut buf[..want]).await?;
        *slot = !all_zero(&buf[..want]);
    }

    // Sectors: [0] header, [1..21] descriptor, GD, GTs, then grains — with the
    // grain area 128-sector aligned, which VMware readers expect.
    let descriptor_sector = 1u64;
    let descriptor_sectors = 20u64;
    let gd_sector = descriptor_sector + descriptor_sectors;
    let gd_sectors = (gt_count * 4).div_ceil(512);
    let gt_sector = gd_sector + gd_sectors;
    let gt_sectors = gt_count * (VMDK_GTES_PER_GT * 4) / 512;
    let overhead = (gt_sector + gt_sectors).div_ceil(VMDK_GRAIN_SECTORS) * VMDK_GRAIN_SECTORS;

    let mut header = vec![0u8; 512];
    header[0..4].copy_from_slice(&0x564d_444bu32.to_le_bytes()); // 'KDMV'
    header[4..8].copy_from_slice(&1u32.to_le_bytes());
    header[8..12].copy_from_slice(&3u32.to_le_bytes()); // valid NL detection + redundant GT off
    header[12..20].copy_from_slice(&capacity_sectors.to_le_bytes());
    header[20..28].copy_from_slice(&VMDK_GRAIN_SECTORS.to_le_bytes());
    header[28..36].copy_from_slice(&descriptor_sector.to_le_bytes());
    header[36..44].copy_from_slice(&descriptor_sectors.to_le_bytes());
    header[44..48].copy_from_slice(&(VMDK_GTES_PER_GT as u32).to_le_bytes());
    header[48..56].copy_from_slice(&0u64.to_le_bytes()); // no redundant grain directory
    header[56..64].copy_from_slice(&gd_sector.to_le_bytes());
    header[64..72].copy_from_slice(&overhead.to_le_bytes());
    header[72] = 0; // cleanly shut down
    header[73] = b'\n';
    header[74] = b' ';
    header[75] = b'\r';
    header[76] = b'\n';
    header[77..79].copy_from_slice(&0u16.to_le_bytes()); // no compression

    let name = out
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("disk.vmdk");
    let descriptor = format!(
        "# Disk DescriptorFile\nversion=1\nCID=fffffffe\nparentCID=ffffffff\n\
         createType=\"monolithicSparse\"\n\n\
         # Extent description\nRW {capacity_sectors} SPARSE \"{name}\"\n\n\
         # The Disk Data Base\n#DDB\nddb.virtualHWVersion = \"14\"\n\
         ddb.geometry.cylinders = \"{cyl}\"\nddb.geometry.heads = \"255\"\n\
         ddb.geometry.sectors = \"63\"\nddb.adapterType = \"lsilogic\"\n",
        cyl = (capacity_sectors / (255 * 63)).max(1)
    );
    let mut desc_bytes = vec![0u8; (descriptor_sectors * 512) as usize];
    desc_bytes[..descriptor.len()].copy_from_slice(descriptor.as_bytes());

    let mut gd = vec![0u8; (gd_sectors * 512) as usize];
    for i in 0..gt_count {
        let s = gt_sector + i * (VMDK_GTES_PER_GT * 4) / 512;
        gd[(i * 4) as usize..(i * 4 + 4) as usize].copy_from_slice(&(s as u32).to_le_bytes());
    }
    let mut gt = vec![0u8; (gt_sectors * 512) as usize];
    let mut next = overhead;
    for (i, u) in used.iter().enumerate() {
        if !*u {
            continue;
        }
        gt[i * 4..i * 4 + 4].copy_from_slice(&(next as u32).to_le_bytes());
        next += VMDK_GRAIN_SECTORS;
    }

    let mut dst = tokio::fs::File::create(out).await?;
    dst.write_all(&header).await?;
    dst.write_all(&desc_bytes).await?;
    dst.write_all(&gd).await?;
    dst.write_all(&gt).await?;
    let written = 512 + desc_bytes.len() as u64 + gd.len() as u64 + gt.len() as u64;
    let pad = overhead * 512 - written;
    dst.write_all(&vec![0u8; pad as usize]).await?;

    src.seek(std::io::SeekFrom::Start(0)).await?;
    for (i, u) in used.iter().enumerate() {
        let want = ((size - i as u64 * grain_bytes).min(grain_bytes)) as usize;
        src.read_exact(&mut buf[..want]).await?;
        if *u {
            for b in buf.iter_mut().take(grain_bytes as usize).skip(want) {
                *b = 0;
            }
            dst.write_all(&buf).await?;
        }
    }
    dst.flush().await?;
    Ok(())
}
