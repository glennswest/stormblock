//! Minimal iSCSI initiator for integration tests.
//!
//! Reuses PDU types from `stormblock::target::iscsi::pdu`.

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, Ordering};

use tokio::io::{BufReader, BufWriter};
use tokio::net::TcpStream;

use stormblock::target::iscsi::pdu::{
    Bhs, IscsiPdu, Opcode, encode_text_params, parse_text_params, read_pdu, write_pdu,
};

// SCSI CDB operation codes
const INQUIRY: u8 = 0x12;
const READ_CAPACITY_10: u8 = 0x25;
const READ_10: u8 = 0x28;
const WRITE_10: u8 = 0x2A;

// Login stages
const STAGE_SECURITY: u8 = 0;
const STAGE_OPERATIONAL: u8 = 1;
const STAGE_FULL_FEATURE: u8 = 3;

/// Atomic counter for unique ISID qualifier per connection.
/// Prevents session collisions when multiple tests run in parallel.
static ISID_COUNTER: AtomicU16 = AtomicU16::new(1);

pub struct IscsiInitiator {
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: BufWriter<tokio::net::tcp::OwnedWriteHalf>,
    cmd_sn: u32,
    exp_stat_sn: u32,
    tsih: u16,
    itt: u32,
    max_recv_data_seg: u32,
    block_size: u32,
    isid: [u8; 6],
}

impl IscsiInitiator {
    pub async fn connect(addr: SocketAddr) -> io::Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        let (reader, writer) = stream.into_split();
        // Each connection gets a unique ISID qualifier to avoid session collisions
        let qualifier = ISID_COUNTER.fetch_add(1, Ordering::Relaxed);
        let isid = [0x40, 0x00, 0x00, 0x01, (qualifier >> 8) as u8, qualifier as u8];
        Ok(IscsiInitiator {
            reader: BufReader::new(reader),
            writer: BufWriter::new(writer),
            cmd_sn: 1,
            exp_stat_sn: 0,
            tsih: 0,
            itt: 1,
            max_recv_data_seg: 8192,
            block_size: 4096, // default, updated by read_capacity
            isid,
        })
    }

    fn next_itt(&mut self) -> u32 {
        let itt = self.itt;
        self.itt += 1;
        itt
    }

    fn make_login_bhs(&self, itt: u32, csg: u8, nsg: u8) -> Bhs {
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::LoginRequest);
        bhs.set_immediate(true);
        bhs.set_csg(csg);
        bhs.set_nsg(nsg);
        bhs.set_transit(true);
        bhs.set_initiator_task_tag(itt);
        bhs.set_cmd_sn(self.cmd_sn);
        // ExpStatSN from target's last StatSN
        bhs.set_stat_sn(self.exp_stat_sn);
        bhs.set_isid(&self.isid);
        bhs.set_tsih(self.tsih);
        bhs
    }

    fn operational_params() -> Vec<(&'static str, &'static str)> {
        vec![
            ("HeaderDigest", "None"),
            ("DataDigest", "None"),
            ("MaxRecvDataSegmentLength", "65536"),
            ("MaxBurstLength", "262144"),
            ("FirstBurstLength", "65536"),
            ("DefaultTime2Wait", "2"),
            ("DefaultTime2Retain", "0"),
            ("MaxOutstandingR2T", "1"),
            ("MaxConnections", "1"),
            ("ImmediateData", "Yes"),
            ("InitialR2T", "No"),
            ("ErrorRecoveryLevel", "0"),
        ]
    }

    fn parse_login_response(&mut self, resp: &IscsiPdu) -> io::Result<()> {
        // Update ExpStatSN from response's StatSN (bytes 24-27)
        self.exp_stat_sn = resp.bhs.cmd_sn(); // same byte offset, StatSN in response
        // Update TSIH from response (target assigns session handle)
        let tsih = resp.bhs.tsih();
        if tsih != 0 {
            self.tsih = tsih;
        }
        eprintln!("  StatSN={} TSIH={}", self.exp_stat_sn, self.tsih);

        // Parse negotiated params
        let resp_params = parse_text_params(&resp.data);
        for (key, val) in &resp_params {
            eprintln!("  negotiated: {}={}", key, val);
            if key == "MaxRecvDataSegmentLength" {
                if let Ok(v) = val.parse::<u32>() {
                    self.max_recv_data_seg = v;
                }
            }
        }
        Ok(())
    }

    /// Perform iSCSI login (no CHAP).
    ///
    /// Two-phase: Security → Operational → FullFeature.
    /// Security params in Phase 1, operational params in Phase 2.
    pub async fn login(&mut self, initiator_name: &str, target_name: &str) -> io::Result<()> {
        // RFC 7143: all login PDUs in a login phase share the same ITT
        let login_itt = self.next_itt();

        // Phase 1: Security negotiation only
        let security_params = encode_text_params(&[
            ("InitiatorName", initiator_name),
            ("TargetName", target_name),
            ("SessionType", "Normal"),
            ("AuthMethod", "None"),
        ]);

        eprintln!("  login phase 1: Security→Operational (ITT={} {} bytes)", login_itt, security_params.len());

        let bhs = self.make_login_bhs(login_itt, STAGE_SECURITY, STAGE_OPERATIONAL);
        let pdu = IscsiPdu::with_data(bhs, security_params);
        write_pdu(&mut self.writer, &pdu, false, false).await?;

        let resp = read_pdu(&mut self.reader, false, false).await?;
        if resp.bhs.opcode() != Some(Opcode::LoginResponse) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected LoginResponse, got {:?}", resp.bhs.opcode()),
            ));
        }

        let status_class = resp.bhs.raw[36];
        let status_detail = resp.bhs.raw[37];

        if status_class != 0 {
            let resp_params = parse_text_params(&resp.data);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "security login failed: class={} detail={:#x} params={:?}",
                    status_class, status_detail, resp_params
                ),
            ));
        }

        self.parse_login_response(&resp)?;

        // If target went straight to FullFeature (some targets skip operational)
        if resp.bhs.transit() && resp.bhs.nsg() == STAGE_FULL_FEATURE {
            eprintln!("  login: Security→FullFeature (skipped operational)");
            self.cmd_sn += 1;
            return Ok(());
        }

        // Phase 2: Operational negotiation
        eprintln!(
            "  login phase 2: Operational→FullFeature (TSIH={} ExpStatSN={} CmdSN={})",
            self.tsih, self.exp_stat_sn, self.cmd_sn
        );

        let op_data = encode_text_params(&Self::operational_params());
        let bhs = self.make_login_bhs(login_itt, STAGE_OPERATIONAL, STAGE_FULL_FEATURE);
        let pdu = IscsiPdu::with_data(bhs, op_data);
        write_pdu(&mut self.writer, &pdu, false, false).await?;

        let resp = read_pdu(&mut self.reader, false, false).await?;
        if resp.bhs.opcode() != Some(Opcode::LoginResponse) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected LoginResponse phase 2, got {:?}", resp.bhs.opcode()),
            ));
        }

        let status_class = resp.bhs.raw[36];
        let status_detail = resp.bhs.raw[37];
        if status_class != 0 {
            let resp_params = parse_text_params(&resp.data);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "operational login failed: class={} detail={:#x} params={:?}",
                    status_class, status_detail, resp_params
                ),
            ));
        }

        self.parse_login_response(&resp)?;
        eprintln!("  login: two-phase OK → FullFeature");
        self.cmd_sn += 1;
        Ok(())
    }

    /// Send SCSI INQUIRY and return the inquiry data.
    pub async fn inquiry(&mut self) -> io::Result<Vec<u8>> {
        let itt = self.next_itt();
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::ScsiCommand);
        bhs.set_final(true);
        // Read flag (bit 6 of byte 1)
        bhs.raw[1] |= 0x40;
        bhs.set_initiator_task_tag(itt);
        bhs.set_cmd_sn(self.cmd_sn);
        bhs.set_stat_sn(self.exp_stat_sn);
        bhs.set_lun(0);
        bhs.set_expected_data_transfer_length(96);

        // INQUIRY CDB (6 bytes)
        let mut cdb = [0u8; 16];
        cdb[0] = INQUIRY;
        cdb[4] = 96; // allocation length
        bhs.set_cdb(&cdb);

        let pdu = IscsiPdu::new(bhs);
        write_pdu(&mut self.writer, &pdu, false, false).await?;
        self.cmd_sn += 1;

        // Read response — may be DataIn + ScsiResponse, or just ScsiResponse
        let mut data = Vec::new();
        loop {
            let resp = read_pdu(&mut self.reader, false, false).await?;
            match resp.bhs.opcode() {
                Some(Opcode::DataIn) => {
                    data.extend_from_slice(&resp.data);
                    if resp.bhs.has_status() {
                        break;
                    }
                }
                Some(Opcode::ScsiResponse) => {
                    break;
                }
                _ => {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected opcode in inquiry response"));
                }
            }
        }

        Ok(data)
    }

    /// Send READ CAPACITY(10) and return (total_blocks, block_size).
    /// Also stores the block_size for use by read()/write().
    pub async fn read_capacity(&mut self) -> io::Result<(u64, u32)> {
        let itt = self.next_itt();
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::ScsiCommand);
        bhs.set_final(true);
        bhs.raw[1] |= 0x40; // Read
        bhs.set_initiator_task_tag(itt);
        bhs.set_cmd_sn(self.cmd_sn);
        bhs.set_stat_sn(self.exp_stat_sn);
        bhs.set_lun(0);
        bhs.set_expected_data_transfer_length(8);

        let mut cdb = [0u8; 16];
        cdb[0] = READ_CAPACITY_10;
        bhs.set_cdb(&cdb);

        let pdu = IscsiPdu::new(bhs);
        write_pdu(&mut self.writer, &pdu, false, false).await?;
        self.cmd_sn += 1;

        let mut data = Vec::new();
        loop {
            let resp = read_pdu(&mut self.reader, false, false).await?;
            match resp.bhs.opcode() {
                Some(Opcode::DataIn) => {
                    data.extend_from_slice(&resp.data);
                    if resp.bhs.has_status() {
                        break;
                    }
                }
                Some(Opcode::ScsiResponse) => {
                    break;
                }
                _ => {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected opcode"));
                }
            }
        }

        if data.len() < 8 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "read capacity data too short"));
        }

        let last_lba = u32::from_be_bytes(data[0..4].try_into().unwrap()) as u64;
        let block_size = u32::from_be_bytes(data[4..8].try_into().unwrap());
        self.block_size = block_size;
        Ok((last_lba + 1, block_size))
    }

    /// Read `block_count` blocks starting at `lba`.
    /// Uses block_size from the last read_capacity() call.
    pub async fn read(&mut self, lba: u64, block_count: u16) -> io::Result<Vec<u8>> {
        let itt = self.next_itt();
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::ScsiCommand);
        bhs.set_final(true);
        bhs.raw[1] |= 0x40; // Read
        bhs.set_initiator_task_tag(itt);
        bhs.set_cmd_sn(self.cmd_sn);
        bhs.set_stat_sn(self.exp_stat_sn);
        bhs.set_lun(0);
        let transfer_len = block_count as u32 * self.block_size;
        bhs.set_expected_data_transfer_length(transfer_len);

        // READ(10) CDB
        let mut cdb = [0u8; 16];
        cdb[0] = READ_10;
        cdb[2..6].copy_from_slice(&(lba as u32).to_be_bytes());
        cdb[7..9].copy_from_slice(&block_count.to_be_bytes());
        bhs.set_cdb(&cdb);

        let pdu = IscsiPdu::new(bhs);
        write_pdu(&mut self.writer, &pdu, false, false).await?;
        self.cmd_sn += 1;

        let mut data = Vec::new();
        loop {
            let resp = read_pdu(&mut self.reader, false, false).await?;
            match resp.bhs.opcode() {
                Some(Opcode::DataIn) => {
                    data.extend_from_slice(&resp.data);
                    if resp.bhs.has_status() {
                        break;
                    }
                }
                Some(Opcode::ScsiResponse) => {
                    break;
                }
                _ => {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected opcode in read response"));
                }
            }
        }

        Ok(data)
    }

    /// Write data at the given LBA. Data length must be a multiple of block_size.
    /// Uses block_size from the last read_capacity() call.
    pub async fn write(&mut self, lba: u64, data: &[u8]) -> io::Result<()> {
        let block_count = (data.len() / self.block_size as usize) as u16;
        let itt = self.next_itt();
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::ScsiCommand);
        bhs.set_immediate(true);
        bhs.set_final(true);
        // Write flag (bit 5 of byte 1)
        bhs.raw[1] |= 0x20;
        bhs.set_initiator_task_tag(itt);
        bhs.set_cmd_sn(self.cmd_sn);
        bhs.set_stat_sn(self.exp_stat_sn);
        bhs.set_lun(0);
        bhs.set_expected_data_transfer_length(data.len() as u32);

        // WRITE(10) CDB
        let mut cdb = [0u8; 16];
        cdb[0] = WRITE_10;
        cdb[2..6].copy_from_slice(&(lba as u32).to_be_bytes());
        cdb[7..9].copy_from_slice(&block_count.to_be_bytes());
        bhs.set_cdb(&cdb);

        // Include immediate data in the PDU
        let pdu = IscsiPdu::with_data(bhs, data.to_vec());
        write_pdu(&mut self.writer, &pdu, false, false).await?;
        self.cmd_sn += 1;

        // Read SCSI response
        let resp = read_pdu(&mut self.reader, false, false).await?;
        match resp.bhs.opcode() {
            Some(Opcode::ScsiResponse) => {
                let status = resp.bhs.status();
                if status != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("SCSI write failed with status {status:#x}"),
                    ));
                }
                Ok(())
            }
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "expected ScsiResponse after write")),
        }
    }

    /// Send iSCSI Logout.
    pub async fn logout(&mut self) -> io::Result<()> {
        let itt = self.next_itt();
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::LogoutRequest);
        bhs.set_immediate(true);
        bhs.set_final(true);
        // Reason code 0 = close session (byte 1 bits 6:0)
        bhs.raw[1] = (bhs.raw[1] & 0x80) | 0x00;
        bhs.set_initiator_task_tag(itt);
        bhs.set_cmd_sn(self.cmd_sn);
        bhs.set_stat_sn(self.exp_stat_sn);

        let pdu = IscsiPdu::new(bhs);
        write_pdu(&mut self.writer, &pdu, false, false).await?;
        self.cmd_sn += 1;

        let resp = read_pdu(&mut self.reader, false, false).await?;
        if resp.bhs.opcode() != Some(Opcode::LogoutResponse) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "expected LogoutResponse"));
        }
        Ok(())
    }
}
