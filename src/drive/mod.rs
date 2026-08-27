//! Drive layer — unified BlockDevice trait over NVMe (VFIO), SAS (io_uring), and file (tokio).

#[cfg(all(target_os = "linux", feature = "nvmeof"))]
pub mod nvme;
#[cfg(target_os = "linux")]
pub mod sas;
pub mod dma;
pub mod filedev;
pub mod handover;
pub mod partition;
// The initiators reuse the *target's* PDU parsers rather than carrying a
// second copy of RFC 7143 / the NVMe-TCP spec, so each exists exactly
// where its target does.
#[cfg(feature = "iscsi")]
pub mod iscsi_dev;
#[cfg(feature = "nvmeof")]
pub mod nvmeof_dev;
pub mod slab;
pub mod slab_registry;
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
    DeviceNotReady,
    VfioNotAvailable,
    Other(anyhow::Error),
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
#[async_trait]
pub trait BlockDevice: Send + Sync {
    /// Device identity.
    fn id(&self) -> &DeviceId;

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
    // Fabric URIs first — a remote namespace attached as a drive
    // (stormblock#73). The same string works everywhere a device path
    // does: config `[[drives]]`, POST /api/v1/drives, RAID members.
    #[cfg(feature = "nvmeof")]
    if let Some(spec) = nvmeof_dev::NvmeTcpSpec::parse(path) {
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

    // Check if it's a block device on Linux
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::FileTypeExt;
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.file_type().is_block_device() {
                let dev = sas::SasDevice::open(path).await?;
                return Ok(Box::new(dev));
            }
        }
    }

    // Fallback: open as file device (regular file or anything else)
    let dev = filedev::FileDevice::open(path).await?;
    Ok(Box::new(dev))
}

fn supported_uri_schemes() -> &'static str {
    #[cfg(all(feature = "nvmeof", feature = "iscsi"))]
    return "nvme-tcp://host:port/nqn?nsid=N, iscsi://host:port/iqn";
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
