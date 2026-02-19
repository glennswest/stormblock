//! Minimal iSCSI initiator for integration tests.
//!
//! Reuses PDU types from `stormblock::target::iscsi::pdu`.

use std::io;
use std::net::SocketAddr;

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

pub struct IscsiInitiator {
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: BufWriter<tokio::net::tcp::OwnedWriteHalf>,
    cmd_sn: u32,
    exp_stat_sn: u32,
    itt: u32,
    max_recv_data_seg: u32,
}

impl IscsiInitiator {
    pub async fn connect(addr: SocketAddr) -> io::Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        let (reader, writer) = stream.into_split();
        Ok(IscsiInitiator {
            reader: BufReader::new(reader),
            writer: BufWriter::new(writer),
            cmd_sn: 1,
            exp_stat_sn: 0,
            itt: 1,
            max_recv_data_seg: 8192,
        })
    }

    fn next_itt(&mut self) -> u32 {
        let itt = self.itt;
        self.itt += 1;
        itt
    }

    /// Perform iSCSI login (no CHAP). Two-phase: Security → Operational → FullFeature.
    pub async fn login(&mut self, initiator_name: &str, target_name: &str) -> io::Result<()> {
        // Phase 1: Security negotiation
        let security_params = encode_text_params(&[
            ("InitiatorName", initiator_name),
            ("TargetName", target_name),
            ("SessionType", "Normal"),
            ("AuthMethod", "None"),
        ]);

        let itt = self.next_itt();
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::LoginRequest);
        bhs.set_immediate(true);
        bhs.set_csg(STAGE_SECURITY);
        bhs.set_nsg(STAGE_OPERATIONAL);
        bhs.set_transit(true);
        bhs.set_initiator_task_tag(itt);
        bhs.set_cmd_sn(self.cmd_sn);
        // Set ISID (6 bytes): type=random (0x80), random A + B
        bhs.raw[8] = 0x40; // type qualifier
        bhs.raw[9] = 0x00;
        bhs.raw[10] = 0x00;
        bhs.raw[11] = 0x01;
        bhs.raw[12] = 0x00;
        bhs.raw[13] = 0x01;

        let pdu = IscsiPdu::with_data(bhs, security_params);
        write_pdu(&mut self.writer, &pdu, false, false).await?;

        let resp = read_pdu(&mut self.reader, false, false).await?;
        if resp.bhs.opcode() != Some(Opcode::LoginResponse) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "expected LoginResponse"));
        }
        // Check status class (byte 36)
        if resp.bhs.raw[36] != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("login failed: class={}, detail={}", resp.bhs.raw[36], resp.bhs.raw[37]),
            ));
        }

        // Check if we went straight to full feature (transit+NSG=3)
        if resp.bhs.transit() && resp.bhs.nsg() == STAGE_FULL_FEATURE {
            self.cmd_sn += 1;
            return Ok(());
        }

        // Phase 2: Operational negotiation
        let op_params = encode_text_params(&[
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
        ]);

        let itt = self.next_itt();
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::LoginRequest);
        bhs.set_immediate(true);
        bhs.set_csg(STAGE_OPERATIONAL);
        bhs.set_nsg(STAGE_FULL_FEATURE);
        bhs.set_transit(true);
        bhs.set_initiator_task_tag(itt);
        self.cmd_sn += 1;
        bhs.set_cmd_sn(self.cmd_sn);
        // Copy ISID from first request
        bhs.raw[8] = 0x40;
        bhs.raw[9] = 0x00;
        bhs.raw[10] = 0x00;
        bhs.raw[11] = 0x01;
        bhs.raw[12] = 0x00;
        bhs.raw[13] = 0x01;

        let pdu = IscsiPdu::with_data(bhs, op_params);
        write_pdu(&mut self.writer, &pdu, false, false).await?;

        let resp = read_pdu(&mut self.reader, false, false).await?;
        if resp.bhs.opcode() != Some(Opcode::LoginResponse) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "expected LoginResponse"));
        }
        if resp.bhs.raw[36] != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("operational login failed: class={}", resp.bhs.raw[36]),
            ));
        }

        // Parse negotiated params
        let resp_params = parse_text_params(&resp.data);
        for (key, val) in &resp_params {
            if key == "MaxRecvDataSegmentLength" {
                if let Ok(v) = val.parse::<u32>() {
                    self.max_recv_data_seg = v;
                }
            }
        }

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
    pub async fn read_capacity(&mut self) -> io::Result<(u64, u32)> {
        let itt = self.next_itt();
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::ScsiCommand);
        bhs.set_final(true);
        bhs.raw[1] |= 0x40; // Read
        bhs.set_initiator_task_tag(itt);
        bhs.set_cmd_sn(self.cmd_sn);
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
        Ok((last_lba + 1, block_size))
    }

    /// Read `block_count` blocks starting at `lba` (4096-byte blocks).
    pub async fn read(&mut self, lba: u64, block_count: u16) -> io::Result<Vec<u8>> {
        let itt = self.next_itt();
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::ScsiCommand);
        bhs.set_final(true);
        bhs.raw[1] |= 0x40; // Read
        bhs.set_initiator_task_tag(itt);
        bhs.set_cmd_sn(self.cmd_sn);
        bhs.set_lun(0);
        let transfer_len = block_count as u32 * 4096;
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

    /// Write data at the given LBA. Data length must be a multiple of 4096.
    pub async fn write(&mut self, lba: u64, data: &[u8]) -> io::Result<()> {
        let block_count = (data.len() / 4096) as u16;
        let itt = self.next_itt();
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::ScsiCommand);
        bhs.set_immediate(true);
        bhs.set_final(true);
        // Write flag (bit 5 of byte 1)
        bhs.raw[1] |= 0x20;
        bhs.set_initiator_task_tag(itt);
        bhs.set_cmd_sn(self.cmd_sn);
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
