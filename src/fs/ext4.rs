//! ext4 — creating, checking and re-identifying filesystems on a thin volume.
//!
//! The on-disk format itself lives in [`mkfs_ext4`], a from-scratch async
//! reimplementation of `mke2fs` and `e2fsck` written against the e2fsprogs
//! source. This module is the seam between it and the engine:
//!
//! - [`VolumeDevice`] adapts a stormblock `BlockDevice` to the one that crate
//!   formats through, so a volume is formatted **in place** — no loopback, no
//!   `/dev` node, no `mkfs.ext4` subprocess, and no network round trip.
//! - [`format`] lays down a blank filesystem for a template.
//! - [`check`] runs a real fsck, which is what a seal guard needs: a
//!   superblock that merely *says* it is clean is not evidence that it is.
//! - [`stamp_uuid`] gives a copy-on-write clone its own identity.
//!
//! # Why identity is cheap here
//!
//! The default profile carries `metadata_csum`, which seeds every checksum —
//! group descriptors, bitmaps, inodes, directory blocks — from the filesystem
//! UUID. Changing the UUID would invalidate all of them, which is why it also
//! carries `metadata_csum_seed`: the seed is then stored in `s_checksum_seed`
//! and a new UUID leaves every other checksum alone. Stamping a clone stays
//! one superblock write (stormblockmk#12).
//!
//! Filesystems that predate that (an externally formatted template, say) are
//! given a seed derived from their current UUID before the stamp, which is
//! what `tune2fs -U` does for the same reason.

use std::sync::Arc;

use uuid::Uuid;

use mkfs_ext4::features::{CompatFeatures, IncompatFeatures, RoCompatFeatures};
use mkfs_ext4::fs::Filesystem;
use mkfs_ext4::fsck::{self, FsckOptions, Severity};
use mkfs_ext4::params::Params;
use mkfs_ext4::structs::superblock::state;
use mkfs_ext4::{Error as Ext4Error, Superblock, SUPERBLOCK_LEN, SUPERBLOCK_OFFSET};

use crate::drive::BlockDevice;

pub use mkfs_ext4::params::Profile as FsProfile;

// ---------------------------------------------------------------------------
// The device seam
// ---------------------------------------------------------------------------

/// A stormblock volume, as something a filesystem can be written onto.
///
/// The formatter fans out across block groups and calls `write_at` from many
/// tasks at once; thin volumes serialise only where a mapping changes, so
/// disjoint ranges proceed concurrently, which is exactly the contract.
pub struct VolumeDevice {
    dev: Arc<dyn BlockDevice>,
    /// Trust that unwritten regions read back as zeros, and satisfy
    /// `write_zeroes` by discarding instead of writing.
    thin: bool,
}

impl VolumeDevice {
    /// Wrap a volume whose unwritten extents read as zeros — a freshly
    /// created thin volume. Zeroing is then a discard, which is what keeps a
    /// template's allocation in kilobytes rather than the tens of megabytes
    /// its inode tables describe.
    pub fn thin(dev: Arc<dyn BlockDevice>) -> Self {
        VolumeDevice { dev, thin: true }
    }

    /// Wrap a device that may hold old data, so zeroing writes real zeros.
    pub fn opaque(dev: Arc<dyn BlockDevice>) -> Self {
        VolumeDevice { dev, thin: false }
    }

    fn io(offset: u64, e: crate::drive::DriveError) -> Ext4Error {
        Ext4Error::Io {
            offset,
            source: std::io::Error::other(format!("{e}")),
        }
    }
}

#[async_trait::async_trait]
impl mkfs_ext4::BlockDevice for VolumeDevice {
    fn size(&self) -> u64 {
        self.dev.capacity_bytes()
    }

    /// The volume's logical sector, which the formatter takes as the floor for
    /// the filesystem's block size.
    ///
    /// Without this the size classes decide alone: a 256 MiB volume gets 1 KiB
    /// blocks, which `e2fsck` is perfectly happy with and the kernel refuses to
    /// mount on a 4 KiB-sector LUN — `EXT4-fs (sdb): bad block size 1024`.
    fn logical_sector_size(&self) -> u32 {
        self.dev.block_size().max(512)
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> mkfs_ext4::Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let end = buf.len();
            // Thin volumes return short reads at extent boundaries.
            let n = self
                .dev
                .read(offset + done as u64, &mut buf[done..end])
                .await
                .map_err(|e| Self::io(offset + done as u64, e))?;
            if n == 0 {
                return Err(Ext4Error::Io {
                    offset: offset + done as u64,
                    source: std::io::Error::from(std::io::ErrorKind::UnexpectedEof),
                });
            }
            done += n;
        }
        Ok(())
    }

    async fn write_at(&self, offset: u64, buf: &[u8]) -> mkfs_ext4::Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let n = self
                .dev
                .write(offset + done as u64, &buf[done..])
                .await
                .map_err(|e| Self::io(offset + done as u64, e))?;
            if n == 0 {
                return Err(Ext4Error::Io {
                    offset: offset + done as u64,
                    source: std::io::Error::from(std::io::ErrorKind::WriteZero),
                });
            }
            done += n;
        }
        Ok(())
    }

    async fn flush(&self) -> mkfs_ext4::Result<()> {
        self.dev.flush().await.map_err(|e| Self::io(0, e))
    }

    /// On a thin volume, zeroing a range is discarding it: an extent nobody
    /// maps reads back as zeros and costs nothing. Only whole slots can be
    /// released, so the unaligned edges are still written.
    async fn write_zeroes(&self, offset: u64, len: u64) -> mkfs_ext4::Result<()> {
        if !self.thin || len == 0 {
            return default_write_zeroes(self, offset, len).await;
        }
        let gran = self.dev.discard_granularity().max(1) as u64;
        let first = offset.div_ceil(gran) * gran;
        let last = (offset + len) / gran * gran;
        if last <= first {
            return default_write_zeroes(self, offset, len).await;
        }
        default_write_zeroes(self, offset, first - offset).await?;
        self.dev
            .discard(first, last - first)
            .await
            .map_err(|e| Self::io(first, e))?;
        default_write_zeroes(self, last, offset + len - last).await
    }
}

/// Write real zeros, in chunks.
async fn default_write_zeroes(
    dev: &VolumeDevice,
    offset: u64,
    len: u64,
) -> mkfs_ext4::Result<()> {
    use mkfs_ext4::BlockDevice as _;
    const CHUNK: usize = 1 << 20;
    if len == 0 {
        return Ok(());
    }
    let zeroes = vec![0u8; CHUNK.min(len as usize)];
    let mut written = 0u64;
    while written < len {
        let n = ((len - written) as usize).min(zeroes.len());
        dev.write_at(offset + written, &zeroes[..n]).await?;
        written += n as u64;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// How to lay down a filesystem.
///
/// The knobs are `mke2fs`'s, because that is the vocabulary every consumer
/// already speaks — a profile plus an `-O` feature list.
#[derive(Debug, Clone)]
pub struct Ext4Params {
    /// ext2, ext3 or ext4. **ext4 by default**, which is what `mke2fs -t ext4`
    /// and RouterOS's own `format-drive` produce: journal, extents,
    /// `flex_bg`, `64bit`, `metadata_csum` and `metadata_csum_seed`.
    pub profile: FsProfile,
    /// Volume label (16 bytes on disk, truncated).
    pub label: String,
    /// Filesystem UUID. Templates get one here; clones get a fresh one at
    /// clone time via [`stamp_uuid`].
    pub uuid: Uuid,
    /// Lay down a journal. `None` follows the profile — ext4 and ext3 have
    /// one, ext2 does not.
    ///
    /// Turning it off is a real choice for a consumer that cannot replay one:
    /// a journal that ever goes dirty on RouterOS leaves the filesystem
    /// read-only for good, while a Linux host or VM wants the crash
    /// consistency.
    pub journal: Option<bool>,
    /// A `mke2fs -O`-style list applied over the profile, e.g. `"^64bit"` or
    /// `"^metadata_csum,^flex_bg"`.
    pub features: Option<String>,
    /// One inode per this many bytes (`mke2fs -i`). `None` uses the default
    /// for the filesystem size.
    pub bytes_per_inode: Option<u32>,
    /// Percentage of blocks reserved for root.
    pub reserved_percent: f64,
    /// Filesystem block size. `None` lets the formatter decide: the size
    /// classes as `mke2fs` computes them, with the volume's logical sector as
    /// the floor (see [`VolumeDevice::logical_sector_size`]).
    pub block_size: Option<u32>,
    /// Trust that the target reads back as zeros, so zeroing is a discard.
    /// True for a freshly created thin volume.
    pub assume_blank: bool,
}

impl Default for Ext4Params {
    fn default() -> Self {
        Ext4Params {
            profile: FsProfile::Ext4,
            label: String::new(),
            uuid: Uuid::new_v4(),
            journal: None,
            features: None,
            bytes_per_inode: None,
            reserved_percent: 5.0,
            block_size: None,
            assume_blank: true,
        }
    }
}

impl Ext4Params {
    fn build(&self) -> Params {
        let mut p = Params::new(self.profile)
            .label(self.label.clone())
            .uuid(*self.uuid.as_bytes())
            .reserved_percent(self.reserved_percent);
        if let Some(spec) = &self.features {
            p = p.features(spec.clone());
        }
        if let Some(ratio) = self.bytes_per_inode {
            p = p.inode_ratio(ratio);
        }
        if let Some(bs) = self.block_size {
            p = p.block_size(bs);
        }
        if self.journal == Some(false) {
            p = p.no_journal();
        }
        p
    }
}

/// What a format produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ext4Report {
    pub block_size: u32,
    pub blocks: u64,
    pub inodes: u32,
    pub free_blocks: u64,
    pub group_count: u32,
    pub journal_blocks: u32,
    pub uuid: Uuid,
    pub label: String,
}

/// Volumes this size and under get no journal unless one is asked for.
///
/// 8 MiB is exactly where `mkfs-ext4`'s size class starts adding its 4 MB
/// journal, and it is the one size at which doing so makes a volume hold
/// *less* than a smaller one: 3.3 MB usable against 6.4 MB at 7 MB. Measured
/// in `tests/small_volumes.rs`.
///
/// Below 8 MiB this changes nothing any more — `mkfs-ext4` v2.0.4 declines the
/// journal there itself, and clears the feature with it (mkfs.ext4.rs#3).
/// The rule exists for the cliff at 8 MiB alone.
pub const JOURNAL_FLOOR_BYTES: u64 = 8 * 1024 * 1024;

/// Write a blank filesystem over the whole device.
pub async fn format(dev: &Arc<dyn BlockDevice>, params: &Ext4Params) -> anyhow::Result<Ext4Report> {
    let target = if params.assume_blank {
        VolumeDevice::thin(dev.clone())
    } else {
        VolumeDevice::opaque(dev.clone())
    };

    // At the floor, the default journal is worse than none: measured in
    // `tests/small_volumes.rs`, an 8 MB volume with the default 4 MB journal
    // has 3.3 MB usable, while a 7 MB one with no journal has 6.4 MB — the
    // larger volume holds less. `mkfs-ext4` turns the journal on from 2048
    // blocks up, which lands exactly on the wrong side of that. Above the
    // floor it amortises (32% at 16 MB, 13% at 64 MB) and is kept, because a
    // consumer with no clean unmount needs it.
    //
    // Only when the caller has not decided: an explicit `journal` is obeyed.
    let mut params = params.clone();
    if params.journal.is_none() && dev.capacity_bytes() <= JOURNAL_FLOOR_BYTES {
        params.journal = Some(false);
    }
    let params = &params;

    let report = mkfs_ext4::format::format(&target, &params.build())
        .await
        .map_err(|e| anyhow::anyhow!("formatting {}: {e}", params.profile.name()))?;

    tracing::info!(
        profile = params.profile.name(),
        blocks = report.blocks_count,
        inodes = report.inodes_count,
        groups = report.group_count,
        journal = report.journal_blocks,
        label = %params.label,
        "filesystem written"
    );

    Ok(Ext4Report {
        block_size: report.block_size,
        blocks: report.blocks_count,
        inodes: report.inodes_count,
        free_blocks: report.free_blocks_count,
        group_count: report.group_count,
        journal_blocks: report.journal_blocks,
        uuid: Uuid::from_bytes(report.uuid),
        label: report.label,
    })
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// What the engine reads off a superblock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ext4Layout {
    pub block_size: u64,
    pub blocks_count: u64,
    pub inodes_count: u32,
    pub group_count: u64,
    pub state: u16,
    pub uuid: Uuid,
    pub label: String,
    pub has_journal: bool,
    pub needs_recovery: bool,
    pub metadata_csum: bool,
    pub csum_seed: bool,
    pub sixty_four_bit: bool,
    /// Cleanly unmounted, nothing pending.
    pub clean: bool,
}

impl Ext4Layout {
    pub fn size_bytes(&self) -> u64 {
        self.blocks_count * self.block_size
    }

    /// The `mke2fs -t` name this filesystem would answer to.
    pub fn profile_name(&self) -> &'static str {
        if self.metadata_csum || self.sixty_four_bit {
            "ext4"
        } else if self.has_journal {
            "ext3"
        } else {
            "ext2"
        }
    }
}

fn layout_of(sb: &Superblock) -> Ext4Layout {
    let has_journal = sb.feature_compat.contains(CompatFeatures::HAS_JOURNAL);
    let needs_recovery = sb.feature_incompat.contains(IncompatFeatures::RECOVER);
    let st = sb.state;
    Ext4Layout {
        block_size: sb.block_size() as u64,
        blocks_count: sb.blocks_count,
        inodes_count: sb.inodes_count,
        group_count: sb.group_count() as u64,
        state: st,
        uuid: Uuid::from_bytes(sb.uuid),
        label: sb.label(),
        has_journal,
        needs_recovery,
        metadata_csum: sb
            .feature_ro_compat
            .contains(RoCompatFeatures::METADATA_CSUM),
        csum_seed: sb.feature_incompat.contains(IncompatFeatures::CSUM_SEED),
        sixty_four_bit: sb
            .feature_incompat
            .contains(IncompatFeatures::SIXTY_FOUR_BIT),
        clean: st & state::VALID_FS != 0
            && st & state::ERROR_FS == 0
            && st & state::ORPHAN_FS == 0
            && !needs_recovery,
    }
}

/// Read and parse the primary superblock.
pub async fn read_layout(dev: &Arc<dyn BlockDevice>) -> anyhow::Result<Ext4Layout> {
    let target = VolumeDevice::opaque(dev.clone());
    let sb = read_superblock(&target).await?;
    Ok(layout_of(&sb))
}

async fn read_superblock(target: &VolumeDevice) -> anyhow::Result<Superblock> {
    use mkfs_ext4::BlockDevice as _;
    let mut buf = vec![0u8; SUPERBLOCK_LEN];
    target
        .read_at(SUPERBLOCK_OFFSET, &mut buf)
        .await
        .map_err(|e| anyhow::anyhow!("reading superblock: {e}"))?;
    let sb = Superblock::decode(&buf)
        .map_err(|e| anyhow::anyhow!("no usable filesystem on this volume: {e}"))?;
    Ok(sb)
}

// ---------------------------------------------------------------------------
// The seal guard
// ---------------------------------------------------------------------------

/// Why a filesystem must not be sealed as a template.
///
/// A sealed template is the ancestor of every clone that follows, so this
/// checks the filesystem itself rather than taking the superblock's word for
/// it. A template that seals dirty does not fail at seal time — it fails
/// later, inside a container, as `Read-only file system`
/// (stormblock-registry#10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SealBlocker {
    /// `VALID_FS` clear — never cleanly unmounted.
    NotCleanlyUnmounted,
    /// `ERROR_FS` set — the kernel recorded an error against this filesystem.
    ErrorFlagSet,
    /// `RECOVER` set — a journal replay is pending, which a consumer that
    /// cannot replay one mounts read-only.
    RecoveryPending,
    /// `ORPHAN_FS` set — orphan inode cleanup is pending.
    OrphanCleanupPending,
    /// fsck found something structurally wrong.
    FsckProblem { code: &'static str, message: String },
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
            SealBlocker::RecoveryPending => write!(
                f,
                "journal replay is pending (RECOVER set) — a consumer that cannot replay mounts read-only"
            ),
            SealBlocker::OrphanCleanupPending => {
                write!(f, "orphan inode cleanup is pending (ORPHAN_FS set)")
            }
            SealBlocker::FsckProblem { code, message } => write!(f, "fsck: {code}: {message}"),
        }
    }
}

/// Superblock flags plus a full fsck pass. Empty means sealable.
///
/// The fsck is the part that earns its keep: `VALID_FS` is a claim the last
/// writer made about itself, while a check walks the inodes, bitmaps and
/// directory tree and compares them against what the counters say.
pub async fn seal_blockers(dev: &Arc<dyn BlockDevice>) -> anyhow::Result<Vec<SealBlocker>> {
    let target = VolumeDevice::opaque(dev.clone());
    let sb = read_superblock(&target).await?;
    let layout = layout_of(&sb);

    let mut out = Vec::new();
    if layout.state & state::VALID_FS == 0 {
        out.push(SealBlocker::NotCleanlyUnmounted);
    }
    if layout.state & state::ERROR_FS != 0 {
        out.push(SealBlocker::ErrorFlagSet);
    }
    if layout.needs_recovery {
        out.push(SealBlocker::RecoveryPending);
    }
    if layout.state & state::ORPHAN_FS != 0 {
        out.push(SealBlocker::OrphanCleanupPending);
    }

    // `force` so the check runs even on a filesystem that calls itself clean:
    // "clean" is the claim under examination.
    let report = fsck::check(
        VolumeDevice::opaque(dev.clone()),
        &FsckOptions {
            repair: false,
            force: true,
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!("checking the filesystem: {e}"))?;
    for p in report.problems.iter().filter(|p| p.severity > Severity::Info) {
        out.push(SealBlocker::FsckProblem {
            code: p.code,
            message: p.message.clone(),
        });
    }

    Ok(out)
}

/// Run a check over a volume, reporting what it found.
pub async fn check(dev: &Arc<dyn BlockDevice>) -> anyhow::Result<fsck::FsckReport> {
    fsck::check(
        VolumeDevice::opaque(dev.clone()),
        &FsckOptions {
            repair: false,
            force: true,
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!("checking the filesystem: {e}"))
}

/// Check and correct what can be corrected.
///
/// RouterOS has no fsck and cannot cleanly unmount a network disk, so the
/// engine is the only place a volume it left dirty can be repaired.
pub async fn repair(dev: &Arc<dyn BlockDevice>) -> anyhow::Result<fsck::FsckReport> {
    fsck::check(VolumeDevice::opaque(dev.clone()), &FsckOptions::repair())
        .await
        .map_err(|e| anyhow::anyhow!("repairing the filesystem: {e}"))
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// Give a filesystem a new UUID.
///
/// This is the piece that can only live in the engine: every consumer clones
/// *through* it, so a UUID stamped anywhere else leaves the clones that
/// consumer never touches sharing identity (stormblockmk#12).
///
/// With `metadata_csum_seed` — which the ext4 profile sets — the checksum seed
/// is stored in the superblock, so a new UUID invalidates nothing and this is
/// a single superblock write. On a filesystem that has `metadata_csum` without
/// the seed, the current UUID's seed is pinned into `s_checksum_seed` first
/// (what `tune2fs -U` does), so the existing checksums stay valid.
pub async fn stamp_uuid(
    dev: &Arc<dyn BlockDevice>,
    uuid: Uuid,
    backups: bool,
) -> anyhow::Result<Ext4Layout> {
    let target = VolumeDevice::opaque(dev.clone());
    let mut fs = Filesystem::open(target)
        .await
        .map_err(|e| anyhow::anyhow!("opening the filesystem to stamp its UUID: {e}"))?;

    let old = fs.superblock().uuid;
    let has_csum = fs
        .superblock()
        .feature_ro_compat
        .contains(RoCompatFeatures::METADATA_CSUM);
    let has_seed = fs
        .superblock()
        .feature_incompat
        .contains(IncompatFeatures::CSUM_SEED);

    if has_csum && !has_seed {
        // Pin the seed the existing checksums were computed with, then declare
        // it, so changing the UUID below leaves every one of them valid.
        let seed = mkfs_ext4::csum::seed_from_uuid(&old);
        let sb = fs.superblock_mut();
        sb.checksum_seed = seed;
        sb.feature_incompat |= IncompatFeatures::CSUM_SEED;
    }

    fs.superblock_mut().uuid = *uuid.as_bytes();
    fs.flush_superblock()
        .await
        .map_err(|e| anyhow::anyhow!("writing the stamped superblock: {e}"))?;

    if backups {
        write_backup_superblocks(&fs)
            .await
            .map_err(|e| anyhow::anyhow!("writing stamped backup superblocks: {e}"))?;
    }

    let layout = layout_of(fs.superblock());
    drop(fs);
    dev.flush()
        .await
        .map_err(|e| anyhow::anyhow!("flushing after stamp: {e}"))?;
    Ok(layout)
}

/// Copy the primary superblock out to every backup that carries one.
///
/// Each backup names its own group in `s_block_group_nr`, which is how a
/// recovery tool tells a backup from the primary.
async fn write_backup_superblocks(
    fs: &Filesystem<VolumeDevice>,
) -> mkfs_ext4::Result<()> {
    use mkfs_ext4::BlockDevice as _;
    let sb = fs.superblock();
    let block_size = sb.block_size() as u64;
    let blocks_per_group = sb.blocks_per_group as u64;
    let first_data_block = sb.first_data_block as u64;

    for group in 1..sb.group_count() {
        if !fs.group_has_super(group) {
            continue;
        }
        let mut copy = sb.clone();
        copy.block_group_nr = group as u16;
        let at = (first_data_block + group as u64 * blocks_per_group) * block_size;
        fs.device().write_at(at, &copy.encode()).await?;
    }
    Ok(())
}

/// Give a filesystem a new label.
pub async fn stamp_label(dev: &Arc<dyn BlockDevice>, label: &str) -> anyhow::Result<()> {
    let target = VolumeDevice::opaque(dev.clone());
    let mut fs = Filesystem::open(target)
        .await
        .map_err(|e| anyhow::anyhow!("opening the filesystem to set its label: {e}"))?;
    let sb = fs.superblock_mut();
    sb.volume_name = [0u8; 16];
    let bytes = label.as_bytes();
    let n = bytes.len().min(16);
    sb.volume_name[..n].copy_from_slice(&bytes[..n]);
    fs.flush_superblock()
        .await
        .map_err(|e| anyhow::anyhow!("writing the labelled superblock: {e}"))?;
    drop(fs);
    dev.flush()
        .await
        .map_err(|e| anyhow::anyhow!("flushing after label stamp: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

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

    #[tokio::test]
    async fn ext4_is_the_default_and_it_checks_clean() {
        let (dev, path) = scratch("default", 256 * 1024 * 1024).await;
        let params = Ext4Params {
            label: "storm".to_string(),
            ..Default::default()
        };
        let report = format(&dev, &params).await.unwrap();
        // Block size follows mke2fs's size classes rather than being fixed —
        // 1 KiB blocks under 512 MiB — so the filesystem is measured in bytes.
        assert_eq!(report.blocks * report.block_size as u64, 256 * 1024 * 1024);
        assert_eq!(report.label, "storm");

        let l = read_layout(&dev).await.unwrap();
        assert_eq!(l.profile_name(), "ext4");
        assert_eq!(l.uuid, params.uuid);
        assert!(l.clean);
        // The RouterOS-native shape: what its own format-drive produces (#39).
        assert!(l.has_journal, "ext4 carries a journal");
        assert!(l.metadata_csum, "ext4 carries metadata_csum");
        assert!(l.sixty_four_bit, "ext4 carries 64bit");
        assert!(l.csum_seed, "and the seed that keeps a UUID stamp cheap");
        assert!(!l.needs_recovery);

        // A real check, not the superblock's opinion of itself.
        let fsck = check(&dev).await.unwrap();
        assert!(fsck.is_clean(), "fresh filesystem: {:?}", fsck.problems);
        assert!(seal_blockers(&dev).await.unwrap().is_empty());

        let _ = std::fs::remove_file(path);
    }

    /// The kernel refuses a filesystem whose blocks are smaller than the
    /// device's sectors, and a 256 MiB volume is exactly the size class that
    /// would otherwise pick 1 KiB blocks. Passed e2fsck, failed to mount:
    /// `EXT4-fs (sdb): bad block size 1024`.
    #[tokio::test]
    async fn blocks_are_never_smaller_than_the_volume_sectors() {
        let (dev, path) = scratch("sectors", 256 * 1024 * 1024).await;
        let sector = dev.block_size();
        let report = format(&dev, &Ext4Params::default()).await.unwrap();
        assert!(
            report.block_size >= sector,
            "{}-byte blocks on a {sector}-byte-sector volume will not mount",
            report.block_size
        );
        assert_eq!(report.blocks * report.block_size as u64, 256 * 1024 * 1024);
        assert!(check(&dev).await.unwrap().is_clean());
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn journal_and_features_are_per_filesystem_choices() {
        // The RouterOS shape: no journal it cannot replay, and none of the
        // features that came later.
        let (dev, path) = scratch("nojournal", 256 * 1024 * 1024).await;
        let params = Ext4Params {
            journal: Some(false),
            features: Some("^64bit,^metadata_csum,^flex_bg".to_string()),
            ..Default::default()
        };
        format(&dev, &params).await.unwrap();
        let l = read_layout(&dev).await.unwrap();
        assert!(!l.has_journal);
        assert!(!l.metadata_csum);
        assert!(!l.sixty_four_bit);
        assert!(l.clean);
        assert!(check(&dev).await.unwrap().is_clean());
        let _ = std::fs::remove_file(path);

        // ext2 and ext3 are the same formatter with different features.
        for (profile, journal) in [(FsProfile::Ext2, false), (FsProfile::Ext3, true)] {
            let (dev, path) = scratch(profile.name(), 64 * 1024 * 1024).await;
            format(
                &dev,
                &Ext4Params {
                    profile,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            let l = read_layout(&dev).await.unwrap();
            assert_eq!(l.has_journal, journal, "{}", profile.name());
            assert!(check(&dev).await.unwrap().is_clean(), "{}", profile.name());
            let _ = std::fs::remove_file(path);
        }
    }

    #[tokio::test]
    async fn stamping_a_uuid_leaves_every_checksum_valid() {
        let (dev, path) = scratch("stamp", 256 * 1024 * 1024).await;
        let params = Ext4Params {
            label: "golden".to_string(),
            ..Default::default()
        };
        format(&dev, &params).await.unwrap();

        let fresh = Uuid::new_v4();
        let after = stamp_uuid(&dev, fresh, true).await.unwrap();
        assert_eq!(after.uuid, fresh);

        let l = read_layout(&dev).await.unwrap();
        assert_eq!(l.uuid, fresh);
        assert_eq!(l.label, "golden", "a UUID stamp must not disturb the label");
        assert!(l.clean);

        // The point of the seed: metadata_csum is on, the UUID changed, and
        // fsck still finds every checksum correct.
        assert!(l.metadata_csum);
        let fsck = check(&dev).await.unwrap();
        assert!(
            fsck.is_clean(),
            "checksums invalidated by the stamp: {:?}",
            fsck.problems
        );

        let _ = std::fs::remove_file(path);
    }

    /// A filesystem formatted without the seed still has to be re-identifiable
    /// — the seed gets pinned from its current UUID first.
    #[tokio::test]
    async fn stamping_pins_a_seed_when_the_filesystem_lacks_one() {
        let (dev, path) = scratch("noseed", 256 * 1024 * 1024).await;
        format(
            &dev,
            &Ext4Params {
                features: Some("^metadata_csum_seed".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let before = read_layout(&dev).await.unwrap();
        assert!(before.metadata_csum && !before.csum_seed);

        let fresh = Uuid::new_v4();
        stamp_uuid(&dev, fresh, false).await.unwrap();

        let after = read_layout(&dev).await.unwrap();
        assert_eq!(after.uuid, fresh);
        assert!(after.csum_seed, "the seed must be pinned, not left to drift");
        let fsck = check(&dev).await.unwrap();
        assert!(fsck.is_clean(), "{:?}", fsck.problems);

        let _ = std::fs::remove_file(path);
    }

    /// stormblock-registry#10: the guard has to catch a filesystem a consumer
    /// would end up mounting read-only.
    #[tokio::test]
    async fn seal_guard_catches_a_dirty_superblock() {
        let (dev, path) = scratch("dirty", 128 * 1024 * 1024).await;
        format(&dev, &Ext4Params::default()).await.unwrap();
        assert!(seal_blockers(&dev).await.unwrap().is_empty());

        // What an unclean unmount leaves behind.
        {
            let target = VolumeDevice::opaque(dev.clone());
            let mut fs = Filesystem::open(target).await.unwrap();
            let sb = fs.superblock_mut();
            sb.state = state::VALID_FS | state::ERROR_FS;
            sb.feature_incompat |= IncompatFeatures::RECOVER;
            fs.flush_superblock().await.unwrap();
        }

        let blockers = seal_blockers(&dev).await.unwrap();
        assert!(blockers.contains(&SealBlocker::ErrorFlagSet), "{blockers:?}");
        assert!(blockers.contains(&SealBlocker::RecoveryPending), "{blockers:?}");
        assert!(!read_layout(&dev).await.unwrap().clean);

        let _ = std::fs::remove_file(path);
    }

    /// A blank thin volume must not pay for the metadata it merely describes.
    #[tokio::test]
    async fn formatting_a_blank_volume_stays_sparse() {
        let (dev, path) = scratch("sparse", 1024 * 1024 * 1024).await;
        format(&dev, &Ext4Params::default()).await.unwrap();
        let used = std::fs::metadata(&path).map(|m| m.blocks() * 512).unwrap_or(u64::MAX);
        assert!(
            used < 128 * 1024 * 1024,
            "a 1 GiB filesystem materialised {used} bytes"
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn nonsense_is_reported_rather_than_parsed() {
        let (dev, path) = scratch("blank", 64 * 1024 * 1024).await;
        assert!(read_layout(&dev).await.is_err(), "zeros are not a filesystem");
        assert!(seal_blockers(&dev).await.is_err());
        let _ = std::fs::remove_file(path);
    }
}
