//! Identify data and the admin command numbers, shared with the engine.
//!
//! The engine's `identify_controller` and `identify_namespace` are built on
//! these and add what only the engine has (ANA, #83): the bytes here are
//! every field both targets report the same way.

/// NVMe admin opcodes.
pub const ADMIN_IDENTIFY: u8 = 0x06;
pub const ADMIN_GET_LOG_PAGE: u8 = 0x02;
pub const ADMIN_ABORT: u8 = 0x08;
pub const ADMIN_SET_FEATURES: u8 = 0x09;
pub const ADMIN_GET_FEATURES: u8 = 0x0A;
pub const ADMIN_ASYNC_EVENT_REQ: u8 = 0x0C;
pub const ADMIN_KEEP_ALIVE: u8 = 0x18;

/// Feature identifier: Number of Queues (NVMe 1.4 §5.21.1.7).
///
/// The host asks how many I/O queue pairs it may create; the controller
/// answers in completion DW0. Both the request and the reply are 0-based.
pub const FEAT_NUM_QUEUES: u8 = 0x07;

/// Identify CNS values.
pub const CNS_NAMESPACE: u8 = 0x00;
pub const CNS_CONTROLLER: u8 = 0x01;
pub const CNS_ACTIVE_NS_LIST: u8 = 0x02;
pub const CNS_NS_DESC_LIST: u8 = 0x03;

/// Identify Namespace: NSATTR's offset, and its write-protected bit. A host
/// that reads it set makes the block device read-only (Linux: `set_disk_ro`).
pub const NSATTR_OFFSET: usize = 99;
pub const NSATTR_WRITE_PROTECTED: u8 = 1 << 0;

/// Sentinel in the Changed Namespace List meaning "more changed than fit —
/// rescan everything".
pub const NS_LIST_OVERFLOW: u32 = 0xFFFF_FFFF;

/// MDTS for a transfer size: log2 of 4 KiB pages.
pub fn mdts_for(bytes: usize) -> u8 {
    (bytes.max(4096) / 4096).trailing_zeros() as u8
}

/// Namespace Identification Descriptor list (CNS 0x03): one UUID descriptor
/// then a zero terminator. The Linux initiator issues this during namespace
/// scan and aborts the namespace when it errors.
pub fn ns_desc_list(uuid: &[u8; 16]) -> Vec<u8> {
    let mut data = vec![0u8; 4096];
    data[0] = 0x03; // NIDT = UUID
    data[1] = 16; // NIDL
    data[4..20].copy_from_slice(uuid);
    data
}

/// Identify Controller data (4096 bytes), with no ANA fields.
///
/// `discovery` selects CNTRLTYPE (the kernel refuses `nvme discover` against
/// a controller that doesn't identify as a discovery controller). CNTLID
/// (bytes 78..80) is the caller's, set once the controller has one.
pub fn controller(
    subnqn: &str,
    serial: &str,
    model: &str,
    firmware: &str,
    max_namespaces: u32,
    discovery: bool,
    mdts: u8,
) -> Vec<u8> {
    let mut data = vec![0u8; 4096];

    // VID, SSVID: 0 for fabrics.
    // Serial Number (bytes 4-23, 20 bytes, ASCII, space-padded)
    let sn = serial.as_bytes();
    let sn_len = sn.len().min(20);
    data[4..4 + sn_len].copy_from_slice(&sn[..sn_len]);
    for b in &mut data[4 + sn_len..24] {
        *b = b' ';
    }

    // Model Number (bytes 24-63, 40 bytes, ASCII, space-padded)
    let mn = model.as_bytes();
    let mn_len = mn.len().min(40);
    data[24..24 + mn_len].copy_from_slice(&mn[..mn_len]);
    for b in &mut data[24 + mn_len..64] {
        *b = b' ';
    }

    // Firmware Revision (bytes 64-71, 8 bytes)
    let fw = firmware.as_bytes();
    let fw_len = fw.len().min(8);
    data[64..64 + fw_len].copy_from_slice(&fw[..fw_len]);
    for b in &mut data[64 + fw_len..72] {
        *b = b' ';
    }

    // MDTS (Maximum Data Transfer Size) — log2 of 4 KiB pages (#407).
    data[77] = mdts;

    // VER = NVMe 1.4
    data[80..84].copy_from_slice(&0x00010400u32.to_le_bytes());

    // OAES (Optional Asynchronous Events Supported), bit 8 = Namespace
    // Attribute Changed notices. Without this the host will not arm for the
    // event, and a hot-added namespace stays invisible until it reconnects.
    data[92..96].copy_from_slice(&(1u32 << 8).to_le_bytes());

    // CNTRLTYPE (byte 111): 1 = I/O controller, 2 = discovery controller
    data[111] = if discovery { 2 } else { 1 };

    // KAS (bytes 320-321): Keep Alive granularity in 100 ms units.
    // Mandatory nonzero for fabrics — the Linux initiator rejects the
    // controller outright otherwise ("keep-alive support is mandatory").
    data[320..322].copy_from_slice(&10u16.to_le_bytes());

    // ACLS (Abort Command Limit) = 3
    data[258] = 3;
    // AERL (Async Event Request Limit) = 3
    data[259] = 3;
    // FRMW (Firmware Updates) — slot 1, no commit
    data[260] = 0x01;

    // SQES: min=6(64B), max=6(64B); CQES: min=4(16B), max=4(16B)
    data[512] = 0x66;
    data[513] = 0x44;

    // MAXCMD
    data[514..516].copy_from_slice(&256u16.to_le_bytes());

    // NN (Number of Namespaces)
    data[516..520].copy_from_slice(&max_namespaces.to_le_bytes());

    // ONCS (Optional NVM Command Support) — Dataset Management
    data[520..522].copy_from_slice(&0x0004u16.to_le_bytes());

    // VWC: a volatile write cache is present (#366). With VWC clear a host
    // takes every completed write as durable and never sends a Flush. The I/O
    // controller only.
    if !discovery {
        data[525] = 0x01;
    }

    // SGLS: SGLs supported (bit 0) + SGL data block in capsule (bit 20).
    // Mandatory nonzero for fabrics — the Linux initiator refuses the
    // controller with "Mandatory sgls are not supported!" otherwise.
    data[536..540].copy_from_slice(&((1u32 << 0) | (1u32 << 20)).to_le_bytes());

    // SUBNQN (bytes 768-1023, 256 bytes)
    let nqn = subnqn.as_bytes();
    let nqn_len = nqn.len().min(256);
    data[768..768 + nqn_len].copy_from_slice(&nqn[..nqn_len]);

    // NVMe-oF specific (bytes 1792+), all mandatory for fabrics:
    // IOCCSZ: I/O command capsule size in 16-byte units — 64B SQE + 8 KB
    // in-capsule data. Writes larger than 8 KB use the R2T/H2CData flow.
    data[1792..1796].copy_from_slice(&(((64 + 8192) / 16) as u32).to_le_bytes());
    // IORCSZ: I/O response capsule size (one 16-byte CQE).
    data[1796..1800].copy_from_slice(&1u32.to_le_bytes());
    // MSDBD: one SGL data block descriptor per command.
    data[1803] = 1;

    data
}

/// Identify Namespace data (4096 bytes): `blocks` of `block_size` bytes,
/// `nguid` the namespace's identity, `anagrpid` its ANA group (0: none).
pub fn namespace(blocks: u64, block_size: u32, nguid: &[u8; 16], anagrpid: u32) -> Vec<u8> {
    let mut data = vec![0u8; 4096];

    // NSZE, NCAP, NUSE (in LBAs)
    data[0..8].copy_from_slice(&blocks.to_le_bytes());
    data[8..16].copy_from_slice(&blocks.to_le_bytes());
    data[16..24].copy_from_slice(&blocks.to_le_bytes());

    // NSFEAT: Deallocate (TRIM) supported
    data[24] = 0x04;
    // NLBAF = 0 (one format), FLBAS = format 0, DPS none.

    // NMIC bit 0: the namespace may be reached through several controllers
    // (Linux refuses a second path to an "unshared" namespace).
    data[30] = 0x01;
    data[92..96].copy_from_slice(&anagrpid.to_le_bytes());

    // NGUID (16 bytes at offset 104)
    data[104..120].copy_from_slice(nguid);

    // LBA Format 0 (offset 128): LBADS = log2(block_size), RP=0
    data[130] = block_size.trailing_zeros() as u8;

    data
}

/// The Changed Namespace List log page (4096 bytes, up to 1024 NSIDs).
pub fn changed_ns_list(nsids: &[u32], overflow: bool) -> Vec<u8> {
    let mut data = vec![0u8; 4096];
    if overflow || nsids.len() > 1024 {
        data[0..4].copy_from_slice(&NS_LIST_OVERFLOW.to_le_bytes());
        return data;
    }
    for (i, &nsid) in nsids.iter().enumerate() {
        data[i * 4..(i + 1) * 4].copy_from_slice(&nsid.to_le_bytes());
    }
    data
}

/// Active Namespace ID list (4096 bytes, up to 1024 NSIDs).
pub fn active_ns_list(nsids: &[u32]) -> Vec<u8> {
    let mut data = vec![0u8; 4096];
    for (i, &nsid) in nsids.iter().enumerate().take(1024) {
        data[i * 4..(i + 1) * 4].copy_from_slice(&nsid.to_le_bytes());
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_namespace_reports_its_size_and_block() {
        let d = namespace(12582912, 4096, &[7; 16], 0);
        assert_eq!(u64::from_le_bytes(d[0..8].try_into().unwrap()), 12582912);
        assert_eq!(d[130], 12);
        assert_eq!(namespace(8, 512, &[0; 16], 0)[130], 9);
        assert_eq!(&d[104..120], &[7; 16]);
    }

    #[test]
    fn mdts_is_pages_log2() {
        assert_eq!(mdts_for(8 << 20), 11);
        assert_eq!(mdts_for(128 << 10), 5);
        assert_eq!(mdts_for(0), 0);
    }
}
