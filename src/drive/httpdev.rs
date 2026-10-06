//! A release image read over HTTP, as a block device (#122).
//!
//! The appliance publishes a release as `GET /api/v1/releases/{v}/image.img`
//! with `Range` support. Staging that release on a running node reads only
//! what its slabs map: 1 MiB `Range` GETs, a small cache of clean chunks.
//! Nothing on the server changes: a write (a record an adopt rewrites) is
//! kept in this process's memory and never sent. `http://` only — the
//! appliance serves plain HTTP on its own network.

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{BlockDevice, DeviceId, DriveError, DriveResult, DriveType};

/// A read-only HTTP resource as a block device: `Range` GETs in 1 MiB
/// chunks, a small cache of clean chunks, writes kept in memory.
pub struct HttpDevice {
    id: DeviceId,
    host: String,
    path: String,
    size: u64,
    chunks: std::sync::Mutex<(std::collections::HashMap<u64, Vec<u8>>, std::collections::VecDeque<u64>)>,
    dirty: std::sync::Mutex<std::collections::HashMap<u64, Vec<u8>>>,
}

const CHUNK: u64 = 1 << 20;

async fn http_get(host: &str, path: &str, range: Option<(u64, u64)>) -> anyhow::Result<(u16, Vec<String>, Vec<u8>)> {
    let mut s = tokio::net::TcpStream::connect(host).await?;
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    if let Some((a, b)) = range {
        req += &format!("Range: bytes={a}-{b}\r\n");
    }
    req += "\r\n";
    s.write_all(req.as_bytes()).await?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await?;
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| anyhow::anyhow!("no header end"))?;
    let head = String::from_utf8_lossy(&raw[..end]).to_string();
    let lines: Vec<String> = head.split("\r\n").map(str::to_string).collect();
    let status: u16 = lines[0].split_whitespace().nth(1).unwrap_or("0").parse()?;
    Ok((status, lines, raw[end + 4..].to_vec()))
}

impl HttpDevice {
    /// Open `http://host[:port]/path`: one `Range: bytes=0-0` GET for its
    /// size, which must answer 206 with a `Content-Range`.
    pub async fn open(url: &str) -> anyhow::Result<Self> {
        let rest = url.strip_prefix("http://").ok_or_else(|| anyhow::anyhow!("http:// only"))?;
        let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        let (status, lines, _) = http_get(host, path, Some((0, 0))).await?;
        anyhow::ensure!(status == 206, "{url}: HTTP {status} to a Range GET");
        let size = lines
            .iter()
            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-range: bytes 0-0/").map(|s| s.trim().parse::<u64>()))
            .ok_or_else(|| anyhow::anyhow!("no Content-Range"))??;
        Ok(Self {
            id: DeviceId {
                uuid: uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, url.as_bytes()),
                serial: "http".into(),
                model: "range-get".into(),
                path: url.into(),
                wwn: String::new(),
            },
            host: host.into(),
            path: path.into(),
            size,
            chunks: Default::default(),
            dirty: Default::default(),
        })
    }

    async fn chunk(&self, n: u64) -> DriveResult<Vec<u8>> {
        if let Some(c) = self.dirty.lock().unwrap().get(&n) {
            return Ok(c.clone());
        }
        if let Some(c) = self.chunks.lock().unwrap().0.get(&n) {
            return Ok(c.clone());
        }
        let a = n * CHUNK;
        let b = (a + CHUNK).min(self.size) - 1;
        let mut tries = 0;
        let body = loop {
            match http_get(&self.host, &self.path, Some((a, b))).await {
                Ok((206, _, body)) if body.len() as u64 == b - a + 1 => break body,
                r if tries < 5 => {
                    tries += 1;
                    tracing::debug!("{}: range {a}-{b}: retry {tries} ({:?})", self.id.path, r.map(|x| (x.0, x.2.len())).map_err(|e| e.to_string()));
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                r => return Err(DriveError::Other(anyhow::anyhow!("range {a}-{b}: {:?}", r.map(|x| x.0)))),
            }
        };
        let mut c = self.chunks.lock().unwrap();
        c.0.insert(n, body.clone());
        c.1.push_back(n);
        while c.1.len() > 64 {
            let old = c.1.pop_front().unwrap();
            c.0.remove(&old);
        }
        Ok(body)
    }
}

#[async_trait]
impl BlockDevice for HttpDevice {
    fn id(&self) -> &DeviceId {
        &self.id
    }
    fn capacity_bytes(&self) -> u64 {
        self.size
    }
    fn block_size(&self) -> u32 {
        4096
    }
    fn optimal_io_size(&self) -> u32 {
        CHUNK as u32
    }
    fn device_type(&self) -> DriveType {
        DriveType::File
    }
    async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done as u64;
            let c = self.chunk(pos / CHUNK).await?;
            let at = (pos % CHUNK) as usize;
            let n = (c.len() - at).min(buf.len() - done);
            buf[done..done + n].copy_from_slice(&c[at..at + n]);
            done += n;
        }
        Ok(done)
    }
    async fn write(&self, offset: u64, buf: &[u8]) -> DriveResult<usize> {
        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done as u64;
            let mut c = self.chunk(pos / CHUNK).await?;
            let at = (pos % CHUNK) as usize;
            let n = (c.len() - at).min(buf.len() - done);
            c[at..at + n].copy_from_slice(&buf[done..done + n]);
            self.dirty.lock().unwrap().insert(pos / CHUNK, c);
            done += n;
        }
        Ok(done)
    }
    async fn flush(&self) -> DriveResult<()> {
        Ok(())
    }
    async fn discard(&self, _offset: u64, _len: u64) -> DriveResult<()> {
        Ok(())
    }
}

