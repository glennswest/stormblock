//! iSCSI target — RFC 7143, port 3260, CHAP auth, SCSI command dispatch.
//!
//! The target accepts TCP connections, runs the login state machine,
//! then enters full-feature phase dispatching SCSI commands to `BlockDevice`.

pub mod pdu;
pub mod login;
pub mod chap;
pub mod scsi;
pub mod session;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};

use crate::drive::BlockDevice;
use super::reactor::ReactorPool;

use pdu::{Bhs, IscsiPdu, Opcode, read_pdu, write_pdu};
use login::{LoginResult, LoginStateMachine};
use chap::ChapConfig;
use scsi::{ScsiStatus, handle_scsi_command};
use session::{ConnectionState, SessionParams, SessionRegistry};

/// iSCSI target configuration.
#[derive(Debug, Clone)]
pub struct IscsiConfig {
    /// Listen address (default: 0.0.0.0:3260).
    pub listen_addr: SocketAddr,
    /// iSCSI Qualified Name for this target.
    pub target_name: String,
    /// CHAP authentication (None = no auth).
    pub chap: Option<ChapConfig>,
    /// Maximum concurrent sessions.
    pub max_sessions: u32,
}

impl Default for IscsiConfig {
    fn default() -> Self {
        IscsiConfig {
            listen_addr: "0.0.0.0:3260".parse().unwrap(),
            target_name: "iqn.2024.io.stormblock:default".into(),
            chap: None,
            max_sessions: 64,
        }
    }
}

/// iSCSI target server.
pub struct IscsiTarget {
    config: IscsiConfig,
    sessions: Arc<SessionRegistry>,
    luns: Arc<HashMap<u64, Arc<dyn BlockDevice>>>,
}

impl IscsiTarget {
    pub fn new(config: IscsiConfig) -> Self {
        IscsiTarget {
            config,
            sessions: Arc::new(SessionRegistry::new()),
            luns: Arc::new(HashMap::new()),
        }
    }

    /// Add a LUN mapping. Must be called before `run()`.
    pub fn add_lun(&mut self, lun: u64, device: Arc<dyn BlockDevice>) {
        Arc::get_mut(&mut self.luns).expect("add_lun after run").insert(lun, device);
    }

    /// Start accepting connections. Runs until the listener is dropped.
    pub async fn run(self: Arc<Self>, reactor: &ReactorPool) -> std::io::Result<()> {
        let listener = TcpListener::bind(self.config.listen_addr).await?;
        tracing::info!("iSCSI target listening on {} ({})", self.config.listen_addr, self.config.target_name);
        self.run_with_listener(listener, reactor).await
    }

    /// Accept connections on a pre-bound listener. Useful for tests with ephemeral ports.
    pub async fn run_with_listener(self: Arc<Self>, listener: TcpListener, _reactor: &ReactorPool) -> std::io::Result<()> {
        loop {
            let (stream, peer) = listener.accept().await?;
            stream.set_nodelay(true)?;
            let target = self.clone();
            tokio::spawn(async move {
                tracing::debug!("iSCSI connection from {peer}");
                if let Err(e) = target.handle_connection(stream, peer).await {
                    tracing::debug!("iSCSI connection {peer} closed: {e}");
                }
            });
        }
    }

    async fn handle_connection(&self, stream: TcpStream, peer: SocketAddr) -> std::io::Result<()> {
        let (reader, writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut writer = BufWriter::new(writer);

        // Login phase
        let (session_params, tsih) = self.login_phase(&mut reader, &mut writer).await?;
        tracing::info!(
            "iSCSI session established from {peer}, TSIH={tsih}, initiator={}",
            session_params.initiator_name
        );

        // Full feature phase
        let result = self.full_feature_phase(
            &mut reader,
            &mut writer,
            &session_params,
        ).await;

        // Cleanup
        self.sessions.remove_session(tsih).await;
        tracing::debug!("iSCSI session {tsih} from {peer} ended");
        result
    }

    async fn login_phase<R, W>(
        &self,
        reader: &mut R,
        writer: &mut W,
    ) -> std::io::Result<(SessionParams, u16)>
    where
        R: AsyncReadExt + Unpin,
        W: AsyncWriteExt + Unpin,
    {
        let mut state_machine = LoginStateMachine::new(
            self.config.target_name.clone(),
            self.config.chap.clone(),
        );

        loop {
            let req = read_pdu(reader, false, false).await?;
            if req.bhs.opcode() != Some(Opcode::LoginRequest) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "expected login request PDU",
                ));
            }

            match state_machine.process(&req) {
                LoginResult::Continue(resp) => {
                    write_pdu(writer, &resp, false, false).await?;
                }
                LoginResult::Complete(resp, params) => {
                    // Allocate TSIH and register session
                    let session = self.sessions.create_session(
                        req.bhs.isid(),
                        params.clone(),
                    ).await;
                    let tsih = session.tsih;

                    // Set TSIH in response
                    let mut final_resp = resp;
                    final_resp.bhs.set_tsih(tsih);

                    write_pdu(writer, &final_resp, false, false).await?;
                    return Ok((params, tsih));
                }
                LoginResult::Failed(resp) => {
                    write_pdu(writer, &resp, false, false).await?;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "login failed",
                    ));
                }
            }
        }
    }

    async fn full_feature_phase<R, W>(
        &self,
        reader: &mut R,
        writer: &mut W,
        params: &SessionParams,
    ) -> std::io::Result<()>
    where
        R: AsyncReadExt + Unpin,
        W: AsyncWriteExt + Unpin,
    {
        let conn = ConnectionState::new(0);
        let header_digest = params.header_digest;
        let data_digest = params.data_digest;
        let max_data_seg = params.max_recv_data_segment_length as usize;

        loop {
            let req = read_pdu(reader, header_digest, data_digest).await?;
            let opcode = match req.bhs.opcode() {
                Some(op) => op,
                None => continue,
            };

            match opcode {
                Opcode::ScsiCommand => {
                    self.handle_scsi_pdu(&req, reader, writer, &conn, params, header_digest, data_digest, max_data_seg).await?;
                }
                Opcode::NopOut => {
                    self.handle_nop_out(&req, writer, &conn, header_digest, data_digest).await?;
                }
                Opcode::LogoutRequest => {
                    self.handle_logout(&req, writer, &conn, header_digest, data_digest).await?;
                    return Ok(());
                }
                Opcode::TaskMgmtRequest => {
                    self.handle_task_mgmt(&req, writer, &conn, header_digest, data_digest).await?;
                }
                _ => {
                    tracing::debug!("ignoring unsupported opcode: {opcode}");
                }
            }
        }
    }

    async fn handle_scsi_pdu<R: AsyncReadExt + Unpin, W: AsyncWriteExt + Unpin>(
        &self,
        req: &IscsiPdu,
        reader: &mut R,
        writer: &mut W,
        conn: &ConnectionState,
        params: &SessionParams,
        header_digest: bool,
        data_digest: bool,
        max_data_seg: usize,
    ) -> std::io::Result<()> {
        let cdb = req.bhs.cdb();
        let lun_id = req.bhs.lun() >> 48; // LUN encoding: peripheral device addressing
        let itt = req.bhs.initiator_task_tag();
        let cmd_sn = req.bhs.cmd_sn();

        conn.advance_cmd_sn(cmd_sn);

        let device = match self.luns.get(&lun_id) {
            Some(dev) => dev,
            None => {
                // LUN not found — send check condition
                let result = scsi::ScsiResult::check_condition(scsi::SenseData::illegal_request());
                let resp_pdu = self.build_scsi_response(&conn, itt, &result);
                return write_pdu(writer, &resp_pdu, header_digest, data_digest).await;
            }
        };

        // For write commands, data may be inline (immediate data) or need R2T
        let is_write = matches!(cdb[0], scsi::WRITE_10 | scsi::WRITE_16);
        let expected_len = req.bhs.expected_data_transfer_length() as usize;

        let data_out = if is_write {
            let mut write_data = req.data.clone();

            // If immediate data is insufficient, use R2T/Data-Out to get the rest
            if write_data.len() < expected_len {
                let remaining = expected_len - write_data.len();
                let additional = self.receive_data_via_r2t(
                    reader,
                    writer,
                    conn,
                    itt,
                    write_data.len() as u32,
                    remaining as u32,
                    params.max_burst_length,
                    header_digest,
                    data_digest,
                ).await?;
                write_data.extend_from_slice(&additional);
            }
            write_data
        } else {
            Vec::new()
        };

        let result = handle_scsi_command(cdb, device, &data_out).await;

        // Send read data via Data-In PDUs if needed
        if !result.data.is_empty() && result.status == ScsiStatus::Good && !is_write {
            self.send_data_in(writer, &conn, itt, &result.data, max_data_seg, header_digest, data_digest).await?;
        } else {
            let resp_pdu = self.build_scsi_response(&conn, itt, &result);
            write_pdu(writer, &resp_pdu, header_digest, data_digest).await?;
        }

        Ok(())
    }

    /// Send read data as Data-In PDUs, with status on the last one.
    async fn send_data_in<W: AsyncWriteExt + Unpin>(
        &self,
        writer: &mut W,
        conn: &ConnectionState,
        itt: u32,
        data: &[u8],
        max_seg: usize,
        header_digest: bool,
        data_digest: bool,
    ) -> std::io::Result<()> {
        let chunks: Vec<&[u8]> = data.chunks(max_seg).collect();
        let last_idx = chunks.len() - 1;

        for (i, chunk) in chunks.iter().enumerate() {
            let is_last = i == last_idx;
            let mut bhs = Bhs::new();
            bhs.set_opcode(Opcode::DataIn);
            bhs.set_final(is_last);
            bhs.set_initiator_task_tag(itt);
            bhs.set_data_sn(i as u32);
            bhs.set_buffer_offset((i * max_seg) as u32);

            if is_last {
                bhs.set_has_status(true);
                bhs.set_status(ScsiStatus::Good as u8);
                let stat_sn = conn.next_stat_sn();
                bhs.set_stat_sn(stat_sn);
                bhs.set_exp_cmd_sn(conn.exp_cmd_sn.load(std::sync::atomic::Ordering::Relaxed));
                bhs.set_max_cmd_sn(conn.max_cmd_sn.load(std::sync::atomic::Ordering::Relaxed));
            }

            let pdu = IscsiPdu::with_data(bhs, chunk.to_vec());
            write_pdu(writer, &pdu, header_digest, data_digest).await?;
        }

        Ok(())
    }

    fn build_scsi_response(
        &self,
        conn: &ConnectionState,
        itt: u32,
        result: &scsi::ScsiResult,
    ) -> IscsiPdu {
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::ScsiResponse);
        bhs.set_final(true);
        bhs.set_initiator_task_tag(itt);
        bhs.set_status(result.status as u8);

        let stat_sn = conn.next_stat_sn();
        bhs.set_stat_sn(stat_sn);
        bhs.set_exp_cmd_sn(conn.exp_cmd_sn.load(std::sync::atomic::Ordering::Relaxed));
        bhs.set_max_cmd_sn(conn.max_cmd_sn.load(std::sync::atomic::Ordering::Relaxed));

        if result.status == ScsiStatus::CheckCondition && result.sense.is_some() {
            let sense_data = result.sense.as_ref().unwrap().to_bytes();
            // Sense data is prefixed with 2-byte sense length
            let mut data = Vec::with_capacity(2 + sense_data.len());
            data.extend_from_slice(&(sense_data.len() as u16).to_be_bytes());
            data.extend_from_slice(&sense_data);
            IscsiPdu::with_data(bhs, data)
        } else {
            IscsiPdu::new(bhs)
        }
    }

    /// Send R2T (Ready To Transfer) and receive Data-Out PDUs for write commands.
    async fn receive_data_via_r2t<R: AsyncReadExt + Unpin, W: AsyncWriteExt + Unpin>(
        &self,
        reader: &mut R,
        writer: &mut W,
        conn: &ConnectionState,
        itt: u32,
        buffer_offset: u32,
        desired_length: u32,
        max_burst: u32,
        header_digest: bool,
        data_digest: bool,
    ) -> std::io::Result<Vec<u8>> {
        let mut collected = Vec::with_capacity(desired_length as usize);
        let mut offset = buffer_offset;
        let mut r2t_sn: u32 = 0;

        while collected.len() < desired_length as usize {
            let remaining = desired_length as usize - collected.len();
            let transfer_len = remaining.min(max_burst as usize) as u32;

            // Send R2T PDU
            let mut r2t_bhs = Bhs::new();
            r2t_bhs.set_opcode(Opcode::R2T);
            r2t_bhs.set_final(true);
            r2t_bhs.set_initiator_task_tag(itt);
            r2t_bhs.set_target_transfer_tag(itt); // Use ITT as TTT for simplicity
            r2t_bhs.set_stat_sn(conn.stat_sn.load(std::sync::atomic::Ordering::Relaxed));
            r2t_bhs.set_exp_cmd_sn(conn.exp_cmd_sn.load(std::sync::atomic::Ordering::Relaxed));
            r2t_bhs.set_max_cmd_sn(conn.max_cmd_sn.load(std::sync::atomic::Ordering::Relaxed));
            r2t_bhs.set_r2t_sn(r2t_sn);
            r2t_bhs.set_buffer_offset(offset);
            r2t_bhs.set_desired_data_transfer_length(transfer_len);

            let r2t_pdu = IscsiPdu::new(r2t_bhs);
            write_pdu(writer, &r2t_pdu, header_digest, data_digest).await?;

            // Receive Data-Out PDUs until this R2T is satisfied
            let mut burst_received: u32 = 0;
            while burst_received < transfer_len {
                let data_out = read_pdu(reader, header_digest, data_digest).await?;
                if data_out.bhs.opcode() != Some(Opcode::DataOut) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "expected Data-Out PDU",
                    ));
                }
                collected.extend_from_slice(&data_out.data);
                burst_received += data_out.data.len() as u32;
            }

            offset += transfer_len;
            r2t_sn += 1;
        }

        Ok(collected)
    }

    async fn handle_nop_out<W: AsyncWriteExt + Unpin>(
        &self,
        req: &IscsiPdu,
        writer: &mut W,
        conn: &ConnectionState,
        header_digest: bool,
        data_digest: bool,
    ) -> std::io::Result<()> {
        let itt = req.bhs.initiator_task_tag();
        if itt == 0xFFFFFFFF {
            return Ok(()); // Unsolicited NOP-Out, no response needed
        }

        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::NopIn);
        bhs.set_final(true);
        bhs.set_initiator_task_tag(itt);
        bhs.set_target_transfer_tag(0xFFFFFFFF);

        let stat_sn = conn.next_stat_sn();
        bhs.set_stat_sn(stat_sn);
        bhs.set_exp_cmd_sn(conn.exp_cmd_sn.load(std::sync::atomic::Ordering::Relaxed));
        bhs.set_max_cmd_sn(conn.max_cmd_sn.load(std::sync::atomic::Ordering::Relaxed));

        let pdu = IscsiPdu::new(bhs);
        write_pdu(writer, &pdu, header_digest, data_digest).await
    }

    async fn handle_logout<W: AsyncWriteExt + Unpin>(
        &self,
        req: &IscsiPdu,
        writer: &mut W,
        conn: &ConnectionState,
        header_digest: bool,
        data_digest: bool,
    ) -> std::io::Result<()> {
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::LogoutResponse);
        bhs.set_final(true);
        bhs.set_initiator_task_tag(req.bhs.initiator_task_tag());
        // Response: 0 = connection closed successfully
        bhs.raw[2] = 0;

        let stat_sn = conn.next_stat_sn();
        bhs.set_stat_sn(stat_sn);
        bhs.set_exp_cmd_sn(conn.exp_cmd_sn.load(std::sync::atomic::Ordering::Relaxed));
        bhs.set_max_cmd_sn(conn.max_cmd_sn.load(std::sync::atomic::Ordering::Relaxed));

        let pdu = IscsiPdu::new(bhs);
        write_pdu(writer, &pdu, header_digest, data_digest).await
    }

    async fn handle_task_mgmt<W: AsyncWriteExt + Unpin>(
        &self,
        req: &IscsiPdu,
        writer: &mut W,
        conn: &ConnectionState,
        header_digest: bool,
        data_digest: bool,
    ) -> std::io::Result<()> {
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::TaskMgmtResponse);
        bhs.set_final(true);
        bhs.set_initiator_task_tag(req.bhs.initiator_task_tag());
        // Response: 0 = function complete
        bhs.raw[2] = 0;

        let stat_sn = conn.next_stat_sn();
        bhs.set_stat_sn(stat_sn);
        bhs.set_exp_cmd_sn(conn.exp_cmd_sn.load(std::sync::atomic::Ordering::Relaxed));
        bhs.set_max_cmd_sn(conn.max_cmd_sn.load(std::sync::atomic::Ordering::Relaxed));

        let pdu = IscsiPdu::new(bhs);
        write_pdu(writer, &pdu, header_digest, data_digest).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iscsi_config_defaults() {
        let config = IscsiConfig::default();
        assert_eq!(config.listen_addr.port(), 3260);
        assert!(config.chap.is_none());
    }

    #[test]
    fn iscsi_target_add_lun() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let config = IscsiConfig::default();
            let mut target = IscsiTarget::new(config);

            let dir = std::env::temp_dir().join("stormblock-iscsi-test");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("{}.bin", uuid::Uuid::new_v4().simple()));
            let dev = crate::drive::filedev::FileDevice::open_with_capacity(
                path.to_str().unwrap(), 1024 * 1024
            ).await.unwrap();

            target.add_lun(0, Arc::new(dev));
            assert_eq!(target.luns.len(), 1);

            let _ = std::fs::remove_file(&path);
        });
    }
}
