//! Read a device or a slab out to files the real e2fsprogs can judge (#239).
//!
//! ```text
//! cargo run --example slab_audit -- fetch URI OUT      # a device (nvme-tcp://…, a file) → sparse file
//! cargo run --example slab_audit -- extract SLAB DIR [MAX]  # every volume in a slab (or a disk of slabs) → DIR/<name>.img
//! ```
//!
//! `SLAB` may also be `http://host:port/path` — a release image read with
//! HTTP `Range` GETs, so nothing is downloaded but what the slabs map, and
//! nothing on the server changes. Writes `extract` makes (records an adopt
//! rewrites) stay in this process's memory. Volumes larger than `MAX` bytes
//! (default 2 GiB) are compared but not written out.
//!
//! `fetch` only reads. `extract` adopts the slabs of a *copy* (it may write
//! records), so point it at a file `fetch` made, never at a served device.
//! For each volume whose parent is in the same slabs, `extract` also lists
//! the 4 KiB blocks where the two differ — a clone differs from its golden
//! where it was written (stamped, mounted), and nowhere else.

use std::io::{Seek, SeekFrom, Write};
use std::sync::Arc;

use async_trait::async_trait;
use stormblock::drive::{BlockDevice, DeviceId, DriveError, DriveResult, DriveType};
use stormblock::volume::VolumeManager;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A read-only HTTP resource as a block device: `Range` GETs in 1 MiB
/// chunks, a small cache of clean chunks, writes kept in memory.
struct HttpDevice {
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
    async fn open(url: &str) -> anyhow::Result<Self> {
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
                    eprintln!("range {a}-{b}: retry {tries} ({:?})", r.map(|x| (x.0, x.2.len())).map_err(|e| e.to_string()));
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

async fn open_any(path: &str, read_only: bool) -> anyhow::Result<Arc<dyn BlockDevice>> {
    if path.starts_with("http://") {
        return Ok(Arc::new(HttpDevice::open(path).await?));
    }
    Ok(stormblock::drive::open_path(path, read_only).await?)
}

async fn dump(dev: &Arc<dyn BlockDevice>, path: &str) -> anyhow::Result<u64> {
    let size = dev.capacity_bytes();
    let mut f = std::fs::File::create(path)?;
    f.set_len(size)?;
    let mut buf = vec![0u8; 1 << 20];
    let (mut off, mut written) = (0u64, 0u64);
    while off < size {
        let n = ((size - off) as usize).min(buf.len());
        dev.read(off, &mut buf[..n]).await?;
        if buf[..n].iter().any(|&b| b != 0) {
            f.seek(SeekFrom::Start(off))?;
            f.write_all(&buf[..n])?;
            written += n as u64;
        }
        off += n as u64;
    }
    Ok(written)
}

/// The 4 KiB blocks where `a` and `b` differ, read 1 MiB at a time.
async fn diff_blocks(a: &Arc<dyn BlockDevice>, b: &Arc<dyn BlockDevice>) -> anyhow::Result<Vec<u64>> {
    let size = a.capacity_bytes().min(b.capacity_bytes());
    let (mut x, mut y) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
    let mut out = Vec::new();
    let mut off = 0u64;
    while off < size {
        let n = ((size - off) as usize).min(x.len());
        a.read(off, &mut x[..n]).await?;
        b.read(off, &mut y[..n]).await?;
        for (i, (p, q)) in x[..n].chunks(4096).zip(y[..n].chunks(4096)).enumerate() {
            if p != q {
                out.push(off / 4096 + i as u64);
            }
        }
        off += n as u64;
    }
    Ok(out)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("fetch") if args.len() == 4 => {
            let dev = open_any(&args[2], true).await?;
            let t = std::time::Instant::now();
            let n = dump(&dev, &args[3]).await?;
            println!(
                "fetched {} ({} bytes, {} non-zero) in {:.1}s",
                args[2],
                dev.capacity_bytes(),
                n,
                t.elapsed().as_secs_f64()
            );
        }
        Some("extract") if args.len() == 4 || args.len() == 5 => {
            let max: u64 = args.get(4).map(|s| s.parse()).transpose()?.unwrap_or(2 << 30);
            let dev = open_any(&args[2], false).await?;
            let found = stormblock::drive::discover::slabs_in_partitions(&dev).await;
            anyhow::ensure!(!found.is_empty(), "no slab in {}", args[2]);
            let slot = found[0].slab.slot_size();
            for f in &found {
                println!("slab {} ({}): role {}", f.slab.slab_id().0, f.label, f.slab.role());
            }
            let mut mgr = VolumeManager::new(slot);
            let r = mgr.adopt_slabs(found).await?;
            println!("adopted {} slab(s), {} volume(s)", r.slabs.len(), r.volumes.len());
            std::fs::create_dir_all(&args[3])?;
            let mut vols = mgr.list_volumes().await;
            vols.sort_by(|a, b| a.1.cmp(&b.1));
            let names: std::collections::HashMap<_, _> =
                vols.iter().map(|(id, n, ..)| (*id, n.clone())).collect();
            for (id, name, size, used) in &vols {
                let dev = mgr.get_volume(id).expect("listed");
                let file = format!("{}/{}.img", args[3], name.replace('/', "_"));
                if *size <= max {
                    dump(&dev, &file).await?;
                }
                let parent = mgr.parent(id);
                println!(
                    "volume {name} {id:?} size {size} mapped {used} parent {}",
                    parent.and_then(|p| names.get(&p).cloned()).unwrap_or_else(|| "-".into())
                );
                if let Some(p) = parent.and_then(|p| mgr.get_volume(&p)) {
                    let diff = diff_blocks(&dev, &p).await?;
                    println!("  differs from its parent in {} block(s): {:?}", diff.len(), &diff[..diff.len().min(64)]);
                }
            }
        }
        _ => anyhow::bail!("usage: slab_audit fetch URI OUT | extract SLAB DIR [MAX]"),
    }
    Ok(())
}
