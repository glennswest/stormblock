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
    _tag_counter: AtomicU64,
}

impl FileDevice {
    /// Open or create a file-backed block device.
    pub async fn open(path: &str) -> DriveResult<Self> {
        let pb = PathBuf::from(path);

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&pb)
            .await
            .map_err(DriveError::Io)?;

        let metadata = file.metadata().await.map_err(DriveError::Io)?;
        let capacity = metadata.len();

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

    async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        let mut file = self.file.lock().await;
        file.seek(SeekFrom::Start(offset)).await.map_err(DriveError::Io)?;
        let n = file.read(buf).await.map_err(DriveError::Io)?;
        Ok(n)
    }

    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        let mut file = self.file.lock().await;
        file.seek(SeekFrom::Start(offset)).await.map_err(DriveError::Io)?;
        let n = file.write(buf).await.map_err(DriveError::Io)?;
        Ok(n)
    }

    async fn flush(&self) -> DriveResult<()> {
        let file = self.file.lock().await;
        file.sync_all().await.map_err(DriveError::Io)?;
        Ok(())
    }

    async fn discard(&self, _offset: u64, _len: u64) -> DriveResult<()> {
        // No-op for file devices. Could use fallocate(PUNCH_HOLE) on Linux.
        Ok(())
    }

    fn smart_status(&self) -> DriveResult<SmartData> {
        Ok(SmartData { healthy: true, ..Default::default() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
