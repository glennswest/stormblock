//! The seam onto XFS (#147): [`mkfs-xfs`](https://github.com/glennswest/mkfs.xfs.rs)
//! formats, [`fio-xfs`](https://github.com/glennswest/fio.xfs.rs) reads.
//!
//! It works like [`super::ext4`]: a thin volume is formatted in place through
//! the engine's own `BlockDevice`, with no loop device, no mount and no
//! `mkfs.xfs` subprocess. What the two crates give today, and so what this
//! module can do:
//!
//! | | ext4 | XFS |
//! |---|---|---|
//! | format | `mkfs-ext4` | `mkfs-xfs`: what `mkfs.xfs` 6.15 writes, 300 MB to 1 PiB |
//! | check before sealing | a real `e2fsck` | **structural**: the superblock's CRC and flags, then the whole tree walked by `fio-xfs` with every v5 checksum checked. `mkfs-xfs` has no `xfs_repair -n` yet. |
//! | write files in (seed) | `fio-ext4` | not yet: `fio-xfs` reads only |
//! | read and verify an image | `fio-ext4` | `fio-xfs` |
//!
//! **Identity.** Two XFS filesystems with one UUID cannot be mounted on one
//! host at all ("Filesystem has duplicate UUID"), so every clone is stamped.
//! On a v5 filesystem the UUID is also written into every metadata block,
//! which is why [`stamp_uuid`] does what `xfs_admin -U` does: the superblock
//! gets the new `sb_uuid`, keeps the old one in `sb_meta_uuid`, and sets the
//! `META_UUID` incompat feature. That rewrites one sector per allocation
//! group and nothing else. The offsets are `mkfs-xfs`'s own, so the format
//! has one description.

use std::sync::Arc;

use uuid::Uuid;

use mkfs_xfs::structs::sb::{off, version};

use crate::drive::BlockDevice;

/// `XFSB`.
pub const MAGIC: [u8; 4] = *b"XFSB";

/// The smallest filesystem `mkfs.xfs` (and so `mkfs-xfs`) will make.
pub const MIN_BYTES: u64 = 300 * 1000 * 1000;

/// One of the engine's volumes, as both crates' block device.
///
/// `thin`: the volume is a thin volume, so zeroing a range is a discard,
/// which is what keeps a formatted blank's allocation to its metadata rather
/// than the gigabytes of log `mkfs.xfs` zeroes.
pub struct XfsDevice {
    dev: Arc<dyn BlockDevice>,
    thin: bool,
}

impl XfsDevice {
    pub fn thin(dev: Arc<dyn BlockDevice>) -> Self {
        XfsDevice { dev, thin: true }
    }

    pub fn opaque(dev: Arc<dyn BlockDevice>) -> Self {
        XfsDevice { dev, thin: false }
    }

    async fn read_full(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let end = buf.len();
            let n = self
                .dev
                .read(offset + done as u64, &mut buf[done..end])
                .await
                .map_err(|e| std::io::Error::other(format!("read at {}: {e}", offset + done as u64)))?;
            if n == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
            }
            done += n;
        }
        Ok(())
    }

    async fn write_full(&self, offset: u64, buf: &[u8]) -> std::io::Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let n = self
                .dev
                .write(offset + done as u64, &buf[done..])
                .await
                .map_err(|e| std::io::Error::other(format!("write at {}: {e}", offset + done as u64)))?;
            if n == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
            }
            done += n;
        }
        Ok(())
    }

    async fn zero_by_writing(&self, offset: u64, len: u64) -> std::io::Result<()> {
        const CHUNK: u64 = 1 << 20;
        let zeroes = vec![0u8; CHUNK.min(len).max(1) as usize];
        let mut done = 0u64;
        while done < len {
            let n = (len - done).min(zeroes.len() as u64) as usize;
            self.write_full(offset + done, &zeroes[..n]).await?;
            done += n as u64;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl mkfs_xfs::device::BlockDevice for XfsDevice {
    fn size(&self) -> u64 {
        self.dev.capacity_bytes()
    }

    fn logical_sector_size(&self) -> u32 {
        self.dev.block_size().max(512)
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> mkfs_xfs::Result<()> {
        self.read_full(offset, buf).await.map_err(|e| mkfs_xfs::Error::io(offset, e))
    }

    async fn write_at(&self, offset: u64, buf: &[u8]) -> mkfs_xfs::Result<()> {
        self.write_full(offset, buf).await.map_err(|e| mkfs_xfs::Error::io(offset, e))
    }

    async fn flush(&self) -> mkfs_xfs::Result<()> {
        self.dev
            .flush()
            .await
            .map_err(|e| mkfs_xfs::Error::io(0, std::io::Error::other(e.to_string())))
    }

    async fn write_zeroes(&self, offset: u64, len: u64) -> mkfs_xfs::Result<()> {
        let io = |o: u64, e: std::io::Error| mkfs_xfs::Error::io(o, e);
        if !self.thin || len == 0 {
            return self.zero_by_writing(offset, len).await.map_err(|e| io(offset, e));
        }
        // Whole discard granules are discarded — a thin volume reads them
        // back as zeros — and only the ragged ends are written.
        let gran = self.dev.discard_granularity().max(1) as u64;
        let first = offset.div_ceil(gran) * gran;
        let last = (offset + len) / gran * gran;
        if last <= first {
            return self.zero_by_writing(offset, len).await.map_err(|e| io(offset, e));
        }
        self.zero_by_writing(offset, first - offset).await.map_err(|e| io(offset, e))?;
        self.dev
            .discard(first, last - first)
            .await
            .map_err(|e| io(first, std::io::Error::other(e.to_string())))?;
        self.zero_by_writing(last, offset + len - last).await.map_err(|e| io(last, e))
    }
}

#[async_trait::async_trait]
impl fio_xfs::BlockDevice for XfsDevice {
    fn size(&self) -> u64 {
        self.dev.capacity_bytes()
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> fio_xfs::Result<()> {
        self.read_full(offset, buf).await.map_err(fio_xfs::Error::Io)
    }
}

/// What to format.
#[derive(Debug, Clone)]
pub struct XfsParams {
    pub label: String,
    pub uuid: Uuid,
    /// Filesystem block size; `None` is `mkfs.xfs`'s default (4 KiB).
    pub block_size: Option<u32>,
}

impl Default for XfsParams {
    fn default() -> Self {
        XfsParams { label: String::new(), uuid: Uuid::new_v4(), block_size: None }
    }
}

/// What a format laid down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XfsReport {
    pub block_size: u32,
    pub blocks: u64,
    pub ag_count: u64,
    pub ag_blocks: u64,
    pub log_blocks: u64,
    pub free_blocks: u64,
    pub uuid: Uuid,
    pub label: String,
}

/// Format `dev` as XFS. A thin volume is assumed blank where `mkfs-xfs`
/// zeroes (it discards those ranges rather than writing them).
pub async fn format(dev: &Arc<dyn BlockDevice>, params: &XfsParams) -> anyhow::Result<XfsReport> {
    if dev.capacity_bytes() < MIN_BYTES {
        anyhow::bail!(
            "XFS needs at least 300 MB (mkfs.xfs refuses anything smaller); this volume is {} bytes",
            dev.capacity_bytes()
        );
    }
    if params.label.len() > 12 {
        anyhow::bail!("an XFS label is at most 12 bytes: {:?}", params.label);
    }
    let mut p = mkfs_xfs::geometry::Params::new().uuid(*params.uuid.as_bytes());
    if !params.label.is_empty() {
        p = p.label(params.label.clone());
    }
    if let Some(bs) = params.block_size {
        p = p.block_size(bs);
    }
    let target = XfsDevice::thin(dev.clone());
    let r = mkfs_xfs::format::format(&target, &p).await?;
    Ok(XfsReport {
        block_size: r.geometry.blocksize,
        blocks: r.geometry.dblocks,
        ag_count: r.geometry.agcount,
        ag_blocks: r.geometry.agsize,
        log_blocks: r.geometry.logblocks,
        free_blocks: r.fdblocks,
        uuid: Uuid::from_bytes(r.uuid),
        label: params.label.clone(),
    })
}

/// What the primary superblock says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XfsLayout {
    /// The UUID the filesystem answers to (`sb_uuid`).
    pub uuid: Uuid,
    /// The UUID stamped into its metadata: the same as `uuid` unless it has
    /// been changed since the format (`META_UUID`).
    pub meta_uuid: Uuid,
    pub label: String,
    /// 4 or 5. Version 5 carries a CRC in every metadata block.
    pub version: u8,
    pub block_size: u32,
    pub sector_size: u32,
    pub blocks: u64,
    pub ag_count: u32,
    pub ag_blocks: u32,
    pub log_blocks: u32,
    /// `sb_inprogress`: a format that never finished.
    pub in_progress: bool,
    /// The `NEEDSREPAIR` incompat feature: `xfs_repair` must run before a
    /// kernel will mount it.
    pub needs_repair: bool,
    pub meta_uuid_feature: bool,
}

fn be16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}
fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}
fn be64(b: &[u8], o: usize) -> u64 {
    u64::from_be_bytes(b[o..o + 8].try_into().unwrap())
}

fn uuid_at(b: &[u8], o: usize) -> Uuid {
    Uuid::from_bytes(b[o..o + 16].try_into().unwrap())
}

/// Read a superblock sector at `at`: the whole sector, so its CRC can be
/// checked and it can be written back.
async fn read_sb_sector(dev: &XfsDevice, at: u64) -> anyhow::Result<Vec<u8>> {
    let mut head = vec![0u8; 512];
    dev.read_full(at, &mut head).await?;
    if head[..4] != MAGIC {
        anyhow::bail!("no XFS superblock at byte {at}");
    }
    let sect = be16(&head, off::SECTSIZE) as usize;
    if !(512..=32768).contains(&sect) || !sect.is_power_of_two() {
        anyhow::bail!("XFS superblock at byte {at} names a {sect}-byte sector");
    }
    if sect == 512 {
        return Ok(head);
    }
    let mut buf = vec![0u8; sect];
    dev.read_full(at, &mut buf).await?;
    Ok(buf)
}

fn is_v5(sb: &[u8]) -> bool {
    be16(sb, off::VERSIONNUM) & 0xf == version::V5
}

fn layout_of(sb: &[u8]) -> anyhow::Result<XfsLayout> {
    let v5 = is_v5(sb);
    if v5 && !mkfs_xfs::crc::verify(sb, off::CRC) {
        anyhow::bail!("XFS superblock fails its CRC");
    }
    let incompat = if v5 { be32(sb, off::FEATURES_INCOMPAT) } else { 0 };
    let meta = incompat & version::IN_META_UUID != 0;
    let uuid = uuid_at(sb, off::UUID);
    let label = String::from_utf8_lossy(&sb[off::FNAME..off::FNAME + 12])
        .trim_end_matches('\0')
        .to_string();
    Ok(XfsLayout {
        uuid,
        meta_uuid: if meta { uuid_at(sb, off::META_UUID) } else { uuid },
        label,
        version: if v5 { 5 } else { 4 },
        block_size: be32(sb, off::BLOCKSIZE),
        sector_size: be16(sb, off::SECTSIZE) as u32,
        blocks: be64(sb, off::DBLOCKS),
        ag_count: be32(sb, off::AGCOUNT),
        ag_blocks: be32(sb, off::AGBLOCKS),
        log_blocks: be32(sb, off::LOGBLOCKS),
        in_progress: sb[off::INPROGRESS] != 0,
        needs_repair: incompat & version::IN_NEEDSREPAIR != 0,
        meta_uuid_feature: meta,
    })
}

/// Read the primary superblock. Fails on anything that is not XFS, and on a
/// v5 superblock whose CRC does not match.
pub async fn read_layout(dev: &Arc<dyn BlockDevice>) -> anyhow::Result<XfsLayout> {
    let d = XfsDevice::opaque(dev.clone());
    layout_of(&read_sb_sector(&d, 0).await?)
}

/// Whether the first sector says XFS: cheap, for a probe that must not pay
/// a full read on every other kind of volume.
pub async fn looks_like_xfs(dev: &Arc<dyn BlockDevice>) -> bool {
    if dev.capacity_bytes() < 512 {
        return false;
    }
    let mut b = vec![0u8; 512];
    matches!(dev.read(0, &mut b).await, Ok(n) if n == 512) && b[..4] == MAGIC
}

/// Why a filesystem must not be sealed as it stands. Empty is sealable.
pub async fn seal_blockers(dev: &Arc<dyn BlockDevice>) -> anyhow::Result<Vec<String>> {
    let l = read_layout(dev).await?;
    let mut out = Vec::new();
    if l.in_progress {
        out.push("the format never finished (sb_inprogress is set)".to_string());
    }
    if l.needs_repair {
        out.push("the filesystem is marked NEEDSREPAIR".to_string());
    }
    Ok(out)
}

/// What walking a filesystem found.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct XfsCheck {
    pub entries: u64,
    pub directories: u64,
    pub files: u64,
    pub symlinks: u64,
    pub file_bytes: u64,
}

/// Check what can be checked without `xfs_repair`: open the filesystem with
/// `fio-xfs` (superblock, CRCs) and walk every directory and inode from the
/// root, every v5 checksum on the way checked. A filesystem that walks is
/// one a consumer can read; `mkfs-xfs` gains a real checker later.
pub async fn check(dev: &Arc<dyn BlockDevice>) -> anyhow::Result<XfsCheck> {
    let vol = fio_xfs::Volume::open(XfsDevice::opaque(dev.clone())).await?;
    let mut c = XfsCheck::default();
    vol.walk_each("/", |e, _| {
        c.entries += 1;
        if e.stat.is_dir() {
            c.directories += 1;
        } else if e.stat.is_file() {
            c.files += 1;
            c.file_bytes += e.stat.size;
        } else if e.stat.is_symlink() {
            c.symlinks += 1;
        }
        Ok(())
    })
    .await?;
    Ok(c)
}

/// Read one file out of the filesystem on `dev`, if it is there.
pub async fn read_file(dev: &Arc<dyn BlockDevice>, path: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let vol = fio_xfs::Volume::open(XfsDevice::opaque(dev.clone())).await?;
    if !vol.exists(path).await? {
        return Ok(None);
    }
    Ok(Some(vol.read(path).await?))
}

/// Byte offset of each allocation group's superblock, primary last so a
/// stamp torn part-way leaves the primary — the one a mount reads — as it
/// was.
fn sb_offsets(l: &XfsLayout) -> Vec<u64> {
    let ag_bytes = l.ag_blocks as u64 * l.block_size as u64;
    let mut v: Vec<u64> = (1..l.ag_count as u64).map(|a| a * ag_bytes).collect();
    v.push(0);
    v
}

/// Rewrite every allocation group's superblock with `edit`, then flush.
async fn rewrite_superblocks(dev: &Arc<dyn BlockDevice>, edit: impl Fn(&mut Vec<u8>)) -> anyhow::Result<()> {
    let d = XfsDevice::opaque(dev.clone());
    let primary = read_sb_sector(&d, 0).await?;
    let l = layout_of(&primary)?;
    for at in sb_offsets(&l) {
        let mut sb = if at == 0 { primary.clone() } else { read_sb_sector(&d, at).await? };
        edit(&mut sb);
        if is_v5(&sb) {
            mkfs_xfs::crc::stamp(&mut sb, off::CRC);
        }
        d.write_full(at, &sb).await?;
    }
    dev.flush().await?;
    Ok(())
}

/// Give the filesystem a new UUID, as `xfs_admin -U` does.
///
/// On v5 the old UUID stays in `sb_meta_uuid` (the one the metadata blocks
/// carry) and `META_UUID` is set; a filesystem that already has one keeps its
/// metadata UUID and only `sb_uuid` changes. On v4 nothing else names the
/// UUID, so only `sb_uuid` is written.
pub async fn stamp_uuid(dev: &Arc<dyn BlockDevice>, uuid: Uuid) -> anyhow::Result<()> {
    rewrite_superblocks(dev, |sb| {
        if is_v5(sb) {
            let incompat = be32(sb, off::FEATURES_INCOMPAT);
            if incompat & version::IN_META_UUID == 0 {
                let old: [u8; 16] = sb[off::UUID..off::UUID + 16].try_into().unwrap();
                sb[off::META_UUID..off::META_UUID + 16].copy_from_slice(&old);
                sb[off::FEATURES_INCOMPAT..off::FEATURES_INCOMPAT + 4]
                    .copy_from_slice(&(incompat | version::IN_META_UUID).to_be_bytes());
            }
        }
        sb[off::UUID..off::UUID + 16].copy_from_slice(uuid.as_bytes());
    })
    .await
}

/// Set the label (`sb_fname`, 12 bytes) in every superblock.
pub async fn stamp_label(dev: &Arc<dyn BlockDevice>, label: &str) -> anyhow::Result<()> {
    if label.len() > 12 {
        anyhow::bail!("an XFS label is at most 12 bytes: {label:?}");
    }
    let mut name = [0u8; 12];
    name[..label.len()].copy_from_slice(label.as_bytes());
    rewrite_superblocks(dev, |sb| sb[off::FNAME..off::FNAME + 12].copy_from_slice(&name)).await
}

/// The volume record's description of an XFS filesystem.
pub fn fs_info(l: &XfsLayout) -> crate::volume::FsInfo {
    crate::volume::FsInfo {
        kind: "xfs".into(),
        // XFS always has a log.
        journal: true,
        features: None,
        sixty_four_bit: true,
        metadata_csum: l.version == 5,
        // What keeps a stamp to one superblock per AG.
        csum_seed: l.meta_uuid_feature,
        label: l.label.clone(),
        uuid: Some(l.uuid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;

    async fn dev(bytes: u64) -> (Arc<dyn BlockDevice>, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("stormblock-xfs-{}.img", Uuid::new_v4().simple()));
        let d = FileDevice::open_with_capacity(path.to_str().unwrap(), bytes).await.unwrap();
        (Arc::new(d), path)
    }

    /// Format, read back, walk; restamp and relabel and read back again.
    #[tokio::test]
    async fn format_read_stamp_and_walk() {
        let (d, path) = dev(512 << 20).await;
        let uuid = Uuid::new_v4();
        let r = format(&d, &XfsParams { label: "data".into(), uuid, block_size: None }).await.unwrap();
        assert_eq!(r.uuid, uuid);
        assert!(r.ag_count >= 1);
        let l = read_layout(&d).await.unwrap();
        assert_eq!((l.uuid, l.meta_uuid, l.label.as_str(), l.version), (uuid, uuid, "data", 5));
        assert!(!l.in_progress && !l.needs_repair && !l.meta_uuid_feature);
        assert!(seal_blockers(&d).await.unwrap().is_empty());
        let c = check(&d).await.unwrap();
        assert_eq!(c.entries, 0, "a blank has an empty root: {c:?}");

        let fresh = Uuid::new_v4();
        stamp_uuid(&d, fresh).await.unwrap();
        stamp_label(&d, "claim-1").await.unwrap();
        let l2 = read_layout(&d).await.unwrap();
        assert_eq!(l2.uuid, fresh);
        assert_eq!(l2.meta_uuid, uuid, "the metadata keeps the UUID it was formatted with");
        assert!(l2.meta_uuid_feature);
        assert_eq!(l2.label, "claim-1");
        // Every secondary superblock agrees with the primary.
        let x = XfsDevice::opaque(d.clone());
        for at in sb_offsets(&l2) {
            let sb = read_sb_sector(&x, at).await.unwrap();
            let s = layout_of(&sb).unwrap();
            assert_eq!((s.uuid, s.meta_uuid, s.label.as_str()), (fresh, uuid, "claim-1"), "superblock at {at}");
        }
        // Stamping again keeps the original metadata UUID.
        let third = Uuid::new_v4();
        stamp_uuid(&d, third).await.unwrap();
        let l3 = read_layout(&d).await.unwrap();
        assert_eq!((l3.uuid, l3.meta_uuid), (third, uuid));
        // And fio-xfs still opens and walks it.
        check(&d).await.unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn too_small_and_not_xfs_are_refused() {
        let (d, path) = dev(64 << 20).await;
        assert!(format(&d, &XfsParams::default()).await.unwrap_err().to_string().contains("300 MB"));
        assert!(read_layout(&d).await.is_err());
        assert!(!looks_like_xfs(&d).await);
        let _ = std::fs::remove_file(path);
    }
}
