//! SAS/SATA block device access via io_uring with O_DIRECT.
//!
//! Opens /dev/sdX block devices with O_DIRECT for aligned DMA I/O.
//! Uses io_uring for async submission/completion.

use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use io_uring::{IoUring, opcode, types};
use uuid::Uuid;

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
            .map_err(|e| DriveError::Io(e))?;

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
        let fd = self.fd;
        let tag = self.next_tag();
        let buf_ptr = buf.as_mut_ptr();
        let buf_len = buf.len() as u32;

        let mut ring = self.ring.lock().unwrap();

        let sqe = opcode::Read::new(types::Fd(fd), buf_ptr, buf_len)
            .offset(offset)
            .build()
            .user_data(tag);

        // Safety: SQE references valid fd and buffer.
        unsafe { ring.submission().push(&sqe).map_err(|_| DriveError::DeviceNotReady)?; }

        ring.submit_and_wait(1)
            .map_err(|e| DriveError::Io(e))?;

        let cqe = ring.completion().next()
            .ok_or(DriveError::DeviceNotReady)?;

        let result = cqe.result();
        if result < 0 {
            return Err(DriveError::Io(std::io::Error::from_raw_os_error(-result)));
        }
        Ok(result as usize)
    }

    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        let fd = self.fd;
        let tag = self.next_tag();
        let buf_ptr = buf.as_ptr();
        let buf_len = buf.len() as u32;

        let mut ring = self.ring.lock().unwrap();

        let sqe = opcode::Write::new(types::Fd(fd), buf_ptr, buf_len)
            .offset(offset)
            .build()
            .user_data(tag);

        unsafe { ring.submission().push(&sqe).map_err(|_| DriveError::DeviceNotReady)?; }

        ring.submit_and_wait(1)
            .map_err(|e| DriveError::Io(e))?;

        let cqe = ring.completion().next()
            .ok_or(DriveError::DeviceNotReady)?;

        let result = cqe.result();
        if result < 0 {
            return Err(DriveError::Io(std::io::Error::from_raw_os_error(-result)));
        }
        Ok(result as usize)
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
            .map_err(|e| DriveError::Io(e))?;

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
