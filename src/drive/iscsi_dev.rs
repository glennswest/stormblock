//! iSCSI initiator block device — connects to a remote iSCSI target and
//! exposes it as a `BlockDevice`.
//!
//! Ported from the test initiator (`tests/common/iscsi_initiator.rs`) into
//! production code. Supports login (no CHAP), READ/WRITE(10), READ CAPACITY,
//! UNMAP (discard), and NOP-Out keepalive handling.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::io::{BufReader, BufWriter};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::target::iscsi::pdu::{
    Bhs, IscsiPdu, Opcode, encode_text_params, parse_text_params, read_pdu, write_pdu,
};

use super::{BlockDevice, DeviceId, DriveError, DriveResult, DriveType, SmartData};

// SCSI CDB operation codes
const INQUIRY: u8 = 0x12;
const READ_CAPACITY_10: u8 = 0x25;
const READ_10: u8 = 0x28;
const WRITE_10: u8 = 0x2A;
const UNMAP: u8 = 0x42;

// Login stages
const STAGE_SECURITY: u8 = 0;
const STAGE_OPERATIONAL: u8 = 1;
const STAGE_FULL_FEATURE: u8 = 3;

/// Atomic counter for unique ISID qualifier per connection.
static ISID_COUNTER: AtomicU32 = AtomicU32::new(1);

/// An iSCSI block device — connects to a remote iSCSI target as an initiator
/// and implements the `BlockDevice` trait.
///
/// All I/O is serialized through a Mutex-wrapped TCP stream. For higher
/// throughput, multiple connections (MC/S) could be added later.
pub struct IscsiDevice {
    /// Reader/writer protected by a mutex for serialized I/O.
    conn: Mutex<IscsiConnection>,
    /// Portal address (host:port).
    portal: String,
    /// Target IQN.
    iqn: String,
    /// Total capacity in bytes.
    capacity: AtomicU64,
    /// Block size in bytes (512 or 4096).
    block_size: AtomicU32,
    /// Device identity.
    id: DeviceId,
}

/// Internal connection state.
struct IscsiConnection {
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

impl IscsiConnection {
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

    fn parse_login_response(&mut self, resp: &IscsiPdu) -> Result<(), DriveError> {
        self.exp_stat_sn = resp.bhs.cmd_sn();
        self.cmd_sn = u32::from_be_bytes(resp.bhs.raw[28..32].try_into().unwrap());
        let tsih = resp.bhs.tsih();
        if tsih != 0 {
            self.tsih = tsih;
        }

        let resp_params = parse_text_params(&resp.data);
        for (key, val) in &resp_params {
            if key == "MaxRecvDataSegmentLength" {
                if let Ok(v) = val.parse::<u32>() {
                    self.max_recv_data_seg = v;
                }
            }
        }
        Ok(())
    }

    /// Perform iSCSI login (no CHAP). Two-phase: Security → Operational → FullFeature.
    async fn login(
        &mut self,
        initiator_name: &str,
        target_name: &str,
    ) -> Result<(), DriveError> {
        let login_itt = self.next_itt();

        // Phase 1: Security negotiation
        let security_params = encode_text_params(&[
            ("InitiatorName", initiator_name),
            ("TargetName", target_name),
            ("SessionType", "Normal"),
            ("AuthMethod", "None"),
        ]);

        let bhs = self.make_login_bhs(login_itt, STAGE_SECURITY, STAGE_OPERATIONAL);
        let pdu = IscsiPdu::with_data(bhs, security_params);
        write_pdu(&mut self.writer, &pdu, false, false)
            .await
            .map_err(DriveError::Io)?;

        let resp = read_pdu(&mut self.reader, false, false)
            .await
            .map_err(DriveError::Io)?;
        if resp.bhs.opcode() != Some(Opcode::LoginResponse) {
            return Err(DriveError::Other(anyhow::anyhow!(
                "expected LoginResponse, got {:?}",
                resp.bhs.opcode()
            )));
        }

        let status_class = resp.bhs.raw[36];
        if status_class != 0 {
            let resp_params = parse_text_params(&resp.data);
            return Err(DriveError::Other(anyhow::anyhow!(
                "iSCSI security login failed: class={} params={:?}",
                status_class,
                resp_params
            )));
        }

        self.parse_login_response(&resp)?;

        // If target went straight to FullFeature
        if resp.bhs.transit() && resp.bhs.nsg() == STAGE_FULL_FEATURE {
            self.exp_stat_sn += 1;
            return Ok(());
        }

        // Phase 2: Operational negotiation
        let op_data = encode_text_params(&Self::operational_params());
        let bhs = self.make_login_bhs(login_itt, STAGE_OPERATIONAL, STAGE_FULL_FEATURE);
        let pdu = IscsiPdu::with_data(bhs, op_data);
        write_pdu(&mut self.writer, &pdu, false, false)
            .await
            .map_err(DriveError::Io)?;

        let resp = read_pdu(&mut self.reader, false, false)
            .await
            .map_err(DriveError::Io)?;
        if resp.bhs.opcode() != Some(Opcode::LoginResponse) {
            return Err(DriveError::Other(anyhow::anyhow!(
                "expected LoginResponse phase 2, got {:?}",
                resp.bhs.opcode()
            )));
        }

        let status_class = resp.bhs.raw[36];
        if status_class != 0 {
            let resp_params = parse_text_params(&resp.data);
            return Err(DriveError::Other(anyhow::anyhow!(
                "iSCSI operational login failed: class={} params={:?}",
                status_class,
                resp_params
            )));
        }

        self.parse_login_response(&resp)?;
        self.exp_stat_sn += 1;
        Ok(())
    }

    /// Read a response PDU, handling unsolicited NOP-In keep-alives transparently.
    ///
    /// Unsolicited NOP-In (target ping): ITT=0xFFFFFFFF, TTT=target-value → respond and skip.
    /// Solicited NOP-In (response to our NOP-Out): ITT=our-value, TTT=0xFFFFFFFF → return it.
    async fn read_response(&mut self) -> Result<IscsiPdu, DriveError> {
        loop {
            let resp = read_pdu(&mut self.reader, false, false)
                .await
                .map_err(DriveError::Io)?;
            if resp.bhs.opcode() == Some(Opcode::NopIn) {
                let itt = resp.bhs.initiator_task_tag();
                if itt == 0xFFFF_FFFF {
                    // Unsolicited NOP-In from target — respond if TTT is set
                    let ttt = resp.bhs.target_transfer_tag();
                    if ttt != 0xFFFF_FFFF {
                        let mut bhs = Bhs::new();
                        bhs.set_opcode(Opcode::NopOut);
                        bhs.set_immediate(true);
                        bhs.set_final(true);
                        bhs.set_initiator_task_tag(0xFFFF_FFFF);
                        bhs.set_target_transfer_tag(ttt);
                        bhs.set_cmd_sn(self.cmd_sn);
                        bhs.set_stat_sn(self.exp_stat_sn);
                        let pdu = IscsiPdu::new(bhs);
                        write_pdu(&mut self.writer, &pdu, false, false)
                            .await
                            .map_err(DriveError::Io)?;
                    }
                    continue;
                }
                // Solicited NOP-In — response to our NOP-Out, return it
                return Ok(resp);
            }
            return Ok(resp);
        }
    }

    /// Send READ CAPACITY(10) and return (total_blocks, block_size).
    async fn read_capacity(&mut self) -> Result<(u64, u32), DriveError> {
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
        write_pdu(&mut self.writer, &pdu, false, false)
            .await
            .map_err(DriveError::Io)?;
        self.cmd_sn += 1;

        let mut data = Vec::new();
        loop {
            let resp = self.read_response().await?;
            match resp.bhs.opcode() {
                Some(Opcode::DataIn) => {
                    data.extend_from_slice(&resp.data);
                    if resp.bhs.has_status() {
                        break;
                    }
                }
                Some(Opcode::ScsiResponse) => break,
                other => {
                    return Err(DriveError::Other(anyhow::anyhow!(
                        "unexpected opcode in read_capacity: {:?}",
                        other
                    )));
                }
            }
        }

        if data.len() < 8 {
            return Err(DriveError::Other(anyhow::anyhow!(
                "read capacity data too short"
            )));
        }

        let last_lba = u32::from_be_bytes(data[0..4].try_into().unwrap()) as u64;
        let block_size = u32::from_be_bytes(data[4..8].try_into().unwrap());
        self.block_size = block_size;
        Ok((last_lba + 1, block_size))
    }

    /// Send SCSI INQUIRY and return the inquiry data.
    async fn inquiry(&mut self) -> Result<Vec<u8>, DriveError> {
        let itt = self.next_itt();
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::ScsiCommand);
        bhs.set_final(true);
        bhs.raw[1] |= 0x40; // Read
        bhs.set_initiator_task_tag(itt);
        bhs.set_cmd_sn(self.cmd_sn);
        bhs.set_stat_sn(self.exp_stat_sn);
        bhs.set_lun(0);
        bhs.set_expected_data_transfer_length(96);

        let mut cdb = [0u8; 16];
        cdb[0] = INQUIRY;
        cdb[4] = 96;
        bhs.set_cdb(&cdb);

        let pdu = IscsiPdu::new(bhs);
        write_pdu(&mut self.writer, &pdu, false, false)
            .await
            .map_err(DriveError::Io)?;
        self.cmd_sn += 1;

        let mut data = Vec::new();
        loop {
            let resp = self.read_response().await?;
            match resp.bhs.opcode() {
                Some(Opcode::DataIn) => {
                    data.extend_from_slice(&resp.data);
                    if resp.bhs.has_status() {
                        break;
                    }
                }
                Some(Opcode::ScsiResponse) => break,
                other => {
                    return Err(DriveError::Other(anyhow::anyhow!(
                        "unexpected opcode in inquiry: {:?}",
                        other
                    )));
                }
            }
        }

        Ok(data)
    }

    /// SCSI READ(10) — read blocks from the target.
    async fn scsi_read(
        &mut self,
        lba: u64,
        block_count: u16,
    ) -> Result<Vec<u8>, DriveError> {
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

        let mut cdb = [0u8; 16];
        cdb[0] = READ_10;
        cdb[2..6].copy_from_slice(&(lba as u32).to_be_bytes());
        cdb[7..9].copy_from_slice(&block_count.to_be_bytes());
        bhs.set_cdb(&cdb);

        let pdu = IscsiPdu::new(bhs);
        write_pdu(&mut self.writer, &pdu, false, false)
            .await
            .map_err(DriveError::Io)?;
        self.cmd_sn += 1;

        let mut data = Vec::new();
        loop {
            let resp = self.read_response().await?;
            match resp.bhs.opcode() {
                Some(Opcode::DataIn) => {
                    data.extend_from_slice(&resp.data);
                    if resp.bhs.has_status() {
                        break;
                    }
                }
                Some(Opcode::ScsiResponse) => break,
                other => {
                    return Err(DriveError::Other(anyhow::anyhow!(
                        "unexpected opcode in read: {:?}",
                        other
                    )));
                }
            }
        }

        Ok(data)
    }

    /// SCSI WRITE(10) — write data at the given LBA.
    ///
    /// Data is padded to a block_size boundary if necessary — SCSI requires
    /// transfer lengths that are exact multiples of the device block size.
    async fn scsi_write(
        &mut self,
        lba: u64,
        data: &[u8],
    ) -> Result<(), DriveError> {
        let bs = self.block_size as usize;
        // Pad data to next block boundary if not aligned
        let padded = if data.len() % bs != 0 {
            let padded_len = data.len().div_ceil(bs) * bs;
            let mut buf = vec![0u8; padded_len];
            buf[..data.len()].copy_from_slice(data);
            buf
        } else {
            data.to_vec()
        };
        let block_count = (padded.len() / bs) as u16;
        let itt = self.next_itt();
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::ScsiCommand);
        bhs.set_final(true);
        bhs.raw[1] |= 0x20; // Write
        bhs.set_initiator_task_tag(itt);
        bhs.set_cmd_sn(self.cmd_sn);
        bhs.set_stat_sn(self.exp_stat_sn);
        bhs.set_lun(0);
        bhs.set_expected_data_transfer_length(padded.len() as u32);

        let mut cdb = [0u8; 16];
        cdb[0] = WRITE_10;
        cdb[2..6].copy_from_slice(&(lba as u32).to_be_bytes());
        cdb[7..9].copy_from_slice(&block_count.to_be_bytes());
        bhs.set_cdb(&cdb);

        let pdu = IscsiPdu::with_data(bhs, padded);
        write_pdu(&mut self.writer, &pdu, false, false)
            .await
            .map_err(DriveError::Io)?;
        self.cmd_sn += 1;

        let resp = self.read_response().await?;
        match resp.bhs.opcode() {
            Some(Opcode::ScsiResponse) => {
                let status = resp.bhs.status();
                if status != 0 {
                    return Err(DriveError::Other(anyhow::anyhow!(
                        "SCSI write failed with status {status:#x}"
                    )));
                }
                Ok(())
            }
            other => Err(DriveError::Other(anyhow::anyhow!(
                "expected ScsiResponse after write, got {:?}",
                other
            ))),
        }
    }

    /// SCSI UNMAP — discard blocks.
    async fn scsi_unmap(
        &mut self,
        lba: u64,
        block_count: u32,
    ) -> Result<(), DriveError> {
        let itt = self.next_itt();
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::ScsiCommand);
        bhs.set_final(true);
        bhs.raw[1] |= 0x20; // Write direction (UNMAP sends a parameter list)
        bhs.set_initiator_task_tag(itt);
        bhs.set_cmd_sn(self.cmd_sn);
        bhs.set_stat_sn(self.exp_stat_sn);
        bhs.set_lun(0);

        // UNMAP parameter list: 8-byte header + 16-byte descriptor
        let param_len: u32 = 24;
        bhs.set_expected_data_transfer_length(param_len);

        let mut cdb = [0u8; 16];
        cdb[0] = UNMAP;
        // Allocation length (bytes 7-8 of CDB)
        cdb[7..9].copy_from_slice(&(param_len as u16).to_be_bytes());
        bhs.set_cdb(&cdb);

        // Build UNMAP parameter list
        let mut params = vec![0u8; 24];
        // Unmap data length (total - 2): 22
        params[0..2].copy_from_slice(&22u16.to_be_bytes());
        // Block descriptor data length: 16
        params[2..4].copy_from_slice(&16u16.to_be_bytes());
        // Block descriptor: LBA (8 bytes) + count (4 bytes) + reserved (4 bytes)
        params[8..16].copy_from_slice(&lba.to_be_bytes());
        params[16..20].copy_from_slice(&block_count.to_be_bytes());

        let pdu = IscsiPdu::with_data(bhs, params);
        write_pdu(&mut self.writer, &pdu, false, false)
            .await
            .map_err(DriveError::Io)?;
        self.cmd_sn += 1;

        let resp = self.read_response().await?;
        match resp.bhs.opcode() {
            Some(Opcode::ScsiResponse) => {
                let status = resp.bhs.status();
                if status != 0 {
                    // UNMAP may not be supported — treat as no-op
                    tracing::debug!("UNMAP returned status {status:#x}, treating as no-op");
                }
                Ok(())
            }
            other => Err(DriveError::Other(anyhow::anyhow!(
                "expected ScsiResponse after UNMAP, got {:?}",
                other
            ))),
        }
    }

    /// Send iSCSI Logout.
    async fn logout(&mut self) -> Result<(), DriveError> {
        let itt = self.next_itt();
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::LogoutRequest);
        bhs.set_immediate(true);
        bhs.set_final(true);
        bhs.raw[1] &= 0x80; // Reason code 0: close session
        bhs.set_initiator_task_tag(itt);
        bhs.set_cmd_sn(self.cmd_sn);
        bhs.set_stat_sn(self.exp_stat_sn);

        let pdu = IscsiPdu::new(bhs);
        write_pdu(&mut self.writer, &pdu, false, false)
            .await
            .map_err(DriveError::Io)?;
        self.cmd_sn += 1;

        let resp = self.read_response().await?;
        if resp.bhs.opcode() != Some(Opcode::LogoutResponse) {
            return Err(DriveError::Other(anyhow::anyhow!(
                "expected LogoutResponse"
            )));
        }
        Ok(())
    }
}

impl IscsiDevice {
    /// Connect to an iSCSI target and perform login + READ CAPACITY.
    ///
    /// Returns a ready-to-use `IscsiDevice` implementing `BlockDevice`.
    pub async fn connect(
        portal: &str,
        port: u16,
        iqn: &str,
    ) -> DriveResult<Self> {
        let addr = format!("{portal}:{port}");
        tracing::info!("iSCSI initiator: connecting to {addr} target={iqn}");

        let stream = TcpStream::connect(&addr)
            .await
            .map_err(DriveError::Io)?;
        stream.set_nodelay(true).map_err(DriveError::Io)?;
        let (reader, writer) = stream.into_split();

        let qualifier = ISID_COUNTER.fetch_add(1, Ordering::Relaxed);
        let isid = [
            0x40,
            0x00,
            0x00,
            0x01,
            (qualifier >> 8) as u8,
            qualifier as u8,
        ];

        let mut conn = IscsiConnection {
            reader: BufReader::new(reader),
            writer: BufWriter::new(writer),
            cmd_sn: 1,
            exp_stat_sn: 0,
            tsih: 0,
            itt: 1,
            max_recv_data_seg: 8192,
            block_size: 512,
            isid,
        };

        // Login
        let initiator_name = "iqn.2024.io.stormblock:initiator";
        conn.login(initiator_name, iqn).await?;
        tracing::info!("iSCSI initiator: login successful");

        // READ CAPACITY to get disk size
        let (total_blocks, block_size) = conn.read_capacity().await?;
        let capacity = total_blocks * block_size as u64;
        tracing::info!(
            "iSCSI initiator: capacity={} bytes ({:.1} GB), block_size={}, blocks={}",
            capacity,
            capacity as f64 / (1024.0 * 1024.0 * 1024.0),
            block_size,
            total_blocks
        );

        // INQUIRY for model/serial
        let inquiry_data = conn.inquiry().await.unwrap_or_default();
        let vendor = if inquiry_data.len() >= 16 {
            String::from_utf8_lossy(&inquiry_data[8..16]).trim().to_string()
        } else {
            "iSCSI".to_string()
        };
        let product = if inquiry_data.len() >= 32 {
            String::from_utf8_lossy(&inquiry_data[16..32]).trim().to_string()
        } else {
            "Disk".to_string()
        };

        let id = DeviceId {
            uuid: Uuid::new_v4(),
            serial: format!("iscsi-{}", &Uuid::new_v4().simple().to_string()[..8]),
            model: format!("{} {}", vendor, product),
            path: format!("iscsi://{}:{}/{}", portal, port, iqn),
        };

        Ok(IscsiDevice {
            conn: Mutex::new(conn),
            portal: addr,
            iqn: iqn.to_string(),
            capacity: AtomicU64::new(capacity),
            block_size: AtomicU32::new(block_size),
            id,
        })
    }

    /// Get the portal address.
    pub fn portal(&self) -> &str {
        &self.portal
    }

    /// Get the target IQN.
    pub fn iqn(&self) -> &str {
        &self.iqn
    }

    /// Gracefully disconnect from the target.
    pub async fn disconnect(&self) -> DriveResult<()> {
        let mut conn = self.conn.lock().await;
        conn.logout().await
    }
}

#[async_trait]
impl BlockDevice for IscsiDevice {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn capacity_bytes(&self) -> u64 {
        self.capacity.load(Ordering::Relaxed)
    }

    fn block_size(&self) -> u32 {
        self.block_size.load(Ordering::Relaxed)
    }

    fn optimal_io_size(&self) -> u32 {
        // 64KB is a good chunk size for iSCSI
        65536
    }

    fn device_type(&self) -> DriveType {
        DriveType::Iscsi
    }

    async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        let bs = self.block_size() as u64;
        if offset % bs != 0 {
            return Err(DriveError::NotAligned {
                offset,
                block_size: bs as u32,
            });
        }

        let mut conn = self.conn.lock().await;
        let mut bytes_read = 0usize;

        while bytes_read < buf.len() {
            let remaining = buf.len() - bytes_read;
            // Cap at 64KB per SCSI command (typical MaxBurstLength)
            let chunk = remaining.min(65536);
            let block_count = (chunk as u64 / bs) as u16;
            if block_count == 0 {
                break;
            }

            let lba = (offset + bytes_read as u64) / bs;
            let data = conn.scsi_read(lba, block_count).await?;
            let to_copy = data.len().min(remaining);
            buf[bytes_read..bytes_read + to_copy].copy_from_slice(&data[..to_copy]);
            bytes_read += to_copy;
        }

        Ok(bytes_read)
    }

    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        let bs = self.block_size() as u64;
        if offset % bs != 0 {
            return Err(DriveError::NotAligned {
                offset,
                block_size: bs as u32,
            });
        }

        let mut conn = self.conn.lock().await;
        let mut bytes_written = 0usize;

        while bytes_written < buf.len() {
            let remaining = buf.len() - bytes_written;
            // Cap at FirstBurstLength (65536) for immediate data,
            // but also round down to block_size boundary unless this
            // is the last chunk (scsi_write handles final padding).
            let max_chunk = 65536usize;
            let chunk = if remaining <= max_chunk {
                remaining
            } else {
                // Round down to block boundary for mid-stream chunks
                (max_chunk / bs as usize) * bs as usize
            };
            let lba = (offset + bytes_written as u64) / bs;
            conn.scsi_write(lba, &buf[bytes_written..bytes_written + chunk])
                .await?;
            bytes_written += chunk;
        }

        Ok(bytes_written)
    }

    async fn flush(&self) -> DriveResult<()> {
        // NOP-Out as keepalive/sync signal
        let mut conn = self.conn.lock().await;
        let itt = conn.next_itt();
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::NopOut);
        bhs.set_immediate(true);
        bhs.set_final(true);
        bhs.set_initiator_task_tag(itt);
        bhs.set_target_transfer_tag(0xFFFF_FFFF);
        bhs.set_cmd_sn(conn.cmd_sn);
        bhs.set_stat_sn(conn.exp_stat_sn);

        let pdu = IscsiPdu::new(bhs);
        write_pdu(&mut conn.writer, &pdu, false, false)
            .await
            .map_err(DriveError::Io)?;

        // Wait for NOP-In response
        let _resp = conn.read_response().await?;
        Ok(())
    }

    async fn discard(&self, offset: u64, len: u64) -> DriveResult<()> {
        let bs = self.block_size() as u64;
        let lba = offset / bs;
        let block_count = (len / bs) as u32;
        if block_count == 0 {
            return Ok(());
        }

        let mut conn = self.conn.lock().await;
        conn.scsi_unmap(lba, block_count).await
    }

    fn smart_status(&self) -> DriveResult<SmartData> {
        Ok(SmartData {
            healthy: true,
            ..Default::default()
        })
    }
}

impl Drop for IscsiDevice {
    fn drop(&mut self) {
        tracing::debug!("IscsiDevice dropped: {}", self.portal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isid_counter_increments() {
        let a = ISID_COUNTER.fetch_add(1, Ordering::Relaxed);
        let b = ISID_COUNTER.fetch_add(1, Ordering::Relaxed);
        assert!(b > a);
    }
}
