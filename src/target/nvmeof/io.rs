//! NVMe I/O commands — Read, Write, Flush, Dataset Management (TRIM).

use std::sync::Arc;

use crate::drive::BlockDevice;

use super::pdu::{NvmeSqe, NvmeCqe};

/// NVMe I/O opcodes.
pub const IO_FLUSH: u8 = 0x00;
pub const IO_WRITE: u8 = 0x01;
pub const IO_READ: u8 = 0x02;
pub const IO_DATASET_MGMT: u8 = 0x09;

/// Result of an NVMe I/O command.
pub struct IoResult {
    pub cqe: NvmeCqe,
    pub data: Vec<u8>, // Read data to send back
}

/// Handle an NVMe I/O command.
pub async fn handle_io_command(
    sqe: &NvmeSqe,
    device: &Arc<dyn BlockDevice>,
    inline_data: &[u8],
) -> IoResult {
    let cid = sqe.cid();
    let opcode = sqe.opcode();

    match opcode {
        IO_READ => handle_read(sqe, device, cid).await,
        IO_WRITE => handle_write(sqe, device, inline_data, cid).await,
        IO_FLUSH => handle_flush(device, cid).await,
        IO_DATASET_MGMT => handle_dataset_mgmt(sqe, device, inline_data, cid).await,
        _ => {
            tracing::debug!("unsupported NVMe I/O opcode: {opcode:#04x}");
            IoResult {
                cqe: NvmeCqe::error(cid, 0, 0, 0, 0x01), // Invalid Opcode
                data: Vec::new(),
            }
        }
    }
}

async fn handle_read(sqe: &NvmeSqe, device: &Arc<dyn BlockDevice>, cid: u16) -> IoResult {
    let slba = ((sqe.cdw11() as u64) << 32) | sqe.cdw10() as u64;
    let nlb = (sqe.cdw12() & 0xFFFF) as u64 + 1; // 0-based
    let bs = device.block_size() as u64;
    let offset = slba * bs;
    let len = nlb * bs;

    if offset + len > device.capacity_bytes() {
        return IoResult {
            cqe: NvmeCqe::error(cid, 0, 0, 0, 0x80), // LBA Out of Range
            data: Vec::new(),
        };
    }

    let mut buf = vec![0u8; len as usize];
    match device.read(offset, &mut buf).await {
        Ok(_) => IoResult {
            cqe: NvmeCqe::success(cid, 0, 0),
            data: buf,
        },
        Err(e) => {
            tracing::error!("NVMe read error at LBA {slba}: {e}");
            IoResult {
                cqe: NvmeCqe::error(cid, 0, 0, 2, 0x81), // Internal Error
                data: Vec::new(),
            }
        }
    }
}

async fn handle_write(
    sqe: &NvmeSqe,
    device: &Arc<dyn BlockDevice>,
    data: &[u8],
    cid: u16,
) -> IoResult {
    let slba = ((sqe.cdw11() as u64) << 32) | sqe.cdw10() as u64;
    let nlb = (sqe.cdw12() & 0xFFFF) as u64 + 1;
    let bs = device.block_size() as u64;
    let offset = slba * bs;
    let expected_len = (nlb * bs) as usize;

    if offset + expected_len as u64 > device.capacity_bytes() {
        return IoResult {
            cqe: NvmeCqe::error(cid, 0, 0, 0, 0x80),
            data: Vec::new(),
        };
    }

    if data.len() < expected_len {
        tracing::warn!("NVMe write: insufficient data ({} < {expected_len})", data.len());
        return IoResult {
            cqe: NvmeCqe::error(cid, 0, 0, 0, 0x02), // Invalid Field
            data: Vec::new(),
        };
    }

    match device.write(offset, &data[..expected_len]).await {
        Ok(_) => IoResult {
            cqe: NvmeCqe::success(cid, 0, 0),
            data: Vec::new(),
        },
        Err(e) => {
            tracing::error!("NVMe write error at LBA {slba}: {e}");
            IoResult {
                cqe: NvmeCqe::error(cid, 0, 0, 2, 0x81),
                data: Vec::new(),
            }
        }
    }
}

async fn handle_flush(device: &Arc<dyn BlockDevice>, cid: u16) -> IoResult {
    match device.flush().await {
        Ok(()) => IoResult {
            cqe: NvmeCqe::success(cid, 0, 0),
            data: Vec::new(),
        },
        Err(e) => {
            tracing::error!("NVMe flush error: {e}");
            IoResult {
                cqe: NvmeCqe::error(cid, 0, 0, 2, 0x81),
                data: Vec::new(),
            }
        }
    }
}

async fn handle_dataset_mgmt(
    sqe: &NvmeSqe,
    device: &Arc<dyn BlockDevice>,
    data: &[u8],
    cid: u16,
) -> IoResult {
    // Check AD (Attribute Deallocate) bit in cdw11
    let ad = sqe.cdw11() & 0x04 != 0;
    if !ad {
        // Not a deallocate command, nothing to do
        return IoResult {
            cqe: NvmeCqe::success(cid, 0, 0),
            data: Vec::new(),
        };
    }

    let nr = (sqe.cdw10() & 0xFF) as usize + 1; // number of ranges (0-based)
    let bs = device.block_size() as u64;

    // Each range is 16 bytes: 4 bytes context attributes, 4 bytes LBA count, 8 bytes starting LBA
    for i in 0..nr {
        let offset = i * 16;
        if offset + 16 > data.len() { break; }
        let lba_count = u32::from_le_bytes(data[offset + 4..offset + 8].try_into().unwrap()) as u64;
        let slba = u64::from_le_bytes(data[offset + 8..offset + 16].try_into().unwrap());
        if lba_count > 0 {
            let _ = device.discard(slba * bs, lba_count * bs).await;
        }
    }

    IoResult {
        cqe: NvmeCqe::success(cid, 0, 0),
        data: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;

    async fn test_device() -> (Arc<dyn BlockDevice>, String) {
        let dir = std::env::temp_dir().join("stormblock-nvme-io-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.bin", uuid::Uuid::new_v4().simple()));
        let path_str = path.to_str().unwrap().to_string();
        let dev = FileDevice::open_with_capacity(&path_str, 1024 * 1024).await.unwrap();
        (Arc::new(dev), path_str)
    }

    fn make_sqe(opcode: u8, nsid: u32, slba: u64, nlb: u16) -> NvmeSqe {
        let mut raw = [0u8; 64];
        raw[0] = opcode;
        raw[2..4].copy_from_slice(&1u16.to_le_bytes()); // CID=1
        raw[4..8].copy_from_slice(&nsid.to_le_bytes());
        raw[40..44].copy_from_slice(&(slba as u32).to_le_bytes()); // CDW10 = SLBA low
        raw[44..48].copy_from_slice(&((slba >> 32) as u32).to_le_bytes()); // CDW11 = SLBA high
        raw[48..52].copy_from_slice(&((nlb - 1) as u32).to_le_bytes()); // CDW12 = NLB (0-based)
        NvmeSqe::from_bytes(&raw)
    }

    #[tokio::test]
    async fn nvme_read_write_roundtrip() {
        let (dev, path) = test_device().await;

        // Write 1 block at LBA 0
        let write_data = vec![0xBEu8; 4096];
        let sqe = make_sqe(IO_WRITE, 1, 0, 1);
        let result = handle_io_command(&sqe, &dev, &write_data).await;
        let status = u16::from_le_bytes([result.cqe.raw[14], result.cqe.raw[15]]);
        assert_eq!(status & 0xFFFE, 0); // success

        // Read it back
        let sqe = make_sqe(IO_READ, 1, 0, 1);
        let result = handle_io_command(&sqe, &dev, &[]).await;
        assert_eq!(result.data.len(), 4096);
        assert!(result.data.iter().all(|&b| b == 0xBE));

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn nvme_read_out_of_range() {
        let (dev, path) = test_device().await;
        let sqe = make_sqe(IO_READ, 1, 0xFFFFFF, 1);
        let result = handle_io_command(&sqe, &dev, &[]).await;
        let status = u16::from_le_bytes([result.cqe.raw[14], result.cqe.raw[15]]);
        assert_ne!(status & 0xFFFE, 0); // error
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn nvme_flush() {
        let (dev, path) = test_device().await;
        let mut raw = [0u8; 64];
        raw[0] = IO_FLUSH;
        raw[2..4].copy_from_slice(&1u16.to_le_bytes());
        let sqe = NvmeSqe::from_bytes(&raw);
        let result = handle_io_command(&sqe, &dev, &[]).await;
        let status = u16::from_le_bytes([result.cqe.raw[14], result.cqe.raw[15]]);
        assert_eq!(status & 0xFFFE, 0);
        let _ = std::fs::remove_file(&path);
    }
}
