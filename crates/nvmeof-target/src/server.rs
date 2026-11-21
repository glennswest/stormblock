//! The portable boot target: one image, served to named hosts (#474).
//!
//! Each host has a subsystem of its own, `<base>:host:<name>`, that admits
//! only that host's NQN (`nqn.2026-09.lo.storm:host-<name>`, what stormbootx
//! and the initramfs present), as forge has done since #210. A claim adds a
//! namespace to it backed by a fresh [`Overlay`] over the image: the clone a
//! claim gets on forge, without a forge. A node claims twice on one boot
//! (stormbootx, then the initramfs), so the two newest claims are kept.
//!
//! What it leaves out from the engine's target, on purpose: DH-HMAC-CHAP (a
//! boot host is handed no secret, firmware could not keep one), ANA,
//! asynchronous events (nothing is hot-added under a host), the reactor pool
//! and storage-NIC binding. Tokio only.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, RwLock};

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};

use crate::discovery::{self, DiscoveryEntry, SubsysType};
use crate::fabric::*;
use crate::identify::{self, *};
use crate::image::Image;
use crate::overlay::Overlay;
use crate::pdu::{self, Digests, Fatal, NvmeCqe, NvmeSqe, NvmeofPdu, ICResp, R2tTransfer};

/// Transient Transport Error: a command whose data failed its digest (#392).
const SC_TRANSIENT_TRANSPORT: u8 = 0x22;

/// I/O opcodes.
const IO_FLUSH: u8 = 0x00;
const IO_WRITE: u8 = 0x01;
const IO_READ: u8 = 0x02;
const IO_WRITE_ZEROES: u8 = 0x08;
const IO_DATASET_MGMT: u8 = 0x09;

/// Connect: the host is not allowed on that subsystem.
const SC_CONNECT_INVALID_HOST: u8 = 0x84;
/// Connect: invalid parameters (an unknown subsystem).
const SC_CONNECT_INVALID_PARAMETERS: u8 = 0x80;

/// The host NQN a boot host presents: stormbootx's and the engine's template
/// (`nqn.2026-09.lo.storm:host-{name}`).
pub fn host_nqn(host: &str) -> String {
    format!("nqn.2026-09.lo.storm:host-{host}")
}

/// Claims a host keeps: the boot's two (stormbootx's and the initramfs's).
const CLAIMS_KEPT: usize = 2;

struct HostSubsystem {
    nqn: String,
    host_nqn: String,
    namespaces: BTreeMap<u32, Arc<Overlay>>,
    next_nsid: u32,
}

/// One claim as the HTTP surface reports it.
#[derive(Debug, Clone)]
pub struct Claimed {
    pub subsystem: String,
    pub host_nqn: String,
    pub nsid: u32,
}

/// The target. Cheap to share: every connection holds an `Arc` of it.
pub struct BootTarget {
    image: Arc<Image>,
    scratch: PathBuf,
    base_nqn: String,
    max_transfer: usize,
    hosts: RwLock<HashMap<String, HostSubsystem>>,
    cntlid: AtomicU16,
}

impl BootTarget {
    /// Serve `image`, overlays under `scratch`, subsystems named under
    /// `base_nqn`. `max_transfer` is the largest command (MDTS), a power of
    /// two from 4096.
    pub fn new(image: Arc<Image>, scratch: PathBuf, base_nqn: &str, max_transfer: usize) -> Arc<BootTarget> {
        let max_transfer = max_transfer.clamp(4096, crate::MAX_TRANSFER_CEILING).next_power_of_two();
        Arc::new(BootTarget {
            image,
            scratch,
            base_nqn: base_nqn.to_string(),
            max_transfer,
            hosts: RwLock::new(HashMap::new()),
            cntlid: AtomicU16::new(1),
        })
    }

    pub fn image(&self) -> &Arc<Image> {
        &self.image
    }

    /// The subsystem a host's claims are served under.
    pub fn subsystem_nqn(&self, host: &str) -> String {
        format!("{}:host:{host}", self.base_nqn)
    }

    /// A new claim for `host`: a namespace over a fresh overlay. The host's
    /// older claims past the boot's two go, and their scratch files with them.
    pub fn claim(&self, host: &str) -> io::Result<Claimed> {
        let mut hosts = self.hosts.write().unwrap_or_else(|e| e.into_inner());
        let sub = hosts.entry(host.to_string()).or_insert_with(|| HostSubsystem {
            nqn: self.subsystem_nqn(host),
            host_nqn: host_nqn(host),
            namespaces: BTreeMap::new(),
            next_nsid: 1,
        });
        let nsid = sub.next_nsid;
        let overlay = Overlay::create(self.image.clone(), &self.scratch)?;
        sub.namespaces.insert(nsid, Arc::new(overlay));
        sub.next_nsid += 1;
        while sub.namespaces.len() > CLAIMS_KEPT {
            let oldest = *sub.namespaces.keys().next().expect("non-empty");
            sub.namespaces.remove(&oldest);
        }
        tracing::info!("{host}: claim served as {} nsid {nsid}", sub.nqn);
        Ok(Claimed { subsystem: sub.nqn.clone(), host_nqn: sub.host_nqn.clone(), nsid })
    }

    /// A claim's overlay, for reporting.
    pub fn overlay(&self, host: &str, nsid: u32) -> Option<Arc<Overlay>> {
        let hosts = self.hosts.read().unwrap_or_else(|e| e.into_inner());
        hosts.get(host)?.namespaces.get(&nsid).cloned()
    }

    /// Every host's claims let go, overlays removed.
    pub fn release(&self, host: &str) {
        self.hosts.write().unwrap_or_else(|e| e.into_inner()).remove(host);
    }

    /// Every host let go: what a stop does, so no scratch file is left.
    pub fn release_all(&self) {
        self.hosts.write().unwrap_or_else(|e| e.into_inner()).clear();
    }

    fn by_subsystem(&self, nqn: &str) -> Option<(String, String)> {
        let hosts = self.hosts.read().unwrap_or_else(|e| e.into_inner());
        hosts.iter().find(|(_, s)| s.nqn == nqn).map(|(h, s)| (h.clone(), s.host_nqn.clone()))
    }

    fn namespace(&self, host: &str, nsid: u32) -> Option<Arc<Overlay>> {
        self.overlay(host, nsid)
    }

    fn nsids(&self, host: &str) -> Vec<u32> {
        let hosts = self.hosts.read().unwrap_or_else(|e| e.into_inner());
        hosts.get(host).map(|s| s.namespaces.keys().copied().collect()).unwrap_or_default()
    }

    fn admitted(&self, host_nqn: &str) -> Vec<String> {
        let hosts = self.hosts.read().unwrap_or_else(|e| e.into_inner());
        let mut v: Vec<String> = hosts.values().filter(|s| s.host_nqn == host_nqn).map(|s| s.nqn.clone()).collect();
        v.sort();
        v
    }

    /// Accept connections on `listener` until it fails.
    pub async fn run(self: Arc<Self>, listener: TcpListener) -> io::Result<()> {
        loop {
            let (stream, peer) = listener.accept().await?;
            let _ = stream.set_nodelay(true);
            let me = self.clone();
            tokio::spawn(async move {
                if let Err(e) = me.connection(stream, peer).await {
                    if e.kind() != io::ErrorKind::UnexpectedEof {
                        tracing::info!("NVMe/TCP connection from {peer} ended: {e}");
                    }
                }
            });
        }
    }

    async fn connection(&self, stream: TcpStream, peer: SocketAddr) -> io::Result<()> {
        let local = stream.local_addr()?;
        let (r, w) = stream.into_split();
        let mut reader = BufReader::new(r);
        let mut writer = BufWriter::new(w);
        let r = self.serve(&mut reader, &mut writer, peer, local).await;
        // A fatal transport error is answered with a C2HTermReq before the
        // connection closes (#392).
        if let Some(f) = r.as_ref().err().and_then(pdu::fatal_of) {
            let _ = pdu::write_c2h_term_req(&mut writer, f).await;
        }
        r
    }

    async fn serve<R, W>(&self, reader: &mut R, writer: &mut W, peer: SocketAddr, local: SocketAddr) -> io::Result<()>
    where
        R: AsyncReadExt + Unpin,
        W: AsyncWriteExt + Unpin,
    {
        // ICReq / ICResp. Digests are taken as asked, as the engine does.
        let icreq = match pdu::read_pdu(reader).await? {
            NvmeofPdu::ICReq(ch, req) => {
                pdu::check_icreq(&ch, &req).map_err(Fatal::into_io)?;
                req
            }
            other => return Err(Fatal::out_of_sequence(&other, "the first PDU is not an ICReq").into_io()),
        };
        let (hdgst, ddgst) = (icreq.dgst & 1 != 0, icreq.dgst & 2 != 0);
        let dg = Digests { header: hdgst, data: ddgst };
        pdu::write_ic_resp(writer, &ICResp { pfv: 0, cpda: 0, dgst: icreq.dgst, maxh2cdata: self.max_transfer as u32 })
            .await?;

        // Fabric Connect.
        let (sqe, data) = match pdu::read_pdu_with(reader, dg).await? {
            NvmeofPdu::CapsuleCmd { sqe, data, data_digest_ok: true, .. } => (sqe, data),
            NvmeofPdu::CapsuleCmd { sqe, .. } => {
                pdu::write_capsule_resp(writer, &NvmeCqe::error(sqe.cid(), 0, 0, 0, SC_TRANSIENT_TRANSPORT), hdgst).await?;
                writer.flush().await?;
                return Err(io::Error::new(io::ErrorKind::InvalidData, "Connect data digest mismatch"));
            }
            other => return Err(Fatal::out_of_sequence(&other, "the PDU after the ICReq is not a Connect").into_io()),
        };
        let fab = FabricCmd::from_sqe(&sqe)
            .filter(|f| f.fctype == FCTYPE_CONNECT)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "expected Connect"))?;
        let connect = ConnectData::from_bytes(&data)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid Connect data"))?;
        let qid = fab.connect_qid();
        let host = if connect.subnqn == discovery::DISCOVERY_NQN {
            None
        } else {
            let refuse = |sc: u8| NvmeCqe::error_dnr(sqe.cid(), 0, 0, 1, sc);
            match self.by_subsystem(&connect.subnqn) {
                None => {
                    tracing::warn!("NVMe/TCP: {peer} ({}) asked for unknown subsystem {}", connect.hostnqn, connect.subnqn);
                    pdu::write_capsule_resp(writer, &refuse(SC_CONNECT_INVALID_PARAMETERS), hdgst).await?;
                    writer.flush().await?;
                    return Err(io::Error::new(io::ErrorKind::NotFound, "unknown subsystem"));
                }
                Some((_, want)) if want != connect.hostnqn => {
                    tracing::warn!("NVMe/TCP: refused {peer}: host {} is not allowed on {}", connect.hostnqn, connect.subnqn);
                    pdu::write_capsule_resp(writer, &refuse(SC_CONNECT_INVALID_HOST), hdgst).await?;
                    writer.flush().await?;
                    return Err(io::Error::new(io::ErrorKind::PermissionDenied, "host not allowed"));
                }
                Some((h, _)) => Some(h),
            }
        };
        let cntlid = if qid == 0 { self.cntlid.fetch_add(1, Ordering::Relaxed).max(1) } else { connect.cntlid };
        let mut cqe = NvmeCqe::success(sqe.cid(), 0, 0);
        cqe.set_dw0(cntlid as u32);
        pdu::write_capsule_resp(writer, &cqe, hdgst).await?;
        writer.flush().await?;
        tracing::info!(
            "NVMe/TCP: {peer} ({}) connected to {}, queue {qid}",
            connect.hostnqn,
            host.as_deref().map(|h| self.subsystem_nqn(h)).unwrap_or_else(|| "discovery".into())
        );

        let mut session = Session { host, hostnqn: connect.hostnqn, local, cntlid, hdgst, ddgst, props: ControllerProperties::new() };
        let mut pending: std::collections::VecDeque<(NvmeSqe, Vec<u8>)> = Default::default();
        loop {
            let (sqe, data) = match pending.pop_front() {
                Some(c) => c,
                None => match pdu::read_pdu_with(reader, dg).await? {
                    NvmeofPdu::CapsuleCmd { sqe, data, data_digest_ok: true, .. } => (sqe, data),
                    NvmeofPdu::CapsuleCmd { sqe, .. } => {
                        pdu::write_capsule_resp(writer, &NvmeCqe::error(sqe.cid(), 0, 0, 0, SC_TRANSIENT_TRANSPORT), hdgst)
                            .await?;
                        writer.flush().await?;
                        continue;
                    }
                    NvmeofPdu::H2CTermReq { fes, .. } => {
                        tracing::info!("NVMe/TCP: {peer} ended the connection (H2CTermReq, status {fes:#04x})");
                        return Ok(());
                    }
                    h2c @ NvmeofPdu::H2CData { .. } => {
                        return Err(Fatal::out_of_sequence(&h2c, "H2CData with no R2T outstanding").into_io());
                    }
                    other => return Err(Fatal::out_of_sequence(&other, "a PDU other than a command").into_io()),
                },
            };
            if sqe.opcode() == NVME_FABRIC_OPC {
                self.fabric(&sqe, writer, &mut session).await?;
            } else if qid == 0 {
                self.admin(&sqe, writer, &session).await?;
            } else {
                self.io(&sqe, &data, reader, writer, &session, &mut pending).await?;
            }
            writer.flush().await?;
        }
    }

    async fn fabric<W: AsyncWriteExt + Unpin>(&self, sqe: &NvmeSqe, w: &mut W, s: &mut Session) -> io::Result<()> {
        let fab = FabricCmd::from_sqe(sqe).expect("a fabric opcode");
        let cid = sqe.cid();
        match fab.fctype {
            FCTYPE_PROPERTY_GET => {
                let val = NvmeProperty::from_offset(fab.property_offset()).map(|p| s.props.get_property(p)).unwrap_or(0);
                let mut cqe = NvmeCqe::success(cid, 0, 0);
                cqe.set_dw0(val as u32);
                if fab.property_size_64() {
                    cqe.raw[4..8].copy_from_slice(&((val >> 32) as u32).to_le_bytes());
                }
                pdu::write_capsule_resp(w, &cqe, s.hdgst).await
            }
            FCTYPE_PROPERTY_SET => {
                if let Some(p) = NvmeProperty::from_offset(fab.property_offset()) {
                    s.props.set_property(p, sqe.cdw12() as u64 | ((sqe.cdw13() as u64) << 32));
                }
                pdu::write_capsule_resp(w, &NvmeCqe::success(cid, 0, 0), s.hdgst).await
            }
            // Authentication and anything else: not offered here.
            _ => pdu::write_capsule_resp(w, &NvmeCqe::error(cid, 0, 0, 0, 0x01), s.hdgst).await,
        }
    }

    async fn admin<W: AsyncWriteExt + Unpin>(&self, sqe: &NvmeSqe, w: &mut W, s: &Session) -> io::Result<()> {
        let cid = sqe.cid();
        let (hdgst, ddgst) = (s.hdgst, s.ddgst);
        let fail = |sc: u8| NvmeCqe::error(cid, 0, 0, 0, sc);
        match sqe.opcode() {
            ADMIN_IDENTIFY => {
                let cns = (sqe.cdw10() & 0xFF) as u8;
                let data = match cns {
                    CNS_CONTROLLER => {
                        let (nqn, nn) = match &s.host {
                            Some(h) => (self.subsystem_nqn(h), 1024),
                            None => (discovery::DISCOVERY_NQN.to_string(), 0),
                        };
                        let mut d = identify::controller(
                            &nqn,
                            &format!("SBS{:04X}", s.cntlid),
                            "StormBlock boot server",
                            env!("CARGO_PKG_VERSION"),
                            nn,
                            s.host.is_none(),
                            mdts_for(self.max_transfer),
                        );
                        d[78..80].copy_from_slice(&s.cntlid.to_le_bytes());
                        d
                    }
                    CNS_NAMESPACE | CNS_NS_DESC_LIST => {
                        let ns = s.host.as_deref().and_then(|h| self.namespace(h, sqe.nsid()));
                        let Some(_) = ns else {
                            return pdu::write_capsule_resp(w, &fail(0x0B), hdgst).await;
                        };
                        let nguid = self.nguid(s, sqe.nsid());
                        if cns == CNS_NAMESPACE {
                            identify::namespace(self.image.blocks(), self.image.block_size(), &nguid, 0)
                        } else {
                            identify::ns_desc_list(&nguid)
                        }
                    }
                    CNS_ACTIVE_NS_LIST => {
                        let from = sqe.nsid();
                        let list: Vec<u32> = s
                            .host
                            .as_deref()
                            .map(|h| self.nsids(h).into_iter().filter(|n| *n > from).collect())
                            .unwrap_or_default();
                        active_ns_list(&list)
                    }
                    _ => return pdu::write_capsule_resp(w, &fail(0x02), hdgst).await,
                };
                pdu::write_c2h_data(w, cid, 0, &data, true, true, hdgst, ddgst).await
            }
            ADMIN_GET_LOG_PAGE => {
                let lid = (sqe.cdw10() & 0xFF) as u8;
                // The length is the host's word: bounded by MDTS before
                // anything is allocated for it (#714).
                let Some(len) = identify::log_page_len(sqe.cdw10(), sqe.cdw11(), self.max_transfer) else {
                    return pdu::write_capsule_resp(w, &NvmeCqe::error_dnr(cid, 0, 0, 0, 0x02), hdgst).await;
                };
                let lpo = usize::try_from(sqe.cdw12() as u64 | ((sqe.cdw13() as u64) << 32)).unwrap_or(usize::MAX);
                let mut out = if lid == 0x70 {
                    // The subsystems the asking host may connect to, at the
                    // address it reached this one on.
                    let entries: Vec<DiscoveryEntry> = self
                        .admitted(&s.hostnqn)
                        .into_iter()
                        .map(|subnqn| DiscoveryEntry {
                            subnqn,
                            traddr: s.local,
                            portid: 1,
                            cntlid: 0xFFFF,
                            subsys_type: SubsysType::NvmeSubsystem,
                        })
                        .collect();
                    let page = discovery::build_discovery_log_page(&entries);
                    page[lpo.min(page.len())..].to_vec()
                } else {
                    Vec::new()
                };
                out.resize(len, 0);
                pdu::write_c2h_data(w, cid, 0, &out, true, true, hdgst, ddgst).await
            }
            ADMIN_SET_FEATURES | ADMIN_GET_FEATURES => {
                let mut cqe = NvmeCqe::success(cid, 0, 0);
                if (sqe.cdw10() & 0xFF) as u8 == FEAT_NUM_QUEUES {
                    // 0-based; up to 8 I/O queues.
                    let (sq, cq) = if sqe.opcode() == ADMIN_SET_FEATURES {
                        ((sqe.cdw11() & 0xFFFF).min(7), (sqe.cdw11() >> 16).min(7))
                    } else {
                        (7, 7)
                    };
                    cqe.set_dw0((cq << 16) | sq);
                }
                pdu::write_capsule_resp(w, &cqe, hdgst).await
            }
            // Held for ever: nothing changes under a host here.
            ADMIN_ASYNC_EVENT_REQ => Ok(()),
            ADMIN_KEEP_ALIVE | ADMIN_ABORT => pdu::write_capsule_resp(w, &NvmeCqe::success(cid, 0, 0), hdgst).await,
            _ => pdu::write_capsule_resp(w, &fail(0x01), hdgst).await,
        }
    }

    /// A namespace's NGUID: the image's, told apart by host and claim so two
    /// claims are two disks to a host that sees both.
    fn nguid(&self, s: &Session, nsid: u32) -> [u8; 16] {
        let mut g = self.image.nguid();
        let tag = format!("{}#{nsid}", s.host.as_deref().unwrap_or(""));
        for (i, b) in tag.bytes().enumerate() {
            g[i % 16] ^= b.rotate_left(i as u32 % 8);
        }
        g
    }

    async fn io<R, W>(
        &self,
        sqe: &NvmeSqe,
        data: &[u8],
        r: &mut R,
        w: &mut W,
        s: &Session,
        pending: &mut std::collections::VecDeque<(NvmeSqe, Vec<u8>)>,
    ) -> io::Result<()>
    where
        R: AsyncReadExt + Unpin,
        W: AsyncWriteExt + Unpin,
    {
        let cid = sqe.cid();
        let (hdgst, ddgst) = (s.hdgst, s.ddgst);
        let Some(ns) = s.host.as_deref().and_then(|h| self.namespace(h, sqe.nsid())) else {
            return pdu::write_capsule_resp(w, &NvmeCqe::error(cid, 0, 0, 0, 0x0B), hdgst).await;
        };
        let bs = self.image.block_size() as u64;
        let slba = ((sqe.cdw11() as u64) << 32) | sqe.cdw10() as u64;
        let nlb = (sqe.cdw12() & 0xFFFF) as u64 + 1;
        let out_of_range = |slba: u64, nlb: u64| slba.checked_add(nlb).is_none_or(|e| e > self.image.blocks());
        let opcode = sqe.opcode();
        if matches!(opcode, IO_READ | IO_WRITE) && (nlb * bs) as usize > self.max_transfer {
            return pdu::write_capsule_resp(w, &NvmeCqe::error_dnr(cid, 0, 0, 0, 0x02), hdgst).await;
        }
        if matches!(opcode, IO_READ | IO_WRITE | IO_WRITE_ZEROES) && out_of_range(slba, nlb) {
            return pdu::write_capsule_resp(w, &NvmeCqe::error(cid, 0, 0, 0, 0x80), hdgst).await;
        }
        let done = |r: io::Result<()>, what: &str| match r {
            Ok(()) => NvmeCqe::success(cid, 0, 0),
            Err(e) => {
                tracing::error!("{what} at LBA {slba}: {e}");
                NvmeCqe::error(cid, 0, 0, 2, if what == "read" { 0x81 } else { 0x80 })
            }
        };
        match opcode {
            IO_READ => {
                let len = (nlb * bs) as usize;
                let res = tokio::task::spawn_blocking(move || {
                    let mut buf = vec![0u8; len];
                    ns.read(&mut buf, slba * bs).map(|()| buf)
                })
                .await
                .map_err(io::Error::other)?;
                match res {
                    Ok(buf) => {
                        let seg = self.max_transfer;
                        let n = buf.len().div_ceil(seg);
                        for (i, chunk) in buf.chunks(seg).enumerate() {
                            let last = i + 1 == n;
                            pdu::write_c2h_data(w, cid, (i * seg) as u32, chunk, last, last, hdgst, ddgst).await?;
                        }
                        Ok(())
                    }
                    Err(e) => pdu::write_capsule_resp(w, &done(Err(e), "read"), hdgst).await,
                }
            }
            IO_WRITE => {
                // Data the capsule did not carry is asked for with an R2T
                // (stormbootx never sends a write in-capsule).
                let expected = (nlb * bs) as usize;
                let mut full = data.to_vec();
                if full.len() < expected {
                    let (r2to, r2tl) = (full.len() as u32, (expected - full.len()) as u32);
                    pdu::write_r2t(w, cid, cid, r2to, r2tl, hdgst).await?;
                    w.flush().await?;
                    // Every H2CData PDU is this R2T's, in order (#392).
                    let mut r2t = R2tTransfer::new(cid, cid, r2to, r2tl, self.max_transfer as u32);
                    loop {
                        match pdu::read_pdu_with(r, Digests { header: hdgst, data: ddgst }).await? {
                            NvmeofPdu::H2CData { header, cccid, ttag, datao, datal, data: chunk, last, data_digest_ok } => {
                                let complete =
                                    r2t.take(&header, cccid, ttag, datao, datal, last, data_digest_ok).map_err(Fatal::into_io)?;
                                full.extend_from_slice(&chunk);
                                if complete {
                                    break;
                                }
                            }
                            NvmeofPdu::CapsuleCmd { sqe, data, data_digest_ok: true, .. } => pending.push_back((sqe, data)),
                            NvmeofPdu::CapsuleCmd { sqe, .. } => {
                                pdu::write_capsule_resp(w, &NvmeCqe::error(sqe.cid(), 0, 0, 0, SC_TRANSIENT_TRANSPORT), hdgst).await?;
                            }
                            other => {
                                return Err(Fatal::out_of_sequence(&other, "a PDU other than H2CData or a command during an R2T").into_io())
                            }
                        }
                    }
                    if r2t.digest_bad() {
                        return pdu::write_capsule_resp(w, &NvmeCqe::error(cid, 0, 0, 0, SC_TRANSIENT_TRANSPORT), hdgst).await;
                    }
                }
                full.truncate(expected);
                let res = tokio::task::spawn_blocking(move || ns.write(&full, slba * bs)).await.map_err(io::Error::other)?;
                pdu::write_capsule_resp(w, &done(res, "write"), hdgst).await
            }
            IO_FLUSH => {
                let res = tokio::task::spawn_blocking(move || ns.flush()).await.map_err(io::Error::other)?;
                pdu::write_capsule_resp(w, &done(res, "flush"), hdgst).await
            }
            IO_WRITE_ZEROES => {
                let res = tokio::task::spawn_blocking(move || ns.zero(slba, nlb)).await.map_err(io::Error::other)?;
                pdu::write_capsule_resp(w, &done(res, "write zeroes"), hdgst).await
            }
            IO_DATASET_MGMT => {
                // Deallocate (CDW11 bit 2): the ranges read as zeros after.
                let ranges = (sqe.cdw10() & 0xFF) as usize + 1;
                let mut res = Ok(());
                if sqe.cdw11() & (1 << 2) != 0 {
                    for rg in data.chunks_exact(16).take(ranges) {
                        let n = u32::from_le_bytes(rg[4..8].try_into().unwrap()) as u64;
                        let lba = u64::from_le_bytes(rg[8..16].try_into().unwrap());
                        if n == 0 || out_of_range(lba, n) {
                            continue;
                        }
                        let ns = ns.clone();
                        res = tokio::task::spawn_blocking(move || ns.zero(lba, n)).await.map_err(io::Error::other)?;
                        if res.is_err() {
                            break;
                        }
                    }
                }
                pdu::write_capsule_resp(w, &done(res, "deallocate"), hdgst).await
            }
            _ => pdu::write_capsule_resp(w, &NvmeCqe::error(cid, 0, 0, 0, 0x01), hdgst).await,
        }
    }
}

struct Session {
    /// The host this connection belongs to; `None` on the discovery controller.
    host: Option<String>,
    hostnqn: String,
    /// The address the host reached: what discovery hands back.
    local: SocketAddr,
    cntlid: u16,
    hdgst: bool,
    ddgst: bool,
    props: ControllerProperties,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(t: u8, hlen: u8, pdo: u8, plen: u32) -> [u8; 8] {
        pdu::CommonHeader { pdu_type: t, flags: 0, hlen, pdo, plen }.to_bytes()
    }

    fn sqe(opcode: u8, cid: u16, cdw10: u32, cdw11: u32) -> [u8; 64] {
        let mut s = [0u8; 64];
        s[0] = opcode;
        s[2..4].copy_from_slice(&cid.to_le_bytes());
        s[40..44].copy_from_slice(&cdw10.to_le_bytes());
        s[44..48].copy_from_slice(&cdw11.to_le_bytes());
        s
    }

    /// A host on the discovery controller, no digests: ICReq, then a
    /// Connect with no registered host behind it.
    async fn discovery_connection(addr: SocketAddr) -> TcpStream {
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(&header(0x00, 128, 0, 128)).await.unwrap();
        c.write_all(&[0u8; 120]).await.unwrap();
        let mut icresp = [0u8; 128];
        c.read_exact(&mut icresp).await.unwrap();
        assert_eq!(icresp[0], 0x01, "ICResp");

        let mut connect = sqe(NVME_FABRIC_OPC, 1, 0, 0);
        connect[4] = FCTYPE_CONNECT;
        let mut data = vec![0u8; 1024];
        data[16..18].copy_from_slice(&0xFFFFu16.to_le_bytes());
        let sub = discovery::DISCOVERY_NQN.as_bytes();
        data[256..256 + sub.len()].copy_from_slice(sub);
        let host = b"nqn.2014-08.org.nvmexpress:uuid:nobody";
        data[512..512 + host.len()].copy_from_slice(host);
        c.write_all(&header(0x04, 72, 72, 72 + 1024)).await.unwrap();
        c.write_all(&connect).await.unwrap();
        c.write_all(&data).await.unwrap();
        let mut resp = [0u8; 24];
        c.read_exact(&mut resp).await.unwrap();
        assert_eq!(resp[0], 0x05, "CapsuleResp");
        assert_eq!(u16::from_le_bytes([resp[22], resp[23]]) >> 1, 0, "Connect succeeded");
        c
    }

    async fn get_log_page(c: &mut TcpStream, cid: u16, cdw10: u32, cdw11: u32) {
        c.write_all(&header(0x04, 72, 0, 72)).await.unwrap();
        c.write_all(&sqe(ADMIN_GET_LOG_PAGE, cid, cdw10, cdw11)).await.unwrap();
    }

    /// The status a CapsuleResp carries: (status code, DNR).
    async fn refusal(c: &mut TcpStream, cid: u16) -> (u16, bool) {
        let mut resp = [0u8; 24];
        c.read_exact(&mut resp).await.unwrap();
        assert_eq!(resp[0], 0x05, "a CapsuleResp, not data");
        assert_eq!(u16::from_le_bytes([resp[20], resp[21]]), cid);
        let status = u16::from_le_bytes([resp[22], resp[23]]);
        ((status >> 1) & 0xFF, status & (1 << 15) != 0)
    }

    /// #714, finding c001083c059d: a Get Log Page's length is the host's
    /// word, and the discovery controller takes any host. One asking for
    /// 8 GiB (NUMDU 0x7fff, NUMDL 0xffff), or for one dword past MDTS, is
    /// refused (Invalid Field, DNR) before anything is allocated, and the
    /// connection goes on serving a page that fits.
    #[tokio::test]
    async fn an_oversized_log_page_on_an_unregistered_discovery_connection_is_refused() {
        let dir = std::env::temp_dir().join(format!("stormboot-server-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("image.raw");
        std::fs::write(&path, vec![7u8; 64 * 1024]).unwrap();
        let image = Arc::new(Image::open(&path, 4096).unwrap());
        let max = 1 << 20;
        let target = BootTarget::new(image, dir.join("s"), "nqn.2026-11.lo.storm:t", max);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(target.run(listener));

        let mut c = discovery_connection(addr).await;
        get_log_page(&mut c, 7, (0xFFFF << 16) | 0x70, 0x7FFF).await;
        assert_eq!(refusal(&mut c, 7).await, (0x02, true), "8 GiB is refused");
        get_log_page(&mut c, 8, u32::MAX, u32::MAX).await;
        assert_eq!(refusal(&mut c, 8).await, (0x02, true), "every NUMD bit set is refused, not wrapped");
        let past = (max / 4) as u32; // NUMD is 0's based: max + 4 bytes
        get_log_page(&mut c, 9, ((past & 0xFFFF) << 16) | 0x70, past >> 16).await;
        assert_eq!(refusal(&mut c, 9).await, (0x02, true), "one dword past MDTS is refused");

        // A page that fits: the discovery log, 4 KiB, on the same connection.
        get_log_page(&mut c, 10, (0x3FF << 16) | 0x70, 0).await;
        let mut ch = [0u8; 24];
        c.read_exact(&mut ch).await.unwrap();
        assert_eq!(ch[0], 0x07, "C2HData");
        assert_eq!(u16::from_le_bytes([ch[8], ch[9]]), 10);
        assert_eq!(u32::from_le_bytes(ch[16..20].try_into().unwrap()), 4096);
        let mut page = vec![0u8; 4096];
        c.read_exact(&mut page).await.unwrap();
        assert_eq!(u64::from_le_bytes(page[8..16].try_into().unwrap()), 0, "no subsystem admits this host");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
