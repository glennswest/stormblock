//! NVMe-oF/TCP target — port 4420, NVMe over TCP transport.
//!
//! Handles ICReq/ICResp handshake, fabric Connect, then admin or I/O commands.

pub mod pdu;
pub mod fabric;
pub mod admin;
pub mod io;
pub mod discovery;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};

use crate::drive::BlockDevice;
use super::reactor::ReactorPool;

use pdu::*;
use fabric::*;

/// NVMe-oF/TCP target configuration.
#[derive(Debug, Clone)]
pub struct NvmeofConfig {
    /// Listen address (default: 0.0.0.0:4420).
    pub listen_addr: SocketAddr,
    /// Subsystem NQN.
    pub nqn: String,
    /// Maximum I/O queues per controller.
    pub max_io_queues: u16,
    /// Queue depth (per queue).
    pub queue_depth: u16,
    /// Maximum H2C data payload per PDU.
    pub maxh2cdata: u32,
}

impl Default for NvmeofConfig {
    fn default() -> Self {
        NvmeofConfig {
            listen_addr: "0.0.0.0:4420".parse().unwrap(),
            nqn: "nqn.2024.io.stormblock:default".into(),
            max_io_queues: 64,
            queue_depth: 128,
            maxh2cdata: 131072,
        }
    }
}

/// NVMe-oF/TCP target server.
pub struct NvmeofTarget {
    config: NvmeofConfig,
    namespaces: Arc<HashMap<u32, Arc<dyn BlockDevice>>>,
    next_cntlid: AtomicU16,
}

impl NvmeofTarget {
    pub fn new(config: NvmeofConfig) -> Self {
        NvmeofTarget {
            config,
            namespaces: Arc::new(HashMap::new()),
            next_cntlid: AtomicU16::new(1),
        }
    }

    /// Add a namespace mapping. Must be called before `run()`.
    pub fn add_namespace(&mut self, nsid: u32, device: Arc<dyn BlockDevice>) {
        Arc::get_mut(&mut self.namespaces)
            .expect("add_namespace after run")
            .insert(nsid, device);
    }

    /// Start accepting connections.
    pub async fn run(self: Arc<Self>, reactor: &ReactorPool) -> std::io::Result<()> {
        let listener = TcpListener::bind(self.config.listen_addr).await?;
        tracing::info!("NVMe-oF/TCP target listening on {} ({})", self.config.listen_addr, self.config.nqn);
        self.run_with_listener(listener, reactor).await
    }

    /// Accept connections on a pre-bound listener. Useful for tests with ephemeral ports.
    pub async fn run_with_listener(self: Arc<Self>, listener: TcpListener, _reactor: &ReactorPool) -> std::io::Result<()> {
        loop {
            let (stream, peer) = listener.accept().await?;
            stream.set_nodelay(true)?;
            let target = self.clone();
            tokio::spawn(async move {
                tracing::debug!("NVMe-oF connection from {peer}");
                if let Err(e) = target.handle_connection(stream, peer).await {
                    tracing::debug!("NVMe-oF connection {peer} closed: {e}");
                }
            });
        }
    }

    async fn handle_connection(&self, stream: TcpStream, peer: SocketAddr) -> std::io::Result<()> {
        let (reader, writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut writer = BufWriter::new(writer);

        // Step 1: ICReq/ICResp handshake
        let (hdgst, ddgst) = self.handle_ic_handshake(&mut reader, &mut writer).await?;

        // Step 2: Fabric Connect → determine admin vs I/O queue
        let (cntlid, qid) = self.handle_fabric_connect(&mut reader, &mut writer, hdgst).await?;
        tracing::info!("NVMe-oF controller {cntlid} connected from {peer}, QID={qid}");

        // Step 3: Command loop
        let mut props = ControllerProperties::new();
        self.command_loop(&mut reader, &mut writer, qid, cntlid, &mut props, hdgst, ddgst).await
    }

    async fn handle_ic_handshake<R, W>(
        &self,
        reader: &mut R,
        writer: &mut W,
    ) -> std::io::Result<(bool, bool)>
    where
        R: AsyncReadExt + Unpin,
        W: AsyncWriteExt + Unpin,
    {
        let pdu = pdu::read_pdu(reader).await?;
        let icreq = match pdu {
            NvmeofPdu::ICReq(_, req) => req,
            _ => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "expected ICReq")),
        };

        if icreq.pfv != 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "unsupported PFV"));
        }

        // Negotiate digests
        let hdgst = icreq.dgst & 0x01 != 0;
        let ddgst = icreq.dgst & 0x02 != 0;

        let resp = ICResp {
            pfv: 0,
            cpda: 0,
            dgst: icreq.dgst, // accept whatever was requested
            maxh2cdata: self.config.maxh2cdata,
        };

        pdu::write_ic_resp(writer, &resp).await?;

        tracing::debug!("NVMe-oF IC handshake complete, hdgst={hdgst}, ddgst={ddgst}");
        Ok((hdgst, ddgst))
    }

    async fn handle_fabric_connect<R, W>(
        &self,
        reader: &mut R,
        writer: &mut W,
        hdgst: bool,
    ) -> std::io::Result<(u16, u16)>
    where
        R: AsyncReadExt + Unpin,
        W: AsyncWriteExt + Unpin,
    {
        let pdu = pdu::read_pdu(reader).await?;
        let (sqe, data) = match pdu {
            NvmeofPdu::CapsuleCmd { sqe, data, .. } => (sqe, data),
            _ => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "expected CapsuleCmd")),
        };

        let fab = FabricCmd::from_sqe(&sqe).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "expected fabric command")
        })?;

        if fab.fctype != FCTYPE_CONNECT {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "expected Connect"));
        }

        let connect = ConnectData::from_bytes(&data).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid Connect data")
        })?;

        // Validate NQN (allow discovery NQN or our subsystem NQN)
        if connect.subnqn != self.config.nqn && connect.subnqn != discovery::DISCOVERY_NQN {
            tracing::warn!("NVMe-oF: unknown subsystem NQN '{}'", connect.subnqn);
            let cqe = NvmeCqe::error(sqe.cid(), 0, 0, 0, 0x02);
            pdu::write_capsule_resp(writer, &cqe, hdgst).await?;
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "unknown NQN"));
        }

        let qid = fab.connect_qid();
        let cntlid = self.next_cntlid.fetch_add(1, Ordering::Relaxed);

        let mut cqe = NvmeCqe::success(sqe.cid(), 0, 0);
        cqe.set_dw0(cntlid as u32); // CNTLID in DW0 of connect response
        pdu::write_capsule_resp(writer, &cqe, hdgst).await?;

        tracing::debug!("NVMe-oF Connect: host='{}', sub='{}', qid={qid}, cntlid={cntlid}", connect.hostnqn, connect.subnqn);
        Ok((cntlid, qid))
    }

    async fn command_loop<R, W>(
        &self,
        reader: &mut R,
        writer: &mut W,
        qid: u16,
        cntlid: u16,
        props: &mut ControllerProperties,
        hdgst: bool,
        ddgst: bool,
    ) -> std::io::Result<()>
    where
        R: AsyncReadExt + Unpin,
        W: AsyncWriteExt + Unpin,
    {
        loop {
            let pdu = pdu::read_pdu(reader).await?;
            match pdu {
                NvmeofPdu::CapsuleCmd { sqe, data, .. } => {
                    let opcode = sqe.opcode();
                    let cid = sqe.cid();

                    if opcode == NVME_FABRIC_OPC {
                        self.handle_fabric_cmd(&sqe, &data, writer, props, hdgst).await?;
                    } else if qid == 0 {
                        // Admin queue
                        self.handle_admin_cmd(&sqe, writer, cntlid, hdgst, ddgst).await?;
                    } else {
                        // I/O queue
                        self.handle_io_cmd(&sqe, &data, writer, cid, hdgst, ddgst).await?;
                    }
                }
                NvmeofPdu::H2CData { cccid, data, .. } => {
                    // H2C data for a pending write — simplified: not yet tracked
                    tracing::debug!("received H2CData for CID {cccid}, {} bytes", data.len());
                }
                _ => {
                    tracing::debug!("ignoring unexpected PDU in command loop");
                }
            }
        }
    }

    async fn handle_fabric_cmd<W: AsyncWriteExt + Unpin>(
        &self,
        sqe: &NvmeSqe,
        _data: &[u8],
        writer: &mut W,
        props: &mut ControllerProperties,
        hdgst: bool,
    ) -> std::io::Result<()> {
        let fab = FabricCmd::from_sqe(sqe).unwrap();
        let cid = sqe.cid();

        match fab.fctype {
            FCTYPE_PROPERTY_GET => {
                let offset = fab.property_offset();
                let prop = NvmeProperty::from_offset(offset);
                let val = match prop {
                    Some(p) => props.get_property(p),
                    None => 0,
                };
                let mut cqe = NvmeCqe::success(cid, 0, 0);
                cqe.set_dw0(val as u32);
                // For 64-bit properties, DW1 holds upper 32 bits
                if fab.property_size_64() {
                    cqe.raw[4..8].copy_from_slice(&((val >> 32) as u32).to_le_bytes());
                }
                pdu::write_capsule_resp(writer, &cqe, hdgst).await
            }
            FCTYPE_PROPERTY_SET => {
                let offset = fab.property_offset();
                let val = sqe.cdw12() as u64 | ((sqe.cdw13() as u64) << 32);
                if let Some(prop) = NvmeProperty::from_offset(offset) {
                    props.set_property(prop, val);
                }
                let cqe = NvmeCqe::success(cid, 0, 0);
                pdu::write_capsule_resp(writer, &cqe, hdgst).await
            }
            _ => {
                tracing::debug!("unsupported fabric fctype: {}", fab.fctype);
                let cqe = NvmeCqe::error(cid, 0, 0, 0, 0x01);
                pdu::write_capsule_resp(writer, &cqe, hdgst).await
            }
        }
    }

    async fn handle_admin_cmd<W: AsyncWriteExt + Unpin>(
        &self,
        sqe: &NvmeSqe,
        writer: &mut W,
        cntlid: u16,
        hdgst: bool,
        ddgst: bool,
    ) -> std::io::Result<()> {
        let opcode = sqe.opcode();
        let cid = sqe.cid();

        match opcode {
            admin::ADMIN_IDENTIFY => {
                let cns = (sqe.cdw10() & 0xFF) as u8;
                let nsid = sqe.nsid();

                let data = match cns {
                    admin::CNS_CONTROLLER => {
                        let serial = format!("SB{cntlid:04X}");
                        let mut d = admin::identify_controller(
                            &self.config.nqn,
                            &serial,
                            "StormBlock NVMe-oF",
                            "1.0.0",
                            self.namespaces.len() as u32,
                        );
                        // Set CNTLID
                        d[78..80].copy_from_slice(&cntlid.to_le_bytes());
                        d
                    }
                    admin::CNS_NAMESPACE => {
                        match self.namespaces.get(&nsid) {
                            Some(dev) => admin::identify_namespace(dev),
                            None => {
                                let cqe = NvmeCqe::error(cid, 0, 0, 0, 0x0B); // NS Not Ready
                                return pdu::write_capsule_resp(writer, &cqe, hdgst).await;
                            }
                        }
                    }
                    admin::CNS_ACTIVE_NS_LIST => {
                        let mut nsids: Vec<u32> = self.namespaces.keys().copied().collect();
                        nsids.sort();
                        admin::active_ns_list(&nsids)
                    }
                    _ => {
                        let cqe = NvmeCqe::error(cid, 0, 0, 0, 0x02);
                        return pdu::write_capsule_resp(writer, &cqe, hdgst).await;
                    }
                };

                // Send identify data via C2HData PDU
                pdu::write_c2h_data(writer, cid, 0, &data, true, true, hdgst, ddgst).await?;
                Ok(())
            }
            admin::ADMIN_GET_LOG_PAGE => {
                let lid = (sqe.cdw10() & 0xFF) as u8;
                let numd = ((sqe.cdw10() >> 16) as u32 | ((sqe.cdw11() & 0xFFFF) << 16)) + 1;
                let log_bytes = numd as usize * 4;

                // Log page 0x70 = Discovery Log Page
                let data = if lid == 0x70 {
                    let entries = vec![discovery::DiscoveryEntry {
                        subnqn: self.config.nqn.clone(),
                        traddr: self.config.listen_addr,
                        portid: 1,
                        cntlid: 0xFFFF,
                        subsys_type: discovery::SubsysType::NvmeSubsystem,
                    }];
                    let mut log = discovery::build_discovery_log_page(&entries);
                    log.truncate(log_bytes);
                    // Pad if needed
                    log.resize(log_bytes, 0);
                    log
                } else {
                    // Return empty log for unknown pages
                    vec![0u8; log_bytes]
                };

                pdu::write_c2h_data(writer, cid, 0, &data, true, true, hdgst, ddgst).await?;
                Ok(())
            }
            admin::ADMIN_SET_FEATURES | admin::ADMIN_GET_FEATURES => {
                // Minimal: just ack
                let cqe = NvmeCqe::success(cid, 0, 0);
                pdu::write_capsule_resp(writer, &cqe, hdgst).await
            }
            admin::ADMIN_ASYNC_EVENT_REQ => {
                // Don't respond immediately — async event requests are held
                Ok(())
            }
            _ => {
                tracing::debug!("unsupported admin opcode: {opcode:#04x}");
                let cqe = NvmeCqe::error(cid, 0, 0, 0, 0x01);
                pdu::write_capsule_resp(writer, &cqe, hdgst).await
            }
        }
    }

    async fn handle_io_cmd<W: AsyncWriteExt + Unpin>(
        &self,
        sqe: &NvmeSqe,
        data: &[u8],
        writer: &mut W,
        cid: u16,
        hdgst: bool,
        ddgst: bool,
    ) -> std::io::Result<()> {
        let nsid = sqe.nsid();
        let device = match self.namespaces.get(&nsid) {
            Some(dev) => dev,
            None => {
                let cqe = NvmeCqe::error(cid, 0, 0, 0, 0x0B);
                return pdu::write_capsule_resp(writer, &cqe, hdgst).await;
            }
        };

        let result = io::handle_io_command(sqe, device, data).await;

        if !result.data.is_empty() {
            // Send read data via C2HData PDU(s)
            let max_seg = self.config.maxh2cdata as usize;
            let chunks: Vec<&[u8]> = result.data.chunks(max_seg).collect();
            let last_idx = chunks.len() - 1;

            for (i, chunk) in chunks.iter().enumerate() {
                let is_last = i == last_idx;
                let offset = (i * max_seg) as u32;
                pdu::write_c2h_data(
                    writer, cid, offset, chunk,
                    is_last, is_last, // last + success on final chunk
                    hdgst, ddgst,
                ).await?;
            }
        } else {
            // No data — send CapsuleResp
            pdu::write_capsule_resp(writer, &result.cqe, hdgst).await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvmeof_config_defaults() {
        let config = NvmeofConfig::default();
        assert_eq!(config.listen_addr.port(), 4420);
        assert_eq!(config.maxh2cdata, 131072);
    }

    #[test]
    fn nvmeof_target_add_namespace() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let config = NvmeofConfig::default();
            let mut target = NvmeofTarget::new(config);

            let dir = std::env::temp_dir().join("stormblock-nvmeof-test");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("{}.bin", uuid::Uuid::new_v4().simple()));
            let dev = crate::drive::filedev::FileDevice::open_with_capacity(
                path.to_str().unwrap(), 1024 * 1024
            ).await.unwrap();

            target.add_namespace(1, Arc::new(dev));
            assert_eq!(target.namespaces.len(), 1);

            let _ = std::fs::remove_file(&path);
        });
    }
}
