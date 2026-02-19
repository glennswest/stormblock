//! Minimal NVMe-oF/TCP initiator for integration tests.
//!
//! Reuses PDU types from `stormblock::target::nvmeof::pdu`.

use std::io;
use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;

use stormblock::target::nvmeof::pdu::*;

// NVMe opcodes
const NVME_FABRIC_OPC: u8 = 0x7F;
const ADMIN_IDENTIFY: u8 = 0x06;
const ADMIN_GET_LOG_PAGE: u8 = 0x02;
const IO_READ: u8 = 0x02;
const IO_WRITE: u8 = 0x01;
const IO_FLUSH: u8 = 0x00;

// Fabric command types
const FCTYPE_CONNECT: u8 = 0x01;

// Identify CNS values
const CNS_CONTROLLER: u8 = 0x01;
const CNS_NAMESPACE: u8 = 0x00;

pub struct NvmeofInitiator {
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: BufWriter<tokio::net::tcp::OwnedWriteHalf>,
    cid: u16,
    cntlid: u16,
    maxh2cdata: u32,
}

impl NvmeofInitiator {
    pub async fn connect(addr: SocketAddr) -> io::Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        let (reader, writer) = stream.into_split();
        Ok(NvmeofInitiator {
            reader: BufReader::new(reader),
            writer: BufWriter::new(writer),
            cid: 1,
            cntlid: 0,
            maxh2cdata: 131072,
        })
    }

    fn next_cid(&mut self) -> u16 {
        let cid = self.cid;
        self.cid += 1;
        cid
    }

    /// Perform ICReq/ICResp handshake.
    pub async fn ic_handshake(&mut self) -> io::Result<()> {
        // Send ICReq (128 bytes total)
        let ch = CommonHeader {
            pdu_type: PduType::ICReq as u8,
            flags: 0,
            hlen: 128,
            pdo: 0,
            plen: 128,
        };
        self.writer.write_all(&ch.to_bytes()).await?;

        // ICReq body (120 bytes after common header)
        let mut body = vec![0u8; 120];
        body[0..2].copy_from_slice(&0u16.to_le_bytes()); // PFV = 0
        body[2] = 0; // HPDA
        body[3] = 0; // No digests
        body[4..8].copy_from_slice(&4u32.to_le_bytes()); // MAXR2T
        self.writer.write_all(&body).await?;
        self.writer.flush().await?;

        // Read ICResp (128 bytes)
        let mut resp_buf = [0u8; 128];
        self.reader.read_exact(&mut resp_buf).await?;

        let resp_ch = CommonHeader::from_bytes(resp_buf[0..8].try_into().unwrap());
        if resp_ch.pdu_type != PduType::ICResp as u8 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "expected ICResp"));
        }

        // Parse maxh2cdata from ICResp body
        let maxh2c = u32::from_le_bytes(resp_buf[12..16].try_into().unwrap());
        if maxh2c > 0 {
            self.maxh2cdata = maxh2c;
        }

        Ok(())
    }

    /// Send Fabric Connect command. Returns controller ID.
    pub async fn fabric_connect(&mut self, subnqn: &str, hostnqn: &str, qid: u16) -> io::Result<u16> {
        let cid = self.next_cid();

        // Build SQE for fabric connect
        let mut sqe = [0u8; 64];
        sqe[0] = NVME_FABRIC_OPC; // opcode
        sqe[2..4].copy_from_slice(&cid.to_le_bytes()); // CID
        // CDW10: fctype=CONNECT(1), SQSIZE in upper 16 bits
        let cdw10: u32 = (FCTYPE_CONNECT as u32) | (127u32 << 16); // SQSIZE = 128
        sqe[40..44].copy_from_slice(&cdw10.to_le_bytes());
        // CDW11: QID
        sqe[44..48].copy_from_slice(&(qid as u32).to_le_bytes());

        // Build 1024-byte connect data
        let mut connect_data = vec![0u8; 1024];
        // hostid (16 bytes at offset 0)
        connect_data[0..16].copy_from_slice(&[0x42u8; 16]);
        // cntlid (2 bytes at offset 16) = 0xFFFF (dynamic allocation)
        connect_data[16] = 0xFF;
        connect_data[17] = 0xFF;
        // subnqn at offset 256 (256 bytes)
        let nqn_bytes = subnqn.as_bytes();
        let nqn_len = nqn_bytes.len().min(256);
        connect_data[256..256 + nqn_len].copy_from_slice(&nqn_bytes[..nqn_len]);
        // hostnqn at offset 512 (256 bytes)
        let hnqn_bytes = hostnqn.as_bytes();
        let hnqn_len = hnqn_bytes.len().min(256);
        connect_data[512..512 + hnqn_len].copy_from_slice(&hnqn_bytes[..hnqn_len]);

        // Write CapsuleCmd PDU
        let hlen: u8 = 72; // 8 + 64
        let plen: u32 = hlen as u32 + connect_data.len() as u32;
        let ch = CommonHeader {
            pdu_type: PduType::CapsuleCmd as u8,
            flags: 0,
            hlen,
            pdo: hlen, // data immediately after header
            plen,
        };
        self.writer.write_all(&ch.to_bytes()).await?;
        self.writer.write_all(&sqe).await?;
        self.writer.write_all(&connect_data).await?;
        self.writer.flush().await?;

        // Read CapsuleResp
        let (cqe, _) = self.read_capsule_resp().await?;
        // CNTLID is in DW0 of CQE
        self.cntlid = u32::from_le_bytes(cqe.raw[0..4].try_into().unwrap()) as u16;

        // Check status
        let status = u16::from_le_bytes(cqe.raw[14..16].try_into().unwrap());
        if status & 0xFFFE != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("fabric connect failed: status={status:#x}"),
            ));
        }

        Ok(self.cntlid)
    }

    /// Send Identify Controller admin command.
    pub async fn identify_controller(&mut self) -> io::Result<Vec<u8>> {
        let cid = self.next_cid();

        let mut sqe = [0u8; 64];
        sqe[0] = ADMIN_IDENTIFY;
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        // CDW10: CNS = 1 (controller)
        sqe[40..44].copy_from_slice(&(CNS_CONTROLLER as u32).to_le_bytes());

        self.send_capsule_cmd(&sqe, &[]).await?;
        self.read_data_response().await
    }

    /// Send Identify Namespace admin command.
    pub async fn identify_namespace(&mut self, nsid: u32) -> io::Result<Vec<u8>> {
        let cid = self.next_cid();

        let mut sqe = [0u8; 64];
        sqe[0] = ADMIN_IDENTIFY;
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        sqe[4..8].copy_from_slice(&nsid.to_le_bytes()); // NSID
        // CDW10: CNS = 0 (namespace)
        sqe[40..44].copy_from_slice(&(CNS_NAMESPACE as u32).to_le_bytes());

        self.send_capsule_cmd(&sqe, &[]).await?;
        self.read_data_response().await
    }

    /// Read blocks from a namespace.
    pub async fn read(&mut self, nsid: u32, slba: u64, nlb: u16) -> io::Result<Vec<u8>> {
        let cid = self.next_cid();

        let mut sqe = [0u8; 64];
        sqe[0] = IO_READ;
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        sqe[4..8].copy_from_slice(&nsid.to_le_bytes());
        // CDW10: SLBA low 32 bits
        sqe[40..44].copy_from_slice(&(slba as u32).to_le_bytes());
        // CDW11: SLBA high 32 bits
        sqe[44..48].copy_from_slice(&((slba >> 32) as u32).to_le_bytes());
        // CDW12: NLB (0-based) in bits 15:0
        sqe[48..52].copy_from_slice(&((nlb.saturating_sub(1)) as u32).to_le_bytes());

        self.send_capsule_cmd(&sqe, &[]).await?;
        self.read_data_response().await
    }

    /// Write data to a namespace. Data length must be a multiple of block size.
    pub async fn write(&mut self, nsid: u32, slba: u64, data: &[u8]) -> io::Result<()> {
        let nlb = (data.len() / 4096) as u16;
        let cid = self.next_cid();

        let mut sqe = [0u8; 64];
        sqe[0] = IO_WRITE;
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        sqe[4..8].copy_from_slice(&nsid.to_le_bytes());
        // CDW10: SLBA low
        sqe[40..44].copy_from_slice(&(slba as u32).to_le_bytes());
        // CDW11: SLBA high
        sqe[44..48].copy_from_slice(&((slba >> 32) as u32).to_le_bytes());
        // CDW12: NLB (0-based)
        sqe[48..52].copy_from_slice(&((nlb.saturating_sub(1)) as u32).to_le_bytes());

        self.send_capsule_cmd(&sqe, data).await?;

        // Read CapsuleResp
        let (cqe, _) = self.read_capsule_resp().await?;
        let status = u16::from_le_bytes(cqe.raw[14..16].try_into().unwrap());
        if status & 0xFFFE != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("NVMe write failed: status={status:#x}"),
            ));
        }
        Ok(())
    }

    /// Flush a namespace.
    pub async fn flush(&mut self, nsid: u32) -> io::Result<()> {
        let cid = self.next_cid();

        let mut sqe = [0u8; 64];
        sqe[0] = IO_FLUSH;
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        sqe[4..8].copy_from_slice(&nsid.to_le_bytes());

        self.send_capsule_cmd(&sqe, &[]).await?;

        let (cqe, _) = self.read_capsule_resp().await?;
        let status = u16::from_le_bytes(cqe.raw[14..16].try_into().unwrap());
        if status & 0xFFFE != 0 {
            return Err(io::Error::new(io::ErrorKind::Other, "flush failed"));
        }
        Ok(())
    }

    /// Send a CapsuleCmd PDU with optional inline data.
    async fn send_capsule_cmd(&mut self, sqe: &[u8; 64], data: &[u8]) -> io::Result<()> {
        let hlen: u8 = 72;
        let pdo = if data.is_empty() { 0 } else { hlen };
        let plen = hlen as u32 + data.len() as u32;
        let ch = CommonHeader {
            pdu_type: PduType::CapsuleCmd as u8,
            flags: 0,
            hlen,
            pdo,
            plen,
        };
        self.writer.write_all(&ch.to_bytes()).await?;
        self.writer.write_all(sqe).await?;
        if !data.is_empty() {
            self.writer.write_all(data).await?;
        }
        self.writer.flush().await
    }

    /// Read a CapsuleResp PDU and return the CQE.
    async fn read_capsule_resp(&mut self) -> io::Result<(NvmeCqe, CommonHeader)> {
        let mut hdr_buf = [0u8; 8];
        self.reader.read_exact(&mut hdr_buf).await?;
        let ch = CommonHeader::from_bytes(&hdr_buf);

        if ch.pdu_type != PduType::CapsuleResp as u8 {
            // Might be C2HData — unexpected here
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected CapsuleResp, got type={}", ch.pdu_type),
            ));
        }

        // Read CQE (16 bytes)
        let mut cqe_buf = [0u8; 16];
        self.reader.read_exact(&mut cqe_buf).await?;

        // Read any remaining header bytes + digest
        let remaining = ch.plen as usize - 8 - 16;
        if remaining > 0 {
            let mut extra = vec![0u8; remaining];
            self.reader.read_exact(&mut extra).await?;
        }

        Ok((NvmeCqe { raw: cqe_buf }, ch))
    }

    /// Read a data response (C2HData PDUs followed by optional CapsuleResp).
    async fn read_data_response(&mut self) -> io::Result<Vec<u8>> {
        let mut data = Vec::new();
        loop {
            let mut hdr_buf = [0u8; 8];
            self.reader.read_exact(&mut hdr_buf).await?;
            let ch = CommonHeader::from_bytes(&hdr_buf);

            match PduType::from_byte(ch.pdu_type) {
                Some(PduType::C2HData) => {
                    // Read C2H-specific header (16 bytes)
                    let mut specific = [0u8; 16];
                    self.reader.read_exact(&mut specific).await?;

                    let _cccid = u16::from_le_bytes(specific[0..2].try_into().unwrap());
                    let _datao = u32::from_le_bytes(specific[4..8].try_into().unwrap());
                    let datal = u32::from_le_bytes(specific[8..12].try_into().unwrap());
                    let last = ch.flags & 0x04 != 0;
                    let success = ch.flags & 0x08 != 0;

                    // Read data payload
                    let data_size = ch.plen as usize - ch.hlen as usize;
                    if data_size > 0 {
                        let mut payload = vec![0u8; data_size];
                        self.reader.read_exact(&mut payload).await?;
                        payload.truncate(datal as usize);
                        data.extend_from_slice(&payload);
                    }

                    if last && success {
                        return Ok(data);
                    }
                    if last {
                        // Last but not success — need CapsuleResp next
                        break;
                    }
                }
                Some(PduType::CapsuleResp) => {
                    // Read CQE
                    let mut cqe_buf = [0u8; 16];
                    self.reader.read_exact(&mut cqe_buf).await?;
                    let remaining = ch.plen as usize - 8 - 16;
                    if remaining > 0 {
                        let mut extra = vec![0u8; remaining];
                        self.reader.read_exact(&mut extra).await?;
                    }
                    return Ok(data);
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unexpected PDU type {} in data response", ch.pdu_type),
                    ));
                }
            }
        }

        // Read trailing CapsuleResp if needed
        let mut hdr_buf = [0u8; 8];
        self.reader.read_exact(&mut hdr_buf).await?;
        let ch = CommonHeader::from_bytes(&hdr_buf);
        let remaining = ch.plen as usize - 8;
        if remaining > 0 {
            let mut extra = vec![0u8; remaining];
            self.reader.read_exact(&mut extra).await?;
        }
        Ok(data)
    }
}
