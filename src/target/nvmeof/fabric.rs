//! NVMe-oF fabric commands — Connect, Property Get/Set.
//!
//! Fabric commands use NVMe opcode 0x7F with sub-commands in the SQE.

use super::pdu::NvmeSqe;

/// Fabric command types (fctype in cdw10 byte 0).
pub const FCTYPE_PROPERTY_SET: u8 = 0x00;
pub const FCTYPE_CONNECT: u8 = 0x01;
pub const FCTYPE_PROPERTY_GET: u8 = 0x04;

/// NVMe opcode for fabric commands.
pub const NVME_FABRIC_OPC: u8 = 0x7F;

/// 1024-byte Connect command data.
#[derive(Debug)]
pub struct ConnectData {
    pub hostid: [u8; 16],
    pub cntlid: u16,
    pub subnqn: String,
    pub hostnqn: String,
}

impl ConnectData {
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 1024 {
            return None;
        }
        let mut hostid = [0u8; 16];
        hostid.copy_from_slice(&data[0..16]);
        let cntlid = u16::from_le_bytes([data[16], data[17]]);

        let subnqn = extract_nqn(&data[256..512]);
        let hostnqn = extract_nqn(&data[512..768]);

        Some(ConnectData { hostid, cntlid, subnqn, hostnqn })
    }
}

/// NVMe property identifiers for Property Get/Set.
#[derive(Debug, Clone, Copy)]
pub enum NvmeProperty {
    Cap,  // Controller Capabilities (offset 0x00, 8 bytes)
    Vs,   // Version (offset 0x08, 4 bytes)
    Cc,   // Controller Configuration (offset 0x14, 4 bytes)
    Csts, // Controller Status (offset 0x1C, 4 bytes)
}

impl NvmeProperty {
    pub fn from_offset(offset: u32) -> Option<Self> {
        match offset {
            0x00 => Some(NvmeProperty::Cap),
            0x08 => Some(NvmeProperty::Vs),
            0x14 => Some(NvmeProperty::Cc),
            0x1C => Some(NvmeProperty::Csts),
            _ => None,
        }
    }
}

/// Parse fabric command from SQE.
pub struct FabricCmd {
    pub fctype: u8,
    pub sqe: NvmeSqe,
}

impl FabricCmd {
    pub fn from_sqe(sqe: &NvmeSqe) -> Option<Self> {
        if sqe.opcode() != NVME_FABRIC_OPC {
            return None;
        }
        let fctype = (sqe.cdw10() & 0xFF) as u8;
        Some(FabricCmd { fctype, sqe: sqe.clone() })
    }

    /// For Property Get/Set: extract the property offset from cdw11.
    pub fn property_offset(&self) -> u32 {
        self.sqe.cdw11()
    }

    /// For Property Get: whether it's a 64-bit (attrib=1) or 32-bit (attrib=0) read.
    pub fn property_size_64(&self) -> bool {
        (self.sqe.cdw10() >> 8) & 0x01 != 0
    }

    /// For Connect: extract SQSIZE from cdw10 bits 31:16 and QID from cdw11 bits 15:0.
    pub fn connect_sqsize(&self) -> u16 {
        ((self.sqe.cdw10() >> 16) & 0xFFFF) as u16
    }

    pub fn connect_qid(&self) -> u16 {
        (self.sqe.cdw11() & 0xFFFF) as u16
    }
}

/// Controller property values for a StormBlock target.
#[derive(Default)]
pub struct ControllerProperties {
    /// CC register value (set by host).
    pub cc: u32,
}

impl ControllerProperties {
    pub fn new() -> Self {
        Self::default()
    }

    /// CAP register: MQES=1023, CQR=1, TO=40 (2s), MPSMIN=0(4K), MPSMAX=0, CSS=NVMe
    pub fn cap(&self) -> u64 {
        let mqes: u64 = 1023;           // max queue entries - 1
        let cqr: u64 = 1 << 16;        // contiguous queues required
        let to: u64 = 40 << 24;        // timeout in 500ms units
        let css: u64 = 1 << 37;        // NVMe command set supported
        mqes | cqr | to | css
    }

    /// VS register: NVMe 1.4 = 0x00010400
    pub fn vs(&self) -> u32 {
        0x00010400
    }

    /// CSTS register: RDY if CC.EN=1
    pub fn csts(&self) -> u32 {
        if self.cc & 0x01 != 0 { 1 } else { 0 } // RDY bit
    }

    pub fn get_property(&self, prop: NvmeProperty) -> u64 {
        match prop {
            NvmeProperty::Cap => self.cap(),
            NvmeProperty::Vs => self.vs() as u64,
            NvmeProperty::Cc => self.cc as u64,
            NvmeProperty::Csts => self.csts() as u64,
        }
    }

    pub fn set_property(&mut self, prop: NvmeProperty, val: u64) {
        match prop {
            NvmeProperty::Cc => { self.cc = val as u32; }
            _ => { tracing::warn!("attempt to set read-only property {:?}", prop); }
        }
    }
}

/// Extract a null-terminated NQN string from a buffer.
fn extract_nqn(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_data_parse() {
        let mut data = vec![0u8; 1024];
        // hostid
        data[0..16].copy_from_slice(&[1u8; 16]);
        // cntlid = 0xFFFF (dynamic)
        data[16] = 0xFF;
        data[17] = 0xFF;
        // subnqn at offset 256
        let nqn = b"nqn.2024.io.stormblock:test";
        data[256..256 + nqn.len()].copy_from_slice(nqn);
        // hostnqn at offset 512
        let hnqn = b"nqn.2014-08.org.nvmexpress:uuid:test-host";
        data[512..512 + hnqn.len()].copy_from_slice(hnqn);

        let cd = ConnectData::from_bytes(&data).unwrap();
        assert_eq!(cd.hostid, [1u8; 16]);
        assert_eq!(cd.cntlid, 0xFFFF);
        assert_eq!(cd.subnqn, "nqn.2024.io.stormblock:test");
        assert!(cd.hostnqn.starts_with("nqn.2014-08"));
    }

    #[test]
    fn controller_properties() {
        let mut props = ControllerProperties::new();

        // Initially not ready
        assert_eq!(props.csts(), 0);

        // Set CC.EN = 1
        props.set_property(NvmeProperty::Cc, 1);
        assert_eq!(props.csts(), 1); // RDY

        // Version
        assert_eq!(props.vs(), 0x00010400);

        // CAP has MQES, CQR, etc
        let cap = props.cap();
        assert_eq!(cap & 0xFFFF, 1023); // MQES
    }
}
