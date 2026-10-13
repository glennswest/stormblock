//! NVMe-oF/TCP initiator `BlockDevice` — attach a remote NVMe-TCP
//! namespace as a local drive (#73).
//!
//! This is what makes a cross-node RAID leg possible: stormstorage places
//! one thin volume per node, each node exports it over NVMe-TCP, and the
//! head node opens `nvme-tcp://host:port/<nqn>?nsid=N` as an ordinary
//! drive and mirrors across it with the existing RAID engine.
//!
//! Wire protocol: reuses the *target's* PDU types (`target::nvmeof::pdu`)
//! rather than carrying a second copy of the NVMe/TCP spec — same rule as
//! the iSCSI initiator. The framing here is the initiator direction the
//! test initiator (`tests/common/nvmeof_initiator.rs`) proved against both
//! this target and the Linux kernel target.
//!
//! Paths (#90): `nvme-tcp://a:p,b:p/<nqn>?nsid=N&iopolicy=round-robin` names
//! the target by several addresses — one per storage NIC — and each is a
//! path with connections of its own. Each connection leaves by the local
//! storage interface on the path's subnet (`netif::source_for`): bound to
//! that device (`SO_BINDTODEVICE`) and its address, so two paths on one
//! subnet really are two NICs, not two source addresses on whichever NIC the
//! route lookup picks (the owner's decision on #90). An operation takes a
//! path by `iopolicy`: `round-robin` (default) or `queue-depth` (the path
//! with the fewest commands in flight). A path whose connection fails is
//! skipped for a while and the retry goes to another; with every path down,
//! every path is tried.
//!
//! Queue model: one admin connection (QID 0) used at open for Identify,
//! then dropped; several I/O connections per path
//! (`STORMBLOCK_NVME_TCP_QUEUES`, 4), each behind its own Mutex, one command
//! at a time on each. An operation
//! takes a free one (#331: with one, a flow-over's parallel 1 MiB copies and
//! the node's own reads all queued on a single connection). The first is
//! made at open, the rest when first needed. A connection that errors is
//! dropped and re-established on the next operation, so a bounced remote
//! node degrades to per-op errors (which RAID sees) and heals without a
//! reopen. A write that is not whole blocks is a read-modify-write, and
//! holds the device's `rmw` lock exclusively so no write on another
//! connection lands between its read and its write.

use std::io;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{BlockDevice, DeviceId, DriveError, DriveResult, DriveType};
use crate::target::nvmeof::pdu::{CommonHeader, NvmeCqe, PduType};

const NVME_FABRIC_OPC: u8 = 0x7F;
const FCTYPE_CONNECT: u8 = 0x01;
const ADMIN_IDENTIFY: u8 = 0x06;
const IO_FLUSH: u8 = 0x00;
const IO_WRITE: u8 = 0x01;
const IO_READ: u8 = 0x02;
const IO_DSM: u8 = 0x09;
const CNS_NAMESPACE: u8 = 0x00;
const CNS_CONTROLLER: u8 = 0x01;

/// What an initiator calls itself when nothing more specific is set.
///
/// A constant is the wrong answer for a node: every stormblock initiator in
/// the fleet presents this same string, so a target cannot tell one caller
/// from another. That matters because the host NQN is the one identity NVMe
/// already carries — stormbootx composes `nqn.…:host-<service-tag>` from
/// SMBIOS and presents it on every connect, which is how the appliance knows
/// which machine is asking before anything else exists.
///
/// So this stays as the fallback and [`set_default_host_nqn`] overrides it,
/// letting a node present the *same* name Linux-side that its firmware
/// presented a stage earlier.
const HOST_NQN: &str = "nqn.2024.io.stormblock:initiator";

static DEFAULT_HOST_NQN: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Set the host NQN every connect presents unless its spec names one.
///
/// First call wins: this is boot-time identity, not a runtime knob, and a
/// second caller changing it under open controllers would mean one node
/// answering to two names.
pub fn set_default_host_nqn(nqn: impl Into<String>) -> &'static str {
    DEFAULT_HOST_NQN.get_or_init(|| nqn.into())
}

/// The host NQN to present when a spec does not name one.
///
/// `STORMBLOCK_HOST_NQN` is read once as a fallback so the initramfs can set
/// it for every connect the boot makes without threading it through each
/// call site.
pub fn default_host_nqn() -> &'static str {
    DEFAULT_HOST_NQN
        .get_or_init(|| std::env::var("STORMBLOCK_HOST_NQN").unwrap_or_else(|_| HOST_NQN.to_string()))
        .as_str()
}
/// I/O connections per attached namespace (#331): `STORMBLOCK_NVME_TCP_QUEUES`,
/// 4, at least 1.
fn io_queues() -> usize {
    std::env::var("STORMBLOCK_NVME_TCP_QUEUES").ok().and_then(|v| v.parse().ok()).filter(|n| *n > 0).unwrap_or(4)
}

/// The transfer one command carries (#407): the target's MDTS (4 KiB pages;
/// 0 is "no limit"), never more than this engine's own
/// [`default_max_transfer`](crate::target::nvmeof::default_max_transfer)
/// nor 65536 blocks (NLB is 16 bits). An older engine (forge 13.7) says
/// 128 KiB, and gets 128 KiB.
pub fn transfer_for(mdts: u8, block_size: u32) -> usize {
    let target = if mdts == 0 {
        crate::target::nvmeof::MAX_TRANSFER_CEILING
    } else {
        4096usize.checked_shl(mdts as u32).unwrap_or(usize::MAX)
    };
    let ours = crate::target::nvmeof::default_max_transfer();
    let nlb_cap = 65536 * block_size.max(512) as usize;
    target.min(ours).min(nlb_cap).max(block_size as usize)
}

/// The path MTU of a connected socket (`IP_MTU` / `IPV6_MTU`), or `None`
/// where the system will not say. What a jumbo-frame storage network is
/// checked by (#407): 9000 end to end, or 1500 somewhere on the way.
pub fn path_mtu(stream: &TcpStream) -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let v6 = stream.peer_addr().ok()?.is_ipv6();
        let (level, name) = if v6 { (libc::IPPROTO_IPV6, libc::IPV6_MTU) } else { (libc::IPPROTO_IP, libc::IP_MTU) };
        let mut mtu: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: a valid fd, an int-sized out value and its length.
        let r = unsafe {
            libc::getsockopt(stream.as_raw_fd(), level, name, &mut mtu as *mut _ as *mut libc::c_void, &mut len)
        };
        (r == 0 && mtu > 0).then_some(mtu as u32)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = stream;
        None
    }
}

/// What a fabric device's path is (#407): its MTU and the transfer one
/// command carries, for the flow-over's health and log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct FabricPath {
    /// The largest path MTU among the paths connected so far; 0 unknown.
    pub mtu: u32,
    /// MTU 9000 or more.
    pub jumbo: bool,
    /// Bytes one command carries.
    pub max_transfer: u32,
}

static HOSTID_COUNTER: AtomicU32 = AtomicU32::new(1);

/// How an operation picks a path (#90): the names Linux's nvme-multipath
/// `iopolicy` uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IoPolicy {
    /// In turn.
    #[default]
    RoundRobin,
    /// The path with the fewest commands in flight.
    QueueDepth,
}

impl IoPolicy {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "round-robin" => Some(IoPolicy::RoundRobin),
            "queue-depth" => Some(IoPolicy::QueueDepth),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            IoPolicy::RoundRobin => "round-robin",
            IoPolicy::QueueDepth => "queue-depth",
        }
    }
}

/// A parsed `nvme-tcp://host:port[,host:port…]/<nqn>?nsid=N` attach spec.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NvmeTcpSpec {
    /// `host:port`, or several comma-separated: one per path (#90).
    pub addr: String,
    pub nqn: String,
    pub nsid: u32,
    /// What to call ourselves on this connection. `None` uses
    /// [`default_host_nqn`], which is the usual case — a node has one
    /// identity, not one per volume.
    pub host_nqn: Option<String>,
    /// The DH-HMAC-CHAP secret to answer with when the target asks (#210).
    /// Never part of the URI — a URI is logged, listed and persisted. `None`
    /// falls back to `$STORMBLOCK_DHCHAP_SECRET`.
    pub dhchap: Option<crate::target::nvmeof::auth::DhchapKey>,
    /// How operations pick a path when there are several (#90).
    pub iopolicy: IoPolicy,
}

impl NvmeTcpSpec {
    /// Each path's `host:port`.
    pub fn addrs(&self) -> Vec<&str> {
        self.addr.split(',').map(str::trim).filter(|a| !a.is_empty()).collect()
    }

    /// Parse the URI form. `nsid` defaults to 1; port is required (the
    /// fleet convention is :4420 but nothing here assumes it).
    pub fn parse(uri: &str) -> Option<Self> {
        let rest = uri.strip_prefix("nvme-tcp://")?;
        let (addr, rest) = rest.split_once('/')?;
        if addr.is_empty() || addr.split(',').any(|a| a.trim().is_empty() || !a.contains(':')) {
            return None;
        }
        let (nqn, query) = match rest.split_once('?') {
            Some((n, q)) => (n, Some(q)),
            None => (rest, None),
        };
        if nqn.is_empty() {
            return None;
        }
        let mut nsid = 1u32;
        let mut host_nqn = None;
        let mut iopolicy = IoPolicy::default();
        if let Some(q) = query {
            for kv in q.split('&') {
                if let Some(v) = kv.strip_prefix("nsid=") {
                    nsid = v.parse().ok()?;
                } else if let Some(v) = kv.strip_prefix("iopolicy=") {
                    iopolicy = IoPolicy::parse(v)?;
                } else if let Some(v) = kv.strip_prefix("hostnqn=") {
                    // Spelled as nvme-cli spells it, so an operator moving
                    // between the two does not have to translate.
                    if !v.is_empty() {
                        host_nqn = Some(v.to_string());
                    }
                }
            }
        }
        Some(NvmeTcpSpec {
            addr: addr.to_string(),
            nqn: nqn.to_string(),
            nsid,
            host_nqn,
            dhchap: None,
            iopolicy,
        })
    }

    pub fn uri(&self) -> String {
        let mut u = format!("nvme-tcp://{}/{}?nsid={}", self.addr, self.nqn, self.nsid);
        if let Some(h) = &self.host_nqn {
            u.push_str("&hostnqn=");
            u.push_str(h);
        }
        if self.iopolicy != IoPolicy::default() {
            u.push_str("&iopolicy=");
            u.push_str(self.iopolicy.as_str());
        }
        u
    }

    /// The secret to answer DH-HMAC-CHAP with, if there is one.
    pub fn effective_dhchap(&self) -> Option<crate::target::nvmeof::auth::DhchapKey> {
        self.dhchap.clone().or_else(|| {
            std::env::var("STORMBLOCK_DHCHAP_SECRET")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .and_then(|s| crate::target::nvmeof::auth::DhchapKey::parse(&s).ok())
        })
    }

    /// The name to present on this connection.
    pub fn effective_host_nqn(&self) -> &str {
        match self.host_nqn.as_deref() {
            Some(h) => h,
            None => default_host_nqn(),
        }
    }
}

/// Decode Identify Namespace: (nsze in blocks, block size in bytes).
pub fn decode_identify_ns(data: &[u8]) -> Option<(u64, u32)> {
    if data.len() < 384 {
        return None;
    }
    let nsze = u64::from_le_bytes(data[0..8].try_into().ok()?);
    let flbas_idx = (data[26] & 0x0F) as usize;
    let lbaf_off = 128 + flbas_idx * 4;
    if data.len() < lbaf_off + 4 {
        return None;
    }
    let lbads = data[lbaf_off + 2];
    if lbads < 9 || lbads > 16 {
        return None; // 512..64K — anything else is a decode error
    }
    Some((nsze, 1u32 << lbads))
}

/// Decode Identify Controller: (serial, model), trimmed.
pub fn decode_identify_ctrl(data: &[u8]) -> (String, String) {
    let field = |range: std::ops::Range<usize>| -> String {
        data.get(range)
            .map(|b| String::from_utf8_lossy(b).trim().to_string())
            .unwrap_or_default()
    };
    (field(4..24), field(24..64))
}

fn cqe_status(cqe: &NvmeCqe) -> u16 {
    u16::from_le_bytes([cqe.raw[14], cqe.raw[15]]) & 0xFFFE
}

/// One NVMe-TCP queue connection (admin or I/O).
struct Conn {
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: BufWriter<tokio::net::tcp::OwnedWriteHalf>,
    cid: u16,
    /// The connection's path MTU when it was made (#407); 0 unknown.
    mtu: u32,
}

/// How long one NVMe/TCP command may take before the connection is given up
/// (#358): `STORMBLOCK_NVME_TCP_IO_TIMEOUT_SECS`, default 30 s, Linux's
/// `nvme_core.io_timeout`. With no bound a command on a connection that died
/// without a FIN or RST (the target restarted, a link went) waited forever.
/// A persist waited with it, holding the volume manager, and the Dell's API
/// stopped answering for hours. A timeout drops the connection, the command
/// fails like any other I/O error, and the next one reconnects.
pub fn io_timeout() -> std::time::Duration {
    static T: std::sync::OnceLock<std::time::Duration> = std::sync::OnceLock::new();
    *T.get_or_init(|| {
        let secs = std::env::var("STORMBLOCK_NVME_TCP_IO_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|s| *s > 0)
            .unwrap_or(30);
        std::time::Duration::from_secs(secs)
    })
}

/// Connect to one path's address, leaving by the local storage interface on
/// its subnet when there is one (#90): bound to the device, so egress is that
/// NIC whatever the route lookup would pick, and to its address, so the
/// source agrees with what the NIC answers ARP for.
async fn connect_path(addr: &str, path: usize) -> io::Result<TcpStream> {
    let target = tokio::net::lookup_host(addr)
        .await?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{addr} resolves to nothing")))?;
    let Some(src) = crate::netif::source_for(target.ip(), path) else {
        return TcpStream::connect(target).await;
    };
    let sock = if target.is_ipv4() { tokio::net::TcpSocket::new_v4()? } else { tokio::net::TcpSocket::new_v6()? };
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let Err(e) = socket2::SockRef::from(&sock).bind_device(Some(src.interface.as_bytes())) {
        // CAP_NET_RAW is the engine's on a node. Without it the source
        // address is still bound; egress is then the route's choice, said once.
        static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !SAID.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "NVMe-TCP: cannot bind connections to {} ({e}): the source address is bound, and the route picks the NIC",
                src.interface
            );
        }
    }
    sock.bind(std::net::SocketAddr::new(src.address, 0))?;
    tracing::debug!("NVMe-TCP path {path} to {target} by {} ({})", src.interface, src.address);
    sock.connect(target).await
}

/// A command (or a connect) bounded by [`io_timeout`].
async fn bounded<T>(what: &str, fut: impl std::future::Future<Output = io::Result<T>>) -> io::Result<T> {
    match tokio::time::timeout(io_timeout(), fut).await {
        Ok(r) => r,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("NVMe-TCP {what}: no answer within {}s; the connection is dropped", io_timeout().as_secs()),
        )),
    }
}

impl Conn {
    /// TCP connect + ICReq/ICResp + Fabric Connect for `qid`, and
    /// DH-HMAC-CHAP when the target asks for it.
    async fn establish(spec: &NvmeTcpSpec, addr: &str, path: usize, qid: u16) -> io::Result<Self> {
        let (nqn, host_nqn) = (spec.nqn.as_str(), spec.effective_host_nqn());
        let stream = bounded("connect", connect_path(addr, path)).await?;
        stream.set_nodelay(true)?;
        // A dead peer is noticed while idle too, not only by a command's
        // timeout: probes after 10 s idle, every 5 s, 3 tries (#358).
        let ka = socket2::TcpKeepalive::new()
            .with_time(std::time::Duration::from_secs(10))
            .with_interval(std::time::Duration::from_secs(5))
            .with_retries(3);
        if let Err(e) = socket2::SockRef::from(&stream).set_tcp_keepalive(&ka) {
            tracing::debug!("NVMe-TCP {addr}: TCP keepalive not set: {e}");
        }
        let mtu = path_mtu(&stream).unwrap_or(0);
        let (reader, writer) = stream.into_split();
        let mut conn = Conn {
            // Room for an extent-sized PDU in one read (#407).
            reader: BufReader::with_capacity(256 << 10, reader),
            writer: BufWriter::with_capacity(256 << 10, writer),
            cid: 1,
            mtu,
        };
        bounded("handshake", conn.ic_handshake()).await?;
        let dw0 = bounded("connect command", conn.fabric_connect_as(nqn, host_nqn, qid)).await?;
        if dw0 & crate::target::nvmeof::CONNECT_AUTHREQ_ATR != 0 {
            let key = spec.effective_dhchap().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{nqn} requires DH-HMAC-CHAP for {host_nqn}, and no secret was given"),
                )
            })?;
            bounded("authentication", conn.authenticate(&key, host_nqn, nqn)).await?;
        }
        Ok(conn)
    }

    /// Authentication Send (`send`) or Receive: SECP 0xE9, SPSP 1/1.
    fn auth_sqe(&mut self, send: bool, len: usize) -> [u8; 64] {
        use crate::target::nvmeof::auth;
        let cid = self.next_cid();
        let mut sqe = [0u8; 64];
        sqe[0] = NVME_FABRIC_OPC;
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        sqe[4] = if send { auth::FCTYPE_AUTH_SEND } else { auth::FCTYPE_AUTH_RECEIVE };
        // SGL: in-capsule data block (offset) out, transport data block in.
        sqe[32..36].copy_from_slice(&(len as u32).to_le_bytes());
        sqe[39] = if send { 0x01 } else { 0x5A };
        sqe[41] = 1; // SPSP0
        sqe[42] = 1; // SPSP1
        sqe[43] = auth::SECP_DHCHAP;
        sqe[44..48].copy_from_slice(&(len as u32).to_le_bytes());
        sqe
    }

    /// The host's side of DH-HMAC-CHAP: negotiate, answer the challenge,
    /// read the verdict.
    async fn authenticate(
        &mut self,
        key: &crate::target::nvmeof::auth::DhchapKey,
        host_nqn: &str,
        nqn: &str,
    ) -> io::Result<()> {
        use crate::target::nvmeof::auth;
        let denied = |e: String| io::Error::new(io::ErrorKind::PermissionDenied, e);
        let tid = (HOSTID_COUNTER.fetch_add(1, Ordering::Relaxed) & 0xFFFF) as u16;
        let neg = auth::host_negotiate(tid);
        let sqe = self.auth_sqe(true, neg.len());
        self.cmd_write_data(&sqe, &neg).await?;
        let sqe = self.auth_sqe(false, 256);
        let challenge = self.cmd_read_data(&sqe, 256).await?;
        let reply = auth::host_reply(&challenge, key, host_nqn, nqn).map_err(denied)?;
        let sqe = self.auth_sqe(true, reply.len());
        self.cmd_write_data(&sqe, &reply).await?;
        let sqe = self.auth_sqe(false, 64);
        let verdict = self.cmd_read_data(&sqe, 64).await?;
        auth::host_check_success1(&verdict).map_err(denied)
    }

    fn next_cid(&mut self) -> u16 {
        let cid = self.cid;
        self.cid = self.cid.wrapping_add(1).max(1);
        cid
    }

    async fn ic_handshake(&mut self) -> io::Result<()> {
        let ch = CommonHeader {
            pdu_type: PduType::ICReq as u8,
            flags: 0,
            hlen: 128,
            pdo: 0,
            plen: 128,
        };
        self.writer.write_all(&ch.to_bytes()).await?;
        let mut body = vec![0u8; 120];
        body[4..8].copy_from_slice(&4u32.to_le_bytes()); // MAXR2T
        self.writer.write_all(&body).await?;
        self.writer.flush().await?;

        let mut resp = [0u8; 128];
        self.reader.read_exact(&mut resp).await?;
        let rch = CommonHeader::from_bytes(resp[0..8].try_into().unwrap());
        if rch.pdu_type != PduType::ICResp as u8 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "expected ICResp"));
        }
        Ok(())
    }

    /// Fabric Connect; answers the response's DW0 (CNTLID and AUTHREQ).
    async fn fabric_connect_as(
        &mut self,
        nqn: &str,
        host_nqn: &str,
        qid: u16,
    ) -> io::Result<u32> {
        let cid = self.next_cid();
        let mut sqe = [0u8; 64];
        sqe[0] = NVME_FABRIC_OPC;
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        // Fabrics Connect: FCTYPE byte 4, QID bytes 42-43, SQSIZE 44-45.
        sqe[4] = FCTYPE_CONNECT;
        sqe[42..44].copy_from_slice(&qid.to_le_bytes());
        sqe[44..46].copy_from_slice(&127u16.to_le_bytes());

        let mut data = vec![0u8; 1024];
        let hn = HOSTID_COUNTER.fetch_add(1, Ordering::Relaxed);
        data[0..4].copy_from_slice(&hn.to_le_bytes());
        data[4..16].copy_from_slice(&[0x53u8; 12]); // 'S' — stormblock hostid tail
        data[16] = 0xFF; // cntlid = 0xFFFF (dynamic)
        data[17] = 0xFF;
        let n = nqn.as_bytes();
        data[256..256 + n.len().min(256)].copy_from_slice(&n[..n.len().min(256)]);
        // Offset 512 of the Connect data is the host NQN — the identity the
        // target sees, and the only one NVMe carries on its own.
        let h = host_nqn.as_bytes();
        let n = h.len().min(256);
        data[512..512 + n].copy_from_slice(&h[..n]);

        self.send_capsule(&sqe, &data).await?;
        let cqe = self.read_capsule_resp().await?;
        if cqe_status(&cqe) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("fabric connect qid={qid} failed: status {:#x}", cqe_status(&cqe)),
            ));
        }
        Ok(u32::from_le_bytes(cqe.raw[0..4].try_into().unwrap()))
    }

    async fn send_capsule(&mut self, sqe: &[u8; 64], data: &[u8]) -> io::Result<()> {
        let hlen: u8 = 72;
        let ch = CommonHeader {
            pdu_type: PduType::CapsuleCmd as u8,
            flags: 0,
            hlen,
            pdo: if data.is_empty() { 0 } else { hlen },
            plen: hlen as u32 + data.len() as u32,
        };
        self.writer.write_all(&ch.to_bytes()).await?;
        self.writer.write_all(sqe).await?;
        if !data.is_empty() {
            self.writer.write_all(data).await?;
        }
        self.writer.flush().await
    }

    async fn read_capsule_resp(&mut self) -> io::Result<NvmeCqe> {
        let mut hdr = [0u8; 8];
        self.reader.read_exact(&mut hdr).await?;
        let ch = CommonHeader::from_bytes(&hdr);
        if ch.pdu_type != PduType::CapsuleResp as u8 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected CapsuleResp, got PDU type {}", ch.pdu_type),
            ));
        }
        let mut cqe = [0u8; 16];
        self.reader.read_exact(&mut cqe).await?;
        let remaining = ch.plen as usize - 8 - 16;
        if remaining > 0 {
            let mut extra = vec![0u8; remaining];
            self.reader.read_exact(&mut extra).await?;
        }
        Ok(NvmeCqe { raw: cqe })
    }

    /// Command with a C2H data response: C2HData PDU(s), then either the
    /// in-band success flag or a trailing CapsuleResp.
    async fn cmd_read_data(&mut self, sqe: &[u8; 64], expect: usize) -> io::Result<Vec<u8>> {
        self.send_capsule(sqe, &[]).await?;
        let mut out = Vec::with_capacity(expect);
        loop {
            let mut hdr = [0u8; 8];
            self.reader.read_exact(&mut hdr).await?;
            let ch = CommonHeader::from_bytes(&hdr);
            match PduType::from_byte(ch.pdu_type) {
                Some(PduType::C2HData) => {
                    let mut specific = [0u8; 16];
                    self.reader.read_exact(&mut specific).await?;
                    let datal = u32::from_le_bytes(specific[8..12].try_into().unwrap());
                    let last = ch.flags & 0x04 != 0;
                    let success = ch.flags & 0x08 != 0;
                    let payload_len = (ch.plen as usize).saturating_sub(ch.hlen as usize);
                    if payload_len > crate::target::nvmeof::pdu::MAX_PDU_BYTES {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("a C2H PDU of {payload_len} bytes is more than any command carries"),
                        ));
                    }
                    if payload_len > 0 {
                        let mut payload = vec![0u8; payload_len];
                        self.reader.read_exact(&mut payload).await?;
                        payload.truncate(datal as usize);
                        out.extend_from_slice(&payload);
                    }
                    if last {
                        if success {
                            return Ok(out);
                        }
                        // Status arrives in a trailing CapsuleResp.
                        let cqe = self.read_capsule_resp().await?;
                        if cqe_status(&cqe) != 0 {
                            return Err(io::Error::other(format!(
                                "read failed: status {:#x}",
                                cqe_status(&cqe)
                            )));
                        }
                        return Ok(out);
                    }
                }
                Some(PduType::CapsuleResp) => {
                    let mut cqe = [0u8; 16];
                    self.reader.read_exact(&mut cqe).await?;
                    let remaining = ch.plen as usize - 8 - 16;
                    if remaining > 0 {
                        let mut extra = vec![0u8; remaining];
                        self.reader.read_exact(&mut extra).await?;
                    }
                    let cqe = NvmeCqe { raw: cqe };
                    if cqe_status(&cqe) != 0 {
                        return Err(io::Error::other(format!(
                            "command failed: status {:#x}",
                            cqe_status(&cqe)
                        )));
                    }
                    return Ok(out);
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unexpected PDU type {} in data response", ch.pdu_type),
                    ))
                }
            }
        }
    }

    /// Command with inline data out and a bare CapsuleResp back.
    async fn cmd_write_data(&mut self, sqe: &[u8; 64], data: &[u8]) -> io::Result<()> {
        self.send_capsule(sqe, data).await?;
        let cqe = self.read_capsule_resp().await?;
        if cqe_status(&cqe) != 0 {
            return Err(io::Error::other(format!(
                "command failed: status {:#x}",
                cqe_status(&cqe)
            )));
        }
        Ok(())
    }

    async fn identify(&mut self, cns: u8, nsid: u32) -> io::Result<Vec<u8>> {
        let cid = self.next_cid();
        let mut sqe = [0u8; 64];
        sqe[0] = ADMIN_IDENTIFY;
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        sqe[4..8].copy_from_slice(&nsid.to_le_bytes());
        sqe[40..44].copy_from_slice(&(cns as u32).to_le_bytes());
        self.cmd_read_data(&sqe, 4096).await
    }

    async fn io_read(&mut self, nsid: u32, slba: u64, nlb: u32, bytes: usize) -> io::Result<Vec<u8>> {
        let cid = self.next_cid();
        let mut sqe = [0u8; 64];
        sqe[0] = IO_READ;
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        sqe[4..8].copy_from_slice(&nsid.to_le_bytes());
        sqe[40..44].copy_from_slice(&(slba as u32).to_le_bytes());
        sqe[44..48].copy_from_slice(&((slba >> 32) as u32).to_le_bytes());
        sqe[48..52].copy_from_slice(&(nlb - 1).to_le_bytes());
        let data = self.cmd_read_data(&sqe, bytes).await?;
        if data.len() != bytes {
            return Err(io::Error::other(format!(
                "short read: wanted {bytes}, got {}",
                data.len()
            )));
        }
        Ok(data)
    }

    async fn io_write(&mut self, nsid: u32, slba: u64, nlb: u32, data: &[u8]) -> io::Result<()> {
        let cid = self.next_cid();
        let mut sqe = [0u8; 64];
        sqe[0] = IO_WRITE;
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        sqe[4..8].copy_from_slice(&nsid.to_le_bytes());
        sqe[40..44].copy_from_slice(&(slba as u32).to_le_bytes());
        sqe[44..48].copy_from_slice(&((slba >> 32) as u32).to_le_bytes());
        sqe[48..52].copy_from_slice(&(nlb - 1).to_le_bytes());
        self.cmd_write_data(&sqe, data).await
    }

    async fn io_flush(&mut self, nsid: u32) -> io::Result<()> {
        let cid = self.next_cid();
        let mut sqe = [0u8; 64];
        sqe[0] = IO_FLUSH;
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        sqe[4..8].copy_from_slice(&nsid.to_le_bytes());
        self.cmd_write_data(&sqe, &[]).await
    }

    async fn io_deallocate(&mut self, nsid: u32, slba: u64, nlb: u32) -> io::Result<()> {
        let cid = self.next_cid();
        let mut sqe = [0u8; 64];
        sqe[0] = IO_DSM;
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        sqe[4..8].copy_from_slice(&nsid.to_le_bytes());
        sqe[40..44].copy_from_slice(&0u32.to_le_bytes()); // NR = 1 (0-based)
        sqe[44..48].copy_from_slice(&(1u32 << 2).to_le_bytes()); // AD
        let mut range = [0u8; 16];
        range[4..8].copy_from_slice(&nlb.to_le_bytes());
        range[8..16].copy_from_slice(&slba.to_le_bytes());
        self.cmd_write_data(&sqe, &range).await
    }
}

/// One path to the target (#90): an address, its connections, and how it
/// has gone.
struct Path {
    addr: String,
    /// I/O queue connections; None until first used, and after an error
    /// until the next op re-establishes it.
    conns: Vec<Mutex<Option<Conn>>>,
    /// Commands in flight on this path (the queue-depth policy's measure).
    inflight: std::sync::atomic::AtomicUsize,
    /// Skipped until then after a failure.
    down_until: std::sync::Mutex<Option<std::time::Instant>>,
    /// Commands and bytes carried.
    ops: std::sync::atomic::AtomicU64,
    bytes: std::sync::atomic::AtomicU64,
    /// The path MTU its last connection had (#407); 0 unknown.
    mtu: AtomicU32,
}

impl Path {
    fn new(addr: &str) -> Self {
        Path {
            addr: addr.to_string(),
            conns: (0..io_queues()).map(|_| Mutex::new(None)).collect(),
            inflight: std::sync::atomic::AtomicUsize::new(0),
            down_until: std::sync::Mutex::new(None),
            ops: std::sync::atomic::AtomicU64::new(0),
            bytes: std::sync::atomic::AtomicU64::new(0),
            mtu: AtomicU32::new(0),
        }
    }

    fn is_down(&self) -> bool {
        matches!(*self.down_until.lock().unwrap_or_else(|e| e.into_inner()), Some(t) if t > std::time::Instant::now())
    }
}

/// How long a path that failed is skipped (#90).
const PATH_DOWN: std::time::Duration = std::time::Duration::from_secs(5);

/// What one path has carried, for a caller checking the spread (#90).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PathStats {
    pub addr: String,
    pub ops: u64,
    pub bytes: u64,
    pub down: bool,
}

/// A held connection on one path: counted in flight until dropped.
struct ConnGuard<'a> {
    guard: tokio::sync::MutexGuard<'a, Option<Conn>>,
    path: &'a Path,
}

impl std::ops::Deref for ConnGuard<'_> {
    type Target = Option<Conn>;
    fn deref(&self) -> &Option<Conn> {
        &self.guard
    }
}

impl std::ops::DerefMut for ConnGuard<'_> {
    fn deref_mut(&mut self) -> &mut Option<Conn> {
        &mut self.guard
    }
}

impl Drop for ConnGuard<'_> {
    fn drop(&mut self) {
        self.path.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}

impl ConnGuard<'_> {
    /// Count what this command carried.
    fn carried(&self, bytes: usize) {
        self.path.ops.fetch_add(1, Ordering::Relaxed);
        self.path.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

/// A remote NVMe-TCP namespace attached as a local drive.
pub struct NvmeofDevice {
    /// One per address in the spec (#90).
    paths: Vec<Path>,
    /// Where the next operation starts looking.
    next: AtomicU32,
    /// Writes hold it shared; a read-modify-write exclusively.
    rmw: tokio::sync::RwLock<()>,
    spec: NvmeTcpSpec,
    capacity: u64,
    block_size: u32,
    /// Bytes one command carries: the target's MDTS and ours (#407).
    max_transfer: usize,
    id: DeviceId,
}

impl NvmeofDevice {
    /// Connect and identify. Admin connection (QID 0) is used for
    /// Identify and dropped; one I/O connection (QID 1) is kept. The first
    /// path that answers is used; the rest connect when first needed.
    pub async fn connect(spec: &NvmeTcpSpec) -> DriveResult<Self> {
        let addrs = spec.addrs();
        if addrs.is_empty() {
            return Err(DriveError::Other(anyhow::anyhow!("nvme-tcp: no address in {}", spec.uri())));
        }
        tracing::info!(
            "NVMe-TCP initiator: connecting to {} nqn={} nsid={}{}",
            spec.addr,
            spec.nqn,
            spec.nsid,
            if addrs.len() > 1 { format!(" ({} paths, {})", addrs.len(), spec.iopolicy.as_str()) } else { String::new() }
        );
        let paths: Vec<Path> = addrs.iter().map(|a| Path::new(a)).collect();
        let mut admin = None;
        let mut last = None;
        for (i, a) in addrs.iter().enumerate() {
            match Conn::establish(spec, a, i, 0).await {
                Ok(c) => {
                    admin = Some((i, c));
                    break;
                }
                Err(e) => {
                    tracing::warn!("NVMe-TCP initiator: path {a} does not answer: {e}");
                    *paths[i].down_until.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(std::time::Instant::now() + PATH_DOWN);
                    last = Some(e);
                }
            }
        }
        let Some((first, mut admin)) = admin else {
            return Err(DriveError::Io(last.expect("at least one path was tried")));
        };
        let ctrl = bounded("identify", admin.identify(CNS_CONTROLLER, 0)).await.map_err(DriveError::Io)?;
        let (serial, model) = decode_identify_ctrl(&ctrl);
        let mdts = ctrl.get(77).copied().unwrap_or(0);
        let ns = bounded("identify", admin.identify(CNS_NAMESPACE, spec.nsid))
            .await
            .map_err(DriveError::Io)?;
        let (nsze, block_size) = decode_identify_ns(&ns).ok_or_else(|| {
            DriveError::Other(anyhow::anyhow!(
                "nsid {} on {}: unparseable Identify Namespace",
                spec.nsid,
                spec.nqn
            ))
        })?;
        if nsze == 0 {
            return Err(DriveError::Other(anyhow::anyhow!(
                "nsid {} on {}: namespace has zero size (not found?)",
                spec.nsid,
                spec.nqn
            )));
        }
        let capacity = nsze * block_size as u64;
        let max_transfer = transfer_for(mdts, block_size);
        drop(admin);

        let io_conn = Conn::establish(spec, addrs[first], first, 1)
            .await
            .map_err(DriveError::Io)?;
        let mtu = io_conn.mtu;
        paths[first].mtu.store(mtu, Ordering::Relaxed);
        *paths[first].conns[0].lock().await = Some(io_conn);
        tracing::info!(
            "NVMe-TCP initiator: {} = {} bytes, block_size {}, model {:?}, {} KiB per command (target MDTS {mdts}), path MTU {}",
            spec.uri(),
            capacity,
            block_size,
            model,
            max_transfer >> 10,
            match mtu {
                0 => "unknown".to_string(),
                m if m >= 9000 => format!("{m}, jumbo"),
                m => m.to_string(),
            },
        );

        let uri = spec.uri();
        let id = DeviceId {
            wwn: String::new(),
            // Stable across reopens: derived from the attach URI, never
            // minted fresh (#65 is about exactly this mistake).
            uuid: Uuid::new_v5(&Uuid::NAMESPACE_URL, uri.as_bytes()),
            serial: if serial.is_empty() {
                format!("nvme-tcp-{}", spec.nsid)
            } else {
                serial
            },
            model: if model.is_empty() { "NVMe-TCP".into() } else { model },
            path: uri,
        };
        Ok(NvmeofDevice {
            paths,
            next: AtomicU32::new(0),
            rmw: tokio::sync::RwLock::new(()),
            spec: spec.clone(),
            capacity,
            block_size,
            max_transfer,
            id,
        })
    }

    /// What each path has carried (#90).
    pub fn path_stats(&self) -> Vec<PathStats> {
        self.paths
            .iter()
            .map(|p| PathStats {
                addr: p.addr.clone(),
                ops: p.ops.load(Ordering::Relaxed),
                bytes: p.bytes.load(Ordering::Relaxed),
                down: p.is_down(),
            })
            .collect()
    }

    /// The path an operation goes by (#90): among those not marked down (all
    /// of them when every one is), by the spec's policy.
    fn pick_path(&self) -> usize {
        let n = self.paths.len();
        let up: Vec<usize> = (0..n).filter(|i| !self.paths[*i].is_down()).collect();
        let pool: Vec<usize> = if up.is_empty() { (0..n).collect() } else { up };
        match self.spec.iopolicy {
            IoPolicy::RoundRobin => pool[self.next.fetch_add(1, Ordering::Relaxed) as usize % pool.len()],
            IoPolicy::QueueDepth => {
                let start = self.next.fetch_add(1, Ordering::Relaxed) as usize;
                (0..pool.len())
                    .map(|k| pool[(start + k) % pool.len()])
                    .min_by_key(|i| self.paths[*i].inflight.load(Ordering::Relaxed))
                    .expect("at least one path")
            }
        }
    }

    /// Lock an I/O connection on the path the policy picks, establishing it
    /// first if the last operation dropped it. The caller runs one operation
    /// and, on error, calls [`failed`](Self::failed) so the next call
    /// reconnects (and goes by another path while this one is down).
    async fn lock_conn(&self) -> DriveResult<ConnGuard<'_>> {
        let pi = self.pick_path();
        let path = &self.paths[pi];
        path.inflight.fetch_add(1, Ordering::Relaxed);
        // A free connection on the path, else wait for one in turn.
        let n = path.conns.len();
        let start = self.next.load(Ordering::Relaxed) as usize;
        let free = (0..n).find_map(|i| path.conns[(start + i) % n].try_lock().ok());
        let guard = match free {
            Some(g) => g,
            None => path.conns[start % n].lock().await,
        };
        let mut guard = ConnGuard { guard, path };
        if guard.is_none() {
            match Conn::establish(&self.spec, &path.addr, pi, 1).await {
                Ok(c) => {
                    if c.mtu > 0 {
                        path.mtu.store(c.mtu, Ordering::Relaxed);
                    }
                    *guard = Some(c);
                    tracing::debug!("NVMe-TCP initiator: connected to {}", path.addr);
                }
                Err(e) => {
                    *path.down_until.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(std::time::Instant::now() + PATH_DOWN);
                    tracing::warn!("NVMe-TCP initiator: path {} is down: {e}", path.addr);
                    return Err(DriveError::Io(e));
                }
            }
        }
        Ok(guard)
    }

    fn is_aligned(&self, offset: u64, len: usize) -> bool {
        let bs = self.block_size as u64;
        offset % bs == 0 && len as u64 % bs == 0
    }

    /// The whole blocks that cover `[offset, offset + len)`.
    fn cover(&self, offset: u64, len: usize) -> DriveResult<(u64, u64)> {
        let bs = self.block_size as u64;
        let end = offset + len as u64;
        if end > self.capacity {
            return Err(DriveError::OutOfRange { offset, len: len as u64, capacity: self.capacity });
        }
        Ok((offset / bs * bs, end.div_ceil(bs) * bs))
    }

    /// Drop the connection after an error, so the next call reconnects, and
    /// skip its path for a while (#90): the retry goes by another.
    fn failed(guard: &mut ConnGuard<'_>, e: io::Error) -> DriveError {
        **guard = None;
        *guard.path.down_until.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now() + PATH_DOWN);
        tracing::warn!("NVMe-TCP initiator: path {} failed: {e}", guard.path.addr);
        DriveError::Io(e)
    }

    /// Whole blocks, in chunks the target takes.
    async fn read_blocks(&self, conn: &mut Conn, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let bs = self.block_size as u64;
        let mut done = 0usize;
        while done < buf.len() {
            let chunk = (buf.len() - done).min(self.max_transfer);
            let slba = (offset + done as u64) / bs;
            let nlb = (chunk as u64 / bs) as u32;
            let data = bounded("read", conn.io_read(self.spec.nsid, slba, nlb, chunk)).await?;
            buf[done..done + chunk].copy_from_slice(&data);
            done += chunk;
        }
        Ok(done)
    }

    async fn write_blocks(&self, conn: &mut Conn, offset: u64, buf: &[u8]) -> io::Result<usize> {
        let bs = self.block_size as u64;
        let mut done = 0usize;
        while done < buf.len() {
            let chunk = (buf.len() - done).min(self.max_transfer);
            let slba = (offset + done as u64) / bs;
            let nlb = (chunk as u64 / bs) as u32;
            bounded("write", conn.io_write(self.spec.nsid, slba, nlb, &buf[done..done + chunk])).await?;
            done += chunk;
        }
        Ok(done)
    }

    fn check_aligned(&self, offset: u64, len: usize) -> DriveResult<()> {
        let bs = self.block_size as u64;
        if offset % bs != 0 || len as u64 % bs != 0 {
            return Err(DriveError::NotAligned {
                offset,
                block_size: self.block_size,
            });
        }
        if offset + len as u64 > self.capacity {
            return Err(DriveError::OutOfRange {
                offset,
                len: len as u64,
                capacity: self.capacity,
            });
        }
        Ok(())
    }
}

#[async_trait]
impl BlockDevice for NvmeofDevice {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn capacity_bytes(&self) -> u64 {
        self.capacity
    }

    fn block_size(&self) -> u32 {
        self.block_size
    }

    fn optimal_io_size(&self) -> u32 {
        self.max_transfer as u32
    }

    fn fabric_path(&self) -> Option<FabricPath> {
        let mtu = self.paths.iter().map(|p| p.mtu.load(Ordering::Relaxed)).max().unwrap_or(0);
        Some(FabricPath { mtu, jumbo: mtu >= 9000, max_transfer: self.max_transfer as u32 })
    }

    fn device_type(&self) -> DriveType {
        DriveType::NvmeTcp
    }

    // Every op runs under the shared retry (#359): a timeout or a dropped
    // connection (the attempt already dropped the connection, so the next
    // reconnects) is tried again, bounded; a real answer (an alignment or
    // range error, a refused command) fails at once. Each is safe to repeat:
    // a read, the same bytes to the same blocks, a flush, a deallocate.
    async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        let len = buf.len();
        let what = format!("NVMe-TCP {} read at {offset}", self.spec.addr);
        let got = crate::retry::with_backoff(&what, crate::retry::Policy::BLOCK_IO, classify, |_| async move {
            let mut tmp = vec![0u8; len];
            let n = self.read_once(offset, &mut tmp).await?;
            Ok::<_, DriveError>((n, tmp))
        })
        .await
        .map_err(|f| f.error)?;
        buf.copy_from_slice(&got.1);
        Ok(got.0)
    }

    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        let what = format!("NVMe-TCP {} write at {offset}", self.spec.addr);
        crate::retry::with_backoff(&what, crate::retry::Policy::BLOCK_IO, classify, |_| self.write_once(offset, buf))
            .await
            .map_err(|f| f.error)
    }

    async fn flush(&self) -> DriveResult<()> {
        let what = format!("NVMe-TCP {} flush", self.spec.addr);
        crate::retry::with_backoff(&what, crate::retry::Policy::BLOCK_IO, classify, |_| self.flush_once())
            .await
            .map_err(|f| f.error)
    }

    async fn discard(&self, offset: u64, len: u64) -> DriveResult<()> {
        let what = format!("NVMe-TCP {} deallocate at {offset}", self.spec.addr);
        crate::retry::with_backoff(&what, crate::retry::Policy::BLOCK_IO, classify, |_| self.discard_once(offset, len))
            .await
            .map_err(|f| f.error)
    }
}

/// A device error's retry class (#359).
fn classify(e: &DriveError) -> crate::retry::Class {
    if e.is_transport() {
        crate::retry::Class::Transient
    } else {
        crate::retry::Class::Permanent
    }
}

impl NvmeofDevice {
    async fn read_once(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        if self.is_aligned(offset, buf.len()) {
            self.check_aligned(offset, buf.len())?;
            let mut guard = self.lock_conn().await?;
            let conn = guard.as_mut().expect("lock_conn established");
            return match self.read_blocks(conn, offset, buf).await {
                Ok(n) => {
                    guard.carried(n);
                    Ok(n)
                }
                Err(e) => Err(Self::failed(&mut guard, e)),
            };
        }
        // Not whole blocks (#301): read the blocks that cover it and copy
        // out, as the O_DIRECT device does. The slot table is read in
        // entries of 64 bytes, and a namespace's blocks are 4096.
        let (start, end) = self.cover(offset, buf.len())?;
        let mut span = vec![0u8; (end - start) as usize];
        let mut guard = self.lock_conn().await?;
        let conn = guard.as_mut().expect("lock_conn established");
        if let Err(e) = self.read_blocks(conn, start, &mut span).await {
            return Err(Self::failed(&mut guard, e));
        }
        guard.carried(span.len());
        let at = (offset - start) as usize;
        buf.copy_from_slice(&span[at..at + buf.len()]);
        Ok(buf.len())
    }

    async fn write_once(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        if self.is_aligned(offset, buf.len()) {
            self.check_aligned(offset, buf.len())?;
            let _w = self.rmw.read().await;
            let mut guard = self.lock_conn().await?;
            let conn = guard.as_mut().expect("lock_conn established");
            return match self.write_blocks(conn, offset, buf).await {
                Ok(n) => {
                    guard.carried(n);
                    Ok(n)
                }
                Err(e) => Err(Self::failed(&mut guard, e)),
            };
        }
        // Read-modify-write the covering blocks under one hold of the
        // connection and the device's `rmw` lock, so no other write of this
        // device lands in between.
        let (start, end) = self.cover(offset, buf.len())?;
        let mut span = vec![0u8; (end - start) as usize];
        let _w = self.rmw.write().await;
        let mut guard = self.lock_conn().await?;
        let conn = guard.as_mut().expect("lock_conn established");
        if let Err(e) = self.read_blocks(conn, start, &mut span).await {
            return Err(Self::failed(&mut guard, e));
        }
        let at = (offset - start) as usize;
        span[at..at + buf.len()].copy_from_slice(buf);
        let conn = guard.as_mut().expect("lock_conn established");
        if let Err(e) = self.write_blocks(conn, start, &span).await {
            return Err(Self::failed(&mut guard, e));
        }
        guard.carried(span.len() * 2);
        Ok(buf.len())
    }

    async fn flush_once(&self) -> DriveResult<()> {
        let nsid = self.spec.nsid;
        let mut guard = self.lock_conn().await?;
        let conn = guard.as_mut().expect("lock_conn established");
        if let Err(e) = bounded("flush", conn.io_flush(nsid)).await {
            return Err(Self::failed(&mut guard, e));
        }
        guard.carried(0);
        Ok(())
    }

    async fn discard_once(&self, offset: u64, len: u64) -> DriveResult<()> {
        if len == 0 {
            return Ok(());
        }
        self.check_aligned(offset, len as usize)?;
        let bs = self.block_size as u64;
        let nsid = self.spec.nsid;
        let mut slba = offset / bs;
        let mut blocks = len / bs;
        while blocks > 0 {
            let nlb = blocks.min(u32::MAX as u64) as u32;
            let mut guard = self.lock_conn().await?;
            let conn = guard.as_mut().expect("lock_conn established");
            if let Err(e) = bounded("deallocate", conn.io_deallocate(nsid, slba, nlb)).await {
                return Err(Self::failed(&mut guard, e));
            }
            slba += nlb as u64;
            blocks -= nlb as u64;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #407: the transfer per command follows the target's MDTS, capped by
    /// ours and by a 16-bit NLB.
    #[test]
    fn the_transfer_per_command_is_the_targets_mdts_within_ours() {
        let ours = crate::target::nvmeof::default_max_transfer();
        assert_eq!(transfer_for(5, 4096), 128 << 10, "an older engine's 128 KiB");
        assert_eq!(transfer_for(8, 4096), (1 << 20).min(ours));
        assert_eq!(transfer_for(11, 4096), (8 << 20).min(ours));
        assert_eq!(transfer_for(0, 4096), ours, "no limit stated: ours");
        assert_eq!(transfer_for(30, 512), ours.min(65536 * 512), "never past 65536 blocks");
    }

    #[test]
    fn uri_parses_and_roundtrips() {
        let s = NvmeTcpSpec::parse("nvme-tcp://10.0.0.5:4420/nqn.2024.io.stormblock:vol1?nsid=3")
            .unwrap();
        assert_eq!(s.addr, "10.0.0.5:4420");
        assert_eq!(s.nqn, "nqn.2024.io.stormblock:vol1");
        assert_eq!(s.nsid, 3);
        assert_eq!(NvmeTcpSpec::parse(&s.uri()).unwrap(), s);

        let d = NvmeTcpSpec::parse("nvme-tcp://host:4420/nqn.x").unwrap();
        assert_eq!(d.nsid, 1, "nsid defaults to 1");

        assert!(NvmeTcpSpec::parse("iscsi://host:3260/iqn").is_none());
        assert!(NvmeTcpSpec::parse("nvme-tcp://hostnoport/nqn.x").is_none());
        assert!(NvmeTcpSpec::parse("nvme-tcp://host:4420/").is_none());
        assert!(NvmeTcpSpec::parse("nvme-tcp://host:4420/nqn?nsid=bogus").is_none());
    }

    /// Several paths and a policy (#90).
    #[test]
    fn a_uri_names_several_paths_and_a_policy() {
        let s = NvmeTcpSpec::parse("nvme-tcp://10.0.0.5:4420,10.0.0.15:4420/nqn.x?nsid=2&iopolicy=queue-depth").unwrap();
        assert_eq!(s.addrs(), vec!["10.0.0.5:4420", "10.0.0.15:4420"]);
        assert_eq!(s.iopolicy, IoPolicy::QueueDepth);
        assert_eq!(NvmeTcpSpec::parse(&s.uri()).unwrap(), s);
        let rr = NvmeTcpSpec::parse("nvme-tcp://a:1,b:2/nqn.x").unwrap();
        assert_eq!(rr.iopolicy, IoPolicy::RoundRobin);
        assert!(!rr.uri().contains("iopolicy"), "the default is not spelled out");
        assert!(NvmeTcpSpec::parse("nvme-tcp://a:1,,b:2/nqn.x").is_none());
        assert!(NvmeTcpSpec::parse("nvme-tcp://a:1,b/nqn.x").is_none());
        assert!(NvmeTcpSpec::parse("nvme-tcp://a:1/nqn.x?iopolicy=numa").is_none());
    }

    #[test]
    fn identify_ns_decodes_size_and_block() {
        let mut data = vec![0u8; 4096];
        data[0..8].copy_from_slice(&1_000_000u64.to_le_bytes()); // NSZE
        data[26] = 0x01; // FLBAS index 1
        data[128 + 4 + 2] = 12; // LBAF[1].LBADS = 12 → 4096
        let (nsze, bs) = decode_identify_ns(&data).unwrap();
        assert_eq!(nsze, 1_000_000);
        assert_eq!(bs, 4096);

        data[128 + 4 + 2] = 3; // absurd LBADS
        assert!(decode_identify_ns(&data).is_none());
        assert!(decode_identify_ns(&[0u8; 10]).is_none());
    }

    #[test]
    fn identify_ctrl_decodes_serial_model() {
        let mut data = vec![0u8; 4096];
        data[4..24].copy_from_slice(b"SN-1234             ");
        data[24..64].copy_from_slice(b"StormBlock Volume                       ");
        let (sn, mn) = decode_identify_ctrl(&data);
        assert_eq!(sn, "SN-1234");
        assert_eq!(mn, "StormBlock Volume");
    }

    #[test]
    fn device_id_uuid_is_stable_for_a_spec() {
        let s = NvmeTcpSpec::parse("nvme-tcp://h:4420/nqn.a?nsid=2").unwrap();
        let a = Uuid::new_v5(&Uuid::NAMESPACE_URL, s.uri().as_bytes());
        let b = Uuid::new_v5(&Uuid::NAMESPACE_URL, s.uri().as_bytes());
        assert_eq!(a, b);
    }
}

#[cfg(test)]
mod host_nqn_tests {
    use super::NvmeTcpSpec;

    /// The identity travels with the address, so a claim's URI can carry it
    /// and every later reconnect presents the same name.
    #[test]
    fn a_uri_can_name_the_host() {
        let s = NvmeTcpSpec::parse(
            "nvme-tcp://10.0.0.5:4420/nqn.2026-09.lo.g16:stormcos?nsid=7&hostnqn=nqn.2026-09.lo.storm:host-C2NR0Q2",
        )
        .expect("parses");
        assert_eq!(s.nsid, 7);
        assert_eq!(s.host_nqn.as_deref(), Some("nqn.2026-09.lo.storm:host-C2NR0Q2"));
        assert_eq!(s.effective_host_nqn(), "nqn.2026-09.lo.storm:host-C2NR0Q2");
        // Round-trips, so a spec that came from a URI can be written back out.
        assert_eq!(NvmeTcpSpec::parse(&s.uri()).unwrap(), s);
    }

    /// Without one, a node still connects — it just does not say who it is,
    /// which is the behaviour every existing caller had.
    #[test]
    fn a_uri_without_a_host_falls_back() {
        let s = NvmeTcpSpec::parse("nvme-tcp://10.0.0.5:4420/nqn.x?nsid=1").unwrap();
        assert_eq!(s.host_nqn, None);
        assert!(!s.effective_host_nqn().is_empty());
        assert_eq!(NvmeTcpSpec::parse(&s.uri()).unwrap(), s);
    }

    /// An empty value is not an identity; it must not become one.
    #[test]
    fn an_empty_host_is_ignored() {
        let s = NvmeTcpSpec::parse("nvme-tcp://10.0.0.5:4420/nqn.x?hostnqn=").unwrap();
        assert_eq!(s.host_nqn, None);
    }
}
