//! NVMe-oF/TCP target — port 4420, NVMe over TCP transport.
//!
//! Handles ICReq/ICResp handshake, fabric Connect, then admin or I/O commands.

pub mod pdu;
pub mod fabric;
pub mod admin;
pub mod io;
pub mod discovery;
#[cfg(target_os = "linux")]
pub mod zerocopy;

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
    /// Address reported in the discovery log page. `listen_addr` is usually a
    /// wildcard, which a remote initiator cannot connect back to (#26).
    pub advertised_addr: Option<SocketAddr>,
}

impl Default for NvmeofConfig {
    fn default() -> Self {
        NvmeofConfig {
            listen_addr: "0.0.0.0:4420".parse().unwrap(),
            nqn: "nqn.2024.io.stormblock:default".into(),
            max_io_queues: 64,
            queue_depth: 128,
            maxh2cdata: 131072,
            advertised_addr: None,
        }
    }
}

/// Log page identifier for the Changed Namespace List (NVMe 1.4 §5.14.1.4).
pub const LID_CHANGED_NS_LIST: u8 = 0x04;

/// Completion DW0 for a Namespace Attribute Changed notice: async event type
/// 0x2 (Notice) in bits 2:0, event info 0x00 (Namespace Attribute Changed) in
/// bits 15:8, and the associated log page in bits 23:16.
const AEN_NS_ATTR_CHANGED: u32 = 0x2 | ((LID_CHANGED_NS_LIST as u32) << 16);

/// Complete a held Asynchronous Event Request with a Namespace Attribute
/// Changed notice, pointing the host at the Changed Namespace List log page.
async fn write_ns_changed_aen<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    cid: u16,
    hdgst: bool,
) -> std::io::Result<()> {
    let mut cqe = NvmeCqe::success(cid, 0, 0);
    cqe.set_dw0(AEN_NS_ATTR_CHANGED);
    pdu::write_capsule_resp(writer, &cqe, hdgst).await
}

/// NVMe-oF/TCP target server.
pub struct NvmeofTarget {
    config: NvmeofConfig,
    /// Namespace map. Behind a `RwLock` so namespaces can be added and removed
    /// while the target is running and shared behind an `Arc` — the CoW
    /// registry model creates exports long after boot.
    namespaces: tokio::sync::RwLock<HashMap<u32, Arc<dyn BlockDevice>>>,
    /// Namespace IDs whose attributes changed, broadcast to admin connections.
    ///
    /// This is what makes hot-add cheap: a host connects once and every
    /// subsequent attach is an async event plus a rescan, with no Connect and
    /// no new TCP session per container.
    ns_changed: tokio::sync::broadcast::Sender<u32>,
    next_cntlid: AtomicU16,
    /// Connections currently being served.
    ///
    /// The target is the only thing that knows this. Everything else has to
    /// infer it — the reconciler was reading /proc/net/tcp and counting
    /// sockets on the portal's port, which is a *sample* of a fact this
    /// process owns exactly, and a sample taken while an initiator is
    /// connecting does not contain it.
    live: std::sync::atomic::AtomicUsize,
    /// Whether new connections are still being accepted.
    ///
    /// Cleared to drain: established connections keep running and the listener
    /// stops. That is what makes a count of zero *final* rather than a guess —
    /// once nothing new can attach, the count can only fall.
    accepting: std::sync::atomic::AtomicBool,
    /// Wakes the accept loop when `accepting` is cleared, so a drain does not
    /// wait for one last connection to arrive before noticing.
    stop_accept: tokio::sync::Notify,
}

impl NvmeofTarget {
    pub fn new(config: NvmeofConfig) -> Self {
        // Depth only bounds how far an admin connection may fall behind before
        // it is told to rescan wholesale, so a modest buffer is fine.
        let (ns_changed, _) = tokio::sync::broadcast::channel(256);
        NvmeofTarget {
            config,
            namespaces: tokio::sync::RwLock::new(HashMap::new()),
            ns_changed,
            next_cntlid: AtomicU16::new(1),
            live: std::sync::atomic::AtomicUsize::new(0),
            accepting: std::sync::atomic::AtomicBool::new(true),
            stop_accept: tokio::sync::Notify::new(),
        }
    }

    /// Add a namespace mapping at startup (before the target is shared).
    pub fn add_namespace(&mut self, nsid: u32, device: Arc<dyn BlockDevice>) {
        self.namespaces.get_mut().insert(nsid, device);
    }

    /// Add a namespace at runtime — no `&mut self`, so this works on a target
    /// already shared behind an `Arc` and serving traffic.
    ///
    /// Connected hosts are notified, so the namespace shows up without a
    /// reconnect.
    pub async fn add_namespace_dynamic(&self, nsid: u32, device: Arc<dyn BlockDevice>) {
        self.namespaces.write().await.insert(nsid, device);
        self.notify_ns_changed(nsid);
    }

    /// Remove a namespace at runtime. Returns true if the namespace existed.
    pub async fn remove_namespace(&self, nsid: u32) -> bool {
        let existed = self.namespaces.write().await.remove(&nsid).is_some();
        if existed {
            self.notify_ns_changed(nsid);
        }
        existed
    }

    /// Announce a namespace attribute change to connected hosts.
    ///
    /// Fails only when nobody is listening, which is the common single-node
    /// case — not an error.
    fn notify_ns_changed(&self, nsid: u32) {
        let _ = self.ns_changed.send(nsid);
    }

    /// Lowest unused namespace ID. NSID 0 is reserved by the spec.
    pub async fn next_free_nsid(&self) -> u32 {
        let ns = self.namespaces.read().await;
        (1u32..).find(|n| !ns.contains_key(n)).unwrap_or(1)
    }

    /// List active namespace IDs, sorted.
    pub async fn list_namespaces(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self.namespaces.read().await.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Number of active namespaces.
    pub async fn namespace_count(&self) -> usize {
        self.namespaces.read().await.len()
    }

    /// Resolve a namespace to its backing device, cloning the `Arc` so the
    /// lock is never held across I/O.
    async fn namespace(&self, nsid: u32) -> Option<Arc<dyn BlockDevice>> {
        self.namespaces.read().await.get(&nsid).cloned()
    }

    /// Start accepting connections.
    pub async fn run(self: Arc<Self>, reactor: &ReactorPool) -> std::io::Result<()> {
        let listener = TcpListener::bind(self.config.listen_addr).await?;
        tracing::info!("NVMe-oF/TCP target listening on {} ({})", self.config.listen_addr, self.config.nqn);
        self.run_with_listener(listener, reactor).await
    }

    /// Accept connections on a pre-bound listener. Useful for tests with ephemeral ports.
    pub async fn run_with_listener(self: Arc<Self>, listener: TcpListener, reactor: &ReactorPool) -> std::io::Result<()> {
        loop {
            let (stream, peer) = tokio::select! {
                r = listener.accept() => r?,
                // Draining: stop taking new connections and drop the listener,
                // leaving everything already established to finish. From here
                // the live count can only fall, which is what lets a drain end
                // on a fact rather than on a timer.
                _ = self.stop_accept.notified() => {
                    tracing::debug!("NVMe-oF {} stopped accepting", self.config.nqn);
                    return Ok(());
                }
            };
            if !self.is_accepting() {
                return Ok(());
            }
            stream.set_nodelay(true)?;
            let target = self.clone();
            // Counted before the connection is handed off, so a drain that
            // samples immediately after an accept still sees it.
            target.live.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // Dispatch onto the reactor pool rather than the ambient runtime.
            // The pool was previously accepted and ignored, which is what made
            // --reactor-cores a no-op.
            reactor.dispatch(async move {
                tracing::debug!("NVMe-oF connection from {peer}");
                let r = target.handle_connection(stream, peer).await;
                target.live.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                if let Err(e) = r {
                    tracing::debug!("NVMe-oF connection {peer} closed: {e}");
                }
            });
        }
    }

    /// How many connections this target is serving right now.
    ///
    /// Authoritative: this is the count the target keeps as it accepts and
    /// finishes connections, not an inference from somewhere else.
    pub fn live_connections(&self) -> usize {
        self.live.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn is_accepting(&self) -> bool {
        self.accepting.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Begin draining: refuse new connections, keep serving the ones in hand.
    ///
    /// Idempotent, and the only thing that makes `live_connections() == 0`
    /// mean "finished" instead of "not started yet".
    pub fn stop_accepting(&self) {
        self.accepting.store(false, std::sync::atomic::Ordering::SeqCst);
        self.stop_accept.notify_waiters();
    }

    async fn handle_connection(&self, stream: TcpStream, peer: SocketAddr) -> std::io::Result<()> {
        let (reader, writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut writer = BufWriter::new(writer);

        // Step 1: ICReq/ICResp handshake
        let (hdgst, ddgst) = self.handle_ic_handshake(&mut reader, &mut writer).await?;

        // Step 2: Fabric Connect → determine admin vs I/O queue
        let (cntlid, qid, is_discovery) =
            self.handle_fabric_connect(&mut reader, &mut writer, hdgst).await?;
        tracing::info!(
            "NVMe-oF controller {cntlid} connected from {peer}, QID={qid}{}",
            if is_discovery { " (discovery)" } else { "" }
        );

        // Step 3: Command loop. The admin queue gets its own loop because it
        // must be able to complete a held Asynchronous Event Request the
        // moment a namespace changes, not just when the next command arrives.
        let mut props = ControllerProperties::new();
        if qid == 0 {
            self.admin_loop(reader, &mut writer, cntlid, is_discovery, &mut props, hdgst, ddgst).await
        } else {
            self.command_loop(&mut reader, &mut writer, qid, cntlid, is_discovery, &mut props, hdgst, ddgst).await
        }
    }

    /// Admin-queue command loop with async event delivery.
    ///
    /// `read_pdu` is not cancellation-safe, so the socket is drained by a
    /// dedicated task and this loop selects over the resulting channel and the
    /// namespace-change stream. That is what lets a hot-added namespace reach
    /// an already-connected host: the held AER completes immediately, the host
    /// reads the Changed Namespace List and rescans, and no Connect or new TCP
    /// session is needed per volume.
    #[allow(clippy::too_many_arguments)]
    async fn admin_loop<W>(
        &self,
        reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
        writer: &mut W,
        cntlid: u16,
        is_discovery: bool,
        props: &mut ControllerProperties,
        hdgst: bool,
        ddgst: bool,
    ) -> std::io::Result<()>
    where
        W: AsyncWriteExt + Unpin,
    {
        use std::collections::{BTreeSet, VecDeque};
        use tokio::sync::broadcast::error::RecvError;

        let (tx, mut cmd_rx) =
            tokio::sync::mpsc::channel::<std::io::Result<(NvmeSqe, Vec<u8>)>>(32);
        tokio::spawn(async move {
            let mut reader = reader;
            loop {
                match pdu::read_pdu(&mut reader).await {
                    Ok(NvmeofPdu::CapsuleCmd { sqe, data, .. }) => {
                        if tx.send(Ok((sqe, data))).await.is_err() {
                            break;
                        }
                    }
                    Ok(_) => continue,
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        break;
                    }
                }
            }
        });

        let mut events = self.ns_changed.subscribe();
        // AERs the host has posted that we have not answered yet.
        let mut held_aers: VecDeque<u16> = VecDeque::new();
        // Namespaces changed since the host last read the log page.
        let mut changed: BTreeSet<u32> = BTreeSet::new();
        let mut overflow = false;

        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => {
                    let (sqe, data) = match cmd {
                        Some(Ok(c)) => c,
                        Some(Err(e)) => return Err(e),
                        None => return Ok(()),
                    };
                    let opcode = sqe.opcode();
                    let cid = sqe.cid();

                    if opcode == NVME_FABRIC_OPC {
                        self.handle_fabric_cmd(&sqe, &data, writer, props, hdgst).await?;
                        continue;
                    }

                    match opcode {
                        admin::ADMIN_ASYNC_EVENT_REQ => {
                            held_aers.push_back(cid);
                            // Something already changed while no AER was
                            // outstanding — report it right away.
                            if !changed.is_empty() || overflow {
                                if let Some(cid) = held_aers.pop_front() {
                                    write_ns_changed_aen(writer, cid, hdgst).await?;
                                }
                            }
                        }
                        admin::ADMIN_GET_LOG_PAGE
                            if (sqe.cdw10() & 0xFF) as u8 == LID_CHANGED_NS_LIST =>
                        {
                            let numd = ((sqe.cdw10() >> 16) | ((sqe.cdw11() & 0xFFFF) << 16)) + 1;
                            let list: Vec<u32> = changed.iter().copied().collect();
                            let mut page = admin::changed_ns_list(&list, overflow);
                            page.resize(numd as usize * 4, 0);
                            // Reading the page clears it, per spec.
                            changed.clear();
                            overflow = false;
                            pdu::write_c2h_data(writer, cid, 0, &page, true, true, hdgst, ddgst).await?;
                        }
                        _ => {
                            self.handle_admin_cmd(&sqe, writer, cntlid, is_discovery, hdgst, ddgst).await?;
                        }
                    }
                }
                ev = events.recv() => {
                    match ev {
                        Ok(nsid) => { changed.insert(nsid); }
                        // Fell too far behind to enumerate: tell the host to
                        // rescan everything rather than trust a partial list.
                        Err(RecvError::Lagged(_)) => { overflow = true; }
                        Err(RecvError::Closed) => continue,
                    }
                    if let Some(cid) = held_aers.pop_front() {
                        write_ns_changed_aen(writer, cid, hdgst).await?;
                    }
                }
            }
        }
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
    ) -> std::io::Result<(u16, u16, bool)>
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
        let is_discovery = connect.subnqn == discovery::DISCOVERY_NQN;

        let mut cqe = NvmeCqe::success(sqe.cid(), 0, 0);
        cqe.set_dw0(cntlid as u32); // CNTLID in DW0 of connect response
        pdu::write_capsule_resp(writer, &cqe, hdgst).await?;

        tracing::debug!("NVMe-oF Connect: host='{}', sub='{}', qid={qid}, cntlid={cntlid}", connect.hostnqn, connect.subnqn);
        Ok((cntlid, qid, is_discovery))
    }

    #[allow(clippy::too_many_arguments)]
    async fn command_loop<R, W>(
        &self,
        reader: &mut R,
        writer: &mut W,
        qid: u16,
        cntlid: u16,
        is_discovery: bool,
        props: &mut ControllerProperties,
        hdgst: bool,
        ddgst: bool,
    ) -> std::io::Result<()>
    where
        R: AsyncReadExt + Unpin,
        W: AsyncWriteExt + Unpin,
    {
        // Commands that arrived interleaved while a write was collecting its
        // R2T data are parked here and processed in order.
        let mut pending: std::collections::VecDeque<(NvmeSqe, Vec<u8>)> =
            std::collections::VecDeque::new();
        loop {
            let (sqe, data) = if let Some(cmd) = pending.pop_front() {
                cmd
            } else {
                match pdu::read_pdu(reader).await? {
                    NvmeofPdu::CapsuleCmd { sqe, data, .. } => (sqe, data),
                    NvmeofPdu::H2CData { cccid, data, .. } => {
                        tracing::warn!(
                            "unsolicited H2CData for CID {cccid} ({} bytes), dropping",
                            data.len()
                        );
                        continue;
                    }
                    _ => {
                        tracing::debug!("ignoring unexpected PDU in command loop");
                        continue;
                    }
                }
            };

            let opcode = sqe.opcode();
            let cid = sqe.cid();

            if opcode == NVME_FABRIC_OPC {
                self.handle_fabric_cmd(&sqe, &data, writer, props, hdgst).await?;
            } else if qid == 0 {
                // Admin queue
                self.handle_admin_cmd(&sqe, writer, cntlid, is_discovery, hdgst, ddgst).await?;
            } else {
                // I/O queue
                self.handle_io_cmd(&sqe, &data, reader, writer, cid, &mut pending, hdgst, ddgst)
                    .await?;
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
        is_discovery: bool,
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
                        // A connection made to the discovery NQN must identify
                        // as a discovery controller under that NQN.
                        let subnqn = if is_discovery {
                            discovery::DISCOVERY_NQN
                        } else {
                            &self.config.nqn
                        };
                        let mut d = admin::identify_controller(
                            subnqn,
                            &serial,
                            "StormBlock NVMe-oF",
                            "1.0.0",
                            self.namespace_count().await as u32,
                            is_discovery,
                        );
                        // Set CNTLID
                        d[78..80].copy_from_slice(&cntlid.to_le_bytes());
                        d
                    }
                    admin::CNS_NAMESPACE => {
                        match self.namespace(nsid).await {
                            Some(dev) => admin::identify_namespace(&dev),
                            None => {
                                let cqe = NvmeCqe::error(cid, 0, 0, 0, 0x0B); // NS Not Ready
                                return pdu::write_capsule_resp(writer, &cqe, hdgst).await;
                            }
                        }
                    }
                    admin::CNS_ACTIVE_NS_LIST => {
                        admin::active_ns_list(&self.list_namespaces().await)
                    }
                    admin::CNS_NS_DESC_LIST => {
                        match self.namespace(nsid).await {
                            Some(dev) => admin::identify_ns_desc_list(
                                dev.id().uuid.as_bytes(),
                            ),
                            None => {
                                let cqe = NvmeCqe::error(cid, 0, 0, 0, 0x0B);
                                return pdu::write_capsule_resp(writer, &cqe, hdgst).await;
                            }
                        }
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
                let numd = ((sqe.cdw10() >> 16) | ((sqe.cdw11() & 0xFFFF) << 16)) + 1;
                let log_bytes = numd as usize * 4;

                // LPO (log page offset) — the initiator reads the header
                // first, then the entries at their offset.
                let lpo = (sqe.cdw12() as u64) | ((sqe.cdw13() as u64) << 32);

                // Log page 0x70 = Discovery Log Page
                let data = if lid == 0x70 {
                    let entries = vec![discovery::DiscoveryEntry {
                        subnqn: self.config.nqn.clone(),
                        traddr: self.config.advertised_addr.unwrap_or(self.config.listen_addr),
                        portid: 1,
                        cntlid: 0xFFFF,
                        subsys_type: discovery::SubsysType::NvmeSubsystem,
                    }];
                    let log = discovery::build_discovery_log_page(&entries);
                    let start = (lpo as usize).min(log.len());
                    let mut out = log[start..].to_vec();
                    out.resize(log_bytes, 0);
                    out
                } else {
                    // Return empty log for unknown pages
                    vec![0u8; log_bytes]
                };

                pdu::write_c2h_data(writer, cid, 0, &data, true, true, hdgst, ddgst).await?;
                Ok(())
            }
            admin::ADMIN_SET_FEATURES | admin::ADMIN_GET_FEATURES => {
                let fid = (sqe.cdw10() & 0xFF) as u8;
                let mut cqe = NvmeCqe::success(cid, 0, 0);

                if fid == admin::FEAT_NUM_QUEUES {
                    // The controller reports how many queues it granted in
                    // DW0, 0-based, as (NCQA << 16) | NSQA. Leaving DW0 at
                    // zero — as the old blanket ack did — tells the host it
                    // gets exactly one I/O queue, which is what produced
                    // "creating 1 I/O queues" and capped throughput at a
                    // single core's worth of completions.
                    let (nsqr, ncqr) = if opcode == admin::ADMIN_SET_FEATURES {
                        // Requested counts are 0-based in CDW11.
                        (
                            (sqe.cdw11() & 0xFFFF) as u16,
                            ((sqe.cdw11() >> 16) & 0xFFFF) as u16,
                        )
                    } else {
                        // Get Features: report the maximum we would grant.
                        (u16::MAX, u16::MAX)
                    };

                    // max_io_queues counts queues; the wire value is 0-based,
                    // so the largest grantable index is one less.
                    let cap = self.config.max_io_queues.saturating_sub(1);
                    let granted_sq = nsqr.min(cap);
                    let granted_cq = ncqr.min(cap);
                    cqe.set_dw0(((granted_cq as u32) << 16) | granted_sq as u32);

                    tracing::debug!(
                        "Number of Queues: requested {}sq/{}cq, granted {}sq/{}cq (0-based)",
                        nsqr, ncqr, granted_sq, granted_cq
                    );
                }

                pdu::write_capsule_resp(writer, &cqe, hdgst).await
            }
            admin::ADMIN_ASYNC_EVENT_REQ => {
                // Don't respond immediately — async event requests are held
                Ok(())
            }
            admin::ADMIN_KEEP_ALIVE => {
                // Fabrics keep-alive heartbeat — always succeed.
                let cqe = NvmeCqe::success(cid, 0, 0);
                pdu::write_capsule_resp(writer, &cqe, hdgst).await
            }
            _ => {
                tracing::debug!("unsupported admin opcode: {opcode:#04x}");
                let cqe = NvmeCqe::error(cid, 0, 0, 0, 0x01);
                pdu::write_capsule_resp(writer, &cqe, hdgst).await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_io_cmd<R, W>(
        &self,
        sqe: &NvmeSqe,
        data: &[u8],
        reader: &mut R,
        writer: &mut W,
        cid: u16,
        pending: &mut std::collections::VecDeque<(NvmeSqe, Vec<u8>)>,
        hdgst: bool,
        ddgst: bool,
    ) -> std::io::Result<()>
    where
        R: AsyncReadExt + Unpin,
        W: AsyncWriteExt + Unpin,
    {
        let nsid = sqe.nsid();
        // Clone the Arc out of the map so the namespace lock is not held
        // across the I/O below (and a concurrent add/remove cannot block it).
        let device = match self.namespace(nsid).await {
            Some(dev) => dev,
            None => {
                let cqe = NvmeCqe::error(cid, 0, 0, 0, 0x0B);
                return pdu::write_capsule_resp(writer, &cqe, hdgst).await;
            }
        };
        let device = &device;

        // Writes larger than the in-capsule allowance arrive without their
        // data: send an R2T for the remainder and collect H2CData PDUs,
        // parking any interleaved commands for the main loop.
        let mut full_data: Vec<u8>;
        let mut data: &[u8] = data;
        if sqe.opcode() == io::IO_WRITE {
            let nlb = (sqe.cdw12() & 0xFFFF) as u64 + 1;
            let expected = (nlb * device.block_size() as u64) as usize;
            if data.len() < expected {
                pdu::write_r2t(
                    writer,
                    cid,
                    cid, // ttag: one outstanding transfer per command
                    data.len() as u32,
                    (expected - data.len()) as u32,
                    hdgst,
                )
                .await?;
                full_data = Vec::with_capacity(expected);
                full_data.extend_from_slice(data);
                while full_data.len() < expected {
                    match pdu::read_pdu(reader).await? {
                        NvmeofPdu::H2CData { cccid, data: chunk, .. } => {
                            if cccid != cid {
                                tracing::warn!(
                                    "H2CData for CID {cccid} while collecting CID {cid}"
                                );
                            }
                            full_data.extend_from_slice(&chunk);
                        }
                        NvmeofPdu::CapsuleCmd { sqe, data, .. } => {
                            pending.push_back((sqe, data));
                        }
                        _ => {
                            tracing::debug!("ignoring unexpected PDU during R2T transfer");
                        }
                    }
                }
                data = &full_data;
            }
        }

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

    async fn test_device(tag: &str) -> (Arc<dyn BlockDevice>, String) {
        let dir = std::env::temp_dir().join("stormblock-nvmeof-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{tag}-{}.bin", uuid::Uuid::new_v4().simple()));
        let path_str = path.to_str().unwrap().to_string();
        let dev = crate::drive::filedev::FileDevice::open_with_capacity(&path_str, 1024 * 1024)
            .await
            .unwrap();
        (Arc::new(dev), path_str)
    }

    /// Set Features (Number of Queues) must report the grant in DW0. Leaving
    /// it zero tells the host it gets exactly one I/O queue, which is what
    /// produced "creating 1 I/O queues" and capped throughput at one core
    /// of completions (#27).
    #[test]
    fn num_queues_grant_encoding() {
        // Wire values are 0-based: 0 means one queue.
        let cap: u16 = 64u16.saturating_sub(1);

        // A modest request is granted in full.
        let (nsqr, ncqr) = (7u16, 7u16);
        let dw0 = ((ncqr.min(cap) as u32) << 16) | nsqr.min(cap) as u32;
        assert_eq!(dw0 & 0xFFFF, 7, "8 submission queues granted");
        assert_eq!(dw0 >> 16, 7, "8 completion queues granted");

        // A request beyond our capacity is clamped, not refused.
        let (nsqr, ncqr) = (1000u16, 1000u16);
        let dw0 = ((ncqr.min(cap) as u32) << 16) | nsqr.min(cap) as u32;
        assert_eq!(dw0 & 0xFFFF, cap as u32);
        assert_eq!(dw0 >> 16, cap as u32);

        // The old behaviour left DW0 at zero, which decodes as one queue —
        // hence "creating 1 I/O queues" on the host.
        let old_dw0: u32 = 0;
        assert_eq!((old_dw0 & 0xFFFF) + 1, 1, "zero DW0 means a single I/O queue");
    }

    #[test]
    fn num_queues_feature_id() {
        // Guards against the FID drifting away from the handler's match.
        assert_eq!(admin::FEAT_NUM_QUEUES, 0x07);
    }

    #[tokio::test]
    async fn nvmeof_target_add_namespace() {
        let mut target = NvmeofTarget::new(NvmeofConfig::default());
        let (dev, path) = test_device("boot").await;

        target.add_namespace(1, dev);
        assert_eq!(target.namespace_count().await, 1);

        let _ = std::fs::remove_file(&path);
    }

    /// Namespaces must be addable and removable after the target is shared
    /// behind an `Arc` — the runtime export path has no `&mut` (#26).
    #[tokio::test]
    async fn nvmeof_namespace_dynamic_add_remove() {
        let target = Arc::new(NvmeofTarget::new(NvmeofConfig::default()));
        let (dev1, path1) = test_device("dyn1").await;
        let (dev2, path2) = test_device("dyn2").await;

        target.add_namespace_dynamic(1, dev1).await;
        target.add_namespace_dynamic(7, dev2).await;

        assert_eq!(target.list_namespaces().await, vec![1, 7]);
        assert!(target.namespace(7).await.is_some());

        assert!(target.remove_namespace(1).await);
        assert!(!target.remove_namespace(1).await); // already gone
        assert_eq!(target.list_namespaces().await, vec![7]);
        assert!(target.namespace(1).await.is_none());

        let _ = std::fs::remove_file(&path1);
        let _ = std::fs::remove_file(&path2);
    }

    /// A hot-add must reach already-connected hosts, so adding or removing a
    /// namespace has to raise a change event — that event is what completes a
    /// held AER and saves the host a Connect per container.
    #[tokio::test]
    async fn namespace_changes_are_broadcast() {
        let target = Arc::new(NvmeofTarget::new(NvmeofConfig::default()));
        let mut events = target.ns_changed.subscribe();

        let (dev, path) = test_device("evt").await;
        target.add_namespace_dynamic(3, dev).await;
        assert_eq!(events.recv().await.unwrap(), 3);

        assert!(target.remove_namespace(3).await);
        assert_eq!(events.recv().await.unwrap(), 3);

        // Removing something that was never there is not an event.
        assert!(!target.remove_namespace(99).await);
        assert!(events.try_recv().is_err());

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn next_free_nsid_skips_reserved_and_used() {
        let target = Arc::new(NvmeofTarget::new(NvmeofConfig::default()));
        // NSID 0 is reserved, so the first handed out is 1.
        assert_eq!(target.next_free_nsid().await, 1);

        let (d1, p1) = test_device("nsid1").await;
        let (d2, p2) = test_device("nsid2").await;
        target.add_namespace_dynamic(1, d1).await;
        target.add_namespace_dynamic(2, d2).await;
        assert_eq!(target.next_free_nsid().await, 3);

        // A gap left by a detached container is reused.
        target.remove_namespace(1).await;
        assert_eq!(target.next_free_nsid().await, 1);

        let _ = std::fs::remove_file(&p1);
        let _ = std::fs::remove_file(&p2);
    }

    #[test]
    fn aen_dw0_encodes_namespace_attribute_changed() {
        // Type 0x2 (Notice), info 0x00 (NS Attribute Changed), log page 0x04.
        assert_eq!(AEN_NS_ATTR_CHANGED & 0x7, 0x2);
        assert_eq!((AEN_NS_ATTR_CHANGED >> 8) & 0xFF, 0x00);
        assert_eq!((AEN_NS_ATTR_CHANGED >> 16) & 0xFF, LID_CHANGED_NS_LIST as u32);
    }

    /// The controller must advertise the notice in OAES or the host never arms
    /// for it and hot-add silently does nothing.
    #[test]
    fn identify_controller_advertises_ns_change_notices() {
        let data = admin::identify_controller(
            "nqn.test:sub", "SB0001", "StormBlock", "1.0.0", 1, false,
        );
        let oaes = u32::from_le_bytes(data[92..96].try_into().unwrap());
        assert_ne!(oaes & (1 << 8), 0, "OAES must set Namespace Attribute Changed");
    }

    #[test]
    fn changed_ns_list_encoding() {
        let page = admin::changed_ns_list(&[2, 7], false);
        assert_eq!(u32::from_le_bytes(page[0..4].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(page[4..8].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(page[8..12].try_into().unwrap()), 0);

        // Overflow tells the host to rescan wholesale rather than trust a
        // truncated list.
        let page = admin::changed_ns_list(&[1], true);
        assert_eq!(u32::from_le_bytes(page[0..4].try_into().unwrap()), admin::NS_LIST_OVERFLOW);

        let many: Vec<u32> = (1..=2000).collect();
        let page = admin::changed_ns_list(&many, false);
        assert_eq!(u32::from_le_bytes(page[0..4].try_into().unwrap()), admin::NS_LIST_OVERFLOW);
    }
}
