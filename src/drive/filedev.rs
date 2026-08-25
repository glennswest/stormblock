//! File-backed block device — portable fallback using tokio async I/O.
//!
//! Used on MikroTik RouterOS (no io_uring), macOS development, and for
//! testing with regular files. Works with both block devices and regular files.

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;

use async_trait::async_trait;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{BlockDevice, DeviceId, DriveError, DriveResult, DriveType, SmartData};

/// A file-backed block device using tokio async file I/O.
///
/// Does not use O_DIRECT — relies on the kernel page cache.
/// Suitable for MikroTik, development, testing, and non-performance-critical paths.
pub struct FileDevice {
    file: Mutex<File>,
    id: DeviceId,
    capacity: u64,
    /// True when the path is a block device rather than a regular file —
    /// discard then means BLKDISCARD, not hole punching. Only consulted on
    /// Linux, where discard is actually implemented.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    is_block_device: bool,
    _tag_counter: AtomicU64,
}

impl FileDevice {
    /// Open or create a file-backed block device.
    pub async fn open(path: &str) -> DriveResult<Self> {
        let pb = PathBuf::from(path);

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&pb)
            .await
            .map_err(DriveError::Io)?;

        let metadata = file.metadata().await.map_err(DriveError::Io)?;
        let is_block_device = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileTypeExt;
                metadata.file_type().is_block_device()
            }
            #[cfg(not(unix))]
            {
                false
            }
        };
        // A block device node has `st_size` 0 — its size is the *device's*, not
        // the inode's, and `stat` does not know it. Seeking to the end does:
        // the kernel answers a block device's `lseek(END)` with its capacity.
        //
        // Taking `metadata.len()` for a block device therefore reported zero
        // bytes, and the failure that produced was not "capacity is wrong" but
        // "this disk has no GPT": `Gpt::read` tries each candidate LBA size and
        // skips any device too small to hold a header, so a capacity of zero
        // skipped every one of them. The node booted anyway for as long as the
        // command line named the slab's partition directly and nothing had to
        // read the table — and stopped booting the moment a pallet was added
        // and the slab had to be *found*.
        let capacity = if is_block_device {
            let size = file.seek(SeekFrom::End(0)).await.map_err(DriveError::Io)?;
            file.seek(SeekFrom::Start(0)).await.map_err(DriveError::Io)?;
            size
        } else {
            metadata.len()
        };

        let id = DeviceId {
            uuid: Uuid::new_v4(),
            serial: "file".to_string(),
            model: "FileDevice".to_string(),
            path: path.to_string(),
        };

        Ok(FileDevice {
            file: Mutex::new(file),
            id,
            capacity,
            is_block_device,
            _tag_counter: AtomicU64::new(0),
        })
    }

    /// Open with a pre-set capacity (for sparse/thin files).
    pub async fn open_with_capacity(path: &str, capacity: u64) -> DriveResult<Self> {
        let mut dev = Self::open(path).await?;
        if dev.capacity < capacity {
            // Extend the file to the desired capacity.
            let file = dev.file.lock().await;
            file.set_len(capacity).await.map_err(DriveError::Io)?;
            drop(file);
            dev.capacity = capacity;
        }
        Ok(dev)
    }
}

#[async_trait]
impl BlockDevice for FileDevice {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn capacity_bytes(&self) -> u64 {
        self.capacity
    }

    fn block_size(&self) -> u32 {
        4096
    }

    fn optimal_io_size(&self) -> u32 {
        4096
    }

    fn device_type(&self) -> DriveType {
        DriveType::File
    }

    /// One call transfers the whole buffer, which is what `BlockDevice`
    /// promises and what every caller above here assumes.
    ///
    /// A single `tokio::fs::File` read or write moves at most 2 MiB — its
    /// internal buffer cap — and reports the short count rather than failing.
    /// A caller that takes that count for the whole transfer silently keeps
    /// whatever was already in the rest of the buffer: a 4 MiB slab slot
    /// copied for copy-on-write arrived half copied, so every clone lost the
    /// data in the second half of any slot it wrote to.
    async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        let mut file = self.file.lock().await;
        file.seek(SeekFrom::Start(offset)).await.map_err(DriveError::Io)?;
        let mut done = 0usize;
        while done < buf.len() {
            // Zero is end of file, not an error: a read that runs off the end
            // of the backing file is short, and the count says so.
            match file.read(&mut buf[done..]).await.map_err(DriveError::Io)? {
                0 => break,
                n => done += n,
            }
        }
        Ok(done)
    }

    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        let mut file = self.file.lock().await;
        file.seek(SeekFrom::Start(offset)).await.map_err(DriveError::Io)?;
        file.write_all(buf).await.map_err(DriveError::Io)?;
        Ok(buf.len())
    }

    async fn flush(&self) -> DriveResult<()> {
        let file = self.file.lock().await;
        file.sync_all().await.map_err(DriveError::Io)?;
        Ok(())
    }

    /// Release the backing store for a range.
    ///
    /// Without this the slab can free a slot while the host file keeps every
    /// byte it ever touched, so a clone-per-container workload grows forever
    /// no matter how much is deleted inside the guest.
    ///
    /// Regular files are punched with `FALLOC_FL_PUNCH_HOLE`; `KEEP_SIZE`
    /// keeps the apparent length so slab offsets stay valid and only the
    /// allocated blocks go back. Block devices get `BLKDISCARD` instead,
    /// since hole punching is meaningless there.
    async fn discard(&self, offset: u64, len: u64) -> DriveResult<()> {
        if len == 0 {
            return Ok(());
        }

        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;

            let fd = {
                let file = self.file.lock().await;
                file.as_raw_fd()
            };
            let is_block = self.is_block_device;

            // errno is thread-local, so it has to be read on the blocking
            // thread that made the call, not after the await.
            let result = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                let rc = if is_block {
                    let range: [u64; 2] = [offset, len];
                    // BLKDISCARD = _IO(0x12, 119)
                    unsafe { libc::ioctl(fd, 0x1277, &range) }
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
                if rc != 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            })
            .await
            .map_err(|e| DriveError::Other(anyhow::anyhow!("discard task failed: {e}")))?;

            if let Err(e) = result {
                // A filesystem or device that cannot discard is not a failure —
                // the range is still logically free, it just keeps its blocks.
                return match e.raw_os_error() {
                    Some(libc::EOPNOTSUPP) | Some(libc::ENOSYS) | Some(libc::ENOTTY) => {
                        tracing::debug!("discard unsupported on {}: {e}", self.id.path);
                        Ok(())
                    }
                    _ => Err(DriveError::Io(e)),
                };
            }
            Ok(())
        }

        #[cfg(not(target_os = "linux"))]
        {
            // No portable hole-punch; the range stays allocated but is
            // logically free, which is correct if wasteful.
            let _ = (offset, len);
            Ok(())
        }
    }

    fn smart_status(&self) -> DriveResult<SmartData> {
        Ok(SmartData { healthy: true, ..Default::default() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One call, one whole buffer — even past the 2 MiB a single
    /// `tokio::fs::File` transfer moves.
    #[tokio::test]
    async fn transfers_larger_than_one_tokio_buffer_complete() {
        const BIG: usize = 5 * 1024 * 1024;
        let dir = std::env::temp_dir().join("stormblock-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test-filedev-big.bin");
        let _ = std::fs::remove_file(&path);

        let dev = FileDevice::open_with_capacity(path.to_str().unwrap(), 8 * 1024 * 1024)
            .await
            .unwrap();

        let pattern: Vec<u8> = (0..BIG).map(|i| (i % 251) as u8).collect();
        assert_eq!(dev.write(0, &pattern).await.unwrap(), BIG, "short write reported as complete");

        let mut back = vec![0u8; BIG];
        assert_eq!(dev.read(0, &mut back).await.unwrap(), BIG, "short read reported as complete");
        assert_eq!(back, pattern);

        // A read that runs off the end is legitimately short, and says so.
        let mut past = vec![0u8; 4096];
        let n = dev.read(8 * 1024 * 1024 - 1024, &mut past).await.unwrap();
        assert_eq!(n, 1024);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn roundtrip_write_read() {
        let dir = std::env::temp_dir().join("stormblock-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test-filedev.bin");
        let path_str = path.to_str().unwrap();

        // Clean up from previous run.
        let _ = std::fs::remove_file(&path);

        let dev = FileDevice::open_with_capacity(path_str, 1024 * 1024).await.unwrap();
        assert_eq!(dev.capacity_bytes(), 1024 * 1024);
        assert_eq!(dev.device_type(), DriveType::File);

        // Write a pattern at offset 0.
        let write_buf = vec![0xABu8; 4096];
        let written = dev.write(0, &write_buf).await.unwrap();
        assert_eq!(written, 4096);

        // Write a different pattern at offset 4096.
        let write_buf2 = vec![0xCDu8; 4096];
        let written2 = dev.write(4096, &write_buf2).await.unwrap();
        assert_eq!(written2, 4096);

        // Flush.
        dev.flush().await.unwrap();

        // Read back offset 0.
        let mut read_buf = vec![0u8; 4096];
        let read = dev.read(0, &mut read_buf).await.unwrap();
        assert_eq!(read, 4096);
        assert!(read_buf.iter().all(|&b| b == 0xAB));

        // Read back offset 4096.
        let mut read_buf2 = vec![0u8; 4096];
        let read2 = dev.read(4096, &mut read_buf2).await.unwrap();
        assert_eq!(read2, 4096);
        assert!(read_buf2.iter().all(|&b| b == 0xCD));

        // Discard is no-op but should not error.
        dev.discard(0, 4096).await.unwrap();

        // Clean up.
        drop(dev);
        let _ = std::fs::remove_file(&path);
    }
}
