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
//! Queue model: one admin connection (QID 0) used at open for Identify,
//! then dropped; one I/O connection (QID 1) kept behind a Mutex for
//! serialized I/O — same concurrency shape as `IscsiDevice`. A connection
//! that errors is dropped and re-established on the next operation, so a
//! bounced remote node degrades to per-op errors (which RAID sees) and
//! heals without a reopen.

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
/// Per-op transfer cap: fits every target's defaults (our ICResp
/// advertises maxh2cdata 131072) and keeps NLB well inside 16 bits.
const MAX_CHUNK: usize = 128 * 1024;

static HOSTID_COUNTER: AtomicU32 = AtomicU32::new(1);

/// A parsed `nvme-tcp://host:port/<nqn>?nsid=N` attach spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvmeTcpSpec {
    pub addr: String,
    pub nqn: String,
    pub nsid: u32,
    /// What to call ourselves on this connection. `None` uses
    /// [`default_host_nqn`], which is the usual case — a node has one
    /// identity, not one per volume.
    pub host_nqn: Option<String>,
}

impl NvmeTcpSpec {
    /// Parse the URI form. `nsid` defaults to 1; port is required (the
    /// fleet convention is :4420 but nothing here assumes it).
    pub fn parse(uri: &str) -> Option<Self> {
        let rest = uri.strip_prefix("nvme-tcp://")?;
        let (addr, rest) = rest.split_once('/')?;
        if addr.is_empty() || !addr.contains(':') {
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
        if let Some(q) = query {
            for kv in q.split('&') {
                if let Some(v) = kv.strip_prefix("nsid=") {
                    nsid = v.parse().ok()?;
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
        })
    }

    pub fn uri(&self) -> String {
        let mut u = format!("nvme-tcp://{}/{}?nsid={}", self.addr, self.nqn, self.nsid);
        if let Some(h) = &self.host_nqn {
            u.push_str("&hostnqn=");
            u.push_str(h);
        }
        u
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
}

impl Conn {
    /// TCP connect + ICReq/ICResp + Fabric Connect for `qid`.
    async fn establish(addr: &str, nqn: &str, host_nqn: &str, qid: u16) -> io::Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        let (reader, writer) = stream.into_split();
        let mut conn = Conn {
            reader: BufReader::new(reader),
            writer: BufWriter::new(writer),
            cid: 1,
        };
        conn.ic_handshake().await?;
        conn.fabric_connect_as(nqn, host_nqn, qid).await?;
        Ok(conn)
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

    async fn fabric_connect_as(
        &mut self,
        nqn: &str,
        host_nqn: &str,
        qid: u16,
    ) -> io::Result<()> {
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
        Ok(())
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
                    let payload_len = ch.plen as usize - ch.hlen as usize;
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

    async fn io_read(&mut self, nsid: u32, slba: u64, nlb: u16, bytes: usize) -> io::Result<Vec<u8>> {
        let cid = self.next_cid();
        let mut sqe = [0u8; 64];
        sqe[0] = IO_READ;
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        sqe[4..8].copy_from_slice(&nsid.to_le_bytes());
        sqe[40..44].copy_from_slice(&(slba as u32).to_le_bytes());
        sqe[44..48].copy_from_slice(&((slba >> 32) as u32).to_le_bytes());
        sqe[48..52].copy_from_slice(&((nlb - 1) as u32).to_le_bytes());
        let data = self.cmd_read_data(&sqe, bytes).await?;
        if data.len() != bytes {
            return Err(io::Error::other(format!(
                "short read: wanted {bytes}, got {}",
                data.len()
            )));
        }
        Ok(data)
    }

    async fn io_write(&mut self, nsid: u32, slba: u64, nlb: u16, data: &[u8]) -> io::Result<()> {
        let cid = self.next_cid();
        let mut sqe = [0u8; 64];
        sqe[0] = IO_WRITE;
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        sqe[4..8].copy_from_slice(&nsid.to_le_bytes());
        sqe[40..44].copy_from_slice(&(slba as u32).to_le_bytes());
        sqe[44..48].copy_from_slice(&((slba >> 32) as u32).to_le_bytes());
        sqe[48..52].copy_from_slice(&((nlb - 1) as u32).to_le_bytes());
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

/// A remote NVMe-TCP namespace attached as a local drive.
pub struct NvmeofDevice {
    /// I/O queue connection (QID 1); None after an error until the next op
    /// re-establishes it.
    conn: Mutex<Option<Conn>>,
    spec: NvmeTcpSpec,
    capacity: u64,
    block_size: u32,
    id: DeviceId,
}

impl NvmeofDevice {
    /// Connect and identify. Admin connection (QID 0) is used for
    /// Identify and dropped; the I/O connection (QID 1) is kept.
    pub async fn connect(spec: &NvmeTcpSpec) -> DriveResult<Self> {
        tracing::info!(
            "NVMe-TCP initiator: connecting to {} nqn={} nsid={}",
            spec.addr,
            spec.nqn,
            spec.nsid
        );
        let mut admin = Conn::establish(&spec.addr, &spec.nqn, spec.effective_host_nqn(), 0)
            .await
            .map_err(DriveError::Io)?;
        let ctrl = admin.identify(CNS_CONTROLLER, 0).await.map_err(DriveError::Io)?;
        let (serial, model) = decode_identify_ctrl(&ctrl);
        let ns = admin
            .identify(CNS_NAMESPACE, spec.nsid)
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
        tracing::info!(
            "NVMe-TCP initiator: {} = {} bytes, block_size {}, model {:?}",
            spec.uri(),
            capacity,
            block_size,
            model
        );
        drop(admin);

        let io_conn = Conn::establish(&spec.addr, &spec.nqn, spec.effective_host_nqn(), 1)
            .await
            .map_err(DriveError::Io)?;

        let uri = spec.uri();
        let id = DeviceId {
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
            conn: Mutex::new(Some(io_conn)),
            spec: spec.clone(),
            capacity,
            block_size,
            id,
        })
    }

    /// Lock the I/O connection, establishing it first if the last
    /// operation dropped it. The caller runs one operation and, on error,
    /// clears the slot so the next call reconnects.
    async fn lock_conn(&self) -> DriveResult<tokio::sync::MutexGuard<'_, Option<Conn>>> {
        let mut guard = self.conn.lock().await;
        if guard.is_none() {
            *guard = Some(
                Conn::establish(&self.spec.addr, &self.spec.nqn, self.spec.effective_host_nqn(), 1)
                    .await
                    .map_err(DriveError::Io)?,
            );
            tracing::info!("NVMe-TCP initiator: reconnected to {}", self.spec.addr);
        }
        Ok(guard)
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
        MAX_CHUNK as u32
    }

    fn device_type(&self) -> DriveType {
        DriveType::NvmeTcp
    }

    async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        self.check_aligned(offset, buf.len())?;
        let bs = self.block_size as u64;
        let nsid = self.spec.nsid;
        let mut done = 0usize;
        while done < buf.len() {
            let chunk = (buf.len() - done).min(MAX_CHUNK);
            let slba = (offset + done as u64) / bs;
            let nlb = (chunk as u64 / bs) as u16;
            let mut guard = self.lock_conn().await?;
            let conn = guard.as_mut().expect("lock_conn established");
            match conn.io_read(nsid, slba, nlb, chunk).await {
                Ok(data) => buf[done..done + chunk].copy_from_slice(&data),
                Err(e) => {
                    *guard = None;
                    return Err(DriveError::Io(e));
                }
            }
            done += chunk;
        }
        Ok(done)
    }

    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        self.check_aligned(offset, buf.len())?;
        let bs = self.block_size as u64;
        let nsid = self.spec.nsid;
        let mut done = 0usize;
        while done < buf.len() {
            let chunk = (buf.len() - done).min(MAX_CHUNK);
            let slba = (offset + done as u64) / bs;
            let nlb = (chunk as u64 / bs) as u16;
            let mut guard = self.lock_conn().await?;
            let conn = guard.as_mut().expect("lock_conn established");
            if let Err(e) = conn.io_write(nsid, slba, nlb, &buf[done..done + chunk]).await {
                *guard = None;
                return Err(DriveError::Io(e));
            }
            done += chunk;
        }
        Ok(done)
    }

    async fn flush(&self) -> DriveResult<()> {
        let nsid = self.spec.nsid;
        let mut guard = self.lock_conn().await?;
        let conn = guard.as_mut().expect("lock_conn established");
        if let Err(e) = conn.io_flush(nsid).await {
            *guard = None;
            return Err(DriveError::Io(e));
        }
        Ok(())
    }

    async fn discard(&self, offset: u64, len: u64) -> DriveResult<()> {
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
            if let Err(e) = conn.io_deallocate(nsid, slba, nlb).await {
                *guard = None;
                return Err(DriveError::Io(e));
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
