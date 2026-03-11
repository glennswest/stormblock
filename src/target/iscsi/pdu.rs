//! iSCSI PDU framing — 48-byte Basic Header Segment, AHS, data, CRC32C digests.
//!
//! Reference: RFC 7143 §11

use std::fmt;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// iSCSI initiator opcodes (bit 6 = immediate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Opcode {
    NopOut = 0x00,
    ScsiCommand = 0x01,
    TaskMgmtRequest = 0x02,
    LoginRequest = 0x03,
    TextRequest = 0x04,
    DataOut = 0x05,
    LogoutRequest = 0x06,
    // Target opcodes
    NopIn = 0x20,
    ScsiResponse = 0x21,
    TaskMgmtResponse = 0x22,
    LoginResponse = 0x23,
    TextResponse = 0x24,
    DataIn = 0x25,
    LogoutResponse = 0x26,
    R2T = 0x31,
    Reject = 0x3f,
}

impl Opcode {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b & 0x3f {
            0x00 => Some(Opcode::NopOut),
            0x01 => Some(Opcode::ScsiCommand),
            0x02 => Some(Opcode::TaskMgmtRequest),
            0x03 => Some(Opcode::LoginRequest),
            0x04 => Some(Opcode::TextRequest),
            0x05 => Some(Opcode::DataOut),
            0x06 => Some(Opcode::LogoutRequest),
            0x20 => Some(Opcode::NopIn),
            0x21 => Some(Opcode::ScsiResponse),
            0x22 => Some(Opcode::TaskMgmtResponse),
            0x23 => Some(Opcode::LoginResponse),
            0x24 => Some(Opcode::TextResponse),
            0x25 => Some(Opcode::DataIn),
            0x26 => Some(Opcode::LogoutResponse),
            0x31 => Some(Opcode::R2T),
            0x3f => Some(Opcode::Reject),
            _ => None,
        }
    }
}

impl fmt::Display for Opcode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// 48-byte iSCSI Basic Header Segment.
#[derive(Clone)]
pub struct Bhs {
    pub raw: [u8; 48],
}

impl Default for Bhs {
    fn default() -> Self {
        Bhs { raw: [0u8; 48] }
    }
}

impl Bhs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_bytes(bytes: &[u8; 48]) -> Self {
        Bhs { raw: *bytes }
    }

    // Byte 0: opcode (bits 5:0), immediate (bit 6)
    pub fn opcode(&self) -> Option<Opcode> {
        Opcode::from_byte(self.raw[0])
    }

    pub fn set_opcode(&mut self, op: Opcode) {
        self.raw[0] = (self.raw[0] & 0xC0) | (op as u8 & 0x3f);
    }

    pub fn is_immediate(&self) -> bool {
        self.raw[0] & 0x40 != 0
    }

    pub fn set_immediate(&mut self, imm: bool) {
        if imm {
            self.raw[0] |= 0x40;
        } else {
            self.raw[0] &= !0x40;
        }
    }

    // Byte 1: flags (opcode-specific)
    pub fn flags(&self) -> u8 {
        self.raw[1]
    }

    pub fn set_flags(&mut self, f: u8) {
        self.raw[1] = f;
    }

    /// Final bit (bit 7 of byte 1) — used in login and data PDUs.
    pub fn is_final(&self) -> bool {
        self.raw[1] & 0x80 != 0
    }

    pub fn set_final(&mut self, f: bool) {
        if f {
            self.raw[1] |= 0x80;
        } else {
            self.raw[1] &= !0x80;
        }
    }

    // Bytes 4..8: total AHS length (byte 4) + data segment length (bytes 5..8, 24-bit)
    pub fn total_ahs_length(&self) -> u8 {
        self.raw[4]
    }

    pub fn set_total_ahs_length(&mut self, len: u8) {
        self.raw[4] = len;
    }

    pub fn data_segment_length(&self) -> u32 {
        ((self.raw[5] as u32) << 16) | ((self.raw[6] as u32) << 8) | (self.raw[7] as u32)
    }

    pub fn set_data_segment_length(&mut self, len: u32) {
        self.raw[5] = ((len >> 16) & 0xff) as u8;
        self.raw[6] = ((len >> 8) & 0xff) as u8;
        self.raw[7] = (len & 0xff) as u8;
    }

    // Bytes 8..16: LUN (for SCSI command) or opcode-specific
    pub fn lun(&self) -> u64 {
        u64::from_be_bytes(self.raw[8..16].try_into().unwrap())
    }

    pub fn set_lun(&mut self, lun: u64) {
        self.raw[8..16].copy_from_slice(&lun.to_be_bytes());
    }

    // Bytes 16..20: Initiator Task Tag
    pub fn initiator_task_tag(&self) -> u32 {
        u32::from_be_bytes(self.raw[16..20].try_into().unwrap())
    }

    pub fn set_initiator_task_tag(&mut self, tag: u32) {
        self.raw[16..20].copy_from_slice(&tag.to_be_bytes());
    }

    // Bytes 20..24: Target Transfer Tag (response) or CmdSN-related
    pub fn target_transfer_tag(&self) -> u32 {
        u32::from_be_bytes(self.raw[20..24].try_into().unwrap())
    }

    pub fn set_target_transfer_tag(&mut self, tag: u32) {
        self.raw[20..24].copy_from_slice(&tag.to_be_bytes());
    }

    // Bytes 24..28: CmdSN
    pub fn cmd_sn(&self) -> u32 {
        u32::from_be_bytes(self.raw[24..28].try_into().unwrap())
    }

    pub fn set_cmd_sn(&mut self, sn: u32) {
        self.raw[24..28].copy_from_slice(&sn.to_be_bytes());
    }

    // Bytes 28..32: ExpStatSN / StatSN depending on direction
    pub fn exp_stat_sn(&self) -> u32 {
        u32::from_be_bytes(self.raw[28..32].try_into().unwrap())
    }

    pub fn set_stat_sn(&mut self, sn: u32) {
        self.raw[28..32].copy_from_slice(&sn.to_be_bytes());
    }

    // Bytes 32..36: MaxCmdSN (response) or opcode-specific
    pub fn max_cmd_sn(&self) -> u32 {
        u32::from_be_bytes(self.raw[32..36].try_into().unwrap())
    }

    pub fn set_max_cmd_sn(&mut self, sn: u32) {
        self.raw[32..36].copy_from_slice(&sn.to_be_bytes());
    }

    // Bytes 36..40: ExpCmdSN (response) or opcode-specific
    pub fn exp_cmd_sn(&self) -> u32 {
        u32::from_be_bytes(self.raw[36..40].try_into().unwrap())
    }

    pub fn set_exp_cmd_sn(&mut self, sn: u32) {
        self.raw[36..40].copy_from_slice(&sn.to_be_bytes());
    }

    // --- SCSI Command specific fields (bytes 32..48) ---

    /// Expected data transfer length (SCSI Command PDU, bytes 20..24 overloaded)
    pub fn expected_data_transfer_length(&self) -> u32 {
        u32::from_be_bytes(self.raw[20..24].try_into().unwrap())
    }

    pub fn set_expected_data_transfer_length(&mut self, len: u32) {
        self.raw[20..24].copy_from_slice(&len.to_be_bytes());
    }

    /// CDB bytes for SCSI Command (bytes 32..48)
    pub fn cdb(&self) -> &[u8] {
        &self.raw[32..48]
    }

    pub fn set_cdb(&mut self, cdb: &[u8]) {
        let len = cdb.len().min(16);
        self.raw[32..32 + len].copy_from_slice(&cdb[..len]);
    }

    // --- Login-specific fields ---

    /// ISID (bytes 8..14) — 6-byte initiator session ID.
    pub fn isid(&self) -> [u8; 6] {
        self.raw[8..14].try_into().unwrap()
    }

    pub fn set_isid(&mut self, isid: &[u8; 6]) {
        self.raw[8..14].copy_from_slice(isid);
    }

    /// TSIH (bytes 14..16) — target session identifying handle.
    pub fn tsih(&self) -> u16 {
        u16::from_be_bytes(self.raw[14..16].try_into().unwrap())
    }

    pub fn set_tsih(&mut self, tsih: u16) {
        self.raw[14..16].copy_from_slice(&tsih.to_be_bytes());
    }

    /// CID (bytes 20..22 in login) — connection ID.
    pub fn cid(&self) -> u16 {
        u16::from_be_bytes(self.raw[20..22].try_into().unwrap())
    }

    pub fn set_cid(&mut self, cid: u16) {
        self.raw[20..22].copy_from_slice(&cid.to_be_bytes());
    }

    /// CSG (bits 3:2 of byte 1) — current stage in login.
    pub fn csg(&self) -> u8 {
        (self.raw[1] >> 2) & 0x03
    }

    pub fn set_csg(&mut self, stage: u8) {
        self.raw[1] = (self.raw[1] & !0x0C) | ((stage & 0x03) << 2);
    }

    /// NSG (bits 1:0 of byte 1) — next stage in login.
    pub fn nsg(&self) -> u8 {
        self.raw[1] & 0x03
    }

    pub fn set_nsg(&mut self, stage: u8) {
        self.raw[1] = (self.raw[1] & !0x03) | (stage & 0x03);
    }

    /// Transit bit (bit 7 of byte 1 in login).
    pub fn transit(&self) -> bool {
        self.raw[1] & 0x80 != 0
    }

    pub fn set_transit(&mut self, t: bool) {
        if t {
            self.raw[1] |= 0x80;
        } else {
            self.raw[1] &= !0x80;
        }
    }

    /// Continue bit (bit 6 of byte 1 in login).
    pub fn cont(&self) -> bool {
        self.raw[1] & 0x40 != 0
    }

    // --- Data-In specific ---

    /// Data-In status flag (bit 0 of byte 1)
    pub fn has_status(&self) -> bool {
        self.raw[1] & 0x01 != 0
    }

    pub fn set_has_status(&mut self, s: bool) {
        if s {
            self.raw[1] |= 0x01;
        } else {
            self.raw[1] &= !0x01;
        }
    }

    /// Status byte (byte 3) — SCSI status in response/data-in PDUs.
    pub fn status(&self) -> u8 {
        self.raw[3]
    }

    pub fn set_status(&mut self, s: u8) {
        self.raw[3] = s;
    }

    // Bytes 40..44: DataSN or R2TSN
    pub fn data_sn(&self) -> u32 {
        u32::from_be_bytes(self.raw[36..40].try_into().unwrap())
    }

    pub fn set_data_sn(&mut self, sn: u32) {
        self.raw[36..40].copy_from_slice(&sn.to_be_bytes());
    }

    /// Buffer offset for Data-In / Data-Out PDUs (bytes 40..44).
    pub fn buffer_offset(&self) -> u32 {
        u32::from_be_bytes(self.raw[40..44].try_into().unwrap())
    }

    pub fn set_buffer_offset(&mut self, off: u32) {
        self.raw[40..44].copy_from_slice(&off.to_be_bytes());
    }

    /// Residual count (bytes 44..48).
    pub fn residual_count(&self) -> u32 {
        u32::from_be_bytes(self.raw[44..48].try_into().unwrap())
    }

    pub fn set_residual_count(&mut self, count: u32) {
        self.raw[44..48].copy_from_slice(&count.to_be_bytes());
    }

    // --- R2T specific ---
    pub fn r2t_sn(&self) -> u32 {
        u32::from_be_bytes(self.raw[36..40].try_into().unwrap())
    }

    pub fn set_r2t_sn(&mut self, sn: u32) {
        self.raw[36..40].copy_from_slice(&sn.to_be_bytes());
    }

    pub fn desired_data_transfer_length(&self) -> u32 {
        u32::from_be_bytes(self.raw[44..48].try_into().unwrap())
    }

    pub fn set_desired_data_transfer_length(&mut self, len: u32) {
        self.raw[44..48].copy_from_slice(&len.to_be_bytes());
    }

    // --- Logout ---
    pub fn reason_code(&self) -> u8 {
        self.raw[1] & 0x7f
    }
}

impl fmt::Debug for Bhs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BHS")
            .field("opcode", &self.opcode())
            .field("data_len", &self.data_segment_length())
            .field("itt", &format_args!("0x{:08x}", self.initiator_task_tag()))
            .finish()
    }
}

/// Complete iSCSI PDU.
pub struct IscsiPdu {
    pub bhs: Bhs,
    pub ahs: Vec<u8>,
    pub data: Vec<u8>,
}

impl IscsiPdu {
    pub fn new(bhs: Bhs) -> Self {
        IscsiPdu {
            bhs,
            ahs: Vec::new(),
            data: Vec::new(),
        }
    }

    pub fn with_data(bhs: Bhs, data: Vec<u8>) -> Self {
        let mut pdu = IscsiPdu {
            bhs,
            ahs: Vec::new(),
            data,
        };
        pdu.bhs.set_data_segment_length(pdu.data.len() as u32);
        pdu
    }
}

/// Pad length to 4-byte boundary.
fn pad4(len: usize) -> usize {
    (4 - (len % 4)) % 4
}

/// Read a complete iSCSI PDU from a stream.
pub async fn read_pdu<R: AsyncReadExt + Unpin>(
    stream: &mut R,
    header_digest: bool,
    data_digest: bool,
) -> std::io::Result<IscsiPdu> {
    // Read 48-byte BHS
    let mut bhs_bytes = [0u8; 48];
    stream.read_exact(&mut bhs_bytes).await?;
    let bhs = Bhs::from_bytes(&bhs_bytes);

    // Header digest (CRC32C, 4 bytes)
    if header_digest {
        let mut digest = [0u8; 4];
        stream.read_exact(&mut digest).await?;
        let expected = crc32c::crc32c(&bhs_bytes);
        let received = u32::from_le_bytes(digest);
        if expected != received {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("header digest mismatch: expected {expected:#x}, got {received:#x}"),
            ));
        }
    }

    // Read AHS if present
    let ahs_len = bhs.total_ahs_length() as usize * 4;
    let ahs = if ahs_len > 0 {
        let mut ahs_buf = vec![0u8; ahs_len];
        stream.read_exact(&mut ahs_buf).await?;
        ahs_buf
    } else {
        Vec::new()
    };

    // Read data segment + padding
    let data_len = bhs.data_segment_length() as usize;
    let data = if data_len > 0 {
        let padded_len = data_len + pad4(data_len);
        let mut data_buf = vec![0u8; padded_len];
        stream.read_exact(&mut data_buf).await?;
        data_buf.truncate(data_len);

        // Data digest
        if data_digest {
            let mut digest = [0u8; 4];
            stream.read_exact(&mut digest).await?;
            let expected = crc32c::crc32c(&data_buf);
            let received = u32::from_le_bytes(digest);
            if expected != received {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "data digest mismatch",
                ));
            }
        }

        data_buf
    } else {
        Vec::new()
    };

    Ok(IscsiPdu { bhs, ahs, data })
}

/// Write a complete iSCSI PDU to a stream.
pub async fn write_pdu<W: AsyncWriteExt + Unpin>(
    stream: &mut W,
    pdu: &IscsiPdu,
    header_digest: bool,
    data_digest: bool,
) -> std::io::Result<()> {
    // Write BHS
    stream.write_all(&pdu.bhs.raw).await?;

    if header_digest {
        let crc = crc32c::crc32c(&pdu.bhs.raw);
        stream.write_all(&crc.to_le_bytes()).await?;
    }

    // Write AHS
    if !pdu.ahs.is_empty() {
        stream.write_all(&pdu.ahs).await?;
    }

    // Write data segment + padding
    if !pdu.data.is_empty() {
        stream.write_all(&pdu.data).await?;
        let padding = pad4(pdu.data.len());
        if padding > 0 {
            stream.write_all(&vec![0u8; padding]).await?;
        }

        if data_digest {
            let crc = crc32c::crc32c(&pdu.data);
            stream.write_all(&crc.to_le_bytes()).await?;
        }
    }

    stream.flush().await?;
    Ok(())
}

/// Parse key=value text data from a login/text PDU.
pub fn parse_text_params(data: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(data);
    text.split('\0')
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            let (key, val) = s.split_once('=')?;
            Some((key.to_string(), val.to_string()))
        })
        .collect()
}

/// Encode key=value pairs into iSCSI text format (null-separated).
pub fn encode_text_params(params: &[(&str, &str)]) -> Vec<u8> {
    let mut data = Vec::new();
    for (key, val) in params {
        data.extend_from_slice(key.as_bytes());
        data.push(b'=');
        data.extend_from_slice(val.as_bytes());
        data.push(0);
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bhs_roundtrip() {
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::ScsiCommand);
        bhs.set_immediate(true);
        bhs.set_final(true);
        bhs.set_data_segment_length(4096);
        bhs.set_initiator_task_tag(0xDEADBEEF);
        bhs.set_cmd_sn(42);
        bhs.set_lun(0);

        assert_eq!(bhs.opcode(), Some(Opcode::ScsiCommand));
        assert!(bhs.is_immediate());
        assert!(bhs.is_final());
        assert_eq!(bhs.data_segment_length(), 4096);
        assert_eq!(bhs.initiator_task_tag(), 0xDEADBEEF);
        assert_eq!(bhs.cmd_sn(), 42);

        let bhs2 = Bhs::from_bytes(&bhs.raw);
        assert_eq!(bhs2.opcode(), Some(Opcode::ScsiCommand));
        assert_eq!(bhs2.data_segment_length(), 4096);
        assert_eq!(bhs2.initiator_task_tag(), 0xDEADBEEF);
    }

    #[test]
    fn text_params_roundtrip() {
        let params = vec![("InitiatorName", "iqn.2024.com.test:init"), ("TargetName", "iqn.2024.com.stormblock:disk1")];
        let encoded = encode_text_params(&params);
        let decoded = parse_text_params(&encoded);
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0], ("InitiatorName".into(), "iqn.2024.com.test:init".into()));
        assert_eq!(decoded[1], ("TargetName".into(), "iqn.2024.com.stormblock:disk1".into()));
    }

    #[tokio::test]
    async fn pdu_read_write_roundtrip() {
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::LoginRequest);
        bhs.set_initiator_task_tag(1);
        let data = b"InitiatorName=iqn.test\0".to_vec();
        let pdu = IscsiPdu::with_data(bhs, data.clone());

        // Write to buffer
        let mut buf = Vec::new();
        write_pdu(&mut buf, &pdu, false, false).await.unwrap();

        // Read back
        let mut cursor = std::io::Cursor::new(buf);
        let pdu2 = read_pdu(&mut cursor, false, false).await.unwrap();
        assert_eq!(pdu2.bhs.opcode(), Some(Opcode::LoginRequest));
        assert_eq!(pdu2.bhs.initiator_task_tag(), 1);
        assert_eq!(pdu2.data, data);
    }

    #[tokio::test]
    async fn pdu_with_digest() {
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::NopOut);
        let pdu = IscsiPdu::with_data(bhs, vec![0xAA; 100]);

        let mut buf = Vec::new();
        write_pdu(&mut buf, &pdu, true, true).await.unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let pdu2 = read_pdu(&mut cursor, true, true).await.unwrap();
        assert_eq!(pdu2.data.len(), 100);
        assert!(pdu2.data.iter().all(|&b| b == 0xAA));
    }
}
