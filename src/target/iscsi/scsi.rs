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
pub const WRITE_SAME_10: u8 = 0x41;
pub const WRITE_SAME_16: u8 = 0x93;
pub const REPORT_LUNS: u8 = 0xA0;
pub const REQUEST_SENSE: u8 = 0x03;
pub const MAINTENANCE_IN: u8 = 0xA3;
pub const MAINTENANCE_OUT: u8 = 0xA4;

/// Commands that carry a data-out payload from the initiator.
///
/// UNMAP and WRITE SAME send a parameter list / pattern block; missing them
/// here means the payload is never collected and the command fails (#25).
pub fn is_data_out_command(opcode: u8) -> bool {
    matches!(
        opcode,
        WRITE_10 | WRITE_16 | WRITE_SAME_10 | WRITE_SAME_16 | UNMAP | MAINTENANCE_OUT
    )
}

/// Commands that modify media, and so must be refused on a readonly LUN.
pub fn modifies_media(opcode: u8) -> bool {
    matches!(
        opcode,
        WRITE_10 | WRITE_16 | WRITE_SAME_10 | WRITE_SAME_16 | UNMAP
    )
}

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

    pub fn write_protected() -> Self {
        SenseData { key: SenseKey::IllegalRequest, asc: 0x27, ascq: 0x00 }
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
/// `lun_ids` — active LUN IDs for REPORT LUNS.
pub async fn handle_scsi_command(
    cdb: &[u8],
    device: &Arc<dyn BlockDevice>,
    data_out: &[u8],
    lun_ids: &[u64],
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

        WRITE_SAME_10 => {
            let lba = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]) as u64;
            let block_count = u16::from_be_bytes([cdb[7], cdb[8]]) as u64;
            // UNMAP is bit 3 of byte 1 in WRITE SAME(10).
            handle_write_same(lba, block_count, cdb[1] & 0x08 != 0, device, data_out).await
        }

        WRITE_SAME_16 => {
            let lba = u64::from_be_bytes([
                cdb[2], cdb[3], cdb[4], cdb[5], cdb[6], cdb[7], cdb[8], cdb[9],
            ]);
            let block_count =
                u32::from_be_bytes([cdb[10], cdb[11], cdb[12], cdb[13]]) as u64;
            handle_write_same(lba, block_count, cdb[1] & 0x08 != 0, device, data_out).await
        }

        REPORT_LUNS => handle_report_luns(lun_ids, cdb),

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
            let pages: [u8; 4] = [
                0x00, // supported pages list
                0x83, // device identification
                0xB0, // block limits
                0xB2, // logical block provisioning
            ];
            let mut data = vec![0u8; 4 + pages.len()];
            data[0] = 0x00; // device type
            data[1] = 0x00; // page code
            data[3] = pages.len() as u8;
            data[4..4 + pages.len()].copy_from_slice(&pages);
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
            data[12..16].copy_from_slice(&optimal.to_be_bytes());
            // Maximum UNMAP LBA count
            data[20..24].copy_from_slice(&0xFFFFFFFFu32.to_be_bytes());
            // Maximum UNMAP block descriptor count
            data[24..28].copy_from_slice(&256u32.to_be_bytes());
            // Optimal UNMAP granularity, in blocks. Storage is reclaimed a
            // whole slab slot at a time, so telling the initiator the real
            // granularity keeps its discards aligned and actually freeing
            // space (#25).
            let granularity = (device.discard_granularity() / bs).max(1);
            data[28..32].copy_from_slice(&granularity.to_be_bytes());
            // UNMAP granularity alignment: 0, valid (UGAVALID = bit 31).
            data[32..36].copy_from_slice(&0x8000_0000u32.to_be_bytes());
            let len = data.len().min(alloc_len);
            data.truncate(len);
            ScsiResult::good(data)
        }
        // Logical Block Provisioning (0xB2)
        //
        // Without this page a Linux initiator leaves discard_max_bytes at 0
        // and never issues UNMAP at all, so thin allocation only ever grows
        // (#25). Declaring it is what turns the reclaim path on.
        0xB2 => {
            let mut data = vec![0u8; 8];
            data[0] = 0x00; // device type
            data[1] = 0xB2; // page code
            data[3] = 0x04; // page length
            data[4] = 0x00; // threshold exponent — no thresholds reported
            // LBPU (bit 7): UNMAP supported.
            // LBPWS (bit 6): WRITE SAME(16) with UNMAP supported.
            // LBPWS10 (bit 5): WRITE SAME(10) with UNMAP supported.
            // LBPRZ (bit 2): unmapped blocks read back as zero — true here,
            // an extent with no GEM mapping reads as zeros.
            data[5] = 0b1110_0100;
            data[6] = 0x02; // provisioning type: thin provisioned
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
    // LBPME=1 (thin provisioned), LBPRZ=1 (unmapped blocks read as zero).
    data[14] = 0xC0;

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

/// WRITE SAME(10/16) — write one pattern block across a range.
///
/// With UNMAP set and an all-zero pattern this deallocates instead of
/// writing, which is how `blkdiscard -z` and some filesystems return space.
/// The zero check matters: a non-zero pattern must still be written out.
async fn handle_write_same(
    lba: u64,
    block_count: u64,
    unmap: bool,
    device: &Arc<dyn BlockDevice>,
    data: &[u8],
) -> ScsiResult {
    let bs = device.block_size() as u64;
    let offset = lba * bs;
    let len = block_count * bs;

    if offset + len > device.capacity_bytes() {
        return ScsiResult::check_condition(SenseData::lba_out_of_range());
    }
    // A block count of zero means "to the end of the device" in SBC; refuse
    // it rather than silently wiping the remainder.
    if block_count == 0 {
        return ScsiResult::check_condition(SenseData::invalid_field_in_cdb());
    }
    if data.len() < bs as usize {
        return ScsiResult::check_condition(SenseData::illegal_request());
    }

    let pattern = &data[..bs as usize];
    let is_zero = pattern.iter().all(|&b| b == 0);

    if unmap && is_zero {
        return match device.discard(offset, len).await {
            Ok(()) => ScsiResult::good_empty(),
            Err(_) => ScsiResult::check_condition(SenseData::medium_error()),
        };
    }

    // Write the pattern out in bounded chunks so a large range does not
    // materialize the whole span in memory at once.
    const MAX_CHUNK: u64 = 1024 * 1024;
    let blocks_per_chunk = (MAX_CHUNK / bs).max(1);
    let mut written = 0u64;
    while written < block_count {
        let chunk_blocks = blocks_per_chunk.min(block_count - written);
        let buf = pattern.repeat(chunk_blocks as usize);
        if device.write(offset + written * bs, &buf).await.is_err() {
            return ScsiResult::check_condition(SenseData::medium_error());
        }
        written += chunk_blocks;
    }

    ScsiResult::good_empty()
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

/// Encode a LUN number into the 2 significant bytes of a SAM-5 LUN field.
///
/// Peripheral addressing (method 00b) for LUN < 256, flat-space addressing
/// (method 01b) up to 16383 — the range a single-level LUN field can carry.
pub fn encode_lun(lun: u64) -> u16 {
    if lun < 256 {
        lun as u16
    } else {
        0x4000 | (lun as u16 & 0x3FFF)
    }
}

/// REPORT LUNS (SPC-4 §6.21).
///
/// `lun_ids` is expected sorted. The LUN LIST LENGTH field always reports the
/// full list size even when the response is truncated to the allocation
/// length, so an initiator can retry with a large enough buffer — essential
/// once thousands of LUNs are exported (#24).
fn handle_report_luns(lun_ids: &[u64], cdb: &[u8]) -> ScsiResult {
    let select_report = cdb[2];
    let alloc_len = u32::from_be_bytes([cdb[6], cdb[7], cdb[8], cdb[9]]) as usize;

    // The allocation length must at least cover the 8-byte header.
    if alloc_len < 16 {
        return ScsiResult::check_condition(SenseData::invalid_field_in_cdb());
    }

    let reported: Vec<u64> = match select_report {
        // 0x00 addressable LUNs, 0x02 all LUNs — both are our full list.
        0x00 | 0x02 => {
            if lun_ids.is_empty() {
                // No LUNs configured: report LUN 0 so an initiator still has
                // something to address (it will fail INQUIRY, not discovery).
                vec![0]
            } else {
                lun_ids.to_vec()
            }
        }
        // 0x01 well-known logical units only — we expose none.
        0x01 => Vec::new(),
        _ => return ScsiResult::check_condition(SenseData::invalid_field_in_cdb()),
    };

    let list_len = reported.len() * 8;
    let mut data = vec![0u8; 8 + list_len];
    // LUN LIST LENGTH (bytes 0-3) — the full length, before truncation.
    data[0..4].copy_from_slice(&(list_len as u32).to_be_bytes());
    // Bytes 4-7 reserved.
    for (i, &lun) in reported.iter().enumerate() {
        let offset = 8 + i * 8;
        data[offset..offset + 2].copy_from_slice(&encode_lun(lun).to_be_bytes());
    }

    data.truncate(alloc_len);
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
        let result = handle_scsi_command(&cdb, &dev, &[], &[0]).await;
        assert_eq!(result.status, ScsiStatus::Good);
        assert!(result.data.len() >= 36);
        assert_eq!(&result.data[8..16], b"StrmBlk ");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_unit_ready() {
        let (dev, path) = test_device().await;
        let cdb = [TEST_UNIT_READY, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let result = handle_scsi_command(&cdb, &dev, &[], &[0]).await;
        assert_eq!(result.status, ScsiStatus::Good);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn read_capacity_10() {
        let (dev, path) = test_device().await;
        let cdb = [READ_CAPACITY_10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let result = handle_scsi_command(&cdb, &dev, &[], &[0]).await;
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
        let result = handle_scsi_command(&cdb_w, &dev, &write_data, &[0]).await;
        assert_eq!(result.status, ScsiStatus::Good);

        // Read it back
        let cdb_r = [READ_10, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0];
        let result = handle_scsi_command(&cdb_r, &dev, &[], &[0]).await;
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
        let result = handle_scsi_command(&cdb, &dev, &[], &[0]).await;
        assert_eq!(result.status, ScsiStatus::CheckCondition);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn report_luns() {
        let (dev, path) = test_device().await;
        let cdb = [REPORT_LUNS, 0, 0, 0, 0, 0, 0, 0, 0, 16, 0, 0, 0, 0, 0, 0];
        let result = handle_scsi_command(&cdb, &dev, &[], &[0]).await;
        assert_eq!(result.status, ScsiStatus::Good);
        assert_eq!(result.data.len(), 16);
        let _ = std::fs::remove_file(&path);
    }

    /// The Logical Block Provisioning page is what switches a Linux initiator's
    /// discard support on; without it thin allocation only ever grows (#25).
    #[tokio::test]
    async fn vpd_logical_block_provisioning() {
        let (dev, path) = test_device().await;

        // The page must be advertised in the supported-pages list, or the
        // initiator will never ask for it.
        let cdb = [INQUIRY, 0x01, 0x00, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let result = handle_scsi_command(&cdb, &dev, &[], &[0]).await;
        assert_eq!(result.status, ScsiStatus::Good);
        let page_len = result.data[3] as usize;
        let pages = &result.data[4..4 + page_len];
        assert!(pages.contains(&0xB2), "0xB2 missing from supported VPD pages");

        let cdb = [INQUIRY, 0x01, 0xB2, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let result = handle_scsi_command(&cdb, &dev, &[], &[0]).await;
        assert_eq!(result.status, ScsiStatus::Good);
        assert_eq!(result.data[1], 0xB2);
        assert_ne!(result.data[5] & 0x80, 0, "LBPU (UNMAP supported) must be set");
        assert_ne!(result.data[5] & 0x40, 0, "LBPWS must be set");
        assert_ne!(result.data[5] & 0x04, 0, "LBPRZ must be set");
        assert_eq!(result.data[6] & 0x07, 0x02, "provisioning type must be thin");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn vpd_block_limits_reports_unmap_granularity() {
        let (dev, path) = test_device().await;
        let cdb = [INQUIRY, 0x01, 0xB0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let result = handle_scsi_command(&cdb, &dev, &[], &[0]).await;

        assert_eq!(result.status, ScsiStatus::Good);
        // A plain file device reclaims per block, so granularity is 1 block.
        let granularity = u32::from_be_bytes(result.data[28..32].try_into().unwrap());
        assert_eq!(granularity, 1);
        // UGAVALID must be set for the alignment field to be believed.
        let alignment = u32::from_be_bytes(result.data[32..36].try_into().unwrap());
        assert_ne!(alignment & 0x8000_0000, 0);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn read_capacity_16_advertises_thin_provisioning() {
        let (dev, path) = test_device().await;
        let mut cdb = [0u8; 16];
        cdb[0] = READ_CAPACITY_16;
        cdb[1] = 0x10;
        cdb[10..14].copy_from_slice(&32u32.to_be_bytes());

        let result = handle_scsi_command(&cdb, &dev, &[], &[0]).await;
        assert_eq!(result.status, ScsiStatus::Good);
        assert_ne!(result.data[14] & 0x80, 0, "LBPME must be set");
        assert_ne!(result.data[14] & 0x40, 0, "LBPRZ must be set");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn write_same_16_writes_pattern() {
        let (dev, path) = test_device().await;
        let bs = dev.block_size() as usize;

        let mut cdb = [0u8; 16];
        cdb[0] = WRITE_SAME_16;
        cdb[10..14].copy_from_slice(&4u32.to_be_bytes()); // 4 blocks
        let pattern = vec![0x5A_u8; bs];
        let result = handle_scsi_command(&cdb, &dev, &pattern, &[0]).await;
        assert_eq!(result.status, ScsiStatus::Good);

        // All four blocks now hold the pattern.
        let mut buf = vec![0u8; bs * 4];
        dev.read(0, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0x5A));

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn write_same_16_rejects_bad_input() {
        let (dev, path) = test_device().await;
        let bs = dev.block_size() as usize;
        let pattern = vec![0u8; bs];

        // A zero block count would mean "to end of device" — refuse it.
        let mut cdb = [0u8; 16];
        cdb[0] = WRITE_SAME_16;
        let result = handle_scsi_command(&cdb, &dev, &pattern, &[0]).await;
        assert_eq!(result.status, ScsiStatus::CheckCondition);

        // A short pattern (less than one block) is an illegal request.
        let mut cdb = [0u8; 16];
        cdb[0] = WRITE_SAME_16;
        cdb[10..14].copy_from_slice(&1u32.to_be_bytes());
        let result = handle_scsi_command(&cdb, &dev, &[0u8; 8], &[0]).await;
        assert_eq!(result.status, ScsiStatus::CheckCondition);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn data_out_and_media_command_classification() {
        // UNMAP and WRITE SAME must collect a data-out payload — missing that
        // is what made UNMAP always fail with an empty parameter list (#25).
        for op in [WRITE_10, WRITE_16, WRITE_SAME_10, WRITE_SAME_16, UNMAP, MAINTENANCE_OUT] {
            assert!(is_data_out_command(op), "{op:#04x} should collect data-out");
        }
        for op in [READ_10, READ_16, INQUIRY, REPORT_LUNS, TEST_UNIT_READY] {
            assert!(!is_data_out_command(op), "{op:#04x} should not collect data-out");
        }

        // MAINTENANCE OUT takes data but does not touch media, so a readonly
        // LUN must still accept it.
        for op in [WRITE_10, WRITE_16, WRITE_SAME_10, WRITE_SAME_16, UNMAP] {
            assert!(modifies_media(op), "{op:#04x} should be refused when readonly");
        }
        assert!(!modifies_media(MAINTENANCE_OUT));
        assert!(!modifies_media(READ_10));
    }

    /// Build a REPORT LUNS CDB with the given SELECT REPORT + allocation length.
    fn report_luns_cdb(select_report: u8, alloc_len: u32) -> [u8; 16] {
        let mut cdb = [0u8; 16];
        cdb[0] = REPORT_LUNS;
        cdb[2] = select_report;
        cdb[6..10].copy_from_slice(&alloc_len.to_be_bytes());
        cdb
    }

    #[test]
    fn encode_lun_addressing_methods() {
        // Peripheral addressing (method 00b) below 256.
        assert_eq!(encode_lun(0), 0x0000);
        assert_eq!(encode_lun(3), 0x0003);
        assert_eq!(encode_lun(255), 0x00FF);
        // Flat-space addressing (method 01b) at and above 256.
        assert_eq!(encode_lun(256), 0x4100);
        assert_eq!(encode_lun(1000), 0x43E8);
    }

    #[tokio::test]
    async fn report_luns_reports_full_length_when_truncated() {
        let (dev, path) = test_device().await;
        let luns: Vec<u64> = (0..64).collect();

        // Only room for the header plus two LUNs.
        let cdb = report_luns_cdb(0x00, 24);
        let result = handle_scsi_command(&cdb, &dev, &[], &luns).await;
        assert_eq!(result.status, ScsiStatus::Good);

        // Payload is truncated to the allocation length...
        assert_eq!(result.data.len(), 24);
        // ...but LUN LIST LENGTH still advertises all 64 LUNs so the
        // initiator knows to retry with a bigger buffer.
        let list_len = u32::from_be_bytes(result.data[0..4].try_into().unwrap());
        assert_eq!(list_len, 64 * 8);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn report_luns_select_report_variants() {
        let (dev, path) = test_device().await;
        let luns = vec![0u64, 1, 2];

        // 0x01 = well-known LUNs only; we expose none.
        let result = handle_scsi_command(&report_luns_cdb(0x01, 256), &dev, &[], &luns).await;
        assert_eq!(result.status, ScsiStatus::Good);
        assert_eq!(u32::from_be_bytes(result.data[0..4].try_into().unwrap()), 0);

        // 0x02 = all LUNs, same as 0x00 for us.
        let result = handle_scsi_command(&report_luns_cdb(0x02, 256), &dev, &[], &luns).await;
        assert_eq!(u32::from_be_bytes(result.data[0..4].try_into().unwrap()), 24);

        // Reserved SELECT REPORT values are an illegal request.
        let result = handle_scsi_command(&report_luns_cdb(0x77, 256), &dev, &[], &luns).await;
        assert_eq!(result.status, ScsiStatus::CheckCondition);

        // An allocation length too small for the header is also illegal.
        let result = handle_scsi_command(&report_luns_cdb(0x00, 8), &dev, &[], &luns).await;
        assert_eq!(result.status, ScsiStatus::CheckCondition);

        let _ = std::fs::remove_file(&path);
    }

    /// The registry model exports thousands of LUNs; REPORT LUNS must encode
    /// all of them, including those past the 255 peripheral-addressing limit.
    #[tokio::test]
    async fn report_luns_at_scale() {
        let (dev, path) = test_device().await;
        let luns: Vec<u64> = (0..2000).collect();

        let alloc_len = 8 + 2000 * 8;
        let cdb = report_luns_cdb(0x00, alloc_len as u32);
        let result = handle_scsi_command(&cdb, &dev, &[], &luns).await;

        assert_eq!(result.status, ScsiStatus::Good);
        assert_eq!(result.data.len(), alloc_len);
        let list_len = u32::from_be_bytes(result.data[0..4].try_into().unwrap());
        assert_eq!(list_len, 2000 * 8);

        // Spot-check both addressing methods round-trip through the response.
        let read_at = |i: usize| -> u16 {
            let off = 8 + i * 8;
            u16::from_be_bytes(result.data[off..off + 2].try_into().unwrap())
        };
        assert_eq!(read_at(0), encode_lun(0));
        assert_eq!(read_at(255), encode_lun(255));
        assert_eq!(read_at(256), encode_lun(256));
        assert_eq!(read_at(1999), encode_lun(1999));

        let _ = std::fs::remove_file(&path);
    }
}
