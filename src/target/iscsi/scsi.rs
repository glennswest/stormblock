//! SCSI command dispatch for iSCSI — INQUIRY, READ/WRITE, READ_CAPACITY, etc.
//!
//! Implements a minimal SBC-3 (SCSI Block Commands) target, enough for
//! Linux/Windows initiators to discover and use a disk.

use std::sync::Arc;

use crate::drive::BlockDevice;

/// SCSI operation codes.
pub const TEST_UNIT_READY: u8 = 0x00;
pub const INQUIRY: u8 = 0x12;
pub const MODE_SENSE_6: u8 = 0x1A;
pub const MODE_SENSE_10: u8 = 0x5A;
pub const READ_CAPACITY_10: u8 = 0x25;
pub const READ_CAPACITY_16: u8 = 0x9E; // service action 0x10
pub const READ_10: u8 = 0x28;
pub const READ_16: u8 = 0x88;
pub const WRITE_10: u8 = 0x2A;
pub const WRITE_16: u8 = 0x8A;
pub const SYNCHRONIZE_CACHE_10: u8 = 0x35;
pub const SYNCHRONIZE_CACHE_16: u8 = 0x91;
pub const UNMAP: u8 = 0x42;
pub const REPORT_LUNS: u8 = 0xA0;
pub const REQUEST_SENSE: u8 = 0x03;
pub const MAINTENANCE_IN: u8 = 0xA3;
pub const MAINTENANCE_OUT: u8 = 0xA4;

/// SCSI status codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ScsiStatus {
    Good = 0x00,
    CheckCondition = 0x02,
    Busy = 0x08,
}

/// Sense key values.
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum SenseKey {
    NoSense = 0x00,
    NotReady = 0x02,
    MediumError = 0x03,
    IllegalRequest = 0x05,
    UnitAttention = 0x06,
}

/// Fixed-format sense data (18 bytes minimum).
pub struct SenseData {
    pub key: SenseKey,
    pub asc: u8,   // Additional Sense Code
    pub ascq: u8,  // Additional Sense Code Qualifier
}

impl SenseData {
    pub fn illegal_request() -> Self {
        SenseData { key: SenseKey::IllegalRequest, asc: 0x20, ascq: 0x00 }
    }

    pub fn invalid_field_in_cdb() -> Self {
        SenseData { key: SenseKey::IllegalRequest, asc: 0x24, ascq: 0x00 }
    }

    pub fn medium_error() -> Self {
        SenseData { key: SenseKey::MediumError, asc: 0x11, ascq: 0x00 }
    }

    pub fn lba_out_of_range() -> Self {
        SenseData { key: SenseKey::IllegalRequest, asc: 0x21, ascq: 0x00 }
    }

    /// Encode as fixed-format sense data (18 bytes).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = vec![0u8; 18];
        buf[0] = 0x70; // response code: current errors, fixed format
        buf[2] = self.key as u8;
        buf[7] = 10; // additional sense length
        buf[12] = self.asc;
        buf[13] = self.ascq;
        buf
    }
}

/// Result from executing a SCSI command.
pub struct ScsiResult {
    pub status: ScsiStatus,
    pub data: Vec<u8>,
    pub sense: Option<SenseData>,
}

impl ScsiResult {
    pub fn good(data: Vec<u8>) -> Self {
        ScsiResult { status: ScsiStatus::Good, data, sense: None }
    }

    pub fn good_empty() -> Self {
        ScsiResult { status: ScsiStatus::Good, data: Vec::new(), sense: None }
    }

    pub fn check_condition(sense: SenseData) -> Self {
        let data = sense.to_bytes();
        ScsiResult { status: ScsiStatus::CheckCondition, data, sense: Some(sense) }
    }
}

/// Handle a SCSI command CDB and return the result.
///
/// `cdb` — 16-byte CDB from the iSCSI SCSI Command PDU.
/// `device` — the block device backing this LUN.
/// `data_out` — data sent by initiator (for write commands).
pub async fn handle_scsi_command(
    cdb: &[u8],
    device: &Arc<dyn BlockDevice>,
    data_out: &[u8],
) -> ScsiResult {
    if cdb.is_empty() {
        return ScsiResult::check_condition(SenseData::illegal_request());
    }

    let opcode = cdb[0];
    match opcode {
        TEST_UNIT_READY => ScsiResult::good_empty(),

        REQUEST_SENSE => handle_request_sense(cdb),

        INQUIRY => handle_inquiry(cdb, device),

        MODE_SENSE_6 => handle_mode_sense_6(cdb),

        MODE_SENSE_10 => handle_mode_sense_10(cdb),

        READ_CAPACITY_10 => handle_read_capacity_10(device),

        READ_CAPACITY_16 => handle_read_capacity_16(cdb, device),

        READ_10 => handle_read_10(cdb, device).await,

        READ_16 => handle_read_16(cdb, device).await,

        WRITE_10 => handle_write_10(cdb, device, data_out).await,

        WRITE_16 => handle_write_16(cdb, device, data_out).await,

        SYNCHRONIZE_CACHE_10 | SYNCHRONIZE_CACHE_16 => {
            match device.flush().await {
                Ok(()) => ScsiResult::good_empty(),
                Err(_) => ScsiResult::check_condition(SenseData::medium_error()),
            }
        }

        UNMAP => handle_unmap(device, data_out).await,

        REPORT_LUNS => handle_report_luns(),

        MAINTENANCE_IN => {
            let service_action = cdb[1] & 0x1F;
            if service_action == super::alua::SA_REPORT_TPG {
                let alloc_len = u32::from_be_bytes([cdb[6], cdb[7], cdb[8], cdb[9]]) as usize;
                let ctrl = super::alua::AluaController::new_single(vec![1]);
                let mut data = ctrl.report_target_port_groups();
                data.truncate(alloc_len);
                ScsiResult::good(data)
            } else {
                ScsiResult::check_condition(SenseData::illegal_request())
            }
        }

        MAINTENANCE_OUT => {
            let service_action = cdb[1] & 0x1F;
            if service_action == super::alua::SA_SET_TPG {
                let ctrl = super::alua::AluaController::new_single(vec![1]);
                ctrl.set_target_port_groups(data_out);
                ScsiResult::good_empty()
            } else {
                ScsiResult::check_condition(SenseData::illegal_request())
            }
        }

        _ => {
            tracing::debug!("unsupported SCSI opcode: {opcode:#04x}");
            ScsiResult::check_condition(SenseData::illegal_request())
        }
    }
}

fn handle_request_sense(_cdb: &[u8]) -> ScsiResult {
    // Return "no sense" — no pending errors
    let sense = SenseData { key: SenseKey::NoSense, asc: 0, ascq: 0 };
    ScsiResult::good(sense.to_bytes())
}

fn handle_inquiry(cdb: &[u8], device: &Arc<dyn BlockDevice>) -> ScsiResult {
    let evpd = cdb[1] & 0x01;
    let page_code = cdb[2];
    let alloc_len = u16::from_be_bytes([cdb[3], cdb[4]]) as usize;

    if evpd == 1 {
        return handle_inquiry_vpd(page_code, alloc_len, device);
    }

    // Standard INQUIRY response (36 bytes minimum)
    let mut data = vec![0u8; 96];
    data[0] = 0x00; // Peripheral qualifier=0, device type=0 (disk)
    data[1] = 0x00; // Not removable
    data[2] = 0x06; // SPC-4 version
    data[3] = 0x02; // Response data format = 2
    data[4] = 91;   // Additional length (96 - 5)
    data[5] = 0x10; // TPGS=01 (implicit ALUA)
    data[6] = 0x00;
    data[7] = 0x02; // CmdQue=1 (tagged command queuing)

    // T10 vendor identification (bytes 8-15)
    let vendor = b"StrmBlk ";
    data[8..16].copy_from_slice(vendor);

    // Product identification (bytes 16-31)
    let model = device.id().model.as_bytes();
    let model_field = &mut data[16..32];
    let copy_len = model.len().min(16);
    model_field[..copy_len].copy_from_slice(&model[..copy_len]);
    // Pad with spaces
    for b in &mut model_field[copy_len..] {
        *b = b' ';
    }

    // Product revision level (bytes 32-35)
    data[32..36].copy_from_slice(b"1.0 ");

    let len = data.len().min(alloc_len);
    data.truncate(len);
    ScsiResult::good(data)
}

fn handle_inquiry_vpd(page_code: u8, alloc_len: usize, device: &Arc<dyn BlockDevice>) -> ScsiResult {
    match page_code {
        // Supported VPD pages
        0x00 => {
            let mut data = vec![0u8; 7];
            data[0] = 0x00; // device type
            data[1] = 0x00; // page code
            data[3] = 3;    // page length
            data[4] = 0x00; // supported pages list
            data[5] = 0x83; // device identification
            data[6] = 0xB0; // block limits
            let len = data.len().min(alloc_len);
            data.truncate(len);
            ScsiResult::good(data)
        }
        // Device Identification (0x83)
        0x83 => {
            let serial = device.id().serial.as_bytes();
            let id_len = serial.len();
            let page_len = 4 + id_len;
            let mut data = vec![0u8; 4 + page_len];
            data[0] = 0x00;
            data[1] = 0x83;
            data[2] = ((page_len >> 8) & 0xff) as u8;
            data[3] = (page_len & 0xff) as u8;
            // Identifier descriptor
            data[4] = 0x02; // ASCII, NAA
            data[5] = 0x01; // T10 vendor ID
            data[6] = 0x00; // reserved
            data[7] = id_len as u8;
            data[8..8 + id_len].copy_from_slice(serial);
            let len = data.len().min(alloc_len);
            data.truncate(len);
            ScsiResult::good(data)
        }
        // Block Limits (0xB0)
        0xB0 => {
            let mut data = vec![0u8; 64];
            data[0] = 0x00;
            data[1] = 0xB0;
            data[3] = 0x3C; // page length = 60
            // Optimal transfer length granularity
            let bs = device.block_size();
            let optimal = device.optimal_io_size() / bs;
            data[6] = ((optimal >> 8) & 0xff) as u8;
            data[7] = (optimal & 0xff) as u8;
            // Maximum transfer length (64K blocks)
            let max_xfer: u32 = 65536;
            data[8..12].copy_from_slice(&max_xfer.to_be_bytes());
            // Optimal transfer length
            data[12..16].copy_from_slice(&(optimal as u32).to_be_bytes());
            // Maximum UNMAP LBA count
            data[20..24].copy_from_slice(&0xFFFFFFFFu32.to_be_bytes());
            // Maximum UNMAP block descriptor count
            data[24..28].copy_from_slice(&256u32.to_be_bytes());
            let len = data.len().min(alloc_len);
            data.truncate(len);
            ScsiResult::good(data)
        }
        _ => ScsiResult::check_condition(SenseData::invalid_field_in_cdb()),
    }
}

fn handle_mode_sense_6(cdb: &[u8]) -> ScsiResult {
    let page_code = cdb[2] & 0x3f;
    let alloc_len = cdb[4] as usize;

    // Minimal mode sense response
    let mut data = vec![0u8; 4]; // mode parameter header (6-byte)
    data[0] = 3; // mode data length (excluding itself)

    match page_code {
        // Caching mode page (0x08)
        0x08 => {
            let mut page = vec![0u8; 20];
            page[0] = 0x08; // page code
            page[1] = 18;   // page length
            page[2] = 0x04; // WCE=1 (write cache enabled)
            data.extend_from_slice(&page);
            data[0] = (data.len() - 1) as u8;
        }
        // All pages (0x3F)
        0x3F => {
            let mut page = vec![0u8; 20];
            page[0] = 0x08;
            page[1] = 18;
            page[2] = 0x04;
            data.extend_from_slice(&page);
            data[0] = (data.len() - 1) as u8;
        }
        _ => {}
    }

    let len = data.len().min(alloc_len);
    data.truncate(len);
    ScsiResult::good(data)
}

fn handle_mode_sense_10(cdb: &[u8]) -> ScsiResult {
    let page_code = cdb[2] & 0x3f;
    let alloc_len = u16::from_be_bytes([cdb[7], cdb[8]]) as usize;

    let mut data = vec![0u8; 8]; // mode parameter header (10-byte)

    match page_code {
        0x08 | 0x3F => {
            let mut page = vec![0u8; 20];
            page[0] = 0x08;
            page[1] = 18;
            page[2] = 0x04;
            data.extend_from_slice(&page);
            let len_minus_2 = (data.len() - 2) as u16;
            data[0] = (len_minus_2 >> 8) as u8;
            data[1] = (len_minus_2 & 0xff) as u8;
        }
        _ => {}
    }

    let len = data.len().min(alloc_len);
    data.truncate(len);
    ScsiResult::good(data)
}

fn handle_read_capacity_10(device: &Arc<dyn BlockDevice>) -> ScsiResult {
    let bs = device.block_size();
    let total_blocks = device.capacity_bytes() / bs as u64;
    // READ CAPACITY 10 returns last LBA (capped at 0xFFFFFFFF)
    let last_lba = if total_blocks > 0 {
        ((total_blocks - 1).min(0xFFFFFFFF)) as u32
    } else {
        0
    };

    let mut data = vec![0u8; 8];
    data[0..4].copy_from_slice(&last_lba.to_be_bytes());
    data[4..8].copy_from_slice(&bs.to_be_bytes());
    ScsiResult::good(data)
}

fn handle_read_capacity_16(cdb: &[u8], device: &Arc<dyn BlockDevice>) -> ScsiResult {
    // Service action must be 0x10 (READ CAPACITY 16)
    let service_action = cdb[1] & 0x1f;
    if service_action != 0x10 {
        return ScsiResult::check_condition(SenseData::illegal_request());
    }

    let bs = device.block_size();
    let total_blocks = device.capacity_bytes() / bs as u64;
    let last_lba = if total_blocks > 0 { total_blocks - 1 } else { 0 };
    let alloc_len = u32::from_be_bytes([cdb[10], cdb[11], cdb[12], cdb[13]]) as usize;

    let mut data = vec![0u8; 32];
    data[0..8].copy_from_slice(&last_lba.to_be_bytes());
    data[8..12].copy_from_slice(&bs.to_be_bytes());
    // Logical blocks per physical block exponent (byte 13)
    let lbppbe = (device.optimal_io_size() / bs).trailing_zeros() as u8;
    data[13] = lbppbe & 0x0f;
    // LBPME=1 (logical block provisioning management enabled — thin provisioning)
    data[14] = 0x80;

    let len = data.len().min(alloc_len);
    data.truncate(len);
    ScsiResult::good(data)
}

async fn handle_read_10(cdb: &[u8], device: &Arc<dyn BlockDevice>) -> ScsiResult {
    let lba = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]) as u64;
    let block_count = u16::from_be_bytes([cdb[7], cdb[8]]) as u64;
    do_read(lba, block_count, device).await
}

async fn handle_read_16(cdb: &[u8], device: &Arc<dyn BlockDevice>) -> ScsiResult {
    let lba = u64::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5], cdb[6], cdb[7], cdb[8], cdb[9]]);
    let block_count = u32::from_be_bytes([cdb[10], cdb[11], cdb[12], cdb[13]]) as u64;
    do_read(lba, block_count, device).await
}

async fn do_read(lba: u64, block_count: u64, device: &Arc<dyn BlockDevice>) -> ScsiResult {
    let bs = device.block_size() as u64;
    let offset = lba * bs;
    let len = block_count * bs;

    if offset + len > device.capacity_bytes() {
        return ScsiResult::check_condition(SenseData::lba_out_of_range());
    }

    let mut buf = vec![0u8; len as usize];
    match device.read(offset, &mut buf).await {
        Ok(_) => ScsiResult::good(buf),
        Err(_) => ScsiResult::check_condition(SenseData::medium_error()),
    }
}

async fn handle_write_10(cdb: &[u8], device: &Arc<dyn BlockDevice>, data: &[u8]) -> ScsiResult {
    let lba = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]) as u64;
    let block_count = u16::from_be_bytes([cdb[7], cdb[8]]) as u64;
    do_write(lba, block_count, device, data).await
}

async fn handle_write_16(cdb: &[u8], device: &Arc<dyn BlockDevice>, data: &[u8]) -> ScsiResult {
    let lba = u64::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5], cdb[6], cdb[7], cdb[8], cdb[9]]);
    let block_count = u32::from_be_bytes([cdb[10], cdb[11], cdb[12], cdb[13]]) as u64;
    do_write(lba, block_count, device, data).await
}

async fn do_write(lba: u64, block_count: u64, device: &Arc<dyn BlockDevice>, data: &[u8]) -> ScsiResult {
    let bs = device.block_size() as u64;
    let offset = lba * bs;
    let expected_len = (block_count * bs) as usize;

    if offset + expected_len as u64 > device.capacity_bytes() {
        return ScsiResult::check_condition(SenseData::lba_out_of_range());
    }

    if data.len() < expected_len {
        return ScsiResult::check_condition(SenseData::illegal_request());
    }

    match device.write(offset, &data[..expected_len]).await {
        Ok(_) => ScsiResult::good_empty(),
        Err(_) => ScsiResult::check_condition(SenseData::medium_error()),
    }
}

async fn handle_unmap(device: &Arc<dyn BlockDevice>, data: &[u8]) -> ScsiResult {
    // UNMAP parameter list: 8-byte header + 16-byte block descriptors
    if data.len() < 8 {
        return ScsiResult::check_condition(SenseData::illegal_request());
    }

    let desc_len = u16::from_be_bytes([data[2], data[3]]) as usize;
    let desc_data = &data[8..];
    let bs = device.block_size() as u64;

    let mut offset = 0;
    while offset + 16 <= desc_len && offset + 16 <= desc_data.len() {
        let lba = u64::from_be_bytes(desc_data[offset..offset + 8].try_into().unwrap());
        let count = u32::from_be_bytes(desc_data[offset + 8..offset + 12].try_into().unwrap()) as u64;
        if count > 0 {
            let _ = device.discard(lba * bs, count * bs).await;
        }
        offset += 16;
    }

    ScsiResult::good_empty()
}

fn handle_report_luns() -> ScsiResult {
    // Report a single LUN 0
    let mut data = vec![0u8; 16];
    // LUN list length (bytes 0-3) = 8 (one LUN entry)
    data[0..4].copy_from_slice(&8u32.to_be_bytes());
    // LUN 0 (bytes 8-15) = all zeros
    ScsiResult::good(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;

    async fn test_device() -> (Arc<dyn BlockDevice>, String) {
        let dir = std::env::temp_dir().join("stormblock-scsi-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.bin", uuid::Uuid::new_v4().simple()));
        let path_str = path.to_str().unwrap().to_string();
        let dev = FileDevice::open_with_capacity(&path_str, 1024 * 1024).await.unwrap();
        (Arc::new(dev), path_str)
    }

    #[tokio::test]
    async fn inquiry_response() {
        let (dev, path) = test_device().await;
        let cdb = [INQUIRY, 0, 0, 0, 96, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let result = handle_scsi_command(&cdb, &dev, &[]).await;
        assert_eq!(result.status, ScsiStatus::Good);
        assert!(result.data.len() >= 36);
        assert_eq!(&result.data[8..16], b"StrmBlk ");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_unit_ready() {
        let (dev, path) = test_device().await;
        let cdb = [TEST_UNIT_READY, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let result = handle_scsi_command(&cdb, &dev, &[]).await;
        assert_eq!(result.status, ScsiStatus::Good);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn read_capacity_10() {
        let (dev, path) = test_device().await;
        let cdb = [READ_CAPACITY_10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let result = handle_scsi_command(&cdb, &dev, &[]).await;
        assert_eq!(result.status, ScsiStatus::Good);
        assert_eq!(result.data.len(), 8);
        let last_lba = u32::from_be_bytes(result.data[0..4].try_into().unwrap());
        let block_size = u32::from_be_bytes(result.data[4..8].try_into().unwrap());
        assert_eq!(block_size, 4096);
        assert_eq!((last_lba as u64 + 1) * block_size as u64, 1024 * 1024);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn read_write_roundtrip() {
        let (dev, path) = test_device().await;

        // Write 1 block at LBA 0
        let write_data = vec![0xABu8; 4096];
        let cdb_w = [WRITE_10, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0];
        let result = handle_scsi_command(&cdb_w, &dev, &write_data).await;
        assert_eq!(result.status, ScsiStatus::Good);

        // Read it back
        let cdb_r = [READ_10, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0];
        let result = handle_scsi_command(&cdb_r, &dev, &[]).await;
        assert_eq!(result.status, ScsiStatus::Good);
        assert_eq!(result.data.len(), 4096);
        assert!(result.data.iter().all(|&b| b == 0xAB));

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn read_out_of_range() {
        let (dev, path) = test_device().await;
        // LBA way past capacity
        let cdb = [READ_10, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0];
        let result = handle_scsi_command(&cdb, &dev, &[]).await;
        assert_eq!(result.status, ScsiStatus::CheckCondition);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn report_luns() {
        let (dev, path) = test_device().await;
        let cdb = [REPORT_LUNS, 0, 0, 0, 0, 0, 0, 0, 0, 16, 0, 0, 0, 0, 0, 0];
        let result = handle_scsi_command(&cdb, &dev, &[]).await;
        assert_eq!(result.status, ScsiStatus::Good);
        assert_eq!(result.data.len(), 16);
        let _ = std::fs::remove_file(&path);
    }
}
