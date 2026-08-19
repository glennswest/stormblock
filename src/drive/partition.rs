//! A partition, as a `BlockDevice`.
//!
//! A window onto another device: reads and writes are clamped to the window
//! and offset by its start. That is what lets anything that formats a device —
//! a slab, a filesystem — be pointed at one partition of an image without
//! knowing it is inside one, and it is how an image is assembled out of the
//! same code paths that run against a real disk.

use std::sync::Arc;

use async_trait::async_trait;

use super::{BlockDevice, DeviceId, DriveError, DriveResult, DriveType, SmartData};

pub struct PartitionDevice {
    inner: Arc<dyn BlockDevice>,
    id: DeviceId,
    start: u64,
    len: u64,
}

impl PartitionDevice {
    /// `start` and `len` are byte offsets into `inner`, and both must be
    /// block-aligned — a partition that begins mid-block could not be written
    /// without a read-modify-write of a neighbour's bytes.
    pub fn new(inner: Arc<dyn BlockDevice>, start: u64, len: u64) -> DriveResult<Self> {
        let bs = inner.block_size() as u64;
        if start % bs != 0 {
            return Err(DriveError::NotAligned { offset: start, block_size: bs as u32 });
        }
        if len % bs != 0 {
            return Err(DriveError::NotAligned { offset: len, block_size: bs as u32 });
        }
        if start + len > inner.capacity_bytes() {
            return Err(DriveError::OutOfRange {
                offset: start,
                len,
                capacity: inner.capacity_bytes(),
            });
        }
        let parent = inner.id().clone();
        let id = DeviceId {
            uuid: uuid::Uuid::new_v4(),
            serial: format!("{}+{}", parent.serial, start),
            model: parent.model.clone(),
            path: format!("{}@{}", parent.path, start),
        };
        Ok(PartitionDevice { inner, id, start, len })
    }

    pub fn start(&self) -> u64 {
        self.start
    }

    fn check(&self, offset: u64, len: u64) -> DriveResult<()> {
        if offset + len > self.len {
            return Err(DriveError::OutOfRange { offset, len, capacity: self.len });
        }
        Ok(())
    }
}

#[async_trait]
impl BlockDevice for PartitionDevice {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn capacity_bytes(&self) -> u64 {
        self.len
    }

    fn block_size(&self) -> u32 {
        self.inner.block_size()
    }

    fn optimal_io_size(&self) -> u32 {
        self.inner.optimal_io_size()
    }

    fn discard_granularity(&self) -> u32 {
        self.inner.discard_granularity()
    }

    fn device_type(&self) -> DriveType {
        self.inner.device_type()
    }

    async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        self.check(offset, buf.len() as u64)?;
        self.inner.read(self.start + offset, buf).await
    }

    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        self.check(offset, buf.len() as u64)?;
        self.inner.write(self.start + offset, buf).await
    }

    async fn flush(&self) -> DriveResult<()> {
        self.inner.flush().await
    }

    async fn discard(&self, offset: u64, len: u64) -> DriveResult<()> {
        self.check(offset, len)?;
        self.inner.discard(self.start + offset, len).await
    }

    fn smart_status(&self) -> DriveResult<SmartData> {
        self.inner.smart_status()
    }

    fn media_errors(&self) -> u64 {
        self.inner.media_errors()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;

    async fn dev(dir: &tempfile::TempDir, len: u64) -> Arc<dyn BlockDevice> {
        let p = dir.path().join("disk.img");
        Arc::new(
            FileDevice::open_with_capacity(p.to_str().unwrap(), len)
                .await
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn a_window_reads_and_writes_only_its_own_bytes() {
        let dir = tempfile::TempDir::new().unwrap();
        let disk = dev(&dir, 1024 * 1024).await;
        let part = PartitionDevice::new(disk.clone(), 8192, 8192).unwrap();

        part.write(0, &[0xAB; 4096]).await.unwrap();
        part.flush().await.unwrap();

        // It landed at the window's start, and nowhere else.
        let mut before = [0u8; 4096];
        disk.read(4096, &mut before).await.unwrap();
        assert!(before.iter().all(|&b| b == 0), "wrote before the window");
        let mut at = [0u8; 4096];
        disk.read(8192, &mut at).await.unwrap();
        assert!(at.iter().all(|&b| b == 0xAB));
    }

    #[tokio::test]
    async fn a_window_cannot_be_read_or_written_past_its_end() {
        let dir = tempfile::TempDir::new().unwrap();
        let disk = dev(&dir, 1024 * 1024).await;
        let part = PartitionDevice::new(disk, 8192, 8192).unwrap();
        assert_eq!(part.capacity_bytes(), 8192);
        assert!(part.write(4096, &[0u8; 8192]).await.is_err());
        assert!(part.read(8192, &mut [0u8; 512]).await.is_err());
    }

    #[tokio::test]
    async fn an_unaligned_window_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let disk = dev(&dir, 1024 * 1024).await;
        assert!(PartitionDevice::new(disk.clone(), 100, 8192).is_err());
        assert!(PartitionDevice::new(disk, 8192, 100).is_err());
    }
}
