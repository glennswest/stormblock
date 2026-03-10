//! NVMe-oF/TCP PDU framing — common header, all PDU types, CRC32C digests.
//!
//! Reference: NVMe/TCP Transport Specification 1.0

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// NVMe-oF/TCP PDU types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PduType {
    ICReq = 0x00,
    ICResp = 0x01,
    H2CTermReq = 0x02,
    C2HTermReq = 0x03,
    CapsuleCmd = 0x04,
    CapsuleResp = 0x05,
    H2CData = 0x06,
    C2HData = 0x07,
    R2T = 0x09,
}

impl PduType {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x00 => Some(PduType::ICReq),
            0x01 => Some(PduType::ICResp),
            0x02 => Some(PduType::H2CTermReq),
            0x03 => Some(PduType::C2HTermReq),
            0x04 => Some(PduType::CapsuleCmd),
            0x05 => Some(PduType::CapsuleResp),
            0x06 => Some(PduType::H2CData),
            0x07 => Some(PduType::C2HData),
            0x09 => Some(PduType::R2T),
            _ => None,
        }
    }
}

/// 8-byte NVMe-oF/TCP common header.
#[derive(Debug, Clone)]
pub struct CommonHeader {
    pub pdu_type: u8,
    pub flags: u8,
    pub hlen: u8,    // header length in bytes
    pub pdo: u8,     // PDU data offset (where data starts)
    pub plen: u32,   // total PDU length including header
}

impl CommonHeader {
    pub fn from_bytes(buf: &[u8; 8]) -> Self {
        CommonHeader {
            pdu_type: buf[0],
            flags: buf[1],
            hlen: buf[2],
            pdo: buf[3],
            plen: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
        }
    }

    pub fn to_bytes(&self) -> [u8; 8] {
        let mut buf = [0u8; 8];
        buf[0] = self.pdu_type;
        buf[1] = self.flags;
        buf[2] = self.hlen;
        buf[3] = self.pdo;
        buf[4..8].copy_from_slice(&self.plen.to_le_bytes());
        buf
    }

    pub fn hdgst_enable(&self) -> bool {
        self.flags & 0x01 != 0
    }

    pub fn ddgst_enable(&self) -> bool {
        self.flags & 0x02 != 0
    }
}

/// 128-byte ICReq (Initialize Connection Request).
#[derive(Debug, Clone)]
pub struct ICReq {
    pub pfv: u16,      // PDU format version
    pub hpda: u8,      // host PDU data alignment (in dwords, 0-based)
    pub dgst: u8,      // digest types requested (bit0=hdgst, bit1=ddgst)
    pub maxr2t: u32,   // max outstanding R2T
}

impl ICReq {
    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < 120 { // 128 - 8 (common header)
            return None;
        }
        Some(ICReq {
            pfv: u16::from_le_bytes([buf[0], buf[1]]),
            hpda: buf[2],
            dgst: buf[3],
            maxr2t: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
        })
    }
}

/// 128-byte ICResp (Initialize Connection Response).
#[derive(Debug, Clone)]
pub struct ICResp {
    pub pfv: u16,
    pub cpda: u8,
    pub dgst: u8,
    pub maxh2cdata: u32,
}

impl ICResp {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = vec![0u8; 120]; // 128 - 8 (common header)
        buf[0..2].copy_from_slice(&self.pfv.to_le_bytes());
        buf[2] = self.cpda;
        buf[3] = self.dgst;
        buf[4..8].copy_from_slice(&self.maxh2cdata.to_le_bytes());
        buf
    }
}

/// 64-byte NVMe Submission Queue Entry (SQE) — the NVMe command.
#[derive(Debug, Clone)]
pub struct NvmeSqe {
    pub raw: [u8; 64],
}

impl NvmeSqe {
    pub fn from_bytes(buf: &[u8; 64]) -> Self {
        NvmeSqe { raw: *buf }
    }

    pub fn opcode(&self) -> u8 {
        self.raw[0]
    }

    pub fn fuse(&self) -> u8 {
        (self.raw[1] >> 0) & 0x03
    }

    pub fn cid(&self) -> u16 {
        u16::from_le_bytes([self.raw[2], self.raw[3]])
    }

    pub fn nsid(&self) -> u32 {
        u32::from_le_bytes(self.raw[4..8].try_into().unwrap())
    }

    // cdw10-15 (command-specific dwords)
    pub fn cdw10(&self) -> u32 {
        u32::from_le_bytes(self.raw[40..44].try_into().unwrap())
    }

    pub fn cdw11(&self) -> u32 {
        u32::from_le_bytes(self.raw[44..48].try_into().unwrap())
    }

    pub fn cdw12(&self) -> u32 {
        u32::from_le_bytes(self.raw[48..52].try_into().unwrap())
    }

    pub fn cdw13(&self) -> u32 {
        u32::from_le_bytes(self.raw[52..56].try_into().unwrap())
    }

    pub fn cdw14(&self) -> u32 {
        u32::from_le_bytes(self.raw[56..60].try_into().unwrap())
    }

    pub fn cdw15(&self) -> u32 {
        u32::from_le_bytes(self.raw[60..64].try_into().unwrap())
    }
}

/// 16-byte NVMe Completion Queue Entry (CQE).
#[derive(Debug, Clone)]
pub struct NvmeCqe {
    pub raw: [u8; 16],
}

impl NvmeCqe {
    pub fn new() -> Self {
        NvmeCqe { raw: [0u8; 16] }
    }

    pub fn success(cid: u16, sq_id: u16, sq_head: u16) -> Self {
        let mut cqe = NvmeCqe::new();
        // DW2: SQ Head Pointer + SQ Identifier
        cqe.raw[8..10].copy_from_slice(&sq_head.to_le_bytes());
        cqe.raw[10..12].copy_from_slice(&sq_id.to_le_bytes());
        // DW3: CID + Status (0 = success)
        cqe.raw[12..14].copy_from_slice(&cid.to_le_bytes());
        // Status field (bits 17:1 of DW3) = 0 for success
        cqe
    }

    pub fn error(cid: u16, sq_id: u16, sq_head: u16, status_code_type: u8, status_code: u8) -> Self {
        let mut cqe = NvmeCqe::new();
        cqe.raw[8..10].copy_from_slice(&sq_head.to_le_bytes());
        cqe.raw[10..12].copy_from_slice(&sq_id.to_le_bytes());
        cqe.raw[12..14].copy_from_slice(&cid.to_le_bytes());
        // Status field: bit 0 = phase, bits 8:1 = status code, bits 11:9 = SCT
        let status = ((status_code_type as u16 & 0x07) << 9) | ((status_code as u16) << 1);
        cqe.raw[14..16].copy_from_slice(&status.to_le_bytes());
        cqe
    }

    pub fn set_dw0(&mut self, val: u32) {
        self.raw[0..4].copy_from_slice(&val.to_le_bytes());
    }
}

/// NVMe-oF/TCP PDU variants.
pub enum NvmeofPdu {
    ICReq(CommonHeader, ICReq),
    ICResp(CommonHeader, ICResp),
    CapsuleCmd {
        header: CommonHeader,
        sqe: NvmeSqe,
        data: Vec<u8>,
    },
    CapsuleResp {
        header: CommonHeader,
        cqe: NvmeCqe,
    },
    C2HData {
        header: CommonHeader,
        cccid: u16,   // command capsule CID
        datao: u32,    // data offset
        datal: u32,    // data length
        data: Vec<u8>,
        last: bool,
        success: bool,
    },
    H2CData {
        header: CommonHeader,
        cccid: u16,
        datao: u32,
        datal: u32,
        data: Vec<u8>,
        last: bool,
    },
    R2T {
        header: CommonHeader,
        cccid: u16,
        ttag: u16,
        r2to: u32,
        r2tl: u32,
    },
}

/// PDU header sizes.
const _ICREQ_HLEN: u8 = 128;
const ICRESP_HLEN: u8 = 128;
const _CAPSULE_CMD_HLEN: u8 = 72;  // 8 (common) + 64 (SQE)
const CAPSULE_RESP_HLEN: u8 = 24; // 8 (common) + 16 (CQE)
const C2H_DATA_HLEN: u8 = 24;
const _H2C_DATA_HLEN: u8 = 24;
const R2T_HLEN: u8 = 24;

/// Read a complete NVMe-oF/TCP PDU from a stream.
pub async fn read_pdu<R: AsyncReadExt + Unpin>(stream: &mut R) -> std::io::Result<NvmeofPdu> {
    // Read 8-byte common header
    let mut hdr_buf = [0u8; 8];
    stream.read_exact(&mut hdr_buf).await?;
    let ch = CommonHeader::from_bytes(&hdr_buf);

    let pdu_type = PduType::from_byte(ch.pdu_type).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("unknown PDU type: {}", ch.pdu_type))
    })?;

    match pdu_type {
        PduType::ICReq => {
            let mut rest = vec![0u8; (ch.hlen as usize).saturating_sub(8)];
            stream.read_exact(&mut rest).await?;
            let icreq = ICReq::from_bytes(&rest).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid ICReq")
            })?;
            Ok(NvmeofPdu::ICReq(ch, icreq))
        }
        PduType::CapsuleCmd => {
            // Read SQE (64 bytes)
            let mut sqe_buf = [0u8; 64];
            stream.read_exact(&mut sqe_buf).await?;
            let sqe = NvmeSqe::from_bytes(&sqe_buf);

            // Optional header digest
            if ch.hdgst_enable() {
                let mut dgst = [0u8; 4];
                stream.read_exact(&mut dgst).await?;
                // Verify: CRC32C over common header + SQE
                let mut hdr_data = Vec::with_capacity(72);
                hdr_data.extend_from_slice(&hdr_buf);
                hdr_data.extend_from_slice(&sqe_buf);
                let expected = crc32c::crc32c(&hdr_data);
                let received = u32::from_le_bytes(dgst);
                if expected != received {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "header digest mismatch"));
                }
            }

            // Read inline data if plen > hlen
            let data_len = ch.plen as usize - ch.hlen as usize
                - if ch.hdgst_enable() { 4 } else { 0 };
            let data = if data_len > 0 {
                // Account for data digest
                let actual_data_len = if ch.ddgst_enable() {
                    data_len - 4
                } else {
                    data_len
                };
                let mut data = vec![0u8; actual_data_len];
                stream.read_exact(&mut data).await?;
                if ch.ddgst_enable() {
                    let mut dgst = [0u8; 4];
                    stream.read_exact(&mut dgst).await?;
                }
                data
            } else {
                Vec::new()
            };

            Ok(NvmeofPdu::CapsuleCmd { header: ch, sqe, data })
        }
        PduType::H2CData => {
            // Read H2C data-specific header (16 bytes after common)
            let mut hdr_rest = [0u8; 16];
            stream.read_exact(&mut hdr_rest).await?;

            let cccid = u16::from_le_bytes([hdr_rest[0], hdr_rest[1]]);
            let ttag = u16::from_le_bytes([hdr_rest[2], hdr_rest[3]]);
            let _ = ttag;
            let datao = u32::from_le_bytes(hdr_rest[4..8].try_into().unwrap());
            let datal = u32::from_le_bytes(hdr_rest[8..12].try_into().unwrap());
            let last = hdr_rest[15] & 0x04 != 0;

            // Read data
            let data_offset = ch.pdo as usize;
            let pad = data_offset.saturating_sub(ch.hlen as usize);
            if pad > 0 {
                let mut padding = vec![0u8; pad];
                stream.read_exact(&mut padding).await?;
            }

            let data_bytes = ch.plen as usize - data_offset
                - if ch.ddgst_enable() { 4 } else { 0 };
            let mut data = vec![0u8; data_bytes];
            stream.read_exact(&mut data).await?;

            if ch.ddgst_enable() {
                let mut dgst = [0u8; 4];
                stream.read_exact(&mut dgst).await?;
            }

            Ok(NvmeofPdu::H2CData {
                header: ch, cccid, datao, datal, data, last,
            })
        }
        _ => {
            // Read remaining bytes and discard
            let remaining = ch.plen as usize - 8;
            if remaining > 0 {
                let mut discard = vec![0u8; remaining];
                stream.read_exact(&mut discard).await?;
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!("unhandled PDU type: {pdu_type:?}"),
            ))
        }
    }
}

/// Write ICResp PDU.
pub async fn write_ic_resp<W: AsyncWriteExt + Unpin>(
    stream: &mut W,
    resp: &ICResp,
) -> std::io::Result<()> {
    let ch = CommonHeader {
        pdu_type: PduType::ICResp as u8,
        flags: 0,
        hlen: ICRESP_HLEN,
        pdo: 0,
        plen: ICRESP_HLEN as u32,
    };
    stream.write_all(&ch.to_bytes()).await?;
    stream.write_all(&resp.to_bytes()).await?;
    stream.flush().await
}

/// Write CapsuleResp PDU.
pub async fn write_capsule_resp<W: AsyncWriteExt + Unpin>(
    stream: &mut W,
    cqe: &NvmeCqe,
    hdgst: bool,
) -> std::io::Result<()> {
    let hlen = CAPSULE_RESP_HLEN;
    let plen = hlen as u32 + if hdgst { 4 } else { 0 };
    let ch = CommonHeader {
        pdu_type: PduType::CapsuleResp as u8,
        flags: if hdgst { 0x01 } else { 0 },
        hlen,
        pdo: 0,
        plen,
    };
    let hdr_bytes = ch.to_bytes();
    stream.write_all(&hdr_bytes).await?;
    stream.write_all(&cqe.raw).await?;

    if hdgst {
        let mut combined = Vec::with_capacity(24);
        combined.extend_from_slice(&hdr_bytes);
        combined.extend_from_slice(&cqe.raw);
        let crc = crc32c::crc32c(&combined);
        stream.write_all(&crc.to_le_bytes()).await?;
    }

    stream.flush().await
}

/// Write C2HData PDU (controller-to-host data, for reads).
pub async fn write_c2h_data<W: AsyncWriteExt + Unpin>(
    stream: &mut W,
    cccid: u16,
    data_offset: u32,
    data: &[u8],
    last: bool,
    success: bool,
    hdgst: bool,
    ddgst: bool,
) -> std::io::Result<()> {
    let hlen = C2H_DATA_HLEN;
    let pdo = hlen; // data immediately follows header
    let plen = hlen as u32 + data.len() as u32
        + if hdgst { 4 } else { 0 }
        + if ddgst { 4 } else { 0 };

    let mut flags = 0u8;
    if hdgst { flags |= 0x01; }
    if ddgst { flags |= 0x02; }
    if last { flags |= 0x04; }
    if success { flags |= 0x08; }

    let ch = CommonHeader {
        pdu_type: PduType::C2HData as u8,
        flags,
        hlen,
        pdo,
        plen,
    };
    let hdr_bytes = ch.to_bytes();

    // C2H-specific header fields (16 bytes after common header)
    let mut specific = [0u8; 16];
    specific[0..2].copy_from_slice(&cccid.to_le_bytes());
    // ttag at [2..4] = 0
    specific[4..8].copy_from_slice(&data_offset.to_le_bytes());
    specific[8..12].copy_from_slice(&(data.len() as u32).to_le_bytes());
    // reserved [12..16]

    stream.write_all(&hdr_bytes).await?;
    stream.write_all(&specific).await?;

    if hdgst {
        let mut combined = Vec::with_capacity(24);
        combined.extend_from_slice(&hdr_bytes);
        combined.extend_from_slice(&specific);
        let crc = crc32c::crc32c(&combined);
        stream.write_all(&crc.to_le_bytes()).await?;
    }

    stream.write_all(data).await?;

    if ddgst {
        let crc = crc32c::crc32c(data);
        stream.write_all(&crc.to_le_bytes()).await?;
    }

    stream.flush().await
}

/// Write R2T PDU (ready to transfer, for writes).
pub async fn write_r2t<W: AsyncWriteExt + Unpin>(
    stream: &mut W,
    cccid: u16,
    ttag: u16,
    r2to: u32,
    r2tl: u32,
    hdgst: bool,
) -> std::io::Result<()> {
    let hlen = R2T_HLEN;
    let plen = hlen as u32 + if hdgst { 4 } else { 0 };
    let ch = CommonHeader {
        pdu_type: PduType::R2T as u8,
        flags: if hdgst { 0x01 } else { 0 },
        hlen,
        pdo: 0,
        plen,
    };
    let hdr_bytes = ch.to_bytes();

    let mut specific = [0u8; 16];
    specific[0..2].copy_from_slice(&cccid.to_le_bytes());
    specific[2..4].copy_from_slice(&ttag.to_le_bytes());
    specific[4..8].copy_from_slice(&r2to.to_le_bytes());
    specific[8..12].copy_from_slice(&r2tl.to_le_bytes());

    stream.write_all(&hdr_bytes).await?;
    stream.write_all(&specific).await?;

    if hdgst {
        let mut combined = Vec::with_capacity(24);
        combined.extend_from_slice(&hdr_bytes);
        combined.extend_from_slice(&specific);
        let crc = crc32c::crc32c(&combined);
        stream.write_all(&crc.to_le_bytes()).await?;
    }

    stream.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_header_roundtrip() {
        let ch = CommonHeader {
            pdu_type: PduType::CapsuleCmd as u8,
            flags: 0x03,
            hlen: 72,
            pdo: 72,
            plen: 4168,
        };
        let bytes = ch.to_bytes();
        let ch2 = CommonHeader::from_bytes(&bytes);
        assert_eq!(ch2.pdu_type, PduType::CapsuleCmd as u8);
        assert_eq!(ch2.flags, 0x03);
        assert_eq!(ch2.hlen, 72);
        assert_eq!(ch2.pdo, 72);
        assert_eq!(ch2.plen, 4168);
        assert!(ch2.hdgst_enable());
        assert!(ch2.ddgst_enable());
    }

    #[test]
    fn nvme_sqe_fields() {
        let mut raw = [0u8; 64];
        raw[0] = 0x02; // Read opcode
        raw[2..4].copy_from_slice(&42u16.to_le_bytes()); // CID
        raw[4..8].copy_from_slice(&1u32.to_le_bytes()); // NSID
        raw[40..44].copy_from_slice(&100u32.to_le_bytes()); // CDW10 (SLBA low)
        raw[44..48].copy_from_slice(&0u32.to_le_bytes()); // CDW11 (SLBA high)
        raw[48..52].copy_from_slice(&7u32.to_le_bytes()); // CDW12 (NLB=7, 0-based = 8 blocks)

        let sqe = NvmeSqe::from_bytes(&raw);
        assert_eq!(sqe.opcode(), 0x02);
        assert_eq!(sqe.cid(), 42);
        assert_eq!(sqe.nsid(), 1);
        assert_eq!(sqe.cdw10(), 100);
        assert_eq!(sqe.cdw11(), 0);
        assert_eq!(sqe.cdw12(), 7);
    }

    #[test]
    fn nvme_cqe_success() {
        let cqe = NvmeCqe::success(42, 0, 1);
        let cid = u16::from_le_bytes([cqe.raw[12], cqe.raw[13]]);
        assert_eq!(cid, 42);
        let status = u16::from_le_bytes([cqe.raw[14], cqe.raw[15]]);
        assert_eq!(status & 0xFFFE, 0); // status code = 0
    }

    #[test]
    fn nvme_cqe_error() {
        let cqe = NvmeCqe::error(1, 0, 0, 0, 0x02); // Invalid Field
        let status = u16::from_le_bytes([cqe.raw[14], cqe.raw[15]]);
        let sc = (status >> 1) & 0xFF;
        assert_eq!(sc, 0x02);
    }

    #[tokio::test]
    async fn ic_resp_write() {
        let resp = ICResp { pfv: 0, cpda: 0, dgst: 0, maxh2cdata: 131072 };
        let mut buf = Vec::new();
        write_ic_resp(&mut buf, &resp).await.unwrap();
        assert_eq!(buf.len(), 128); // ICResp is always 128 bytes
    }

    #[tokio::test]
    async fn capsule_resp_write() {
        let cqe = NvmeCqe::success(1, 0, 0);
        let mut buf = Vec::new();
        write_capsule_resp(&mut buf, &cqe, false).await.unwrap();
        assert_eq!(buf.len(), 24); // 8 + 16
    }
}
