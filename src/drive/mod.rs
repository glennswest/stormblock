//! Drive layer — unified BlockDevice trait over NVMe (VFIO), SAS (io_uring), and file (tokio).

#[cfg(all(target_os = "linux", feature = "nvmeof"))]
pub mod nvme;
#[cfg(target_os = "linux")]
pub mod sas;
pub mod backing;
#[cfg(target_os = "linux")]
pub mod crashdev;
pub mod direct;
pub mod dma;
pub mod emulated;
pub mod erase;
pub mod filedev;
pub mod freemap;
pub mod flushgate;
pub mod identity;
pub mod handover;
pub mod httpdev;
pub mod partition;
pub mod ridethrough;
pub mod discover;
// The initiators reuse the *target's* PDU parsers rather than carrying a
// second copy of RFC 7143 / the NVMe-TCP spec, so each exists exactly
// where its target does.
#[cfg(feature = "iscsi")]
pub mod iscsi_dev;
#[cfg(feature = "nvmeof")]
pub mod nvmeof_dev;
pub mod slab;
pub mod slab_registry;
pub mod slottable;
#[cfg(target_os = "linux")]
pub mod ublk;
pub mod uring_channel;
pub mod uring_server;

use std::fmt;

use async_trait::async_trait;
use serde::{Serialize, Deserialize};
use uuid::Uuid;

// Re-export the DMA buffer type
pub use dma::DmaBuf;

/// Unique identifier for a physical drive.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId {
    pub uuid: Uuid,
    pub serial: String,
    pub model: String,
    pub path: String,
    /// World-wide name (SCSI NAA / NVMe EUI-64/NGUID) when the drive has
    /// one — with `serial`, the identity stormdrive knows a drive by, and
    /// the one that survives the drive moving to another bay or host (#136).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub wwn: String,
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.model, self.serial)
    }
}

/// Type of physical drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DriveType {
    NVMe,
    SasSsd,
    SasHdd,
    File,  // loopback / MikroTik / dev testing
    Iscsi, // remote iSCSI target
    /// Remote NVMe-TCP namespace attached as a drive — the cross-node
    /// RAID-leg transport (#73).
    NvmeTcp,
    /// A drive that reports any capacity and stores only what is written,
    /// for scale tests (#208): never media.
    Emulated,
}

impl fmt::Display for DriveType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DriveType::NVMe => write!(f, "NVMe"),
            DriveType::SasSsd => write!(f, "SAS-SSD"),
            DriveType::SasHdd => write!(f, "SAS-HDD"),
            DriveType::File => write!(f, "File"),
            DriveType::Iscsi => write!(f, "iSCSI"),
            DriveType::NvmeTcp => write!(f, "NVMe-TCP"),
            DriveType::Emulated => write!(f, "Emulated"),
        }
    }
}

/// Errors from the drive layer.
#[derive(Debug)]
pub enum DriveError {
    Io(std::io::Error),
    NotAligned { offset: u64, block_size: u32 },
    OutOfRange { offset: u64, len: u64, capacity: u64 },
    BufferTooSmall { need: usize, have: usize },
    /// The backing store has nowhere to put the write: a thin volume whose
    /// slabs are full, or — the case that reads as a hardware fault if it is
    /// not named — a volume whose placement role has no slab at all on this
    /// node. Distinct from `Io` because a target has an honest status for it
    /// (NVMe "Capacity Exceeded", SCSI "space allocation failed"), and
    /// answering with a media error sends the operator to the wrong layer
    /// (#92).
    NoSpace(String),
    /// The write was refused by policy, not by hardware: a sealed volume, or
    /// one whose access is read-only. Targets answer "write protected", so an
    /// initiator can tell a setting from a fault.
    ReadOnly(String),
    DeviceNotReady,
    VfioNotAvailable,
    Other(anyhow::Error),
}

impl DriveError {
    /// The network (or a remote device) did not answer: a timeout, a refused,
    /// reset or dropped connection (#359). Worth another attempt on a new
    /// connection; never a reason to stop trusting the media.
    pub fn is_transport(&self) -> bool {
        matches!(self, DriveError::Io(e) if crate::retry::classify_io(e) == crate::retry::Class::Transient)
    }

    /// Whether this is a reason to stop trusting the storage underneath.
    ///
    /// Marking a leg failed is sticky and it is written into the volume's
    /// record, so it has to mean the media and not the request. `EINVAL` is
    /// the device refusing an I/O it was never going to accept — an offset,
    /// a length or a buffer address that is not a multiple of the block size
    /// under `O_DIRECT`. The same read issued correctly succeeds, so taking
    /// the volume offline for it turns a caller's mistake into an outage
    /// that survives a restart. Same reasoning as #92: answer at the layer
    /// the fault is actually in.
    pub fn is_media_failure(&self) -> bool {
        match self {
            // A timeout or a dropped connection is the network, not the
            // media (#359): the same I/O on a new connection succeeds.
            DriveError::Io(e) if self.is_transport() => {
                let _ = e;
                false
            }
            DriveError::Io(e) => !matches!(
                e.kind(),
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData
            ),
            DriveError::NotAligned { .. }
            | DriveError::OutOfRange { .. }
            | DriveError::BufferTooSmall { .. }
            | DriveError::NoSpace(_)
            | DriveError::ReadOnly(_) => false,
            _ => true,
        }
    }
}


impl fmt::Display for DriveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DriveError::Io(e) => write!(f, "I/O error: {e}"),
            DriveError::NotAligned { offset, block_size } => {
                write!(f, "offset {offset} not aligned to block size {block_size}")
            }
            DriveError::OutOfRange { offset, len, capacity } => {
                write!(f, "range [{offset}..{}] exceeds capacity {capacity}", offset + len)
            }
            DriveError::BufferTooSmall { need, have } => {
                write!(f, "buffer too small: need {need}, have {have}")
            }
            DriveError::NoSpace(what) => write!(f, "no space: {what}"),
            DriveError::ReadOnly(why) => write!(f, "read-only: {why}"),
            DriveError::DeviceNotReady => write!(f, "device not ready"),
            DriveError::VfioNotAvailable => write!(f, "VFIO not available"),
            DriveError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DriveError {}

impl From<std::io::Error> for DriveError {
    fn from(e: std::io::Error) -> Self {
        DriveError::Io(e)
    }
}

pub type DriveResult<T> = Result<T, DriveError>;

/// SMART health data (placeholder — will be expanded per drive type).
#[derive(Debug, Clone, Default, Serialize)]
pub struct SmartData {
    pub temperature_celsius: Option<u16>,
    pub power_on_hours: Option<u64>,
    pub media_errors: u64,
    pub available_spare_pct: Option<u8>,
    pub healthy: bool,
}

/// A single I/O operation for batch submission.
#[derive(Debug, Clone)]
pub enum IoOp {
    Read { offset: u64, buf_idx: u32, len: u32 },
    Write { offset: u64, buf_idx: u32, len: u32 },
    Flush,
    Discard { offset: u64, len: u64 },
}

/// Result of a completed I/O operation.
#[derive(Debug)]
pub struct IoCompletion {
    pub op_idx: u32,
    pub result: DriveResult<u32>,
    pub latency_ns: u64,
}

/// Unified interface for all physical drive types.
///
/// Implemented by NvmeDevice (VFIO), SasDevice (io_uring), and FileDevice (tokio).
/// The RAID engine and volume manager only interact with this trait.
/// Zeros written as zeros, a megabyte at a time: what a device that cannot
/// do better does, and what one that can falls back to.
pub async fn write_zeroes_by_writing<D: BlockDevice + ?Sized>(dev: &D, offset: u64, len: u64) -> DriveResult<()> {
    const CHUNK: u64 = 1 << 20;
    let zeros = vec![0u8; CHUNK.min(len).max(1) as usize];
    let mut done = 0u64;
    while done < len {
        let n = (len - done).min(zeros.len() as u64) as usize;
        dev.write(offset + done, &zeros[..n]).await?;
        done += n as u64;
    }
    Ok(())
}

/// Zeros without moving them (#173): `BLKZEROOUT` on a block device (the
/// drive's WRITE ZEROES / WRITE SAME where it has one, the kernel's own
/// zeroing where it does not), a punched hole in a regular file. `Ok(false)`
/// when neither applies; the caller writes zeros then. Only the part of the
/// range on whole `unit`s is done here; the caller zeroes the edges.
#[cfg(target_os = "linux")]
pub(crate) async fn zero_out_fd(fd: std::os::unix::io::RawFd, block_device: bool, offset: u64, len: u64) -> DriveResult<bool> {
    let r = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let rc = if block_device {
            let range: [u64; 2] = [offset, len];
            // BLKZEROOUT = _IO(0x12, 127)
            unsafe { libc::ioctl(fd, 0x127F, &range) }
        } else {
            unsafe {
                libc::fallocate(
                    fd,
                    libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                    offset as libc::off_t,
                    len as libc::off_t,
                )
            }
        };
        if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
    })
    .await
    .map_err(|e| DriveError::Other(e.into()))?;
    Ok(r.is_ok())
}

/// [`zero_out_fd`] on the whole `unit`s of a range, the edges written: the
/// range reads back as zeros either way.
#[cfg(target_os = "linux")]
pub(crate) async fn zero_range_fast<D: BlockDevice + ?Sized>(
    dev: &D,
    fd: std::os::unix::io::RawFd,
    block_device: bool,
    unit: u64,
    offset: u64,
    len: u64,
) -> DriveResult<()> {
    let unit = unit.max(512);
    let start = offset.div_ceil(unit) * unit;
    let end = (offset + len) / unit * unit;
    if end <= start {
        return write_zeroes_by_writing(dev, offset, len).await;
    }
    if !zero_out_fd(fd, block_device, start, end - start).await? {
        return write_zeroes_by_writing(dev, offset, len).await;
    }
    if start > offset {
        write_zeroes_by_writing(dev, offset, start - offset).await?;
    }
    if offset + len > end {
        write_zeroes_by_writing(dev, end, offset + len - end).await?;
    }
    Ok(())
}

#[async_trait]
pub trait BlockDevice: Send + Sync {
    /// Device identity.
    fn id(&self) -> &DeviceId;

    /// The identity of the **drive** this device is on — for a partition,
    /// its disk's (#136). What a failure domain and a drive label key on:
    /// two slabs on one spindle fail together whatever their offsets.
    fn drive_id(&self) -> DeviceId {
        self.id().clone()
    }

    /// Total capacity in bytes.
    fn capacity_bytes(&self) -> u64;

    /// Logical block size (512 or 4096).
    fn block_size(&self) -> u32;

    /// Optimal I/O size for alignment (typically 4096).
    fn optimal_io_size(&self) -> u32;

    /// Smallest span, in bytes, that `discard` can actually reclaim.
    ///
    /// Targets advertise this so initiators align their discards to something
    /// that frees space — a thin volume reclaims a whole slab slot at a time,
    /// so a smaller discard is silently a no-op. Defaults to the block size
    /// for devices where every block is independently reclaimable.
    fn discard_granularity(&self) -> u32 {
        self.block_size()
    }

    /// Physical drive type.
    fn device_type(&self) -> DriveType;

    /// Read `buf.len()` bytes from `offset` into `buf`.
    /// `offset` must be aligned to `block_size()`.
    async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize>;

    /// Write `buf.len()` bytes from `buf` to `offset`.
    /// `offset` must be aligned to `block_size()`.
    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize>;

    /// Flush any cached writes to stable storage.
    async fn flush(&self) -> DriveResult<()>;

    /// Discard (TRIM/UNMAP) a range. No-op on HDDs.
    async fn discard(&self, offset: u64, len: u64) -> DriveResult<()>;

    /// Make `len` bytes at `offset` read back as zeros. The default writes
    /// zeros; a device that can do better (a thin volume skipping what it
    /// never mapped) overrides it. Unlike `discard`, this is a promise.
    async fn write_zeroes(&self, offset: u64, len: u64) -> DriveResult<()> {
        write_zeroes_by_writing(self, offset, len).await
    }

    /// Query SMART health data. Returns None if not supported.
    fn smart_status(&self) -> DriveResult<SmartData> {
        Ok(SmartData { healthy: true, ..Default::default() })
    }

    /// Total media error count.
    fn media_errors(&self) -> u64 {
        0
    }
}

/// Open drives from a list of device paths.
///
/// On Linux, paths like `/dev/sdX` are opened via io_uring (SasDevice).
/// Paths to regular files (or anything else) use the tokio FileDevice fallback.
pub async fn open_drives(paths: &[String]) -> Vec<(String, DriveResult<Box<dyn BlockDevice>>)> {
    let mut results = Vec::with_capacity(paths.len());
    for path in paths {
        let result = open_one_drive(path).await;
        results.push((path.clone(), result));
    }
    results
}

pub async fn open_one_drive(path: &str) -> DriveResult<Box<dyn BlockDevice>> {
    open_one_drive_with_secret(path, None).await
}

/// [`open_one_drive`], with the DH-HMAC-CHAP secret an `nvme-tcp://` drive
/// answers with when its target asks (#213). The secret is never part of the
/// path: a path is logged, listed and persisted. It is kept with the open
/// device, so a reconnect answers with it too. A secret for anything that is
/// not an `nvme-tcp://` URI is refused rather than ignored.
pub async fn open_one_drive_with_secret(
    path: &str,
    dhchap: Option<crate::target::nvmeof::auth::DhchapKey>,
) -> DriveResult<Box<dyn BlockDevice>> {
    if dhchap.is_some() && !path.starts_with("nvme-tcp://") {
        return Err(DriveError::Other(anyhow::anyhow!(
            "a DH-HMAC-CHAP secret is for an nvme-tcp:// drive, and {path:?} is not one"
        )));
    }
    // An emulated drive for scale tests (#208): any capacity, nothing stored
    // but what is written; one name is one drive for the process.
    if let Some(spec) = emulated::EmulatedSpec::parse(path) {
        return Ok(Box::new(emulated::open(&spec?)?));
    }
    // Fabric URIs first — a remote namespace attached as a drive
    // (stormblock#73). The same string works everywhere a device path
    // does: config `[[drives]]`, POST /api/v1/drives, RAID members.
    #[cfg(feature = "nvmeof")]
    if let Some(mut spec) = nvmeof_dev::NvmeTcpSpec::parse(path) {
        if dhchap.is_some() {
            spec.dhchap = dhchap;
        }
        let dev = nvmeof_dev::NvmeofDevice::connect(&spec).await?;
        return Ok(Box::new(dev));
    }
    #[cfg(feature = "iscsi")]
    if let Some((portal, port, iqn)) = parse_iscsi_uri(path) {
        let dev = iscsi_dev::IscsiDevice::connect(&portal, port, &iqn).await?;
        return Ok(Box::new(dev));
    }
    if path.contains("://") {
        return Err(DriveError::Other(anyhow::anyhow!(
            "unsupported drive URI {path:?} (this build understands: {})",
            supported_uri_schemes()
        )));
    }

    // A block device is a block device (#140): O_DIRECT, never a file.
    if is_block_device(path) {
        #[cfg(target_os = "linux")]
        // A controller reset on this drive's HBA is ridden through (#391).
        return Ok(ridethrough::wrap_box(Box::new(sas::SasDevice::open(path).await?)));
    }

    // A regular file: tests and development only.
    let dev = filedev::FileDevice::open(path).await?;
    Ok(Box::new(dev))
}

/// Is `path` a block device node?
pub fn is_block_device(path: &str) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        std::fs::metadata(path).map(|m| m.file_type().is_block_device()).unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// Open storage by path, the one way (#140): a fabric URI is attached, a
/// block device is opened `O_DIRECT` as the drive it is ([`sas::SasDevice`]),
/// and only a regular file — an image, a test's scratch disk — is a
/// [`filedev::FileDevice`]. The owner's direction: "I don't want any file
/// IO" for real storage; every slab on the installed disk goes through here.
///
/// `read_only` opens for inspection: the kernel refuses writes. A file that
/// does not exist is an error, never created — use [`filedev::FileDevice`]
/// directly to make an image.
pub async fn open_path(path: &str, read_only: bool) -> DriveResult<std::sync::Arc<dyn BlockDevice>> {
    if path.contains("://") {
        if read_only {
            tracing::debug!("{path}: a fabric device is opened as it is served");
        }
        return open_one_drive(path).await.map(std::sync::Arc::from);
    }
    if is_block_device(path) {
        #[cfg(target_os = "linux")]
        {
            let dev: std::sync::Arc<dyn BlockDevice> = if read_only {
                std::sync::Arc::new(sas::SasDevice::open_read_only(path).await?)
            } else {
                std::sync::Arc::new(sas::SasDevice::open(path).await?)
            };
            // A controller reset on this drive's HBA is ridden through, not
            // passed up as EIO (#391).
            return Ok(ridethrough::RideThrough::wrap(dev));
        }
    }
    if !std::path::Path::new(path).exists() {
        return Err(DriveError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{path} does not exist"),
        )));
    }
    let dev = if read_only {
        filedev::FileDevice::open_read_only(path).await?
    } else {
        filedev::FileDevice::open(path).await?
    };
    Ok(std::sync::Arc::new(dev))
}

fn supported_uri_schemes() -> &'static str {
    #[cfg(all(feature = "nvmeof", feature = "iscsi"))]
    return "nvme-tcp://host:port/nqn?nsid=N, iscsi://host:port/iqn, emulated://name?size=1P";
    #[cfg(all(feature = "nvmeof", not(feature = "iscsi")))]
    return "nvme-tcp://host:port/nqn?nsid=N";
    #[cfg(all(not(feature = "nvmeof"), feature = "iscsi"))]
    return "iscsi://host:port/iqn";
    #[cfg(all(not(feature = "nvmeof"), not(feature = "iscsi")))]
    return "none (built without fabric features)";
}

/// Parse `iscsi://host:port/iqn` into (host, port, iqn).
#[cfg(feature = "iscsi")]
fn parse_iscsi_uri(uri: &str) -> Option<(String, u16, String)> {
    let rest = uri.strip_prefix("iscsi://")?;
    let (addr, iqn) = rest.split_once('/')?;
    let (host, port) = addr.split_once(':')?;
    if host.is_empty() || iqn.is_empty() {
        return None;
    }
    Some((host.to_string(), port.parse().ok()?, iqn.to_string()))
}

#[cfg(all(test, feature = "iscsi"))]
mod uri_tests {
    use super::*;

    #[test]
    fn iscsi_uri_parses() {
        let (h, p, iqn) = parse_iscsi_uri("iscsi://10.0.0.1:3260/iqn.2024.io.stormblock:v1").unwrap();
        assert_eq!(h, "10.0.0.1");
        assert_eq!(p, 3260);
        assert_eq!(iqn, "iqn.2024.io.stormblock:v1");
        assert!(parse_iscsi_uri("iscsi://noport/iqn").is_none());
        assert!(parse_iscsi_uri("nvme-tcp://h:4420/nqn").is_none());
    }
}
