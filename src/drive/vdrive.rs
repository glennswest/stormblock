//! VDrive — offset-translating BlockDevice wrapper over a parent device.
//!
//! A VDrive represents a sub-region of a DiskPool's underlying device.
//! All I/O is delegated to the parent device with an offset translation.

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use super::{BlockDevice, DeviceId, DriveError, DriveResult, DriveType, SmartData};

/// A virtual drive backed by a sub-region of a parent device.
pub struct VDrive {
    id: DeviceId,
    pool_uuid: Uuid,
    parent: Arc<dyn BlockDevice>,
    start_offset: u64,
    size: u64,
    label: String,
}

impl VDrive {
    /// Create a new VDrive with a region of the parent device.
    pub fn new(
        pool_uuid: Uuid,
        parent: Arc<dyn BlockDevice>,
        start_offset: u64,
        size: u64,
        label: String,
    ) -> Self {
        let id = DeviceId {
            uuid: Uuid::new_v4(),
            serial: format!("vdrive-{}", &label),
            model: "VDrive".to_string(),
            path: format!("pool:{}:{}", pool_uuid, label),
        };
        VDrive {
            id,
            pool_uuid,
            parent,
            start_offset,
            size,
            label,
        }
    }

    /// Pool UUID this VDrive belongs to.
    pub fn pool_uuid(&self) -> Uuid {
        self.pool_uuid
    }

    /// Label for this VDrive.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Start offset within the parent device.
    pub fn start_offset(&self) -> u64 {
        self.start_offset
    }

    fn translate_offset(&self, offset: u64, len: u64) -> DriveResult<u64> {
        if offset + len > self.size {
            return Err(DriveError::OutOfRange {
                offset,
                len,
                capacity: self.size,
            });
        }
        Ok(self.start_offset + offset)
    }
}

#[async_trait]
impl BlockDevice for VDrive {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn capacity_bytes(&self) -> u64 {
        self.size
    }

    fn block_size(&self) -> u32 {
        self.parent.block_size()
    }

    fn optimal_io_size(&self) -> u32 {
        self.parent.optimal_io_size()
    }

    fn device_type(&self) -> DriveType {
        DriveType::VDrive
    }

    async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        let phys = self.translate_offset(offset, buf.len() as u64)?;
        self.parent.read(phys, buf).await
    }

    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        let phys = self.translate_offset(offset, buf.len() as u64)?;
        self.parent.write(phys, buf).await
    }

    async fn flush(&self) -> DriveResult<()> {
        self.parent.flush().await
    }

    async fn discard(&self, offset: u64, len: u64) -> DriveResult<()> {
        let phys = self.translate_offset(offset, len)?;
        self.parent.discard(phys, len).await
    }

    fn smart_status(&self) -> DriveResult<SmartData> {
        self.parent.smart_status()
    }

    fn media_errors(&self) -> u64 {
        self.parent.media_errors()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;

    #[tokio::test]
    async fn vdrive_offset_translation() {
        let dir = std::env::temp_dir().join("stormblock-vdrive-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vdrive-parent.bin");
        let path_str = path.to_str().unwrap();
        let _ = std::fs::remove_file(&path);

        let parent = Arc::new(
            FileDevice::open_with_capacity(path_str, 10 * 1024 * 1024).await.unwrap()
        );

        // VDrive at offset 1MB, size 2MB
        let vdrive = VDrive::new(
            Uuid::new_v4(),
            parent.clone(),
            1024 * 1024,
            2 * 1024 * 1024,
            "test".to_string(),
        );

        assert_eq!(vdrive.capacity_bytes(), 2 * 1024 * 1024);
        assert_eq!(vdrive.device_type(), DriveType::VDrive);

        // Write at VDrive offset 0 (parent offset 1MB)
        let data = vec![0xAB_u8; 4096];
        vdrive.write(0, &data).await.unwrap();

        // Read back via VDrive
        let mut buf = vec![0u8; 4096];
        vdrive.read(0, &mut buf).await.unwrap();
        assert_eq!(buf, data);

        // Verify it's at parent offset 1MB
        let mut parent_buf = vec![0u8; 4096];
        parent.read(1024 * 1024, &mut parent_buf).await.unwrap();
        assert_eq!(parent_buf, data);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn vdrive_bounds_check() {
        let dir = std::env::temp_dir().join("stormblock-vdrive-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vdrive-bounds.bin");
        let path_str = path.to_str().unwrap();
        let _ = std::fs::remove_file(&path);

        let parent = Arc::new(
            FileDevice::open_with_capacity(path_str, 10 * 1024 * 1024).await.unwrap()
        );

        let vdrive = VDrive::new(
            Uuid::new_v4(),
            parent,
            1024 * 1024,
            2 * 1024 * 1024,
            "bounded".to_string(),
        );

        // Write beyond VDrive boundary should fail
        let data = vec![0xFF_u8; 4096];
        let result = vdrive.write(2 * 1024 * 1024, &data).await;
        assert!(result.is_err());

        let _ = std::fs::remove_file(&path);
    }
}
