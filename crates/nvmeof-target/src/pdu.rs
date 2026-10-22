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
        self.raw[1] & 0x03
    }

    pub fn cid(&self) -> u16 {
        u16::from_le_bytes([self.raw[2], self.raw[3]])
    }

    pub fn nsid(&self) -> u32 {
        u32::from_le_bytes(self.raw[4..8].try_into().unwrap())
    }

    /// Raw SQE byte — fabrics commands place fields outside the CDW grid
    /// (e.g. FCTYPE at byte 4, Connect QID at bytes 42-43).
    pub fn byte(&self, idx: usize) -> u8 {
        self.raw[idx]
    }

    pub fn u16_at(&self, idx: usize) -> u16 {
        u16::from_le_bytes([self.raw[idx], self.raw[idx + 1]])
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
#[derive(Debug, Clone, Default)]
pub struct NvmeCqe {
    pub raw: [u8; 16],
}

impl NvmeCqe {
    /// Status Code and Status Code Type (DW3 bits 17–27): 0 is success.
    pub fn status(&self) -> u16 {
        (u16::from_le_bytes([self.raw[14], self.raw[15]]) >> 1) & 0x07FF
    }

    pub fn new() -> Self {
        Self::default()
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

    /// An error the host must not retry (DNR set): a refusal, not a fault.
    pub fn error_dnr(cid: u16, sq_id: u16, sq_head: u16, status_code_type: u8, status_code: u8) -> Self {
        let mut cqe = Self::error(cid, sq_id, sq_head, status_code_type, status_code);
        // DNR is bit 14 of the status field, which sits one bit up in DW3.
        let status = u16::from_le_bytes([cqe.raw[14], cqe.raw[15]]) | (1 << 15);
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
        /// False when a negotiated data digest did not match the in-capsule
        /// data: the command is completed with Transient Transport Error and
        /// not run; the connection goes on (#392).
        data_digest_ok: bool,
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
        ttag: u16,
        datao: u32,
        datal: u32,
        data: Vec<u8>,
        last: bool,
        /// As for a capsule: false when the data digest did not match.
        data_digest_ok: bool,
    },
    /// The host ends the connection (#392): read, said, closed.
    H2CTermReq {
        header: CommonHeader,
        fes: u16,
        fei: u32,
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
const CAPSULE_CMD_HLEN: u8 = 72;  // 8 (common) + 64 (SQE)
const CAPSULE_RESP_HLEN: u8 = 24; // 8 (common) + 16 (CQE)
const C2H_DATA_HLEN: u8 = 24;
const H2C_DATA_HLEN: u8 = 24;
const R2T_HLEN: u8 = 24;

/// The largest PDU a target reads (#407): the largest transfer a command may
/// carry ([`crate::MAX_TRANSFER_CEILING`]) and its headers. A PDU's length
/// is the peer's word, read before anything is authenticated; without a
/// bound, one header could make the target allocate 4 GiB.
pub const MAX_PDU_BYTES: usize = crate::MAX_TRANSFER_CEILING + 4096;

/// The digests a connection negotiated in its ICReq/ICResp (#392). Before
/// the handshake there are none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Digests {
    pub header: bool,
    pub data: bool,
}

/// Fatal Error Status values of a C2HTermReq (NVMe/TCP 1.0, §3.6.2.9).
pub mod fes {
    pub const INVALID_HEADER_FIELD: u16 = 0x01;
    pub const PDU_SEQUENCE_ERROR: u16 = 0x02;
    pub const HEADER_DIGEST_ERROR: u16 = 0x03;
    pub const DATA_OUT_OF_RANGE: u16 = 0x04;
    pub const DATA_LIMIT_EXCEEDED: u16 = 0x05;
    pub const UNSUPPORTED_PARAMETER: u16 = 0x06;
}

/// A transport error the specification calls fatal (#392): the target
/// answers it with a C2HTermReq ([`write_c2h_term_req`]) and closes the
/// connection. `fei` is the byte offset of the field in error, counted from
/// the start of the PDU, for the two statuses that name one; `header` is as
/// much of the offending PDU's header as was read.
#[derive(Debug, Clone)]
pub struct Fatal {
    pub fes: u16,
    pub fei: u32,
    pub header: Vec<u8>,
    pub what: String,
}

impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (fatal error status {:#04x}, field {})", self.what, self.fes, self.fei)
    }
}

impl std::error::Error for Fatal {}

impl Fatal {
    pub fn new(fes: u16, fei: u32, header: &[u8], what: impl Into<String>) -> Self {
        Fatal { fes, fei, header: header[..header.len().min(MAX_TERM_HEADER)].to_vec(), what: what.into() }
    }

    /// A PDU that may not come now: a second ICReq, a Connect's place taken
    /// by something else, H2CData with no R2T outstanding.
    pub fn out_of_sequence(pdu: &NvmeofPdu, what: impl Into<String>) -> Self {
        Self::new(fes::PDU_SEQUENCE_ERROR, 0, &pdu.header().to_bytes(), what)
    }

    pub fn into_io(self) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidData, self)
    }
}

/// The fatal transport error inside an I/O error, when it is one.
pub fn fatal_of(e: &std::io::Error) -> Option<&Fatal> {
    e.get_ref()?.downcast_ref::<Fatal>()
}

/// A C2HTermReq carries at most this much of the PDU in error.
const MAX_TERM_HEADER: usize = 152;
const TERM_REQ_HLEN: u8 = 24;

impl NvmeofPdu {
    pub fn header(&self) -> &CommonHeader {
        match self {
            NvmeofPdu::ICReq(h, _) | NvmeofPdu::ICResp(h, _) => h,
            NvmeofPdu::CapsuleCmd { header, .. }
            | NvmeofPdu::CapsuleResp { header, .. }
            | NvmeofPdu::C2HData { header, .. }
            | NvmeofPdu::H2CData { header, .. }
            | NvmeofPdu::R2T { header, .. }
            | NvmeofPdu::H2CTermReq { header, .. } => header,
        }
    }
}

/// Read one PDU before the handshake: the ICReq, with no digests.
pub async fn read_pdu<R: AsyncReadExt + Unpin>(stream: &mut R) -> std::io::Result<NvmeofPdu> {
    read_pdu_with(stream, Digests::default()).await
}

/// Read one PDU a host sent, as the NVMe/TCP transport specification has it
/// (#392). Everything the header claims is checked before any of the body
/// is read or allocated: the PDU type is one a host sends, the header length
/// is that type's, the PDU length and data offset fit inside each other, and
/// the digest flags are the ones the connection negotiated. A header digest
/// that does not match is fatal; a data digest that does not match is said
/// on the PDU (`data_digest_ok`) and the command fails on its own. Every
/// fatal error is a [`Fatal`] inside the `io::Error`.
pub async fn read_pdu_with<R: AsyncReadExt + Unpin>(stream: &mut R, dg: Digests) -> std::io::Result<NvmeofPdu> {
    let mut hdr_buf = [0u8; 8];
    stream.read_exact(&mut hdr_buf).await?;
    let ch = CommonHeader::from_bytes(&hdr_buf);
    let bad = |fes: u16, fei: u32, header: &[u8], what: String| Fatal::new(fes, fei, header, what).into_io();

    let Some(pdu_type) = PduType::from_byte(ch.pdu_type) else {
        return Err(bad(fes::INVALID_HEADER_FIELD, 0, &hdr_buf, format!("unknown PDU type {:#04x}", ch.pdu_type)));
    };
    // The PDU's length is the peer's word, read before anything is
    // authenticated (#407): bounded before anything is allocated for it.
    if ch.plen as usize > MAX_PDU_BYTES {
        return Err(bad(
            fes::DATA_LIMIT_EXCEEDED,
            4,
            &hdr_buf,
            format!("PDU length {} is past the {MAX_PDU_BYTES} this target takes", ch.plen),
        ));
    }

    match pdu_type {
        PduType::ICReq => {
            if ch.hlen != 128 {
                return Err(bad(fes::INVALID_HEADER_FIELD, 2, &hdr_buf, format!("ICReq header length {}, not 128", ch.hlen)));
            }
            if ch.plen != 128 {
                return Err(bad(fes::INVALID_HEADER_FIELD, 4, &hdr_buf, format!("ICReq PDU length {}, not 128", ch.plen)));
            }
            if ch.flags != 0 {
                return Err(bad(fes::INVALID_HEADER_FIELD, 1, &hdr_buf, format!("ICReq flags {:#04x}", ch.flags)));
            }
            if ch.pdo != 0 {
                return Err(bad(fes::INVALID_HEADER_FIELD, 3, &hdr_buf, format!("ICReq data offset {}", ch.pdo)));
            }
            let mut rest = vec![0u8; 120];
            stream.read_exact(&mut rest).await?;
            let icreq = ICReq::from_bytes(&rest).expect("120 bytes");
            Ok(NvmeofPdu::ICReq(ch, icreq))
        }
        PduType::CapsuleCmd => read_capsule(stream, ch, hdr_buf, dg).await,
        PduType::H2CData => read_h2c_data(stream, ch, hdr_buf, dg).await,
        PduType::H2CTermReq => {
            if ch.hlen != TERM_REQ_HLEN {
                return Err(bad(fes::INVALID_HEADER_FIELD, 2, &hdr_buf, format!("H2CTermReq header length {}", ch.hlen)));
            }
            let max = TERM_REQ_HLEN as u32 + MAX_TERM_HEADER as u32;
            if ch.plen < TERM_REQ_HLEN as u32 || ch.plen > max {
                return Err(bad(fes::INVALID_HEADER_FIELD, 4, &hdr_buf, format!("H2CTermReq PDU length {}", ch.plen)));
            }
            let mut rest = vec![0u8; ch.plen as usize - 8];
            stream.read_exact(&mut rest).await?;
            let fes = u16::from_le_bytes([rest[0], rest[1]]);
            let fei = u32::from_le_bytes(rest[2..6].try_into().unwrap());
            Ok(NvmeofPdu::H2CTermReq { header: ch, fes, fei })
        }
        // ICResp, C2HTermReq, CapsuleResp, C2HData, R2T: a controller's.
        other => Err(bad(fes::INVALID_HEADER_FIELD, 0, &hdr_buf, format!("{other:?} is a controller's PDU, not a host's"))),
    }
}

/// Check the digest flags a PDU carries against the negotiated ones.
fn digest_flags(ch: &CommonHeader, hdr: &[u8], header: bool, data: bool) -> std::io::Result<()> {
    if ch.hdgst_enable() != header || ch.ddgst_enable() != data {
        return Err(Fatal::new(
            fes::INVALID_HEADER_FIELD,
            1,
            hdr,
            format!(
                "digest flags {:#04x}: the connection carries {} header digest and {} data digest here",
                ch.flags & 0x03,
                if header { "a" } else { "no" },
                if data { "a" } else { "no" },
            ),
        )
        .into_io());
    }
    Ok(())
}

/// Read and check a header digest over `header`.
async fn header_digest<R: AsyncReadExt + Unpin>(stream: &mut R, header: &[u8]) -> std::io::Result<()> {
    let mut dgst = [0u8; 4];
    stream.read_exact(&mut dgst).await?;
    if crc32c::crc32c(header) != u32::from_le_bytes(dgst) {
        return Err(Fatal::new(fes::HEADER_DIGEST_ERROR, 0, header, "header digest mismatch").into_io());
    }
    Ok(())
}

/// The bytes between a header (and its digest) and the data `pdo` names.
async fn skip<R: AsyncReadExt + Unpin>(stream: &mut R, n: usize) -> std::io::Result<()> {
    if n > 0 {
        let mut pad = vec![0u8; n];
        stream.read_exact(&mut pad).await?;
    }
    Ok(())
}

/// Read data and, when negotiated, its digest: whether the digest matched.
async fn data_and_digest<R: AsyncReadExt + Unpin>(stream: &mut R, len: usize, digest: bool) -> std::io::Result<(Vec<u8>, bool)> {
    let mut data = vec![0u8; len];
    stream.read_exact(&mut data).await?;
    let mut ok = true;
    if digest {
        let mut dgst = [0u8; 4];
        stream.read_exact(&mut dgst).await?;
        ok = crc32c::crc32c(&data) == u32::from_le_bytes(dgst);
    }
    Ok((data, ok))
}

async fn read_capsule<R: AsyncReadExt + Unpin>(
    stream: &mut R,
    ch: CommonHeader,
    hdr_buf: [u8; 8],
    dg: Digests,
) -> std::io::Result<NvmeofPdu> {
    let bad = |fei: u32, header: &[u8], what: String| Fatal::new(fes::INVALID_HEADER_FIELD, fei, header, what).into_io();
    if ch.hlen != CAPSULE_CMD_HLEN {
        return Err(bad(2, &hdr_buf, format!("CapsuleCmd header length {}, not {CAPSULE_CMD_HLEN}", ch.hlen)));
    }
    let hd = if dg.header { 4 } else { 0 };
    let after_hdr = CAPSULE_CMD_HLEN as usize + hd;
    let plen = ch.plen as usize;
    if plen < after_hdr {
        return Err(bad(4, &hdr_buf, format!("CapsuleCmd PDU length {plen}, shorter than its header ({after_hdr})")));
    }
    let has_data = plen > after_hdr;
    digest_flags(&ch, &hdr_buf, dg.header, dg.data && has_data)?;
    let dd = if dg.data && has_data { 4 } else { 0 };
    let pdo = ch.pdo as usize;
    if has_data {
        // In-capsule data starts at `pdo`, at or past the header and its
        // digest, and is not empty.
        if pdo < after_hdr || pdo + dd >= plen {
            return Err(bad(3, &hdr_buf, format!("CapsuleCmd data offset {pdo} in a PDU of {plen} (header {after_hdr})")));
        }
    } else if pdo != 0 && pdo != after_hdr {
        return Err(bad(3, &hdr_buf, format!("CapsuleCmd data offset {pdo} with no data")));
    }

    let mut sqe_buf = [0u8; 64];
    stream.read_exact(&mut sqe_buf).await?;
    let mut header = Vec::with_capacity(72);
    header.extend_from_slice(&hdr_buf);
    header.extend_from_slice(&sqe_buf);
    if dg.header {
        header_digest(stream, &header).await?;
    }
    let sqe = NvmeSqe::from_bytes(&sqe_buf);
    if !has_data {
        return Ok(NvmeofPdu::CapsuleCmd { header: ch, sqe, data: Vec::new(), data_digest_ok: true });
    }
    skip(stream, pdo - after_hdr).await?;
    let (data, data_digest_ok) = data_and_digest(stream, plen - pdo - dd, dd != 0).await?;
    Ok(NvmeofPdu::CapsuleCmd { header: ch, sqe, data, data_digest_ok })
}

async fn read_h2c_data<R: AsyncReadExt + Unpin>(
    stream: &mut R,
    ch: CommonHeader,
    hdr_buf: [u8; 8],
    dg: Digests,
) -> std::io::Result<NvmeofPdu> {
    let bad = |fei: u32, header: &[u8], what: String| Fatal::new(fes::INVALID_HEADER_FIELD, fei, header, what).into_io();
    if ch.hlen != H2C_DATA_HLEN {
        return Err(bad(2, &hdr_buf, format!("H2CData header length {}, not {H2C_DATA_HLEN}", ch.hlen)));
    }
    digest_flags(&ch, &hdr_buf, dg.header, dg.data)?;
    let hd = if dg.header { 4 } else { 0 };
    let dd = if dg.data { 4 } else { 0 };
    let after_hdr = H2C_DATA_HLEN as usize + hd;
    let (plen, pdo) = (ch.plen as usize, ch.pdo as usize);
    if pdo < after_hdr || pdo + dd >= plen {
        return Err(bad(3, &hdr_buf, format!("H2CData data offset {pdo} in a PDU of {plen} (header {after_hdr})")));
    }

    let mut rest = [0u8; 16];
    stream.read_exact(&mut rest).await?;
    let mut header = Vec::with_capacity(24);
    header.extend_from_slice(&hdr_buf);
    header.extend_from_slice(&rest);
    if dg.header {
        header_digest(stream, &header).await?;
    }
    let cccid = u16::from_le_bytes([rest[0], rest[1]]);
    let ttag = u16::from_le_bytes([rest[2], rest[3]]);
    let datao = u32::from_le_bytes(rest[4..8].try_into().unwrap());
    let datal = u32::from_le_bytes(rest[8..12].try_into().unwrap());
    let last = ch.flags & 0x04 != 0;
    let len = plen - pdo - dd;
    if datal as usize != len {
        return Err(bad(16, &header, format!("H2CData data length {datal}, but the PDU carries {len} bytes")));
    }
    skip(stream, pdo - after_hdr).await?;
    let (data, data_digest_ok) = data_and_digest(stream, len, dd != 0).await?;
    Ok(NvmeofPdu::H2CData { header: ch, cccid, ttag, datao, datal, data, last, data_digest_ok })
}

/// One R2T a target sent, and the H2CData that has come back for it (#392).
/// Each H2CData PDU must name the command and transfer tag of the R2T, start
/// where the last one ended (Linux's target holds hosts to the same order),
/// carry no more than MAXH2CDATA, stay within the R2T, and set LAST_PDU on
/// the one that completes it and on no other.
#[derive(Debug, Clone)]
pub struct R2tTransfer {
    cid: u16,
    ttag: u16,
    r2to: u32,
    r2tl: u32,
    done: u32,
    max_h2c: u32,
    digest_bad: bool,
}

impl R2tTransfer {
    pub fn new(cid: u16, ttag: u16, r2to: u32, r2tl: u32, max_h2c: u32) -> Self {
        R2tTransfer { cid, ttag, r2to, r2tl, done: 0, max_h2c, digest_bad: false }
    }

    /// Take one H2CData PDU. `Ok(true)` once the R2T is complete; a
    /// [`Fatal`] when the PDU is not this R2T's.
    #[allow(clippy::too_many_arguments)]
    pub fn take(
        &mut self,
        header: &CommonHeader,
        cccid: u16,
        ttag: u16,
        datao: u32,
        datal: u32,
        last: bool,
        data_digest_ok: bool,
    ) -> Result<bool, Fatal> {
        let mut raw = header.to_bytes().to_vec();
        raw.extend_from_slice(&cccid.to_le_bytes());
        raw.extend_from_slice(&ttag.to_le_bytes());
        raw.extend_from_slice(&datao.to_le_bytes());
        raw.extend_from_slice(&datal.to_le_bytes());
        raw.extend_from_slice(&[0u8; 4]);
        let fail = |fes: u16, fei: u32, what: String| Err(Fatal::new(fes, fei, &raw, what));
        if cccid != self.cid {
            return fail(fes::INVALID_HEADER_FIELD, 8, format!("H2CData for command {cccid}, while command {}'s R2T is outstanding", self.cid));
        }
        if ttag != self.ttag {
            return fail(fes::INVALID_HEADER_FIELD, 10, format!("H2CData with transfer tag {ttag}, not the R2T's {}", self.ttag));
        }
        if datal > self.max_h2c {
            return fail(fes::DATA_LIMIT_EXCEEDED, 16, format!("H2CData of {datal} bytes, past MAXH2CDATA {}", self.max_h2c));
        }
        let expect = self.r2to + self.done;
        if datao != expect {
            return fail(fes::DATA_OUT_OF_RANGE, 12, format!("H2CData at offset {datao}, where {expect} comes next"));
        }
        let done = self.done as u64 + datal as u64;
        if done > self.r2tl as u64 {
            return fail(fes::DATA_OUT_OF_RANGE, 16, format!("H2CData past the R2T: {done} of {} bytes", self.r2tl));
        }
        let complete = done == self.r2tl as u64;
        if last != complete {
            return fail(
                fes::INVALID_HEADER_FIELD,
                1,
                format!("H2CData LAST_PDU {}set with {done} of {} bytes", if last { "" } else { "not " }, self.r2tl),
            );
        }
        self.done = done as u32;
        self.digest_bad |= !data_digest_ok;
        Ok(complete)
    }

    /// Whether any of the R2T's data failed its digest: the command is then
    /// completed with Transient Transport Error, and its data not used.
    pub fn digest_bad(&self) -> bool {
        self.digest_bad
    }
}

/// Write a C2HTermReq for a fatal error (#392); the caller closes the
/// connection after it. Never carries a digest.
pub async fn write_c2h_term_req<W: AsyncWriteExt + Unpin>(stream: &mut W, f: &Fatal) -> std::io::Result<()> {
    let header = &f.header[..f.header.len().min(MAX_TERM_HEADER)];
    let ch = CommonHeader {
        pdu_type: PduType::C2HTermReq as u8,
        flags: 0,
        hlen: TERM_REQ_HLEN,
        pdo: 0,
        plen: TERM_REQ_HLEN as u32 + header.len() as u32,
    };
    let mut specific = [0u8; 16];
    specific[0..2].copy_from_slice(&f.fes.to_le_bytes());
    // FEI only for the statuses that name a field.
    if matches!(f.fes, fes::INVALID_HEADER_FIELD | fes::UNSUPPORTED_PARAMETER) {
        specific[2..6].copy_from_slice(&f.fei.to_le_bytes());
    }
    stream.write_all(&ch.to_bytes()).await?;
    stream.write_all(&specific).await?;
    stream.write_all(header).await?;
    stream.flush().await
}

/// The ICReq fields this target cannot honour, refused as the specification
/// asks (Unsupported Parameter): a PDU format other than 0, and a host PDU
/// data alignment it does not pad C2HData to (#392).
pub fn check_icreq(ch: &CommonHeader, icreq: &ICReq) -> Result<(), Fatal> {
    let hdr = ch.to_bytes();
    if icreq.pfv != 0 {
        return Err(Fatal::new(fes::UNSUPPORTED_PARAMETER, 8, &hdr, format!("PDU format version {}", icreq.pfv)));
    }
    if icreq.hpda != 0 {
        return Err(Fatal::new(fes::UNSUPPORTED_PARAMETER, 10, &hdr, format!("host PDU data alignment {}", icreq.hpda)));
    }
    Ok(())
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
#[allow(clippy::too_many_arguments)]
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
    // The data follows the header and its digest (#392: pdo named the
    // digest's first byte when one was negotiated).
    let pdo = hlen + if hdgst { 4 } else { 0 };
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

    // ---- #392: the reader holds a host to the specification ----

    const BOTH: Digests = Digests { header: true, data: true };

    fn ch(t: u8, flags: u8, hlen: u8, pdo: u8, plen: u32) -> Vec<u8> {
        CommonHeader { pdu_type: t, flags, hlen, pdo, plen }.to_bytes().to_vec()
    }

    /// A capsule: optional header digest, `pad` bytes, data, optional data
    /// digest; `bad_*` corrupts a digest.
    fn capsule(dg: Digests, pad: usize, data: &[u8], bad_h: bool, bad_d: bool) -> Vec<u8> {
        let hd = if dg.header { 4 } else { 0 };
        let dd = if dg.data && !data.is_empty() { 4 } else { 0 };
        let pdo = if data.is_empty() { 0 } else { 72 + hd + pad };
        let plen = if data.is_empty() { 72 + hd } else { pdo + data.len() + dd };
        let flags = (dg.header as u8) | if dd != 0 { 2 } else { 0 };
        let mut v = ch(0x04, flags, 72, pdo as u8, plen as u32);
        let mut sqe = [0u8; 64];
        sqe[0] = 0x01;
        sqe[2] = 9;
        v.extend_from_slice(&sqe);
        if dg.header {
            let c = crc32c::crc32c(&v) ^ bad_h as u32;
            v.extend_from_slice(&c.to_le_bytes());
        }
        v.extend(std::iter::repeat_n(0xEEu8, if data.is_empty() { 0 } else { pad }));
        v.extend_from_slice(data);
        if dd != 0 {
            v.extend_from_slice(&(crc32c::crc32c(data) ^ bad_d as u32).to_le_bytes());
        }
        v
    }

    fn h2c(dg: Digests, data: &[u8], datal: u32, pad: usize, bad_h: bool, bad_d: bool) -> Vec<u8> {
        let hd = if dg.header { 4 } else { 0 };
        let dd = if dg.data { 4 } else { 0 };
        let pdo = 24 + hd + pad;
        let flags = (dg.header as u8) | ((dg.data as u8) << 1) | 0x04;
        let mut v = ch(0x06, flags, 24, pdo as u8, (pdo + data.len() + dd) as u32);
        v.extend_from_slice(&5u16.to_le_bytes());
        v.extend_from_slice(&6u16.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&datal.to_le_bytes());
        v.extend_from_slice(&[0u8; 4]);
        if dg.header {
            v.extend_from_slice(&(crc32c::crc32c(&v) ^ bad_h as u32).to_le_bytes());
        }
        v.extend(std::iter::repeat_n(0u8, pad));
        v.extend_from_slice(data);
        if dg.data {
            v.extend_from_slice(&(crc32c::crc32c(data) ^ bad_d as u32).to_le_bytes());
        }
        v
    }

    async fn read(bytes: &[u8], dg: Digests) -> std::io::Result<NvmeofPdu> {
        let mut r = bytes;
        read_pdu_with(&mut r, dg).await
    }

    async fn fatal(bytes: &[u8], dg: Digests) -> (u16, u32) {
        match read(bytes, dg).await {
            Err(e) => {
                let f = fatal_of(&e).unwrap_or_else(|| panic!("not a fatal transport error: {e}"));
                (f.fes, f.fei)
            }
            Ok(_) => panic!("taken"),
        }
    }

    #[tokio::test]
    async fn a_capsule_is_read_with_its_digests_and_padding() {
        for dg in [Digests::default(), BOTH, Digests { header: true, data: false }, Digests { header: false, data: true }] {
            for pad in [0, 4, 12] {
                let NvmeofPdu::CapsuleCmd { sqe, data, data_digest_ok, .. } = read(&capsule(dg, pad, b"inline data!", false, false), dg).await.unwrap()
                else {
                    panic!()
                };
                assert_eq!((sqe.opcode(), sqe.cid(), data.as_slice(), data_digest_ok), (1, 9, &b"inline data!"[..], true), "{dg:?} pad {pad}");
            }
            let NvmeofPdu::CapsuleCmd { data, .. } = read(&capsule(dg, 0, b"", false, false), dg).await.unwrap() else { panic!() };
            assert!(data.is_empty());
        }
    }

    /// A data digest that does not match is said, not fatal; a header digest
    /// that does not match is fatal.
    #[tokio::test]
    async fn digests_are_checked() {
        let NvmeofPdu::CapsuleCmd { data_digest_ok, .. } = read(&capsule(BOTH, 0, b"x", false, true), BOTH).await.unwrap() else { panic!() };
        assert!(!data_digest_ok);
        assert_eq!(fatal(&capsule(BOTH, 0, b"x", true, false), BOTH).await.0, fes::HEADER_DIGEST_ERROR);
        let NvmeofPdu::H2CData { data_digest_ok, ttag, .. } = read(&h2c(BOTH, b"abcd", 4, 0, false, true), BOTH).await.unwrap() else { panic!() };
        assert_eq!((data_digest_ok, ttag), (false, 6));
        assert_eq!(fatal(&h2c(BOTH, b"abcd", 4, 0, true, false), BOTH).await.0, fes::HEADER_DIGEST_ERROR);
        let NvmeofPdu::H2CData { data, data_digest_ok, .. } = read(&h2c(BOTH, b"abcd", 4, 8, false, false), BOTH).await.unwrap() else { panic!() };
        assert_eq!((data.as_slice(), data_digest_ok), (&b"abcd"[..], true));
    }

    /// The digest flags are the negotiated ones, or the PDU is refused at
    /// the flags byte.
    #[tokio::test]
    async fn digest_flags_must_match_the_negotiation() {
        let none = Digests::default();
        assert_eq!(fatal(&capsule(BOTH, 0, b"x", false, false), none).await, (fes::INVALID_HEADER_FIELD, 1));
        assert_eq!(fatal(&capsule(none, 0, b"x", false, false), BOTH).await, (fes::INVALID_HEADER_FIELD, 1));
        assert_eq!(fatal(&h2c(none, b"ab", 2, 0, false, false), BOTH).await, (fes::INVALID_HEADER_FIELD, 1));
    }

    /// Lengths are checked before a byte of the body is read: each stream
    /// here holds only the eight-byte common header, so reading on would be
    /// an end of file, not a refusal.
    #[tokio::test]
    async fn lengths_are_checked_before_the_body_is_read() {
        let none = Digests::default();
        let cases: &[(Vec<u8>, Digests, u16, u32, &str)] = &[
            (ch(0x04, 0, 72, 0, 70), none, fes::INVALID_HEADER_FIELD, 4, "capsule shorter than its header"),
            (ch(0x04, 1, 72, 0, 72), BOTH, fes::INVALID_HEADER_FIELD, 4, "capsule with no room for its header digest"),
            (ch(0x04, 0, 64, 0, 64), none, fes::INVALID_HEADER_FIELD, 2, "capsule header length"),
            (ch(0x04, 0, 72, 40, 80), none, fes::INVALID_HEADER_FIELD, 3, "capsule data inside its header"),
            (ch(0x04, 0, 72, 30, 72), none, fes::INVALID_HEADER_FIELD, 3, "capsule data offset with no data"),
            (ch(0x06, 0x04, 24, 24, 24), none, fes::INVALID_HEADER_FIELD, 3, "H2CData with no data"),
            (ch(0x06, 0x04, 24, 16, 64), none, fes::INVALID_HEADER_FIELD, 3, "H2CData data inside its header"),
            (ch(0x06, 0x04, 32, 32, 64), none, fes::INVALID_HEADER_FIELD, 2, "H2CData header length"),
            (ch(0x00, 0, 120, 0, 128), none, fes::INVALID_HEADER_FIELD, 2, "ICReq header length"),
            (ch(0x00, 0, 128, 0, 256), none, fes::INVALID_HEADER_FIELD, 4, "ICReq PDU length"),
            (ch(0x00, 1, 128, 0, 128), none, fes::INVALID_HEADER_FIELD, 1, "ICReq flags"),
            (ch(0x04, 0, 72, 72, u32::MAX), none, fes::DATA_LIMIT_EXCEEDED, 4, "a 4 GiB PDU"),
            (ch(0x09, 0, 24, 0, 24), none, fes::INVALID_HEADER_FIELD, 0, "an R2T from a host"),
            (ch(0x07, 0, 24, 24, 28), none, fes::INVALID_HEADER_FIELD, 0, "C2HData from a host"),
            (ch(0x0F, 0, 24, 0, 24), none, fes::INVALID_HEADER_FIELD, 0, "an unknown type"),
            (ch(0x02, 0, 24, 0, 400), none, fes::INVALID_HEADER_FIELD, 4, "an H2CTermReq too long"),
        ];
        for (bytes, dg, want_fes, want_fei, what) in cases {
            assert_eq!(fatal(bytes, *dg).await, (*want_fes, *want_fei), "{what}");
        }
        // H2CData whose DATAL is not what it carries: read up to DATAL.
        assert_eq!(fatal(&h2c(none, b"abcd", 8, 0, false, false), none).await, (fes::INVALID_HEADER_FIELD, 16));
    }

    #[tokio::test]
    async fn an_icreq_this_target_cannot_honour_is_refused_and_an_h2c_term_req_read() {
        let mut v = ch(0x00, 0, 128, 0, 128);
        v.extend_from_slice(&[0u8; 120]);
        let NvmeofPdu::ICReq(h, req) = read(&v, Digests::default()).await.unwrap() else { panic!() };
        assert!(check_icreq(&h, &req).is_ok());
        let f = check_icreq(&h, &ICReq { pfv: 1, ..req.clone() }).unwrap_err();
        assert_eq!((f.fes, f.fei), (fes::UNSUPPORTED_PARAMETER, 8));
        let f = check_icreq(&h, &ICReq { hpda: 3, ..req }).unwrap_err();
        assert_eq!((f.fes, f.fei), (fes::UNSUPPORTED_PARAMETER, 10));

        let mut t = ch(0x02, 0, 24, 0, 32);
        t.extend_from_slice(&fes::HEADER_DIGEST_ERROR.to_le_bytes());
        t.extend_from_slice(&[0u8; 14]);
        t.extend_from_slice(&[7u8; 8]);
        let NvmeofPdu::H2CTermReq { fes: f, .. } = read(&t, BOTH).await.unwrap() else { panic!() };
        assert_eq!(f, fes::HEADER_DIGEST_ERROR);
    }

    #[test]
    fn h2c_data_answers_its_r2t_in_order_or_is_refused() {
        let h = CommonHeader { pdu_type: 0x06, flags: 0, hlen: 24, pdo: 24, plen: 0 };
        let r2t = || R2tTransfer::new(7, 7, 4096, 8192, 4096);
        // In order, LAST on the one that completes it.
        let mut t = r2t();
        assert_eq!(t.take(&h, 7, 7, 4096, 4096, false, true).unwrap(), false);
        assert_eq!(t.take(&h, 7, 7, 8192, 4096, true, false).unwrap(), true);
        assert!(t.digest_bad(), "a bad data digest anywhere fails the command");
        let refused = |cccid, ttag, datao, datal, last| r2t().take(&h, cccid, ttag, datao, datal, last, true).map(|_| ()).unwrap_err();
        let cases = [
            (refused(8, 7, 4096, 4096, false), fes::INVALID_HEADER_FIELD, 8, "another command"),
            (refused(7, 8, 4096, 4096, false), fes::INVALID_HEADER_FIELD, 10, "another transfer tag"),
            (refused(7, 7, 4096, 8192, true), fes::DATA_LIMIT_EXCEEDED, 16, "past MAXH2CDATA"),
            (refused(7, 7, 8192, 4096, false), fes::DATA_OUT_OF_RANGE, 12, "skipping ahead"),
            (refused(7, 7, 0, 4096, false), fes::DATA_OUT_OF_RANGE, 12, "before the R2T's offset"),
            (refused(7, 7, 4096, 4096, true), fes::INVALID_HEADER_FIELD, 1, "LAST before the end"),
        ];
        for (f, want_fes, want_fei, what) in cases {
            assert_eq!((f.fes, f.fei), (want_fes, want_fei), "{what}");
        }
        let mut t = R2tTransfer::new(7, 7, 0, 4096, 8192);
        let f = t.take(&h, 7, 7, 0, 8192, true, true).unwrap_err();
        assert_eq!(f.fes, fes::DATA_OUT_OF_RANGE, "past the R2T");
        let mut t = R2tTransfer::new(7, 7, 0, 4096, 8192);
        assert_eq!(t.take(&h, 7, 7, 0, 4096, false, true).unwrap_err().fei, 1, "no LAST at the end");
    }

    #[tokio::test]
    async fn a_c2h_term_req_names_the_error_and_carries_the_header() {
        let f = Fatal::new(fes::INVALID_HEADER_FIELD, 16, &[0xAB; 200], "x");
        let mut buf = Vec::new();
        write_c2h_term_req(&mut buf, &f).await.unwrap();
        let h = CommonHeader::from_bytes(buf[..8].try_into().unwrap());
        assert_eq!((h.pdu_type, h.flags, h.hlen, h.plen as usize), (0x03, 0, 24, 24 + 152));
        assert_eq!(u16::from_le_bytes([buf[8], buf[9]]), fes::INVALID_HEADER_FIELD);
        assert_eq!(u32::from_le_bytes(buf[10..14].try_into().unwrap()), 16);
        assert_eq!(buf.len(), 24 + 152, "at most 152 bytes of the PDU in error");
        // A status that names no field leaves FEI zero.
        let mut buf = Vec::new();
        write_c2h_term_req(&mut buf, &Fatal::new(fes::HEADER_DIGEST_ERROR, 9, &[1; 8], "x")).await.unwrap();
        assert_eq!(u32::from_le_bytes(buf[10..14].try_into().unwrap()), 0);
    }

    /// C2HData's data offset counts the header digest (#392).
    #[tokio::test]
    async fn c2h_data_offset_counts_the_header_digest() {
        for (hdgst, pdo) in [(false, 24), (true, 28)] {
            let mut buf = Vec::new();
            write_c2h_data(&mut buf, 1, 0, b"abcd", true, true, hdgst, false).await.unwrap();
            assert_eq!(buf[3], pdo);
            assert_eq!(&buf[pdo as usize..pdo as usize + 4], b"abcd");
        }
    }
}
