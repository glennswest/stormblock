//! NVMe admin commands — Identify Controller, Identify Namespace, Get Log Page.
//!
//! These commands are handled on the admin queue (QID 0) after fabric connect.

use std::sync::Arc;

use crate::drive::BlockDevice;

// Every field both targets report the same way is the portable crate's
// (#474); what is the engine's alone (ANA, #83) is added here.
pub use stormblock_nvmeof::identify::{
    active_ns_list, changed_ns_list, ADMIN_ABORT, ADMIN_ASYNC_EVENT_REQ, ADMIN_GET_FEATURES,
    ADMIN_GET_LOG_PAGE, ADMIN_IDENTIFY, ADMIN_KEEP_ALIVE, ADMIN_SET_FEATURES, CNS_ACTIVE_NS_LIST,
    CNS_CONTROLLER, CNS_NAMESPACE, CNS_NS_DESC_LIST, FEAT_NUM_QUEUES, NSATTR_OFFSET,
    NSATTR_WRITE_PROTECTED, NS_LIST_OVERFLOW,
};

/// Namespace Identification Descriptor list (CNS 0x03): one UUID descriptor
/// then a zero terminator.
pub fn identify_ns_desc_list(uuid: &[u8; 16]) -> Vec<u8> {
    stormblock_nvmeof::identify::ns_desc_list(uuid)
}

/// Build Identify Controller data (4096 bytes).
///
/// `discovery` selects CNTRLTYPE (the kernel refuses `nvme discover` against
/// a controller that doesn't identify as a discovery controller). MDTS is the
/// default transfer; the target overwrites it with its own (#407).
pub fn identify_controller(
    subnqn: &str,
    serial: &str,
    model: &str,
    firmware: &str,
    max_namespaces: u32,
    discovery: bool,
) -> Vec<u8> {
    let mut data = stormblock_nvmeof::identify::controller(
        subnqn,
        serial,
        model,
        firmware,
        max_namespaces,
        discovery,
        super::mdts_for(super::DEFAULT_MAX_TRANSFER),
    );
    if !discovery {
        // Asymmetric Namespace Access (#83). CMIC: the subsystem may have
        // several ports and controllers (bits 0, 1) and reports ANA (bit 3)
        // — what makes Linux put a namespace served by two nodes under one
        // multipath head. OAES bit 11: ANA change notices.
        data[76] = 0b1011;
        let oaes = u32::from_le_bytes(data[92..96].try_into().unwrap()) | (1 << 11);
        data[92..96].copy_from_slice(&oaes.to_le_bytes());
        data[343] = super::ana::ANATT_SECS;
        data[344] = super::ana::ANACAP;
        data[345..349].copy_from_slice(&super::ana::GROUPS.to_le_bytes()); // ANAGRPMAX
        data[349..353].copy_from_slice(&super::ana::GROUPS.to_le_bytes()); // NANAGRPID
        // MNAN: Linux sizes its ANA log buffer from it and refuses a
        // controller whose MNAN is zero or above NN.
        data[540..544].copy_from_slice(&max_namespaces.to_le_bytes());
    }
    data
}

/// Build Identify Namespace data (4096 bytes): the volume's size and block,
/// its id as the NGUID, and its ANA group on this node (#83).
pub fn identify_namespace(device: &Arc<dyn BlockDevice>) -> Vec<u8> {
    let bs = device.block_size();
    let uuid = device.id().uuid;
    stormblock_nvmeof::identify::namespace(
        device.capacity_bytes() / bs as u64,
        bs,
        uuid.as_bytes(),
        super::ana::state_of(uuid).group(),
    )
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
        assert_eq!(data[525] & 1, 1, "VWC: hosts flush and send FUA (#366)");

        // ANA (#83), as Linux's nvme_mpath_init_identify reads it.
        assert_eq!(data[76] & 0b1010, 0b1010, "CMIC: multi-controller + ANA");
        let oaes = u32::from_le_bytes(data[92..96].try_into().unwrap());
        assert_ne!(oaes & (1 << 11), 0, "ANA change notices");
        assert_ne!(oaes & (1 << 8), 0, "namespace notices kept");
        assert_eq!(data[344], 0x1F, "ANACAP");
        assert_eq!(u32::from_le_bytes(data[345..349].try_into().unwrap()), 5, "ANAGRPMAX");
        assert_eq!(u32::from_le_bytes(data[349..353].try_into().unwrap()), 5, "NANAGRPID");
        let mnan = u32::from_le_bytes(data[540..544].try_into().unwrap());
        assert!(mnan > 0 && mnan <= 16, "MNAN nonzero and within NN");

        let disc = identify_controller("nqn.2014-08.org.nvmexpress.discovery", "SN", "M", "1", 0, true);
        assert_eq!(disc[76], 0, "a discovery controller reports no ANA");

        // Serial
        let sn = std::str::from_utf8(&data[4..24]).unwrap().trim();
        assert_eq!(sn, "SN123456");

        // Model
        let mn = std::str::from_utf8(&data[24..64]).unwrap().trim();
        assert_eq!(mn, "StormBlock Virtual");

        // MDTS: 8 MiB = 2^11 pages (#407)
        assert_eq!(data[77], 11);

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

        assert_eq!(data[30] & 1, 1, "NMIC: shared");
        assert_eq!(u32::from_le_bytes(data[92..96].try_into().unwrap()), 1, "ANAGRPID: optimized");
        crate::target::nvmeof::ana::set(dev.id().uuid, crate::target::nvmeof::ana::AnaState::Inaccessible);
        let data = identify_namespace(&dev);
        assert_eq!(u32::from_le_bytes(data[92..96].try_into().unwrap()), 3, "ANAGRPID: inaccessible");

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
