//! NVMe admin commands — Identify Controller, Identify Namespace, Get Log Page.
//!
//! These commands are handled on the admin queue (QID 0) after fabric connect.

use std::sync::Arc;

use crate::drive::BlockDevice;

/// NVMe admin opcodes.
pub const ADMIN_IDENTIFY: u8 = 0x06;
pub const ADMIN_GET_LOG_PAGE: u8 = 0x02;
pub const ADMIN_ABORT: u8 = 0x08;
pub const ADMIN_SET_FEATURES: u8 = 0x09;
pub const ADMIN_GET_FEATURES: u8 = 0x0A;
pub const ADMIN_ASYNC_EVENT_REQ: u8 = 0x0C;
pub const ADMIN_KEEP_ALIVE: u8 = 0x18;

/// Identify CNS values.
pub const CNS_NAMESPACE: u8 = 0x00;
pub const CNS_CONTROLLER: u8 = 0x01;
pub const CNS_ACTIVE_NS_LIST: u8 = 0x02;

/// Build Identify Controller data (4096 bytes).
///
/// `discovery` selects CNTRLTYPE (the kernel refuses `nvme discover` against
/// a controller that doesn't identify as a discovery controller).
pub fn identify_controller(
    subnqn: &str,
    serial: &str,
    model: &str,
    firmware: &str,
    max_namespaces: u32,
    discovery: bool,
) -> Vec<u8> {
    let mut data = vec![0u8; 4096];

    // VID (vendor ID) - PCI, 0 for fabric
    data[0..2].copy_from_slice(&0u16.to_le_bytes());
    // SSVID
    data[2..4].copy_from_slice(&0u16.to_le_bytes());

    // Serial Number (bytes 4-23, 20 bytes, ASCII, space-padded)
    let sn = serial.as_bytes();
    let sn_len = sn.len().min(20);
    data[4..4 + sn_len].copy_from_slice(&sn[..sn_len]);
    for b in &mut data[4 + sn_len..24] { *b = b' '; }

    // Model Number (bytes 24-63, 40 bytes, ASCII, space-padded)
    let mn = model.as_bytes();
    let mn_len = mn.len().min(40);
    data[24..24 + mn_len].copy_from_slice(&mn[..mn_len]);
    for b in &mut data[24 + mn_len..64] { *b = b' '; }

    // Firmware Revision (bytes 64-71, 8 bytes)
    let fw = firmware.as_bytes();
    let fw_len = fw.len().min(8);
    data[64..64 + fw_len].copy_from_slice(&fw[..fw_len]);
    for b in &mut data[64 + fw_len..72] { *b = b' '; }

    // MDTS (Maximum Data Transfer Size) — log2(pages), 5 = 32 pages = 128KB
    data[77] = 5;

    // CNTLID (assigned during connect, filled by caller)
    // data[78..80] set externally

    // VER = NVMe 1.4
    data[80..84].copy_from_slice(&0x00010400u32.to_le_bytes());

    // CNTRLTYPE (byte 111): 1 = I/O controller, 2 = discovery controller
    data[111] = if discovery { 2 } else { 1 };

    // OACS (Optional Admin Command Support) — none for now
    data[256..258].copy_from_slice(&0u16.to_le_bytes());

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

    // SQES: min=6(64B), max=6(64B)
    data[512] = 0x66;
    // CQES: min=4(16B), max=4(16B)
    data[513] = 0x44;

    // MAXCMD
    data[514..516].copy_from_slice(&256u16.to_le_bytes());

    // NN (Number of Namespaces)
    data[516..520].copy_from_slice(&max_namespaces.to_le_bytes());

    // ONCS (Optional NVM Command Support) — Write Zeroes, Dataset Management
    data[520..522].copy_from_slice(&0x0004u16.to_le_bytes()); // Dataset Management

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
    // ICDOFF = 0, FCATT = 0 (dynamic controller model)
    // MSDBD: one SGL data block descriptor per command.
    data[1803] = 1;

    data
}

/// Build Identify Namespace data (4096 bytes).
pub fn identify_namespace(device: &Arc<dyn BlockDevice>) -> Vec<u8> {
    let mut data = vec![0u8; 4096];

    let bs = device.block_size();
    let total_blocks = device.capacity_bytes() / bs as u64;

    // NSZE (Namespace Size in LBAs)
    data[0..8].copy_from_slice(&total_blocks.to_le_bytes());
    // NCAP (Namespace Capacity)
    data[8..16].copy_from_slice(&total_blocks.to_le_bytes());
    // NUSE (Namespace Utilization)
    data[16..24].copy_from_slice(&total_blocks.to_le_bytes());

    // NSFEAT (Namespace Features)
    data[24] = 0x04; // Deallocate (TRIM) supported

    // NLBAF (Number of LBA Formats) — 0 = 1 format
    data[25] = 0;

    // FLBAS (Formatted LBA Size) — format 0, no metadata
    data[26] = 0;

    // DPS (Data Protection) — none
    data[29] = 0;

    // NGUID (16 bytes at offset 104)
    let uuid = device.id().uuid;
    data[104..120].copy_from_slice(uuid.as_bytes());

    // LBA Format 0 (offset 128): LBADS = log2(block_size), RP=0 (best perf)
    let lbads = (bs as f64).log2() as u8; // 512->9, 4096->12
    data[128..132].copy_from_slice(&0u32.to_le_bytes());
    data[130] = lbads;

    data
}

/// Build Active Namespace ID list (4096 bytes, up to 1024 NSIDs).
pub fn active_ns_list(nsids: &[u32]) -> Vec<u8> {
    let mut data = vec![0u8; 4096];
    for (i, &nsid) in nsids.iter().enumerate() {
        if i >= 1024 { break; }
        data[i * 4..(i + 1) * 4].copy_from_slice(&nsid.to_le_bytes());
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identify_controller_fields() {
        let data = identify_controller(
            "nqn.2024.io.stormblock:test",
            "SN123456",
            "StormBlock Virtual",
            "1.0.0",
            16,
            false,
        );
        assert_eq!(data.len(), 4096);

        // Fabrics-mandatory fields the Linux initiator validates
        assert_eq!(u16::from_le_bytes([data[320], data[321]]), 10, "KAS");
        assert_eq!(
            u32::from_le_bytes(data[1792..1796].try_into().unwrap()),
            (64 + 8192) / 16,
            "IOCCSZ"
        );
        assert_eq!(u32::from_le_bytes(data[1796..1800].try_into().unwrap()), 1, "IORCSZ");
        let sgls = u32::from_le_bytes(data[536..540].try_into().unwrap());
        assert_ne!(sgls & 0x3, 0, "SGLS mandatory for fabrics");
        assert_eq!(data[111], 1, "CNTRLTYPE = I/O controller");

        // Serial
        let sn = std::str::from_utf8(&data[4..24]).unwrap().trim();
        assert_eq!(sn, "SN123456");

        // Model
        let mn = std::str::from_utf8(&data[24..64]).unwrap().trim();
        assert_eq!(mn, "StormBlock Virtual");

        // MDTS
        assert_eq!(data[77], 5);

        // NN (number of namespaces)
        let nn = u32::from_le_bytes(data[516..520].try_into().unwrap());
        assert_eq!(nn, 16);

        // SUBNQN
        let nqn = &data[768..768 + 27];
        assert_eq!(std::str::from_utf8(nqn).unwrap(), "nqn.2024.io.stormblock:test");
    }

    #[tokio::test]
    async fn identify_namespace_fields() {
        let dir = std::env::temp_dir().join("stormblock-admin-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.bin", uuid::Uuid::new_v4().simple()));
        let dev = crate::drive::filedev::FileDevice::open_with_capacity(
            path.to_str().unwrap(), 1024 * 1024
        ).await.unwrap();
        let dev: Arc<dyn BlockDevice> = Arc::new(dev);

        let data = identify_namespace(&dev);
        assert_eq!(data.len(), 4096);

        let nsze = u64::from_le_bytes(data[0..8].try_into().unwrap());
        assert_eq!(nsze, 1024 * 1024 / 4096); // 256 blocks at 4K

        let lbads = data[130];
        assert_eq!(lbads, 12); // log2(4096) = 12

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn active_ns_list_encoding() {
        let data = active_ns_list(&[1, 2, 3]);
        let ns1 = u32::from_le_bytes(data[0..4].try_into().unwrap());
        let ns2 = u32::from_le_bytes(data[4..8].try_into().unwrap());
        let ns3 = u32::from_le_bytes(data[8..12].try_into().unwrap());
        let ns4 = u32::from_le_bytes(data[12..16].try_into().unwrap());
        assert_eq!(ns1, 1);
        assert_eq!(ns2, 2);
        assert_eq!(ns3, 3);
        assert_eq!(ns4, 0); // sentinel
    }
}
