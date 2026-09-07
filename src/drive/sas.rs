//! SAS/SATA block device access via io_uring with O_DIRECT.
//!
//! Opens /dev/sdX block devices with O_DIRECT for aligned DMA I/O.
//! Uses io_uring for async submission/completion.
//!
//! `O_DIRECT` is a contract with the kernel: the file offset, the length
//! **and the buffer address** must all be multiples of the device's logical
//! block, or the request is refused with `EINVAL` before it reaches the
//! drive. Offset and length are the caller's to get right and are checked
//! here. The buffer address is not the caller's business — a `Vec<u8>` from
//! malloc is 16-byte aligned, and every layer above this one (the thin
//! volume, the slab, `mkfs-ext4`) hands down whatever it has — so a buffer
//! that is not where `O_DIRECT` needs it is bounced through a page-aligned
//! [`DmaBuf`]. That contract lives in this file because this is the file
//! that opened the fd `O_DIRECT`; nothing above it can be expected to know.
//!
//! Found the hard way: every ext4 template format on a 4 KiB-sector
//! appliance failed at the first inode-table zeroing — the first write whose
//! buffer was a plain `vec![0u8; 1 << 20]` — with an offset and a length that
//! were both whole 4096-byte blocks (mkfs.ext4.rs#5). Reproduced on a
//! `losetup -b 4096` loop device: a 4096-byte write from a `Vec` at offset 0
//! is `EINVAL`, the same bytes from a `DmaBuf` are written.

use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use io_uring::{IoUring, opcode, types};
use uuid::Uuid;

use super::dma::DmaBuf;
use super::{BlockDevice, DeviceId, DriveError, DriveResult, DriveType, SmartData};

/// A SAS/SATA block device accessed via io_uring.
pub struct SasDevice {
    fd: RawFd,
    ring: std::sync::Mutex<IoUring>,
    id: DeviceId,
    capacity: u64,
    block_size: u32,
    device_type: DriveType,
    tag_counter: AtomicU64,
}

impl SasDevice {
    /// Open a block device at `path` with O_DIRECT.
    pub async fn open(path: &str) -> DriveResult<Self> {
        let path = path.to_string();
        // Open on a blocking thread since it may involve kernel work.
        let (fd, capacity, block_size) = tokio::task::spawn_blocking({
            let path = path.clone();
            move || -> DriveResult<(RawFd, u64, u32)> {
                use nix::fcntl::{open, OFlag};
                use nix::sys::stat::Mode;

                let flags = OFlag::O_RDWR | OFlag::O_DIRECT;
                let fd = open(path.as_str(), flags, Mode::empty())
                    .map_err(|e| DriveError::Io(e.into()))?;

                let capacity = ioctl_blkgetsize64(fd)?;
                let block_size = ioctl_blksszget(fd)?;

                Ok((fd, capacity, block_size))
            }
        })
        .await
        .map_err(|e| DriveError::Other(e.into()))??;

        // Read serial/model from sysfs if possible.
        let (serial, model) = read_device_identity(&path);

        // Detect SSD vs HDD via rotational flag.
        let device_type = detect_drive_type(&path);

        // Create io_uring instance.
        let ring = IoUring::builder()
            .build(256)
            .map_err(DriveError::Io)?;

        let id = DeviceId {
            uuid: Uuid::new_v4(),
            serial,
            model,
            path,
        };

        Ok(SasDevice {
            fd,
            ring: std::sync::Mutex::new(ring),
            id,
            capacity,
            block_size,
            device_type,
            tag_counter: AtomicU64::new(0),
        })
    }

    fn next_tag(&self) -> u64 {
        self.tag_counter.fetch_add(1, Ordering::Relaxed)
    }

    /// The caller's half of the `O_DIRECT` contract: offset and length are
    /// whole logical blocks. Anything else is a request the drive cannot
    /// perform, and it is named as such rather than surfacing as the
    /// kernel's bare `EINVAL`.
    fn check_request(&self, offset: u64, len: usize) -> DriveResult<()> {
        let bs = u64::from(self.block_size);
        if offset % bs != 0 {
            return Err(DriveError::NotAligned { offset, block_size: self.block_size });
        }
        if len as u64 % bs != 0 {
            return Err(DriveError::NotAligned { offset: len as u64, block_size: self.block_size });
        }
        Ok(())
    }

    /// Whether `O_DIRECT` will take a buffer at this address as it is.
    fn buffer_is_direct(&self, ptr: *const u8) -> bool {
        ptr as usize % self.block_size as usize == 0
    }

    /// Submit one write and wait for it. The buffer must satisfy `O_DIRECT`.
    fn submit_write(&self, offset: u64, ptr: *const u8, len: usize) -> DriveResult<usize> {
        let fd = self.fd;
        let tag = self.next_tag();
        let mut ring = self.ring.lock().unwrap();

        let sqe = opcode::Write::new(types::Fd(fd), ptr, len as u32)
            .offset(offset)
            .build()
            .user_data(tag);

        // Safety: SQE references a valid fd and a buffer that outlives the
        // wait below.
        unsafe { ring.submission().push(&sqe).map_err(|_| DriveError::DeviceNotReady)?; }

        ring.submit_and_wait(1).map_err(DriveError::Io)?;

        let cqe = ring.completion().next().ok_or(DriveError::DeviceNotReady)?;
        let result = cqe.result();
        if result < 0 {
            return Err(DriveError::Io(std::io::Error::from_raw_os_error(-result)));
        }
        Ok(result as usize)
    }

    /// Submit one read and wait for it. The buffer must satisfy `O_DIRECT`.
    fn submit_read(&self, offset: u64, ptr: *mut u8, len: usize) -> DriveResult<usize> {
        let fd = self.fd;
        let tag = self.next_tag();
        let mut ring = self.ring.lock().unwrap();

        let sqe = opcode::Read::new(types::Fd(fd), ptr, len as u32)
            .offset(offset)
            .build()
            .user_data(tag);

        // Safety: SQE references a valid fd and a buffer that outlives the
        // wait below.
        unsafe { ring.submission().push(&sqe).map_err(|_| DriveError::DeviceNotReady)?; }

        ring.submit_and_wait(1).map_err(DriveError::Io)?;

        let cqe = ring.completion().next().ok_or(DriveError::DeviceNotReady)?;
        let result = cqe.result();
        if result < 0 {
            return Err(DriveError::Io(std::io::Error::from_raw_os_error(-result)));
        }
        Ok(result as usize)
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
        self.check_request(offset, buf.len())?;
        if buf.is_empty() {
            return Ok(0);
        }
        if self.buffer_is_direct(buf.as_ptr()) {
            return self.submit_read(offset, buf.as_mut_ptr(), buf.len());
        }
        // Not where O_DIRECT needs it: read into a page-aligned buffer and
        // copy out. The copy is the price of a caller that did not allocate
        // for DMA, and it is paid here rather than as EINVAL at the caller.
        let mut bounce = DmaBuf::alloc(buf.len());
        let n = self.submit_read(offset, bounce.as_mut_ptr(), buf.len())?;
        buf[..n].copy_from_slice(&bounce[..n]);
        Ok(n)
    }

    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        self.check_request(offset, buf.len())?;
        if buf.is_empty() {
            return Ok(0);
        }
        if self.buffer_is_direct(buf.as_ptr()) {
            return self.submit_write(offset, buf.as_ptr(), buf.len());
        }
        let mut bounce = DmaBuf::alloc(buf.len());
        bounce[..buf.len()].copy_from_slice(buf);
        self.submit_write(offset, bounce.as_ptr(), buf.len())
    }

    async fn flush(&self) -> DriveResult<()> {
        let fd = self.fd;
        let tag = self.next_tag();

        let mut ring = self.ring.lock().unwrap();

        let sqe = opcode::Fsync::new(types::Fd(fd))
            .build()
            .user_data(tag);

        unsafe { ring.submission().push(&sqe).map_err(|_| DriveError::DeviceNotReady)?; }

        ring.submit_and_wait(1)
            .map_err(DriveError::Io)?;

        let cqe = ring.completion().next()
            .ok_or(DriveError::DeviceNotReady)?;

        let result = cqe.result();
        if result < 0 {
            return Err(DriveError::Io(std::io::Error::from_raw_os_error(-result)));
        }
        Ok(())
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

        // The caller's half is still the caller's: a sub-block length is a
        // request the drive cannot perform, and is named, not EINVAL.
        assert!(matches!(
            dev.write(4096, &pattern[..1024]).await,
            Err(DriveError::NotAligned { .. })
        ));
        assert!(matches!(
            dev.write(1024, &pattern[..4096]).await,
            Err(DriveError::NotAligned { .. })
        ));
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
