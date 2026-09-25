//! A raw block device: SAS, SATA or NVMe, opened `O_DIRECT` (#140).
//!
//! **Every drive is opened this way — the installed disk included.** The
//! owner's direction on #140: "we will get rid of the file/block copies …
//! I don't want any file IO." `FileDevice` is for tests and development;
//! a block device goes through here (see `drive::open_path`).
//!
//! The I/O itself is [`DirectIo`](super::direct::DirectIo): an io_uring on a
//! thread of its own with many requests in flight, or `pread`/`pwrite` on the
//! blocking pool where io_uring is not available. What was here before held a
//! lock across `submit_and_wait` on the async runtime's own thread — one
//! request at a time per drive, and a worker blocked for each.
//!
//! `O_DIRECT` is a contract with the kernel: the offset, the length **and the
//! buffer address** must be multiples of the logical block, or the request is
//! `EINVAL`. This device keeps the whole contract, so a caller does not have
//! to: every request goes through a page-aligned [`DmaBuf`], and one that is
//! not whole logical blocks is widened to them — a read reads the span and
//! copies out; a write reads the edge blocks, patches them and writes the
//! span back, under a lock so two such writes cannot interleave on a block.
//! (The buffer half was found the hard way: every ext4 template format on a
//! 4 KiB-sector appliance failed at the first inode-table zeroing, a
//! `vec![0u8; 1 << 20]` at a whole-block offset — mkfs.ext4.rs#5.)

use std::os::unix::io::RawFd;

use async_trait::async_trait;
use uuid::Uuid;

use super::direct::DirectIo;
use super::dma::DmaBuf;
use super::{BlockDevice, DeviceId, DriveError, DriveResult, DriveType, SmartData};

/// A raw block device, `O_DIRECT`.
pub struct SasDevice {
    fd: RawFd,
    io: DirectIo,
    id: DeviceId,
    capacity: u64,
    block_size: u32,
    device_type: DriveType,
    /// Held across a read-modify-write, so two partial-block writes to the
    /// same block cannot each keep the other's bytes out.
    rmw: tokio::sync::Mutex<()>,
}

impl SasDevice {
    /// Open a block device at `path` for reading and writing.
    pub async fn open(path: &str) -> DriveResult<Self> {
        Self::open_with(path, false, None).await
    }

    /// Open a block device read-only — for anything that inspects. The
    /// kernel refuses writes, so an inspection cannot change what it looks
    /// at even by mistake.
    pub async fn open_read_only(path: &str) -> DriveResult<Self> {
        Self::open_with(path, true, None).await
    }

    /// Open a **regular file** `O_DIRECT` with an explicit logical block size
    /// — the same engine and the same contract as a drive, for tests, where
    /// a block device cannot be had without root.
    pub async fn open_file_direct(path: &str, block_size: u32) -> DriveResult<Self> {
        Self::open_with(path, false, Some(block_size)).await
    }

    async fn open_with(path: &str, read_only: bool, file_block: Option<u32>) -> DriveResult<Self> {
        let path = path.to_string();
        let (fd, capacity, block_size) = tokio::task::spawn_blocking({
            let path = path.clone();
            move || -> DriveResult<(RawFd, u64, u32)> {
                use nix::fcntl::{open, OFlag};
                use nix::sys::stat::Mode;

                let rw = if read_only { OFlag::O_RDONLY } else { OFlag::O_RDWR };
                let fd = open(path.as_str(), rw | OFlag::O_DIRECT | OFlag::O_CLOEXEC, Mode::empty())
                    .map_err(|e| DriveError::Io(e.into()))?;
                let sizes = match file_block {
                    Some(bs) => std::fs::metadata(&path)
                        .map(|m| (m.len(), bs))
                        .map_err(DriveError::Io),
                    None => ioctl_blkgetsize64(fd).and_then(|c| Ok((c, ioctl_blksszget(fd)?))),
                };
                match sizes {
                    Ok((capacity, block_size)) => Ok((fd, capacity, block_size)),
                    Err(e) => {
                        unsafe { libc::close(fd) };
                        Err(e)
                    }
                }
            }
        })
        .await
        .map_err(|e| DriveError::Other(e.into()))??;

        // Who the drive is: serial, model, WWN; a partition is its disk (#136).
        let (serial, model) = read_device_identity(&path);
        let known = super::identity::of(&path);
        let id = DeviceId {
            wwn: known.as_ref().map(|i| i.wwn.clone()).unwrap_or_default(),
            uuid: Uuid::new_v4(),
            serial: known
                .as_ref()
                .map(|i| i.serial.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or(serial),
            model: known.as_ref().map(|i| i.model.clone()).filter(|m| !m.is_empty()).unwrap_or(model),
            path: path.clone(),
        };
        let device_type = detect_drive_type(&path);
        let io = DirectIo::new(fd);
        tracing::debug!(%path, engine = io.engine_name(), block_size, "block device opened O_DIRECT");

        Ok(SasDevice {
            fd,
            io,
            id,
            capacity,
            block_size,
            device_type,
            rmw: tokio::sync::Mutex::new(()),
        })
    }

    /// Which engine carries this device's I/O: `io_uring` or `blocking`.
    pub fn engine(&self) -> &'static str {
        self.io.engine_name()
    }

    /// The whole logical blocks covering `[offset, offset + len)`.
    fn span(&self, offset: u64, len: usize) -> (u64, usize) {
        let bs = u64::from(self.block_size.max(1));
        let start = offset / bs * bs;
        let end = (offset + len as u64).div_ceil(bs) * bs;
        (start, (end - start) as usize)
    }

    fn check_range(&self, offset: u64, len: usize) -> DriveResult<()> {
        if offset + len as u64 > self.capacity {
            return Err(DriveError::OutOfRange { offset, len: len as u64, capacity: self.capacity });
        }
        Ok(())
    }
}

#[async_trait]
impl BlockDevice for SasDevice {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn capacity_bytes(&self) -> u64 {
        self.capacity
    }

    fn block_size(&self) -> u32 {
        self.block_size
    }

    fn optimal_io_size(&self) -> u32 {
        4096
    }

    fn device_type(&self) -> DriveType {
        self.device_type
    }

    async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.check_range(offset, buf.len())?;
        let (start, len) = self.span(offset, buf.len());
        let got = self.io.read(start, len).await?;
        let skew = (offset - start) as usize;
        buf.copy_from_slice(&got[skew..skew + buf.len()]);
        Ok(buf.len())
    }

    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.check_range(offset, buf.len())?;
        let (start, len) = self.span(offset, buf.len());
        if start == offset && len == buf.len() {
            let mut out = DmaBuf::alloc(len);
            out[..len].copy_from_slice(buf);
            self.io.write(start, out, len).await?;
            return Ok(buf.len());
        }
        // Not whole blocks: read the span, patch it, write it back — under
        // the lock, so another partial write to these blocks waits its turn.
        let _one = self.rmw.lock().await;
        let mut span = self.io.read(start, len).await?;
        let skew = (offset - start) as usize;
        span[skew..skew + buf.len()].copy_from_slice(buf);
        self.io.write(start, span, len).await?;
        Ok(buf.len())
    }

    async fn flush(&self) -> DriveResult<()> {
        self.io.sync().await
    }

    async fn discard(&self, offset: u64, len: u64) -> DriveResult<()> {
        if self.device_type == DriveType::SasHdd {
            return Ok(()); // No-op for HDDs.
        }
        // BLKDISCARD ioctl for SSDs.
        let fd = self.fd;
        tokio::task::spawn_blocking(move || {
            ioctl_blkdiscard(fd, offset, len)
        })
        .await
        .map_err(|e| DriveError::Other(e.into()))?
    }

    fn smart_status(&self) -> DriveResult<SmartData> {
        read_smart_sysfs(&self.id.path)
    }
}

impl Drop for SasDevice {
    fn drop(&mut self) {
        // The engine works on a duplicate of this fd and finishes what it has
        // in flight on it; this one is ours to close.
        unsafe { libc::close(self.fd); }
    }
}

// --- ioctl helpers ---

fn ioctl_blkgetsize64(fd: RawFd) -> DriveResult<u64> {
    let mut size: u64 = 0;
    // BLKGETSIZE64 = 0x80081272
    let ret = unsafe { libc::ioctl(fd, 0x80081272u64 as libc::Ioctl, &mut size) };
    if ret < 0 {
        return Err(DriveError::Io(std::io::Error::last_os_error()));
    }
    Ok(size)
}

fn ioctl_blksszget(fd: RawFd) -> DriveResult<u32> {
    let mut size: libc::c_int = 0;
    // BLKSSZGET = 0x1268
    let ret = unsafe { libc::ioctl(fd, 0x1268u64 as libc::Ioctl, &mut size) };
    if ret < 0 {
        return Err(DriveError::Io(std::io::Error::last_os_error()));
    }
    Ok(size as u32)
}

fn ioctl_blkdiscard(fd: RawFd, offset: u64, len: u64) -> DriveResult<()> {
    let range: [u64; 2] = [offset, len];
    // BLKDISCARD = 0x1277
    let ret = unsafe { libc::ioctl(fd, 0x1277u64 as libc::Ioctl, &range) };
    if ret < 0 {
        return Err(DriveError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Try to read serial number and model from sysfs for a block device path.
fn read_device_identity(path: &str) -> (String, String) {
    // /dev/sda -> /sys/block/sda/device/{serial,model}
    let devname = path.rsplit('/').next().unwrap_or("");
    let serial = std::fs::read_to_string(format!("/sys/block/{devname}/device/serial"))
        .unwrap_or_default()
        .trim()
        .to_string();
    let model = std::fs::read_to_string(format!("/sys/block/{devname}/device/model"))
        .unwrap_or_default()
        .trim()
        .to_string();
    (
        if serial.is_empty() { "unknown".to_string() } else { serial },
        if model.is_empty() { "unknown".to_string() } else { model },
    )
}

/// Read SMART health data from sysfs for a SAS/SATA block device.
fn read_smart_sysfs(path: &str) -> DriveResult<SmartData> {
    let devname = path.rsplit('/').next().unwrap_or("");
    let sysfs_base = format!("/sys/block/{devname}/device");

    // Read SCSI device state — sysfs exposes "running" for healthy devices.
    let state = std::fs::read_to_string(format!("{sysfs_base}/state"))
        .unwrap_or_default()
        .trim()
        .to_string();
    let healthy = state.is_empty() || state == "running";

    // Read I/O error count from /sys/block/<dev>/stat (field 10 is io_ticks, field 9 is I/O errors
    // on some kernels). More reliably, read /sys/block/<dev>/device/ioerr_cnt if available.
    let media_errors = std::fs::read_to_string(format!("{sysfs_base}/ioerr_cnt"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);

    // Try reading hwmon temperature (some SCSI/SATA drives expose this).
    let temperature_celsius = read_hwmon_temp(devname);

    Ok(SmartData {
        temperature_celsius,
        power_on_hours: None,
        media_errors,
        available_spare_pct: None,
        healthy,
    })
}

/// Try to read drive temperature from hwmon sysfs entries.
fn read_hwmon_temp(devname: &str) -> Option<u16> {
    let hwmon_dir = format!("/sys/block/{devname}/device/hwmon");
    let entries = std::fs::read_dir(&hwmon_dir).ok()?;
    for entry in entries.flatten() {
        let temp_path = entry.path().join("temp1_input");
        if let Ok(val) = std::fs::read_to_string(&temp_path) {
            // hwmon temp is in millidegrees Celsius
            if let Ok(millideg) = val.trim().parse::<u64>() {
                return Some((millideg / 1000) as u16);
            }
        }
    }
    None
}

/// Detect if a block device is SSD or HDD via the rotational flag.
fn detect_drive_type(path: &str) -> DriveType {
    let devname = path.rsplit('/').next().unwrap_or("");
    let rotational = std::fs::read_to_string(format!("/sys/block/{devname}/queue/rotational"))
        .unwrap_or_default()
        .trim()
        .to_string();
    if rotational == "0" {
        DriveType::SasSsd
    } else {
        DriveType::SasHdd
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::drive::slab::Slab;
    use crate::drive::slab_registry::SlabRegistry;
    use crate::fs::ext4::{check, format, Ext4Params};
    use crate::placement::topology::StorageTier;
    use crate::volume::gem::GlobalExtentMap;
    use crate::volume::thin::{PlacementPolicy, ThinVolume, ThinVolumeHandle};

    /// The device these tests need: a block device with a 4096-byte logical
    /// block, which a regular file cannot stand in for. On the build box:
    ///
    ///     truncate -s 256M /build/images/align.img
    ///     losetup -b 4096 --show -f /build/images/align.img   # -> /dev/loopN
    ///     STORMBLOCK_4K_LOOP=/dev/loopN cargo test sas -- --ignored
    fn loop_device() -> Option<String> {
        std::env::var("STORMBLOCK_4K_LOOP").ok()
    }

    /// The exact failure from the appliance: a buffer from `vec!` is not
    /// page-aligned, and O_DIRECT refuses it whatever the offset and length.
    /// The drive owns the O_DIRECT contract, so it bounces the buffer, and
    /// the write — and the read back into another `Vec` — succeed.
    #[tokio::test]
    #[ignore = "needs STORMBLOCK_4K_LOOP, a block device with 4096-byte sectors"]
    async fn a_malloc_buffer_is_written_through_o_direct() {
        let Some(path) = loop_device() else { return };
        let dev = SasDevice::open(&path).await.unwrap();
        assert_eq!(dev.block_size(), 4096, "{path} is not a 4 KiB-sector device");

        let pattern: Vec<u8> = (0..(1usize << 20)).map(|i| (i % 251) as u8).collect();
        // Whatever malloc did, the drive must cope; and if malloc happened to
        // hand back a page-aligned buffer this run, it proves nothing, so
        // take an offset copy that certainly is not.
        let skewed = &pattern[16..(1 << 20) - 4096 + 16];
        assert_ne!(skewed.as_ptr() as usize % 4096, 0);
        assert_eq!(dev.write(4096, skewed).await.unwrap(), skewed.len());

        let mut back = vec![0u8; skewed.len() + 16];
        let back = &mut back[16..];
        assert_ne!(back.as_ptr() as usize % 4096, 0);
        assert_eq!(dev.read(4096, back).await.unwrap(), skewed.len());
        assert_eq!(back, skewed);

        // A request that is not whole blocks is widened by the device
        // (#140): read the edges, patch, write back.
        assert_eq!(dev.write(4096 + 100, &pattern[..1024]).await.unwrap(), 1024);
        let mut round = vec![0u8; 1024];
        dev.read(4096 + 100, &mut round).await.unwrap();
        assert_eq!(round, &pattern[..1024]);
    }

    /// The whole provisioning path: a slab on the 4 KiB drive, a thin volume
    /// on the slab, an ext4 template formatted onto it through the
    /// `BlockDevice` seam, and the result checked clean. This is what the
    /// appliance does for every PVC blank, and what failed at offset 45056.
    #[tokio::test]
    #[ignore = "needs STORMBLOCK_4K_LOOP, a block device with 4096-byte sectors"]
    async fn an_ext4_template_formats_on_a_thin_volume_over_a_4k_drive() {
        let Some(path) = loop_device() else { return };
        let dev = SasDevice::open(&path).await.unwrap();
        assert_eq!(dev.block_size(), 4096);

        let backing: Arc<dyn BlockDevice> = Arc::new(dev);
        let slab = Slab::format(backing, 1 << 20, StorageTier::Hot).await.unwrap();
        let mut registry = SlabRegistry::new();
        registry.add(slab);
        let registry = Arc::new(tokio::sync::RwLock::new(registry));
        let gem = Arc::new(tokio::sync::RwLock::new(GlobalExtentMap::new()));
        let vol = ThinVolume::new("blank-ext4-64M".to_string(), 64 << 20, 1 << 20);
        let handle: Arc<dyn BlockDevice> = Arc::new(ThinVolumeHandle::new(
            vol,
            gem,
            registry,
            PlacementPolicy::default(),
        ));
        assert_eq!(handle.block_size(), 4096);

        let report = format(&handle, &Ext4Params::default())
            .await
            .expect("formatting a 64M template on a thin volume over a 4 KiB drive");
        assert_eq!(report.block_size, 4096);

        let fsck = check(&handle).await.unwrap();
        assert!(fsck.is_clean(), "{:?}", fsck.problems);
    }
}
