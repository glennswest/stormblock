//! NVMe-oF discovery subsystem — discovery log page with target subsystems.

use std::net::SocketAddr;

/// Discovery log page header (16 bytes) + entries (1024 bytes each).
pub fn build_discovery_log_page(subsystems: &[DiscoveryEntry]) -> Vec<u8> {
    let num_entries = subsystems.len() as u64;
    let entry_size = 1024usize;
    let total_size = 16 + num_entries as usize * entry_size;
    let mut data = vec![0u8; total_size];

    // Header: Generation Counter (8 bytes) + Number of Records (8 bytes)
    data[0..8].copy_from_slice(&0u64.to_le_bytes()); // genctr
    data[8..16].copy_from_slice(&num_entries.to_le_bytes()); // numrec

    for (i, entry) in subsystems.iter().enumerate() {
        let offset = 16 + i * entry_size;
        write_discovery_entry(&mut data[offset..offset + entry_size], entry);
    }

    data
}

/// A single discovery log page entry.
pub struct DiscoveryEntry {
    pub subnqn: String,
    pub traddr: SocketAddr,
    pub portid: u16,
    pub cntlid: u16,
    pub subsys_type: SubsysType,
}

#[derive(Debug, Clone, Copy)]
pub enum SubsysType {
    NvmeSubsystem = 2,    // NVM subsystem
    DiscoverySubsystem = 1, // Discovery subsystem
}

fn write_discovery_entry(buf: &mut [u8], entry: &DiscoveryEntry) {
    // TRTYPE (byte 0): TCP = 0x03
    buf[0] = 0x03;
    // ADRFAM (byte 1): IPv4 = 0x01, IPv6 = 0x02
    buf[1] = if entry.traddr.is_ipv4() { 0x01 } else { 0x02 };
    // SUBTYPE (byte 2)
    buf[2] = entry.subsys_type as u8;
    // TREQ (byte 3): not required (0)
    buf[3] = 0;
    // PORTID (bytes 4-5)
    buf[4..6].copy_from_slice(&entry.portid.to_le_bytes());
    // CNTLID (bytes 6-7)
    buf[6..8].copy_from_slice(&entry.cntlid.to_le_bytes());
    // ASQSZ (admin submission queue size, bytes 8-9)
    buf[8..10].copy_from_slice(&128u16.to_le_bytes());

    // TRSVCID (bytes 32-63, 32 bytes, ASCII port number)
    let port_str = entry.traddr.port().to_string();
    let port_bytes = port_str.as_bytes();
    let port_len = port_bytes.len().min(32);
    buf[32..32 + port_len].copy_from_slice(&port_bytes[..port_len]);

    // TRADDR (bytes 256-511, 256 bytes, ASCII IP address)
    let addr_str = entry.traddr.ip().to_string();
    let addr_bytes = addr_str.as_bytes();
    let addr_len = addr_bytes.len().min(256);
    buf[256..256 + addr_len].copy_from_slice(&addr_bytes[..addr_len]);

    // SUBNQN (bytes 512-767, 256 bytes)
    let nqn = entry.subnqn.as_bytes();
    let nqn_len = nqn.len().min(256);
    buf[512..512 + nqn_len].copy_from_slice(&nqn[..nqn_len]);
}

/// Well-known discovery NQN.
pub const DISCOVERY_NQN: &str = "nqn.2014-08.org.nvmexpress.discovery";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_log_page() {
        let entries = vec![
            DiscoveryEntry {
                subnqn: "nqn.2024.io.stormblock:vol1".into(),
                traddr: "192.168.1.100:4420".parse().unwrap(),
                portid: 1,
                cntlid: 0xFFFF,
                subsys_type: SubsysType::NvmeSubsystem,
            },
        ];

        let log = build_discovery_log_page(&entries);
        assert_eq!(log.len(), 16 + 1024);

        let numrec = u64::from_le_bytes(log[8..16].try_into().unwrap());
        assert_eq!(numrec, 1);

        // Check TRTYPE
        assert_eq!(log[16], 0x03); // TCP
        // Check ADRFAM
        assert_eq!(log[17], 0x01); // IPv4

        // Check SUBNQN
        let nqn = &log[16 + 512..16 + 512 + 27];
        assert_eq!(std::str::from_utf8(nqn).unwrap(), "nqn.2024.io.stormblock:vol1");
    }

    #[test]
    fn empty_discovery_log() {
        let log = build_discovery_log_page(&[]);
        assert_eq!(log.len(), 16); // header only
        let numrec = u64::from_le_bytes(log[8..16].try_into().unwrap());
        assert_eq!(numrec, 0);
    }
}
