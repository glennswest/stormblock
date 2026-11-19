//! A read-only view of a volume for a check (#677): its content read in
//! large, aligned I/Os, in order, and the check's small reads answered from
//! memory.
//!
//! The seal's fsck reads a template a block at a time: on forge1 (a RAID set
//! on rotating drives) 4 KiB reads at ~4.4 ms each took a 146 MiB golden's
//! check to 43–48 s, 3.2 MB/s, where the digester reads the same set at
//! ~267 MB/s in extent-sized I/Os. Here the check reads through a chunk
//! cache: what the volume maps is read first, sequentially, in chunks (the
//! digester's path), and any other chunk the check asks for is read whole
//! the first time. Writes are refused: a check only reads.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::drive::{BlockDevice, DeviceId, DriveError, DriveResult, DriveType};

/// The I/O a chunk is read in.
pub const CHUNK: u64 = 1 << 20;

/// The most the cache keeps; past it the oldest chunks go.
pub const MAX_CACHED: u64 = 512 << 20;

/// What the reads under a check cost: said in the seal's line.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Stats {
    /// Bytes read off the volume, and in how many I/Os.
    pub bytes: u64,
    pub ios: u64,
    /// The I/O size (the chunk).
    pub io_size: u64,
    /// Time spent in those reads.
    pub reading: Duration,
    /// Reads the check made, and how many were answered from memory.
    pub asked: u64,
    pub hits: u64,
}

impl Stats {
    /// MB (10^6) a second over the time spent reading.
    pub fn mb_per_s(&self) -> f64 {
        let s = self.reading.as_secs_f64();
        if s <= 0.0 {
            0.0
        } else {
            self.bytes as f64 / 1e6 / s
        }
    }
}

#[derive(Default)]
struct Cache {
    chunks: HashMap<u64, Arc<Vec<u8>>>,
    order: VecDeque<u64>,
    bytes: u64,
}

pub struct ReadAhead {
    inner: Arc<dyn BlockDevice>,
    cache: Mutex<Cache>,
    max: u64,
    bytes: AtomicU64,
    ios: AtomicU64,
    reading_ns: AtomicU64,
    asked: AtomicU64,
    hits: AtomicU64,
}

impl ReadAhead {
    pub fn new(inner: Arc<dyn BlockDevice>) -> Arc<Self> {
        Self::with_limit(inner, MAX_CACHED)
    }

    pub fn with_limit(inner: Arc<dyn BlockDevice>, max: u64) -> Arc<Self> {
        Arc::new(ReadAhead {
            inner,
            cache: Mutex::new(Cache::default()),
            max: max.max(CHUNK),
            bytes: AtomicU64::new(0),
            ios: AtomicU64::new(0),
            reading_ns: AtomicU64::new(0),
            asked: AtomicU64::new(0),
            hits: AtomicU64::new(0),
        })
    }

    pub fn stats(&self) -> Stats {
        Stats {
            bytes: self.bytes.load(Relaxed),
            ios: self.ios.load(Relaxed),
            io_size: CHUNK,
            reading: Duration::from_nanos(self.reading_ns.load(Relaxed)),
            asked: self.asked.load(Relaxed),
            hits: self.hits.load(Relaxed),
        }
    }

    /// Read the chunks holding `offsets` (byte offsets, any order) in
    /// ascending order, as far as the cache holds them: the sequential pass
    /// the check then reads from.
    pub async fn prefetch(&self, offsets: impl IntoIterator<Item = u64>) -> DriveResult<()> {
        let cap = self.inner.capacity_bytes();
        let mut idx: Vec<u64> = offsets.into_iter().filter(|o| *o < cap).map(|o| o / CHUNK).collect();
        idx.sort_unstable();
        idx.dedup();
        let room = (self.max / CHUNK) as usize;
        for i in idx.into_iter().take(room) {
            self.chunk(i).await?;
        }
        Ok(())
    }

    fn cached(&self, i: u64) -> Option<Arc<Vec<u8>>> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).chunks.get(&i).cloned()
    }

    /// Chunk `i`, read whole the first time.
    async fn chunk(&self, i: u64) -> DriveResult<Arc<Vec<u8>>> {
        if let Some(c) = self.cached(i) {
            return Ok(c);
        }
        let cap = self.inner.capacity_bytes();
        let start = i * CHUNK;
        if start >= cap {
            return Err(DriveError::OutOfRange { offset: start, len: CHUNK, capacity: cap });
        }
        let len = CHUNK.min(cap - start) as usize;
        let mut buf = vec![0u8; len];
        let t = Instant::now();
        let mut done = 0usize;
        while done < len {
            let n = self.inner.read(start + done as u64, &mut buf[done..]).await?;
            self.ios.fetch_add(1, Relaxed);
            if n == 0 {
                return Err(DriveError::Io(std::io::Error::from(std::io::ErrorKind::UnexpectedEof)));
            }
            done += n;
        }
        self.reading_ns.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
        self.bytes.fetch_add(len as u64, Relaxed);
        let c = Arc::new(buf);
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if !cache.chunks.contains_key(&i) {
            while cache.bytes + len as u64 > self.max {
                let Some(old) = cache.order.pop_front() else { break };
                if let Some(o) = cache.chunks.remove(&old) {
                    cache.bytes -= o.len() as u64;
                }
            }
            cache.bytes += len as u64;
            cache.order.push_back(i);
            cache.chunks.insert(i, c.clone());
        }
        Ok(c)
    }
}

#[async_trait::async_trait]
impl BlockDevice for ReadAhead {
    fn id(&self) -> &DeviceId {
        self.inner.id()
    }
    fn capacity_bytes(&self) -> u64 {
        self.inner.capacity_bytes()
    }
    fn block_size(&self) -> u32 {
        self.inner.block_size()
    }
    fn optimal_io_size(&self) -> u32 {
        self.inner.optimal_io_size()
    }
    fn discard_granularity(&self) -> u32 {
        self.inner.discard_granularity()
    }
    fn device_type(&self) -> DriveType {
        self.inner.device_type()
    }
    fn rotational(&self) -> bool {
        self.inner.rotational()
    }

    async fn read(&self, offset: u64, buf: &mut [u8]) -> DriveResult<usize> {
        let cap = self.inner.capacity_bytes();
        if offset.saturating_add(buf.len() as u64) > cap {
            return Err(DriveError::OutOfRange { offset, len: buf.len() as u64, capacity: cap });
        }
        self.asked.fetch_add(1, Relaxed);
        let mut all_hit = true;
        let mut done = 0usize;
        while done < buf.len() {
            let at = offset + done as u64;
            let i = at / CHUNK;
            let c = match self.cached(i) {
                Some(c) => c,
                None => {
                    all_hit = false;
                    self.chunk(i).await?
                }
            };
            let within = (at - i * CHUNK) as usize;
            let n = (c.len() - within).min(buf.len() - done);
            buf[done..done + n].copy_from_slice(&c[within..within + n]);
            done += n;
        }
        if all_hit {
            self.hits.fetch_add(1, Relaxed);
        }
        Ok(done)
    }

    async fn write(&self, _offset: u64, _buf: &[u8]) -> DriveResult<usize> {
        Err(DriveError::ReadOnly("a check's read-ahead view takes no writes (#677)".into()))
    }

    async fn flush(&self) -> DriveResult<()> {
        Ok(())
    }

    async fn discard(&self, _offset: u64, _len: u64) -> DriveResult<()> {
        Err(DriveError::ReadOnly("a check's read-ahead view takes no discards (#677)".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;

    async fn file(len: u64) -> (Arc<dyn BlockDevice>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join("stormblock-readahead-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.bin", uuid::Uuid::new_v4().simple()));
        let dev: Arc<dyn BlockDevice> =
            Arc::new(FileDevice::open_with_capacity(path.to_str().unwrap(), len).await.unwrap());
        (dev, path)
    }

    /// #677: the check's 4 KiB reads come from a few chunk-sized ones, the
    /// bytes are the volume's, and a read across a chunk edge is whole.
    #[tokio::test]
    async fn small_reads_come_from_large_ones() {
        let len = 3 * CHUNK + 8192;
        let (dev, path) = file(len).await;
        let pattern: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        dev.write(0, &pattern).await.unwrap();
        let ra = ReadAhead::new(dev.clone());
        ra.prefetch([0, CHUNK + 5, 2 * CHUNK]).await.unwrap();
        assert_eq!(ra.stats().bytes, 3 * CHUNK);
        let mut b = vec![0u8; 4096];
        for k in 0..(3 * CHUNK / 4096) {
            ra.read(k * 4096, &mut b).await.unwrap();
            assert_eq!(&b[..], &pattern[(k * 4096) as usize..(k * 4096 + 4096) as usize]);
        }
        let s = ra.stats();
        assert_eq!(s.bytes, 3 * CHUNK, "every 4 KiB read was answered from memory");
        assert_eq!(s.hits, s.asked);
        // Across the edge into the short last chunk, read on demand.
        let mut across = vec![0u8; 8192];
        ra.read(3 * CHUNK - 4096, &mut across).await.unwrap();
        assert_eq!(&across[..], &pattern[(3 * CHUNK - 4096) as usize..(3 * CHUNK + 4096) as usize]);
        assert_eq!(ra.stats().bytes, 3 * CHUNK + 8192);
        assert!(ra.write(0, &b).await.is_err());
        assert!(ra.read(len - 4096, &mut vec![0u8; 8192]).await.is_err());
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn the_cache_keeps_to_its_limit() {
        let (dev, path) = file(4 * CHUNK).await;
        let ra = ReadAhead::with_limit(dev, 2 * CHUNK);
        ra.prefetch([0, CHUNK, 2 * CHUNK, 3 * CHUNK]).await.unwrap();
        assert_eq!(ra.stats().bytes, 2 * CHUNK, "the prefetch stops at what the cache holds");
        let mut b = vec![0u8; 4096];
        ra.read(3 * CHUNK, &mut b).await.unwrap();
        assert!(ra.cache.lock().unwrap().bytes <= 2 * CHUNK);
        let _ = std::fs::remove_file(path);
    }
}
