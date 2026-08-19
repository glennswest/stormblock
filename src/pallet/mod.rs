//! Pallets — the unit of atomic replacement, and the unit that gets signed.
//!
//! > A **pallet** is a GPT partition containing a named, versioned,
//! > self-contained set of sealed member images and the manifest that
//! > describes them.
//!
//! The format is specified in `docs/pallets.md` in this repository, and its
//! decode side is `crates/pallet-format` — `no_std`, no allocation, no write
//! path — which firmware links too, so there is one reader rather than two
//! that must stay bit-compatible forever (#53). This module is the
//! **producer**: it writes what that reader consumes, and owns the lifecycle
//! around it (#51, #52).
//!
//! # Why a partition
//!
//! Three properties follow from "partition", and each of them is load-bearing:
//!
//! 1. **Firmware can see it** without a driver — but only if it never aliases
//!    another entry, which is why [`gpt::Gpt::allocate`] refuses overlap
//!    rather than trusting the caller.
//! 2. **It is relocatable.** Everything inside a pallet is addressed relative
//!    to the partition start, so a pallet is byte-for-byte copyable to another
//!    disk, image or ISO. Assembling a bootable image is then concatenating
//!    pallets and writing a GPT — no offsets to rewrite.
//! 3. **GPT already carries the state.** Priority, tries and the successful
//!    bit live in attribute bits 48–63, so boot selection is readable before
//!    any filesystem exists, and **activation is an attribute write** rather
//!    than a data write.
//!
//! # Many pallets, many drives
//!
//! A drive holds as many pallets as fit — an upgrade is a *new* partition
//! beside the one running, which is what makes fallback structural rather
//! than aspirational — and a node's pallets are spread over every drive it
//! has. Neither is configured: [`store::PalletStore`] finds them by scanning
//! the GPT of each drive it is given.
//!
//! # Ordering, and what a torn publish leaves behind
//!
//! [`manager::PalletManager::publish`] writes member content first and the
//! superblock last. A publish interrupted anywhere before that last write
//! leaves a partition whose superblock is absent or fails its own CRC, which
//! a consumer refuses — never one that describes content it does not have.

pub mod format;
pub mod gpt;
pub mod manager;
pub mod select;
pub mod store;

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::drive::{BlockDevice, DriveError};

pub use format::{
    parse_member_kind, parse_pallet_kind, Attributes, BuiltPallet, MemberExt, MemberKind,
    MemberSpec, Pallet, PalletBuilder, PalletKind, Placement, FLAG_DIGEST, FLAG_READ_ONLY,
    FLAG_SEALED, PALLET_TYPE_GUID,
};
pub use gpt::{Gpt, GptEntry};
pub use manager::{
    ConvertOptions, ConvertReport, MemberVerdict, PalletManager, PalletMemberContent, PalletStatus,
    PublishSpec, RecomposeSpec, VerifyReport,
};
pub use select::{Candidate, PalletBrowser};
pub use store::{DriveRef, PalletLocation, PalletState, PalletStore};

/// Errors from the pallet layer.
#[derive(Debug)]
pub enum PalletError {
    Io(std::io::Error),
    Drive(DriveError),
    /// The superblock does not start with `STORMPAL`.
    BadMagic,
    /// A newer on-disk version. Refused with a diagnostic, never guessed at.
    UnsupportedVersion(u32),
    /// Geometry a v1 reader cannot use: bad block size, or a table stride
    /// that is not what this version fixes it at.
    BadGeometry(String),
    BadHeaderCrc,
    /// A GPT partition entry array that does not match its own CRC.
    BadEntryCrc,
    BadMemberCrc,
    BadExtentCrc,
    Truncated { need: usize, have: usize },
    /// Content did not hash to what the manifest demands.
    DigestMismatch { member: String },
    /// The manifest's own digest does not cover the tables on disk.
    ManifestMismatch,
    /// A member the pallet claims carries no digest, so nothing can be checked.
    NoDigest { member: String },
    NotFound(String),
    OutOfRange { offset: u64, len: u64 },
    /// A fixed-width field in the on-disk format cannot hold this value.
    TooLong { field: &'static str, max: usize, got: usize },
    /// No free run on the drive large enough, or no free GPT entry.
    NoSpace { need: u64, largest_free: u64 },
    /// The requested range overlaps a partition that already exists. Firmware
    /// does not publish a handle for an aliased entry, so this is refused.
    Overlaps { with: String },
    /// The operation would have destroyed something still in use — the active
    /// pallet, or the one that fallback depends on.
    Refused(String),
    NotGpt,
}

impl fmt::Display for PalletError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PalletError::Io(e) => write!(f, "I/O error: {e}"),
            PalletError::Drive(e) => write!(f, "drive error: {e}"),
            PalletError::BadMagic => write!(f, "not a pallet: bad magic"),
            PalletError::UnsupportedVersion(v) => write!(
                f,
                "pallet format version {v} is newer than this build understands (v{})",
                format::VERSION
            ),
            PalletError::BadGeometry(m) => write!(f, "bad pallet geometry: {m}"),
            PalletError::BadHeaderCrc => write!(f, "header CRC mismatch"),
            PalletError::BadEntryCrc => write!(f, "GPT partition entry array CRC mismatch"),
            PalletError::BadMemberCrc => write!(f, "pallet member table CRC mismatch"),
            PalletError::BadExtentCrc => write!(f, "pallet extent table CRC mismatch"),
            PalletError::Truncated { need, have } => {
                write!(f, "pallet truncated: need {need} bytes, have {have}")
            }
            PalletError::DigestMismatch { member } => {
                write!(f, "member '{member}' does not match the digest the manifest records")
            }
            PalletError::ManifestMismatch => {
                write!(f, "manifest digest does not cover the member and extent tables on disk")
            }
            PalletError::NoDigest { member } => {
                write!(f, "member '{member}' carries no digest, so nothing can be verified")
            }
            PalletError::NotFound(what) => write!(f, "not found: {what}"),
            PalletError::OutOfRange { offset, len } => {
                write!(f, "read of {len} bytes at {offset} runs past the member")
            }
            PalletError::TooLong { field, max, got } => {
                write!(f, "{field} is {got} bytes, and the format allows {max}")
            }
            PalletError::NoSpace { need, largest_free } => write!(
                f,
                "no free run large enough: need {need} bytes, largest free run is {largest_free}"
            ),
            PalletError::Overlaps { with } => {
                write!(f, "range overlaps partition '{with}' — a pallet must never alias")
            }
            PalletError::Refused(why) => write!(f, "refused: {why}"),
            PalletError::NotGpt => write!(f, "no usable GPT on this device"),
        }
    }
}

impl std::error::Error for PalletError {}

impl From<std::io::Error> for PalletError {
    fn from(e: std::io::Error) -> Self {
        PalletError::Io(e)
    }
}

impl From<DriveError> for PalletError {
    fn from(e: DriveError) -> Self {
        PalletError::Drive(e)
    }
}

/// The shared reader's errors, carried into the engine's vocabulary. It has no
/// allocator, so its errors carry no strings; this is where they get context.
impl From<stormblock_pallet_format::Error> for PalletError {
    fn from(e: stormblock_pallet_format::Error) -> Self {
        use stormblock_pallet_format::Error as E;
        match e {
            E::BadMagic => PalletError::BadMagic,
            E::UnsupportedVersion(v) => PalletError::UnsupportedVersion(v),
            E::BadGeometry => PalletError::BadGeometry("superblock geometry".into()),
            E::BadHeaderCrc => PalletError::BadHeaderCrc,
            E::BadMemberCrc => PalletError::BadMemberCrc,
            E::BadExtentCrc => PalletError::BadExtentCrc,
            E::Truncated => PalletError::Truncated { need: 0, have: 0 },
            E::NotFound => PalletError::NotFound("member".into()),
            E::OutOfRange => PalletError::OutOfRange { offset: 0, len: 0 },
            E::ManifestMismatch => PalletError::ManifestMismatch,
            E::DigestMismatch => PalletError::DigestMismatch { member: String::new() },
            E::NoDigest => PalletError::NoDigest { member: String::new() },
            E::ReadFailed => PalletError::Refused("read failed".into()),
            E::ScratchTooSmall => PalletError::BadGeometry("scratch smaller than one block".into()),
        }
    }
}

pub type Result<T> = std::result::Result<T, PalletError>;

// CRC-32/IEEE — the one GPT and the pallet superblock use, and *not* `crc32c`,
// which is a different polynomial. Defined in the shared format crate so the
// engine and the firmware reader compute the same thing by construction.
pub use stormblock_pallet_format::{crc32, crc32_continue, superblock_crc};

// ------------------------------------------------------------ partition view

/// A byte-addressable window onto a device, offset by a partition start.
///
/// Everything inside a pallet is partition-relative, so this is the only place
/// the absolute address is known. It also absorbs block alignment: callers work
/// in bytes and the view reads or read-modify-writes whole blocks underneath.
#[derive(Clone)]
pub struct PartitionView {
    device: Arc<dyn BlockDevice>,
    start: u64,
    len: u64,
}

impl PartitionView {
    /// `start` and `len` are byte offsets into the device.
    pub fn new(device: Arc<dyn BlockDevice>, start: u64, len: u64) -> Self {
        PartitionView { device, start, len }
    }

    /// The whole device as one window — used when a pallet image is written to
    /// a bare file rather than into a partition.
    pub fn whole(device: Arc<dyn BlockDevice>) -> Self {
        let len = device.capacity_bytes();
        PartitionView { device, start: 0, len }
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn start(&self) -> u64 {
        self.start
    }

    pub fn device(&self) -> &Arc<dyn BlockDevice> {
        &self.device
    }

    pub fn block_size(&self) -> u32 {
        self.device.block_size()
    }

    /// Read `buf.len()` bytes at a partition-relative offset.
    pub async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let len = buf.len() as u64;
        if offset + len > self.len {
            return Err(PalletError::OutOfRange { offset, len });
        }
        let bs = self.device.block_size() as u64;
        let abs = self.start + offset;
        let aligned = abs / bs * bs;
        let skew = (abs - aligned) as usize;
        let span = ((skew as u64 + len).div_ceil(bs) * bs) as usize;
        let mut tmp = vec![0u8; span];
        self.device.read(aligned, &mut tmp).await?;
        buf.copy_from_slice(&tmp[skew..skew + buf.len()]);
        Ok(())
    }

    /// Write at a partition-relative offset, read-modify-writing the edge
    /// blocks when the range is not block-aligned.
    pub async fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let len = buf.len() as u64;
        if offset + len > self.len {
            return Err(PalletError::OutOfRange { offset, len });
        }
        let bs = self.device.block_size() as u64;
        let abs = self.start + offset;
        let aligned = abs / bs * bs;
        let skew = (abs - aligned) as usize;
        let span = ((skew as u64 + len).div_ceil(bs) * bs) as usize;
        let mut tmp = vec![0u8; span];
        // Only the first and last block can carry bytes we must preserve, and
        // only when the range does not start or end on a block boundary.
        let head_partial = skew != 0;
        let tail_partial = (skew as u64 + len) % bs != 0;
        if head_partial || tail_partial {
            if span == bs as usize {
                self.device.read(aligned, &mut tmp).await?;
            } else {
                if head_partial {
                    self.device.read(aligned, &mut tmp[..bs as usize]).await?;
                }
                if tail_partial {
                    let last = span - bs as usize;
                    self.device.read(aligned + last as u64, &mut tmp[last..]).await?;
                }
            }
        }
        tmp[skew..skew + buf.len()].copy_from_slice(buf);
        self.device.write(aligned, &tmp).await?;
        Ok(())
    }

    pub async fn flush(&self) -> Result<()> {
        self.device.flush().await?;
        Ok(())
    }
}

// ---------------------------------------------------------------- content

/// The bytes of one member, wherever they come from.
///
/// A publish reads each source twice — once to size and digest it, once to lay
/// it down — so a source has to be re-readable rather than a stream. That is
/// what lets the superblock be written last: the digests are known before any
/// of the header exists on disk.
#[async_trait]
pub trait MemberContent: Send + Sync {
    fn byte_len(&self) -> u64;
    /// Read at a content-relative offset. Reads past the end are an error, not
    /// a short read.
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;
}

/// Content already in memory.
pub struct BytesContent(pub Vec<u8>);

#[async_trait]
impl MemberContent for BytesContent {
    fn byte_len(&self) -> u64 {
        self.0.len() as u64
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let end = offset as usize + buf.len();
        if end > self.0.len() {
            return Err(PalletError::OutOfRange { offset, len: buf.len() as u64 });
        }
        buf.copy_from_slice(&self.0[offset as usize..end]);
        Ok(())
    }
}

/// Content from a file on the host.
pub struct FileContent {
    path: PathBuf,
    len: u64,
}

impl FileContent {
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let len = tokio::fs::metadata(&path).await?.len();
        Ok(FileContent { path, len })
    }
}

#[async_trait]
impl MemberContent for FileContent {
    fn byte_len(&self) -> u64 {
        self.len
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let mut f = tokio::fs::File::open(&self.path).await?;
        f.seek(std::io::SeekFrom::Start(offset)).await?;
        f.read_exact(buf).await?;
        Ok(())
    }
}

/// Content from a block device — which is how a **volume** becomes a pallet
/// member, since a thin volume is a `BlockDevice` like any other. The golden a
/// pallet ships is a sealed clone, and it is published by being read out of the
/// engine rather than by being copied into a file first.
pub struct DeviceContent {
    view: PartitionView,
    len: u64,
}

impl DeviceContent {
    /// Take the first `len` bytes of `device`.
    pub fn new(device: Arc<dyn BlockDevice>, len: u64) -> Self {
        let cap = device.capacity_bytes();
        DeviceContent { view: PartitionView::new(device, 0, cap.max(len)), len }
    }

    /// Take the whole device.
    pub fn whole(device: Arc<dyn BlockDevice>) -> Self {
        let len = device.capacity_bytes();
        DeviceContent::new(device, len)
    }
}

#[async_trait]
impl MemberContent for DeviceContent {
    fn byte_len(&self) -> u64 {
        self.len
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if offset + buf.len() as u64 > self.len {
            return Err(PalletError::OutOfRange { offset, len: buf.len() as u64 });
        }
        self.view.read_at(offset, buf).await
    }
}
